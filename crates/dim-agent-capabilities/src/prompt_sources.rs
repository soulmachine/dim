//! Prompt sources: the system prompt (`dim/base`, `dim/session`), the
//! AGENTS.md rules (`dim/instructions/project`, `dim/instructions/user`) and
//! the runtime environment, date and reply-language states. Prompt delivers
//! them; each loaded branch checks the changing ones and has Prompt adopt
//! changes.
use crate::{
    context_block,
    host::{HostError, HostFuture, LoadPrompt, PromptHost},
    host_platform::{
        ExecShellRuntime, HostPlatform, Os, OutputEncoding, ShellEdition, ShellStatus,
    },
    prompt::Prompt,
    prompt_registry::{PromptRegistry, PromptRegistryPlugin, targets},
    prompt_state::PromptSnapshot,
};
use dim_agent_runtime::{
    AgentBuilder, BranchLifecycle, BranchPhase, ConfigureUpdate, Names, PromptSourceTarget,
    ReplyLanguage, SessionConfig, StateCell,
};
use dim_agent_types::*;
use rutis::Ctx;
use rutis::host::{BoxFuture, CtxHost, DynPlugin, Effect, Key, TypeKey};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

pub const BASE: &str = "dim/base";
pub const PROJECT_INSTRUCTIONS: &str = "dim/instructions/project";
pub const USER_INSTRUCTIONS: &str = "dim/instructions/user";
pub const SESSION: &str = "dim/session";
pub const ENVIRONMENT: &str = "dim/environment/context";
pub const DATE: &str = "dim/environment/date";
pub const LANGUAGE: &str = "dim/language";
/// Sources whose content changes while a Session runs, compared by each
/// loaded branch. The reply language is Session-wide: a side branch that was
/// loaded before an update adopts it through this comparison.
const CHECKED: [&str; 5] = [
    PROJECT_INSTRUCTIONS,
    USER_INSTRUCTIONS,
    ENVIRONMENT,
    DATE,
    LANGUAGE,
];

/// Today's date as `YYYY-MM-DD` in the process-local time zone.
pub type Today = Arc<dyn Fn() -> String + Send + Sync>;

/// How often a loaded branch compares the AGENTS.md files, the runtime
/// environment and the local date with what it adopted. Polling the wall clock, instead of sleeping
/// until midnight on the monotonic clock, follows system sleep and clock or
/// zone changes; polling the files avoids watchers tied to branch lifetimes.
const DATE_CHECK: Duration = Duration::from_secs(60);

/// Registers PromptSources with the process-local clock.
pub fn install(builder: &mut AgentBuilder) {
    install_with_clock(builder, Arc::new(local_today), DATE_CHECK);
}

/// Registers PromptSources with a replaceable date source, checked every
/// `every`.
pub fn install_with_clock(builder: &mut AgentBuilder, today: Today, every: Duration) {
    builder.require_feature(
        dim_agent_runtime::Feature::new("PromptSources")
            .session("PromptRegistry", |names| {
                Ok(Box::new(PromptRegistryPlugin::new(names)?))
            })
            .session("PromptSources", move |names| {
                Ok(Box::new(PromptSourcesPlugin::new(names, today.clone())?))
            })
            .branch("PromptSourcesBranch", move |names| {
                Ok(Box::new(PromptSourcesBranchPlugin {
                    branch: names.key()?,
                    host: names.key()?,
                    configure: names.key()?,
                    prompts: names.state_cell()?,
                    language: names.key()?,
                    service: names.key()?,
                    every,
                }))
            }),
    );
}

/// Every source this capability provides, in delivery order.
const SOURCES: [&str; 7] = [
    BASE,
    SESSION,
    PROJECT_INSTRUCTIONS,
    USER_INSTRUCTIONS,
    ENVIRONMENT,
    DATE,
    LANGUAGE,
];

/// Refreshes dynamic sources without replacing adopted System entries.
pub fn refresh_update() -> ConfigureUpdate {
    ConfigureUpdate {
        sources: targets(CHECKED),
        model: None,
    }
}

/// The update that has Prompt adopt the reply-language state alone.
pub fn language_update() -> ConfigureUpdate {
    ConfigureUpdate {
        sources: targets([LANGUAGE]),
        model: None,
    }
}

