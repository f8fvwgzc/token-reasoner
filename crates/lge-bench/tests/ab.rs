//! The A/B runner with a stand-in agent and a stand-in daemon: no Codex, no
//! provider. The copies are real git worktrees of a small temporary repository.

use anyhow::{bail, Result};
use lge_bench::ab::{
    clustered_interval, median, parse_task_file, parse_tasks, prompt_with_note, render, resolve_commit, resolve_note,
    run_ab, run_preflight, summarize, summarize_with, verdict, AbEnv, AbOptions, AbTask, Arm, ArmRun, DaemonApi,
    HttpDaemon, Tally, Variant, Weights, BOOTSTRAP_RESAMPLES, BOOTSTRAP_SEED, DEFAULT_NOTE, DEFAULT_WEIGHTS,
    NOISE_SPREAD, RESULTS_FILE, TRACES_DIR,
};
use lge_session::{SessionRequest, SessionResult, SessionRunner};
use lge_usage::TokenUsage;
use serde_json::{json, Value};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

// ---- a stand-in daemon -------------------------------------------------------

#[derive(Default)]
struct State {
    settings: Value,
    /// Every settings object posted, in order.
    sets: Vec<Value>,
    repos: Vec<String>,
    added: Vec<String>,
    removed: Vec<String>,
    /// Model requests of the one conversation the daemon lists.
    requests: u64,
    /// The n-th settings post (1-based) is refused.
    refuse_set: Option<usize>,
    workspaces: Vec<String>,
    /// Registered copies stay in the `indexing` state.
    never_ready: bool,
    /// Answers `requests()` with a trace for each thread it is asked about.
    traces: bool,
    /// Threads whose trace was asked for.
    traced: Vec<String>,
}

struct FakeDaemon(Mutex<State>);

fn original_settings() -> Value {
    json!({ "inject_context": true, "shorten_outputs": false, "shorten_reads": false, "context_budget": 1500, "auto_index": true })
}

impl FakeDaemon {
    fn new() -> FakeDaemon {
        FakeDaemon(Mutex::new(State {
            settings: original_settings(),
            ..State::default()
        }))
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl DaemonApi for FakeDaemon {
    fn status(&self) -> Result<Value> {
        let s = self.state();
        let repos: Vec<Value> = s
            .repos
            .iter()
            .map(|r| json!({ "root": r, "state": if s.never_ready { "indexing" } else { "ready" }, "detail": "" }))
            .collect();
        Ok(json!({
            "settings": s.settings,
            "workspaces": s.workspaces,
            "repos": repos,
            "sessions": [{ "id": "conv", "requests": s.requests }],
        }))
    }

    fn set_settings(&self, settings: &Value) -> Result<()> {
        let mut s = self.state();
        s.sets.push(settings.clone());
        if s.refuse_set == Some(s.sets.len()) {
            bail!("the daemon is gone");
        }
        s.settings = settings.clone();
        Ok(())
    }

    fn add_repo(&self, path: &Path) -> Result<String> {
        let root = path.canonicalize()?.to_string_lossy().to_string();
        let mut s = self.state();
        s.repos.push(root.clone());
        s.added.push(root.clone());
        Ok(root)
    }

    fn remove_repo(&self, root: &str) -> Result<()> {
        let mut s = self.state();
        s.repos.retain(|r| r != root);
        s.removed.push(root.to_string());
        Ok(())
    }

    fn requests(&self, thread: &str) -> Result<Value> {
        let mut s = self.state();
        if !s.traces {
            bail!("no traces here");
        }
        s.traced.push(thread.to_string());
        let big = if thread.ends_with("on") { 2_000 } else { 9_000 };
        Ok(json!({ "thread": thread, "known": true, "requests": [
            { "request": 1, "status": 200, "usage_found": true, "input_tokens": 4_000, "cached_input_tokens": 0,
              "output_tokens": 100, "reasoning_tokens": 50, "lge_added_tokens": 0, "lge_removed_tokens": 0,
              "largest_outputs": [{ "command": "rg -n create_order", "tokens": 1_200, "shortened": false }] },
            { "request": 2, "status": 200, "usage_found": true, "input_tokens": 8_000, "cached_input_tokens": 3_000,
              "output_tokens": 100, "reasoning_tokens": 50, "lge_added_tokens": 0, "lge_removed_tokens": 7_000,
              "largest_outputs": [{ "command": "cargo test", "tokens": big, "shortened": thread.ends_with("on") }] }
        ]}))
    }
}

// ---- a stand-in agent --------------------------------------------------------

/// What the agent saw when its session started.
#[derive(Debug, Clone)]
struct Seen {
    /// The first line of the prompt: the task's own prompt.
    task: String,
    prompt: String,
    cwd: PathBuf,
    arm: Arm,
    settings: Value,
    /// The daemon listed the session's folder.
    registered: bool,
    setup_ran: bool,
    sandbox: Option<String>,
    approve_for_me: bool,
    /// `moved.txt` in the copy: what a session committed to the repository.
    moved: Option<String>,
    /// AGENTS.md at the top of the copy.
    agents_md: Option<String>,
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Normal,
    Fail,
    /// The agent cannot start in this arm only.
    FailIn(Arm),
    Panic,
    /// The session reports no usage (it was killed before `turn.completed`)
    /// but its edit is there.
    NoUsageIn(Arm),
}

struct FakeAgent<'a> {
    daemon: &'a FakeDaemon,
    seen: RefCell<Vec<Seen>>,
    mode: Mode,
    /// What it answers when asked whether it has `--approve-for-me`.
    approve_for_me: bool,
    /// Times it was asked.
    approve_asked: Cell<usize>,
    /// Each session commits a new file to this repository: the branch moves during the run.
    move_branch: Option<PathBuf>,
}

impl<'a> FakeAgent<'a> {
    fn new(daemon: &'a FakeDaemon) -> FakeAgent<'a> {
        FakeAgent {
            daemon,
            seen: RefCell::new(Vec::new()),
            mode: Mode::Normal,
            approve_for_me: false,
            approve_asked: Cell::new(0),
            move_branch: None,
        }
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.borrow().clone()
    }
}

/// The token counts the stand-in reports: lge on saves 30% of input.
fn usage_for(arm: Arm) -> (i64, i64) {
    match arm {
        Arm::Off => (9_000, 1_000),
        Arm::On => (6_000, 1_000),
    }
}

impl SessionRunner for FakeAgent<'_> {
    fn name(&self) -> &str {
        "fake"
    }

    fn approve_for_me_available(&self) -> bool {
        self.approve_asked.set(self.approve_asked.get() + 1);
        self.approve_for_me
    }

