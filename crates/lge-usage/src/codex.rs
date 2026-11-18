//! Codex session output: the `codex exec --json` event stream and the
//! `rollout-*.jsonl` session files under `$CODEX_HOME/sessions`.

use anyhow::{Context, Result};
use lge_core::UsageRecord;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader};
use std::ops::{Add, AddAssign};
use std::path::{Path, PathBuf};

/// Token counts of one session, or of several sessions added up.
///
/// `cached_input_tokens` is a part of `input_tokens` and `reasoning_tokens` is a
/// part of `output_tokens` (see the crate documentation), so the total is
/// [`TokenUsage::total`], never the sum of the four fields. A runner that
/// reports cached or reasoning tokens apart from input and output must add them
/// in before it writes a record, or shares and costs come out wrong.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub reasoning_tokens: i64,
    pub output_tokens: i64,
}

impl TokenUsage {
    /// Input plus output. Cached and reasoning tokens are already inside those two.
    pub fn total(&self) -> i64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }

    /// Input tokens that were not read from the cache: the part billed at the full
    /// input price. Never negative, even if a log reports more cached than input.
    pub fn uncached_input_tokens(&self) -> i64 {
        self.input_tokens.saturating_sub(self.cached_input_tokens).max(0)
    }

    /// The four counts stored on a usage record. A negative count is read as
    /// zero, as in the parser: no session uses fewer than no tokens, and one
    /// damaged record must not be subtracted from the others in a report.
    pub fn of(rec: &UsageRecord) -> TokenUsage {
        TokenUsage {
            input_tokens: rec.input_tokens.max(0),
            cached_input_tokens: rec.cached_input_tokens.max(0),
            reasoning_tokens: rec.reasoning_tokens.max(0),
            output_tokens: rec.output_tokens.max(0),
        }
    }
}

impl Add for TokenUsage {
    type Output = TokenUsage;

    /// Saturating, because the counts come from files: a corrupt log must not
    /// overflow and abort a report.
    fn add(self, other: TokenUsage) -> TokenUsage {
        TokenUsage {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            cached_input_tokens: self.cached_input_tokens.saturating_add(other.cached_input_tokens),
            reasoning_tokens: self.reasoning_tokens.saturating_add(other.reasoning_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
        }
    }
}

impl AddAssign for TokenUsage {
    fn add_assign(&mut self, other: TokenUsage) {
        *self = *self + other;
    }
}

/// What one Codex session reported about itself. A field the log does not carry
/// is empty: the `exec --json` stream has no model and no reasoning effort.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedSession {
    /// The Codex thread id: `thread_id` of the `thread.started` event, `id` of a
    /// rollout's `session_meta` line, and the tail of the rollout file name.
    pub session_id: String,
    pub model: String,
    /// The effort Codex ran with, as Codex names it (`low`, `medium`, `high`, ...).
    pub reasoning_effort: String,
    pub usage: TokenUsage,
    /// The last message the agent wrote; empty when it wrote none.
    pub final_message: String,
    pub turns: u32,
    /// The label of a thread Codex ran in the background (an approval review,
    /// a title, a compaction, a memory consolidation), from the rollout's
    /// first `session_meta` line; `None` for a thread someone worked in (see
    /// [`background_session`]).
    #[serde(default)]
    pub background: Option<String>,
}

impl ParsedSession {
    /// Combines two views of the same session, typically the `exec --json` stream
    /// and the rollout file found for its session id.
    ///
    /// Text fields keep this view's value and take the other's only where this one
    /// is empty. The usage is never added: both views read the same running total,
    /// so the larger reading is kept.
    pub fn merged_with(self, other: &ParsedSession) -> ParsedSession {
        let or = |mine: String, theirs: &str| if mine.is_empty() { theirs.to_string() } else { mine };
        ParsedSession {
            session_id: or(self.session_id, &other.session_id),
            model: or(self.model, &other.model),
            reasoning_effort: or(self.reasoning_effort, &other.reasoning_effort),
            usage: if other.usage.total() > self.usage.total() {
                other.usage
            } else {
                self.usage
            },
            final_message: or(self.final_message, &other.final_message),
            turns: self.turns.max(other.turns),
            background: self.background.or_else(|| other.background.clone()),
        }
    }
}

/// Characters of a background label kept.
const BACKGROUND_LABEL_CHARS: usize = 40;

/// The label of a thread Codex runs in the background, by its thread source
/// (`thread_source` in a rollout's `session_meta` and in a request's turn
/// metadata; codex-rs/protocol/src/protocol.rs `ThreadSource`): a task title
/// and a recap (the TUI's temporary threads, sources `thread_title` and
/// `system`), the approval reviewer, a memory consolidation. `None` for any
/// other source (`user`, `subagent`, another feature).
pub fn background_thread_source(thread_source: &str) -> Option<&'static str> {
    match thread_source.trim() {
        "thread_title" => Some("title"),
        "system" => Some("recap"),
        "guardian_review" => Some("guardian"),
        "memory_consolidation" => Some("memory_consolidation"),
        _ => None,
    }
}

/// The default model of Codex's approval reviewer
/// (codex-rs/model-provider/src/provider.rs), whose label is `guardian`.
pub const REVIEWER_MODEL: &str = "codex-auto-review";

/// How the desktop app's request for a task's title starts (the app's own
/// prompt; it is not in codex-rs).
pub const TITLE_PROMPT: &str = "You are a helpful assistant. You will be presented with a user prompt, and your job is to provide a short title for a task that will be created from that prompt";

/// How the approval reviewer's request opens its transcript.
pub const GUARDIAN_PROMPT: &str = "The following is the Codex agent history whose request action you are assessing.";

/// How a memory consolidation's request opens.
pub const MEMORY_PROMPT: &str = "Consolidate the supplied rollout summaries into `memory_summary.md`";

/// Codex's own prompts that open a background request, each with its label.
/// Each is shorter than the 200 characters of a typed message a usage record
/// keeps, so a stored task is matched as well as a request. The wording is
/// Codex's and may change between its versions.
pub const BACKGROUND_PROMPTS: [(&str, &str); 3] = [
    (TITLE_PROMPT, "title"),
    (GUARDIAN_PROMPT, "guardian"),
    (MEMORY_PROMPT, "memory_consolidation"),
];

/// The label of a background request whose text (its instructions, or a
/// message up to and including the first one the person typed) opens with
/// one of [`BACKGROUND_PROMPTS`]; `None` for any other text.
pub fn background_task(text: &str) -> Option<&'static str> {
    let text = text.trim_start();
    BACKGROUND_PROMPTS
        .iter()
        .find(|(prompt, _)| text.starts_with(prompt))
        .map(|(_, label)| *label)
}

/// The label of a stored session that was one of Codex's background ones,
/// told from what its record keeps: its first typed message (`task`, cut to
/// 200 characters) opens with one of [`BACKGROUND_PROMPTS`], or its model is
/// the approval reviewer's. The model is the one of the session's last
/// request, so it decides only for a session that counted no background
/// request by label (`counted_background_requests` false): a session someone
/// works in whose requests once went to the reviewer counted them from
/// v0.0.3 on. A record of an earlier version counted none, so such a session
/// whose last request went to the reviewer is taken for the reviewer's.
pub fn background_record(model: &str, task: &str, counted_background_requests: bool) -> Option<&'static str> {
    background_task(task).or_else(|| {
        (!counted_background_requests && model.trim().eq_ignore_ascii_case(REVIEWER_MODEL)).then_some("guardian")
    })
}