/// The one sentence the `dim/language` state carries: the setting itself,
/// never an instruction ([PromptSources §3]).
///
/// [PromptSources §3]: ../../../docs/design/plugins/PromptSources.md
pub fn language_body(language: ReplyLanguage) -> String {
    format!(
        "The operator set this session's reply language to {}.",
        language.name()
    )
}

/// The Session System entry used by both creation and explicit profile adoption.
pub fn session_entry(body: String, scope: PromptScope) -> Option<PromptEntry> {
    (!body.trim().is_empty()).then(|| PromptEntry {
        source: SESSION.into(),
        version: sha256_version(&body),
        body,
        scope,
        delivery: PromptDelivery::System,
        order: 50,
    })
}

/// Mirrors TS `formatCurrentDate`.
pub fn local_today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

pub(crate) fn sha256_version(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    let mut version = String::with_capacity(7 + digest.len() * 2);
    version.push_str("sha256:");
    for byte in digest {
        version.push_str(&format!("{byte:02x}"));
    }
    version
}

const IDENTITY: &str = "You are DimAgent. You and the user work together in their workspace toward whatever they're trying to get done.";
const DEFAULTS: &str = "Tone and communication:
Think and respond in the same language as the user unless asked otherwise.
Keep responses concise; add detail only when it materially helps; skip preamble and wrap-up; use emojis only when asked.

Objectivity:
Prioritize technical accuracy over agreeing with the user. Disagree when warranted; correction beats false agreement.
Do not fabricate facts, capabilities, results, or whether tests passed. If unsure, say so and verify.

Engineering:
Follow the user's request and the repository's existing conventions, libraries, and patterns.
Make the smallest change that fully solves the problem; avoid unrelated refactors.
Fail fast on invalid input and surface clear errors.
Run relevant tests or checks after code changes, using repository-documented commands.

Git:
Stage only files you intentionally changed for the current task, using explicit paths.
Protect unrelated user changes when editing, resolving conflicts, staging, or committing.
Avoid `git add -A`, `git add .`, `git stash`, `git reset --hard`, `git checkout .`, `git clean -fd`, `git commit --no-verify`, and force push.
Do not commit unless the user asks you to.

Working method:
If the user only needs information or explanation, answer directly.

Ending a turn:
Finish any independent work that can be done now before ending the turn.
When the remaining work depends on asynchronous background tasks or agent notifications, end the turn or use the wait tool; do not poll or sleep.
Otherwise continue until the task is complete, explicit user input is required, or a real external blocker prevents further progress.

Other:
When referencing specific code, include `file_path:line_number`.
When mentioning a file, especially a local file, render it as a markdown link (e.g. `[name](path)`) so the client can display it. To link to a specific line, append `#L{line}` to the href (e.g. `[name](path/to/file#L42)`). `:{line}` is also supported as a fallback.
When linking to a local file outside the current workspace, preserve the complete absolute path or use an exact workspace-relative path including every required `..` segment (for example, `../Documents/file.md`); never shorten it to a workspace-relative-looking path such as `Documents/file.md`.";

const POSIX_GUIDANCE: &str = "Use POSIX shell syntax. Quote paths with single quotes and escape an embedded single quote as '\\''.";
const FILE_ENCODING_GUIDANCE: &str = "The read, edit, and write tools receive text as Unicode strings and do not pass it through the shell code page. Shell encoding concerns do not change the dedicated file tool selection rules. If read returns text_encoding_required, retry with an explicit read.encoding such as gbk or cp936; do not guess legacy encodings.";

fn shell_guidance(edition: ShellEdition) -> (&'static str, &'static str) {
    match edition {
        ShellEdition::Pwsh => (
            "pwsh",
            "Use PowerShell 7 syntax: $env:NAME; invoke a quoted executable path with & 'C:\\path with spaces\\app.exe'; && is supported.",
        ),
        ShellEdition::WindowsPowershell => (
            "windows-powershell",
            "Use Windows PowerShell 5.1 syntax: $env:NAME; invoke a quoted executable path with & 'C:\\path with spaces\\app.exe'; do not use &&.",
        ),
        ShellEdition::Cmd => (
            "cmd",
            "Use cmd.exe syntax: %NAME%; quote paths with double quotes; escape metacharacters with ^; %NAME% expands even inside quotes; delayed ! expansion is disabled; do not use PowerShell syntax.",
        ),
        ShellEdition::GitBash => (
            "git-bash",
            "Use Git Bash POSIX syntax. Quote paths with single quotes and escape an embedded single quote as '\\''. Shell commands use Git Bash paths such as /c/path; read, write, and edit keep native Windows paths.",
        ),
        ShellEdition::Bash => ("bash", POSIX_GUIDANCE),
        ShellEdition::Zsh => ("zsh", POSIX_GUIDANCE),
        ShellEdition::Sh => ("sh", POSIX_GUIDANCE),
        ShellEdition::Dash => ("dash", POSIX_GUIDANCE),
        ShellEdition::Ksh => ("ksh", POSIX_GUIDANCE),
    }
}

