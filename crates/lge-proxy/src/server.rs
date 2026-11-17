//! The daemon: one local HTTP server that is the proxy in front of the model
//! provider, the owner of every graph database, the MCP endpoint, and the page
//! that shows what was spent and saved.
//!
//! Rules of the proxy path:
//! * Fail open. If anything about rewriting a request goes wrong, the original
//!   bytes are forwarded.
//! * The response is relayed as it arrives, byte for byte; the proxy only reads
//!   the token usage out of it. The one exception: when the provider refuses a
//!   rewritten request with 400, 413 or 422, before any byte is relayed, the
//!   request is sent once more without the rewrites first made in it.
//! * Nothing of a conversation is stored except token counts, rule counts, the
//!   first typed message cut to 200 characters, and the decisions needed to
//!   repeat a rewrite (in memory).

use crate::responses::{
    background_label, compaction_request, inspect, transform, OutputFact, Seen, Usage, UsageScanner, SUBAGENT_HEADER,
    TURN_METADATA_HEADER,
};
use crate::rules::{
    CohortStats, Mode, RuleId, RuleKind, RuleModes, RuleStats, DEMOTE_ABOVE, HEALTHY_BELOW, MIN_JUDGED,
};
use crate::session::{Graph, Ledger, Options, PrefixStats, Saved, Session};
use anyhow::{anyhow, bail, Context, Result};
use lge_core::{content_hash, outcome, stable_id, Level, UsageRecord};
use lge_retrieve::{index_repo, unix_now, IndexOptions};
use lge_store::{GraphStore, Store};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tiny_http::{Header, Method, Request, Response};

const UI: &str = include_str!("ui.html");
/// How long the first request in a new repository waits for its index.
const FIRST_INDEX_WAIT: Duration = Duration::from_secs(10);
/// Largest request body the proxy reads into memory.
const MAX_BODY: usize = 128 * 1024 * 1024;
/// Most of a refused request's error body that is read before the retry.
const MAX_ERROR_BODY: u64 = 64 * 1024;
/// Statuses after which a rewritten request is sent again without the
/// rewrites first made in it: the provider refused the body itself. Never
/// 429 (rate limit) or 5xx (the provider's own trouble).
const RETRY_STATUSES: [u16; 3] = [400, 413, 422];
/// A response more than this long after the previous one of a conversation
/// may find the provider's prompt cache gone.
const COLD_GAP_SECONDS: i64 = 300;
/// A cold restart is counted only after a request of at least this many input tokens.
const COLD_MIN_INPUT: i64 = 4096;
/// The window of stored sessions the rule check, the cohorts and the holdout
/// comparison read.
const WINDOW_SECONDS: i64 = 30 * 24 * 3600;
/// The rules' recovery rates are checked once per this many recorded responses.
const CHECK_EVERY: u64 = 50;
/// Request traces kept in memory: conversations, and requests per conversation.
const TRACE_CONVERSATIONS: usize = 200;
const TRACE_REQUESTS: usize = 500;

#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    /// 0 lets the system choose (tests).
    pub port: u16,
    /// Where graph databases, the usage log and the daemon's settings live.
    pub data_dir: PathBuf,
    /// Folders the daemon may index. Empty = any folder.
    pub workspaces: Vec<PathBuf>,
    /// Provider base for API-key requests.
    pub upstream_api: String,
    /// Provider base for requests made with a ChatGPT login.
    pub upstream_chatgpt: String,
    /// The agent's home (`~/.codex`), for importing the usage of past sessions.
    pub codex_home: Option<PathBuf>,
}

impl Config {
    pub fn new(data_dir: PathBuf) -> Config {
        Config {
            host: "127.0.0.1".to_string(),
            port: 8787,
            data_dir,
            workspaces: Vec::new(),
            upstream_api: "https://api.openai.com/v1".to_string(),
            upstream_chatgpt: "https://chatgpt.com/backend-api/codex".to_string(),
            codex_home: None,
        }
    }
}

/// What the page can switch. Saved in `daemon.json`.
///
/// Every key may be missing: a file or a request without it keeps the default
/// (or, for a request, the current value), so settings saved by an older
/// version still load. `shorten_reads` is the older name of the skeleton
/// rule's switch and is read and written for compatibility: it is always
/// `rules.skeleton == on`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "SettingsPatch")]
pub struct Settings {
    pub inject_context: bool,
    pub shorten_outputs: bool,
    /// The skeleton rule is `on`. Kept for older clients.
    pub shorten_reads: bool,
    pub context_budget: usize,
    /// Tokens of the packs present in one request together at most.
    pub pack_conversation_budget: usize,
    /// Index a git repository the first time the agent works in it.
    pub auto_index: bool,
    /// The mode of each rule.
    pub rules: RuleModes,
    /// The share of new conversations in the holdout arm (every rule in
    /// shadow, the client's bytes forwarded), from 0 to 1.
    pub holdout_share: f64,
    /// Rules moved to shadow by the recovery check: rule id -> why.
    pub demoted: BTreeMap<String, String>,
    /// Rule id -> the unix time from which its recovery rate is counted (set
    /// when a demotion is cleared).
    pub rule_since: BTreeMap<String, i64>,
    /// Which of the router's recommendations lower a conversation's reasoning
    /// effort. Off by default: the effort stays what the client asked for.
    pub router_apply: RouterApply,
}

/// Which router recommendations are applied to the reasoning effort.
///
/// An applied recommendation only ever lowers the effort the client asked for,
/// never raises it, and is decided once, on a conversation's first request, so
/// every request of the conversation has the same effort. Held-out and
/// background conversations are never changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RouterApply {
    /// Log the recommendation only.
    #[default]
    Off,
    /// Apply `low` (a change in one file); keep the effort otherwise.
    LowOnly,
    /// Apply `low` and `medium` (several files in one service).
    LowAndMedium,
}

impl RouterApply {
    pub fn allows(self, level: Level) -> bool {
        match self {
            RouterApply::Off => false,
            RouterApply::LowOnly => level == Level::Low,
            RouterApply::LowAndMedium => level != Level::Top,
        }
    }
}

/// The reasoning effort a router level stands for; `None` keeps the client's.
fn effort_of(level: Level) -> Option<&'static str> {
    match level {
        Level::Low => Some("low"),
        Level::Medium => Some("medium"),
        Level::Top => None,
    }
}

fn effort_rank(effort: &str) -> Option<u8> {
    match effort {
        "none" => Some(0),
        "minimal" => Some(1),
        "low" => Some(2),
        "medium" => Some(3),
        "high" => Some(4),
        "xhigh" => Some(5),
        _ => None,
    }
}

/// Lower the request's `reasoning.effort` to `level`'s. Never raises it, and
/// leaves a request alone that names no effort, or one this version does not
/// know. Returns the effort before and after when it changed.
pub(crate) fn lower_effort(value: &mut Value, level: Level) -> Option<(String, String)> {
    let target = effort_of(level)?;
    let effort = value.get_mut("reasoning")?.get_mut("effort")?;
    let asked = effort.as_str()?.to_string();
    if effort_rank(target)? >= effort_rank(&asked)? {
        return None;
    }
    *effort = Value::String(target.to_string());
    Some((asked, target.to_string()))
}

impl Default for Settings {
    fn default() -> Self {
        let o = Options::default();
        Settings {
            inject_context: o.inject_context,
            shorten_outputs: o.shorten_outputs,
            shorten_reads: o.rules.skeleton == Mode::On,
            context_budget: o.context_budget,
            pack_conversation_budget: o.pack_conversation_budget,
            auto_index: true,
            rules: o.rules,
            holdout_share: 0.0,
            demoted: BTreeMap::new(),
            rule_since: BTreeMap::new(),
            router_apply: RouterApply::Off,
        }
    }
}

/// Settings as written in a file or a request: every key optional.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SettingsPatch {
    inject_context: Option<bool>,
    shorten_outputs: Option<bool>,
    shorten_reads: Option<bool>,
    context_budget: Option<usize>,
    pack_conversation_budget: Option<usize>,
    auto_index: Option<bool>,
    rules: Option<serde_json::Map<String, Value>>,
    holdout_share: Option<f64>,
    demoted: Option<BTreeMap<String, String>>,
    rule_since: Option<BTreeMap<String, i64>>,
    router_apply: Option<RouterApply>,
    /// Request only: lift the demotion of this rule.
    clear_demotion: Option<String>,
}

impl SettingsPatch {
    /// The rule modes the patch names. A rule id this version does not know
    /// is ignored; a mode it does not know is an error.
    fn modes(&self) -> Result<Vec<(RuleId, Mode)>> {
        let mut out = Vec::new();
        for (id, value) in self.rules.iter().flatten() {
            let Some(rule) = RuleId::parse(id) else { continue };
            let mode: Mode = serde_json::from_value(value.clone())
                .map_err(|_| anyhow!("rules.{id}: expected \"off\", \"shadow\" or \"on\", got {value}"))?;
            out.push((rule, mode));
        }
        Ok(out)
    }
}

impl TryFrom<SettingsPatch> for Settings {
    type Error = anyhow::Error;

    /// Settings read from `daemon.json`: missing keys take their defaults. An
    /// explicit `rules.skeleton` wins over `shorten_reads`; without it,
    /// `shorten_reads` true sets the skeleton rule to `on`, false to `shadow`.
    fn try_from(p: SettingsPatch) -> Result<Settings> {
        let mut s = Settings::default();
        let modes = p.modes()?;
        if let Some(reads) = p.shorten_reads {
            s.rules.skeleton = if reads { Mode::On } else { Mode::Shadow };
        }
        for (rule, mode) in modes {
            s.rules.set(rule, mode);
        }
        s.inject_context = p.inject_context.unwrap_or(s.inject_context);
        s.shorten_outputs = p.shorten_outputs.unwrap_or(s.shorten_outputs);
        s.context_budget = p.context_budget.unwrap_or(s.context_budget);
        s.pack_conversation_budget = p.pack_conversation_budget.unwrap_or(s.pack_conversation_budget);
        s.auto_index = p.auto_index.unwrap_or(s.auto_index);
        s.holdout_share = valid_share(p.holdout_share.unwrap_or(0.0));
        s.demoted = p.demoted.unwrap_or_default();
        s.rule_since = p.rule_since.unwrap_or_default();
        s.router_apply = p.router_apply.unwrap_or_default();
        Ok(s.synced())
    }
}

/// The router level applied to a proxy conversation's reasoning effort, or ""
/// when the client's effort was kept.
fn applied_level(r: &UsageRecord) -> &str {
    match r.level.as_str() {
        "low" | "medium" if r.mode == "proxy" => &r.level,
        _ => "",
    }
}

/// A share outside 0 to 1, or not a number, is 0: nobody is held out.
fn valid_share(share: f64) -> f64 {
    if (0.0..=1.0).contains(&share) {
        share
    } else {
        0.0
    }
}

impl Settings {
    fn synced(mut self) -> Settings {
        self.shorten_reads = self.rules.skeleton == Mode::On;
        self
    }

    /// Apply a request from the page or a client onto these settings. Only
    /// the keys it has change. The skeleton switch changes through whichever
    /// of `rules.skeleton` and `shorten_reads` differs from its current value,
    /// so a client that knows only one of them and sends the other back as it
    /// read it still works. Demotions and their dates are the daemon's; a
    /// request lifts one with `clear_demotion` and changes them no other way.
    fn apply(&mut self, p: SettingsPatch, now: i64) -> Result<()> {
        let modes = p.modes()?;
        let skeleton = modes.iter().find(|(r, _)| *r == RuleId::Skeleton).map(|(_, m)| *m);
        for (rule, mode) in &modes {
            if *rule != RuleId::Skeleton {
                self.rules.set(*rule, *mode);
            }
        }
        match (skeleton, p.shorten_reads) {
            (Some(mode), _) if mode != self.rules.skeleton => self.rules.skeleton = mode,
            (_, Some(reads)) if reads != (self.rules.skeleton == Mode::On) => {
                self.rules.skeleton = if reads { Mode::On } else { Mode::Shadow };
            }
            _ => {}
        }
        if let Some(v) = p.inject_context {
            self.inject_context = v;
        }
        if let Some(v) = p.shorten_outputs {
            self.shorten_outputs = v;
        }
        if let Some(v) = p.context_budget {
            self.context_budget = v;
        }
        if let Some(v) = p.pack_conversation_budget {
            self.pack_conversation_budget = v;
        }
        if let Some(v) = p.auto_index {
            self.auto_index = v;
        }
        if let Some(v) = p.holdout_share {
            self.holdout_share = valid_share(v);
        }
        if let Some(v) = p.router_apply {
            self.router_apply = v;
        }
        if let Some(id) = p.clear_demotion {
            let rule = RuleId::parse(&id).ok_or_else(|| anyhow!("no rule `{id}`"))?;
            self.demoted.remove(rule.as_str());
            self.rule_since.insert(rule.as_str().to_string(), now);
        }
        *self = self.clone().synced();
        Ok(())
    }

    /// The mode a rule runs in for new items: a demoted rule set to `on` runs
    /// in `shadow`.
    fn effective_mode(&self, rule: RuleId) -> Mode {
        match self.rules.get(rule) {
            Mode::On if self.demoted.contains_key(rule.as_str()) => Mode::Shadow,
            mode => mode,
        }
    }