/// The label of a thread Codex ran in the background, from its rollout's
/// `session_meta` payload, or `None` for a thread someone worked in.
/// Background: a thread source [`background_thread_source`] names; a
/// sub-agent `source` other than a spawned agent (`thread_spawn`): `review`,
/// `compact`, `memory_consolidation`, or another by name (`guardian` for the
/// approval reviewer); an internal `source` (`guardian`,
/// `memory_consolidation`). The shapes are those of codex-rs's
/// `SessionSource`: `"cli"`, `{"subagent":"review"}`,
/// `{"subagent":{"other":"guardian"}}`, `{"internal":"guardian"}`, ...
pub fn background_session(payload: &Value) -> Option<String> {
    if let Some(label) = background_thread_source(text(payload, "thread_source")) {
        return Some(label.to_string());
    }
    let cut = |s: &str| -> String {
        let s = s.trim();
        if s.is_empty() {
            "subagent".to_string()
        } else {
            s.chars().take(BACKGROUND_LABEL_CHARS).collect()
        }
    };
    let source = payload.get("source")?.as_object()?;
    if let Some(sub) = source.get("subagent") {
        return match sub {
            Value::String(s) => Some(cut(s)),
            Value::Object(o) if o.contains_key("thread_spawn") => None,
            Value::Object(o) => Some(cut(o.get("other").and_then(Value::as_str).unwrap_or(""))),
            _ => Some(cut("")),
        };
    }
    source
        .get("internal")
        .map(|internal| cut(internal.as_str().unwrap_or("internal")))
}

/// Parses a `codex exec --json` event stream or the text of a rollout file.
///
/// Every line is one JSON event. Lines that are not JSON, a last line cut off in
/// the middle, and events this function does not know are skipped: a usage log
/// must survive a crashed session and a newer Codex.
///
/// Token usage. Both formats report a running total for the whole thread, so the
/// result is one reading, not a sum:
///
/// * exec stream: `usage` of `turn.completed`.
/// * rollout: `info.total_token_usage` of the `token_count` event, and
///   `thread_token_usage` of the `token_usage_record` line (the same number,
///   written twice).
///
/// The largest reading wins. A running total only grows, so that is the last one,
/// and it stays right if two lines were written out of order. Only when a log has
/// no running total at all are the per-call counts added up, from one source.
///
/// Several sessions in one text. A running total belongs to one thread, so the
/// readings are kept per thread id (`thread.started`, `session_meta`) and the
/// result is the sum of the threads' totals: the same session twice is counted
/// once, two sessions are both counted. The session id is that of the first.
pub fn parse_codex_jsonl(text: &str) -> ParsedSession {
    let mut scan = Scan::default();
    for line in text.lines() {
        scan.line(line);
    }
    scan.finish()
}

/// Reads and parses one rollout file. The file is read line by line and invalid
/// UTF-8 is replaced, so a large or damaged file still gives its usage. When the
/// file has no `session_meta` line the session id is taken from the file name.
pub fn parse_rollout_file(path: &Path) -> Result<ParsedSession> {
    let file = fs::File::open(path).with_context(|| format!("open rollout file {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut scan = Scan::default();
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let read = reader
            .read_until(b'\n', &mut buf)
            .with_context(|| format!("read rollout file {}", path.display()))?;
        if read == 0 {
            break;
        }
        scan.line(&String::from_utf8_lossy(&buf));
    }
    let mut session = scan.finish();
    if session.session_id.is_empty() {
        session.session_id = id_from_file_name(path).unwrap_or_default();
    }
    Ok(session)
}

/// The Codex state directory: `$CODEX_HOME`, or `~/.codex`.
pub fn codex_home() -> PathBuf {
    codex_home_from(std::env::var_os("CODEX_HOME"), std::env::home_dir())
}

/// Split from [`codex_home`] so the rule can be tested without changing the
/// environment of the test process.
fn codex_home_from(codex_home: Option<OsString>, home: Option<PathBuf>) -> PathBuf {
    match codex_home.filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => home.unwrap_or_default().join(".codex"),
    }
}

/// Directories under the Codex home that hold rollout files.
const ROLLOUT_DIRS: [&str; 2] = ["sessions", "archived_sessions"];

/// `sessions/YYYY/MM/DD/<file>` is three levels deep; the limit only stops a
/// symlink loop.
const MAX_ROLLOUT_DEPTH: usize = 6;

/// The rollout file of a session:
/// `<codex_home>/sessions/YYYY/MM/DD/rollout-<timestamp>-<session_id>.jsonl`.
/// Archived sessions are searched too. `None` when the session left no file, for
/// example after `codex exec --ephemeral`.
pub fn find_rollout(codex_home: &Path, session_id: &str) -> Option<PathBuf> {
    let id = session_id.trim();
    // The id is matched against file names only; it must never act as a path.
    if id.is_empty() || id.contains(['/', '\\']) {
        return None;
    }
    let suffix = format!("-{id}.jsonl");
    ROLLOUT_DIRS
        .iter()
        .find_map(|dir| find_in(&codex_home.join(dir), &suffix, 0))
}

/// Depth first in reverse name order. Directory and file names start with the
/// date, so recent sessions are visited first and the walk order is fixed.
fn find_in(dir: &Path, suffix: &str, depth: usize) -> Option<PathBuf> {
    if depth > MAX_ROLLOUT_DEPTH {
        return None;
    }
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    entries.sort_by(|a, b| b.cmp(a));
    entries.into_iter().find_map(|path| {
        if path.is_dir() {
            find_in(&path, suffix, depth + 1)
        } else if is_rollout_named(&path, suffix) {
            Some(path)
        } else {
            None
        }
    })
}

/// True for `rollout-<timestamp><suffix>`. The part between the prefix and the id
/// must be exactly a timestamp: otherwise the tail of one id (`...-0001`) would
/// match the file of another session, and its tokens would land on the wrong task.
fn is_rollout_named(path: &Path, suffix: &str) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("rollout-"))
        .and_then(|rest| rest.strip_suffix(suffix))
        .is_some_and(is_timestamp)
}

/// `2026-01-02T03-04-05`, the form Codex puts in rollout file names.
fn is_timestamp(s: &str) -> bool {
    s.len() == 19
        && s.bytes().enumerate().all(|(i, b)| match i {
            4 | 7 | 13 | 16 => b == b'-',
            10 => b == b'T',
            _ => b.is_ascii_digit(),
        })
}

/// The session id at the end of `rollout-<timestamp>-<uuid>.jsonl`.
fn id_from_file_name(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?.strip_prefix("rollout-")?;
    let id = stem.get(stem.len().checked_sub(UUID_LEN)?..)?;
    looks_like_uuid(id).then(|| id.to_string())
}

const UUID_LEN: usize = 36;

