//! A/B runner: the same tasks with the proxy's features off and on.
//!
//! For each task, run and arm: a fresh copy of the repository at the base
//! commit (a detached git worktree; HEAD, resolved once when the run starts,
//! unless another revision is given), the daemon switched to the arm's
//! settings through its page API, one agent session in the copy through a
//! [`SessionRunner`] (Codex by default), the token usage read from its event
//! stream, and the task's check command run in the copy. The daemon's own
//! settings are put back at the end, also after an error, a panic or Ctrl-C.
//!
//! The two arms of a task alternate which goes first (off, on / on, off) so
//! that whatever the first session warms (the provider's prompt cache, the
//! disk cache, the daemon) favours each arm equally often.

mod daemon;
pub mod interrupt;
mod report;
mod worktree;

pub use daemon::{DaemonApi, HttpDaemon};
pub use report::{
    median, render, summarize, verdict, AbReport, ArmSummary, Tally, TaskSummary, Verdict, TARGET_CHANGE,
};
pub use worktree::{resolve_commit, Worktree};

use anyhow::{anyhow, bail, Context, Result};
use lge_session::{use_approve_for_me, SessionRequest, SessionRunner, WORKSPACE_WRITE};
use lge_usage::{parse_codex_jsonl, TokenUsage};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// One `[[task]]` of an A/B file.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AbTask {
    /// Unique within the file.
    pub name: String,
    /// What the agent is asked, as you would type it.
    pub prompt: String,
    /// A shell command run in the copy after the session; exit 0 = done right.
    pub check: String,
    /// A shell command run in the copy before the session (install, build).
    #[serde(default)]
    pub setup: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AbFile {
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    task: Vec<AbTask>,
}

/// An A/B file.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskFile {
    pub tasks: Vec<AbTask>,
    /// The top-level `note`: replaces [`DEFAULT_NOTE`]; `Some("")` turns it off.
    pub note: Option<String>,
}

/// Appended to every task's prompt unless the task file or `--no-note` says
/// otherwise: a session that installs dependencies or runs the whole test
/// suite in the sandbox stalls, and the check runs after it anyway.
pub const DEFAULT_NOTE: &str = "Do not install, download or sync dependencies and do not run long test suites: \
the task's check runs after you finish.";

/// Parse an A/B file. Every task needs a name, a prompt and a check, and the
/// names must be unique.
pub fn parse_task_file(text: &str) -> Result<TaskFile> {
    let file: AbFile = toml::from_str(text)?;
    if file.task.is_empty() {
        bail!("no [[task]] in the file");
    }
    let mut names = HashSet::new();
    for t in &file.task {
        for (field, value) in [("name", &t.name), ("prompt", &t.prompt), ("check", &t.check)] {
            if value.trim().is_empty() {
                bail!("a task has an empty `{field}` (task `{}`)", t.name);
            }
        }
        if !names.insert(t.name.trim()) {
            bail!("two tasks are named `{}`", t.name);
        }
    }
    Ok(TaskFile {
        tasks: file.task,
        note: file.note,
    })
}

/// The tasks of an A/B file (see [`parse_task_file`]).
pub fn parse_tasks(text: &str) -> Result<Vec<AbTask>> {
    Ok(parse_task_file(text)?.tasks)
}

pub fn load_task_file(path: &Path) -> Result<TaskFile> {
    let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    parse_task_file(&text).with_context(|| format!("{} is not a valid A/B task file", path.display()))
}

/// The note appended to every prompt: none with `--no-note` (`disabled`) or
/// an empty `note` in the task file, the file's note when it has one, else
/// [`DEFAULT_NOTE`].
pub fn resolve_note(file_note: Option<&str>, disabled: bool) -> Option<String> {
    if disabled {
        return None;
    }
    match file_note {
        Some(n) if n.trim().is_empty() => None,
        Some(n) => Some(n.trim().to_string()),
        None => Some(DEFAULT_NOTE.to_string()),
    }
}

