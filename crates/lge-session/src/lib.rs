//! One short session per task.
//!
//! A session starts from a prompt built in a fixed order: a prefix that never
//! changes (so it can be served from the prompt cache), the evidence pack for
//! the task, what is remembered from earlier sessions, and the task last.
//! After the session the local checks run; their filtered output is all that
//! goes back to the model on a retry. Every attempt is written to the usage log.
//!
//! By default the session keeps the user's own model and reasoning level
//! (`level_mode = "fixed"`): the router's choice is logged, not applied.

use anyhow::{bail, Context, Result};
use lge_core::{outcome, stable_id, HandoffNote, Level, UsageRecord};
use lge_memory::{recall, refresh_pages, retain, RecallOptions, RetainInput};
use lge_retrieve::budget::{count, truncate_lines};
use lge_retrieve::{evidence_pack, index_repo, unix_now, EvidencePack, IndexOptions, PackOptions};
use lge_router::{route, Route, RouterConfig};
use lge_store::GraphStore;
use lge_usage::{apply_session, find_rollout, new_record, parse_codex_jsonl, parse_rollout_file, task_id};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Bump when [`STABLE_PREFIX`] changes: a new prefix starts a new cache entry.
pub const PREFIX_VERSION: u32 = 1;

/// The start of every session prompt. It must stay byte-identical between
/// sessions: nothing task-specific, no dates, no paths.
pub const STABLE_PREFIX: &str = "\
You are working in a repository that has a local code graph and project memory (lge).

The Context section below was built from the graph for this task: the starting symbols with their source, what they use, what uses them, the tests that reach them, and the APIs, tables and queues involved. Treat it as the map of the change.

Rules:
1. Start from the Context. Do not list directories or search the repository to rediscover what it already shows.
2. Read only the line ranges you need; locations are given as path:start-end. Do not read whole files.
3. When you need more, ask the local_graph tools (search_code_context, analyze_change_impact, trace_execution_flow, find_dependencies, get_relevant_source, get_architecture_context, recall_memory) before reading files.
4. Edges marked inferred or unresolved are not verified. Check them in the source before relying on them.
5. Run the listed tests after your change.
6. Finish with a short handoff: what changed, in which files, and what is left. Store a decision or convention worth keeping with retain_memory.
";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LevelMode {
    /// Keep the configured model and reasoning level; log the router's choice.
    Fixed,
    /// Apply the router's level and retry one level higher when checks fail.
    Routed,
}

/// The `[session]` table of `lge.toml`. Every key is optional.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionConfig {
    pub level_mode: LevelMode,
    /// Model passed to the runner; "" keeps the runner's own default.
    pub model: String,
    /// Reasoning effort per level in routed mode, as the runner names it.
    /// "" passes no override, which keeps the user's configured effort.
    pub effort_low: String,
    pub effort_medium: String,
    pub effort_top: String,
    pub pack_budget: usize,
    pub memory_budget: usize,
    /// Run the local checks after the session and record success or failure.
    pub run_checks: bool,
    /// Attempts per task, counting the first.
    pub max_attempts: u8,
    pub codex_bin: String,
    /// Sandbox mode passed to Codex; "" keeps its default.
    pub sandbox: String,
    /// Label written to the usage log, to compare runs with different levers on.
    pub mode: String,
}

impl Default for SessionConfig {
    fn default() -> Self {
        SessionConfig {
            level_mode: LevelMode::Fixed,
            model: String::new(),
            effort_low: "low".to_string(),
            effort_medium: "medium".to_string(),
            effort_top: String::new(),
            pack_budget: lge_retrieve::budget::DEFAULT_PACK_BUDGET,
            memory_budget: 600,
            run_checks: true,
            max_attempts: 2,
            codex_bin: "codex".to_string(),
            sandbox: String::new(),
            mode: "pack+mcp".to_string(),
        }
    }
}

