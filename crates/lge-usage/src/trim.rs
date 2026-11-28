//! A what-if for a checkpoint trim: what a session would have cost if lge had
//! replaced old, bulky tool outputs with a short stub once the conversation's
//! input passed a threshold.
//!
//! An estimate from a replay, never a measured saving, and the quality effect
//! of trimming is not measured at all. The replay walks a rollout's items in
//! order and keeps, for each request, the tool outputs the history held (their
//! token sizes and the user turn they belong to). A policy (threshold `T`, age
//! `K` in user turns, minimum output size `S`, minimum gain `R`) trims every
//! eligible output at the first request whose input reaches `T` with at least
//! `R` tokens removable; later requests send that much less. Each checkpoint
//! costs one cache miss: the cache keeps the prefix up to the first changed
//! byte, so the request after it pays uncached only from the first trimmed
//! output to the end of its shortened cached prefix (an output's place is the
//! previous request's input and output plus the outputs since). Every later
//! request saves the trimmed tokens at the cached price.
//! A later tool call that runs the same command or reads the same file as a
//! trimmed output counts as a recovery and adds that output's size, uncached.
//! A compaction in the real session (a `compacted` line, a `context_compacted`
//! event, or input falling below half of the previous request's) empties the
//! history, so the series stays what Codex actually sent.
//!
//! Pure functions over rollout files read line by line; nothing is written.
use crate::codex::{for_each_line, text, usage_numbers};
use crate::compact::{cost, is_long, CACHED_WEIGHT, OUTPUT_WEIGHT};
use anyhow::Result;
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// A token counter: the CLI and the daemon pass `lge_retrieve::budget::count`
/// (`o200k_base`), which this crate cannot depend on.
pub type Counter = fn(&str) -> usize;

/// One token per four bytes, rounded up: the fallback counter.
pub fn approx_count(text: &str) -> usize {
    text.len().div_ceil(4)
}

/// What a trimmed output would still cost: the stub that replaces it.
pub const STUB_TOKENS: i64 = 20;
/// The stub's text, with `<tokens>` for the trimmed output's size.
pub const STUB: &str = "[output elided by lge: <tokens> tokens; run the command again to see it]";
/// The output item types a checkpoint may stub.
const OUTPUT_KINDS: [&str; 3] = [
    "function_call_output",
    "custom_tool_call_output",
    "local_shell_call_output",
];
/// Programs whose single file argument is a file read.
const READERS: [&str; 7] = ["cat", "head", "tail", "nl", "bat", "less", "sed"];

/// One trim policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Policy {
    /// `T`: a checkpoint is considered when a request's input (after earlier trims) reaches this.
    pub threshold: i64,
    /// `K`: an output is old enough from this many user turns before the current one.
    pub age_turns: u32,
    /// `S`: an output is bulky enough from this many tokens.
    pub min_output: i64,
    /// `R`: a checkpoint needs at least this many removable tokens.
    pub min_gain: i64,
}

impl Policy {
    pub fn label(&self) -> String {
        format!(
            "T={}k K={} S={} R={}k",
            self.threshold / 1000,
            self.age_turns,
            self.min_output,
            self.min_gain / 1000
        )
    }
}

/// `T` in {100k, 150k}, `K` in {2, 4}, `S` in {500, 2000}, `R` = 20k.
pub fn default_policies() -> Vec<Policy> {
    let mut v = Vec::new();
    for threshold in [100_000, 150_000] {
        for age_turns in [2, 4] {
            for min_output in [500, 2_000] {
                v.push(Policy {
                    threshold,
                    age_turns,
                    min_output,
                    min_gain: 20_000,
                });
            }
        }
    }
    v
}

/// One step of a rollout, in file order. Sizes and keys only: no text is kept.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A user turn started.
    Turn,
    /// A tool call, with what identifies it for the recovery signal
    /// (`cmd:<command line>`, `file:<path read>`).
    Call { call_id: String, keys: Vec<String> },
    /// A tool output and its size.
    Output { call_id: String, tokens: i64 },
    /// A model request: `last_token_usage` (or its usage record).
    Request { input: i64, cached: i64, output: i64 },
    /// A compaction: the history restarts.
    Reset,
}