    fn run(&self, request: &SessionRequest) -> Result<SessionResult> {
        let settings = self.daemon.state().settings.clone();
        let arm = if settings["inject_context"] == true {
            Arm::On
        } else {
            Arm::Off
        };
        let cwd = request.cwd.clone();
        assert!(
            cwd.ancestors().any(|d| d.join(".git").is_file()),
            "the session runs in a worktree: {cwd:?}"
        );
        assert!(cwd.join("README.md").is_file(), "the copy holds HEAD");
        let registered = self.daemon.state().repos.iter().any(|r| Path::new(r) == cwd);
        self.seen.borrow_mut().push(Seen {
            task: request.prompt.lines().next().unwrap_or_default().to_string(),
            prompt: request.prompt.clone(),
            cwd: cwd.clone(),
            arm,
            settings,
            registered,
            setup_ran: cwd.join("setup.txt").exists(),
            sandbox: request.sandbox.clone(),
            approve_for_me: request.approve_for_me,
            moved: std::fs::read_to_string(cwd.join("moved.txt")).ok(),
            agents_md: std::fs::read_to_string(cwd.join("AGENTS.md")).ok(),
        });
        if let Some(repo) = &self.move_branch {
            let n = self.seen.borrow().len();
            std::fs::write(repo.join("moved.txt"), format!("{n}\n")).unwrap();
            git(repo, &["add", "moved.txt"]);
            git(repo, &["commit", "-q", "-m", &format!("move {n}")]);
        }
        match self.mode {
            Mode::Fail => bail!("the agent crashed"),
            Mode::FailIn(a) if a == arm => bail!("the agent crashed"),
            Mode::Panic => panic!("the agent panicked"),
            _ => {}
        }
        // The agent's work: the edit the check looks for.
        std::fs::write(cwd.join("done.txt"), arm.as_str()).unwrap();
        self.daemon.state().requests += 3;
        let (input, output) = usage_for(arm);
        let events = format!(
            "{{\"type\":\"thread.started\",\"thread_id\":\"t-{arm}\"}}\n{{\"type\":\"turn.started\"}}\n\
             {{\"type\":\"turn.completed\",\"usage\":{{\"input_tokens\":{input},\"cached_input_tokens\":{},\"output_tokens\":{output},\"reasoning_output_tokens\":400}}}}\n",
            input / 2
        );
        if self.mode == Mode::NoUsageIn(arm) {
            return Ok(SessionResult {
                events: format!("{{\"type\":\"thread.started\",\"thread_id\":\"t-{arm}\"}}\n"),
                exit_ok: false,
            });
        }
        Ok(SessionResult { events, exit_ok: true })
    }
}

// ---- helpers -------------------------------------------------------------------

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=lge test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// A repository with one commit, and a folder for the copies next to it.
fn repository() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    std::fs::write(repo.join("README.md"), "# demo\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let repo = repo.canonicalize().unwrap();
    let work = dir.path().canonicalize().unwrap().join("copies");
    (dir, repo, work)
}

fn task(name: &str, check: &str) -> AbTask {
    AbTask {
        name: name.to_string(),
        prompt: format!("do {name}"),
        check: check.to_string(),
        setup: Some("touch setup.txt".to_string()),
    }
}

/// The options of the runner tests: the preflight is off (most of their
/// checks pass on the base commit); its own tests turn it on.
fn options(work: &Path) -> AbOptions {
    let mut o = AbOptions::new(work.to_path_buf());
    o.poll = Duration::from_millis(1);
    o.preflight = false;
    o
}

fn worktrees(repo: &Path) -> usize {
    git(repo, &["worktree", "list", "--porcelain"])
        .lines()
        .filter(|l| l.starts_with("worktree "))
        .count()
}

fn never() -> bool {
    false
}

/// Forget the results of an earlier run in `work`, so the next run starts
/// afresh (a run refuses a work dir that holds another run's results).
fn clear(work: &Path) {
    let _ = std::fs::remove_file(work.join(RESULTS_FILE));
    let _ = std::fs::remove_dir_all(work.join(TRACES_DIR));
}

/// What a run leaves in its work dir: the results file, and no copy.
fn only_results(work: &Path) {
    let mut left: Vec<String> = std::fs::read_dir(work)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    left.sort();
    assert_eq!(left, [RESULTS_FILE], "{work:?}");
}

fn quiet(_: &str) {}

// ---- the runner ----------------------------------------------------------------

#[test]
fn arms_alternate_and_each_session_sees_its_arm() {
    let (_dir, repo, work) = repository();
    let daemon = FakeDaemon::new();
    let agent = FakeAgent::new(&daemon);
    let tasks = [
        task("one", "test -f done.txt"),
        task("two", "test -f done.txt"),
        task("three", "test -f done.txt"),
    ];
    let env = AbEnv {
        daemon: &daemon,
        runner: &agent,
        stop: &never,
        log: &quiet,
    };
    let report = run_ab(&repo, &tasks, &options(&work), &env).unwrap();

    let seen = agent.seen();
    let order: Vec<(&str, Arm)> = seen.iter().map(|s| (s.task.as_str(), s.arm)).collect();
    assert_eq!(
        order,
        [
            ("do one", Arm::Off),
            ("do one", Arm::On),
            ("do two", Arm::On),
            ("do two", Arm::Off),
            ("do three", Arm::Off),
            ("do three", Arm::On),
        ]
    );
    for s in &seen {
        let on = s.arm == Arm::On;
        assert_eq!(s.settings["shorten_outputs"], on, "{s:?}");
        assert_eq!(s.settings["shorten_reads"], false, "{s:?}");
        assert_eq!(s.settings["auto_index"], false, "{s:?}");
        assert_eq!(s.settings["holdout_share"], 0.0, "no holdout in either arm: {s:?}");
        assert_eq!(s.settings["inject_context"], on, "{s:?}");
        assert_eq!(s.settings["context_budget"], 1500, "other settings are kept");
        assert_eq!(s.agents_md, None, "the features variant leaves AGENTS.md as committed");
        // The arm with features on starts in a copy whose graph is ready.
        assert_eq!(s.registered, on, "{s:?}");
        assert!(s.setup_ran, "setup runs before the session");
        assert_eq!(s.sandbox.as_deref(), Some("workspace-write"));
        assert!(!s.approve_for_me, "this agent has no --approve-for-me");
        assert_eq!(
            s.prompt,
            format!("{}\n\n{DEFAULT_NOTE}", s.task),
            "the note ends every prompt"
        );
    }
    assert_eq!(agent.approve_asked.get(), 1, "asked once per run");
    assert_eq!(daemon.state().settings, original_settings(), "settings are back");

    assert_eq!(report.runs.len(), 6);
    assert!(
        report.runs.iter().all(|r| r.success && r.error.is_none()),
        "{report:#?}"
    );
    let first = &report.runs[0];
    assert_eq!((first.arm, first.position), (Arm::Off, 1));
    assert_eq!(
        (
            first.usage.input_tokens,
            first.usage.cached_input_tokens,
            first.usage.output_tokens
        ),
        (9_000, 4_500, 1_000)
    );
    assert_eq!((first.total_tokens, first.turns), (10_000, 1));
    assert_eq!((first.requests, first.proxied), (Some(3), Some(true)));
    assert_eq!(report.runs[2].position, 1, "the second task starts with lge on");
    assert_eq!(report.compared, 3);
    assert!((report.median_change.unwrap() + 0.3).abs() < 1e-9, "{report:#?}");
    assert_eq!(report.verdict.met, Some(true), "{}", report.verdict.text);
    assert!(!report.interrupted && report.settings_restored);
}

#[test]
fn runs_of_one_task_alternate_too() {
    let (_dir, repo, work) = repository();
    let daemon = FakeDaemon::new();
    let agent = FakeAgent::new(&daemon);
    let mut opts = options(&work);
    opts.runs = 2;
    let env = AbEnv {
        daemon: &daemon,
        runner: &agent,
        stop: &never,
        log: &quiet,
    };
    let report = run_ab(&repo, &[task("one", "true")], &opts, &env).unwrap();
    let arms: Vec<Arm> = agent.seen().iter().map(|s| s.arm).collect();
    assert_eq!(arms, [Arm::Off, Arm::On, Arm::On, Arm::Off]);
    let runs: Vec<usize> = report.runs.iter().map(|r| r.run).collect();
    assert_eq!(runs, [1, 1, 2, 2]);
    assert_eq!(report.tasks[0].off.runs, 2);
}

#[test]
fn every_copy_is_removed_and_dropped_from_the_daemon() {
    let (_dir, repo, work) = repository();
    let daemon = FakeDaemon::new();
    let agent = FakeAgent::new(&daemon);
    let env = AbEnv {
        daemon: &daemon,
        runner: &agent,
        stop: &never,
        log: &quiet,
    };
    run_ab(
        &repo,
        &[task("one", "true"), task("two", "true")],
        &options(&work),
        &env,
    )
    .unwrap();

    let seen = agent.seen();
    let mut folders: Vec<&PathBuf> = seen.iter().map(|s| &s.cwd).collect();
    folders.dedup();
    assert_eq!(folders.len(), 4, "a fresh copy for every session");
    for s in &seen {
        assert!(!s.cwd.exists(), "{:?} was removed", s.cwd);
        assert!(s.cwd.starts_with(&work), "{:?}", s.cwd);
    }
    // Every copy is gone; the results file stays, for a resume.
    only_results(&work);
    assert_eq!(worktrees(&repo), 1, "git forgot the copies");
    let state = daemon.state();
    assert_eq!(
        state.added.len(),
        2,
        "one registered copy per task, for the arm with lge on"
    );
    assert_eq!(state.added, state.removed);
    assert!(state.repos.is_empty());
}

#[test]
fn the_check_decides_success_and_a_failed_session_does_not_stop_the_run() {
    let (_dir, repo, work) = repository();
    let daemon = FakeDaemon::new();
    let mut agent = FakeAgent::new(&daemon);
    let env_tasks = [task("done", "test -f done.txt"), task("wrong", "echo nope; exit 3")];
    let report = {
        let env = AbEnv {
            daemon: &daemon,
            runner: &agent,
            stop: &never,
            log: &quiet,
        };
        run_ab(&repo, &env_tasks, &options(&work), &env).unwrap()
    };
    assert!(report.runs[..2].iter().all(|r| r.success));
    let wrong = &report.runs[2];
    assert_eq!((wrong.check_passed, wrong.success), (Some(false), false));
    assert!(wrong.check_tail.contains("nope"), "{wrong:?}");
    assert_eq!(report.compared, 1);

    // An agent that cannot run: every session is recorded with the error.
    clear(&work);
    agent.mode = Mode::Fail;
    let env = AbEnv {
        daemon: &daemon,
        runner: &agent,
        stop: &never,
        log: &quiet,
    };
    let report = run_ab(&repo, &[task("done", "true")], &options(&work), &env).unwrap();
    assert_eq!(report.runs.len(), 2);
    assert!(report
        .runs
        .iter()
        .all(|r| !r.success && r.error.as_deref().unwrap().contains("the agent crashed")));
    assert_eq!(report.verdict.met, None);
    assert_eq!(daemon.state().settings, original_settings());
    assert_eq!(worktrees(&repo), 1);

    // A setup that fails is an error of that session, before the agent starts.
    agent.mode = Mode::Normal;
    let env = AbEnv {
        daemon: &daemon,
        runner: &agent,
        stop: &never,
        log: &quiet,
    };
    let broken = AbTask {
        setup: Some("echo missing tool >&2; exit 127".to_string()),
        ..task("broken", "true")
    };
    let before = agent.seen().len();
    clear(&work);
    let report = run_ab(&repo, &[broken], &options(&work), &env).unwrap();
    assert_eq!(agent.seen().len(), before, "no session after a failed setup");
    assert!(
        report.runs[0]
            .error
            .as_deref()
            .unwrap()
            .contains("setup failed (exit 127): missing tool"),
        "{:?}",
        report.runs[0]
    );
}

#[test]
fn settings_are_restored_when_the_daemon_refuses_a_switch() {
    let (_dir, repo, work) = repository();
    let daemon = FakeDaemon::new();
    // The first pair switches twice; the third switch is refused.
    daemon.state().refuse_set = Some(3);
    let agent = FakeAgent::new(&daemon);
    let env = AbEnv {
        daemon: &daemon,
        runner: &agent,
        stop: &never,
        log: &quiet,
    };
    let err = run_ab(
        &repo,
        &[task("one", "true"), task("two", "true")],
        &options(&work),
        &env,
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("cannot switch the daemon"), "{err:#}");
    assert_eq!(agent.seen().len(), 2);
    let state = daemon.state();
    assert_eq!(state.settings, original_settings());
    assert_eq!(state.sets.last(), Some(&original_settings()));
    assert!(state.repos.is_empty());
    drop(state);
    assert_eq!(worktrees(&repo), 1);
    only_results(&work);
}

#[test]
fn settings_are_restored_after_ctrl_c() {
    let (_dir, repo, work) = repository();
    let daemon = FakeDaemon::new();
    let agent = FakeAgent::new(&daemon);
    // Ctrl-C arrives while the first session runs.
    let stop = || !agent.seen.borrow().is_empty();
    let env = AbEnv {
        daemon: &daemon,
        runner: &agent,
        stop: &stop,
        log: &quiet,
    };
    let report = run_ab(
        &repo,
        &[task("one", "true"), task("two", "true")],
        &options(&work),
        &env,
    )
    .unwrap();
    assert!(report.interrupted);
    assert_eq!(report.runs.len(), 1);
    assert_eq!(report.runs[0].error.as_deref(), Some("interrupted"));
    assert!(report.runs[0].interrupted);
    assert_eq!(
        report.runs[0].check_passed, None,
        "no check after an interrupted session"
    );
    assert_eq!(report.compared, 0, "a pair cut short is never compared");
    assert_eq!(report.success_off.runs, 0);
    assert_eq!(daemon.state().settings, original_settings());
    assert_eq!(worktrees(&repo), 1);
    assert!(render(&report).contains("interrupted"));

    // Ctrl-C while the check runs: the check's failure is not the agent's.
    clear(&work);
    let agent = FakeAgent::new(&daemon);
    let stop = || {
        agent
            .seen
            .borrow()
            .last()
            .is_some_and(|s| s.cwd.join("checked").exists())
    };
    let env = AbEnv {
        daemon: &daemon,
        runner: &agent,
        stop: &stop,
        log: &quiet,
    };
    let report = run_ab(&repo, &[task("one", "touch checked; exit 1")], &options(&work), &env).unwrap();
    assert!(report.interrupted);
    let run = &report.runs[0];
    assert!(
        run.interrupted && run.check_passed.is_none() && run.check_tail.is_empty(),
        "{run:?}"
    );
    assert_eq!(daemon.state().settings, original_settings());
    assert_eq!(worktrees(&repo), 1);
}

#[test]
fn settings_are_restored_when_the_agent_panics() {
    let (_dir, repo, work) = repository();
    let daemon = FakeDaemon::new();
    let mut agent = FakeAgent::new(&daemon);
    agent.mode = Mode::Panic;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let env = AbEnv {
            daemon: &daemon,
            runner: &agent,
            stop: &never,
            log: &quiet,
        };
        run_ab(&repo, &[task("one", "true")], &options(&work), &env)
    }));
    assert!(result.is_err());
    assert_eq!(daemon.state().settings, original_settings());
    assert_eq!(worktrees(&repo), 1);
}