/// The prompt a session gets: the task's, then the note in a paragraph of its own.
pub fn prompt_with_note(prompt: &str, note: Option<&str>) -> String {
    match note.map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) => format!("{}\n\n{n}", prompt.trim_end()),
        None => prompt.to_string(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Arm {
    /// The proxy forwards every request unchanged.
    Off,
    /// Graph context and output shortening on.
    On,
}

impl Arm {
    pub fn as_str(&self) -> &'static str {
        match self {
            Arm::Off => "off",
            Arm::On => "on",
        }
    }
}

impl std::fmt::Display for Arm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The arms of a (task, run) pair in the order they run: off first when
/// `pair` is even, on first when it is odd. The runner passes the task's index
/// plus the run's (both from 0), so neighbouring tasks start with different
/// arms and each task swaps its order from one run to the next, whatever the
/// number of tasks.
pub fn arm_order(pair: usize) -> [Arm; 2] {
    if pair.is_multiple_of(2) {
        [Arm::Off, Arm::On]
    } else {
        [Arm::On, Arm::Off]
    }
}

/// The daemon's settings for an arm: the original ones, with graph context
/// and output shortening switched on or off. Whole-file skeletons stay as the
/// user set them in the arm with features on (they are experimental) and are
/// off in the other. Indexing on first use is off in both arms: the runner
/// registers the copy itself for the arm that needs a graph, and waits for its
/// index before the session starts. Every other setting is kept.
pub fn arm_settings(original: &Value, arm: Arm) -> Value {
    let mut s = original.clone();
    if let Some(map) = s.as_object_mut() {
        let on = arm == Arm::On;
        map.insert("inject_context".to_string(), Value::Bool(on));
        map.insert("shorten_outputs".to_string(), Value::Bool(on));
        let reads = on && original["shorten_reads"].as_bool().unwrap_or(false);
        map.insert("shorten_reads".to_string(), Value::Bool(reads));
        map.insert("auto_index".to_string(), Value::Bool(false));
    }
    s
}

#[derive(Debug, Clone)]
pub struct AbOptions {
    /// Times each task runs in each arm.
    pub runs: usize,
    /// Where the copies are made. Created when missing, and removed at the end
    /// when it was created and is empty.
    pub work_dir: PathBuf,
    /// The revision the copies are made from, resolved to a commit once when
    /// the run starts.
    pub base: String,
    /// Model for the sessions; `None` keeps the agent's own.
    pub model: Option<String>,
    /// Sandbox for the sessions; `None` keeps the agent's default.
    pub sandbox: Option<String>,
    /// With the sandbox `workspace-write`, run the sessions with
    /// `--approve-for-me` in its place when the agent has it (asked once per
    /// run): approval requests are reviewed automatically instead of stalling.
    pub approve_for_me: bool,
    /// Appended to every prompt; see [`resolve_note`].
    pub note: Option<String>,
    /// Longest wait for the daemon to index a copy.
    pub index_timeout: Duration,
    /// Pause between two looks at the daemon's indexing state.
    pub poll: Duration,
}

impl AbOptions {
    pub fn new(work_dir: PathBuf) -> AbOptions {
        AbOptions {
            runs: 1,
            work_dir,
            base: "HEAD".to_string(),
            model: None,
            // `codex exec` is read-only by default; the tasks edit files.
            sandbox: Some(WORKSPACE_WRITE.to_string()),
            approve_for_me: true,
            note: Some(DEFAULT_NOTE.to_string()),
            index_timeout: Duration::from_secs(20 * 60),
            poll: Duration::from_millis(250),
        }
    }
}

/// A folder of its own under the system's temporary folder.
pub fn default_work_dir() -> PathBuf {
    std::env::temp_dir().join(format!("lge-ab-{}", std::process::id()))
}

/// What the runner works with.
pub struct AbEnv<'a> {
    pub daemon: &'a dyn DaemonApi,
    pub runner: &'a dyn SessionRunner,
    /// Asked between steps; true stops the run (Ctrl-C).
    pub stop: &'a dyn Fn() -> bool,
    /// Progress lines.
    pub log: &'a dyn Fn(&str),
}