fn xml_tag(name: &str, value: &str) -> String {
    format!("<{name}>{}</{name}>", context_block::text(value))
}

/// The runtime environment (TS `renderRuntimeEnvironment`): one tag per
/// line, values escaped; the date is its own source.
fn environment(
    platform: &HostPlatform,
    shell: &ShellStatus,
    cwd: &str,
    policy: &PromptSourcesConfig,
) -> String {
    let os = match platform.os {
        Os::Macos => "darwin",
        Os::Linux => "linux",
        Os::Windows => "win32",
    };
    let mut entries = vec![
        xml_tag("cwd", cwd),
        xml_tag("home", &platform.home.to_string_lossy()),
    ];
    match shell {
        ShellStatus::Available(ExecShellRuntime {
            executable_path,
            edition,
            output_encoding,
            ..
        }) => {
            let shell = *edition;
            let (edition, built_in) = shell_guidance(shell);
            let guidance = match &policy.shell_guidance {
                Some(custom) => custom(shell),
                None => built_in.to_owned(),
            };
            entries.push(xml_tag("shell", &executable_path.to_string_lossy()));
            entries.push(xml_tag("shell_edition", edition));
            entries.push(xml_tag("shell_guidance", &guidance));
            entries.push(xml_tag(
                "shell_output_encoding",
                match output_encoding {
                    OutputEncoding::Utf8 => "utf8",
                    OutputEncoding::Unknown => "unknown",
                },
            ));
            if platform.os == Os::Windows {
                let guidance = policy
                    .file_encoding_guidance
                    .as_deref()
                    .unwrap_or(FILE_ENCODING_GUIDANCE);
                entries.push(xml_tag("file_encoding_guidance", guidance));
            }
        }
        ShellStatus::Unavailable { reason, .. } => {
            entries.push(xml_tag("shell", &format!("unavailable: {reason}")));
        }
    }
    entries.push(xml_tag("platform", os));
    if let Some(timezone) = &platform.timezone {
        entries.push(xml_tag("timezone", timezone));
    }
    format!("\n{}\n", entries.join("\n"))
}

fn read_project_instructions(path: &Path) -> Result<String, HostError> {
    let bytes = std::fs::read(path).map_err(|error| {
        HostError::failed(format!(
            "{PROJECT_INSTRUCTIONS}: AGENTS.md not readable: {}",
            error.kind()
        ))
    })?;
    String::from_utf8(bytes).map_err(|_| {
        HostError::failed(format!("{PROJECT_INSTRUCTIONS}: AGENTS.md not valid UTF-8"))
    })
}

fn read_instructions(cwd: &Path, home: &Path, project: bool) -> Result<Option<String>, HostError> {
    let project_path = cwd
        .ancestors()
        .map(|dir| dir.join("AGENTS.md"))
        .find(|candidate| candidate.exists());
    if project {
        return project_path
            .map(|path| {
                let content = read_project_instructions(&path)?;
                Ok((!content.trim().is_empty()).then(|| {
                    format!(
                        "# Project instructions ({}; applies to this directory and below)\n{}",
                        context_block::text(&path.display().to_string()),
                        context_block::text(content.trim())
                    )
                }))
            })
            .transpose()
            .map(Option::flatten);
    }
    let user_path = home.join("AGENTS.md");
    if project_path.as_ref().is_some_and(
        |path| matches!((path.canonicalize(), user_path.canonicalize()), (Ok(a), Ok(b)) if a == b),
    ) {
        return Ok(None);
    }
    Ok(std::fs::read_to_string(user_path)
        .ok()
        .filter(|content| !content.trim().is_empty())
        .map(|content| {
            format!(
                "# User instructions (~/AGENTS.md)\n{}",
                context_block::text(content.trim())
            )
        }))
}