    fn options(&self) -> Options {
        let mut rules = self.rules.clone();
        for rule in RuleId::ALL {
            rules.set(rule, self.effective_mode(rule));
        }
        Options {
            inject_context: self.inject_context,
            shorten_outputs: self.shorten_outputs,
            rules,
            context_budget: self.context_budget,
            pack_conversation_budget: self.pack_conversation_budget,
            ..Options::default()
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct SavedState {
    #[serde(default)]
    repos: Vec<String>,
    #[serde(default)]
    settings: Option<Settings>,
}

/// Read `daemon.json`. A missing file gives the defaults. A file that exists
/// but cannot be read or parsed also gives the default settings (the
/// repository list is kept when it can still be read), is copied to
/// `daemon.json.unreadable` so nothing is lost when it is next saved, and
/// gives the error to show.
fn load_saved(data_dir: &Path) -> (SavedState, Option<String>) {
    let path = data_dir.join("daemon.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (SavedState::default(), None),
        Err(e) => {
            return (
                SavedState::default(),
                Some(format!("{} could not be read: {e}", path.display())),
            )
        }
    };
    match serde_json::from_str::<SavedState>(&text) {
        Ok(saved) => (saved, None),
        Err(e) => {
            let repos = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| serde_json::from_value::<Vec<String>>(v.get("repos")?.clone()).ok())
                .unwrap_or_default();
            let backup = data_dir.join("daemon.json.unreadable");
            let kept = std::fs::write(&backup, &text).is_ok();
            let error = format!(
                "{} could not be read ({e}); the default settings are in use until they are saved from this page{}",
                path.display(),
                if kept {
                    format!(". The file was copied to {}", backup.display())
                } else {
                    String::new()
                }
            );
            (SavedState { repos, settings: None }, Some(error))
        }
    }
}

enum RepoState {
    Indexing,
    Ready(Box<lge_mcp::Server>),
    Failed(String),
}

struct Repo {
    root: PathBuf,
    state: Mutex<RepoState>,
    /// Folders searches would wade through, found when the repository is
    /// indexed (not on every status call).
    bulky: Mutex<Vec<lge_index::BulkyDir>>,
}

/// What each rule did in a session, as stored in `UsageRecord.rule_stats`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct RuleTotals {
    by_rule: BTreeMap<String, RuleStats>,
    cohorts: BTreeMap<String, CohortStats>,
    condensed_upstream: u64,
}

impl RuleTotals {
    fn plus(&self, saved: &Saved) -> RuleTotals {
        let mut out = self.clone();
        for (id, stats) in &saved.by_rule {
            let slot = out.by_rule.entry(id.clone()).or_default();
            *slot = slot.plus(stats);
        }
        for (key, stats) in &saved.cohorts {
            let slot = out.cohorts.entry(key.clone()).or_default();
            *slot = slot.plus(stats);
        }
        out.condensed_upstream += saved.condensed_upstream as u64;
        out
    }
}

/// The cache monitor of a session, as stored in `UsageRecord.monitor`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct MonitorJson {
    prefix: PrefixStats,
    /// Requests sent again without their new rewrites after a refusal.
    raw_retries: u64,
    /// Responses after an idle gap whose cached tokens fell below half of the
    /// previous request's input, and the input tokens that were not cached
    /// because of it.
    cold_restarts: u64,
    cold_tokens: i64,
    /// Packs a holdout conversation would have added.
    contexts_would_add: u64,
    /// Model requests of the session.
    requests: u64,
    /// Each item's decision by hash, so a restarted daemon sends the same
    /// bytes for it and does not count it again.
    ledger: Ledger,
    /// The label of a session of Codex's background requests (`guardian`,
    /// `title`, `recap`, ...), fixed by its first request.
    background: Option<String>,
    /// Background requests by label, in this session.
    background_requests: BTreeMap<String, u64>,
    /// Packs added; follow-ups skipped as already known; packs skipped for
    /// the conversation budget; packs retired after a compaction.
    packs_added: u64,
    packs_known: u64,
    packs_over_budget: u64,
    packs_retired: u64,
}

/// What a conversation's stored record had counted before this daemon
/// started: added to what the new session counts, so a restart continues the
/// counters instead of starting them from zero.
#[derive(Debug, Clone, Default)]
struct Carried {
    rules: RuleTotals,
    monitor: MonitorJson,
    pack_tokens: i64,
}

/// One tool definition of a request, for the fixed-prefix report.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ToolDef {
    /// As Codex names the tool in a call: its namespace, if any, then its name.
    name: String,
    group: String,
    /// The group is an MCP server (`mcp__` names).
    mcp: bool,
    tokens: usize,
}

/// The fixed part of a conversation's requests, counted once per distinct
/// instructions and tool list.
#[derive(Debug, Clone, Default)]
struct FixedPrefix {
    instructions_hash: String,
    instructions_tokens: usize,
    tools_hash: String,
    tools: Vec<ToolDef>,
}

/// A tool seen in the definitions since the daemon started.
#[derive(Debug, Clone, Default)]
struct ToolSeen {
    group: String,
    mcp: bool,
    tokens: usize,
    /// Requests whose definitions included it.
    requests_seen: u64,
    /// When a request last showed a call of it.
    last_called: Option<i64>,
}

/// One request of a conversation, kept in memory for `lge ab`.
#[derive(Debug, Clone, Default, Serialize)]
struct Trace {
    request: u64,
    /// The provider's status for the request as relayed.
    status: u16,
    /// Sent a second time without its new rewrites.
    retried: bool,
    /// The usage was found in the response.
    usage_found: bool,
    input_tokens: i64,
    cached_input_tokens: i64,
    output_tokens: i64,
    reasoning_tokens: i64,
    /// Tokens lge added to (its packs) and removed from (replaced outputs) this request.
    lge_added_tokens: i64,
    lge_removed_tokens: i64,
    /// The largest tool outputs new in this request.
    largest_outputs: Vec<OutputFact>,
}

/// The last requests of the last conversations.
#[derive(Default)]
struct Traces {
    order: VecDeque<String>,
    by_conv: HashMap<String, VecDeque<Trace>>,
}

impl Traces {
    fn push(&mut self, key: &str, trace: Trace) {
        if !self.by_conv.contains_key(key) {
            if self.order.len() >= TRACE_CONVERSATIONS {
                if let Some(old) = self.order.pop_front() {
                    self.by_conv.remove(&old);
                }
            }
            self.order.push_back(key.to_string());
        }
        let list = self.by_conv.entry(key.to_string()).or_default();
        if list.len() >= TRACE_REQUESTS {
            list.pop_front();
        }
        list.push_back(trace);
    }
}

/// One conversation as the proxy sees it.
struct Conv {
    key: String,
    session: Session,
    record: UsageRecord,
    repo: Option<PathBuf>,
    requests: u64,
    routed: bool,
    /// In the holdout arm: fixed when the conversation is first seen.
    holdout: bool,
    /// A conversation of Codex's background requests, by label: fixed when
    /// the conversation is first seen.
    background: Option<String>,
    /// Background requests by label.
    background_requests: BTreeMap<String, u64>,
    carried: Carried,
    raw_retries: u64,
    cold_restarts: u64,
    cold_tokens: i64,
    last_response_at: Option<i64>,
    last_input_tokens: i64,
    fixed: FixedPrefix,
    /// What the latest rewrite added to the record's annuity counters, so a
    /// retry of the same request can take it back.
    last_sent: (i64, i64),
}

impl Conv {
    fn monitor_json(&self) -> MonitorJson {
        let base = &self.carried.monitor;
        let prefix = &self.session.prefix;
        let saved = &self.session.saved;
        MonitorJson {
            prefix: PrefixStats {
                lge_breaks: base.prefix.lge_breaks + prefix.lge_breaks,
                client_breaks: base.prefix.client_breaks + prefix.client_breaks,
                first_lge_break_item: base.prefix.first_lge_break_item.or(prefix.first_lge_break_item),
            },
            raw_retries: self.raw_retries,
            cold_restarts: self.cold_restarts,
            cold_tokens: self.cold_tokens,
            contexts_would_add: base.contexts_would_add + self.session.saved.contexts_would_add as u64,
            requests: self.requests,
            ledger: self.session.ledger().clone(),
            background: self.background.clone(),
            background_requests: self.background_requests.clone(),
            packs_added: base.packs_added + saved.contexts_added as u64,
            packs_known: base.packs_known + saved.contexts_known as u64,
            packs_over_budget: base.packs_over_budget + saved.contexts_over_budget as u64,
            packs_retired: base.packs_retired + saved.contexts_retired as u64,
        }
    }

    /// Write the session's counts into the record.
    fn sync_record(&mut self) {
        self.record.pack_tokens = self.carried.pack_tokens + self.session.saved.context_tokens as i64;
        let rules = self.carried.rules.plus(&self.session.saved);
        self.record.rule_stats = serde_json::to_string(&rules).unwrap_or_default();
        self.record.monitor = serde_json::to_string(&self.monitor_json()).unwrap_or_default();
    }

    /// Count a cold restart: the previous response was more than
    /// [`COLD_GAP_SECONDS`] ago, its request had at least [`COLD_MIN_INPUT`]
    /// input tokens, and this response read less than half of that from the
    /// cache. The tokens counted are the previous input less what was cached.
    fn observe_response(&mut self, now: i64, usage: &Usage) {
        if let Some(at) = self.last_response_at {
            let previous = self.last_input_tokens;
            if now - at > COLD_GAP_SECONDS && previous >= COLD_MIN_INPUT && usage.cached_input_tokens * 2 < previous {
                self.cold_restarts += 1;
                self.cold_tokens += (previous - usage.cached_input_tokens).max(0);
            }
        }
        self.last_response_at = Some(now);
        self.last_input_tokens = usage.input_tokens;
    }
}

/// The result of rewriting one request.
struct Rewritten {
    /// What to forward.
    body: Vec<u8>,
    /// It differs from what the client sent (beyond decoding).
    changed: bool,
    /// Items first decided in this request that changed what is sent.
    new_changes: usize,
    /// The label of a background request of Codex, for the log line.
    background: Option<String>,
    /// The conversation's reasoning effort was first lowered in this request.
    effort_decided: bool,
    conv: Arc<Mutex<Conv>>,
    /// The trace of this request, completed with the response's numbers. It
    /// travels with the request: Codex may send the next request of the same
    /// conversation before this response has been fully relayed.
    trace: Trace,
}

pub struct Daemon {
    pub cfg: Config,
    agent: ureq::Agent,
    usage: Store,
    repos: Mutex<BTreeMap<PathBuf, Arc<Repo>>>,
    convs: Mutex<HashMap<String, Arc<Mutex<Conv>>>>,
    settings: Mutex<Settings>,
    /// Why `daemon.json` could not be read, until the settings are saved.
    settings_error: Mutex<Option<String>>,
    last_repo: Mutex<Option<PathBuf>>,
    /// Tool name -> what was seen of it since start, for the fixed-prefix report.
    tools: Mutex<BTreeMap<String, ToolSeen>>,
    /// Tokens of the instructions of the latest request.
    instructions_tokens: AtomicU64,
    traces: Mutex<Traces>,
    /// Responses recorded since start, for the periodic rule check.
    recorded: AtomicU64,
    /// Seconds added to the system clock (tests).
    clock_offset: AtomicI64,
    started: Instant,
}

/// A poisoned lock means another request panicked; its data is still the best there is.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn header(name: &str, value: &str) -> Option<Header> {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).ok()
}

fn respond_json(request: Request, status: u16, body: &Value) {
    let mut response = Response::from_string(body.to_string()).with_status_code(status);
    if let Some(h) = header("Content-Type", "application/json") {
        response = response.with_header(h);
    }
    let _ = request.respond(response);
}

/// Headers that belong to one hop and are never forwarded.
fn hop_by_hop(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "host"
            | "content-length"
            | "expect"
    )
}

/// `/v1/responses?x=1` -> (`/v1/responses`, `x=1`).
fn split_url(url: &str) -> (&str, &str) {
    url.split_once('?').unwrap_or((url, ""))
}

fn query_value(query: &str, key: &str) -> Option<String> {
    query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| percent_decode(v))
}

fn percent_decode(s: &str) -> String {
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match (
            bytes[i],
            bytes.get(i + 1).copied().and_then(hex),
            bytes.get(i + 2).copied().and_then(hex),
        ) {
            (b'%', Some(high), Some(low)) => {
                out.push(high * 16 + low);
                i += 3;
            }
            (b'+', _, _) => {
                out.push(b' ');
                i += 1;
            }
            (b, _, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// The conversation of `/lge/api/sessions/<id>/requests`.
fn trace_id(path: &str) -> Option<&str> {
    path.strip_prefix("/lge/api/sessions/")?
        .strip_suffix("/requests")
        .filter(|id| !id.is_empty() && !id.contains('/'))
}

/// 0 = the repository is one service; 1 = each top-level folder that has its
/// own manifest is a service (a monorepo).
fn guess_service_depth(root: &Path) -> usize {
    const MANIFESTS: [&str; 6] = [
        "package.json",
        "pyproject.toml",
        "requirements.txt",
        "setup.py",
        "Cargo.toml",
        "go.mod",
    ];
    let with_manifest = std::fs::read_dir(root)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().is_dir() && MANIFESTS.iter().any(|m| e.path().join(m).exists()))
                .count()
        })
        .unwrap_or(0);
    usize::from(with_manifest >= 2)
}

/// Where a conversation falls between 0 and 1, from the first 16 hex
/// characters of the content hash of `key`. The same key always falls in the
/// same place.
fn holdout_bucket(key: &str) -> f64 {
    let hash = content_hash(key.as_bytes());
    let n = u64::from_str_radix(&hash[..16], 16).unwrap_or(0);
    n as f64 / 18_446_744_073_709_551_616.0
}

/// True when the conversation keyed `key` is in the holdout arm.
fn in_holdout(share: f64, key: &str) -> bool {
    share >= 1.0 || (share > 0.0 && holdout_bucket(key) < share)
}