#[test]
fn a_daemon_that_cannot_index_the_copies_stops_the_run_before_any_session() {
    let (_dir, repo, work) = repository();
    let daemon = FakeDaemon::new();
    daemon.state().workspaces = vec!["/somewhere/else".to_string()];
    let agent = FakeAgent::new(&daemon);
    let env = AbEnv {
        daemon: &daemon,
        runner: &agent,
        stop: &never,
        log: &quiet,
    };
    let err = run_ab(&repo, &[task("one", "true")], &options(&work), &env).unwrap_err();
    assert!(format!("{err:#}").contains("--work-dir"), "{err:#}");
    assert!(agent.seen().is_empty());
    assert_eq!(daemon.state().settings, original_settings());
    assert!(!work.exists());
}

#[test]
fn a_folder_without_a_commit_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let daemon = FakeDaemon::new();
    let agent = FakeAgent::new(&daemon);
    let env = AbEnv {
        daemon: &daemon,
        runner: &agent,
        stop: &never,
        log: &quiet,
    };
    let err = run_ab(
        dir.path(),
        &[task("one", "true")],
        &options(&dir.path().join("w")),
        &env,
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("no commit to copy"), "{err:#}");
    assert!(daemon.state().sets.is_empty(), "the daemon was not touched");
}

// ---- the report ----------------------------------------------------------------

fn result(task: &str, run: usize, arm: Arm, total: i64, success: bool) -> ArmRun {
    ArmRun {
        task: task.to_string(),
        run,
        arm,
        position: 1,
        usage: TokenUsage {
            input_tokens: total - 100,
            cached_input_tokens: 0,
            reasoning_tokens: 50,
            output_tokens: 100,
        },
        total_tokens: total,
        turns: 2,
        requests: Some(2),
        proxied: Some(true),
        session_ok: true,
        check_passed: Some(success),
        success,
        seconds: 1.0,
        error: None,
        interrupted: false,
        check_tail: String::new(),
        ..ArmRun::new(task, run, arm, 1)
    }
}

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

#[test]
fn medians_and_means_are_taken_over_tasks_both_arms_got_right() {
    assert_eq!(median(&[]), None);
    assert_eq!(median(&[3.0, 1.0, 2.0]), Some(2.0));
    assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), Some(2.5));

    let runs = vec![
        result("a", 1, Arm::Off, 1_000, true),
        result("a", 1, Arm::On, 700, true),
        result("b", 1, Arm::On, 1_000, true),
        result("b", 1, Arm::Off, 2_000, true),
        result("c", 1, Arm::Off, 1_000, true),
        result("c", 1, Arm::On, 100, false),
        result("d", 1, Arm::On, 500, true),
        result("d", 1, Arm::Off, 1_000, false),
    ];
    let r = summarize(&names(&["a", "b", "c", "d"]), runs);
    assert_eq!(r.compared, 2);
    assert!((r.median_change.unwrap() + 0.4).abs() < 1e-9, "{r:#?}");
    assert!((r.mean_change.unwrap() + 0.4).abs() < 1e-9);
    assert_eq!(r.tasks[2].change, None, "a failed arm is not compared");
    assert_eq!(r.tasks[2].on.median_total, None);
    assert_eq!((r.success_off.successes, r.success_off.runs), (3, 4));
    assert_eq!((r.success_on.successes, r.success_on.runs), (3, 4));
    assert_eq!(r.verdict.met, Some(true), "{}", r.verdict.text);
    let table = render(&r);
    assert!(table.contains("compared over 2 of 4 task(s)"), "{table}");
    assert!(table.contains("median -40.0%, mean -40.0%"), "{table}");
    assert!(table.contains("verdict: target met"), "{table}");

    // Several runs: the median of the successful runs of each arm.
    let runs = vec![
        result("a", 1, Arm::Off, 1_000, true),
        result("a", 1, Arm::On, 600, true),
        result("a", 2, Arm::On, 800, false),
        result("a", 2, Arm::Off, 1_200, true),
        result("a", 3, Arm::Off, 900, true),
        result("a", 3, Arm::On, 700, true),
        // Pairs cut short by Ctrl-C: listed, not counted.
        result("a", 4, Arm::Off, 5_000, true),
        result("a", 5, Arm::On, 100, true),
        ArmRun {
            interrupted: true,
            ..result("a", 5, Arm::Off, 0, false)
        },
    ];
    let r = summarize(&names(&["a"]), runs);
    assert_eq!(r.tasks[0].off.median_total, Some(1_000.0));
    assert_eq!(r.tasks[0].on.median_total, Some(650.0));
    assert!((r.tasks[0].change.unwrap() + 0.35).abs() < 1e-9);
    assert_eq!((r.success_off.runs, r.success_on.runs), (3, 3));
    assert_eq!(r.runs.len(), 9);
    assert_eq!(r.verdict.met, Some(false), "success dropped from 3/3 to 2/3");
}

#[test]
fn the_verdict_needs_a_quarter_fewer_tokens_and_no_drop_in_success() {
    let tally = |runs, successes| Tally {
        runs,
        successes,
        rate: Some(successes as f64 / runs as f64),
    };
    let met = verdict(Some(-0.25), &tally(4, 3), &tally(4, 3));
    assert_eq!(met.met, Some(true), "{}", met.text);
    assert!(met.text.contains("25.0% lower"), "{}", met.text);

    let short = verdict(Some(-0.2), &tally(4, 4), &tally(4, 4));
    assert_eq!(short.met, Some(false));
    assert!(short.text.contains("median total tokens 20.0% lower"), "{}", short.text);

    let worse = verdict(Some(0.1), &tally(4, 4), &tally(4, 4));
    assert!(worse.text.contains("10.0% higher"), "{}", worse.text);

    let dropped = verdict(Some(-0.6), &tally(4, 4), &tally(4, 3));
    assert_eq!(dropped.met, Some(false));
    assert!(
        dropped.text.contains("success dropped from 100% to 75%"),
        "{}",
        dropped.text
    );

    let better = verdict(Some(-0.3), &tally(4, 2), &tally(4, 3));
    assert_eq!(better.met, Some(true), "more successes with lge is no drop");

    let none = verdict(None, &tally(1, 0), &tally(1, 1));
    assert_eq!(none.met, None);
    assert!(none.text.starts_with("no verdict"), "{}", none.text);
}

#[test]
fn the_example_in_the_documentation_parses() {
    let doc = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/AB.md")).unwrap();
    let start = doc.find("```toml\n").expect("a toml example") + "```toml\n".len();
    let end = start + doc[start..].find("```").unwrap();
    let tasks = parse_tasks(&doc[start..end]).unwrap();
    assert!(tasks.len() >= 2, "{tasks:?}");
    assert!(tasks.iter().any(|t| t.setup.is_some()));
}

// ---- the daemon over HTTP ------------------------------------------------------

/// What the stand-in daemon received: (method, url, body).
type Calls = Arc<Mutex<Vec<(String, String, String)>>>;

/// A stand-in daemon on a local socket: the four endpoints the runner uses.
fn http_daemon() -> (String, Calls) {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", server.server_addr().to_ip().unwrap());
    let log: Calls = Arc::new(Mutex::new(Vec::new()));
    let seen = log.clone();
    std::thread::spawn(move || {
        let mut settings = original_settings();
        for mut request in server.incoming_requests() {
            let mut body = String::new();
            request.as_reader().read_to_string(&mut body).unwrap();
            let method = request.method().as_str().to_string();
            let url = request.url().to_string();
            seen.lock().unwrap().push((method.clone(), url.clone(), body.clone()));
            let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let (status, answer) = match (method.as_str(), url.as_str()) {
                ("GET", "/lge/api/status") => (200, json!({ "settings": settings, "repos": [] })),
                ("POST", "/lge/api/settings") if parsed.get("bad").is_some() => {
                    (400, json!({ "error": "invalid type: map, expected a boolean" }))
                }
                ("POST", "/lge/api/settings") => {
                    settings = parsed.clone();
                    (200, parsed)
                }
                ("POST", "/lge/api/repos") => (
                    200,
                    json!({ "root": format!("/canonical{}", parsed["path"].as_str().unwrap()) }),
                ),
                ("DELETE", "/lge/api/repos") => (200, json!({ "removed": true })),
                ("GET", u) if u.starts_with("/lge/api/sessions/") && u.ends_with("/requests") => (
                    200,
                    json!({ "thread": u.trim_start_matches("/lge/api/sessions/").trim_end_matches("/requests"), "known": true, "requests": [] }),
                ),
                _ => (400, json!({ "error": "no such endpoint" })),
            };
            let response = tiny_http::Response::from_string(answer.to_string()).with_status_code(status);
            request.respond(response).unwrap();
        }
    });
    (base, log)
}