fn looks_like_uuid(s: &str) -> bool {
    s.len() == UUID_LEN
        && s.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

/// One tool call or tool output a rollout file recorded (a `response_item`
/// line). Read for replaying past sessions; nothing of it is stored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolloutItem {
    /// The item's type: `function_call`, `function_call_output`,
    /// `custom_tool_call`, `custom_tool_call_output`, `local_shell_call` or
    /// `local_shell_call_output`.
    pub kind: String,
    pub call_id: String,
    /// The tool's name, after its namespace when it has one other than Codex's
    /// default (`mcp__server__` + `tool`), as Codex names the tool. Empty for
    /// outputs.
    pub name: String,
    /// A function call's arguments (a JSON string); a local shell call's
    /// action, as JSON.
    pub arguments: String,
    /// A custom tool call's input (a code-mode script, a patch).
    pub input: String,
    /// An output's text: the string, or the text of each `input_text` part in
    /// order. Empty for calls.
    pub outputs: Vec<String>,
}

/// Codex's namespace for its own tools (codex-rs/protocol/src/tool_name.rs).
const DEFAULT_TOOL_NAMESPACE: &str = "functions";

/// The tool calls and outputs of one rollout file, in file order. Other lines,
/// lines that are not JSON and items of other types are skipped. The file is
/// read line by line and invalid UTF-8 is replaced.
pub fn rollout_items(path: &Path) -> Result<Vec<RolloutItem>> {
    let mut items = Vec::new();
    for_each_line(path, |v| {
        if text(v, "type") != "response_item" {
            return;
        }
        let p = &v["payload"];
        let kind = text(p, "type");
        let mut item = RolloutItem {
            kind: kind.to_string(),
            call_id: text(p, "call_id").to_string(),
            ..RolloutItem::default()
        };
        match kind {
            "function_call" | "custom_tool_call" => {
                let namespace = text(p, "namespace");
                item.name = if namespace.is_empty() || namespace == DEFAULT_TOOL_NAMESPACE {
                    text(p, "name").to_string()
                } else {
                    format!("{namespace}{}", text(p, "name"))
                };
                item.arguments = text(p, "arguments").to_string();
                item.input = text(p, "input").to_string();
            }
            "local_shell_call" => {
                item.name = "local_shell".to_string();
                item.arguments = p.get("action").map(Value::to_string).unwrap_or_default();
            }
            "function_call_output" | "custom_tool_call_output" | "local_shell_call_output" => {
                item.outputs = match &p["output"] {
                    Value::String(s) => vec![s.clone()],
                    Value::Array(parts) => parts
                        .iter()
                        .filter(|part| text(part, "type") == "input_text")
                        .map(|part| text(part, "text").to_string())
                        .collect(),
                    _ => Vec::new(),
                };
            }
            _ => return,
        }
        items.push(item);
    })?;
    Ok(items)
}

/// Model requests a rollout file records: its `token_count` events that carry
/// the usage of one response (`info.last_token_usage`). Events that only carry
/// rate limits are not requests.
pub fn rollout_requests(path: &Path) -> Result<u64> {
    let mut n = 0;
    for_each_line(path, |v| {
        if text(v, "type") == "event_msg"
            && text(&v["payload"], "type") == "token_count"
            && v["payload"]["info"]["last_token_usage"].is_object()
        {
            n += 1;
        }
    })?;
    Ok(n)
}

/// The folder a rollout file's session worked in: its `session_meta` line's
/// `cwd`, else its first `turn_context` line's. `None` when neither is there
/// or the file cannot be read.
pub fn rollout_cwd(path: &Path) -> Option<String> {
    let mut meta: Option<String> = None;
    let mut turn: Option<String> = None;
    for_each_line(path, |v| {
        let cwd = text(&v["payload"], "cwd");
        if cwd.is_empty() {
            return;
        }
        match text(v, "type") {
            "session_meta" if meta.is_none() => meta = Some(cwd.to_string()),
            "turn_context" if turn.is_none() => turn = Some(cwd.to_string()),
            _ => {}
        }
    })
    .ok()?;
    meta.or(turn)
}

fn for_each_line(path: &Path, mut each: impl FnMut(&Value)) -> Result<()> {
    let file = fs::File::open(path).with_context(|| format!("open rollout file {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let read = reader
            .read_until(b'\n', &mut buf)
            .with_context(|| format!("read rollout file {}", path.display()))?;
        if read == 0 {
            return Ok(());
        }
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim().trim_start_matches('\u{feff}');
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            each(&v);
        }
    }
}

/// The rollout files under the Codex home (`sessions` and
/// `archived_sessions`) last written at or after `since_unix`, oldest name
/// first. A file whose time cannot be read is left out.
pub fn rollout_files(codex_home: &Path, since_unix: i64) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for dir in ROLLOUT_DIRS {
        collect_rollouts(&codex_home.join(dir), since_unix, 0, &mut found);
    }
    found.sort();
    found
}

fn collect_rollouts(dir: &Path, since: i64, depth: usize, found: &mut Vec<PathBuf>) {
    if depth > MAX_ROLLOUT_DEPTH {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rollouts(&path, since, depth + 1, found);
            continue;
        }
        let named = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"));
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64);
        if named && modified.is_some_and(|m| m >= since) {
            found.push(path);
        }
    }
}

/// The token readings of one thread.
#[derive(Default)]
struct ThreadUsage {
    /// The largest thread total seen so far.
    running_total: Option<TokenUsage>,
    /// Sum of `last_token_usage` over the `token_count` events.
    per_call_events: TokenUsage,
    /// Sum of `usage` over the `token_usage_record` lines. The same model calls
    /// as `per_call_events`, so the two are never added together.
    per_call_records: TokenUsage,
}

impl ThreadUsage {
    /// Takes a reading of the thread's running total.
    fn reading(&mut self, v: &Value) {
        let Some(usage) = usage_of(v) else { return };
        if self.running_total.is_none_or(|best| usage.total() >= best.total()) {
            self.running_total = Some(usage);
        }
    }

    fn usage(&self) -> TokenUsage {
        let per_call = if self.per_call_records.total() > self.per_call_events.total() {
            self.per_call_records
        } else {
            self.per_call_events
        };
        self.running_total.unwrap_or(per_call)
    }
}

/// State of one pass over a log.
#[derive(Default)]
struct Scan {
    session_id: String,
    model: String,
    reasoning_effort: String,
    final_message: String,
    /// The thread the lines being read belong to; empty until a line names one.
    thread: String,
    /// Readings per thread id. A running total is only comparable with readings
    /// of the same thread: taking the largest across two sessions would drop the
    /// smaller session, and adding within one would count it twice.
    threads: BTreeMap<String, ThreadUsage>,
    turns_started: u32,
    turns_ended: u32,
    /// The first `session_meta` line was read: it describes the thread itself.
    meta_seen: bool,
    background: Option<String>,
}

impl Scan {
    fn line(&mut self, line: &str) {
        let line = line.trim().trim_start_matches('\u{feff}');
        if line.is_empty() {
            return;
        }
        if let Ok(event) = serde_json::from_str::<Value>(line) {
            self.event(&event);
        }
    }