async fn load_instructions(
    cwd: &str,
    home: &Path,
    project: bool,
) -> Result<Option<String>, HostError> {
    let (cwd, home) = (PathBuf::from(cwd), home.to_owned());
    tokio::task::spawn_blocking(move || read_instructions(&cwd, &home, project))
        .await
        .map_err(|_| HostError::failed("AGENTS.md read task failed"))?
}

/// Factory configuration of the sources' policy texts ([PromptSources §2]).
/// The product sets none of them, so the built-in texts are in force.
#[derive(Clone, Default)]
pub struct PromptSourcesConfig {
    /// `dim/base` in place of the built-in identity and rules.
    pub base: Option<Arc<str>>,
    /// The shell guidance the environment source carries, per edition.
    pub shell_guidance: Option<Arc<dyn Fn(ShellEdition) -> String + Send + Sync>>,
    /// The file-encoding guidance the environment source carries.
    pub file_encoding_guidance: Option<Arc<str>>,
}

/// The host's user override of `dim/base`. `None` keeps this plugin's factory
/// configuration or the built-in text.
pub trait BasePromptSettings: Send + Sync + 'static {
    fn base(&self, cancel: CancellationToken) -> HostFuture<'_, Option<Arc<str>>>;
}

pub struct BasePromptSettingsHost(pub Arc<dyn BasePromptSettings>);
/// The Session's reply language, when the operator selected one: the live
/// value the `dim/language` source reads and the App Server's
/// `session.language.*` methods change. It starts from `SessionConfig.language`
/// when the Session is created or restored.
pub struct SessionLanguage {
    language: std::sync::Mutex<Option<ReplyLanguage>>,
    /// Serializes the Session's reads and updates of the value and its
    /// adopted entry with each branch's own check: an update writes the
    /// record and the value and then has Prompt adopt it, and a check that
    /// ran between those steps would adopt the value the update replaces.
    gate: tokio::sync::Mutex<()>,
}
impl SessionLanguage {
    pub fn new(language: Option<ReplyLanguage>) -> Self {
        Self {
            language: std::sync::Mutex::new(language),
            gate: tokio::sync::Mutex::new(()),
        }
    }
    /// The Session's language gate: held across a value's whole update and
    /// across a branch's whole check, so the two never interleave.
    pub async fn gate(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.gate.lock().await
    }
    /// Replaces the value with `language`.
    pub fn replace(&self, language: Option<ReplyLanguage>) -> Option<ReplyLanguage> {
        *self.language.lock().expect("session language") = language;
        language
    }
}

/// The `get` reply: the Session's reply language when the operator selected
/// one, `null` otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionLanguageReply {
    pub language: Option<ReplyLanguage>,
}

#[rutis::service]
impl SessionLanguage {
    /// The reply language the operator selected, as data. The App Server's
    /// `session.language.get` answers the same value; its update also writes
    /// the Session record and has Prompt adopt the source, so changing the
    /// value is not this face's operation.
    pub fn get(&self) -> SessionLanguageReply {
        SessionLanguageReply {
            language: *self.language.lock().expect("session language"),
        }
    }
}

/// The Session's reply-language cell, reachable from its branches: the App
/// Server's Session methods read and change it through `main`'s service.
pub struct PromptSourcesBranch {
    language: Arc<SessionLanguage>,
}
impl PromptSourcesBranch {
    pub fn language(&self) -> Arc<SessionLanguage> {
        self.language.clone()
    }
}

struct Sources {
    platform: Arc<HostPlatform>,
    config: Arc<SessionConfig>,
    today: Today,
    stopped: CancellationToken,
    policy: PromptSourcesConfig,
    language: Arc<SessionLanguage>,
}