/// The steps of one rollout.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Timeline {
    pub events: Vec<Event>,
}

impl Timeline {
    pub fn requests(&self) -> usize {
        self.events
            .iter()
            .filter(|e| matches!(e, Event::Request { .. }))
            .count()
    }
}

/// Reads the steps of one rollout file (read only). Requests come from the
/// `token_usage_record` lines when the file has any, else from the
/// `token_count` events, as [`crate::rollout_series`] reads them.
pub fn read_timeline(path: &Path, count: Counter) -> Result<Timeline> {
    // (event, from a usage record): the request source is chosen at the end.
    let mut raw: Vec<(Event, Option<bool>)> = Vec::new();
    let mut records = 0usize;
    let mut seen_ids: HashSet<String> = HashSet::new();
    for_each_line(path, |v| {
        let p = &v["payload"];
        match text(v, "type") {
            "response_item" => {
                let kind = text(p, "type");
                let call_id = text(p, "call_id").to_string();
                match kind {
                    "function_call" | "custom_tool_call" | "local_shell_call" => {
                        let keys = call_keys(kind, text(p, "name"), p);
                        raw.push((Event::Call { call_id, keys }, None));
                    }
                    k if OUTPUT_KINDS.contains(&k) => {
                        let tokens = match &p["output"] {
                            Value::String(s) => count(s),
                            Value::Array(parts) => parts.iter().map(|part| count(text(part, "text"))).sum(),
                            other => other.get("content").and_then(Value::as_str).map_or(0, count),
                        };
                        raw.push((
                            Event::Output {
                                call_id,
                                tokens: tokens as i64,
                            },
                            None,
                        ));
                    }
                    _ => {}
                }
            }
            "token_usage_record" => {
                let usage = &p["usage"];
                let id = text(p, "response_id");
                if usage.is_object() && (id.is_empty() || seen_ids.insert(id.to_string())) {
                    let (input, cached, output, _, _) = usage_numbers(usage);
                    records += 1;
                    raw.push((Event::Request { input, cached, output }, Some(true)));
                }
            }
            "compacted" => raw.push((Event::Reset, None)),
            "event_msg" => match text(p, "type") {
                "task_started" | "turn_started" => raw.push((Event::Turn, None)),
                "context_compacted" => raw.push((Event::Reset, None)),
                "token_count" => {
                    let last = &p["info"]["last_token_usage"];
                    if last.is_object() {
                        let (input, cached, output, _, _) = usage_numbers(last);
                        if input > 0 {
                            raw.push((Event::Request { input, cached, output }, Some(false)));
                        }
                    }
                }
                _ => {}
            },
            _ => {}
        }
    })?;
    let use_records = records > 0;
    let events = raw
        .into_iter()
        .filter(|(_, src)| src.is_none_or(|from_record| from_record == use_records))
        .map(|(e, _)| e)
        .collect();
    Ok(Timeline { events })
}

/// What identifies a tool call for the recovery signal.
fn call_keys(kind: &str, name: &str, p: &Value) -> Vec<String> {
    let mut keys = Vec::new();
    let add_command = |keys: &mut Vec<String>, command: &str| {
        let command = command.trim();
        if command.is_empty() {
            return;
        }
        keys.push(format!("cmd:{command}"));
        if let Some(file) = file_read(command) {
            keys.push(format!("file:{file}"));
        }
    };
    match kind {
        "function_call" | "local_shell_call" => {
            let args: Value = if kind == "local_shell_call" {
                p.get("action").cloned().unwrap_or(Value::Null)
            } else {
                serde_json::from_str(text(p, "arguments")).unwrap_or(Value::Null)
            };
            if let Some(command) = command_of(&args) {
                add_command(&mut keys, &command);
            } else if let Some(path) = ["path", "file_path"]
                .iter()
                .find_map(|k| args.get(*k).and_then(Value::as_str))
            {
                keys.push(format!("file:{}", path.trim()));
            }
        }
        "custom_tool_call" if name != "apply_patch" => {
            let script = text(p, "input");
            let commands = script_commands(script);
            if commands.is_empty() {
                if !script.trim().is_empty() {
                    keys.push(format!("script:{}", script.trim()));
                }
            } else {
                for c in commands {
                    add_command(&mut keys, &c);
                }
            }
        }
        _ => {}
    }
    keys
}

