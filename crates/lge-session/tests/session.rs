//! Planning and running a task with a stand-in for the agent.
use lge_core::memory_kind;
use lge_retrieve::IndexOptions;
use lge_session::*;
use lge_store::{GraphStore, Store};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn repo(check_command: &str) -> (tempfile::TempDir, PathBuf, Store) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    copy_dir(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/polyglot"),
        &root,
    );
    std::fs::write(
        root.join("lge.toml"),
        format!("[[check]]\nname = \"suite\"\nkind = \"tests\"\ncommand = \"{check_command}\"\n"),
    )
    .unwrap();
    let store = Store::open(&dir.path().join("graph.lbug")).unwrap();
    (dir, root, store)
}

/// Answers every session with the same events and remembers what it was asked.
struct Fake {
    asked: Mutex<Vec<SessionRequest>>,
}

impl SessionRunner for Fake {
    fn name(&self) -> &str {
        "fake"
    }

    fn run(&self, request: &SessionRequest) -> anyhow::Result<SessionResult> {
        self.asked.lock().unwrap().push(request.clone());
        let events = "{\"type\":\"thread.started\",\"thread_id\":\"thread-1\"}\n\
            {\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"Changed orders_api/routes.py to reject an empty payload. Tests pass.\"}}\n\
            {\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":5000,\"cached_input_tokens\":4000,\"output_tokens\":700,\"reasoning_output_tokens\":500}}\n";
        Ok(SessionResult {
            events: events.to_string(),
            exit_ok: true,
        })
    }
}

#[test]
fn the_prompt_starts_with_the_same_bytes_for_every_task() {
    let (_dir, root, store) = repo("true");
    let cfg = SessionConfig::default();
    let a = plan_task(
        &store,
        &root,
        "create_order must validate the payload",
        &cfg,
        &IndexOptions::default(),
    )
    .unwrap();
    let b = plan_task(
        &store,
        &root,
        "handleOrderCreated should retry the email",
        &cfg,
        &IndexOptions::default(),
    )
    .unwrap();
    for plan in [&a, &b] {
        assert!(plan.prompt.starts_with(STABLE_PREFIX));
        assert!(plan.prompt[STABLE_PREFIX.len()..].starts_with("\n# Context\n# Context for: "));
        assert!(
            plan.prompt.trim_end().ends_with(&format!("# Task\n{}", plan.task)),
            "the task comes last"
        );
        assert_eq!(plan.level, lge_core::Level::Top, "fixed mode keeps the user's level");
        assert!(plan.prompt_tokens > plan.prefix_tokens + plan.pack.tokens);
    }
    assert_ne!(a.pack.text, b.pack.text);
    assert_eq!(a.route.level, lge_core::Level::Top);
    assert!(plan_task(&store, &root, "  ", &cfg, &IndexOptions::default()).is_err());
    // Fixed mode never passes an effort; routed mode passes the configured one.
    assert_eq!(cfg.effort_for(lge_core::Level::Low), None);
    let routed = SessionConfig {
        level_mode: LevelMode::Routed,
        ..SessionConfig::default()
    };
    assert_eq!(routed.effort_for(lge_core::Level::Low).as_deref(), Some("low"));
    assert_eq!(
        routed.effort_for(lge_core::Level::Top),
        None,
        "top keeps the user's own effort"
    );
}

#[test]
fn a_run_is_measured_and_leaves_a_handoff() {
    let (_dir, root, store) = repo("true");
    let fake = Fake {
        asked: Mutex::new(Vec::new()),
    };
    let out = run_task(
        &store,
        &root,
        "create_order must validate the payload",
        &SessionConfig::default(),
        &IndexOptions::default(),
        &fake,
    )
    .unwrap();
    assert_eq!(out.attempts.len(), 1);
    assert_eq!(out.checks_passed, Some(true));
    let asked = fake.asked.lock().unwrap();
    assert_eq!(
        (asked.len(), asked[0].effort.clone(), asked[0].cwd.clone()),
        (1, None, root.clone())
    );

    let stored = store.list_usage(0).unwrap();
    assert_eq!(stored.len(), 1);
    let rec = &stored[0];
    assert_eq!(
        (
            rec.input_tokens,
            rec.cached_input_tokens,
            rec.output_tokens,
            rec.reasoning_tokens
        ),
        (5000, 4000, 700, 500)
    );
    assert_eq!(
        (
            rec.level.as_str(),
            rec.recommended_level.as_str(),
            rec.mode.as_str(),
            rec.runner.as_str()
        ),
        ("top", "top", "pack+mcp", "fake")
    );
    assert_eq!(
        (rec.outcome.as_str(), rec.session_id.as_str(), rec.attempt),
        ("success", "thread-1", 1)
    );
    assert!(rec.pack_tokens > 0 && rec.services.contains(&"orders_api".to_string()));

    let note = &store.latest_handoffs(1).unwrap()[0];
    assert!(note.text.starts_with("Changed orders_api/routes.py") && note.task.starts_with("create_order"));
    let remembered = store.recent_memories(Some(memory_kind::EXPERIENCE), &[], 5).unwrap();
    assert!(
        remembered[0]
            .text
            .starts_with("Task: create_order must validate the payload Result: Changed orders_api/routes.py"),
        "{}",
        remembered[0].text
    );
    assert!(store.get_page("feature-work").unwrap().is_some());

    // The next session starts from that handoff.
    let next = plan_task(
        &store,
        &root,
        "get_order should return 404 for a missing order",
        &SessionConfig::default(),
        &IndexOptions::default(),
    )
    .unwrap();
    assert!(
        next.prompt.contains("# From earlier sessions\n") && next.prompt.contains("Changed orders_api/routes.py"),
        "{}",
        next.prompt
    );
}

#[test]
fn failed_checks_retry_one_level_higher_with_only_the_failures() {
    let (_dir, root, store) = repo("echo 'FAILED tests/test_x.py::test_y - assert 1 == 2'; exit 1");
    let fake = Fake {
        asked: Mutex::new(Vec::new()),
    };
    let cfg = SessionConfig {
        level_mode: LevelMode::Routed,
        ..SessionConfig::default()
    };
    // One file in one service, every edge verified: the router says medium.
    let out = run_task(
        &store,
        &root,
        "find_order should return None for a missing row",
        &cfg,
        &IndexOptions::default(),
        &fake,
    )
    .unwrap();
    assert_eq!(out.checks_passed, Some(false));
    let levels: Vec<(&str, bool, &str)> = out
        .attempts
        .iter()
        .map(|a| (a.level.as_str(), a.escalated, a.outcome.as_str()))
        .collect();
    assert_eq!(levels, [("medium", false, "failure"), ("top", true, "failure")]);
    let asked = fake.asked.lock().unwrap();
    assert_eq!(
        (asked[0].effort.as_deref(), asked[1].effort.as_deref()),
        (Some("medium"), None)
    );
    assert!(!asked[0].prompt.contains("The previous attempt failed"));
    assert!(
        asked[1].prompt.contains("# The previous attempt failed these checks\n") && asked[1].prompt.contains("test_y"),
        "{}",
        asked[1].prompt
    );
    assert!(asked[1].prompt.starts_with(STABLE_PREFIX));
    assert_eq!(store.list_usage(0).unwrap().len(), 2);
}