#[derive(Deserialize)]
struct ConfigFile {
    session: Option<SessionConfig>,
}

/// Read `[session]` from `lge.toml` in `root`; defaults when the file or the
/// table is missing. A file that does not parse is an error, not a default.
pub fn load_config(root: &Path) -> Result<SessionConfig> {
    let path = root.join("lge.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(SessionConfig::default()),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    // Other tables of the file belong to other crates and are ignored here.
    let file: ConfigFile = toml::from_str(&text).with_context(|| format!("{} is not valid", path.display()))?;
    Ok(file.session.unwrap_or_default())
}

impl SessionConfig {
    /// The reasoning-effort override for a level; `None` = pass nothing.
    pub fn effort_for(&self, level: Level) -> Option<String> {
        if self.level_mode == LevelMode::Fixed {
            return None;
        }
        let e = match level {
            Level::Low => &self.effort_low,
            Level::Medium => &self.effort_medium,
            Level::Top => &self.effort_top,
        };
        (!e.is_empty()).then(|| e.clone())
    }
}

/// The prompt, in cache order: stable prefix, context, memory, then the task.
pub fn build_prompt(pack_text: &str, memory_text: &str, task: &str, retry_note: &str) -> String {
    let mut p = String::from(STABLE_PREFIX);
    p.push_str("\n# Context\n");
    p.push_str(pack_text.trim_end());
    p.push('\n');
    if !memory_text.trim().is_empty() {
        p.push_str("\n# From earlier sessions\n");
        p.push_str(memory_text.trim_end());
        p.push('\n');
    }
    if !retry_note.trim().is_empty() {
        p.push_str("\n# The previous attempt failed these checks\n");
        p.push_str(retry_note.trim_end());
        p.push('\n');
    }
    p.push_str("\n# Task\n");
    p.push_str(task.trim());
    p.push('\n');
    p
}

#[derive(Debug, Clone)]
pub struct SessionRequest {
    pub prompt: String,
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub sandbox: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SessionResult {
    /// The runner's event stream (`codex exec --json` lines).
    pub events: String,
    pub exit_ok: bool,
}

/// Starts one session and waits for it. Codex is the default; anything that
/// takes a prompt and reports token usage can stand in.
pub trait SessionRunner {
    fn name(&self) -> &str;
    fn run(&self, request: &SessionRequest) -> Result<SessionResult>;
}

/// `codex exec --json`, with the prompt on stdin.
pub struct CodexRunner {
    pub bin: String,
}

/// The command line a request becomes. The prompt is not an argument: it goes
/// to stdin (`-`), so its size and content never hit argument limits.
pub fn codex_args(request: &SessionRequest) -> Vec<String> {
    let mut args = vec![
        "exec".to_string(),
        "--json".to_string(),
        "-C".to_string(),
        request.cwd.to_string_lossy().to_string(),
    ];
    if let Some(m) = &request.model {
        args.extend(["-m".to_string(), m.clone()]);
    }
    if let Some(e) = &request.effort {
        args.extend(["-c".to_string(), format!("model_reasoning_effort=\"{e}\"")]);
    }
    if let Some(s) = &request.sandbox {
        args.extend(["-s".to_string(), s.clone()]);
    }
    args.push("-".to_string());
    args
}

impl SessionRunner for CodexRunner {
    fn name(&self) -> &str {
        "codex"
    }

    fn run(&self, request: &SessionRequest) -> Result<SessionResult> {
        let mut child = Command::new(&self.bin)
            .args(codex_args(request))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("cannot start `{}`; is the Codex CLI installed and on PATH?", self.bin))?;
        // Write the prompt from another thread: a child that fills its stdout
        // pipe before reading all of stdin would otherwise block both sides.
        let mut stdin = child.stdin.take().context("no stdin for the session")?;
        let prompt = request.prompt.clone();
        let writer = std::thread::spawn(move || stdin.write_all(prompt.as_bytes()));
        let output = child.wait_with_output()?;
        let _ = writer.join();
        Ok(SessionResult {
            events: String::from_utf8_lossy(&output.stdout).to_string(),
            exit_ok: output.status.success(),
        })
    }
}

/// Everything decided before a session starts.
#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub task: String,
    pub route: Route,
    /// The level the session will run at: the router's in routed mode, top
    /// (the user's own level) in fixed mode.
    pub level: Level,
    pub pack: EvidencePack,
    pub memory_text: String,
    pub prompt: String,
    pub prompt_tokens: usize,
    /// Tokens of the part of the prompt that is identical in every session.
    pub prefix_tokens: usize,
}