#[test]
fn the_http_client_speaks_the_daemons_page_api() {
    let (base, log) = http_daemon();
    let daemon = HttpDaemon::new(&format!("{base}/")).unwrap();
    assert_eq!(daemon.base(), base);
    assert_eq!(daemon.status().unwrap()["settings"], original_settings());
    let off = json!({ "inject_context": false });
    daemon.set_settings(&off).unwrap();
    assert_eq!(daemon.status().unwrap()["settings"], off);
    assert_eq!(daemon.add_repo(Path::new("/tmp/copy")).unwrap(), "/canonical/tmp/copy");
    daemon.remove_repo("/canonical/tmp/copy").unwrap();
    let trace = daemon.requests("0199a1b2-c3d4-7e5f").unwrap();
    assert_eq!(trace["thread"], "0199a1b2-c3d4-7e5f");
    let calls_before = log.lock().unwrap().len();
    assert!(
        daemon.requests("../status").is_err(),
        "a thread id that is not plain never reaches the URL"
    );
    assert_eq!(log.lock().unwrap().len(), calls_before);
    let err = daemon.set_settings(&json!({ "bad": 1 })).unwrap_err();
    assert!(
        format!("{err:#}").contains("answered 400: invalid type"),
        "the daemon's own message: {err:#}"
    );
    let calls = log.lock().unwrap().clone();
    let delete = calls.iter().find(|(m, _, _)| m == "DELETE").unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&delete.2).unwrap()["path"],
        "/canonical/tmp/copy"
    );

    assert!(
        HttpDaemon::new("127.0.0.1:8787").is_err(),
        "an address without http:// is refused"
    );
    let gone = HttpDaemon::new("http://127.0.0.1:9").unwrap();
    assert!(format!("{:#}", gone.status().unwrap_err()).contains("is `lge serve` running?"));
}

// ---- adversarial ---------------------------------------------------------------

fn env<'a>(daemon: &'a FakeDaemon, agent: &'a FakeAgent<'a>, stop: &'a dyn Fn() -> bool) -> AbEnv<'a> {
    AbEnv {
        daemon,
        runner: agent,
        stop,
        log: &quiet,
    }
}

#[test]
fn each_task_swaps_its_order_from_run_to_run_with_an_even_number_of_tasks() {
    let (_dir, repo, work) = repository();
    let daemon = FakeDaemon::new();
    let agent = FakeAgent::new(&daemon);
    let mut opts = options(&work);
    opts.runs = 2;
    run_ab(
        &repo,
        &[task("one", "true"), task("two", "true")],
        &opts,
        &env(&daemon, &agent, &never),
    )
    .unwrap();
    let order: Vec<(String, Arm)> = agent.seen().into_iter().map(|s| (s.task, s.arm)).collect();
    let first = |name: &str| -> Vec<Arm> {
        order
            .chunks(2)
            .filter(|pair| pair[0].0 == name)
            .map(|pair| pair[0].1)
            .collect()
    };
    assert_eq!(first("do one"), [Arm::Off, Arm::On], "{order:?}");
    assert_eq!(first("do two"), [Arm::On, Arm::Off], "{order:?}");
}

#[test]
fn a_session_that_errors_before_its_check_does_not_count_for_success() {
    let (_dir, repo, work) = repository();
    let daemon = FakeDaemon::new();
    let mut agent = FakeAgent::new(&daemon);
    // The agent cannot start with lge on: no answer about the task, not a failed task.
    agent.mode = Mode::FailIn(Arm::On);
    let report = run_ab(
        &repo,
        &[task("one", "true"), task("two", "true")],
        &options(&work),
        &env(&daemon, &agent, &never),
    )
    .unwrap();
    assert_eq!(report.errored, 2, "{report:#?}");
    assert_eq!((report.success_off.runs, report.success_on.runs), (0, 0), "{report:#?}");
    assert_eq!(report.verdict.met, None, "{}", report.verdict.text);
    assert!(render(&report).contains("2 session(s) ended in an error before their check"));

    // The same in the report alone: an off arm whose setup failed does not
    // make lge look better.
    let runs = vec![
        result("a", 1, Arm::Off, 1_000, true),
        result("a", 1, Arm::On, 600, true),
        ArmRun {
            check_passed: None,
            error: Some("setup failed (exit 1): network".to_string()),
            ..result("b", 1, Arm::Off, 0, false)
        },
        result("b", 1, Arm::On, 600, true),
    ];
    let r = summarize(&names(&["a", "b"]), runs);
    assert_eq!((r.success_off.successes, r.success_off.runs), (1, 1));
    assert_eq!((r.success_on.successes, r.success_on.runs), (1, 1));
    assert_eq!(r.tasks[1].on.runs, 0, "{r:#?}");
    assert_eq!(r.errored, 1);
}

#[test]
fn ctrl_c_that_makes_a_setup_or_a_session_fail_is_an_interruption() {
    let (dir, repo, work) = repository();
    let daemon = FakeDaemon::new();
    let agent = FakeAgent::new(&daemon);
    // Ctrl-C reaches the setup, which fails because of it.
    let flag = dir.path().join("ctrl-c");
    let stop = || flag.exists();
    let cut = AbTask {
        setup: Some(format!("touch '{}'; exit 130", flag.display())),
        ..task("one", "true")
    };
    let report = run_ab(
        &repo,
        &[cut, task("two", "true")],
        &options(&work),
        &env(&daemon, &agent, &stop),
    )
    .unwrap();
    assert!(report.interrupted);
    assert_eq!(report.runs.len(), 1, "{report:#?}");
    let run = &report.runs[0];
    assert!(
        run.interrupted && run.error.as_deref().unwrap().starts_with("interrupted"),
        "{run:?}"
    );
    assert_eq!(report.errored, 0, "an interruption is not an error of the arm");
    assert_eq!(daemon.state().settings, original_settings());
    assert_eq!(worktrees(&repo), 1);

    // The agent dies of the same Ctrl-C and its runner reports an error.
    clear(&work);
    let mut agent = FakeAgent::new(&daemon);
    agent.mode = Mode::Fail;
    let stop = || !agent.seen.borrow().is_empty();
    let report = run_ab(
        &repo,
        &[task("one", "true"), task("two", "true")],
        &options(&work),
        &env(&daemon, &agent, &stop),
    )
    .unwrap();
    assert!(
        report.interrupted && report.runs.len() == 1 && report.runs[0].interrupted,
        "{report:#?}"
    );
    assert_eq!(daemon.state().settings, original_settings());
    assert_eq!(worktrees(&repo), 1);
}

#[test]
fn ctrl_c_while_the_copy_is_indexed_removes_it_everywhere() {
    let (_dir, repo, work) = repository();
    let daemon = FakeDaemon::new();
    daemon.state().never_ready = true;
    let agent = FakeAgent::new(&daemon);
    let polls = std::cell::Cell::new(0);
    // Stop after a few looks at the indexing state of the arm with lge on.
    let stop = || {
        if daemon.state().repos.is_empty() {
            return false;
        }
        polls.set(polls.get() + 1);
        polls.get() > 3
    };
    let report = run_ab(
        &repo,
        &[task("one", "true")],
        &options(&work),
        &env(&daemon, &agent, &stop),
    )
    .unwrap();
    assert!(report.interrupted, "{report:#?}");
    assert_eq!(agent.seen().len(), 1, "only the arm with lge off ran a session");
    let state = daemon.state();
    assert!(
        state.repos.is_empty() && state.added == state.removed,
        "the copy left the daemon"
    );
    assert_eq!(state.settings, original_settings());
    drop(state);
    assert_eq!(worktrees(&repo), 1);
    only_results(&work);

    // A copy that is never ready: an error of that arm after the timeout, cleaned up the same way.
    clear(&work);
    let agent = FakeAgent::new(&daemon);
    let mut opts = options(&work);
    opts.index_timeout = Duration::from_millis(20);
    let report = run_ab(&repo, &[task("one", "true")], &opts, &env(&daemon, &agent, &never)).unwrap();
    let on = report.runs.iter().find(|r| r.arm == Arm::On).unwrap();
    assert!(
        on.error.as_deref().unwrap().contains("did not finish indexing"),
        "{on:?}"
    );
    assert_eq!(report.errored, 1);
    assert!(daemon.state().repos.is_empty());
    assert_eq!(worktrees(&repo), 1);
}

#[test]
fn a_success_without_usage_is_left_out_of_the_medians() {
    let (_dir, repo, work) = repository();
    let daemon = FakeDaemon::new();
    let mut agent = FakeAgent::new(&daemon);
    // The session with lge on was cut before it reported usage, but its edit is
    // there and the check passes: it must not read as a session that cost nothing.
    agent.mode = Mode::NoUsageIn(Arm::On);
    let report = run_ab(
        &repo,
        &[task("one", "test -f done.txt")],
        &options(&work),
        &env(&daemon, &agent, &never),
    )
    .unwrap();
    assert!(report.runs.iter().all(|r| r.success), "{report:#?}");
    assert_eq!(report.unmeasured, 1);
    assert_eq!(report.tasks[0].on.successes, 1);
    assert_eq!(report.tasks[0].on.median_total, None);
    assert_eq!((report.compared, report.median_change), (0, None), "{report:#?}");
    assert_eq!(report.verdict.met, None, "{}", report.verdict.text);
    assert!(render(&report).contains("reported no token usage"));
}

#[test]
fn the_json_report_has_the_same_keys_for_every_session() {
    let runs = vec![
        result("a", 1, Arm::Off, 1_000, true),
        ArmRun {
            check_tail: "assert failed".to_string(),
            ..result("a", 1, Arm::On, 700, false)
        },
        ArmRun {
            interrupted: true,
            check_passed: None,
            error: Some("interrupted".to_string()),
            ..result("a", 2, Arm::On, 0, false)
        },
    ];
    let report = summarize(&names(&["a"]), runs);
    let value = serde_json::to_value(&report).unwrap();
    let keys = |v: &Value| -> Vec<String> {
        let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    };
    assert_eq!(
        keys(&value),
        [
            "approve_for_me",
            "base",
            "compared",
            "errored",
            "geo_mean_ratio",
            "interrupted",
            "interval",
            "mean_change",
            "mean_change_billed",
            "median_change",
            "median_change_billed",
            "noisy",
            "not_proxied",
            "note",
            "per_arm",
            "resumed",
            "runs",
            "settings_restored",
            "success_off",
            "success_on",
            "target_change",
            "tasks",
            "unmeasured",
            "variant",
            "verdict",
            "verdict_billed",
            "weights",
            "work_dir"
        ]
    );
    let runs = value["runs"].as_array().unwrap();
    let first = keys(&runs[0]);
    for r in runs {
        assert_eq!(keys(r), first, "{r}");
    }
    for k in [
        "interrupted",
        "check_tail",
        "input_tokens",
        "cached_input_tokens",
        "reasoning_tokens",
        "thread",
        "trace",
        "started_at",
        "ended_at",
    ] {
        assert!(first.iter().any(|f| f == k), "{k} in {first:?}");
    }
    // Without a comparison, the numbers are null, not missing.
    assert!(
        value["median_change"].is_null() && value["verdict"]["met"].is_null(),
        "{value}"
    );
}