/// One session: one task, one run, one arm.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ArmRun {
    pub task: String,
    /// 1-based.
    pub run: usize,
    pub arm: Arm,
    /// 1 when this arm went first in its pair, 2 when second.
    pub position: usize,
    /// Input (cached included) and output (reasoning included), from the
    /// session's event stream.
    #[serde(flatten)]
    pub usage: TokenUsage,
    /// Input plus output.
    pub total_tokens: i64,
    pub turns: u32,
    /// Model requests the daemon saw during the session; `None` when its
    /// status could not be read.
    pub requests: Option<u64>,
    /// Whether the session's requests reached the daemon.
    pub proxied: Option<bool>,
    /// The agent exited with success.
    pub session_ok: bool,
    /// `None` when the check did not run.
    pub check_passed: Option<bool>,
    /// The check passed.
    pub success: bool,
    pub seconds: f64,
    pub error: Option<String>,
    /// Stopped by Ctrl-C: listed, never compared.
    pub interrupted: bool,
    /// The end of the check's output when it failed; empty otherwise.
    pub check_tail: String,
}

impl ArmRun {
    fn new(task: &str, run: usize, arm: Arm, position: usize) -> ArmRun {
        ArmRun {
            task: task.to_string(),
            run,
            arm,
            position,
            usage: TokenUsage::default(),
            total_tokens: 0,
            turns: 0,
            requests: None,
            proxied: None,
            session_ok: false,
            check_passed: None,
            success: false,
            seconds: 0.0,
            error: None,
            interrupted: false,
            check_tail: String::new(),
        }
    }
}

/// The error a step returns when a stop was asked for.
#[derive(Debug)]
struct Interrupted;

impl std::fmt::Display for Interrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("interrupted")
    }
}

impl std::error::Error for Interrupted {}

/// Puts the daemon's original settings back when dropped, unless
/// [`Restore::finish`] already did.
struct Restore<'a> {
    daemon: &'a dyn DaemonApi,
    original: Value,
    armed: bool,
}

impl Restore<'_> {
    fn finish(mut self) -> Result<()> {
        self.armed = false;
        self.daemon.set_settings(&self.original)
    }
}

impl Drop for Restore<'_> {
    fn drop(&mut self) {
        if self.armed {
            if let Err(e) = self.daemon.set_settings(&self.original) {
                eprintln!(
                    "lge ab: the daemon's settings could not be restored ({e:#}); set them back on its page: {}",
                    self.original
                );
            }
        }
    }
}

/// Drops the copy from the daemon's list when dropped.
struct Registered<'a> {
    daemon: &'a dyn DaemonApi,
    root: String,
}

impl Drop for Registered<'_> {
    fn drop(&mut self) {
        let _ = self.daemon.remove_repo(&self.root);
    }
}