/// `YYYY-MM-DD` (UTC) of a unix time.
fn date_of(unix: i64) -> String {
    let z = unix.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Codex's namespace for its own tools (codex-rs/protocol/src/tool_name.rs).
const CODEX_NAMESPACE: &str = "functions";

/// The group of a tool namespace: Codex's own (`functions`) is `built-in`;
/// an MCP server's (`mcp__server__`, or `mcp__server` in newer versions;
/// codex-rs/codex-mcp/src/tools.rs) is the server's name; any other namespace
/// is its own name.
fn namespace_group(namespace: &str) -> (String, bool) {
    if namespace.is_empty() || namespace == CODEX_NAMESPACE {
        return ("built-in".to_string(), false);
    }
    match namespace.strip_prefix("mcp__") {
        Some(rest) => (rest.trim_end_matches("__").to_string(), true),
        None => (namespace.trim_end_matches("__").to_string(), false),
    }
}

/// The group of a tool defined without a namespace: a flat MCP name
/// (`mcp__server__tool`, Codex's older form) is grouped by the part before
/// the last `__`, without `mcp__`; any other name is `built-in`.
fn tool_group(name: &str) -> (String, bool) {
    match name.rsplit_once("__") {
        Some((head, _)) if !head.is_empty() => match head.strip_prefix("mcp__") {
            Some(server) => (server.to_string(), true),
            None if head == "mcp" => ("mcp".to_string(), true),
            None => (head.to_string(), false),
        },
        _ => ("built-in".to_string(), false),
    }
}

/// The tool definitions of a request: its `tools` field and the `tools` of
/// any `additional_tools` input item (where Codex puts them in its lite
/// request form; codex-rs/core/src/client.rs). A namespace entry is split into
/// its tools, each named as a call names it (`namespace` + `name`). Each tool
/// is counted by the tokens of its own definition.
fn tool_defs(body: &Value) -> Vec<ToolDef> {
    let mut entries: Vec<&Value> = body
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .collect();
    for item in body.get("input").and_then(Value::as_array).into_iter().flatten() {
        if item.get("type").and_then(Value::as_str) == Some("additional_tools") {
            entries.extend(item.get("tools").and_then(Value::as_array).into_iter().flatten());
        }
    }
    let name_of = |v: &Value| -> String {
        v.get("name")
            .and_then(Value::as_str)
            .or_else(|| v.get("type").and_then(Value::as_str))
            .unwrap_or("")
            .to_string()
    };
    let tokens = |v: &Value| lge_retrieve::budget::count(&v.to_string());
    let mut out = Vec::new();
    for entry in entries {
        if entry.get("type").and_then(Value::as_str) == Some("namespace") {
            let namespace = entry.get("name").and_then(Value::as_str).unwrap_or("");
            let (group, mcp) = namespace_group(namespace);
            for tool in entry.get("tools").and_then(Value::as_array).into_iter().flatten() {
                let name = name_of(tool);
                out.push(ToolDef {
                    name: if namespace == CODEX_NAMESPACE {
                        name
                    } else {
                        format!("{namespace}{name}")
                    },
                    group: group.clone(),
                    mcp,
                    tokens: tokens(tool),
                });
            }
        } else {
            let name = name_of(entry);
            let (group, mcp) = tool_group(&name);
            out.push(ToolDef {
                name,
                group,
                mcp,
                tokens: tokens(entry),
            });
        }
    }
    out
}

/// The content hash of a request's tool definitions, to count them only when
/// they change; [`tool_defs`] reads them.
fn tools_hash(body: &Value) -> String {
    let mut bytes = body.get("tools").map(Value::to_string).unwrap_or_default().into_bytes();
    for item in body.get("input").and_then(Value::as_array).into_iter().flatten() {
        if item.get("type").and_then(Value::as_str) == Some("additional_tools") {
            bytes.extend_from_slice(item.get("tools").map(Value::to_string).unwrap_or_default().as_bytes());
        }
    }
    content_hash(&bytes)
}

/// The request's base instructions: its `instructions` field, or, as newer
/// Codex versions send them, the developer message that opens `input` (after
/// any `additional_tools` item; codex-rs/core/src/client.rs).
fn instructions_text(body: &Value) -> String {
    if let Some(text) = body.get("instructions").and_then(Value::as_str) {
        return text.to_string();
    }
    let first = body.get("input").and_then(Value::as_array).and_then(|items| {
        items
            .iter()
            .find(|i| i.get("type").and_then(Value::as_str) != Some("additional_tools"))
    });
    match first {
        Some(item)
            if item.get("type").and_then(Value::as_str) == Some("message")
                && item.get("role").and_then(Value::as_str) == Some("developer") =>
        {
            item.get("content").map(Value::to_string).unwrap_or_default()
        }
        _ => String::new(),
    }
}

/// What a request's headers say about where it comes from, for
/// [`background_label`].
#[derive(Debug, Clone, Default)]
struct Origin {
    /// The `x-openai-subagent` header.
    subagent: Option<String>,
    /// The `x-codex-turn-metadata` header.
    turn_metadata: Option<String>,
}

/// The label under which requests for a compaction's summary are counted.
const COMPACTION_LABEL: &str = "compaction";

/// The record mode of a conversation of Codex's background requests (an
/// approval review, a review, a title, a recap, a memory consolidation): its
/// rules apply as usual, it gets no pack, it is never held out, and it is
/// left out of the holdout comparison and the before-and-after numbers.
const BACKGROUND_MODE: &str = "proxy-background";

/// A proxy conversation's record (`proxy`, `proxy-holdout` or `proxy-background`).
fn is_proxy_mode(mode: &str) -> bool {
    mode == "proxy" || mode == "proxy-holdout" || mode == BACKGROUND_MODE
}

/// A session whose requests re-send this many input tokens on average, over at
/// least [`LONG_SESSION_REQUESTS`] requests, is named in the page's note.
const LONG_SESSION_MEAN_INPUT: i64 = 150_000;
const LONG_SESSION_REQUESTS: u64 = 50;

/// What each rule did over `records`, counting a record for a rule only when
/// it started at or after the rule's `since` time. Also gives, per rule, the
/// start of the earliest record that judged an item of it.
fn rule_totals(
    records: &[UsageRecord],
    since: &BTreeMap<String, i64>,
) -> (BTreeMap<String, RuleStats>, BTreeMap<String, i64>) {
    let mut totals: BTreeMap<String, RuleStats> = BTreeMap::new();
    let mut earliest: BTreeMap<String, i64> = BTreeMap::new();
    for r in records.iter().filter(|r| is_proxy_mode(&r.mode)) {
        let Ok(stats) = serde_json::from_str::<RuleTotals>(&r.rule_stats) else {
            continue;
        };
        for (id, s) in stats.by_rule {
            if r.started_at < since.get(&id).copied().unwrap_or(i64::MIN) {
                continue;
            }
            if s.judged > 0 {
                let e = earliest.entry(id.clone()).or_insert(r.started_at);
                *e = (*e).min(r.started_at);
            }
            let slot = totals.entry(id).or_default();
            *slot = slot.plus(&s);
        }
    }
    (totals, earliest)
}

/// The skeleton cohorts over `records`.
fn cohort_totals(records: &[UsageRecord]) -> BTreeMap<String, CohortStats> {
    let mut out: BTreeMap<String, CohortStats> = BTreeMap::new();
    for r in records.iter().filter(|r| is_proxy_mode(&r.mode)) {
        if let Ok(stats) = serde_json::from_str::<RuleTotals>(&r.rule_stats) {
            for (key, c) in stats.cohorts {
                let slot = out.entry(key).or_default();
                *slot = slot.plus(&c);
            }
        }
    }
    out
}

/// A judged rule's verdict: `healthy` below [`HEALTHY_BELOW`], demotion due
/// above [`DEMOTE_ABOVE`], once at least [`MIN_JUDGED`] items were judged.
fn rule_rate(stats: &RuleStats) -> (Option<f64>, bool, bool) {
    let rate = stats.recovery_rate();
    let judged = stats.judged >= MIN_JUDGED;
    let healthy = judged && rate.is_some_and(|r| r < HEALTHY_BELOW);
    let demote = judged && rate.is_some_and(|r| r > DEMOTE_ABOVE);
    (rate, healthy, demote)
}

/// The rules to demote now: in `on`, not demoted yet, and over the
/// threshold. Each with the reason shown on the page.
fn due_demotions(settings: &Settings, records: &[UsageRecord]) -> Vec<(String, String)> {
    let (totals, earliest) = rule_totals(records, &settings.rule_since);
    let mut out = Vec::new();
    for rule in RuleId::ALL {
        let id = rule.as_str();
        if settings.rules.get(rule) != Mode::On || settings.demoted.contains_key(id) {
            continue;
        }
        let Some(stats) = totals.get(id) else { continue };
        let (rate, _, demote) = rule_rate(stats);
        if !demote {
            continue;
        }
        let since = earliest.get(id).copied().unwrap_or_default();
        out.push((
            id.to_string(),
            format!(
                "recovery {:.1}% over {} items since {}",
                rate.unwrap_or(0.0) * 100.0,
                stats.judged,
                date_of(since)
            ),
        ));
    }
    out
}

/// Input tokens per model request of a session; 0 before its first request.
fn mean_input(r: &UsageRecord, requests: u64) -> i64 {
    if requests == 0 {
        0
    } else {
        r.input_tokens / requests as i64
    }
}

/// Codex's background requests by label: the sessions that were background
/// sessions (their tokens included), and every background request, in such
/// sessions or in others.
fn background_totals(records: &[UsageRecord]) -> Vec<Value> {
    // label -> (sessions, requests, input tokens)
    let mut by_label: BTreeMap<String, (u64, u64, i64)> = BTreeMap::new();
    for r in records.iter().filter(|r| is_proxy_mode(&r.mode)) {
        let m: MonitorJson = serde_json::from_str(&r.monitor).unwrap_or_default();
        if let (Some(label), true) = (&m.background, r.mode == BACKGROUND_MODE) {
            let slot = by_label.entry(label.clone()).or_default();
            slot.0 += 1;
            slot.2 += r.input_tokens;
        }
        for (label, n) in &m.background_requests {
            by_label.entry(label.clone()).or_default().1 += n;
        }
    }
    by_label
        .into_iter()
        .map(|(label, (sessions, requests, input))| {
            json!({ "label": label, "sessions": sessions, "requests": requests, "input_tokens": input })
        })
        .collect()
}

/// The rows of the page's session table (`rows`, as the status gives them)
/// that are not background sessions and re-send more than
/// [`LONG_SESSION_MEAN_INPUT`] input tokens per request on average over at
/// least [`LONG_SESSION_REQUESTS`] requests, with the note the page shows
/// under the table. Read from the rows, so the note and the table give the
/// same counts. A report only: lge never changes Codex's configuration.
fn long_sessions(rows: &[Value]) -> Value {
    let sessions: Vec<Value> = rows
        .iter()
        .filter(|row| row["mode"] != BACKGROUND_MODE)
        .filter_map(|row| {
            let requests = row["requests"].as_u64().unwrap_or(0);
            let mean = row["mean_input_tokens"].as_i64().unwrap_or(0);
            (requests >= LONG_SESSION_REQUESTS && mean > LONG_SESSION_MEAN_INPUT).then(
                || json!({ "id": row["id"], "task": row["task"], "requests": requests, "mean_input_tokens": mean }),
            )
        })
        .collect();
    // Plain text: the page shows it as text.
    let note = (!sessions.is_empty()).then(|| {
        "Every request of these sessions re-sends that many input tokens. A new session per task, or a lower \
         model_auto_compact_token_limit in ~/.codex/config.toml (Codex compacts sooner), makes each request \
         smaller, at the cost of Codex working from a summary after a compaction."
            .to_string()
    });
    json!({
        "min_requests": LONG_SESSION_REQUESTS,
        "min_mean_input_tokens": LONG_SESSION_MEAN_INPUT,
        "sessions": sessions,
        "note": note,
    })
}

/// The type and code of a provider's error body, for the log. Nothing else of
/// the body is kept.
fn error_kind(body: &[u8]) -> String {
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let field = |k: &str| {
        v.get("error")
            .and_then(|e| e.get(k))
            .and_then(Value::as_str)
            .unwrap_or("-")
            .to_string()
    };
    format!("type {}, code {}", field("type"), field("code"))
}

impl Daemon {
    /// Open the data directory, the usage log and the saved repositories.
    pub fn open(cfg: Config) -> Result<Arc<Daemon>> {
        std::fs::create_dir_all(&cfg.data_dir).with_context(|| format!("cannot create {}", cfg.data_dir.display()))?;
        let usage = Store::open(&cfg.data_dir.join("usage.lbug"))
            .context("cannot open the usage log (is another lge daemon running?)")?;
        let (saved, settings_error) = load_saved(&cfg.data_dir);
        if let Some(e) = &settings_error {
            eprintln!("lge: warning: {e}");
        }
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .http_status_as_error(false)
                .max_redirects(0)
                .timeout_connect(Some(Duration::from_secs(20)))
                .build(),
        );
        let daemon = Arc::new(Daemon {
            cfg,
            agent,
            usage,
            repos: Mutex::new(BTreeMap::new()),
            convs: Mutex::new(HashMap::new()),
            settings: Mutex::new(saved.settings.unwrap_or_default()),
            settings_error: Mutex::new(settings_error),
            last_repo: Mutex::new(None),
            tools: Mutex::new(BTreeMap::new()),
            instructions_tokens: AtomicU64::new(0),
            traces: Mutex::new(Traces::default()),
            recorded: AtomicU64::new(0),
            clock_offset: AtomicI64::new(0),
            started: Instant::now(),
        });
        for path in saved.repos {
            // A folder that went away is dropped from the list, not an error.
            let _ = daemon.add_repo(Path::new(&path));
        }
        Ok(daemon)
    }

    /// Unix seconds, as this daemon counts them.
    pub fn now(&self) -> i64 {
        unix_now() + self.clock_offset.load(Ordering::Relaxed)
    }

    /// Move this daemon's clock forward by `seconds`: for tests and
    /// simulations of idle gaps. Nothing else uses it.
    #[doc(hidden)]
    pub fn advance_clock(&self, seconds: i64) {
        self.clock_offset.fetch_add(seconds, Ordering::Relaxed);
    }

    fn save(&self) {
        let repos: Vec<String> = lock(&self.repos)
            .keys()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        let file = self.cfg.data_dir.join("daemon.json");
        let text = if lock(&self.settings_error).is_some() {
            // The file's settings could not be read: they stay as written, so the
            // error stays until settings are saved from the page. Only the list of
            // folders is updated, and only when the file is JSON at all.
            match std::fs::read_to_string(&file)
                .ok()
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            {
                Some(Value::Object(mut fields)) => {
                    fields.insert("repos".to_string(), json!(repos));
                    serde_json::to_string_pretty(&Value::Object(fields)).ok()
                }
                _ => None,
            }
        } else {
            serde_json::to_string_pretty(&SavedState {
                repos,
                settings: Some(lock(&self.settings).clone()),
            })
            .ok()
        };
        if let Some(text) = text {
            let _ = std::fs::write(file, text);
        }
    }

    /// Whether `host` (a `Host` header, or the host and port of an `Origin`)
    /// names this daemon: a loopback name, or the host it was told to listen
    /// on, with any port. A page served from any other name (another site, or
    /// one whose name resolves to this machine) is refused.
    fn own_host(&self, host: &str) -> bool {
        let name = match host.strip_prefix('[') {
            Some(rest) => match rest.split_once(']') {
                Some((inside, after)) if after.is_empty() || port_only(after) => format!("[{inside}]"),
                _ => return false,
            },
            None => match host.split_once(':') {
                Some((name, port)) if port.chars().all(|c| c.is_ascii_digit()) && !port.is_empty() => name.to_string(),
                Some(_) => return false,
                None => host.to_string(),
            },
        };
        let name = name.to_ascii_lowercase();
        let configured = self.cfg.host.to_ascii_lowercase();
        matches!(name.as_str(), "127.0.0.1" | "localhost" | "[::1]")
            || (!matches!(configured.as_str(), "0.0.0.0" | "::" | "[::]") && name == configured)
    }

    fn allowed(&self, path: &Path) -> bool {
        self.cfg.workspaces.is_empty() || self.cfg.workspaces.iter().any(|w| path.starts_with(w))
    }

    /// Register a folder and index it in the background.
    pub fn add_repo(self: &Arc<Self>, path: &Path) -> Result<PathBuf> {
        let root = path
            .canonicalize()
            .with_context(|| format!("{} does not exist", path.display()))?;
        if !root.is_dir() {
            bail!("{} is not a folder", root.display());
        }
        if !self.allowed(&root) {
            bail!("{} is outside the folders this daemon may index", root.display());
        }
        let repo = Arc::new(Repo {
            root: root.clone(),
            state: Mutex::new(RepoState::Indexing),
            bulky: Mutex::new(Vec::new()),
        });
        if lock(&self.repos).insert(root.clone(), repo.clone()).is_some() {
            // Already known: index again with the existing entry replaced.
        }
        self.save();
        let daemon = self.clone();
        std::thread::spawn(move || {
            // Beside the index, so the warning shows while a slow index runs (a
            // bulky folder is often why it is slow) without holding the index
            // up, and a failure there never fails the index.
            let bulky = {
                let repo = repo.clone();
                std::thread::spawn(move || *lock(&repo.bulky) = lge_index::bulky_dirs(&repo.root))
            };
            let state = match daemon.open_repo(&repo.root) {
                Ok(server) => RepoState::Ready(Box::new(server)),
                Err(e) => RepoState::Failed(format!("{e:#}")),
            };
            // A repository listed as ready has its warnings too.
            let _ = bulky.join();
            *lock(&repo.state) = state;
        });
        Ok(root)
    }

    pub fn remove_repo(&self, path: &Path) -> bool {
        let removed = lock(&self.repos).remove(path).is_some();
        self.save();
        removed
    }

    fn graph_dir(&self, root: &Path) -> PathBuf {
        let name = root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "root".to_string());
        let safe: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        self.cfg
            .data_dir
            .join("graphs")
            .join(format!("{safe}-{}", &stable_id(&[&root.to_string_lossy()])[..8]))
    }

    fn open_repo(&self, root: &Path) -> Result<lge_mcp::Server> {
        let dir = self.graph_dir(root);
        std::fs::create_dir_all(&dir)?;
        let store = Store::open(&dir.join("graph.lbug"))?;
        let index = IndexOptions {
            service_depth: guess_service_depth(root),
            ..IndexOptions::default()
        };
        index_repo(&store, root, &index)?;
        // Recent commits and merged pull requests, as facts linked to their files.
        let _ = lge_memory::ingest_repo_history(&store, root, lge_retrieve::unix_now());
        let _ = lge_memory::export_pages(&store, &dir.join("pages"));
        let mut server = lge_mcp::Server::new(Box::new(store), root, index);
        server.pages_dir = Some(dir.join("pages"));
        Ok(server)
    }

    /// The registered repository that holds `cwd`. With auto-indexing on, a
    /// git repository seen for the first time is registered and `None` is
    /// returned until its index is ready.
    fn repo_for(self: &Arc<Self>, cwd: &Path) -> Option<Arc<Repo>> {
        let found = lock(&self.repos)
            .iter()
            .filter(|(root, _)| cwd.starts_with(root))
            .max_by_key(|(root, _)| root.as_os_str().len())
            .map(|(_, r)| r.clone());
        if found.is_some() {
            return found;
        }
        if lock(&self.settings).auto_index {
            let mut dir = Some(cwd);
            while let Some(d) = dir {
                if d.join(".git").exists() {
                    if self.allowed(d) {
                        if let Ok(root) = self.add_repo(d) {
                            return self.wait_ready(&root, FIRST_INDEX_WAIT);
                        }
                    }
                    break;
                }
                dir = d.parent();
            }
        }
        None
    }

    /// Wait up to `limit` for the first index of a repository. The first
    /// message of a session is where context saves the most, and a context
    /// added on a later turn would change the cached prefix; a small
    /// repository is indexed in well under the limit. A larger one is used as
    /// soon as it is ready, on a later request.
    fn wait_ready(&self, root: &Path, limit: Duration) -> Option<Arc<Repo>> {
        let repo = lock(&self.repos).get(root).cloned()?;
        let deadline = Instant::now() + limit;
        loop {
            if matches!(*lock(&repo.state), RepoState::Ready(_)) {
                return Some(repo);
            }
            if matches!(*lock(&repo.state), RepoState::Failed(_)) || Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    // ---- the proxy path ---------------------------------------------------

    /// The conversation `key` belongs to, created on its first request. A new
    /// conversation continues the counters of a stored record with the same
    /// id (the daemon restarted), keeps its arm, and otherwise is placed in
    /// the holdout arm by its bucket. A conversation whose first request is
    /// one of Codex's background requests (`background`, its label) is a
    /// background session, never held out.
    fn conv_for(&self, key: &str, seen: &Seen, background: Option<&str>) -> Arc<Mutex<Conv>> {
        if let Some(c) = lock(&self.convs).get(key) {
            return c.clone();
        }
        let id = stable_id(&["session", key]);
        let stored = self.usage.get_usage(&id).ok().flatten();
        let share = lock(&self.settings).holdout_share;
        let bucket_key = seen.conversation.as_deref().unwrap_or(key);
        let now = self.now();
        let mut session = Session::default();
        let (record, holdout, carried) = match stored {
            Some(mut record) if is_proxy_mode(&record.mode) => {
                let mut monitor: MonitorJson = serde_json::from_str(&record.monitor).unwrap_or_default();
                // A session of Codex's background requests that started before
                // lge told them apart (no label stored) becomes a background
                // session when it never had a pack: no pack, never held out,
                // and out of the comparisons from now on. Its items decided
                // before keep their bytes; one with packs keeps its arm.
                let had_pack = monitor.ledger.contexts.values().any(|added| *added) || record.pack_tokens > 0;
                if let (Some(label), None, false) = (background, &monitor.background, had_pack) {
                    if record.mode != BACKGROUND_MODE {
                        record.mode = BACKGROUND_MODE.to_string();
                    }
                    monitor.background = Some(label.to_string());
                }
                // The items decided before the restart keep their bytes and are not counted again.
                session.restore(std::mem::take(&mut monitor.ledger));
                let carried = Carried {
                    rules: serde_json::from_str(&record.rule_stats).unwrap_or_default(),
                    monitor,
                    pack_tokens: record.pack_tokens,
                };
                let holdout = record.mode == "proxy-holdout";
                (record, holdout, carried)
            }
            _ => {
                let holdout = background.is_none() && in_holdout(share, bucket_key);
                let mode = match (background, holdout) {
                    (Some(_), _) => BACKGROUND_MODE,
                    (None, true) => "proxy-holdout",
                    (None, false) => "proxy",
                };
                let record = UsageRecord {
                    id,
                    task_id: key.to_string(),
                    task: seen.first_user_message.clone(),
                    attempt: 1,
                    level: "top".to_string(),
                    runner: "codex".to_string(),
                    session_id: key.to_string(),
                    started_at: now,
                    finished_at: now,
                    outcome: outcome::UNKNOWN.to_string(),
                    mode: mode.to_string(),
                    ..UsageRecord::default()
                };
                let carried = Carried {
                    monitor: MonitorJson {
                        background: background.map(str::to_string),
                        ..MonitorJson::default()
                    },
                    ..Carried::default()
                };
                (record, holdout, carried)
            }
        };
        let conv = Conv {
            key: key.to_string(),
            session,
            requests: carried.monitor.requests,
            raw_retries: carried.monitor.raw_retries,
            cold_restarts: carried.monitor.cold_restarts,
            cold_tokens: carried.monitor.cold_tokens,
            record,
            repo: None,
            routed: false,
            holdout,
            background: carried.monitor.background.clone(),
            background_requests: carried.monitor.background_requests.clone(),
            carried,
            last_response_at: None,
            last_input_tokens: 0,
            fixed: FixedPrefix::default(),
            last_sent: (0, 0),
        };
        lock(&self.convs)
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(conv)))
            .clone()
    }

    /// Rewrite a Responses request body. Returns the bytes to forward and the
    /// conversation they belong to, or `None` to forward the original.
    /// `origin`: the request's `x-openai-subagent` and `x-codex-turn-metadata`
    /// headers. `again`: the same request a second time, after the provider
    /// refused it and the request's new decisions were reverted; it is not
    /// counted as another request.
    fn rewrite(self: &Arc<Self>, body: &[u8], thread: Option<&str>, origin: &Origin, again: bool) -> Option<Rewritten> {
        let mut value: Value = serde_json::from_slice(body).ok()?;
        let seen = inspect(&value);
        let background = background_label(origin.subagent.as_deref(), origin.turn_metadata.as_deref(), &value);
        // Codex's request for a compaction's summary: a request of the
        // conversation itself, counted apart, never a background session.
        let compaction = compaction_request(origin.turn_metadata.as_deref(), &value);
        // Sub-agents share the cache key of their parent; the thread header tells them apart.
        let key = thread
            .map(str::to_string)
            .or_else(|| seen.conversation.clone())
            .unwrap_or_else(|| {
                stable_id(&[
                    "conversation",
                    &seen.first_user_message,
                    seen.cwd.as_deref().unwrap_or(""),
                ])
            });
        let conv = self.conv_for(&key, &seen, background.as_deref());
        let (mut options, router_apply) = {
            let settings = lock(&self.settings);
            (settings.options(), settings.router_apply)
        };
        let repo = seen.cwd.as_deref().and_then(|cwd| self.repo_for(Path::new(cwd)));
        let fixed_instructions = instructions_text(&value);
        let fixed_tools = tools_hash(&value);
        let now = self.now();
        let first = seen.first_user_message.clone();
        let model = seen.model.clone();
        let seen;
        let mut new_changes;
        let trace;
        let label;
        let mut effort_decided = false;
        let mut effort_changed = false;
        {
            let mut guard = lock(&conv);
            let c = &mut *guard;
            options.holdout = c.holdout;
            // A background request (or any request of a background session),
            // and the request for a compaction's summary, never get a new
            // pack; their outputs are shortened as usual and packs decided
            // before are sent again.
            let background = background
                .or_else(|| c.background.clone())
                .or_else(|| compaction.then(|| COMPACTION_LABEL.to_string()));
            if background.is_some() {
                options.inject_context = false;
            }
            label = background.clone();
            if !again {
                c.requests += 1;
                if let Some(label) = &background {
                    *c.background_requests.entry(label.clone()).or_default() += 1;
                }
            }
            if c.record.task.is_empty() {
                c.record.task = first.clone();
            }
            if let Some(model) = &model {
                c.record.model = model.clone();
            }
            // Starts the request: what it decides for the first time can be reverted.
            c.session.begin_request();
            match &repo {
                Some(repo) => {
                    *lock(&self.last_repo) = Some(repo.root.clone());
                    c.repo = Some(repo.root.clone());
                    let mut state = lock(&repo.state);
                    match &mut *state {
                        RepoState::Ready(server) => {
                            if !again {
                                let _ = server.freshen();
                                // New commits since the last request (Codex may have committed, a PR may have been merged).
                                let _ = lge_memory::ingest_repo_history(
                                    server.store(),
                                    server.root(),
                                    lge_retrieve::unix_now(),
                                );
                            }
                            let graph = Graph {
                                store: server.store(),
                                root: server.root(),
                            };
                            // Routed once per conversation. A conversation
                            // restored after a restart keeps its decision, so
                            // its reasoning effort never changes mid-way.
                            if !c.routed && !first.is_empty() && c.background.is_none() {
                                c.routed = true;
                                if c.record.recommended_level.is_empty() {
                                    if let Ok(route) =
                                        lge_router::route(graph.store, &first, &lge_router::RouterConfig::default())
                                    {
                                        c.record.recommended_level = route.level.as_str().to_string();
                                        c.record.reason = route.reason;
                                        c.record.files = route.files;
                                        c.record.services = route.services;
                                        // Applied only from a conversation's first
                                        // request: a later start would change the
                                        // effort in the middle of it.
                                        if c.requests == 1 && !c.holdout && router_apply.allows(route.level) {
                                            c.record.level = route.level.as_str().to_string();
                                            effort_decided = true;
                                        }
                                    }
                                }
                            }
                            seen = transform(&mut value, &mut c.session, Some(&graph), &options);
                        }
                        _ => {
                            seen = transform(&mut value, &mut c.session, None, &options);
                        }
                    }
                }
                None => {
                    seen = transform(&mut value, &mut c.session, None, &options);
                }
            }
            new_changes = c.session.new_changes();
            // The router's level, once applied, holds for every request of
            // the conversation except Codex's background ones.
            if label.is_none() && !c.holdout {
                if let Some(level) = Level::parse(&c.record.level) {
                    if let Some((asked, sent)) = lower_effort(&mut value, level) {
                        effort_changed = true;
                        if effort_decided {
                            new_changes += 1;
                            eprintln!(
                                "lge: router applied: reasoning effort {asked} -> {sent} for conversation {} ({})",
                                key.chars().take(8).collect::<String>(),
                                c.record.reason
                            );
                        }
                    } else if effort_decided {
                        // Nothing to lower: the client already asked for this
                        // effort or less, or named none.
                        c.record.level = Level::Top.as_str().to_string();
                        effort_decided = false;
                    }
                }
            }
            // The annuity: tokens kept out of, and added to, what was sent.
            if again {
                c.record.removed_tokens_sent -= c.last_sent.0;
                c.record.added_tokens_sent -= c.last_sent.1;
            }
            c.record.removed_tokens_sent += seen.removed_present;
            c.record.added_tokens_sent += seen.added_present;
            c.last_sent = (seen.removed_present, seen.added_present);
            c.sync_record();
            trace = Trace {
                request: c.requests,
                lge_added_tokens: seen.added_present,
                lge_removed_tokens: seen.removed_present,
                largest_outputs: seen.largest_new_outputs.clone(),
                ..Trace::default()
            };
            if let Some(item) = seen.lge_break_item {
                eprintln!(
                    "lge: cache break caused by lge at item {item} (conversation {})",
                    key.chars().take(8).collect::<String>()
                );
            }
            // The fixed prefix, counted once per distinct instructions and tools.
            let instructions_hash = content_hash(fixed_instructions.as_bytes());
            if c.fixed.instructions_hash != instructions_hash {
                c.fixed.instructions_hash = instructions_hash;
                c.fixed.instructions_tokens = lge_retrieve::budget::count(&fixed_instructions);
            }
            if c.fixed.tools_hash != fixed_tools {
                c.fixed.tools_hash = fixed_tools;
                c.fixed.tools = tool_defs(&value);
            }
            if !again {
                self.instructions_tokens
                    .store(c.fixed.instructions_tokens as u64, Ordering::Relaxed);
                let mut tools = lock(&self.tools);
                for def in &c.fixed.tools {
                    let t = tools.entry(def.name.clone()).or_default();
                    t.group = def.group.clone();
                    t.mcp = def.mcp;
                    t.tokens = def.tokens;
                    t.requests_seen += 1;
                }
                for name in &seen.called_tools {
                    if let Some(t) = tools.get_mut(name) {
                        t.last_called = Some(now);
                    }
                }
            }
        }
        // Nothing was replaced or added: send the bytes exactly as they came.
        let changed = seen.outputs_replaced > 0 || seen.contexts_present > 0 || effort_changed;
        let body = if changed {
            serde_json::to_vec(&value).ok()?
        } else {
            body.to_vec()
        };
        Some(Rewritten {
            body,
            changed,
            new_changes,
            background: label,
            effort_decided,
            conv,
            trace,
        })
    }

    /// Add a response's usage to its conversation and store the record. Every
    /// [`CHECK_EVERY`] records, the rules' recovery rates are checked.
    fn record_usage(&self, conv: &Arc<Mutex<Conv>>, usage: Usage) {
        let now = self.now();
        let record = {
            let mut c = lock(conv);
            c.observe_response(now, &usage);
            c.record.input_tokens += usage.input_tokens;
            c.record.cached_input_tokens += usage.cached_input_tokens;
            c.record.output_tokens += usage.output_tokens;
            c.record.reasoning_tokens += usage.reasoning_tokens;
            c.record.finished_at = now;
            c.sync_record();
            c.record.clone()
        };
        if let Err(e) = self.usage.put_usage(&record) {
            eprintln!("lge: the usage of a session could not be stored: {e:#}");
        }
        if (self.recorded.fetch_add(1, Ordering::Relaxed) + 1).is_multiple_of(CHECK_EVERY) {
            self.check_rules();
        }
    }

    /// The harm tripwire: a rule in `on` whose recovery rate over the stored
    /// sessions of the last 30 days (since its `rule_since`) is above
    /// [`DEMOTE_ABOVE`], over at least [`MIN_JUDGED`] judged items, is moved to
    /// `shadow` for new items. Items decided before keep their bytes. The
    /// demotion is saved in `daemon.json` and shown with its reason.
    fn check_rules(&self) {
        let Ok(records) = self.usage.list_usage(self.now() - WINDOW_SECONDS) else {
            return;
        };
        let due = due_demotions(&lock(&self.settings), &records);
        if due.is_empty() {
            return;
        }
        {
            let mut settings = lock(&self.settings);
            for (id, reason) in due {
                eprintln!("lge: rule {id} moved to shadow for new items: {reason}");
                settings.demoted.insert(id, reason);
            }
        }
        self.save();
    }

    fn upstream_for(&self, headers: &[(String, String)]) -> &str {
        // A ChatGPT login names its account; an API key does not.
        if headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("chatgpt-account-id"))
        {
            &self.cfg.upstream_chatgpt
        } else {
            &self.cfg.upstream_api
        }
    }

    /// Send one request to the provider. `accept-encoding` is replaced by
    /// `identity` so the usage can be read from the stream.
    fn send(
        &self,
        method: &str,
        target: &str,
        headers: &[(String, String)],
        body: Vec<u8>,
    ) -> Result<ureq::http::Response<ureq::Body>> {
        let mut builder = ureq::http::Request::builder().method(method).uri(target);
        for (k, v) in headers {
            if hop_by_hop(k) || k.eq_ignore_ascii_case("accept-encoding") {
                continue;
            }
            builder = builder.header(k.as_str(), v.as_str());
        }
        builder = builder.header("accept-encoding", "identity");
        if body.is_empty() && method != "POST" {
            builder
                .body(())
                .map_err(|e| anyhow!("{e}"))
                .and_then(|r| self.agent.run(r).map_err(|e| anyhow!("{e}")))
        } else {
            builder
                .body(body)
                .map_err(|e| anyhow!("{e}"))
                .and_then(|r| self.agent.run(r).map_err(|e| anyhow!("{e}")))
        }
    }

    fn proxy(self: &Arc<Self>, mut request: Request) {
        // Codex opens a WebSocket first. 426 makes it switch to HTTP streaming
        // for the session at once (a 400 would end the turn), and the proxy can
        // only read and rewrite HTTP requests.
        let upgrade = request
            .headers()
            .iter()
            .any(|h| h.field.equiv("Upgrade") && h.value.as_str().eq_ignore_ascii_case("websocket"));
        if upgrade {
            return respond_json(
                request,
                426,
                &json!({ "error": { "message": "lge proxy: use HTTP streaming for this provider", "type": "upgrade_required" } }),
            );
        }
        let method = request.method().as_str().to_string();
        let url = request.url().to_string();
        let (path, query) = split_url(&url);
        let original_headers: Vec<(String, String)> = request
            .headers()
            .iter()
            .map(|h| (h.field.as_str().as_str().to_string(), h.value.as_str().to_string()))
            .collect();
        let mut original_body = Vec::new();
        if request
            .as_reader()
            .take(MAX_BODY as u64)
            .read_to_end(&mut original_body)
            .is_err()
        {
            return respond_json(
                request,
                400,
                &json!({ "error": { "message": "lge proxy: the request body could not be read" } }),
            );
        }
        let mut headers = original_headers.clone();
        let mut body = original_body.clone();

        // Rewrite a Responses request; anything else, and any failure, passes as it came.
        let mut rewritten: Option<Rewritten> = None;
        let mut plain: Option<Vec<u8>> = None;
        let mut thread: Option<String> = None;
        let mut origin = Origin::default();
        // One line per model request in the log, without any content: what came
        // in, what was done with it, and whether the usage was read.
        let mut note = String::new();
        let is_responses = method == "POST" && path.trim_end_matches('/').ends_with("/responses");
        if is_responses {
            let encoding = headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("content-encoding"))
                .map(|(_, v)| v.to_ascii_lowercase());
            plain = match encoding.as_deref() {
                None | Some("identity") => Some(body.clone()),
                Some("zstd") => ruzstd::decoding::StreamingDecoder::new(&body[..])
                    .ok()
                    .and_then(|mut d| {
                        let mut out = Vec::new();
                        d.read_to_end(&mut out).ok().map(|_| out)
                    }),
                Some(_) => None,
            };
            thread = headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("thread-id"))
                .map(|(_, v)| v.clone());
            let header = |name: &str| {
                headers
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(name))
                    .map(|(_, v)| v.clone())
            };
            origin = Origin {
                subagent: header(SUBAGENT_HEADER),
                turn_metadata: header(TURN_METADATA_HEADER),
            };
            rewritten = plain
                .as_deref()
                .and_then(|p| self.rewrite_safely(p, thread.as_deref(), &origin, false));
            note = format!(
                "{} {}B {}",
                encoding.as_deref().unwrap_or("plain"),
                body.len(),
                match (plain.is_some(), &rewritten) {
                    (false, _) => "not decoded: forwarded as is",
                    (true, None) => "not a request lge reads: forwarded as is",
                    (true, Some(_)) => "read",
                }
            );
            if let Some(r) = &rewritten {
                note.push_str(&format!(" -> {}B", r.body.len()));
                if let Some(label) = &r.background {
                    note.push_str(&format!(" (background: {label})"));
                }
                body = r.body.clone();
                headers.retain(|(k, _)| !k.eq_ignore_ascii_case("content-encoding"));
            }
        }

        let base = self.upstream_for(&headers).trim_end_matches('/').to_string();
        let tail = path
            .strip_prefix("/v1")
            .filter(|rest| rest.is_empty() || rest.starts_with('/'))
            .unwrap_or(path);
        let target = if query.is_empty() {
            format!("{base}{tail}")
        } else {
            format!("{base}{tail}?{query}")
        };
        let mut response = match self.send(&method, &target, &headers, body) {
            Ok(r) => r,
            Err(e) => {
                return respond_json(
                    request,
                    502,
                    &json!({ "error": { "message": format!("lge proxy: the provider could not be reached: {e}"), "type": "proxy_error" } }),
                );
            }
        };

        // The provider refused a body lge changed, before anything was
        // relayed: send it once more without the rewrites first made in this
        // request. Older decisions stay, so the cached prefix survives, and
        // the reverted items stay as the client sent them from now on.
        let mut retried = false;
        let first_status = response.status().as_u16();
        let due = rewritten
            .as_ref()
            .is_some_and(|r| RETRY_STATUSES.contains(&first_status) && r.changed && r.new_changes > 0);
        let refused = if due && plain.is_some() { rewritten.take() } else { None };
        if let (Some(r), Some(plain)) = (refused, &plain) {
            {
                let mut error = Vec::new();
                let _ = response
                    .into_body()
                    .into_reader()
                    .take(MAX_ERROR_BODY)
                    .read_to_end(&mut error);
                // Codex sends a conversation's requests one at a time, so the
                // request's new decisions are still the session's latest.
                {
                    let mut c = lock(&r.conv);
                    c.session.revert_new();
                    // The provider may have refused the lowered effort: the
                    // conversation keeps the client's effort from now on.
                    if r.effort_decided {
                        c.record.level = Level::Top.as_str().to_string();
                        c.record
                            .reason
                            .push_str("; not applied: the provider refused the request");
                    }
                }
                let again = self.rewrite_safely(plain, thread.as_deref(), &origin, true);
                // Unchanged now: the client's own bytes and headers, as they came.
                let (retry_headers, retry_body) = match &again {
                    Some(a) if a.changed => (headers.clone(), a.body.clone()),
                    _ => (original_headers.clone(), original_body.clone()),
                };
                response = match self.send(&method, &target, &retry_headers, retry_body) {
                    Ok(resp) => resp,
                    Err(e) => {
                        return respond_json(
                            request,
                            502,
                            &json!({ "error": { "message": format!("lge proxy: the provider could not be reached: {e}"), "type": "proxy_error" } }),
                        );
                    }
                };
                retried = true;
                {
                    let mut c = lock(&r.conv);
                    c.raw_retries += 1;
                    c.sync_record();
                }
                eprintln!(
                    "lge: the provider refused a rewritten request with {first_status} ({}); sent again without its {} new rewrites: {}",
                    error_kind(&error),
                    r.new_changes,
                    response.status().as_u16()
                );
                rewritten = Some(again.unwrap_or(r));
            }
        }

        // Relay the status, the headers and the body as they arrive.
        let status = response.status();
        let mut head = format!(
            "HTTP/1.1 {} {}\r\n",
            status.as_u16(),
            status.canonical_reason().unwrap_or("")
        );
        let mut event_stream = false;
        let (mut kind, mut response_encoding) = (String::new(), String::new());
        for (name, value) in response.headers() {
            if hop_by_hop(name.as_str()) {
                continue;
            }
            let Ok(value) = value.to_str() else { continue };
            if name.as_str().eq_ignore_ascii_case("content-type") {
                kind = value.to_string();
                if value.contains("text/event-stream") {
                    event_stream = true;
                }
            }
            if name.as_str().eq_ignore_ascii_case("content-encoding") {
                response_encoding = value.to_string();
            }
            head.push_str(&format!("{}: {value}\r\n", name.as_str()));
        }
        head.push_str("Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n");
        let mut reader = response.into_body().into_reader();
        let mut writer = request.into_writer();
        let mut scanner = UsageScanner::default();
        let mut whole = Vec::new();
        let relay = (|| -> std::io::Result<()> {
            writer.write_all(head.as_bytes())?;
            let mut buf = vec![0u8; 16 * 1024];
            loop {
                let n = reader.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                // Read the usage whatever the content type says: some backends
                // stream events without naming them as such.
                scanner.feed(&buf[..n]);
                if !event_stream && whole.len() < 8 * 1024 * 1024 {
                    whole.extend_from_slice(&buf[..n]);
                }
                write!(writer, "{n:x}\r\n")?;
                writer.write_all(&buf[..n])?;
                writer.write_all(b"\r\n")?;
                writer.flush()?;
            }
            Ok(())
        })();
        // A stream the client cut still used tokens: record what was seen.
        let usage = scanner.usage.or_else(|| {
            // A response that was not streamed carries its usage in the body.
            let v: Value = serde_json::from_slice(&whole).ok()?;
            crate::responses::usage_of_event(&json!({ "type": "response.completed", "response": v }).to_string())
        });
        if is_responses {
            eprintln!(
                "lge: {method} {path} {}{} | {note} | {} | usage {}",
                status.as_u16(),
                if retried {
                    format!(" (after {first_status})")
                } else {
                    String::new()
                },
                format!("{kind} {response_encoding}").trim(),
                match usage {
                    Some(u) => format!(
                        "in {} (cached {}) out {} (reasoning {})",
                        u.input_tokens, u.cached_input_tokens, u.output_tokens, u.reasoning_tokens
                    ),
                    None => "not found".to_string(),
                }
            );
        }
        // Recorded before the last chunk goes out, so a client that asks for
        // the numbers right after the answer ends sees them.
        if let Some(r) = &rewritten {
            if let Some(usage) = usage {
                self.record_usage(&r.conv, usage);
            }
            self.trace(&r.conv, r.trace.clone(), status.as_u16(), retried, usage);
        }
        if relay.is_ok() {
            let _ = writer.write_all(b"0\r\n\r\n").and_then(|_| writer.flush());
        }
        drop(writer);
    }

    /// [`Daemon::rewrite`], with a panic taken as "forward the original".
    fn rewrite_safely(
        self: &Arc<Self>,
        plain: &[u8],
        thread: Option<&str>,
        origin: &Origin,
        again: bool,
    ) -> Option<Rewritten> {
        let daemon = self.clone();
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            daemon.rewrite(plain, thread, origin, again)
        }))
        .ok()
        .flatten()
    }

    /// Keep the trace of the request that just ended.
    fn trace(&self, conv: &Arc<Mutex<Conv>>, mut t: Trace, status: u16, retried: bool, usage: Option<Usage>) {
        let (key, trace) = {
            let c = lock(conv);
            t.status = status;
            t.retried = retried;
            if let Some(u) = usage {
                t.usage_found = true;
                t.input_tokens = u.input_tokens;
                t.cached_input_tokens = u.cached_input_tokens;
                t.output_tokens = u.output_tokens;
                t.reasoning_tokens = u.reasoning_tokens;
            }
            (c.key.clone(), t)
        };
        lock(&self.traces).push(&key, trace);
    }

    /// The traces of one conversation, oldest first.
    fn requests_of(&self, key: &str) -> Value {
        let traces = lock(&self.traces);
        let list: Vec<&Trace> = traces.by_conv.get(key).map(|l| l.iter().collect()).unwrap_or_default();
        json!({ "thread": key, "known": traces.by_conv.contains_key(key), "requests": list })
    }

    // ---- MCP over HTTP ----------------------------------------------------

    /// The repository an MCP call is about: named in the query, or the one the
    /// agent last worked in.
    fn mcp_repo(&self, query: &str) -> Option<Arc<Repo>> {
        let named = query_value(query, "repo").map(PathBuf::from);
        let root = named.or_else(|| lock(&self.last_repo).clone()).or_else(|| {
            let repos = lock(&self.repos);
            (repos.len() == 1).then(|| repos.keys().next().cloned()).flatten()
        })?;
        lock(&self.repos).get(&root).cloned()
    }

    fn mcp(&self, mut request: Request) {
        if *request.method() != Method::Post {
            // No server-initiated stream is offered.
            let _ = request.respond(Response::empty(405));
            return;
        }
        let (_, query) = split_url(request.url());
        let repo = self.mcp_repo(query);
        let mut text = String::new();
        if request
            .as_reader()
            .take(8 * 1024 * 1024)
            .read_to_string(&mut text)
            .is_err()
        {
            return respond_json(request, 400, &json!({ "error": "unreadable body" }));
        }
        let Ok(message) = serde_json::from_str::<Value>(&text) else {
            return respond_json(
                request,
                400,
                &json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": "invalid JSON" } }),
            );
        };
        let mut call = |name: &str, args: &Value| -> Result<String> {
            let Some(repo) = &repo else {
                bail!("no indexed repository yet; add a folder on the lge page first")
            };
            let mut state = lock(&repo.state);
            match &mut *state {
                RepoState::Ready(server) => server.call(name, args),
                RepoState::Indexing => bail!("{} is still being indexed; try again in a moment", repo.root.display()),
                RepoState::Failed(e) => bail!("{} could not be indexed: {e}", repo.root.display()),
            }
        };
        let answer = match &message {
            Value::Array(batch) => {
                let replies: Vec<Value> = batch.iter().filter_map(|m| lge_mcp::reply(m, &mut call)).collect();
                (!replies.is_empty()).then_some(Value::Array(replies))
            }
            single => lge_mcp::reply(single, &mut call),
        };
        match answer {
            Some(v) => respond_json(request, 200, &v),
            None => {
                let _ = request.respond(Response::empty(202));
            }
        }
    }

    // ---- the page and its API ----------------------------------------------

    fn repo_json(&self, repo: &Repo) -> Value {
        let state = lock(&repo.state);
        let (word, detail, stats) = match &*state {
            // Deleted or moved since it was added: its graph is stale, and no
            // request can come from it any more.
            _ if !repo.root.is_dir() => (
                "missing",
                "The folder no longer exists (deleted or moved). Remove it from the list.".to_string(),
                Vec::new(),
            ),
            RepoState::Indexing => ("indexing", String::new(), Vec::new()),
            RepoState::Failed(e) => ("failed", e.clone(), Vec::new()),
            RepoState::Ready(server) => ("ready", String::new(), server.store().stats().unwrap_or_default()),
        };
        let stat = |k: &str| stats.iter().find(|(n, _)| n == k).map(|(_, v)| *v).unwrap_or(0);
        let bulky: Vec<Value> = lock(&repo.bulky)
            .iter()
            .map(|d| {
                let file = repo.root.join(lge_index::ignore_file(&repo.root));
                json!({ "path": d.path, "bytes": d.bytes, "files": d.files, "reason": d.reason, "ignore_line": d.ignore_line(), "ignore_file": file.to_string_lossy() })
            })
            .collect();
        json!({
            "root": repo.root.to_string_lossy(),
            "name": repo.root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
            "state": word,
            "detail": detail,
            "files": stat("files"),
            "symbols": stat("symbols"),
            "services": stat("services"),
            "memories": stat("memories"),
            "bulky": bulky,
        })
    }

    fn status(&self) -> Value {
        let repos: Vec<Value> = lock(&self.repos).values().map(|r| self.repo_json(r)).collect();
        let records = self.usage.list_usage(0).unwrap_or_else(|e| {
            eprintln!("lge: the usage log could not be read: {e:#}");
            Vec::new()
        });
        let now = self.now();
        let window: Vec<UsageRecord> = records
            .iter()
            .filter(|r| r.started_at >= now - WINDOW_SECONDS)
            .cloned()
            .collect();
        let live: HashMap<String, (u64, Saved, Option<String>)> = lock(&self.convs)
            .values()
            .map(|c| {
                let c = lock(c);
                (
                    c.record.id.clone(),
                    (
                        c.requests,
                        c.session.saved.clone(),
                        c.repo.as_ref().map(|p| p.to_string_lossy().to_string()),
                    ),
                )
            })
            .collect();
        // Totals per mode: what past sessions cost, and what sessions through the proxy cost.
        let mut by_mode: BTreeMap<String, [i64; 5]> = BTreeMap::new();
        // Codex's background requests are not sessions anyone worked in.
        for r in records.iter().filter(|r| r.mode != BACKGROUND_MODE) {
            let t = by_mode
                .entry(if r.mode.is_empty() {
                    "other".to_string()
                } else {
                    r.mode.clone()
                })
                .or_insert([0; 5]);
            t[0] += 1;
            t[1] += r.input_tokens;
            t[2] += r.cached_input_tokens;
            t[3] += r.output_tokens;
            t[4] += r.reasoning_tokens;
        }
        let modes: Vec<Value> = by_mode
            .iter()
            .map(|(mode, t)| json!({ "mode": mode, "sessions": t[0], "input_tokens": t[1], "cached_input_tokens": t[2], "output_tokens": t[3], "reasoning_tokens": t[4] }))
            .collect();
        let mut saved = Saved::default();
        for (_, s, _) in live.values() {
            saved.outputs_shortened += s.outputs_shortened;
            saved.tokens_before += s.tokens_before;
            saved.tokens_after += s.tokens_after;
            saved.contexts_added += s.contexts_added;
            saved.context_tokens += s.context_tokens;
            saved.contexts_known += s.contexts_known;
            saved.contexts_over_budget += s.contexts_over_budget;
            saved.contexts_retired += s.contexts_retired;
        }
        let monitor_of = |r: &UsageRecord| -> MonitorJson { serde_json::from_str(&r.monitor).unwrap_or_default() };
        let mut recent: Vec<&UsageRecord> = records.iter().filter(|r| is_proxy_mode(&r.mode)).collect();
        recent.sort_by_key(|r| std::cmp::Reverse(r.finished_at));
        let sessions: Vec<Value> = recent
            .iter()
            .take(50)
            .map(|r| {
                let (requests, s, repo) = live.get(&r.id).cloned().unwrap_or_default();
                let m = monitor_of(r);
                let net = r.removed_tokens_sent - r.added_tokens_sent;
                let share = if r.input_tokens + net > 0 {
                    net as f64 / (r.input_tokens + net) as f64
                } else {
                    0.0
                };
                let requests = requests.max(m.requests);
                json!({
                    "id": r.session_id, "task": r.task, "model": r.model, "repo": repo, "mode": r.mode,
                    "background": m.background,
                    "requests": requests,
                    "mean_input_tokens": mean_input(r, requests),
                    "packs_added": m.packs_added, "packs_known": m.packs_known,
                    "packs_over_budget": m.packs_over_budget, "packs_retired": m.packs_retired,
                    "started_at": r.started_at, "finished_at": r.finished_at,
                    "input_tokens": r.input_tokens, "cached_input_tokens": r.cached_input_tokens,
                    "output_tokens": r.output_tokens, "reasoning_tokens": r.reasoning_tokens,
                    "recommended_level": r.recommended_level,
                    "applied_level": applied_level(r),
                    "outputs_shortened": s.outputs_shortened, "tokens_before": s.tokens_before, "tokens_after": s.tokens_after,
                    "contexts_added": s.contexts_added, "context_tokens": s.context_tokens,
                    "removed_tokens_sent": r.removed_tokens_sent, "added_tokens_sent": r.added_tokens_sent,
                    "net_tokens": net, "net_share": share,
                    "lge_breaks": m.prefix.lge_breaks, "client_breaks": m.prefix.client_breaks,
                    "first_lge_break_item": m.prefix.first_lge_break_item,
                    "cold_restarts": m.cold_restarts, "cold_tokens": m.cold_tokens, "raw_retries": m.raw_retries,
                })
            })
            .collect();

        let settings = lock(&self.settings).clone();
        let (totals, _) = rule_totals(&window, &settings.rule_since);
        let rules: Vec<Value> = RuleId::ALL
            .iter()
            .map(|rule| {
                let id = rule.as_str();
                let stats = totals.get(id).copied().unwrap_or_default();
                let (rate, healthy, _) = rule_rate(&stats);
                json!({
                    "id": id,
                    "kind": match rule.kind() { RuleKind::Lossy => "lossy", RuleKind::Lossless => "lossless", RuleKind::MeasureOnly => "measure_only" },
                    "mode": settings.rules.get(*rule),
                    "effective_mode": settings.effective_mode(*rule),
                    "demoted": settings.demoted.get(id),
                    "since": settings.rule_since.get(id),
                    "stats": stats,
                    "rate": rate,
                    "healthy": healthy,
                })
            })
            .collect();
        let cohorts: Vec<Value> = cohort_totals(&window)
            .into_iter()
            .map(|(cohort, c)| json!({ "cohort": cohort, "reads": c.reads, "would_save_tokens": c.would_save_tokens, "patched_within_5": c.patched_within_5 }))
            .collect();
        let mut holdout = serde_json::to_value(lge_usage::compare_arms(&window)).unwrap_or(Value::Null);
        holdout["share"] = json!(settings.holdout_share);

        // The cache monitor over every proxy session.
        let (mut lge_breaks, mut client_breaks, mut raw_retries, mut cold_restarts, mut cold_tokens) = (0, 0, 0, 0, 0);
        for r in records.iter().filter(|r| is_proxy_mode(&r.mode)) {
            let m = monitor_of(r);
            lge_breaks += m.prefix.lge_breaks;
            client_breaks += m.prefix.client_breaks;
            raw_retries += m.raw_retries;
            cold_restarts += m.cold_restarts;
            cold_tokens += m.cold_tokens;
        }
        let tools = lock(&self.tools).clone();
        let mut groups: BTreeMap<String, (bool, usize, usize, u64)> = BTreeMap::new();
        for t in tools.values() {
            let g = groups.entry(t.group.clone()).or_insert((t.mcp, 0, 0, 0));
            g.1 += 1;
            g.2 += t.tokens;
            g.3 = g.3.max(t.requests_seen);
        }
        let instructions_tokens = self.instructions_tokens.load(Ordering::Relaxed);
        let mut group_rows: Vec<Value> = groups
            .iter()
            .map(|(group, (mcp, n, tokens, requests))| {
                json!({ "group": group, "mcp": mcp, "tools": n, "tokens": tokens, "requests_seen": requests,
                        "config": if *mcp { format!("[mcp_servers.{group}]") } else { String::new() } })
            })
            .collect();
        group_rows.sort_by_key(|g| std::cmp::Reverse(g["tokens"].as_u64().unwrap_or(0)));
        let tool_tokens: usize = tools.values().map(|t| t.tokens).sum();
        let mut ghosts: Vec<(&String, &ToolSeen)> = tools.iter().filter(|(_, t)| t.last_called.is_none()).collect();
        ghosts.sort_by_key(|(name, t)| (std::cmp::Reverse(t.tokens), (*name).clone()));
        let ghost_tools: Vec<Value> = ghosts
            .iter()
            .map(|(name, t)| {
                json!({ "name": name, "group": t.group, "mcp": t.mcp, "tokens": t.tokens, "requests_seen": t.requests_seen,
                        "config": if t.mcp { format!("[mcp_servers.{}]", t.group) } else { String::new() } })
            })
            .collect();
        let conversations: Vec<Value> = recent
            .iter()
            .take(50)
            .map(|r| {
                let m = monitor_of(r);
                json!({ "id": r.session_id, "task": r.task, "lge_breaks": m.prefix.lge_breaks, "client_breaks": m.prefix.client_breaks,
                        "first_lge_break_item": m.prefix.first_lge_break_item, "cold_restarts": m.cold_restarts })
            })
            .collect();
        let (removed, added): (i64, i64) = records
            .iter()
            .filter(|r| r.mode == "proxy" || r.mode == BACKGROUND_MODE)
            .fold((0, 0), |(a, b), r| (a + r.removed_tokens_sent, b + r.added_tokens_sent));
        // Packs over the sessions that get them (not the held-out ones, whose
        // packs are decided and never added): what was added and why others
        // were not. What the held-out sessions would have added, apart.
        let mut packs = [0u64; 4];
        let mut would_add = 0u64;
        for r in records.iter().filter(|r| is_proxy_mode(&r.mode)) {
            let m = monitor_of(r);
            if r.mode == "proxy-holdout" {
                would_add += m.contexts_would_add;
                continue;
            }
            for (slot, n) in packs
                .iter_mut()
                .zip([m.packs_added, m.packs_known, m.packs_over_budget, m.packs_retired])
            {
                *slot += n;
            }
        }
        let background = background_totals(&records);
        let long_sessions = long_sessions(&sessions);

        json!({
            "version": env!("CARGO_PKG_VERSION"),
            "uptime_seconds": self.started.elapsed().as_secs(),
            "listen": format!("{}:{}", self.cfg.host, self.cfg.port),
            "upstream_api": self.cfg.upstream_api,
            "upstream_chatgpt": self.cfg.upstream_chatgpt,
            "workspaces": self.cfg.workspaces.iter().map(|w| w.to_string_lossy().to_string()).collect::<Vec<_>>(),
            "codex_home": self.cfg.codex_home.as_ref().map(|p| p.to_string_lossy().to_string()),
            "settings": settings,
            "settings_error": *lock(&self.settings_error),
            "repos": repos,
            "modes": modes,
            "saved": saved,
            "sessions": sessions,
            "rules": rules,
            "thresholds": { "min_judged": MIN_JUDGED, "demote_above": DEMOTE_ABOVE, "healthy_below": HEALTHY_BELOW, "window_days": WINDOW_SECONDS / 86_400 },
            "cohorts": cohorts,
            "holdout": holdout,
            "prefix": {
                "lge_breaks": lge_breaks,
                "client_breaks": client_breaks,
                "conversations": conversations,
                "fixed": { "instructions_tokens": instructions_tokens, "tool_tokens": tool_tokens, "groups": group_rows },
            },
            "ghost_tools": ghost_tools,
            "monitor": { "raw_retries": raw_retries, "cold_restarts": cold_restarts, "cold_tokens": cold_tokens, "lge_breaks": lge_breaks, "client_breaks": client_breaks },
            "annuity": { "removed_tokens_sent": removed, "added_tokens_sent": added, "net_tokens": removed - added },
            "packs": { "added": packs[0], "known": packs[1], "over_budget": packs[2], "retired": packs[3], "tokens_sent": added, "holdout_would_add": would_add },
            "background": background,
            "long_sessions": long_sessions,
        })
    }

    /// Folders under `path`, for the folder picker.
    fn browse(&self, path: Option<String>) -> Value {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        browse_listing(path.as_deref(), &self.cfg.workspaces, home.as_deref())
    }

    /// Token counts of sessions the agent recorded itself, as the baseline.
    /// Sessions the proxy already measured are not counted twice, and threads
    /// Codex ran in the background (an approval review, a review, a memory
    /// consolidation; see [`lge_usage::background_session`]) are left out, as
    /// they are on the proxy's side. Only counts, ids and the model are read;
    /// no conversation text is stored.
    fn import_history(&self) -> Result<Value> {
        let home = self
            .cfg
            .codex_home
            .clone()
            .ok_or_else(|| anyhow!("no agent home folder is configured (mount ~/.codex and set LGE_CODEX_HOME)"))?;
        let known: std::collections::HashSet<String> = self.usage.list_usage(0)?.into_iter().map(|r| r.id).collect();
        let (mut imported, mut skipped, mut failed, mut background) = (0, 0, 0, 0);
        let mut stack = vec![home.join("sessions")];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                if !(name.starts_with("rollout-") && name.ends_with(".jsonl")) {
                    continue;
                }
                let Ok(session) = lge_usage::parse_rollout_file(&path) else {
                    failed += 1;
                    continue;
                };
                if session.background.is_some() {
                    background += 1;
                    continue;
                }
                let id = stable_id(&["session", &session.session_id]);
                if session.session_id.is_empty() || known.contains(&id) || session.usage.total() == 0 {
                    skipped += 1;
                    continue;
                }
                let when = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                let record = UsageRecord {
                    id,
                    task_id: session.session_id.clone(),
                    attempt: 1,
                    level: "top".to_string(),
                    runner: "codex".to_string(),
                    model: session.model.clone(),
                    session_id: session.session_id.clone(),
                    started_at: when,
                    finished_at: when,
                    input_tokens: session.usage.input_tokens,
                    cached_input_tokens: session.usage.cached_input_tokens,
                    reasoning_tokens: session.usage.reasoning_tokens,
                    output_tokens: session.usage.output_tokens,
                    outcome: outcome::UNKNOWN.to_string(),
                    mode: "history".to_string(),
                    ..UsageRecord::default()
                };
                self.usage.put_usage(&record)?;
                imported += 1;
            }
        }
        Ok(json!({ "imported": imported, "skipped": skipped, "unreadable": failed, "background": background }))
    }

    fn api(self: &Arc<Self>, mut request: Request) {
        let url = request.url().to_string();
        let (path, query) = split_url(&url);
        let method = request.method().clone();
        let header = |name: &str| header_value(&request, name);
        // A page whose name only resolves to this machine (DNS rebinding) must
        // not read the status or the traces.
        if header("Host").is_some_and(|h| !self.own_host(&h)) {
            return respond_json(
                request,
                403,
                &json!({ "error": "requests for another host name are refused" }),
            );
        }
        // A page on another origin must not be able to drive the daemon: its
        // origin must be this daemon's exactly, and a POST must be JSON, which a
        // page elsewhere cannot send without asking first.
        if method != Method::Get {
            let foreign = header("Origin").is_some_and(|o| {
                let host = o.strip_prefix("http://").or_else(|| o.strip_prefix("https://"));
                !host.is_some_and(|h| !h.contains(['/', '@', '?', '#']) && self.own_host(h))
            });
            let json_body = header("Content-Type").is_some_and(|t| {
                t.split(';')
                    .next()
                    .is_some_and(|m| m.trim().eq_ignore_ascii_case("application/json"))
            });
            if foreign || (method == Method::Post && !json_body) {
                return respond_json(
                    request,
                    403,
                    &json!({ "error": "requests from other sites are refused" }),
                );
            }
        }
        let mut text = String::new();
        let _ = request.as_reader().take(1024 * 1024).read_to_string(&mut text);
        let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let path_arg = || body.get("path").and_then(Value::as_str).map(PathBuf::from);
        let result: Result<Value> = match (method, path) {
            (Method::Get, "/lge/api/status") => Ok(self.status()),
            (Method::Get, "/lge/api/browse") => Ok(self.browse(query_value(query, "path"))),
            (Method::Post, "/lge/api/repos") => path_arg()
                .ok_or_else(|| anyhow!("`path` is required"))
                .and_then(|p| self.add_repo(&p))
                .map(|root| json!({ "root": root.to_string_lossy() })),
            (Method::Delete, "/lge/api/repos") => {
                let path = query_value(query, "path")
                    .map(PathBuf::from)
                    .or_else(path_arg)
                    .ok_or_else(|| anyhow!("`path` is required"));
                path.map(|p| json!({ "removed": self.remove_repo(&p) }))
            }
            (Method::Post, "/lge/api/settings") => serde_json::from_value::<SettingsPatch>(body.clone())
                .map_err(|e| anyhow!("{e}"))
                .and_then(|patch| {
                    let now = self.now();
                    let mut settings = lock(&self.settings).clone();
                    settings.apply(patch, now)?;
                    *lock(&self.settings) = settings.clone();
                    *lock(&self.settings_error) = None;
                    self.save();
                    Ok(json!(settings))
                }),
            (Method::Post, "/lge/api/import-history") => self.import_history(),
            (Method::Post, "/lge/api/discover") => self
                .cfg
                .codex_home
                .clone()
                .ok_or_else(|| anyhow!("no agent home folder is configured (mount ~/.codex and set LGE_CODEX_HOME)"))
                .and_then(|home| crate::discover::replay(&home, 30))
                .and_then(|report| serde_json::to_value(report).map_err(|e| anyhow!("{e}"))),
            (Method::Get, p) if trace_id(p).is_some() => {
                Ok(self.requests_of(&percent_decode(trace_id(p).unwrap_or(""))))
            }
            _ => Err(anyhow!("no such endpoint")),
        };
        match result {
            Ok(v) => respond_json(request, 200, &v),
            Err(e) => respond_json(request, 400, &json!({ "error": format!("{e:#}") })),
        }
    }

    /// Serve one request: the page and its API, MCP, or the proxy.
    pub fn handle(self: &Arc<Self>, request: Request) {
        let url = request.url().to_string();
        let (path, _) = split_url(&url);
        if path.starts_with("/lge/api/") {
            self.api(request);
        } else if path == "/mcp" || path == "/mcp/" {
            self.mcp(request);
        } else if (path == "/" || path == "/index.html") && *request.method() == Method::Get {
            let mut response = Response::from_string(UI);
            if let Some(h) = header("Content-Type", "text/html; charset=utf-8") {
                response = response.with_header(h);
            }
            let _ = request.respond(response);
        } else if path == "/health" {
            respond_json(request, 200, &json!({ "ok": true }));
        } else if path == "/favicon.ico" {
            let _ = request.respond(Response::empty(204));
        } else {
            self.proxy(request);
        }
    }
}