/// Index what changed, route, build the pack, recall memory, build the prompt.
pub fn plan_task(
    store: &dyn GraphStore,
    root: &Path,
    task: &str,
    cfg: &SessionConfig,
    index: &IndexOptions,
) -> Result<Plan> {
    if task.trim().is_empty() {
        bail!("the task is empty");
    }
    index_repo(store, root, index)?;
    let route = route(store, task, &RouterConfig::default())?;
    let level = if cfg.level_mode == LevelMode::Routed {
        route.level
    } else {
        Level::Top
    };
    let pack = evidence_pack(
        store,
        task,
        &PackOptions {
            budget: cfg.pack_budget,
            ..PackOptions::default()
        },
    )?;

    let mut memory_text = String::new();
    if cfg.memory_budget > 0 {
        let found = recall(
            store,
            task,
            &RecallOptions {
                budget: cfg.memory_budget,
                now: unix_now(),
                ..RecallOptions::default()
            },
            None,
        )?;
        memory_text.push_str(&found.text);
        if let Some(note) = store.latest_handoffs(1)?.into_iter().next() {
            let left = cfg.memory_budget.saturating_sub(found.tokens);
            if left > 60 {
                let (text, _) = truncate_lines(&format!("Last handoff ({}):\n{}", note.task, note.text), left);
                memory_text.push_str(&text);
                memory_text.push('\n');
            }
        }
    }
    let prompt = build_prompt(&pack.text, &memory_text, task, "");
    Ok(Plan {
        task: task.to_string(),
        prompt_tokens: count(&prompt),
        prefix_tokens: count(STABLE_PREFIX),
        prompt,
        route,
        level,
        pack,
        memory_text,
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct RunOutcome {
    pub task_id: String,
    /// One record per attempt, in order.
    pub attempts: Vec<UsageRecord>,
    pub final_message: String,
    /// `None` when no checks ran.
    pub checks_passed: Option<bool>,
    /// The filtered check output of the last attempt.
    pub check_text: String,
    pub plan: Plan,
}

/// Longest handoff kept, in characters.
const MAX_HANDOFF_CHARS: usize = 1600;

fn clip(text: &str, max_chars: usize) -> String {
    let t = text.trim();
    if t.chars().count() <= max_chars {
        t.to_string()
    } else {
        t.chars().take(max_chars).collect::<String>() + " ..."
    }
}

/// Run the local checks. `None` when there is nothing to run.
fn run_checks(root: &Path) -> Result<Option<lge_filter::Report>> {
    let specs = match lge_filter::load_config(root)? {
        Some(specs) => specs,
        None => lge_filter::detect_checks(root),
    };
    if specs.is_empty() {
        return Ok(None);
    }
    Ok(Some(lge_filter::run_all(root, &specs)?))
}

/// Plan and run one task: one session per attempt, checks after each, a retry
/// with only the filtered failures, the usage log and the handoff at the end.
pub fn run_task(
    store: &dyn GraphStore,
    root: &Path,
    task: &str,
    cfg: &SessionConfig,
    index: &IndexOptions,
    runner: &dyn SessionRunner,
) -> Result<RunOutcome> {
    let plan = plan_task(store, root, task, cfg, index)?;
    let started = unix_now();
    let tid = task_id(task, started);
    let mut out = RunOutcome {
        task_id: tid.clone(),
        attempts: Vec::new(),
        final_message: String::new(),
        checks_passed: None,
        check_text: String::new(),
        plan: plan.clone(),
    };
    let mut level = plan.level;
    let mut prompt = plan.prompt.clone();

    for attempt in 1..=cfg.max_attempts.max(1) as i64 {
        let attempt_started = unix_now();
        let mut rec = new_record(
            task,
            &tid,
            attempt,
            level,
            &plan.route.reason,
            runner.name(),
            attempt_started,
        );
        rec.recommended_level = plan.route.level.as_str().to_string();
        rec.mode = cfg.mode.clone();
        rec.model = cfg.model.clone();
        rec.pack_tokens = plan.pack.tokens as i64;
        rec.files = plan.route.files.clone();
        rec.services = plan.route.services.clone();

        let request = SessionRequest {
            prompt: prompt.clone(),
            cwd: root.to_path_buf(),
            model: (!cfg.model.is_empty()).then(|| cfg.model.clone()),
            effort: cfg.effort_for(level),
            sandbox: (!cfg.sandbox.is_empty()).then(|| cfg.sandbox.clone()),
        };
        let result = runner.run(&request)?;
        let mut parsed = parse_codex_jsonl(&result.events);
        // The session file has what the event stream lacks: model, effort, and
        // the verified running totals.
        if !parsed.session_id.is_empty() {
            if let Some(path) = find_rollout(&lge_usage::codex_home(), &parsed.session_id) {
                if let Ok(rollout) = parse_rollout_file(&path) {
                    parsed = parsed.merged_with(&rollout);
                }
            }
        }
        out.final_message = parsed.final_message.clone();

        let checks = if cfg.run_checks && result.exit_ok {
            run_checks(root)?
        } else {
            None
        };
        let result_word = match (&checks, result.exit_ok) {
            (_, false) => outcome::FAILURE,
            (Some(report), true) if report.passed => outcome::SUCCESS,
            (Some(_), true) => outcome::FAILURE,
            (None, true) => outcome::UNKNOWN,
        };
        out.checks_passed = checks.as_ref().map(|r| r.passed);
        out.check_text = checks.as_ref().map(lge_filter::render).unwrap_or_default();
        apply_session(&mut rec, &parsed, unix_now(), result_word);
        store.put_usage(&rec)?;
        out.attempts.push(rec);

        let failed_checks = checks.as_ref().is_some_and(|r| !r.passed);
        if !failed_checks {
            break;
        }
        // Retry: one level higher in routed mode, and only the filtered
        // failures go back, never the raw output.
        if cfg.level_mode == LevelMode::Routed {
            match level.escalate() {
                Some(next) => level = next,
                None if attempt > 1 => break,
                None => {}
            }
        }
        prompt = build_prompt(&plan.pack.text, &plan.memory_text, task, &out.check_text);
    }

    // The handoff is what the next session starts from.
    let now = unix_now();
    if !out.final_message.trim().is_empty() {
        let text = clip(&out.final_message, MAX_HANDOFF_CHARS);
        store.put_handoff(&HandoffNote {
            id: stable_id(&["handoff", &tid]),
            task_id: tid.clone(),
            task: clip(task, 200),
            created_at: now,
            text: text.clone(),
        })?;
        let fact = RetainInput {
            text: clip(
                &format!("Task: {} Result: {}", clip(task, 200), text),
                lge_memory::MAX_FACT_CHARS - 4,
            ),
            kind: lge_core::memory_kind::EXPERIENCE.to_string(),
            key: String::new(),
            tags: vec!["feature-work".to_string()],
            source: tid.clone(),
            valid_at: None,
        };
        retain(store, &fact, now)?;
        refresh_pages(store, now)?;
    }
    Ok(out)
}