/// Run every task in both arms and report. `repo` is the repository (or a
/// folder inside one) whose commit `opts.base` is copied; the sessions start
/// in the same folder of each copy.
pub fn run_ab(repo: &Path, tasks: &[AbTask], opts: &AbOptions, env: &AbEnv) -> Result<AbReport> {
    if tasks.is_empty() {
        bail!("no task to run");
    }
    let repo = repo
        .canonicalize()
        .with_context(|| format!("cannot open {}", repo.display()))?;
    // Once: a branch that moves during the run does not change the copies.
    let base = resolve_commit(&repo, &opts.base)?;
    let prefix = worktree::prefix(&repo)?;
    worktree::has_folder(&repo, &base, &prefix)?;
    let status = env.daemon.status().context("cannot read the daemon's status")?;
    let original = status
        .get("settings")
        .filter(|s| s.is_object())
        .cloned()
        .ok_or_else(|| anyhow!("the daemon's status has no settings; is it an lge daemon?"))?;

    let top = worktree::toplevel(&repo)?;

    let created = !opts.work_dir.exists();
    std::fs::create_dir_all(&opts.work_dir).with_context(|| format!("cannot create {}", opts.work_dir.display()))?;
    let work_dir = opts.work_dir.canonicalize()?;
    // Stop before the daemon is touched.
    if let Err(e) = allowed_folder(&status, &work_dir).and_then(|()| outside_repository(&top, &work_dir)) {
        if created {
            let _ = std::fs::remove_dir(&work_dir);
        }
        return Err(e);
    }

    let approve_for_me = use_approve_for_me(opts.approve_for_me, opts.sandbox.as_deref(), env.runner);
    (env.log)(&format!("copies of commit {base}"));
    if approve_for_me {
        (env.log)(&format!(
            "sessions run with --approve-for-me in place of -s {WORKSPACE_WRITE}: approval requests are reviewed automatically"
        ));
    } else if opts.approve_for_me && opts.sandbox.as_deref() == Some(WORKSPACE_WRITE) {
        (env.log)(&format!(
            "{} has no --approve-for-me: sessions run with -s {WORKSPACE_WRITE}",
            env.runner.name()
        ));
    }
    let restore = Restore {
        daemon: env.daemon,
        original: original.clone(),
        armed: true,
    };
    let plan = Plan {
        repo: &repo,
        base: &base,
        prefix: &prefix,
        work_dir: &work_dir,
        original: &original,
        approve_for_me,
    };
    let outcome = run_pairs(&plan, tasks, opts, env);
    let restored = restore.finish();
    if created {
        // Only succeeds when every copy is gone.
        let _ = std::fs::remove_dir(&work_dir);
    }
    // A copy the session left undeletable (a folder without write permission)
    // stays: say where, rather than leave it unnoticed.
    let left: Vec<String> = std::fs::read_dir(&work_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("lge-ab-"))
        .map(|e| e.path().display().to_string())
        .collect();
    if !left.is_empty() {
        (env.log)(&format!(
            "{} copy folder(s) could not be removed; delete them and run `git worktree prune` in {}: {}",
            left.len(),
            repo.display(),
            left.join(", ")
        ));
    }
    let (runs, interrupted) = match (outcome, &restored) {
        (Ok(done), _) => done,
        (Err(e), Ok(())) => return Err(e),
        (Err(e), Err(r)) => {
            return Err(e.context(format!(
                "the daemon's settings could not be restored either ({r:#}); set them back on its page: {original}"
            )))
        }
    };
    if let Err(e) = &restored {
        (env.log)(&format!(
            "the daemon's settings could not be restored ({e:#}); set them back on its page: {original}"
        ));
    }
    let names: Vec<String> = tasks.iter().map(|t| t.name.clone()).collect();
    let mut report = summarize(&names, runs);
    report.interrupted = interrupted;
    report.settings_restored = restored.is_ok();
    report.base = base;
    report.approve_for_me = approve_for_me;
    report.note = opts.note.clone().filter(|n| !n.trim().is_empty());
    Ok(report)
}

/// A daemon started with `--workspace` indexes nothing outside those folders:
/// the arm with features on would get no graph, so stop before any session.
fn allowed_folder(status: &Value, work_dir: &Path) -> Result<()> {
    let workspaces: Vec<&str> = status["workspaces"]
        .as_array()
        .map(|w| w.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if workspaces.is_empty() || workspaces.iter().any(|w| work_dir.starts_with(w)) {
        return Ok(());
    }
    bail!(
        "the daemon only indexes folders under {}; pass --work-dir with a folder inside one of them, \
         or the arm with features on gets no graph",
        workspaces.join(", ")
    )
}

/// Copies inside the repository would show up in its `git status`, and the
/// daemon would read them into the graph of the repository itself when it is
/// registered there.
fn outside_repository(top: &Path, work_dir: &Path) -> Result<()> {
    if work_dir.starts_with(top) {
        bail!(
            "the copies would be made inside the repository ({}); pass --work-dir with a folder outside it",
            top.display()
        );
    }
    Ok(())
}

/// What every arm of the run shares.
struct Plan<'a> {
    repo: &'a Path,
    /// The commit every copy is made from.
    base: &'a str,
    /// Where `repo` sits in its git repository, to find it in a copy.
    prefix: &'a str,
    work_dir: &'a Path,
    original: &'a Value,
    /// Sessions run with `--approve-for-me` in place of the sandbox.
    approve_for_me: bool,
}