#[test]
fn a_change_just_short_of_the_target_is_never_printed_as_the_target() {
    let tally = Tally {
        runs: 1,
        successes: 1,
        rate: Some(1.0),
    };
    // 4,001 -> 3,001 is 24.99% lower: missed.
    let miss = verdict(Some((3_001.0 - 4_001.0) / 4_001.0), &tally, &tally);
    assert_eq!(miss.met, Some(false));
    assert!(miss.text.contains("24.99% lower"), "{}", miss.text);
    // Exactly a quarter: met, and printed with one decimal.
    let hit = verdict(Some((3_000.0 - 4_000.0) / 4_000.0), &tally, &tally);
    assert_eq!(hit.met, Some(true));
    assert!(hit.text.contains("25.0% lower"), "{}", hit.text);
    let flat = verdict(Some(0.0), &tally, &tally);
    assert!(flat.text.contains("unchanged"), "{}", flat.text);

    let runs = vec![
        result("a", 1, Arm::Off, 4_001, true),
        result("a", 1, Arm::On, 3_001, true),
    ];
    let table = render(&summarize(&names(&["a"]), runs));
    assert!(table.contains("median -24.99%"), "{table}");
}

#[test]
fn copies_inside_the_repository_are_refused_before_the_daemon_is_touched() {
    let (_dir, repo, _work) = repository();
    let daemon = FakeDaemon::new();
    let agent = FakeAgent::new(&daemon);
    let inside = repo.join("ab-copies");
    let err = run_ab(
        &repo,
        &[task("one", "true")],
        &options(&inside),
        &env(&daemon, &agent, &never),
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("inside the repository"), "{err:#}");
    assert!(daemon.state().sets.is_empty(), "the daemon was not touched");
    assert!(!inside.exists(), "the folder made for the copies was removed");
    assert_eq!(worktrees(&repo), 1);
}