impl LoadPrompt for Sources {
    fn load(
        &self,
        target: PromptSourceTarget,
        cancel: CancellationToken,
    ) -> HostFuture<'_, Option<PromptEntry>> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(HostError::Cancelled("prompt source load cancelled".into()));
            }
            if self.stopped.is_cancelled() {
                return Err(HostError::failed("prompt sources reloading"));
            }
            let (body, delivery, order) = match target.source.as_str() {
                BASE => (
                    Some(match &self.policy.base {
                        Some(base) => base.to_string(),
                        None => format!("{IDENTITY}\n{DEFAULTS}"),
                    }),
                    PromptDelivery::System,
                    0,
                ),
                PROJECT_INSTRUCTIONS | USER_INSTRUCTIONS => {
                    let body = load_instructions(
                        &self.config.cwd,
                        &self.platform.home,
                        target.source == PROJECT_INSTRUCTIONS,
                    )
                    .await?;
                    let order = if target.source == PROJECT_INSTRUCTIONS {
                        100
                    } else {
                        110
                    };
                    (body, PromptDelivery::Context, order)
                }
                ENVIRONMENT => (
                    Some(environment(
                        &self.platform,
                        &self.platform.exec_shell().await,
                        &self.config.cwd,
                        &self.policy,
                    )),
                    PromptDelivery::State,
                    200,
                ),
                DATE => (
                    Some(context_block::text(&(self.today)())),
                    PromptDelivery::State,
                    210,
                ),
                LANGUAGE => (
                    self.language.get().language.map(language_body),
                    PromptDelivery::State,
                    220,
                ),
                SESSION => {
                    return Ok(session_entry(
                        self.config.system_prompt.clone(),
                        target.scope,
                    ));
                }
                other => {
                    return Err(HostError::failed(format!("unknown prompt source: {other}")));
                }
            };
            Ok(body.map(|body| PromptEntry {
                source: target.source,
                version: sha256_version(&body),
                body,
                scope: target.scope,
                delivery,
                order,
            }))
        })
    }
}

/// Session part: registers each of its sources with the prompt registry.
pub struct PromptSourcesPlugin {
    platform: Key<HostPlatform>,
    config: Key<SessionConfig>,
    registry: Key<PromptRegistry>,
    language: Key<SessionLanguage>,
    settings: Option<Key<BasePromptSettingsHost>>,
    pub today: Today,
    policy: PromptSourcesConfig,
}
impl PromptSourcesPlugin {
    pub fn new(names: &Names<'_>, today: Today) -> rutis::Result<Self> {
        Self::with_config(names, today, PromptSourcesConfig::default())
    }
    /// The plugin with a candidate's policy texts at its factory
    /// configuration ([PromptSources §2](../../../docs/design/plugins/PromptSources.md)).
    pub fn with_config(
        names: &Names<'_>,
        today: Today,
        policy: PromptSourcesConfig,
    ) -> rutis::Result<Self> {
        Ok(Self {
            platform: names.key()?,
            config: names.key()?,
            registry: names.key()?,
            language: names.key()?,
            settings: names
                .registered::<BasePromptSettingsHost>()
                .then(|| names.key::<BasePromptSettingsHost>())
                .transpose()?,
            today,
            policy,
        })
    }
}
impl DynPlugin for PromptSourcesPlugin {
    fn name(&self) -> &str {
        "PromptSources"
    }
    fn injects(&self) -> Vec<TypeKey> {
        vec![
            self.platform.erase(),
            self.config.erase(),
            self.registry.erase(),
        ]
    }
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, rutis::Result<Effect>> {
        Box::pin(async move {
            let platform = ctx.require_at(&self.platform)?;
            let config = ctx.require_at(&self.config)?;
            let registry = ctx.require_at(&self.registry)?;
            // Apply runs once per Session instance: builder rows cannot
            // reload, and every rebuilt daemon or fork creates a new
            // instance, each reading its record.
            let language = Arc::new(SessionLanguage::new(config.language));
            let mut policy = self.policy.clone();
            if policy.base.is_none() {
                let settings = match &self.settings {
                    Some(key) => ctx.get_at(key),
                    None => None,
                };
                if let Some(settings) = settings {
                    match settings
                        .0
                        .base(ctx.cancellation_token().child_token())
                        .await
                    {
                        Ok(Some(base)) => policy.base = Some(base),
                        Ok(None) => {}
                        Err(error) => {
                            tracing::warn!(%error, "base prompt setting was not read")
                        }
                    }
                }
            }
            let sources: Arc<dyn LoadPrompt> = Arc::new(Sources {
                platform,
                config,
                today: self.today.clone(),
                stopped: ctx.cancellation_token(),
                policy,
                language: language.clone(),
            });
            for name in SOURCES {
                registry.register(ctx, name, sources.clone())?;
            }
            ctx.provide_at(self.language.clone(), language)?;
            Ok(Effect::Done)
        })
    }
}