/// The command line of a shell-type call's arguments: `command` as a string or
/// a list (`bash -lc <script>` gives the script), or `cmd`.
fn command_of(args: &Value) -> Option<String> {
    match args.get("command").or_else(|| args.get("cmd"))? {
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => {
            let words: Vec<&str> = parts.iter().filter_map(Value::as_str).collect();
            Some(match words.as_slice() {
                [_, flag, script] if matches!(*flag, "-lc" | "-c") => script.to_string(),
                _ => words.join(" "),
            })
        }
        _ => None,
    }
}

/// The string literals a code-mode script passes as `cmd` (`cmd: "..."`,
/// `"cmd": '...'`), unescaped. A computed command is not followed.
fn script_commands(script: &str) -> Vec<String> {
    let b: Vec<char> = script.chars().collect();
    let mut found = Vec::new();
    let mut i = 0;
    while i + 3 <= b.len() {
        let word = b[i] == 'c' && b[i + 1] == 'm' && b[i + 2] == 'd';
        let starts = i == 0 || !(b[i - 1].is_alphanumeric() || b[i - 1] == '_');
        if !(word && starts) {
            i += 1;
            continue;
        }
        let mut j = i + 3;
        if j < b.len() && (b[j] == '"' || b[j] == '\'') {
            j += 1;
        }
        while j < b.len() && b[j].is_whitespace() {
            j += 1;
        }
        if j >= b.len() || b[j] != ':' {
            i += 3;
            continue;
        }
        j += 1;
        while j < b.len() && b[j].is_whitespace() {
            j += 1;
        }
        let Some(&quote) = b.get(j).filter(|c| matches!(c, '"' | '\'' | '`')) else {
            i = j;
            continue;
        };
        j += 1;
        let mut s = String::new();
        let mut closed = false;
        while j < b.len() {
            match b[j] {
                '\\' if j + 1 < b.len() => {
                    j += 1;
                    s.push(match b[j] {
                        'n' => '\n',
                        't' => '\t',
                        c => c,
                    });
                }
                '$' if quote == '`' && b.get(j + 1) == Some(&'{') => break,
                c if c == quote => {
                    closed = true;
                    break;
                }
                c => s.push(c),
            }
            j += 1;
        }
        if closed {
            found.push(s);
        }
        i = j + 1;
    }
    found
}

/// The file a simple read command reads (`cat f`, `sed -n 1,80p f`,
/// `head -n 50 f`, `nl -ba f`): one file, no pipe, redirection or chaining.
fn file_read(command: &str) -> Option<String> {
    if command.contains(['|', ';', '&', '>', '<', '`', '$', '\n']) {
        return None;
    }
    let words: Vec<&str> = command.split_whitespace().collect();
    let (program, rest) = words.split_first()?;
    if !READERS.contains(program) {
        return None;
    }
    let mut args: Vec<&str> = Vec::new();
    let mut skip_value = false;
    for w in rest {
        if skip_value {
            skip_value = false;
            continue;
        }
        if w.starts_with('-') {
            skip_value = matches!(*program, "head" | "tail") && matches!(*w, "-n" | "-c");
            continue;
        }
        args.push(w);
    }
    if *program == "sed" {
        if !rest.contains(&"-n") || args.is_empty() {
            return None;
        }
        args.remove(0);
    }
    match args.as_slice() {
        [file] => Some(file.trim_matches(['"', '\'']).to_string()),
        _ => None,
    }
}