    fn event(&mut self, v: &Value) {
        let payload = &v["payload"];
        match text(v, "type") {
            // `codex exec --json`
            "thread.started" => self.enter_thread(text(v, "thread_id")),
            "turn.started" => self.turns_started += 1,
            "turn.completed" => {
                self.turns_ended += 1;
                self.current().reading(&v["usage"]);
            }
            "turn.failed" => self.turns_ended += 1,
            "item.completed" if text(&v["item"], "type") == "agent_message" => {
                self.message(text(&v["item"], "text"));
            }
            // rollout file
            "session_meta" => {
                // `id` is the thread itself. `session_id` names the parent session
                // when the thread is a sub-agent, so it is only a fallback.
                self.enter_thread(text(payload, "id"));
                self.set_session_id(text(payload, "session_id"));
                if !self.meta_seen {
                    self.meta_seen = true;
                    self.background = background_session(payload);
                }
            }
            "turn_context" => self.turn_context(payload),
            "token_usage_record" => {
                let thread = self.current();
                thread.reading(&payload["thread_token_usage"]);
                add_usage(&mut thread.per_call_records, &payload["usage"]);
            }
            "event_msg" => self.event_msg(payload),
            "response_item" if text(payload, "type") == "message" && text(payload, "role") == "assistant" => {
                self.message(&content_text(&payload["content"]));
            }
            _ => {}
        }
    }

    fn event_msg(&mut self, p: &Value) {
        match text(p, "type") {
            "token_count" => {
                // `info` is null on events that only carry rate limits.
                let thread = self.current();
                thread.reading(&p["info"]["total_token_usage"]);
                add_usage(&mut thread.per_call_events, &p["info"]["last_token_usage"]);
            }
            // Codex writes `task_*`; `turn_*` are its newer names for the same events.
            "task_started" | "turn_started" => self.turns_started += 1,
            "task_complete" | "turn_complete" => {
                self.turns_ended += 1;
                self.message(text(p, "last_agent_message"));
            }
            "turn_aborted" => self.turns_ended += 1,
            "agent_message" => self.message(text(p, "message")),
            "item_completed" if text(&p["item"], "type") == "AgentMessage" => {
                self.message(&content_text(&p["item"]["content"]));
            }
            _ => {}
        }
    }

    /// The settings of the latest turn win: they are the ones the session ended with.
    fn turn_context(&mut self, p: &Value) {
        let settings = &p["collaboration_mode"]["settings"];
        for model in [text(settings, "model"), text(p, "model")] {
            set_if_present(&mut self.model, model);
        }
        for effort in [text(settings, "reasoning_effort"), text(p, "effort")] {
            set_if_present(&mut self.reasoning_effort, effort);
        }
    }

    /// The first id wins: the first line of a rollout describes the thread itself.
    fn set_session_id(&mut self, id: &str) {
        if self.session_id.is_empty() {
            self.session_id = id.to_string();
        }
    }

    /// The lines that follow belong to thread `id`. Readings taken before any
    /// thread was named are given to the first one that is: they are the same
    /// session with its first line missing, not another session.
    fn enter_thread(&mut self, id: &str) {
        if id.is_empty() || id == self.thread {
            return;
        }
        if let Some(unnamed) = self.threads.remove("") {
            self.threads.insert(id.to_string(), unnamed);
        }
        self.thread = id.to_string();
        self.set_session_id(id);
    }

    /// The readings of the thread being read.
    fn current(&mut self) -> &mut ThreadUsage {
        self.threads.entry(self.thread.clone()).or_default()
    }

    /// The same message appears several times in a rollout (response item, turn
    /// item, turn summary); keeping the last non-empty one covers all of them.
    fn message(&mut self, message: &str) {
        set_if_present(&mut self.final_message, message);
    }

    fn finish(self) -> ParsedSession {
        let usage = self
            .threads
            .values()
            .fold(TokenUsage::default(), |sum, thread| sum + thread.usage());
        ParsedSession {
            session_id: self.session_id,
            model: self.model,
            reasoning_effort: self.reasoning_effort,
            usage,
            final_message: self.final_message,
            // A turn that was cut off has a start and no end; an old log may have
            // only the end. Either way it was one turn.
            turns: self.turns_started.max(self.turns_ended),
            background: self.background,
        }
    }
}

fn set_if_present(slot: &mut String, value: &str) {
    if !value.trim().is_empty() {
        *slot = value.to_string();
    }
}

fn add_usage(sum: &mut TokenUsage, v: &Value) {
    if let Some(usage) = usage_of(v) {
        *sum += usage;
    }
}

/// One Codex usage object. `None` when `v` is not one, so that a missing or null
/// object is never mistaken for a reading of zero.
fn usage_of(v: &Value) -> Option<TokenUsage> {
    let fields = v.as_object()?;
    if !fields.contains_key("input_tokens") && !fields.contains_key("output_tokens") {
        return None;
    }
    Some(TokenUsage {
        input_tokens: count(v, "input_tokens"),
        cached_input_tokens: count(v, "cached_input_tokens"),
        reasoning_tokens: count(v, "reasoning_output_tokens"),
        output_tokens: count(v, "output_tokens"),
    })
}

/// A token count; 0 when the field is missing, negative or not a number.
fn count(v: &Value, key: &str) -> i64 {
    let n = &v[key];
    n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)).unwrap_or(0).max(0)
}