/// The value of the request header `name`, any case.
fn header_value(request: &Request, name: &str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str().to_string())
}

/// `:8787`: a colon and a port number.
fn port_only(s: &str) -> bool {
    s.strip_prefix(':')
        .is_some_and(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

/// Bind the listening socket. Returns the server and the address it got.
pub fn bind(daemon: &Arc<Daemon>) -> Result<(tiny_http::Server, std::net::SocketAddr)> {
    let addr = format!("{}:{}", daemon.cfg.host, daemon.cfg.port);
    let server = tiny_http::Server::http(&addr).map_err(|e| anyhow!("cannot listen on {addr}: {e}"))?;
    let bound = server
        .server_addr()
        .to_ip()
        .ok_or_else(|| anyhow!("not an IP listener"))?;
    Ok((server, bound))
}

/// Serve until the process ends: one thread per request.
pub fn run(daemon: Arc<Daemon>, server: tiny_http::Server) {
    for request in server.incoming_requests() {
        let daemon = daemon.clone();
        std::thread::spawn(move || daemon.handle(request));
    }
}

/// The folders the page's picker shows for `path`. With no path it starts at
/// the workspaces, or at `home` when any folder may be indexed. A path that no
/// longer exists (a folder deleted or renamed) shows the nearest folder above
/// it that does, with a note, instead of an empty list.
fn browse_listing(path: Option<&str>, workspaces: &[PathBuf], home: Option<&Path>) -> Value {
    let allowed = |p: &Path| workspaces.is_empty() || workspaces.iter().any(|w| p.starts_with(w));
    let typed = path.map(str::trim).filter(|p| !p.is_empty());
    let requested = match typed {
        Some("~") => home.map(Path::to_path_buf),
        Some(p) => match (p.strip_prefix("~/"), home) {
            (Some(rest), Some(h)) => Some(h.join(rest)),
            _ => Some(PathBuf::from(p)),
        },
        None if workspaces.is_empty() => home.map(Path::to_path_buf),
        None => None,
    };
    let Some(requested) = requested else {
        let roots: Vec<Value> = workspaces
            .iter()
            .map(
                |w| json!({ "name": w.to_string_lossy(), "path": w.to_string_lossy(), "git": w.join(".git").exists() }),
            )
            .collect();
        let error = roots.is_empty().then_some("Type the path of a folder, then Browse.");
        return json!({ "path": "", "folders": roots, "error": error });
    };
    let mut dir = requested.clone();
    while !dir.is_dir() {
        match dir.parent() {
            Some(up) if !up.as_os_str().is_empty() => dir = up.to_path_buf(),
            _ => break,
        }
    }
    let note = (dir != requested && dir.is_dir())
        .then(|| format!("{} does not exist; showing {}", requested.display(), dir.display()));
    if !dir.is_dir() {
        return json!({ "path": requested.to_string_lossy(), "folders": [], "error": format!("{} does not exist", requested.display()) });
    }
    if !allowed(&dir) {
        return json!({ "path": dir.to_string_lossy(), "folders": [], "error": "outside the folders this daemon may index" });
    }
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) => {
            return json!({ "path": dir.to_string_lossy(), "folders": [], "error": format!("{} cannot be read: {e}", dir.display()) });
        }
    };
    let mut folders: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| (e.file_name().to_string_lossy().to_string(), e.path()))
        .filter(|(name, _)| !name.starts_with('.') && name != "node_modules" && name != "target")
        .collect();
    folders.sort();
    let folders: Vec<Value> = folders
        .into_iter()
        .take(500)
        .map(|(name, p)| json!({ "name": name, "path": p.to_string_lossy(), "git": p.join(".git").exists() }))
        .collect();
    json!({
        "path": dir.to_string_lossy(),
        "parent": dir.parent().filter(|p| allowed(p)).map(|p| p.to_string_lossy().to_string()),
        "folders": folders,
        "note": note,
    })
}

