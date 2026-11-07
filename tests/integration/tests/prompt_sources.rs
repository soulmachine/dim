mod local_support;

use dim_agent_capabilities::prompt_sources::refresh_update;
use dim_agent_runtime::testing::chain_events;
use dim_agent_runtime::*;
use dim_agent_types::*;
use local_support::*;
use std::sync::Arc;

fn prompt_changes(events: &[Arc<AgentEvent>]) -> Vec<PromptChangedData> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::PromptChanged(data) => Some(data.clone()),
            _ => None,
        })
        .collect()
}

fn last_system(requests: &[Arc<ModelRequest>]) -> String {
    system_text(requests.last().unwrap().system.as_deref().unwrap())
}

fn in_order(text: &str, parts: &[&str]) {
    let mut at = 0;
    for part in parts {
        let found = text[at..]
            .find(part)
            .unwrap_or_else(|| panic!("{part:?} missing or out of order in:\n{text}"));
        at += found + part.len();
    }
}

/// The text of every keyed block delivered under `key`, oldest first.
async fn delivered(session: &Session, key: &str) -> Vec<String> {
    inputs(session)
        .await
        .iter()
        .flat_map(|input| &input.blocks)
        .filter_map(|block| match block {
            InputBlock::Raw {
                text,
                key: Some(found),
                ..
            } if found == key => Some(text.clone()),
            _ => None,
        })
        .collect()
}

const PROJECT_RULE: &str =
    "rule:dim/instructions/project:{\"kind\":\"branch\",\"timeline\":\"main\"}";
const ENVIRONMENT: &str =
    "state:dim/environment/context:{\"kind\":\"branch\",\"timeline\":\"main\"}";