/// The result of one policy on one session, or summed over sessions.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Outcome {
    pub policy: Policy,
    /// Cost-weighted tokens of every request as it was.
    pub actual_cost: f64,
    /// The same requests with the trims, checkpoint misses and recoveries.
    pub trimmed_cost: f64,
    /// `actual_cost - trimmed_cost` = `saving - checkpoint_cost - recovery_cost`.
    pub net: f64,
    /// `net` as a percentage of `actual_cost`: positive is cheaper.
    pub net_pct: f64,
    /// What the smaller history saved on every request after a checkpoint.
    pub saving: f64,
    /// The cache misses: each request after a checkpoint pays uncached from the
    /// first trimmed output to the end of its shortened cached prefix.
    pub checkpoint_cost: f64,
    /// Recovered outputs read again, uncached.
    pub recovery_cost: f64,
    pub checkpoints: u64,
    pub outputs_trimmed: u64,
    /// Tokens removed (output sizes less the stubs).
    pub tokens_trimmed: i64,
    /// Trimmed outputs whose command or file a later call ran or read again.
    pub recoveries: u64,
    /// `recoveries / outputs_trimmed`.
    pub recovery_share: f64,
}

impl Outcome {
    fn new(policy: Policy) -> Outcome {
        Outcome {
            policy,
            actual_cost: 0.0,
            trimmed_cost: 0.0,
            net: 0.0,
            net_pct: 0.0,
            saving: 0.0,
            checkpoint_cost: 0.0,
            recovery_cost: 0.0,
            checkpoints: 0,
            outputs_trimmed: 0,
            tokens_trimmed: 0,
            recoveries: 0,
            recovery_share: 0.0,
        }
    }

    fn add(&mut self, o: &Outcome) {
        self.actual_cost += o.actual_cost;
        self.trimmed_cost += o.trimmed_cost;
        self.saving += o.saving;
        self.checkpoint_cost += o.checkpoint_cost;
        self.recovery_cost += o.recovery_cost;
        self.checkpoints += o.checkpoints;
        self.outputs_trimmed += o.outputs_trimmed;
        self.tokens_trimmed += o.tokens_trimmed;
        self.recoveries += o.recoveries;
    }

    fn finish(mut self) -> Outcome {
        self.net = self.actual_cost - self.trimmed_cost;
        self.net_pct = if self.actual_cost > 0.0 {
            self.net / self.actual_cost * 100.0
        } else {
            0.0
        };
        self.recovery_share = if self.outputs_trimmed > 0 {
            self.recoveries as f64 / self.outputs_trimmed as f64
        } else {
            0.0
        };
        self
    }
}

/// A tool output the history holds.
struct Held {
    tokens: i64,
    turn: u32,
    /// The request it was first sent with (index among the requests).
    first_request: usize,
    /// Tokens of the history before it (unshortened): the previous request's
    /// input and output plus the outputs since. 0 when unknown (after a
    /// compaction or a cut), which charges the whole prefix.
    before: i64,
    keys: Vec<String>,
    trimmed: bool,
    recovered: bool,
}