#[cfg(test)]
mod tests {

    #[test]
    fn the_effort_is_only_ever_lowered() {
        let lowered = |asked: Value, level: Level| {
            let mut body = json!({ "model": "m", "reasoning": { "effort": asked, "summary": "auto" } });
            let change = lower_effort(&mut body, level);
            (
                change,
                body["reasoning"]["effort"].clone(),
                body["reasoning"]["summary"].clone(),
            )
        };
        let (change, effort, summary) = lowered(json!("xhigh"), Level::Low);
        assert_eq!(change, Some(("xhigh".to_string(), "low".to_string())));
        assert_eq!(
            (effort, summary),
            (json!("low"), json!("auto")),
            "only the effort changes"
        );
        assert_eq!(lowered(json!("high"), Level::Medium).1, json!("medium"));
        // Never raised, never below what was asked, top keeps it, unknown values stay.
        assert_eq!(lowered(json!("minimal"), Level::Low).0, None);
        assert_eq!(lowered(json!("low"), Level::Low).0, None);
        assert_eq!(lowered(json!("low"), Level::Medium).0, None);
        assert_eq!(lowered(json!("high"), Level::Top).0, None);
        assert_eq!(lowered(json!("turbo"), Level::Low).0, None);
        assert_eq!(lowered(json!(3), Level::Low).0, None);
        let mut none = json!({ "model": "m" });
        assert_eq!(lower_effort(&mut none, Level::Low), None);
        assert_eq!(none, json!({ "model": "m" }));
    }