/// Every pair, in order; stops early when a stop is asked for. An error that
/// makes the comparison meaningless (the daemon refusing a setting) ends the
/// run; an error in one session is recorded on it and the run goes on.
fn run_pairs(plan: &Plan, tasks: &[AbTask], opts: &AbOptions, env: &AbEnv) -> Result<(Vec<ArmRun>, bool)> {
    let mut runs = Vec::new();
    for run in 1..=opts.runs.max(1) {
        for (i, task) in tasks.iter().enumerate() {
            for (position, arm) in arm_order(i + run - 1).into_iter().enumerate() {
                if (env.stop)() {
                    return Ok((runs, true));
                }
                (env.log)(&format!("{} (run {run}): lge {arm}", task.name));
                env.daemon
                    .set_settings(&arm_settings(plan.original, arm))
                    .with_context(|| format!("cannot switch the daemon to the arm with features {arm}"))?;
                // The folder name becomes the name of the copy's graph in the
                // daemon's data folder (`graphs/lge-ab-...`).
                let dir = plan
                    .work_dir
                    .join(format!("lge-ab-{:02}-{}-r{run}-{arm}", i + 1, slug(&task.name)));
                let mut out = ArmRun::new(&task.name, run, arm, position + 1);
                let step = run_arm(plan, &dir, task, arm, opts, env, &mut out);
                let stopped = match step {
                    Ok(()) => false,
                    Err(e) if e.downcast_ref::<Interrupted>().is_some() => {
                        out.error = Some(format!("{e:#}"));
                        true
                    }
                    // Ctrl-C reaches the setup, the session, git and the check
                    // too: what fails while a stop is asked for failed because of it.
                    Err(e) if (env.stop)() => {
                        out.error = Some(format!("interrupted: {e:#}"));
                        true
                    }
                    Err(e) => {
                        out.error = Some(format!("{e:#}"));
                        false
                    }
                };
                if stopped {
                    // What a session or check cut short left is not a result.
                    out.interrupted = true;
                    out.check_passed = None;
                    out.success = false;
                    out.check_tail.clear();
                }
                (env.log)(&format!(
                    "{} (run {run}): lge {arm}: {} tokens, {}",
                    task.name,
                    out.total_tokens,
                    match (&out.error, out.success) {
                        (Some(e), _) => format!("error: {e}"),
                        (None, true) => "check passed".to_string(),
                        (None, false) => "check failed".to_string(),
                    }
                ));
                runs.push(out);
                if stopped {
                    return Ok((runs, true));
                }
            }
        }
    }
    Ok((runs, false))
}

/// One session in a fresh copy, with the arm's settings already applied.
fn run_arm(
    plan: &Plan,
    dir: &Path,
    task: &AbTask,
    arm: Arm,
    opts: &AbOptions,
    env: &AbEnv,
    out: &mut ArmRun,
) -> Result<()> {
    let copy = Worktree::add(plan.repo, dir, plan.base)?;
    let cwd = match plan.prefix.trim_end_matches('/') {
        "" => copy.path().to_path_buf(),
        inner => copy.path().join(inner),
    };
    if let Some(setup) = task.setup.as_deref().filter(|s| !s.trim().is_empty()) {
        let done = shell(setup, &cwd)?;
        if !done.ok {
            bail!("setup failed ({}): {}", done.code, done.tail.trim());
        }
    }
    // Declared after the copy, so it is dropped first: the daemon lets go of
    // the folder before it is removed.
    let _registered = match arm {
        Arm::On => Some(index_copy(&cwd, opts, env)?),
        Arm::Off => None,
    };
    if (env.stop)() {
        return Err(Interrupted.into());
    }
    let before = env.daemon.status().ok();
    let started = Instant::now();
    let session = env.runner.run(&SessionRequest {
        prompt: prompt_with_note(&task.prompt, opts.note.as_deref()),
        cwd: cwd.clone(),
        model: opts.model.clone(),
        effort: None,
        sandbox: if plan.approve_for_me {
            None
        } else {
            opts.sandbox.clone()
        },
        approve_for_me: plan.approve_for_me,
    });
    out.seconds = started.elapsed().as_secs_f64();
    let session = session.with_context(|| format!("the {} session could not run", env.runner.name()))?;
    let parsed = parse_codex_jsonl(&session.events);
    out.usage = parsed.usage;
    out.total_tokens = parsed.usage.total();
    out.turns = parsed.turns;
    out.session_ok = session.exit_ok;
    if let (Some(before), Ok(after)) = (before, env.daemon.status()) {
        let n = requests_between(&before, &after);
        out.requests = Some(n);
        out.proxied = Some(n > 0);
    }
    // The terminal sent Ctrl-C to the session as well: what it left is not a result.
    if (env.stop)() {
        return Err(Interrupted.into());
    }
    let check = shell(&task.check, &cwd)?;
    if (env.stop)() {
        return Err(Interrupted.into());
    }
    out.check_passed = Some(check.ok);
    out.success = check.ok;
    if !check.ok {
        out.check_tail = check.tail;
    }
    Ok(())
}