async fn until_delivered(session: &Session, key: &str, count: usize) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while delivered(session, key).await.len() < count {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("periodic check delivered the change");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn system_is_static_and_instructions_and_environment_arrive_as_inputs() {
    let dirs = Dirs::new();
    let cwd = dirs.dir("root/a/b");
    dirs.write(&dirs.root.join("root/AGENTS.md"), "PROJECT-P\n");
    dirs.write(&dirs.home.join("AGENTS.md"), "USER-U");
    dirs.write(
        &dirs.home.join(".agents/skills/review/SKILL.md"),
        &skill_file("review", "Review a diff", "REVIEW"),
    );
    let env = env(&dirs, Options::default()).await;
    let session = env.session(&cwd, "SESSION-S").await.unwrap();
    run(&session, "hello").await;

    let sent = requests(&session).await;
    let system = last_system(&sent);
    in_order(
        &system,
        &[
            "You are DimAgent.",
            "Tone and communication:",
            "<skills_instructions>",
            "SESSION-S",
        ],
    );
    for absent in [
        "PROJECT-P",
        "USER-U",
        "<cwd>",
        "environment_context",
        "- review",
        "current_date",
        "<system-reminder>",
        "TodoWrite",
    ] {
        assert!(!system.contains(absent), "{absent} in system:\n{system}");
    }
    in_order(
        &request_text(&sent[0]),
        &[
            "<dim-rule source=\"dim/instructions/project\" scope=\"branch\" op=\"set\">\n",
            &format!(
                "# Project instructions ({}; applies to this directory and below)\nPROJECT-P",
                dirs.root.join("root/AGENTS.md").display()
            ),
            "</dim-rule>",
            "<dim-rule source=\"dim/instructions/user\" scope=\"branch\" op=\"set\">\n",
            "# User instructions (~/AGENTS.md)\nUSER-U",
            "<dim-state source=\"dim/environment/context\">\n",
            &format!("<cwd>{}</cwd>", cwd.display()),
            &format!("<home>{}</home>", dirs.home.display()),
            "<shell>/bin/zsh</shell>",
            "<shell_edition>zsh</shell_edition>",
            "<shell_output_encoding>utf8</shell_output_encoding>",
            "<platform>linux</platform>",
            "<timezone>Asia/Shanghai</timezone>",
            "\n</dim-state>",
            "<dim-state source=\"dim/environment/date\">2026-09-30</dim-state>",
            "<dim-state source=\"dim/skills/list\">\n- review: Review a diff\n</dim-state>",
            "hello",
        ],
    );
    run(&session, "again").await;
    let sent = requests(&session).await;
    assert!(sent.windows(2).all(|pair| pair[0].system == pair[1].system));
    env.agent.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_instructions_appear_once_and_absent_files_produce_no_entry() {
    let dirs = Dirs::new();
    let cwd = dirs.home.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    dirs.write(&dirs.home.join("AGENTS.md"), "USER-U");
    let env = env(&dirs, Options::default()).await;
    let session = env.session(&cwd, "").await.unwrap();
    run(&session, "hello").await;
    let text = request_text(&requests(&session).await[0]);
    assert_eq!(text.matches("USER-U").count(), 1, "{text}");

    let bare = dirs.dir("bare");
    std::fs::remove_file(dirs.home.join("AGENTS.md")).unwrap();
    let session = env.session(&bare, "").await.unwrap();
    let changes = prompt_changes(&chain_events(&session, "main", ..).await.unwrap());
    let sources: Vec<_> = changes
        .iter()
        .flat_map(|data| &data.changes)
        .map(|change| match change {
            PromptChange::Set { entry } => entry.source.clone(),
            PromptChange::Revoke { source, .. } => source.clone(),
        })
        .collect();
    assert_eq!(
        sources,
        [
            "dim/base",
            "dim/environment/context",
            "dim/environment/date",
            "dim/skills/instructions"
        ]
    );
    env.agent.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agents_md_changes_arrive_as_rules_from_the_periodic_check() {
    let dirs = Dirs::new();
    let cwd = dirs.dir("work");
    let project = cwd.join("AGENTS.md");
    dirs.write(&project, "RULE-ONE");
    let env = env(&dirs, Options::default()).await;
    let session = env.session(&cwd, "").await.unwrap();
    run(&session, "first").await;

    dirs.write(&project, "RULE-TWO");
    until_delivered(&session, PROJECT_RULE, 2).await;
    run(&session, "second").await;
    let sent = requests(&session).await;
    let latest = request_text(sent.last().unwrap());
    in_order(
        &latest,
        &[
            "RULE-ONE",
            "first",
            "op=\"replace\">\n",
            "RULE-TWO",
            "second",
        ],
    );
    assert!(sent.iter().all(|request| {
        let system = system_text(request.system.as_deref().unwrap());
        !system.contains("RULE-ONE") && !system.contains("RULE-TWO")
    }));
    assert_eq!(
        session.configure(refresh_update(), "main").await.unwrap(),
        ConfigureOutcome::Unchanged
    );

    std::fs::remove_file(&project).unwrap();
    until_delivered(&session, PROJECT_RULE, 3).await;
    let changes = prompt_changes(&chain_events(&session, "main", ..).await.unwrap());
    assert_eq!(
        changes.last().unwrap().changes,
        vec![PromptChange::Revoke {
            source: "dim/instructions/project".into(),
            scope: PromptScope::Branch {
                timeline: "main".into()
            },
        }]
    );
    run(&session, "third").await;
    in_order(
        &request_text(requests(&session).await.last().unwrap()),
        &[
            "second",
            "op=\"revoke\">\nThis rule no longer applies to branch.",
            "third",
        ],
    );
    env.agent.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restored_session_receives_the_changed_environment_once() {
    let dirs = Dirs::new();
    let cwd = dirs.dir("work");
    let store = Arc::new(MemoryTimelineStore::default());
    let env = env(
        &dirs,
        Options {
            store: Some(store.clone()),
            ..Options::default()
        },
    )
    .await;
    let session = env.session(&cwd, "").await.unwrap();
    let id = session.id().to_owned();
    run(&session, "first").await;
    let system = last_system(&requests(&session).await);
    env.agent.close().await.unwrap();

    for (timezone, environments) in [
        ("Asia/Shanghai", 1),
        ("Europe/Berlin", 2),
        ("Europe/Berlin", 2),
    ] {
        let env = local_support::env(
            &dirs,
            Options {
                store: Some(store.clone()),
                timezone,
                ..Options::default()
            },
        )
        .await;
        let session = env.agent.restore_session(id.clone()).await.unwrap();
        run(&session, "after restart").await;
        let sent = delivered(&session, ENVIRONMENT).await;
        assert_eq!(sent.len(), environments);
        assert!(
            sent.last()
                .unwrap()
                .contains(&format!("<timezone>{timezone}</timezone>"))
        );
        let last = requests(&session).await.pop().unwrap();
        assert_eq!(system_text(last.system.as_deref().unwrap()), system);
        env.agent.close().await.unwrap();
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unreadable_project_instructions_keep_the_adopted_version() {
    use std::os::unix::fs::PermissionsExt;
    let dirs = Dirs::new();
    let cwd = dirs.dir("work");
    let project = cwd.join("AGENTS.md");
    dirs.write(&project, "SECRET-P");
    let env = env(&dirs, Options::default()).await;
    let session = env.session(&cwd, "").await.unwrap();
    run(&session, "first").await;

    std::fs::set_permissions(&project, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&project).is_ok() {
        return;
    }
    let before = prompt_changes(&chain_events(&session, "main", ..).await.unwrap()).len();
    let error = session
        .configure(refresh_update(), "main")
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("dim/instructions/project: AGENTS.md not readable"),
        "{error}"
    );
    assert!(!error.contains("SECRET-P") && !error.contains(&*dirs.root.to_string_lossy()));
    assert_eq!(
        prompt_changes(&chain_events(&session, "main", ..).await.unwrap()).len(),
        before
    );
    run(&session, "second").await;
    assert_eq!(delivered(&session, PROJECT_RULE).await.len(), 1);
    assert!(request_text(&requests(&session).await.pop().unwrap()).contains("SECRET-P"));

    let error = env.session(&cwd, "").await.err().unwrap().to_string();
    assert!(error.contains("not readable"), "{error}");
    assert!(!error.contains("SECRET-P") && !error.contains(&*dirs.root.to_string_lossy()));
    env.agent.close().await.unwrap();
}

fn date(input: &InputMessageData) -> Option<Option<String>> {
    state_value(input, "dim/environment/date")
}

/// Every date reminder of `main`, oldest first.
async fn dates(session: &Session) -> Vec<String> {
    inputs(session)
        .await
        .iter()
        .filter_map(|input| date(input).flatten())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_loaded_branch_appends_a_date_reminder_when_the_day_changes() {
    let dirs = Dirs::new();
    let cwd = dirs.dir("work");
    let store = Arc::new(MemoryTimelineStore::default());
    let options = Options {
        store: Some(store.clone()),
        ..Options::default()
    };
    let today = options.today.clone();
    let env = env(&dirs, options).await;
    let session = env.session(&cwd, "").await.unwrap();
    let id = session.id().to_owned();
    run(&session, "first").await;
    run(&session, "second").await;

    let sent = inputs(&session).await;
    let user = sent.iter().position(|input| input.user.is_some()).unwrap();
    let at = sent.iter().position(|input| date(input).is_some()).unwrap();
    assert!(at < user);
    assert_eq!(date(&sent[at]), Some(Some("2026-09-30".into())));
    assert_eq!(sent[at].mode, Some(InputMode::Append));
    assert_eq!(dates(&session).await, ["2026-09-30"]);
    assert!(
        request_text(&requests(&session).await[0])
            .contains("<dim-state source=\"dim/environment/date\">2026-09-30</dim-state>")
    );

    *today.lock().unwrap() = "2026-10-01".into();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while dates(&session).await.len() < 2 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("date reminder appended");
    assert_eq!(dates(&session).await, ["2026-09-30", "2026-10-01"]);
    assert_eq!(requests(&session).await.len(), 2);
    run(&session, "third").await;
    let last = requests(&session).await.pop().unwrap();
    assert!(request_text(&last).contains("2026-10-01"));
    let system = last_system(&requests(&session).await);
    assert!(!system.contains("2026-09-30") && !system.contains("2026-10-01"));
    env.agent.close().await.unwrap();

    let options = Options {
        store: Some(store),
        today,
        ..Options::default()
    };
    let env = local_support::env(&dirs, options).await;
    let session = env.agent.restore_session(id).await.unwrap();
    run(&session, "after restart").await;
    assert_eq!(dates(&session).await, ["2026-09-30", "2026-10-01"]);
    env.agent.close().await.unwrap();
}