    #[test]
    fn router_apply_allows_by_level() {
        assert!(!RouterApply::Off.allows(Level::Low));
        assert!(RouterApply::LowOnly.allows(Level::Low) && !RouterApply::LowOnly.allows(Level::Medium));
        assert!(RouterApply::LowAndMedium.allows(Level::Medium) && !RouterApply::LowAndMedium.allows(Level::Top));
    }

    #[test]
    fn the_folder_picker_starts_at_home_and_survives_a_deleted_folder() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path().canonicalize().unwrap();
        std::fs::create_dir_all(h.join("code/shop/.git")).unwrap();
        std::fs::create_dir_all(h.join("notes")).unwrap();
        let names = |v: &Value| -> Vec<String> {
            v["folders"]
                .as_array()
                .unwrap()
                .iter()
                .map(|f| f["name"].as_str().unwrap().to_string())
                .collect()
        };

        // Nothing typed and no workspace limit: the home folder.
        let start = browse_listing(None, &[], Some(&h));
        assert_eq!(start["path"].as_str(), Some(h.to_str().unwrap()));
        assert_eq!(names(&start), ["code", "notes"]);
        assert_eq!(names(&browse_listing(Some("  "), &[], Some(&h))), ["code", "notes"]);
        assert_eq!(names(&browse_listing(Some("~/code"), &[], Some(&h))), ["shop"]);