/// Register the copy with the daemon and wait until its graph is ready, so
/// the first message of the session already gets its context.
fn index_copy<'a>(cwd: &Path, opts: &AbOptions, env: &AbEnv<'a>) -> Result<Registered<'a>> {
    let root = env
        .daemon
        .add_repo(cwd)
        .context("the daemon refused to index the copy")?;
    let registered = Registered {
        daemon: env.daemon,
        root: root.clone(),
    };
    let deadline = Instant::now() + opts.index_timeout;
    loop {
        let status = env.daemon.status()?;
        let entry = status["repos"]
            .as_array()
            .and_then(|repos| repos.iter().find(|r| r["root"].as_str() == Some(root.as_str())));
        match entry.map(|r| (r["state"].as_str().unwrap_or(""), r)) {
            Some(("ready", _)) => return Ok(registered),
            Some(("failed", r)) => bail!(
                "the daemon could not index the copy: {}",
                r["detail"].as_str().unwrap_or("")
            ),
            None => bail!("the daemon does not list the copy {root}"),
            Some(_) => {}
        }
        if (env.stop)() {
            return Err(Interrupted.into());
        }
        if Instant::now() >= deadline {
            bail!(
                "the daemon did not finish indexing the copy within {} s",
                opts.index_timeout.as_secs()
            );
        }
        std::thread::sleep(opts.poll);
    }
}

/// Model requests per session id in a daemon status.
fn session_requests(status: &Value) -> HashMap<String, u64> {
    status["sessions"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|s| Some((s["id"].as_str()?.to_string(), s["requests"].as_u64().unwrap_or(0))))
                .collect()
        })
        .unwrap_or_default()
}

/// Requests the daemon saw between two statuses, over the sessions listed in
/// the second.
fn requests_between(before: &Value, after: &Value) -> u64 {
    let before = session_requests(before);
    session_requests(after)
        .into_iter()
        .map(|(id, n)| n.saturating_sub(before.get(&id).copied().unwrap_or(0)))
        .sum()
}

/// A task name as part of a folder name.
fn slug(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .take(40)
        .collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        "task".to_string()
    } else {
        s
    }
}

/// What a shell command did.
struct Shell {
    ok: bool,
    /// `exit 3`, or the signal that ended it.
    code: String,
    /// The last lines of its output.
    tail: String,
}

/// Lines of output kept from a failed command.
const TAIL_LINES: usize = 20;

fn shell(command: &str, dir: &Path) -> Result<Shell> {
    let out = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("cannot run `{command}`"))?;
    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    let lines: Vec<&str> = text.lines().collect();
    let tail = lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n");
    Ok(Shell {
        ok: out.status.success(),
        code: match out.status.code() {
            Some(c) => format!("exit {c}"),
            None => "ended by a signal".to_string(),
        },
        tail,
    })
}