/// Replays one session under one policy.
pub fn simulate(tl: &Timeline, policy: &Policy) -> Outcome {
    let mut o = Outcome::new(*policy);
    let mut held: Vec<Held> = Vec::new();
    let mut call_keys: HashMap<&str, &[String]> = HashMap::new();
    let mut turn = 0u32;
    let mut request = 0usize;
    let mut prev_input: Option<i64> = None;
    // Tokens of the history at this point of the file (unshortened).
    let mut pos = 0i64;
    // After a checkpoint: the next request keeps its cache up to the first
    // trimmed output (this many tokens of the shortened history) and pays the
    // rest of its shortened prefix uncached.
    let mut pending: Option<i64> = None;
    for e in &tl.events {
        match e {
            Event::Turn => turn += 1,
            Event::Reset => {
                held.clear();
                prev_input = None;
                pending = None;
                pos = 0;
            }
            Event::Call { call_id, keys } => {
                // A call that runs or reads what a trimmed output held: the model needed it again.
                for h in held.iter_mut().filter(|h| h.trimmed && !h.recovered) {
                    if h.keys.iter().any(|k| keys.contains(k)) {
                        h.recovered = true;
                        o.recoveries += 1;
                        o.recovery_cost += h.tokens as f64;
                    }
                }
                call_keys.insert(call_id.as_str(), keys.as_slice());
            }
            Event::Output { call_id, tokens } => {
                held.push(Held {
                    tokens: *tokens,
                    turn,
                    first_request: request,
                    before: pos,
                    keys: call_keys.get(call_id.as_str()).map_or_else(Vec::new, |k| k.to_vec()),
                    trimmed: false,
                    recovered: false,
                });
                pos += tokens;
            }
            &Event::Request { input, cached, output } => {
                // Input fell below half of the last request's with no compaction
                // line: the history was cut; only what came since stays.
                if prev_input.is_some_and(|p| 2 * input < p) {
                    held.retain(|h| h.first_request == request);
                    for h in &mut held {
                        h.before = 0;
                    }
                    pending = None;
                }
                let removed: i64 = held.iter().filter(|h| h.trimmed).map(|h| h.tokens - STUB_TOKENS).sum();
                let removed = removed.min(input);
                let cached = cached.clamp(0, input);
                let actual = cost(input, cached, output);
                let sent = input - removed;
                let warm = cost(sent, (cached - removed).max(0), output);
                o.actual_cost += actual;
                o.saving += actual - warm;
                if let Some(kept) = pending.take() {
                    // The cache holds the prefix up to the first changed byte.
                    let warm_cached = (cached - removed).max(0);
                    let cold = cost(sent, kept.clamp(0, warm_cached), output);
                    o.checkpoint_cost += cold - warm;
                    o.trimmed_cost += cold;
                } else {
                    o.trimmed_cost += warm;
                }
                if sent >= policy.threshold {
                    let eligible = |h: &Held| {
                        !h.trimmed
                            && h.tokens >= policy.min_output
                            && h.tokens > STUB_TOKENS
                            && turn.saturating_sub(h.turn) >= policy.age_turns
                    };
                    let removable: i64 = held
                        .iter()
                        .filter(|h| eligible(h))
                        .map(|h| h.tokens - STUB_TOKENS)
                        .sum();
                    if removable > 0 && removable >= policy.min_gain {
                        // Where the first newly trimmed output sits in the shortened
                        // history: its position less what earlier trims removed before it.
                        let first = held.iter().filter(|h| eligible(h)).map(|h| h.before).min().unwrap_or(0);
                        let earlier: i64 = held
                            .iter()
                            .filter(|h| h.trimmed && h.before < first)
                            .map(|h| h.tokens - STUB_TOKENS)
                            .sum();
                        for h in held.iter_mut().filter(|h| eligible(h)) {
                            h.trimmed = true;
                            o.outputs_trimmed += 1;
                        }
                        o.tokens_trimmed += removable;
                        o.checkpoints += 1;
                        pending = Some((first - earlier).max(0));
                    }
                }
                prev_input = Some(input);
                pos = input + output;
                request += 1;
            }
        }
    }
    o.trimmed_cost += o.recovery_cost;
    o.finish()
}

/// Requests after a compaction in which a file read counts as a re-read of
/// what the conversation held before it.
pub const REREAD_WINDOW: usize = 20;

/// Files read again soon after a compaction: what a post-compaction note of
/// the conversation's working set could spare.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Rereads {
    pub compactions: usize,
    /// Outputs, in the first [`REREAD_WINDOW`] requests after a compaction, of
    /// calls that read a file already read before it (since the start or the
    /// compaction before).
    pub outputs: usize,
    pub tokens: i64,
    /// Their cost-weighted tokens: sent once uncached, then cached on every
    /// later request until the next compaction.
    pub cost: f64,
}