        // A folder that was deleted: the nearest one above it, with a note.
        let gone = h.join("code/old-project/src");
        let up = browse_listing(Some(gone.to_str().unwrap()), &[], Some(&h));
        assert_eq!(up["path"].as_str(), Some(h.join("code").to_str().unwrap()));
        assert_eq!(names(&up), ["shop"]);
        assert!(up["note"].as_str().unwrap().contains("does not exist"), "{up}");
        assert_eq!(up["folders"][0]["git"], true);

        // With workspaces, nothing typed lists them; outside them is refused.
        let ws = vec![h.join("code")];
        let roots = browse_listing(None, &ws, Some(&h));
        assert_eq!(
            roots["folders"][0]["path"].as_str(),
            Some(h.join("code").to_str().unwrap())
        );
        assert!(browse_listing(Some(h.join("notes").to_str().unwrap()), &ws, Some(&h))["error"].is_string());
        // No home and nothing typed: a message, not a silent empty list.
        assert!(browse_listing(None, &[], None)["error"].is_string());
    }
    use super::*;

    fn patch(v: Value) -> SettingsPatch {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn old_settings_files_load_and_shorten_reads_maps_the_skeleton() {
        let old: Settings = serde_json::from_value(json!({
            "inject_context": false, "shorten_outputs": true, "shorten_reads": true, "context_budget": 900, "auto_index": true
        }))
        .unwrap();
        assert_eq!(old.rules.skeleton, Mode::On);
        assert!(old.shorten_reads && !old.inject_context);
        assert_eq!(old.context_budget, 900);
        assert_eq!(old.rules.check, Mode::On, "the other rules take their defaults");
        assert_eq!(old.holdout_share, 0.0);
        let off: Settings = serde_json::from_value(json!({ "shorten_reads": false })).unwrap();
        assert_eq!(off.rules.skeleton, Mode::Shadow);
        // An explicit rules.skeleton wins in a file.
        let both: Settings =
            serde_json::from_value(json!({ "shorten_reads": true, "rules": { "skeleton": "off" } })).unwrap();
        assert_eq!((both.rules.skeleton, both.shorten_reads), (Mode::Off, false));
        let empty: Settings = serde_json::from_value(json!({})).unwrap();
        assert_eq!(empty, Settings::default());
        assert_eq!(empty.rules.skeleton, Mode::Shadow, "shadow by default");
        assert!(serde_json::from_value::<Settings>(json!({ "rules": { "check": "maybe" } })).is_err());
        let future: Settings = serde_json::from_value(json!({ "rules": { "a_later_rule": "on" }, "x": 1 })).unwrap();
        assert_eq!(future, Settings::default());
        for share in [json!(-0.5), json!(1.5)] {
            let s: Settings = serde_json::from_value(json!({ "holdout_share": share })).unwrap();
            assert_eq!(s.holdout_share, 0.0);
        }
        // Written and read back: the same.
        let round: Settings = serde_json::from_str(&serde_json::to_string(&old).unwrap()).unwrap();
        assert_eq!(round, old);
    }

    #[test]
    fn a_request_changes_only_what_it_names() {
        let mut s = Settings::default();
        s.apply(patch(json!({ "rules": { "noise": "on" }, "holdout_share": 0.25 })), 100)
            .unwrap();
        assert_eq!(
            (s.rules.noise, s.holdout_share, s.inject_context),
            (Mode::On, 0.25, true)
        );
        // A client that knows only shorten_reads and sends the rest back as read.
        let mut sent = serde_json::to_value(&s).unwrap();
        sent["shorten_reads"] = json!(true);
        s.apply(patch(sent), 100).unwrap();
        assert_eq!((s.rules.skeleton, s.shorten_reads), (Mode::On, true));
        // A client that changes rules.skeleton and sends shorten_reads back as read.
        let mut sent = serde_json::to_value(&s).unwrap();
        sent["rules"]["skeleton"] = json!("off");
        s.apply(patch(sent), 100).unwrap();
        assert_eq!((s.rules.skeleton, s.shorten_reads), (Mode::Off, false));
        // Demotions are the daemon's: a request lifts one only by name.
        s.demoted
            .insert("check".into(), "recovery 20.0% over 200 items since 2026-11-01".into());
        let mut sent = serde_json::to_value(Settings::default()).unwrap();
        sent["demoted"] = json!({});
        s.apply(patch(sent), 100).unwrap();
        assert!(s.demoted.contains_key("check"));
        assert_eq!(s.effective_mode(RuleId::Check), Mode::Shadow);
        assert_eq!(s.options().rules.check, Mode::Shadow, "a demoted rule runs in shadow");
        s.apply(patch(json!({ "clear_demotion": "check" })), 1234).unwrap();
        assert!(s.demoted.is_empty());
        assert_eq!(s.rule_since["check"], 1234);
        assert_eq!(s.options().rules.check, Mode::On);
        assert!(s.apply(patch(json!({ "clear_demotion": "nope" })), 1).is_err());
        assert!(s.apply(patch(json!({ "rules": { "check": 3 } })), 1).is_err());
    }

    #[test]
    fn an_unreadable_settings_file_gives_the_defaults_and_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_saved(dir.path()).1.is_none(), "no file: no error");
        std::fs::write(
            dir.path().join("daemon.json"),
            r#"{"repos":["/work/a"],"settings":{"rules":{"check":"sometimes"}}}"#,
        )
        .unwrap();
        let (saved, error) = load_saved(dir.path());
        assert_eq!(saved.repos, ["/work/a"], "the repository list is kept");
        assert!(saved.settings.is_none());
        let error = error.expect("an error to show");
        assert!(
            error.contains("daemon.json") && error.contains("default settings"),
            "{error}"
        );
        assert!(dir.path().join("daemon.json.unreadable").exists());
        std::fs::write(dir.path().join("daemon.json"), "{not json").unwrap();
        let (saved, error) = load_saved(dir.path());
        assert!(saved.repos.is_empty() && error.is_some());
    }

    #[test]
    fn the_holdout_bucket_is_stable_and_follows_the_share() {
        let keys: Vec<String> = (0..400).map(|i| format!("conversation-{i}")).collect();
        for k in &keys {
            assert_eq!(holdout_bucket(k), holdout_bucket(k));
            assert!((0.0..1.0).contains(&holdout_bucket(k)));
            assert!(!in_holdout(0.0, k), "share 0 holds out nobody");
            assert!(in_holdout(1.0, k), "share 1 holds out everybody");
        }
        let held = keys.iter().filter(|k| in_holdout(0.25, k)).count();
        assert!((60..=140).contains(&held), "{held} of 400 at 25%");
    }

    #[test]
    fn tools_are_grouped_as_codex_names_them() {
        let body = json!({
            "tools": [
                { "type": "function", "name": "exec_command", "description": "Runs a command.", "parameters": {} },
                { "type": "custom", "name": "apply_patch", "description": "Patches files.", "format": {} },
                { "type": "web_search" },
                { "type": "function", "name": "mcp__github__create_issue", "description": "Creates an issue.", "parameters": {} },
                { "type": "namespace", "name": "mcp__docs__", "description": "Docs.", "tools": [
                    { "type": "function", "name": "lookup", "description": "Looks up.", "parameters": {} },
                    { "type": "function", "name": "fetch", "description": "Fetches.", "parameters": {} }
                ] },
                { "type": "namespace", "name": "mcp__linear", "description": "", "tools": [
                    { "type": "function", "name": "list_issues", "description": "Lists.", "parameters": {} }
                ] },
                { "type": "namespace", "name": "functions", "description": "", "tools": [
                    { "type": "function", "name": "update_plan", "description": "Plans.", "parameters": {} }
                ] }
            ],
            "input": [
                { "type": "additional_tools", "role": "developer", "tools": [
                    { "type": "namespace", "name": "multi_agent", "description": "", "tools": [
                        { "type": "function", "name": "spawn", "description": "Spawns.", "parameters": {} }
                    ] }
                ] }
            ]
        });
        let defs = tool_defs(&body);
        let groups: Vec<(&str, &str, bool)> = defs
            .iter()
            .map(|d| (d.name.as_str(), d.group.as_str(), d.mcp))
            .collect();
        assert_eq!(
            groups,
            [
                ("exec_command", "built-in", false),
                ("apply_patch", "built-in", false),
                ("web_search", "built-in", false),
                ("mcp__github__create_issue", "github", true),
                ("mcp__docs__lookup", "docs", true),
                ("mcp__docs__fetch", "docs", true),
                ("mcp__linearlist_issues", "linear", true),
                ("update_plan", "built-in", false),
                ("multi_agentspawn", "multi_agent", false),
            ]
        );
        assert!(defs.iter().all(|d| d.tokens > 0));
        assert_eq!(tools_hash(&body), tools_hash(&body.clone()));
        assert_ne!(tools_hash(&body), tools_hash(&json!({ "tools": [] })));
        assert_eq!(instructions_text(&json!({ "instructions": "Be brief." })), "Be brief.");
        let newer = json!({ "input": [
            { "type": "additional_tools", "tools": [] },
            { "type": "message", "role": "developer", "content": [{ "type": "input_text", "text": "Base." }] },
            { "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "hi" }] }
        ] });
        assert!(instructions_text(&newer).contains("Base."));
        assert_eq!(instructions_text(&json!({ "input": [] })), "");
    }

    fn judged(id: &str, started_at: i64, rule: &str, judged: u64, recovered: u64) -> UsageRecord {
        let mut totals = RuleTotals::default();
        totals.by_rule.insert(
            rule.to_string(),
            RuleStats {
                items: judged,
                judged,
                recovered,
                ..RuleStats::default()
            },
        );
        UsageRecord {
            id: id.to_string(),
            mode: "proxy".to_string(),
            started_at,
            rule_stats: serde_json::to_string(&totals).unwrap(),
            ..UsageRecord::default()
        }
    }

    #[test]
    fn a_rule_over_the_recovery_threshold_is_demoted() {
        let day = 86_400;
        let records = vec![
            judged("a", 20_000 * day, "check", 120, 24),
            judged("b", 20_001 * day, "check", 80, 16),
            judged("c", 20_000 * day, "search_cap", 400, 4),
            judged("d", 20_000 * day, "line_clip", 300, 200),
        ];
        let settings = Settings::default();
        let due = due_demotions(&settings, &records);
        assert_eq!(
            due,
            [(
                "check".to_string(),
                format!("recovery 20.0% over 200 items since {}", date_of(20_000 * day))
            )],
            "line_clip is in shadow: never demoted"
        );
        let (totals, _) = rule_totals(&records, &BTreeMap::new());
        let (rate, healthy, _) = rule_rate(&totals["search_cap"]);
        assert_eq!((rate, healthy), (Some(0.01), true));
        // Below the minimum, nothing is judged.
        assert!(due_demotions(&settings, &records[..1]).is_empty());
        // Counting restarts after a demotion is cleared.
        let mut since = settings.clone();
        since.rule_since.insert("check".into(), 20_001 * day);
        assert!(due_demotions(&since, &records).is_empty());
        let mut demoted = settings;
        demoted.demoted.insert("check".into(), "x".into());
        assert!(due_demotions(&demoted, &records).is_empty(), "demoted once");
        assert_eq!(date_of(0), "1970-01-01");
        assert_eq!(date_of(1_771_200_000), "2026-02-16");
        assert_eq!(date_of(951_782_400), "2000-02-29");
    }

    #[test]
    fn a_cold_restart_needs_a_gap_a_large_request_and_a_cache_miss() {
        let mut conv = Conv {
            key: "k".into(),
            session: Session::default(),
            record: UsageRecord::default(),
            repo: None,
            requests: 0,
            routed: false,
            holdout: false,
            background: None,
            background_requests: BTreeMap::new(),
            carried: Carried::default(),
            raw_retries: 0,
            cold_restarts: 0,
            cold_tokens: 0,
            last_response_at: None,
            last_input_tokens: 0,
            fixed: FixedPrefix::default(),
            last_sent: (0, 0),
        };
        let usage = |input, cached| Usage {
            input_tokens: input,
            cached_input_tokens: cached,
            ..Usage::default()
        };
        conv.observe_response(1000, &usage(50_000, 0));
        conv.observe_response(1100, &usage(52_000, 1000));
        assert_eq!(conv.cold_restarts, 0, "no gap");
        conv.observe_response(1500, &usage(53_000, 40_000));
        assert_eq!(conv.cold_restarts, 0, "a gap, but the cache held");
        conv.observe_response(2000, &usage(54_000, 3_000));
        assert_eq!((conv.cold_restarts, conv.cold_tokens), (1, 50_000));
        conv.last_input_tokens = 1000;
        conv.observe_response(3000, &usage(2000, 0));
        assert_eq!(conv.cold_restarts, 1, "too small to matter");
        assert_eq!(conv.monitor_json().cold_restarts, 1);
    }

    #[test]
    fn traces_are_bounded() {
        let mut traces = Traces::default();
        for c in 0..TRACE_CONVERSATIONS + 5 {
            traces.push(&format!("c{c}"), Trace::default());
        }
        assert_eq!(traces.by_conv.len(), TRACE_CONVERSATIONS);
        assert!(!traces.by_conv.contains_key("c0") && traces.by_conv.contains_key("c204"));
        for r in 0..TRACE_REQUESTS + 3 {
            traces.push(
                "c204",
                Trace {
                    request: r as u64,
                    ..Trace::default()
                },
            );
        }
        let list = &traces.by_conv["c204"];
        assert_eq!(list.len(), TRACE_REQUESTS);
        assert_eq!(list.back().map(|t| t.request), Some(TRACE_REQUESTS as u64 + 2));
        assert_eq!(trace_id("/lge/api/sessions/abc-1/requests"), Some("abc-1"));
        assert_eq!(trace_id("/lge/api/sessions/requests"), None);
        assert_eq!(trace_id("/lge/api/sessions//requests"), None);
    }
}