/// True when the Codex configuration `config` (the text of
/// `~/.codex/config.toml`) sends model requests to the daemon at `daemon`:
/// a top-level `openai_base_url` of `<daemon>/v1`. `localhost` and
/// `127.0.0.1` are the same host.
pub fn codex_points_at(config: &str, daemon: &str) -> bool {
    let Ok(value) = toml::from_str::<toml::Table>(config) else {
        return false;
    };
    let norm = |url: &str| {
        url.trim()
            .trim_end_matches('/')
            .replace("://localhost", "://127.0.0.1")
            .to_ascii_lowercase()
    };
    value
        .get("openai_base_url")
        .and_then(|v| v.as_str())
        .is_some_and(|url| norm(url) == norm(&format!("{}/v1", daemon.trim().trim_end_matches('/'))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn arms_alternate_per_pair() {
        let orders: Vec<[Arm; 2]> = (0..4).map(arm_order).collect();
        assert_eq!(
            orders,
            [
                [Arm::Off, Arm::On],
                [Arm::On, Arm::Off],
                [Arm::Off, Arm::On],
                [Arm::On, Arm::Off]
            ]
        );
    }

    #[test]
    fn arm_settings_switch_only_the_features() {
        let original = json!({ "inject_context": false, "shorten_outputs": true, "shorten_reads": true, "context_budget": 900, "auto_index": true, "future": "kept" });
        let on = arm_settings(&original, Arm::On);
        assert_eq!(
            on,
            json!({ "inject_context": true, "shorten_outputs": true, "shorten_reads": true, "context_budget": 900, "auto_index": false, "future": "kept" })
        );
        let off = arm_settings(&original, Arm::Off);
        assert_eq!(
            off,
            json!({ "inject_context": false, "shorten_outputs": false, "shorten_reads": false, "context_budget": 900, "auto_index": false, "future": "kept" })
        );
        // Skeletons stay off in the arm with features on when the user has them off.
        let plain = json!({ "shorten_reads": false });
        assert_eq!(arm_settings(&plain, Arm::On)["shorten_reads"], false);
    }

    #[test]
    fn requests_are_counted_between_two_statuses() {
        let before = json!({ "sessions": [{ "id": "a", "requests": 3 }, { "id": "old", "requests": 9 }] });
        let after = json!({ "sessions": [{ "id": "b", "requests": 4 }, { "id": "a", "requests": 5 }] });
        assert_eq!(requests_between(&before, &after), 6);
        assert_eq!(requests_between(&after, &after), 0);
        assert_eq!(requests_between(&json!({}), &json!({})), 0);
    }

    #[test]
    fn slugs_are_safe_folder_names() {
        assert_eq!(slug("Fix the parser!"), "fix-the-parser");
        assert_eq!(slug("../.."), "task");
        assert_eq!(slug(&"x".repeat(80)).len(), 40);
    }

    #[test]
    fn the_codex_configuration_must_point_at_the_daemon() {
        let daemon = "http://127.0.0.1:8787";
        assert!(codex_points_at(
            "openai_base_url = \"http://127.0.0.1:8787/v1\"\nmodel = \"gpt\"\n",
            daemon
        ));
        assert!(codex_points_at(
            "openai_base_url = \"http://localhost:8787/v1/\"\n",
            "http://127.0.0.1:8787/"
        ));
        // Under a table it is not the top-level key Codex reads.
        assert!(!codex_points_at(
            "[profiles.x]\nopenai_base_url = \"http://127.0.0.1:8787/v1\"\n",
            daemon
        ));
        assert!(!codex_points_at(
            "openai_base_url = \"https://api.openai.com/v1\"\n",
            daemon
        ));
        assert!(!codex_points_at("", daemon));
        assert!(!codex_points_at("not toml [", daemon));
    }

    #[test]
    fn task_files_are_checked() {
        let ok = "[[task]]\nname = \"a\"\nprompt = \"do a\"\ncheck = \"true\"\n\n[[task]]\nname = \"b\"\nprompt = \"do b\"\ncheck = \"true\"\nsetup = \"make\"\n";
        let tasks = parse_tasks(ok).unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[1].setup.as_deref(), Some("make"));
        for (bad, why) in [
            ("", "no [[task]]"),
            ("[[task]]\nname = \"a\"\nprompt = \"p\"\n", "missing field `check`"),
            ("[[task]]\nname = \"a\"\nprompt = \"p\"\ncheck = \" \"\n", "empty `check`"),
            (
                "[[task]]\nname = \"a\"\nprompt = \"p\"\ncheck = \"c\"\n[[task]]\nname = \"a\"\nprompt = \"q\"\ncheck = \"c\"\n",
                "two tasks are named `a`",
            ),
            ("[[task]]\nname = \"a\"\nprompt = \"p\"\ncheck = \"c\"\nfiles = []\n", "unknown field"),
        ] {
            let err = format!("{:#}", parse_tasks(bad).unwrap_err());
            assert!(err.contains(why), "{bad:?}: {err}");
        }
    }
}