impl Rereads {
    fn add(&mut self, o: &Rereads) {
        self.compactions += o.compactions;
        self.outputs += o.outputs;
        self.tokens += o.tokens;
        self.cost += o.cost;
    }
}

/// Counts the file re-reads after each compaction of a timeline.
pub fn rereads(tl: &Timeline) -> Rereads {
    // Requests left until the next compaction, after each event.
    let mut left = vec![0usize; tl.events.len()];
    let mut n = 0;
    for (i, e) in tl.events.iter().enumerate().rev() {
        match e {
            Event::Reset => n = 0,
            Event::Request { .. } => n += 1,
            _ => {}
        }
        left[i] = n;
    }
    let mut r = Rereads::default();
    let mut before: HashSet<String> = HashSet::new();
    let mut current: HashSet<String> = HashSet::new();
    let mut since_reset: Option<usize> = None;
    let mut pending: HashMap<String, bool> = HashMap::new();
    for (i, e) in tl.events.iter().enumerate() {
        match e {
            Event::Reset => {
                r.compactions += 1;
                before = std::mem::take(&mut current);
                since_reset = Some(0);
            }
            Event::Request { .. } => {
                if let Some(k) = since_reset.as_mut() {
                    *k += 1;
                }
            }
            Event::Call { call_id, keys } => {
                let files: Vec<&String> = keys.iter().filter(|k| k.starts_with("file:")).collect();
                let in_window = since_reset.is_some_and(|k| k < REREAD_WINDOW);
                let again = in_window && files.iter().any(|f| before.contains(*f));
                pending.insert(call_id.clone(), again);
                current.extend(files.into_iter().cloned());
            }
            Event::Output { call_id, tokens } => {
                if pending.remove(call_id) == Some(true) {
                    r.outputs += 1;
                    r.tokens += tokens;
                    r.cost += *tokens as f64 * (1.0 + CACHED_WEIGHT * left[i] as f64);
                }
            }
            Event::Turn => {}
        }
    }
    r
}

/// One long session under every policy.
#[derive(Debug, Clone, Serialize)]
pub struct SessionTrim {
    pub session_id: String,
    pub model: String,
    pub requests: usize,
    pub tool_outputs: usize,
    pub tool_output_tokens: i64,
    pub actual_cost: f64,
    /// Files read again soon after a compaction.
    pub rereads: Rereads,
    /// One per policy, in the order of [`TrimWhatIf::policies`].
    pub outcomes: Vec<Outcome>,
}

/// Every policy on one rollout's timeline.
pub fn analyse(session_id: &str, model: &str, tl: &Timeline, policies: &[Policy]) -> SessionTrim {
    let outcomes: Vec<Outcome> = policies.iter().map(|p| simulate(tl, p)).collect();
    let outputs = tl.events.iter().filter_map(|e| match e {
        Event::Output { tokens, .. } => Some(*tokens),
        _ => None,
    });
    SessionTrim {
        session_id: session_id.to_string(),
        model: model.to_string(),
        requests: tl.requests(),
        tool_outputs: outputs.clone().count(),
        tool_output_tokens: outputs.sum(),
        actual_cost: outcomes.first().map_or(0.0, |o| o.actual_cost),
        rereads: rereads(tl),
        outcomes,
    }
}

/// Options of the what-if.
#[derive(Debug, Clone)]
pub struct Options {
    pub policies: Vec<Policy>,
    pub count: Counter,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            policies: default_policies(),
            count: approx_count,
        }
    }
}

/// The warning printed with every figure.
pub const CAVEAT: &str = "This is an estimate from a replay of your sessions, not a measured saving. The quality effects of trimming are not measured: the model may work differently without the old outputs. A recovery is a later call with the same command or file as a trimmed output; commands that are simply run again (tests, git status) count too, so it overstates what trimming caused. After a checkpoint the next request pays uncached from the first trimmed output to the end of its cached history (where an output sits is estimated from the request before it; unknown after a compaction, then the whole history is charged). Compactions stay where they happened, and output tokens are kept as they were.";