#[test]
fn a_subfolder_of_the_repository_is_where_each_session_starts() {
    let (_dir, repo, work) = repository();
    std::fs::create_dir_all(repo.join("svc/api")).unwrap();
    std::fs::write(repo.join("svc/api/README.md"), "# api\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "svc"]);
    let daemon = FakeDaemon::new();
    let agent = FakeAgent::new(&daemon);
    let report = run_ab(
        &repo.join("svc/api"),
        &[task("one", "test -f done.txt && test -f ../../svc/api/done.txt")],
        &options(&work),
        &env(&daemon, &agent, &never),
    )
    .unwrap();
    assert!(report.runs.iter().all(|r| r.success), "{report:#?}");
    for s in agent.seen() {
        assert!(s.cwd.ends_with("svc/api"), "{:?}", s.cwd);
        assert!(s.cwd.starts_with(&work));
    }
    // The arm with lge on registered the folder the session runs in.
    assert!(daemon.state().added.iter().all(|a| a.ends_with("svc/api")));
}

#[cfg(unix)]
#[test]
fn a_copy_that_cannot_be_removed_is_named() {
    use std::os::unix::fs::PermissionsExt;
    let (_dir, repo, work) = repository();
    let daemon = FakeDaemon::new();
    let agent = FakeAgent::new(&daemon);
    let lines = RefCell::new(Vec::<String>::new());
    let log = |m: &str| lines.borrow_mut().push(m.to_string());
    let env = AbEnv {
        daemon: &daemon,
        runner: &agent,
        stop: &never,
        log: &log,
    };
    // The check leaves a folder whose files cannot be deleted.
    let report = run_ab(
        &repo,
        &[task("one", "mkdir -p locked && touch locked/f && chmod 555 locked")],
        &options(&work),
        &env,
    )
    .unwrap();
    assert!(report.settings_restored);
    let said = lines.borrow().join("\n");
    for s in agent.seen() {
        let _ = std::fs::set_permissions(s.cwd.join("locked"), std::fs::Permissions::from_mode(0o755));
    }
    assert!(said.contains("2 copy folder(s) could not be removed"), "{said}");
    assert!(said.contains("git worktree prune"), "{said}");
}

// ---- base commit, prompt note, approvals ----------------------------------------

/// The repository of [`repository`] with a second commit; returns the first commit.
fn second_commit(repo: &Path) -> String {
    let first = git(repo, &["rev-parse", "HEAD"]).trim().to_string();
    std::fs::create_dir_all(repo.join("sub")).unwrap();
    std::fs::write(repo.join("sub/new.txt"), "new\n").unwrap();
    git(repo, &["add", "."]);
    git(repo, &["commit", "-q", "-m", "second"]);
    first
}

#[test]
fn copies_are_made_from_the_base_commit_resolved_once() {
    let (_dir, repo, work) = repository();
    let first = second_commit(&repo);
    let head = git(&repo, &["rev-parse", "HEAD"]).trim().to_string();
    let daemon = FakeDaemon::new();
    let mut agent = FakeAgent::new(&daemon);
    // Every session moves the branch: later copies must not follow it.
    agent.move_branch = Some(repo.clone());
    let mut opts = options(&work);
    opts.base = "HEAD~1".to_string();
    let report = run_ab(
        &repo,
        &[task("one", "test -f done.txt && test ! -e sub/new.txt")],
        &opts,
        &env(&daemon, &agent, &never),
    )
    .unwrap();
    assert_eq!(report.base, first, "the full id of the base");
    assert!(report.runs.iter().all(|r| r.success), "{report:#?}");
    assert!(agent.seen().iter().all(|s| s.moved.is_none()), "{:?}", agent.seen());
    assert!(render(&report).contains(&format!("base commit: {first}\n")));

    // Default: HEAD when the run starts, kept while the branch moves.
    clear(&work);
    let agent = {
        let mut a = FakeAgent::new(&daemon);
        a.move_branch = Some(repo.clone());
        a
    };
    let start = git(&repo, &["rev-parse", "HEAD"]).trim().to_string();
    assert_ne!(start, head, "the first run moved the branch");
    let report = run_ab(
        &repo,
        &[task("one", "test -f sub/new.txt"), task("two", "true")],
        &options(&work),
        &env(&daemon, &agent, &never),
    )
    .unwrap();
    assert_eq!(report.base, start);
    let seen = agent.seen();
    assert_eq!(seen.len(), 4);
    // HEAD at the start holds what the first run's second session committed;
    // what this run's sessions commit never reaches a later copy.
    for s in &seen {
        assert_eq!(s.moved.as_deref(), Some("2\n"), "{s:?}");
    }
    assert!(report.runs.iter().all(|r| r.success), "{report:#?}");
    assert_eq!(worktrees(&repo), 1);
}

#[test]
fn a_base_that_is_not_a_commit_stops_before_the_daemon_is_touched() {
    let (_dir, repo, work) = repository();
    let daemon = FakeDaemon::new();
    let agent = FakeAgent::new(&daemon);
    let mut opts = options(&work);
    opts.base = "no-such-branch".to_string();
    let err = run_ab(&repo, &[task("one", "true")], &opts, &env(&daemon, &agent, &never)).unwrap_err();
    assert!(
        format!("{err:#}").contains("`no-such-branch` is not a commit"),
        "{err:#}"
    );
    assert!(daemon.state().sets.is_empty() && agent.seen().is_empty());
    for bad in ["", "--all"] {
        assert!(resolve_commit(&repo, bad).is_err(), "{bad:?}");
    }
    let head = git(&repo, &["rev-parse", "HEAD"]).trim().to_string();
    assert_eq!(resolve_commit(&repo, &head[..8]).unwrap(), head);

    // A folder of the repository that the base commit does not have.
    let first = second_commit(&repo);
    let mut opts = options(&work);
    opts.base = first;
    let err = run_ab(
        &repo.join("sub"),
        &[task("one", "true")],
        &opts,
        &env(&daemon, &agent, &never),
    )
    .unwrap_err();
    assert!(
        format!("{err:#}").contains("the folder sub is not in commit"),
        "{err:#}"
    );
    assert!(daemon.state().sets.is_empty());
}

#[test]
fn the_prompt_note_is_the_default_the_files_or_none() {
    let file = parse_task_file("[[task]]\nname = \"a\"\nprompt = \"p\"\ncheck = \"true\"\n").unwrap();
    assert_eq!(file.note, None);
    assert_eq!(resolve_note(file.note.as_deref(), false).as_deref(), Some(DEFAULT_NOTE));
    assert!(DEFAULT_NOTE.starts_with("Do not install, download or sync dependencies"));
    assert!(DEFAULT_NOTE.ends_with("the task's check runs after you finish."));
    let file = parse_task_file(
        "note = \"Use the vendored packages.\"\n[[task]]\nname = \"a\"\nprompt = \"p\"\ncheck = \"true\"\n",
    )
    .unwrap();
    assert_eq!(
        resolve_note(file.note.as_deref(), false).as_deref(),
        Some("Use the vendored packages.")
    );
    assert_eq!(resolve_note(file.note.as_deref(), true), None, "--no-note wins");
    assert_eq!(resolve_note(Some(""), false), None, "an empty note turns it off");
    assert_eq!(resolve_note(Some("  "), false), None);
    assert_eq!(prompt_with_note("do it\n", Some("note")), "do it\n\nnote");
    assert_eq!(prompt_with_note("do it", None), "do it");

    // The sessions get it, and the report says which.
    let (_dir, repo, work) = repository();
    let daemon = FakeDaemon::new();
    let agent = FakeAgent::new(&daemon);
    let mut opts = options(&work);
    opts.note = Some("Stay offline.".to_string());
    let report = run_ab(&repo, &[task("one", "true")], &opts, &env(&daemon, &agent, &never)).unwrap();
    assert!(agent.seen().iter().all(|s| s.prompt == "do one\n\nStay offline."));
    assert_eq!(report.note.as_deref(), Some("Stay offline."));
    assert!(render(&report).contains("note appended to every prompt: Stay offline.\n"));

    clear(&work);
    let agent = FakeAgent::new(&daemon);
    opts.note = None;
    let report = run_ab(&repo, &[task("one", "true")], &opts, &env(&daemon, &agent, &never)).unwrap();
    assert!(agent.seen().iter().all(|s| s.prompt == "do one"));
    assert_eq!(report.note, None);
    assert!(render(&report).contains("note appended to every prompt: none\n"));
    let value = serde_json::to_value(&report).unwrap();
    assert!(value["note"].is_null(), "{value}");
}

#[test]
fn sessions_use_approve_for_me_in_place_of_the_sandbox_when_the_agent_has_it() {
    let (_dir, repo, work) = repository();
    let daemon = FakeDaemon::new();
    let mut agent = FakeAgent::new(&daemon);
    agent.approve_for_me = true;
    let report = run_ab(
        &repo,
        &[task("one", "true"), task("two", "true")],
        &options(&work),
        &env(&daemon, &agent, &never),
    )
    .unwrap();
    assert_eq!(agent.approve_asked.get(), 1, "asked once per run, not per session");
    for s in agent.seen() {
        assert!(s.approve_for_me, "{s:?}");
        assert_eq!(s.sandbox, None, "-s is never passed with --approve-for-me");
    }
    assert!(report.approve_for_me);
    assert!(render(&report).contains("sessions ran with --approve-for-me"));

    // --no-approve-for-me: the sandbox as before, and the agent is not asked.
    clear(&work);
    let mut agent = FakeAgent::new(&daemon);
    agent.approve_for_me = true;
    let mut opts = options(&work);
    opts.approve_for_me = false;
    let report = run_ab(&repo, &[task("one", "true")], &opts, &env(&daemon, &agent, &never)).unwrap();
    assert_eq!(agent.approve_asked.get(), 0);
    for s in agent.seen() {
        assert!(!s.approve_for_me);
        assert_eq!(s.sandbox.as_deref(), Some("workspace-write"));
    }
    assert!(!report.approve_for_me);
    assert!(!render(&report).contains("--approve-for-me"));

    // Another sandbox is not the one the flag selects: kept as asked.
    clear(&work);
    let mut agent = FakeAgent::new(&daemon);
    agent.approve_for_me = true;
    let mut opts = options(&work);
    opts.sandbox = Some("danger-full-access".to_string());
    run_ab(&repo, &[task("one", "true")], &opts, &env(&daemon, &agent, &never)).unwrap();
    assert_eq!(agent.approve_asked.get(), 0);
    assert!(agent
        .seen()
        .iter()
        .all(|s| !s.approve_for_me && s.sandbox.as_deref() == Some("danger-full-access")));
}

// ---- adversarial: base, note, approvals -----------------------------------------

#[test]
fn an_annotated_tag_as_base_resolves_to_its_commit_and_the_copies_hold_it() {
    let (_dir, repo, work) = repository();
    let first = second_commit(&repo);
    git(&repo, &["tag", "-a", "v1", "-m", "release", &first]);
    let tag_object = git(&repo, &["rev-parse", "v1"]).trim().to_string();
    assert_ne!(tag_object, first, "an annotated tag is an object of its own");
    assert_eq!(resolve_commit(&repo, "v1").unwrap(), first);
    assert_eq!(resolve_commit(&repo, " v1 ").unwrap(), first);
    // A tree or a blob is not a commit.
    let err = resolve_commit(&repo, "HEAD:README.md").unwrap_err();
    assert!(format!("{err:#}").contains("is not a commit"), "{err:#}");

    let daemon = FakeDaemon::new();
    let agent = FakeAgent::new(&daemon);
    let mut opts = options(&work);
    opts.base = "v1".to_string();
    let report = run_ab(
        &repo,
        &[task("one", "test ! -e sub/new.txt && test -f README.md")],
        &opts,
        &env(&daemon, &agent, &never),
    )
    .unwrap();
    assert_eq!(report.base, first);
    assert!(report.runs.iter().all(|r| r.success), "{report:#?}");
    let value = serde_json::to_value(&report).unwrap();
    assert_eq!(value["base"], first.as_str());
    // The user's checkout is where it was.
    assert!(repo.join("sub/new.txt").exists());
    assert_eq!(worktrees(&repo), 1);
}

#[test]
fn the_note_is_appended_once_and_both_arms_get_the_same_prompt() {
    let (_dir, repo, work) = repository();
    let daemon = FakeDaemon::new();
    let agent = FakeAgent::new(&daemon);
    let mut opts = options(&work);
    opts.runs = 2;
    let mut t = task("one", "true");
    t.prompt = "do one\n\n\n".to_string();
    run_ab(&repo, &[t], &opts, &env(&daemon, &agent, &never)).unwrap();
    let seen = agent.seen();
    assert_eq!(seen.len(), 4);
    for s in &seen {
        assert_eq!(s.prompt.matches(DEFAULT_NOTE).count(), 1, "{:?}", s.prompt);
        assert_eq!(s.prompt, format!("do one\n\n{DEFAULT_NOTE}"));
    }
    assert!(seen.iter().any(|s| s.arm == Arm::Off) && seen.iter().any(|s| s.arm == Arm::On));
}

#[test]
fn the_log_says_when_the_agent_has_no_approve_for_me() {
    let (_dir, repo, work) = repository();
    let daemon = FakeDaemon::new();
    let agent = FakeAgent::new(&daemon);
    let lines = RefCell::new(Vec::<String>::new());
    let log = |m: &str| lines.borrow_mut().push(m.to_string());
    let env = AbEnv {
        daemon: &daemon,
        runner: &agent,
        stop: &never,
        log: &log,
    };
    let report = run_ab(&repo, &[task("one", "true")], &options(&work), &env).unwrap();
    assert!(!report.approve_for_me);
    for s in agent.seen() {
        assert!(!s.approve_for_me);
        assert_eq!(s.sandbox.as_deref(), Some("workspace-write"));
    }
    let said = lines.borrow().join("\n");
    assert!(said.contains("has no --approve-for-me"), "{said}");
    assert!(said.contains(&format!("copies of commit {}", report.base)), "{said}");
}

// ---- v0.0.2: variants, preflight, resume, traces, billed tokens, noise ---------

#[test]
fn a_variant_run_writes_the_variants_text_into_one_arm_only() {
    use lge_bench::agents::{AGENTS_BLOCK, AGENTS_BLOCK_BATCH, BATCH_LINE, TERSE_SECTION};
    let (_dir, repo, work) = repository();
    // The commit has its own AGENTS.md, with an lge block that has the batch line.
    std::fs::write(repo.join("AGENTS.md"), format!("# Rules\n\n{AGENTS_BLOCK_BATCH}")).unwrap();
    git(&repo, &["add", "AGENTS.md"]);
    git(&repo, &["commit", "-q", "-m", "agents"]);
    let daemon = FakeDaemon::new();
    for (variant, off_text, on_text) in [
        (
            Variant::Terse,
            format!("# Rules\n\n{AGENTS_BLOCK}"),
            format!("# Rules\n\n{}\n\n{TERSE_SECTION}", AGENTS_BLOCK.trim_end()),
        ),
        (
            Variant::Batch,
            format!("# Rules\n\n{AGENTS_BLOCK}"),
            format!("# Rules\n\n{AGENTS_BLOCK_BATCH}"),
        ),
    ] {
        clear(&work);
        let agent = FakeAgent::new(&daemon);
        let mut opts = options(&work);
        opts.variant = variant;
        let sets_before = daemon.state().sets.len();
        let report = run_ab(
            &repo,
            &[task("one", "test -f done.txt"), task("two", "test -f done.txt")],
            &opts,
            &env(&daemon, &agent, &never),
        )
        .unwrap();
        assert_eq!(report.variant, variant);
        let seen = agent.seen();
        assert_eq!(seen.len(), 4);
        for (run, s) in report.runs.iter().zip(&seen) {
            let want = if run.arm == Arm::On { &on_text } else { &off_text };
            assert_eq!(s.agents_md.as_ref(), Some(want), "{variant} {:?}", run.arm);
            // The user's settings in both arms (graph context on here), with
            // no holdout; the copy is indexed in both, since both use the graph.
            assert_eq!(s.settings["inject_context"], true);
            assert_eq!(s.settings["holdout_share"], 0.0);
            assert!(s.registered, "{s:?}");
        }
        assert!(!off_text.contains(BATCH_LINE.trim_end()));
        let sets = daemon.state().sets[sets_before..].to_vec();
        assert!(
            sets[..4].windows(2).all(|w| w[0] == w[1]),
            "both arms get the same settings"
        );
        assert!(render(&report).contains(&format!("variant: {variant}:")));
        assert_eq!(serde_json::to_value(&report).unwrap()["variant"], variant.as_str());
        // The user's checkout is untouched.
        assert_eq!(
            std::fs::read_to_string(repo.join("AGENTS.md")).unwrap(),
            format!("# Rules\n\n{AGENTS_BLOCK_BATCH}")
        );
    }
}

#[test]
fn the_report_has_requests_output_and_reasoning_per_arm() {
    let run = |task: &str, n: usize, arm: Arm, total: i64, requests: u64, output: i64, reasoning: i64| ArmRun {
        requests: Some(requests),
        usage: TokenUsage {
            input_tokens: total - output,
            cached_input_tokens: (total - output) / 2,
            reasoning_tokens: reasoning,
            output_tokens: output,
        },
        ..result(task, n, arm, total, true)
    };
    let runs = vec![
        run("a", 1, Arm::Off, 10_000, 20, 1_000, 400),
        run("a", 1, Arm::On, 8_000, 14, 800, 300),
        run("b", 1, Arm::On, 5_000, 9, 500, 200),
        run("b", 1, Arm::Off, 6_000, 12, 600, 250),
    ];
    let r = summarize(&names(&["a", "b"]), runs);
    assert_eq!(r.tasks[0].off.median_requests, Some(20.0));
    assert_eq!(r.tasks[0].on.median_output, Some(800.0));
    assert_eq!(r.tasks[1].off.median_reasoning, Some(250.0));
    assert_eq!(r.per_arm.off.sessions, 2);
    assert_eq!(r.per_arm.off.median_requests, Some(16.0));
    assert_eq!(r.per_arm.on.median_requests, Some(11.5));
    assert_eq!(r.per_arm.on.median_output, Some(650.0));
    assert_eq!(r.per_arm.on.median_reasoning, Some(250.0));
    // Output already holds reasoning: the total is input plus output, nothing more.
    assert_eq!(r.per_arm.off.median_total, Some(8_000.0));
    let table = render(&r);
    assert!(table.contains("per arm, medians"), "{table}");
    let line = table.lines().find(|l| l.starts_with("on ")).unwrap();
    assert!(line.contains("11.5") || line.contains(" 12 "), "{line}");
    assert!(line.contains("650") && line.contains("250"), "{line}");
}

#[test]
fn billed_equivalent_tokens_get_their_own_change_and_a_second_reading() {
    assert_eq!(Weights::parse("1,0.1,8").unwrap(), DEFAULT_WEIGHTS);
    assert_eq!(
        Weights::parse(" 2 , 0.5 , 10 ").unwrap(),
        Weights {
            input: 2.0,
            cached: 0.5,
            output: 10.0
        }
    );
    for bad in ["", "1,2", "1,2,3,4", "a,b,c", "-1,0,1", "0,0,0", "1,NaN,1"] {
        assert!(Weights::parse(bad).is_err(), "{bad}");
    }
    let u = TokenUsage {
        input_tokens: 10_000,
        cached_input_tokens: 9_000,
        reasoning_tokens: 300,
        output_tokens: 500,
    };
    // 1,000 uncached + 9,000 x 0.1 + 500 x 8 (reasoning is part of output).
    assert_eq!(DEFAULT_WEIGHTS.billed(&u), 1_000.0 + 900.0 + 4_000.0);

    // Fewer cached tokens but much more output: total tokens fall, billed rise.
    let with = |task: &str, arm: Arm, input: i64, cached: i64, output: i64| ArmRun {
        usage: TokenUsage {
            input_tokens: input,
            cached_input_tokens: cached,
            reasoning_tokens: 0,
            output_tokens: output,
        },
        ..result(task, 1, arm, input + output, true)
    };
    let runs = vec![
        with("a", Arm::Off, 100_000, 90_000, 1_000),
        with("a", Arm::On, 60_000, 54_000, 3_000),
    ];
    let r = summarize(&names(&["a"]), runs);
    assert!(r.median_change.unwrap() < -0.3, "{:?}", r.median_change);
    // off: 10,000 + 9,000 + 8,000 = 27,000; on: 6,000 + 5,400 + 24,000 = 35,400.
    let billed = r.median_change_billed.unwrap();
    assert!((billed - (35_400.0 - 27_000.0) / 27_000.0).abs() < 1e-9, "{billed}");
    assert_eq!(r.verdict.met, Some(true), "the verdict stays on total tokens");
    assert_eq!(r.verdict_billed.met, Some(false));
    assert!(r.verdict_billed.text.contains("billed-equivalent tokens"));
    let table = render(&r);
    assert!(
        table.contains("billed-equivalent tokens (uncached input x1, cached input x0.1, output x8): median +31.1%"),
        "{table}"
    );
    assert!(
        table.contains("second reading, on billed-equivalent tokens (not the verdict): target not met"),
        "{table}"
    );
    assert!(table.contains("\nverdict: target met"), "{table}");
    let custom = summarize_with(&names(&["a"]), r.runs.clone(), Weights::parse("1,1,1").unwrap());
    assert_eq!(custom.median_change_billed, custom.median_change);
}

#[test]
fn noisy_arms_are_marked_and_the_geometric_mean_is_reported() {
    let runs = vec![
        result("a", 1, Arm::Off, 1_000, true),
        result("a", 1, Arm::On, 1_000, true),
        result("a", 2, Arm::On, 2_000, true),
        result("a", 2, Arm::Off, 1_400, true),
        result("b", 1, Arm::Off, 4_000, true),
        result("b", 1, Arm::On, 2_000, true),
        result("b", 2, Arm::On, 2_000, true),
        result("b", 2, Arm::Off, 4_000, true),
    ];
    let r = summarize(&names(&["a", "b"]), runs);
    assert_eq!(r.tasks[0].on.spread, Some(2.0));
    assert!(r.tasks[0].on.noisy);
    assert!(!r.tasks[0].off.noisy, "1.4x is within {NOISE_SPREAD}x");
    assert_eq!(r.tasks[1].off.spread, Some(1.0));
    assert_eq!(r.noisy, 1);
    // a: 1,500 / 1,200 = 1.25; b: 2,000 / 4,000 = 0.5; geometric mean sqrt(0.625).
    let g = r.geo_mean_ratio.unwrap();
    assert!((g - 0.625f64.sqrt()).abs() < 1e-9, "{g}");
    let table = render(&r);
    assert!(
        table.contains("noisy: a with lge on: total tokens of its passing runs differ 2.0x"),
        "{table}"
    );
    assert!(
        table.contains("geometric mean of the tasks' on/off ratios: 0.791 (-20.9%)"),
        "{table}"
    );
    assert!(
        table.lines().any(|l| l.starts_with("a ") && l.ends_with("on 2.0x")),
        "{table}"
    );
    // One passing run: no spread, never noisy.
    let r = summarize(
        &names(&["a"]),
        vec![
            result("a", 1, Arm::Off, 1_000, true),
            result("a", 1, Arm::On, 9_000, true),
        ],
    );
    assert_eq!((r.tasks[0].on.spread, r.noisy), (None, 0));
}

/// Totals for a task: `runs` values around `base` (within 10%), times `factor`.
fn synthetic(base: f64, factor: f64, runs: usize, seed: u64) -> Vec<f64> {
    let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    (0..runs)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let noise = 0.9 + 0.2 * ((x >> 11) as f64 / (1u64 << 53) as f64);
            base * factor * noise
        })
        .collect()
}