/// A string field; "" when it is missing or not a string.
fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Text of a message whose content is a list of parts (`output_text` parts in a
/// response item, `Text` parts in a turn item).
fn content_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().map(|part| text(part, "text")).collect(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shapes copied from codex-cli 0.160.0 rollout files; ids and text are made up.
    const ROLLOUT: &str = include_str!("../tests/fixtures/rollout.jsonl");
    /// Shapes of the `codex exec --json` events; ids and text are made up.
    const EXEC_STREAM: &str = include_str!("../tests/fixtures/exec_stream.jsonl");

    const ROLLOUT_ID: &str = "00000000-0000-7000-8000-000000000001";
    const EXEC_ID: &str = "00000000-0000-7000-8000-000000000002";

    fn usage(input: i64, cached: i64, reasoning: i64, output: i64) -> TokenUsage {
        TokenUsage {
            input_tokens: input,
            cached_input_tokens: cached,
            reasoning_tokens: reasoning,
            output_tokens: output,
        }
    }

    #[test]
    fn token_usage_adds_by_class_and_totals_input_plus_output() {
        let mut sum = usage(1000, 400, 30, 100) + usage(500, 100, 20, 50);
        assert_eq!(sum, usage(1500, 500, 50, 150));
        assert_eq!(sum.total(), 1650, "cached and reasoning are parts, not additions");
        assert_eq!(sum.uncached_input_tokens(), 1000);
        sum += usage(1, 1, 1, 1);
        assert_eq!(sum, usage(1501, 501, 51, 151));
        assert_eq!(usage(10, 50, 0, 0).uncached_input_tokens(), 0, "never negative");
        assert_eq!(
            (usage(i64::MAX, 0, 0, 0) + usage(1, 0, 0, 1)).total(),
            i64::MAX,
            "saturates"
        );
    }

    #[test]
    fn token_usage_reads_a_record() {
        let rec = UsageRecord {
            input_tokens: 9,
            cached_input_tokens: 4,
            reasoning_tokens: 2,
            output_tokens: 3,
            ..UsageRecord::default()
        };
        assert_eq!(TokenUsage::of(&rec), usage(9, 4, 2, 3));
    }

    #[test]
    fn rollout_takes_the_last_running_total() {
        let s = parse_codex_jsonl(ROLLOUT);
        assert_eq!(s.session_id, ROLLOUT_ID, "the thread id, not the parent session id");
        assert_eq!(s.model, "gpt-test");
        assert_eq!(s.reasoning_effort, "low");
        // Two model calls: 1000/200/20/50 and 1500/1200/30/80. Each total is in the
        // file twice (token_usage_record and token_count); it must be counted once.
        assert_eq!(s.usage, usage(2500, 1400, 50, 130));
        assert_eq!(s.usage.total(), 2630);
        assert_eq!(s.final_message, "Done: renamed the helper.");
        assert_eq!(s.turns, 1);
    }

    #[test]
    fn exec_stream_reads_thread_usage_and_last_message() {
        let s = parse_codex_jsonl(EXEC_STREAM);
        assert_eq!(s.session_id, EXEC_ID);
        assert_eq!(s.usage, usage(2400, 1200, 120, 300));
        assert_eq!(s.final_message, "All tests pass.");
        assert_eq!(s.turns, 1);
        assert_eq!(s.model, "", "the stream does not name the model");
        assert_eq!(s.reasoning_effort, "", "nor the effort");
    }

    #[test]
    fn merging_stream_and_rollout_fills_gaps_and_never_adds_usage() {
        let stream = parse_codex_jsonl(EXEC_STREAM);
        let rollout = ParsedSession {
            session_id: "another-id".to_string(),
            model: "gpt-test".to_string(),
            reasoning_effort: "low".to_string(),
            usage: stream.usage,
            final_message: "from the rollout".to_string(),
            turns: 2,
            background: None,
        };
        let merged = stream.clone().merged_with(&rollout);
        assert_eq!(merged.session_id, EXEC_ID, "the stream's own values stay");
        assert_eq!(merged.final_message, "All tests pass.");
        assert_eq!(merged.model, "gpt-test", "the rollout fills what the stream lacks");
        assert_eq!(merged.reasoning_effort, "low");
        assert_eq!(merged.usage, stream.usage, "the same total twice is still one total");
        assert_eq!(merged.turns, 2);

        let fuller = ParsedSession {
            usage: usage(2400, 1200, 120, 301),
            ..ParsedSession::default()
        };
        assert_eq!(
            stream.clone().merged_with(&fuller).usage,
            fuller.usage,
            "the larger reading wins"
        );
        assert_eq!(fuller.clone().merged_with(&stream).usage, fuller.usage);
        assert_eq!(stream.clone().merged_with(&ParsedSession::default()), stream);
        assert_eq!(ParsedSession::default().merged_with(&stream), stream);
    }

    #[test]
    fn exec_stream_totals_are_cumulative_across_turns() {
        let text = r#"{"type":"thread.started","thread_id":"t-1"}
{"type":"turn.started"}
{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"first"}}
{"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":10,"output_tokens":20,"reasoning_output_tokens":5}}
{"type":"turn.started"}
{"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":"second"}}
{"type":"turn.completed","usage":{"input_tokens":250,"cached_input_tokens":110,"output_tokens":45,"reasoning_output_tokens":9}}
"#;
        let s = parse_codex_jsonl(text);
        assert_eq!(s.usage, usage(250, 110, 9, 45), "the last total, not 350/120/14/65");
        assert_eq!(s.turns, 2);
        assert_eq!(s.final_message, "second");
    }

    #[test]
    fn failed_turn_counts_as_a_turn_and_keeps_earlier_usage() {
        let text = r#"{"type":"thread.started","thread_id":"t-2"}
{"type":"turn.started"}
{"type":"error","message":"stream disconnected"}
{"type":"turn.failed","error":{"message":"stream disconnected"}}
"#;
        let s = parse_codex_jsonl(text);
        assert_eq!(s.session_id, "t-2");
        assert_eq!(s.turns, 1);
        assert_eq!(s.usage, TokenUsage::default());
        assert_eq!(s.final_message, "");
    }

    #[test]
    fn older_exec_usage_without_reasoning_field_reads_zero() {
        let s = parse_codex_jsonl(
            r#"{"type":"turn.completed","usage":{"input_tokens":7,"cached_input_tokens":3,"output_tokens":2}}"#,
        );
        assert_eq!(s.usage, usage(7, 3, 0, 2));
        assert_eq!(s.turns, 1, "an end without a start is still one turn");
    }

    #[test]
    fn malformed_input_is_skipped_not_fatal() {
        let text = [
            "",
            "   ",
            "Reading prompt from stdin...",
            "{not json}",
            "42",
            "\"a string\"",
            "[1,2,3]",
            "null",
            r#"{"type":42}"#,
            r#"{"no_type":true}"#,
            r#"{"type":"some.future.event","usage":{"input_tokens":999999}}"#,
            r#"{"type":"event_msg","payload":{"type":"future_event","info":{"total_token_usage":{"input_tokens":999999}}}}"#,
            r#"{"type":"event_msg","payload":null}"#,
            r#"{"type":"event_msg"}"#,
            r#"{"type":"event_msg","payload":{"type":"token_count","info":null,"rate_limits":{"limit_id":"codex"}}}"#,
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":"n/a"}}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_complete","last_agent_message":null}}"#,
            r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"AgentMessage","content":7}}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":null}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"not the agent"}]}}"#,
            r#"{"type":"session_meta","payload":{"id":17}}"#,
            r#"{"type":"turn_context","payload":{"model":null,"effort":["low"]}}"#,
            r#"{"type":"turn.completed","usage":null}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":"12","cached_input_tokens":-5,"output_tokens":3.0,"reasoning_output_tokens":{}}}"#,
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"still here"}}"#,
            r#"{"type":"item.completed","item":"oops"}"#,
            // A last line cut off in the middle of a write.
            r#"{"type":"turn.completed","usage":{"input_tokens":5000,"cached_input_to"#,
        ]
        .join("\n");
        let s = parse_codex_jsonl(&text);
        assert_eq!(s.session_id, "");
        assert_eq!(s.model, "");
        assert_eq!(s.reasoning_effort, "");
        // Only the one well-formed usage object counts: a string is not a number,
        // a negative count is zero, 3.0 is three.
        assert_eq!(s.usage, usage(0, 0, 0, 3));
        assert_eq!(s.final_message, "still here");
        assert_eq!(s.turns, 3, "task_complete and two turn.completed lines");
    }

    #[test]
    fn empty_input_gives_an_empty_session() {
        assert_eq!(parse_codex_jsonl(""), ParsedSession::default());
        assert_eq!(parse_codex_jsonl("\n\n"), ParsedSession::default());
    }

    #[test]
    fn byte_order_mark_and_crlf_are_tolerated() {
        let text = "\u{feff}{\"type\":\"thread.started\",\"thread_id\":\"t-3\"}\r\n{\"type\":\"turn.started\"}\r\n";
        let s = parse_codex_jsonl(text);
        assert_eq!(s.session_id, "t-3");
        assert_eq!(s.turns, 1);
    }

    #[test]
    fn running_totals_out_of_order_keep_the_largest() {
        let text = r#"{"type":"token_usage_record","payload":{"usage":{"input_tokens":60,"output_tokens":6},"thread_token_usage":{"input_tokens":160,"cached_input_tokens":50,"output_tokens":16,"reasoning_output_tokens":4}}}
{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"reasoning_output_tokens":1},"last_token_usage":{"input_tokens":100,"output_tokens":10}}}}
"#;
        assert_eq!(parse_codex_jsonl(text).usage, usage(160, 50, 4, 16));
    }

    #[test]
    fn per_call_counts_are_summed_only_without_a_running_total() {
        // Two model calls, each reported by both per-call sources and by no total.
        let text = r#"{"type":"token_usage_record","payload":{"usage":{"input_tokens":100,"cached_input_tokens":10,"output_tokens":20,"reasoning_output_tokens":5}}}
{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"cached_input_tokens":10,"output_tokens":20,"reasoning_output_tokens":5}}}}
{"type":"token_usage_record","payload":{"usage":{"input_tokens":150,"cached_input_tokens":90,"output_tokens":30,"reasoning_output_tokens":7}}}
{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":150,"cached_input_tokens":90,"output_tokens":30,"reasoning_output_tokens":7}}}}
"#;
        assert_eq!(
            parse_codex_jsonl(text).usage,
            usage(250, 100, 12, 50),
            "one source, not both"
        );

        let events_only = r#"{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":100,"output_tokens":20}}}}
{"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":150,"output_tokens":30}}}}
"#;
        assert_eq!(parse_codex_jsonl(events_only).usage, usage(250, 0, 0, 50));
    }

    const TWO_EXEC_RUNS: &str = r#"{"type":"thread.started","thread_id":"t-a"}
{"type":"turn.started"}
{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"first run"}}
{"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":10,"output_tokens":20,"reasoning_output_tokens":5}}
{"type":"thread.started","thread_id":"t-b"}
{"type":"turn.started"}
{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"second run"}}
{"type":"turn.completed","usage":{"input_tokens":5000,"cached_input_tokens":110,"output_tokens":45,"reasoning_output_tokens":9}}
"#;

    #[test]
    fn two_sessions_in_one_text_are_both_counted() {
        let s = parse_codex_jsonl(TWO_EXEC_RUNS);
        // Each thread has its own running total. The largest of the two would
        // give session t-a the tokens of t-b and lose its own.
        assert_eq!(s.usage, usage(5100, 120, 14, 65));
        assert_eq!(s.session_id, "t-a", "the first session names the log");
        assert_eq!(s.turns, 2);
        assert_eq!(s.final_message, "second run");

        let other = ROLLOUT.replace(ROLLOUT_ID, EXEC_ID);
        let s = parse_codex_jsonl(&format!("{ROLLOUT}\n{other}"));
        assert_eq!(s.usage, usage(5000, 2800, 100, 260), "two rollouts, two totals");
        assert_eq!(s.session_id, ROLLOUT_ID);
    }

    #[test]
    fn the_same_session_twice_is_counted_once() {
        assert_eq!(
            parse_codex_jsonl(&format!("{ROLLOUT}\n{ROLLOUT}")).usage,
            usage(2500, 1400, 50, 130)
        );

        // A stream followed by the rollout of the same thread: one running total.
        let stream = EXEC_STREAM.replace(EXEC_ID, ROLLOUT_ID);
        let s = parse_codex_jsonl(&format!("{stream}{ROLLOUT}"));
        assert_eq!(s.usage, usage(2400, 1200, 120, 300), "the larger reading, not the sum");
        assert_eq!(s.session_id, ROLLOUT_ID);
    }

    #[test]
    fn a_thread_that_comes_back_keeps_one_running_total() {
        // Run a, run b, then run a resumed: its total goes on from 120.
        let resumed = r#"{"type":"thread.started","thread_id":"t-a"}
{"type":"turn.completed","usage":{"input_tokens":300,"cached_input_tokens":210,"output_tokens":50,"reasoning_output_tokens":11}}
"#;
        let s = parse_codex_jsonl(&format!("{TWO_EXEC_RUNS}{resumed}"));
        assert_eq!(s.usage, usage(5300, 320, 20, 95), "a: 300/210/11/50, b: 5000/110/9/45");
        assert_eq!(s.turns, 3);
    }

    #[test]
    fn readings_before_the_first_id_belong_to_that_thread() {
        // The first line is missing or came late: still one session.
        let text = r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"reasoning_output_tokens":1}}}}
{"type":"session_meta","payload":{"id":"late"}}
{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":160,"cached_input_tokens":50,"output_tokens":16,"reasoning_output_tokens":4}}}}
"#;
        let s = parse_codex_jsonl(text);
        assert_eq!(s.usage, usage(160, 50, 4, 16));
        assert_eq!(s.session_id, "late");
    }

    #[test]
    fn parent_ids_and_copied_history_do_not_add_a_session() {
        // A sub-agent thread names its parent in `session_id`; a thread started
        // from another one may carry that thread's first line and history. The
        // running total goes on across both, so it is still one total.
        let text = r#"{"type":"session_meta","payload":{"id":"child","session_id":"parent"}}
{"type":"session_meta","payload":{"id":"parent","session_id":"parent"}}
{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"reasoning_output_tokens":1}}}}
{"type":"token_usage_record","payload":{"thread_id":"child","session_id":"parent","usage":{"input_tokens":60,"output_tokens":6},"thread_token_usage":{"input_tokens":160,"cached_input_tokens":50,"output_tokens":16,"reasoning_output_tokens":4}}}
"#;
        let s = parse_codex_jsonl(text);
        assert_eq!(s.session_id, "child");
        assert_eq!(s.usage, usage(160, 50, 4, 16));
    }

    #[test]
    fn very_large_counts_saturate_instead_of_wrapping() {
        // Past u64, past i64, a float past both, and a negative past i64.
        let text = r#"{"type":"turn.completed","usage":{"input_tokens":18446744073709551615,"cached_input_tokens":99999999999999999999999999999,"output_tokens":9223372036854775807,"reasoning_output_tokens":1e300}}"#;
        let s = parse_codex_jsonl(text);
        assert_eq!(s.usage, usage(i64::MAX, i64::MAX, i64::MAX, i64::MAX));
        assert_eq!(s.usage.total(), i64::MAX);
        assert_eq!(s.usage.uncached_input_tokens(), 0);

        let negative = r#"{"type":"turn.completed","usage":{"input_tokens":-9223372036854775809,"output_tokens":2.9}}"#;
        assert_eq!(parse_codex_jsonl(negative).usage, usage(0, 0, 0, 2));

        // Two saturated sessions in one text still add without overflow.
        let two = format!(
            "{{\"type\":\"thread.started\",\"thread_id\":\"a\"}}\n{text}\n{{\"type\":\"thread.started\",\"thread_id\":\"b\"}}\n{text}\n"
        );
        assert_eq!(parse_codex_jsonl(&two).usage.total(), i64::MAX);

        // A number JSON cannot hold makes the line unreadable; the line before it stays.
        let lines = r#"{"type":"turn.completed","usage":{"input_tokens":7,"output_tokens":1}}
{"type":"turn.completed","usage":{"input_tokens":1e999,"output_tokens":5}}"#;
        assert_eq!(parse_codex_jsonl(lines).usage, usage(7, 0, 0, 1));
    }

    #[test]
    fn log_lines_around_and_inside_the_stream_are_skipped() {
        let text = [
            "\u{1b}[31mERROR\u{1b}[0m codex_core: stream error",
            r#"{"type":"thread.started","thread_id":"t-9"}"#,
            // A log prefix or suffix on the line makes it not an event.
            r#"WARN {"type":"turn.completed","usage":{"input_tokens":999,"output_tokens":9}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":888,"output_tokens":8}} trailing"#,
            "2026-01-02T03:04:05Z INFO retrying in 2s",
            r#"{"type":"turn.completed","usage":{"input_tokens":3,"output_tokens":1}}"#,
            "}{",
        ]
        .join("\n");
        let s = parse_codex_jsonl(&text);
        assert_eq!(s.session_id, "t-9");
        assert_eq!(s.usage, usage(3, 0, 0, 1));
        assert_eq!(s.turns, 1);
    }

    #[test]
    fn a_record_with_negative_counts_reads_as_zero() {
        let rec = UsageRecord {
            input_tokens: -100,
            cached_input_tokens: -900,
            reasoning_tokens: i64::MIN,
            output_tokens: 7,
            ..UsageRecord::default()
        };
        assert_eq!(TokenUsage::of(&rec), usage(0, 0, 0, 7));
    }

    #[test]
    fn later_turn_context_replaces_model_and_effort() {
        let text = r#"{"type":"turn_context","payload":{"model":"model-a","effort":"low"}}
{"type":"turn_context","payload":{"model":"model-b","effort":"high"}}
{"type":"turn_context","payload":{"collaboration_mode":{"mode":"default","settings":{"model":"model-c","reasoning_effort":"medium"}}}}
"#;
        let s = parse_codex_jsonl(text);
        assert_eq!(
            s.model, "model-c",
            "settings are the fallback when the turn names no model"
        );
        assert_eq!(s.reasoning_effort, "medium");

        let both = r#"{"type":"turn_context","payload":{"model":"model-a","effort":"low","collaboration_mode":{"settings":{"model":"model-x","reasoning_effort":"high"}}}}"#;
        let s = parse_codex_jsonl(both);
        assert_eq!(s.model, "model-a", "the turn's own fields win over the settings");
        assert_eq!(s.reasoning_effort, "low");
    }

    #[test]
    fn older_rollout_agent_message_event_is_read() {
        let text = r#"{"type":"session_meta","payload":{"session_id":"parent","id":"thread"}}
{"type":"event_msg","payload":{"type":"task_started","turn_id":"x"}}
{"type":"event_msg","payload":{"type":"agent_message","message":"older shape","phase":"final_answer","memory_citation":null}}
{"type":"event_msg","payload":{"type":"task_complete","turn_id":"x","last_agent_message":null,"error":{"message":"limit"}}}
"#;
        let s = parse_codex_jsonl(text);
        assert_eq!(s.session_id, "thread");
        assert_eq!(
            s.final_message, "older shape",
            "a null summary does not erase the message"
        );
        assert_eq!(s.turns, 1);
    }

    #[test]
    fn session_id_falls_back_to_session_id_field() {
        let s = parse_codex_jsonl(r#"{"type":"session_meta","payload":{"session_id":"only-this"}}"#);
        assert_eq!(s.session_id, "only-this");
    }

    #[test]
    fn find_rollout_matches_the_id_at_the_end_of_the_name() {
        let home = tempfile::tempdir().unwrap();
        let day = home.path().join("sessions/2026/01/02");
        fs::create_dir_all(&day).unwrap();
        let wanted = day.join(format!("rollout-2026-01-02T03-04-05-{ROLLOUT_ID}.jsonl"));
        fs::write(&wanted, ROLLOUT).unwrap();
        fs::write(day.join(format!("rollout-2026-01-02T03-04-06-{EXEC_ID}.jsonl")), "").unwrap();
        fs::write(day.join(format!("notes-{ROLLOUT_ID}.jsonl")), "").unwrap();
        fs::write(
            day.join(format!("rollout-2026-01-02T03-04-05-{ROLLOUT_ID}.jsonl.bak")),
            "",
        )
        .unwrap();

        assert_eq!(find_rollout(home.path(), ROLLOUT_ID), Some(wanted.clone()));
        assert_eq!(find_rollout(home.path(), &format!("  {ROLLOUT_ID}\n")), Some(wanted));
        assert_eq!(find_rollout(home.path(), "00000000-0000-7000-8000-00000000ffff"), None);
        assert_eq!(
            find_rollout(home.path(), "000000000001"),
            None,
            "a tail of an id is not the id"
        );
        assert_eq!(find_rollout(home.path(), ""), None);
        assert_eq!(find_rollout(home.path(), "../x"), None);
        assert_eq!(find_rollout(&home.path().join("missing"), ROLLOUT_ID), None);
    }

    #[test]
    fn find_rollout_looks_in_archived_sessions_too() {
        let home = tempfile::tempdir().unwrap();
        let archive = home.path().join("archived_sessions");
        fs::create_dir_all(&archive).unwrap();
        let wanted = archive.join(format!("rollout-2025-12-31T23-59-59-{EXEC_ID}.jsonl"));
        fs::write(&wanted, "").unwrap();
        assert_eq!(find_rollout(home.path(), EXEC_ID), Some(wanted));
    }

    #[test]
    fn parse_rollout_file_reads_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join(format!("rollout-2026-01-02T03-04-05-{ROLLOUT_ID}.jsonl"));
        fs::write(&path, ROLLOUT).unwrap();
        assert_eq!(parse_rollout_file(&path).unwrap(), parse_codex_jsonl(ROLLOUT));
    }

    #[test]
    fn parse_rollout_file_survives_bad_bytes_and_a_cut_last_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("rollout-2026-01-02T03-04-05-{EXEC_ID}.jsonl"));
        let mut bytes = b"\xff\xfe not utf-8 \x80\n".to_vec();
        bytes.extend_from_slice(
            br#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":40,"cached_input_tokens":8,"output_tokens":4,"reasoning_output_tokens":1}}}}"#,
        );
        bytes.extend_from_slice(b"\n{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_cou");
        fs::write(&path, bytes).unwrap();

        let s = parse_rollout_file(&path).unwrap();
        assert_eq!(s.usage, usage(40, 8, 1, 4));
        assert_eq!(
            s.session_id, EXEC_ID,
            "no session_meta line: the id comes from the file name"
        );
    }

    #[test]
    fn a_rollout_of_a_background_thread_is_labelled_from_its_session_meta() {
        let label = |meta: &str| {
            parse_codex_jsonl(&format!(
                "{{\"type\":\"session_meta\",\"payload\":{meta}}}\n{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"later\",\"source\":{{\"subagent\":\"review\"}}}}}}"
            ))
            .background
        };
        // The approval reviewer (codex-rs/state/src/extract.rs is_guardian_review_source).
        assert_eq!(
            label(r#"{"id":"t","source":{"subagent":{"other":"guardian"}}}"#).as_deref(),
            Some("guardian")
        );
        assert_eq!(
            label(r#"{"id":"t","source":{"subagent":"review"}}"#).as_deref(),
            Some("review")
        );
        assert_eq!(
            label(r#"{"id":"t","source":{"subagent":"compact"}}"#).as_deref(),
            Some("compact")
        );
        assert_eq!(
            label(r#"{"id":"t","source":{"internal":"memory_consolidation"}}"#).as_deref(),
            Some("memory_consolidation")
        );
        assert_eq!(
            label(r#"{"id":"t","source":"vscode","thread_source":"guardian_review"}"#).as_deref(),
            Some("guardian")
        );
        assert_eq!(
            label(r#"{"id":"t","source":"cli","thread_source":"thread_title"}"#).as_deref(),
            Some("title")
        );
        // Threads someone works in; only the first line describes the thread.
        for meta in [
            r#"{"id":"t","source":"cli"}"#,
            r#"{"id":"t","source":"vscode","thread_source":"user"}"#,
            r#"{"id":"t","source":{"subagent":{"thread_spawn":{"parent_thread_id":"p","depth":1}}},"thread_source":"subagent"}"#,
            r#"{"id":"t"}"#,
        ] {
            assert_eq!(label(meta), None, "{meta}");
        }
    }

    #[test]
    fn parse_rollout_file_reports_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let err = parse_rollout_file(&dir.path().join("rollout-none.jsonl")).unwrap_err();
        assert!(format!("{err:#}").contains("rollout-none.jsonl"), "{err:#}");
    }

    #[test]
    fn file_name_id_must_be_a_uuid() {
        let id = |name: &str| id_from_file_name(Path::new(name));
        assert_eq!(
            id(&format!("rollout-2026-01-02T03-04-05-{ROLLOUT_ID}.jsonl")),
            Some(ROLLOUT_ID.to_string())
        );
        assert_eq!(id("rollout-short.jsonl"), None);
        assert_eq!(
            id("rollout-2026-01-02T03-04-05-this-is-not-a-uuid-but-is-36-chars-long.jsonl"),
            None
        );
        assert_eq!(id(&format!("{ROLLOUT_ID}.jsonl")), None, "not a rollout file");
    }

    #[test]
    fn codex_home_prefers_the_environment_variable() {
        let home = Some(PathBuf::from("/home/dev"));
        assert_eq!(
            codex_home_from(Some("/srv/codex".into()), home.clone()),
            PathBuf::from("/srv/codex")
        );
        assert_eq!(codex_home_from(None, home.clone()), PathBuf::from("/home/dev/.codex"));
        assert_eq!(
            codex_home_from(Some("".into()), home),
            PathBuf::from("/home/dev/.codex"),
            "empty is unset"
        );
        assert_eq!(codex_home_from(None, None), PathBuf::from(".codex"));
        assert!(codex_home().ends_with(".codex") || std::env::var_os("CODEX_HOME").is_some());
    }

    /// Function calls, custom tool calls (a code-mode script, a patch), a
    /// namespaced MCP call, a local shell call and outputs as strings and as
    /// part lists; ids and text are made up. The last line is cut.
    const CALLS: &str = include_str!("../tests/fixtures/rollout_calls.jsonl");

    #[test]
    fn rollout_items_reads_calls_and_outputs_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        fs::write(&path, CALLS).unwrap();
        let items = rollout_items(&path).unwrap();
        let kinds: Vec<(&str, &str, &str)> = items
            .iter()
            .map(|i| (i.kind.as_str(), i.call_id.as_str(), i.name.as_str()))
            .collect();
        assert_eq!(
            kinds,
            [
                ("function_call", "call_1", "exec_command"),
                ("function_call_output", "call_1", ""),
                ("custom_tool_call", "call_2", "exec"),
                ("custom_tool_call_output", "call_2", ""),
                ("function_call", "call_3", "mcp__docs__lookup"),
                ("function_call_output", "call_3", ""),
                ("custom_tool_call", "call_4", "apply_patch"),
                ("custom_tool_call_output", "call_4", ""),
                ("local_shell_call", "call_5", "local_shell"),
                ("function_call_output", "call_5", ""),
                ("function_call", "call_6", "shell"),
                ("function_call_output", "call_6", ""),
            ]
        );
        let args: Value = serde_json::from_str(&items[0].arguments).unwrap();
        assert_eq!(args["cmd"], "pytest -q");
        assert_eq!(
            items[1].outputs,
            ["Exit code: 0\nWall time: 1.2 seconds\nOutput:\n3 passed in 0.10s\n"]
        );
        assert!(items[2].input.contains("tools.exec_command"));
        // Text parts only, in order; the image part is not text.
        assert_eq!(
            items[3].outputs,
            [
                "Script completed\nWall time 0.4 seconds\nOutput:\n",
                "src/a.py:3:def helper():\n"
            ]
        );
        assert_eq!(items[5].outputs, ["{\"hits\": 1}"]);
        assert!(items[6].input.starts_with("*** Begin Patch"));
        let action: Value = serde_json::from_str(&items[8].arguments).unwrap();
        assert_eq!(action["command"][2], "git status --short");
        assert!(items
            .iter()
            .filter(|i| i.kind.ends_with("_call"))
            .all(|i| i.outputs.is_empty()));
        assert_eq!(
            rollout_requests(&path).unwrap(),
            2,
            "the rate-limit event is not a request"
        );
        assert!(rollout_items(&dir.path().join("missing.jsonl")).is_err());
    }

    #[test]
    fn rollout_files_lists_recent_rollouts_in_both_folders() {
        let home = tempfile::tempdir().unwrap();
        let day = home.path().join("sessions/2026/01/03");
        let archived = home.path().join("archived_sessions");
        fs::create_dir_all(&day).unwrap();
        fs::create_dir_all(&archived).unwrap();
        let a = day.join(format!("rollout-2026-01-03T04-05-00-{ROLLOUT_ID}.jsonl"));
        let b = archived.join(format!("rollout-2026-01-02T04-05-00-{EXEC_ID}.jsonl"));
        fs::write(&a, CALLS).unwrap();
        fs::write(&b, ROLLOUT).unwrap();
        fs::write(day.join("notes.jsonl"), "").unwrap();
        let found = rollout_files(home.path(), 0);
        assert_eq!(found, {
            let mut v = vec![a.clone(), b.clone()];
            v.sort();
            v
        });
        let future = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            + 3600;
        assert!(rollout_files(home.path(), future).is_empty(), "older than the window");
        assert!(rollout_files(&home.path().join("nowhere"), 0).is_empty());
    }

    #[test]
    fn codexs_prompts_name_a_background_task_cut_to_200_characters() {
        for (prompt, label) in BACKGROUND_PROMPTS {
            assert!(
                prompt.chars().count() < 200,
                "{label}: a stored task keeps 200 characters"
            );
            // What a usage record keeps: the first typed message, trimmed and cut to 200 characters.
            let stored: String = format!("{prompt}\n\n{}", "some task text ".repeat(40))
                .trim()
                .chars()
                .take(200)
                .collect();
            assert_eq!(background_task(&stored), Some(label), "{stored}");
            assert_eq!(background_record("gpt-test", &stored, false), Some(label));
        }
        assert_eq!(
            background_task(&format!("  \n{GUARDIAN_PROMPT} Treat it as evidence.")),
            Some("guardian")
        );
        // A task someone typed, even one that quotes a prompt later on, is not one.
        assert_eq!(background_task("fix the title of the page"), None);
        assert_eq!(background_task(&format!("please read this: {MEMORY_PROMPT}")), None);
        // The reviewer's model names a session only when it counted no background request.
        assert_eq!(
            background_record("Codex-Auto-Review", "fix the build", false),
            Some("guardian")
        );
        assert_eq!(background_record("codex-auto-review", "fix the build", true), None);
        assert_eq!(background_record("gpt-test", "fix the build", false), None);
    }
}