/// What the command and the daemon give.
#[derive(Debug, Clone, Serialize)]
pub struct TrimWhatIf {
    pub generated_at: i64,
    pub days: i64,
    pub weights: Value,
    pub stub_tokens: i64,
    /// Rollouts read, and how many were long enough to replay.
    pub rollouts_read: usize,
    pub policies: Vec<Policy>,
    /// Each policy summed over the sessions.
    pub totals: Vec<Outcome>,
    /// The policy with the largest net saving, by index.
    pub best: Option<usize>,
    /// File re-reads after compactions, over the sessions.
    pub rereads: Rereads,
    pub sessions: Vec<SessionTrim>,
    pub caveats: Vec<&'static str>,
}

/// Reads the rollouts of the last `days` days under `codex_home` (read only),
/// keeps the long sessions ([`is_long`]; `session` narrows by id prefix) and
/// replays each under every policy.
pub fn whatif(codex_home: &Path, days: i64, session: Option<&str>, opts: &Options) -> TrimWhatIf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let files = crate::codex::rollout_files(codex_home, now - days * 86_400);
    let mut sessions = Vec::new();
    let mut seen = HashSet::new();
    let mut read = 0;
    for f in &files {
        let Ok(series) = crate::codex::rollout_series(f) else {
            continue;
        };
        if series.background.is_some() || series.requests.is_empty() || !seen.insert(series.session_id.clone()) {
            continue;
        }
        read += 1;
        if session.is_some_and(|p| !series.session_id.starts_with(p)) || !is_long(&series) {
            continue;
        }
        let Ok(tl) = read_timeline(f, opts.count) else {
            continue;
        };
        sessions.push(analyse(&series.session_id, &series.model, &tl, &opts.policies));
    }
    let totals: Vec<Outcome> = opts
        .policies
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let mut t = Outcome::new(*p);
            for s in &sessions {
                t.add(&s.outcomes[i]);
            }
            t.finish()
        })
        .collect();
    let best = (!sessions.is_empty())
        .then(|| {
            totals
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.net.total_cmp(&b.1.net))
                .map(|(i, _)| i)
        })
        .flatten();
    let mut all = Rereads::default();
    for s in &sessions {
        all.add(&s.rereads);
    }
    TrimWhatIf {
        generated_at: now,
        days,
        weights: serde_json::json!({"cached": CACHED_WEIGHT, "uncached": 1.0, "output": OUTPUT_WEIGHT}),
        stub_tokens: STUB_TOKENS,
        rollouts_read: read,
        policies: opts.policies.clone(),
        totals,
        best,
        rereads: all,
        sessions,
        caveats: vec![CAVEAT],
    }
}

fn kilo(x: f64) -> String {
    format!("{:.0}k", x / 1000.0)
}