#[test]
fn the_clustered_bootstrap_is_deterministic_and_brackets_the_true_difference() {
    // Six tasks of very different sizes, three runs per arm; lge on costs 20% less.
    let clusters: Vec<(Vec<f64>, Vec<f64>)> = [40_000.0, 150_000.0, 600_000.0, 900_000.0, 1_500_000.0, 3_000_000.0]
        .iter()
        .enumerate()
        .map(|(i, &base)| {
            (
                synthetic(base, 1.0, 3, 2 * i as u64 + 1),
                synthetic(base, 0.8, 3, 2 * i as u64 + 2),
            )
        })
        .collect();
    let first = clustered_interval(&clusters, BOOTSTRAP_RESAMPLES, BOOTSTRAP_SEED).unwrap();
    assert_eq!(
        Some(first),
        clustered_interval(&clusters, BOOTSTRAP_RESAMPLES, BOOTSTRAP_SEED),
        "same results, same interval"
    );
    let (low, high) = first;
    assert!(low < -0.2 && -0.2 < high, "{first:?}");
    assert!(high - low < 0.3, "{first:?}");
    assert!(clustered_interval(&[], 100, 1).is_none());

    // Through the report: only with two or more tasks over two or more runs.
    let mut runs = Vec::new();
    for (t, (off, on)) in clusters.iter().enumerate() {
        for k in 0..3 {
            let name = format!("t{t}");
            runs.push(result(&name, k + 1, Arm::Off, off[k] as i64, true));
            runs.push(result(&name, k + 1, Arm::On, on[k] as i64, true));
        }
    }
    let task_names: Vec<String> = (0..6).map(|t| format!("t{t}")).collect();
    let r = summarize(&task_names, runs.clone());
    let i = r.interval.unwrap();
    assert_eq!((i.resamples, i.seed), (BOOTSTRAP_RESAMPLES, BOOTSTRAP_SEED));
    assert!(
        i.low <= r.median_change.unwrap() && r.median_change.unwrap() <= i.high,
        "{r:#?}"
    );
    assert!(render(&r).contains("95% interval of the median change"));
    let one_run: Vec<ArmRun> = runs.iter().filter(|r| r.run == 1).cloned().collect();
    assert_eq!(summarize(&task_names, one_run).interval, None);
    let one_task: Vec<ArmRun> = runs.iter().filter(|r| r.task == "t0").cloned().collect();
    assert_eq!(summarize(&task_names, one_task).interval, None);
}

fn preflight_options(work: &Path) -> AbOptions {
    let mut o = options(work);
    o.preflight = true;
    o
}

#[test]
fn the_preflight_stops_the_run_before_any_session_and_names_each_bad_task() {
    let (_dir, repo, work) = repository();
    let daemon = FakeDaemon::new();
    let agent = FakeAgent::new(&daemon);
    let tasks = [
        task("good", "test -f done.txt"),
        task("done-already", "true"),
        task("missing-tool", "nosuch_tool_for_lge_ab --fast"),
        task("bad-quoting", "python -m pytest 'tests/order flow"),
        AbTask {
            setup: Some("echo cannot install >&2; exit 3".to_string()),
            ..task("broken-setup", "test -f done.txt")
        },
        task("not-executable", "./README.md"),
    ];
    let err = run_ab(&repo, &tasks, &preflight_options(&work), &env(&daemon, &agent, &never)).unwrap_err();
    let text = format!("{err:#}");
    assert!(
        text.contains("5 task(s) cannot be measured, so no session was started"),
        "{text}"
    );
    assert!(
        text.contains("- done-already: the check passes on the base commit"),
        "{text}"
    );
    assert!(
        text.contains("- missing-tool: the shell could not run the check (exit 127)"),
        "{text}"
    );
    assert!(text.contains("command not found"), "{text}");
    assert!(
        text.contains("- bad-quoting: the shell could not run the check (exit 2)"),
        "{text}"
    );
    assert!(
        text.contains("- broken-setup: the setup failed (exit 3): cannot install"),
        "{text}"
    );
    assert!(
        text.contains("- not-executable: the check could not run (exit 126"),
        "{text}"
    );
    assert!(!text.contains("- good:"), "{text}");
    assert!(text.contains("--no-preflight"), "{text}");
    assert!(agent.seen().is_empty(), "no session");
    assert!(daemon.state().sets.is_empty(), "the daemon was not touched");
    assert_eq!(worktrees(&repo), 1, "the preflight copies are gone");
    assert!(!work.exists(), "the folder the run made is removed");
}

#[test]
fn a_task_whose_check_fails_on_the_base_passes_the_preflight_and_runs() {
    let (_dir, repo, work) = repository();
    let daemon = FakeDaemon::new();
    let agent = FakeAgent::new(&daemon);
    let lines = RefCell::new(Vec::<String>::new());
    let log = |m: &str| lines.borrow_mut().push(m.to_string());
    let env = AbEnv {
        daemon: &daemon,
        runner: &agent,
        stop: &never,
        log: &log,
    };
    let report = run_ab(
        &repo,
        &[task("good", "test -f done.txt")],
        &preflight_options(&work),
        &env,
    )
    .unwrap();
    assert_eq!(report.runs.len(), 2);
    assert!(report.runs.iter().all(|r| r.success));
    let said = lines.borrow().join("\n");
    assert!(said.contains("preflight: good"), "{said}");
    assert!(said.contains("fails on the base commit, as it should"), "{said}");
    assert!(agent.seen().iter().all(|s| s.setup_ran));
    assert_eq!(worktrees(&repo), 1);
}