/// Branch part: while the branch is loaded, compares what the checked
/// sources hold now (AGENTS.md, runtime environment, date, reply language)
/// with what Prompt adopted on this branch, and has Prompt adopt any
/// difference. Opening loads are reused; periodic checks begin one `every`
/// later. A part reloaded on an active branch checks immediately.
pub struct PromptSourcesBranchPlugin {
    branch: Key<BranchLifecycle>,
    host: Key<PromptHost>,
    configure: Key<Prompt>,
    prompts: Key<StateCell<PromptSnapshot>>,
    language: Key<SessionLanguage>,
    service: Key<PromptSourcesBranch>,
    every: Duration,
}
impl DynPlugin for PromptSourcesBranchPlugin {
    fn name(&self) -> &str {
        "PromptSourcesBranch"
    }
    fn injects(&self) -> Vec<TypeKey> {
        vec![
            self.branch.erase(),
            self.host.erase(),
            self.configure.erase(),
            self.prompts.erase(),
            self.language.erase(),
        ]
    }
    fn apply<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, rutis::Result<Effect>> {
        Box::pin(async move {
            let branch = ctx.require_at(&self.branch)?;
            let host = ctx.require_at(&self.host)?;
            let configure = ctx.require_at(&self.configure)?;
            let prompts = ctx.require_at(&self.prompts)?;
            let language = ctx.require_at(&self.language)?;
            let (every, lifecycle) = (self.every, ctx.cancellation_token());
            let checked_language = language.clone();
            let initial_opening = branch.phase() != BranchPhase::Active;
            let prompts = prompts.subscribe();
            let tasks = TaskTracker::new();
            tasks.spawn(async move {
                tokio::select! {
                    _ = lifecycle.cancelled() => return,
                    active = branch.activated() => if !active { return },
                }
                if initial_opening {
                    tokio::select! {
                        _ = lifecycle.cancelled() => return,
                        _ = branch.closing() => return,
                        _ = tokio::time::sleep(every) => {}
                    }
                }
                loop {
                    let mut changed = Vec::new();
                    for target in targets(CHECKED).into_iter().filter(|target| target.source != LANGUAGE) {
                        let current = tokio::select! {
                            _ = lifecycle.cancelled() => return,
                            loaded = host.0.load(target.clone(), lifecycle.child_token()) => loaded,
                        };
                        match current {
                            Ok(current) if current.as_ref() != prompts.borrow().entries.get(&(target.source.clone(), target.scope.clone())) => changed.push((target, current)),
                            Ok(_) => {}
                            Err(error) => tracing::warn!(%error, source = target.source, "prompt source was not checked"),
                        }
                    }
                    let language_gate = checked_language.gate().await;
                    let target = targets([LANGUAGE]).pop().expect("language target");
                    let current = tokio::select! {
                        _ = lifecycle.cancelled() => return,
                        loaded = host.0.load(target.clone(), lifecycle.child_token()) => loaded,
                    };
                    let mut language_changed = false;
                    match current {
                        Ok(current) if current.as_ref() != prompts.borrow().entries.get(&(target.source.clone(), target.scope.clone())) => {
                            changed.push((target, current));
                            language_changed = true;
                        }
                        Ok(_) => {}
                        Err(error) => tracing::warn!(%error, source = LANGUAGE, "prompt source was not checked"),
                    }
                    let _language_gate = language_changed.then_some(language_gate);
                    if !changed.is_empty() {
                        tokio::select! {
                            _ = lifecycle.cancelled() => return,
                            result = configure.clone().configure_loaded(None, changed) => if let Err(error) = result {
                                tracing::warn!(%error, "changed prompt sources were not adopted");
                            },
                        }
                    }
                    drop(_language_gate);
                    tokio::select! {
                        _ = lifecycle.cancelled() => return,
                        _ = branch.closing() => return,
                        _ = tokio::time::sleep(every) => {}
                    }
                }
            });
            ctx.effect(move || async move {
                tasks.close();
                tasks.wait().await;
                Ok(())
            })?;
            ctx.provide_at(
                self.service.clone(),
                Arc::new(PromptSourcesBranch { language }),
            )?;
            Ok(Effect::Done)
        })
    }
}