/// The text of the command.
pub fn render(w: &TrimWhatIf) -> String {
    use std::fmt::Write;
    let mut o = String::new();
    let _ = writeln!(
        o,
        "checkpoint trim what-if: {} long sessions of {} rollouts read, last {} days (cost-weighted tokens: cached input 0.1, uncached input 1, output 8; stub {} tokens)",
        w.sessions.len(),
        w.rollouts_read,
        w.days,
        w.stub_tokens
    );
    if w.sessions.is_empty() {
        let _ = writeln!(
            o,
            "no session with enough requests (at least {} requests and more than {} input tokens each on average)",
            crate::compact::LONG_SESSION_REQUESTS,
            crate::compact::LONG_SESSION_MEAN_INPUT
        );
        let _ = writeln!(o, "\n{CAVEAT}");
        return o;
    }
    let _ = writeln!(
        o,
        "\n{:<26} {:>10} {:>10} {:>8} {:>9} {:>9} {:>11} {:>8} {:>11}",
        "policy", "actual", "with trim", "net", "saving", "misses", "checkpoints", "trimmed", "recoveries"
    );
    for (i, t) in w.totals.iter().enumerate() {
        let mark = if w.best == Some(i) { " *" } else { "" };
        let _ = writeln!(
            o,
            "{:<26} {:>10} {:>10} {:>+7.1}% {:>9} {:>9} {:>11} {:>8} {:>5} ({:>3.0}%){}",
            t.policy.label(),
            kilo(t.actual_cost),
            kilo(t.trimmed_cost),
            t.net_pct,
            kilo(t.saving),
            kilo(t.checkpoint_cost + t.recovery_cost),
            t.checkpoints,
            t.outputs_trimmed,
            t.recoveries,
            t.recovery_share * 100.0,
            mark
        );
    }
    if let Some(b) = w.best {
        let _ = writeln!(o, "\nper session, best policy {}:", w.policies[b].label());
        let _ = writeln!(
            o,
            "{:<10} {:>8} {:>10} {:>10} {:>8} {:>11} {:>15} {:>11}",
            "session", "requests", "actual", "with trim", "net", "checkpoints", "outputs trimmed", "recoveries"
        );
        for s in &w.sessions {
            let x = &s.outcomes[b];
            let _ = writeln!(
                o,
                "{:<10} {:>8} {:>10} {:>10} {:>+7.1}% {:>11} {:>15} {:>11}",
                s.session_id.chars().take(8).collect::<String>(),
                s.requests,
                kilo(x.actual_cost),
                kilo(x.trimmed_cost),
                x.net_pct,
                x.checkpoints,
                x.outputs_trimmed,
                x.recoveries
            );
        }
        let t = &w.totals[b];
        let requests: usize = w.sessions.iter().map(|s| s.requests).sum();
        let _ = writeln!(
            o,
            "{:<10} {:>8} {:>10} {:>10} {:>+7.1}% {:>11} {:>15} {:>11}",
            "TOTAL",
            requests,
            kilo(t.actual_cost),
            kilo(t.trimmed_cost),
            t.net_pct,
            t.checkpoints,
            t.outputs_trimmed,
            t.recoveries
        );
    }
    render_rereads(&mut o, w);
    let _ = writeln!(o, "\nWarning: {CAVEAT}");
    o
}

fn render_rereads(o: &mut String, w: &TrimWhatIf) {
    use std::fmt::Write;
    let r = &w.rereads;
    let actual: f64 = w.sessions.iter().map(|s| s.actual_cost).sum();
    let share = if actual > 0.0 { r.cost / actual * 100.0 } else { 0.0 };
    let _ = writeln!(
        o,
        "\nre-reads after compaction (files read again in the first {REREAD_WINDOW} requests after a compaction that were read before it):"
    );
    let _ = writeln!(
        o,
        "{:<10} {:>11} {:>8} {:>10} {:>10} {:>7}",
        "session", "compactions", "outputs", "tokens", "cost", "share"
    );
    for s in &w.sessions {
        let x = &s.rereads;
        let pct = if s.actual_cost > 0.0 {
            x.cost / s.actual_cost * 100.0
        } else {
            0.0
        };
        let _ = writeln!(
            o,
            "{:<10} {:>11} {:>8} {:>10} {:>10} {:>6.1}%",
            s.session_id.chars().take(8).collect::<String>(),
            x.compactions,
            x.outputs,
            x.tokens,
            kilo(x.cost),
            pct
        );
    }
    let _ = writeln!(
        o,
        "{:<10} {:>11} {:>8} {:>10} {:>10} {:>6.1}%",
        "TOTAL",
        r.compactions,
        r.outputs,
        r.tokens,
        kilo(r.cost),
        share
    );
    let _ = writeln!(
        o,
        "A lower model_auto_compact_token_limit means more compactions and more of these; a note of the working set after each compaction is what could spare them."
    );
}

#[cfg(test)]
mod tests;