#[test]
fn preflight_only_needs_neither_daemon_nor_agent() {
    let (_dir, repo, work) = repository();
    let tasks = [task("good", "test -f done.txt"), task("done-already", "true")];
    let checked = run_preflight(&repo, &tasks, &preflight_options(&work), &never, &quiet).unwrap();
    assert!(!checked.ok());
    assert_eq!(checked.rows.len(), 2);
    assert_eq!(
        (checked.rows[0].setup.as_str(), checked.rows[0].check.as_str()),
        ("ok", "exit 1")
    );
    assert_eq!(checked.rows[0].problem, None);
    assert!(checked.rows[1].problem.is_some());
    let table = checked.render();
    assert!(table.contains("preflight on base commit"), "{table}");
    assert!(table.contains("fails as expected"), "{table}");
    assert!(table.contains("the check passes on the base commit"), "{table}");
    assert_eq!(worktrees(&repo), 1);
    assert!(!work.exists());
    // Ctrl-C during it: an error, and the copies are still removed.
    let stop = || true;
    assert!(run_preflight(&repo, &tasks, &preflight_options(&work), &stop, &quiet).is_err());
    assert_eq!(worktrees(&repo), 1);
}

fn result_lines(work: &Path) -> Vec<Value> {
    std::fs::read_to_string(work.join(RESULTS_FILE))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn every_session_is_appended_to_the_results_and_a_resume_runs_only_what_is_missing() {
    let (_dir, repo, work) = repository();
    let daemon = FakeDaemon::new();
    let tasks = [task("one", "test -f done.txt"), task("two", "test -f done.txt")];
    let mut opts = options(&work);
    opts.runs = 2;
    opts.tasks_hash = "hash-1".to_string();
    opts.model = Some("gpt-test".to_string());
    // Ctrl-C while the third session runs.
    let agent = FakeAgent::new(&daemon);
    let stop = || agent.seen.borrow().len() >= 3;
    let report = run_ab(&repo, &tasks, &opts, &env(&daemon, &agent, &stop)).unwrap();
    assert!(report.interrupted);
    assert_eq!(report.runs.len(), 3);
    // Ctrl-C left a valid file: one line per session, the cut one marked.
    let lines = result_lines(&work);
    assert_eq!(lines.len(), 3);
    for (line, run) in lines.iter().zip(&report.runs) {
        assert_eq!(line["task"], run.task.as_str());
        assert_eq!(line["arm"], run.arm.as_str());
        assert_eq!(line["base"], report.base.as_str());
        assert_eq!(line["tasks_hash"], "hash-1");
        assert_eq!(line["model"], "gpt-test");
        assert_eq!(line["sandbox"], "workspace-write");
        assert_eq!(line["approve_for_me"], false);
        assert_eq!(line["variant"], "features");
        assert_eq!(line["note"], DEFAULT_NOTE);
        assert!(line["started_at"].as_i64().unwrap() > 0);
        assert!(line["ended_at"].as_i64().unwrap() >= line["started_at"].as_i64().unwrap());
        assert!(line["total_tokens"].is_i64() && line["success"].is_boolean());
    }
    assert_eq!(lines[2]["interrupted"], true);

    // A fresh run in the same folder would mix two runs: refused.
    let agent = FakeAgent::new(&daemon);
    let err = run_ab(&repo, &tasks, &opts, &env(&daemon, &agent, &never)).unwrap_err();
    assert!(format!("{err:#}").contains("--resume"), "{err:#}");
    assert!(agent.seen().is_empty());

    // Resume: the two finished sessions are not run again; the cut one is.
    // A line cut by a second Ctrl-C is skipped.
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(work.join(RESULTS_FILE))
        .unwrap();
    std::io::Write::write_all(&mut file, b"{\"task\":\"one\",\"ru").unwrap();
    drop(file);
    opts.resume = true;
    let agent = FakeAgent::new(&daemon);
    let report = run_ab(&repo, &tasks, &opts, &env(&daemon, &agent, &never)).unwrap();
    assert_eq!(agent.seen().len(), 6, "8 sessions in all, 2 already finished");
    assert!(!report.interrupted);
    assert_eq!(report.resumed, 2);
    assert_eq!(report.runs.len(), 8, "the report covers every session once");
    let mut keys: Vec<(String, usize, Arm)> = report.runs.iter().map(|r| (r.task.clone(), r.run, r.arm)).collect();
    keys.sort_by(|a, b| (a.0.as_str(), a.1, a.2.as_str()).cmp(&(b.0.as_str(), b.1, b.2.as_str())));
    keys.dedup();
    assert_eq!(keys.len(), 8);
    assert!(report.runs.iter().all(|r| r.success && !r.interrupted), "{report:#?}");
    assert_eq!(report.compared, 2);
    assert!(render(&report).contains("resumed: 2 session(s)"));
    assert_eq!(worktrees(&repo), 1);

    // Everything finished: a resume runs nothing and reports the same.
    let agent = FakeAgent::new(&daemon);
    let again = run_ab(&repo, &tasks, &opts, &env(&daemon, &agent, &never)).unwrap();
    assert!(agent.seen().is_empty());
    assert_eq!(again.runs.len(), 8);
    assert_eq!(again.median_change, report.median_change);
}

#[test]
fn a_resume_refuses_results_of_another_base_task_file_or_model_unless_forced() {
    let (_dir, repo, work) = repository();
    let daemon = FakeDaemon::new();
    let tasks = [task("one", "test -f done.txt")];
    let mut opts = options(&work);
    opts.tasks_hash = "hash-1".to_string();
    let agent = FakeAgent::new(&daemon);
    run_ab(&repo, &tasks, &opts, &env(&daemon, &agent, &never)).unwrap();
    opts.resume = true;
    opts.runs = 2;

    let refused = |opts: &AbOptions, why: &str| {
        let agent = FakeAgent::new(&daemon);
        let err = run_ab(&repo, &tasks, opts, &env(&daemon, &agent, &never)).unwrap_err();
        assert!(format!("{err:#}").contains(why), "{err:#}");
        assert!(agent.seen().is_empty());
    };
    let mut other = opts.clone();
    other.tasks_hash = "hash-2".to_string();
    refused(&other, "the task file changed");
    let mut other = opts.clone();
    other.model = Some("another".to_string());
    refused(&other, "used model the agent's default, this run uses `another`");
    let mut other = opts.clone();
    other.variant = Variant::Terse;
    refused(&other, "variant `features`");
    let first = git(&repo, &["rev-parse", "HEAD"]).trim().to_string();
    second_commit(&repo);
    refused(&opts, &format!("pass --base {first}"));

    // --force mixes them; with the old base given, a resume adds the second run.
    let mut forced = opts.clone();
    forced.base = first.clone();
    let agent = FakeAgent::new(&daemon);
    let report = run_ab(&repo, &tasks, &forced, &env(&daemon, &agent, &never)).unwrap();
    assert_eq!(agent.seen().len(), 2, "run 2 only");
    assert_eq!(report.runs.len(), 4);
    forced.base = "HEAD".to_string();
    forced.force = true;
    forced.runs = 3;
    let agent = FakeAgent::new(&daemon);
    let report = run_ab(&repo, &tasks, &forced, &env(&daemon, &agent, &never)).unwrap();
    assert_eq!(agent.seen().len(), 2, "run 3 only");
    assert_eq!(report.runs.len(), 6);
    // A resume needs the folder.
    let mut missing = opts.clone();
    missing.work_dir = work.join("nope");
    let agent = FakeAgent::new(&daemon);
    let err = run_ab(&repo, &tasks, &missing, &env(&daemon, &agent, &never)).unwrap_err();
    assert!(format!("{err:#}").contains("nothing to resume"), "{err:#}");
}

#[test]
fn tasks_whose_names_give_the_same_slug_keep_their_own_traces() {
    let (_dir, repo, work) = repository();
    let daemon = FakeDaemon::new();
    daemon.state().traces = true;
    let agent = FakeAgent::new(&daemon);
    run_ab(
        &repo,
        &[task("a b", "test -f done.txt"), task("a-b", "test -f done.txt")],
        &options(&work),
        &env(&daemon, &agent, &never),
    )
    .unwrap();
    let mut files: Vec<String> = std::fs::read_dir(work.join(TRACES_DIR))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    files.sort();
    assert_eq!(
        files,
        [
            "01-a-b-1-off.json",
            "01-a-b-1-on.json",
            "02-a-b-1-off.json",
            "02-a-b-1-on.json"
        ]
    );
}

#[test]
fn each_sessions_trace_is_saved_and_reported() {
    let (_dir, repo, work) = repository();
    let daemon = FakeDaemon::new();
    daemon.state().traces = true;
    let agent = FakeAgent::new(&daemon);
    let report = run_ab(
        &repo,
        &[task("Fix the parser!", "test -f done.txt")],
        &options(&work),
        &env(&daemon, &agent, &never),
    )
    .unwrap();
    assert_eq!(daemon.state().traced, ["t-off", "t-on"]);
    for run in &report.runs {
        assert_eq!(run.thread, format!("t-{}", run.arm));
        let file = work
            .join(TRACES_DIR)
            .join(format!("01-fix-the-parser-1-{}.json", run.arm));
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(saved["thread"], run.thread.as_str());
        let t = run.trace.as_ref().unwrap();
        assert_eq!((t.requests, t.measured), (2, 2));
        assert_eq!((t.mean_context, t.max_context), (Some(6_000.0), Some(8_000)));
        assert_eq!(t.largest_outputs.len(), 2);
    }
    let on = report.runs.iter().find(|r| r.arm == Arm::On).unwrap();
    let top = &on.trace.as_ref().unwrap().largest_outputs[0];
    assert_eq!(
        (top.command.as_str(), top.tokens, top.shortened),
        ("cargo test", 2_000, true)
    );
    let table = render(&report);
    assert!(
        table.contains("context per request, from the daemon's traces"),
        "{table}"
    );
    assert!(
        table.contains("largest outputs: 9,000 tokens `cargo test`; 1,200 tokens `rg -n create_order`"),
        "{table}"
    );
    assert!(
        table.contains("2,000 tokens `cargo test` (shortened by lge)"),
        "{table}"
    );
    // Saved with the results, and read back on a resume.
    let lines = result_lines(&work);
    assert_eq!(lines[0]["trace"]["requests"], 2);

    // A daemon without traces: no trace, no file, and the run goes on.
    let (_dir, repo, work) = repository();
    let daemon = FakeDaemon::new();
    let agent = FakeAgent::new(&daemon);
    let report = run_ab(
        &repo,
        &[task("one", "true")],
        &options(&work),
        &env(&daemon, &agent, &never),
    )
    .unwrap();
    assert!(report.runs.iter().all(|r| r.trace.is_none() && r.success));
    assert!(!work.join(TRACES_DIR).exists());
    assert!(!render(&report).contains("context per request"));
}
