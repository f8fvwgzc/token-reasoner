//! What the proxy remembers about one conversation, and the two decisions it
//! makes with it: what to send in place of a tool output, and what context to
//! add after a message the user typed.
//!
//! The provider caches the start of a prompt only while it stays byte-identical,
//! and the agent resends the whole conversation on every turn. So a decision
//! made for an item is made once and remembered: the same item is rewritten
//! the same way in every later request, whatever the settings say by then.
//! Settings, rule modes and the holdout flag apply only to items seen for the
//! first time.

use crate::command::{normalise, purpose, Normalised, Purpose};
use crate::rules::{cohort_key, CohortStats, Mode, RuleId, RuleModes, RuleStats};
use crate::shorten::{
    cap_numbered_search, cap_search, check_run, clip_match_lines, condensed_upstream, inert, minify_json, skeleton,
    terminal_only, worth_it, CheckDecision, RunStatus,
};
use lge_core::{content_hash, split_words, NodeLabel, NodeRec};
use lge_retrieve::budget::count;
use lge_retrieve::{dependents_of, evidence_pack, find_seeds, PackOptions, SeedOptions, WalkOptions};
use lge_store::GraphStore;
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::LazyLock;

/// The graph of the repository a conversation works in.
pub struct Graph<'a> {
    pub store: &'a dyn GraphStore,
    pub root: &'a Path,
}

#[derive(Debug, Clone)]
pub struct Options {
    /// Add the evidence pack after a message the user typed.
    pub inject_context: bool,
    /// The master switch of every output rule. Off: new items get no rule at
    /// all, not even in shadow. Items decided before keep their bytes.
    pub shorten_outputs: bool,
    /// The mode of each rule.
    pub rules: RuleModes,
    /// This conversation is in the control arm: every rule acts as `shadow`,
    /// and packs are decided and counted but not added.
    pub holdout: bool,
    /// Tokens of one injected pack.
    pub context_budget: usize,
    /// Packs added per conversation at most.
    pub max_contexts: usize,
    /// Tokens of the packs present in one request together at most. A pack
    /// is re-sent on every later request, so this bounds what packs add to
    /// each request of a long conversation.
    pub pack_conversation_budget: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            inject_context: true,
            shorten_outputs: true,
            rules: RuleModes::default(),
            holdout: false,
            context_budget: 1500,
            max_contexts: 6,
            pack_conversation_budget: PACK_CONVERSATION_BUDGET,
        }
    }
}

/// The default of [`Options::pack_conversation_budget`].
pub const PACK_CONVERSATION_BUDGET: usize = 3000;
/// Tokens of a pack after the first one of a conversation at most.
pub const FOLLOW_UP_PACK_TOKENS: usize = 600;
/// Below this many tokens of room in the conversation budget, no pack is added.
pub const MIN_PACK_ROOM: usize = 300;

impl Options {
    /// The options that decide an item again exactly as it was decided before
    /// the daemon restarted: the rules in `mask` (see [`Ledger`]) on, every
    /// other rule off, whatever the settings say now.
    pub(crate) fn pinned(&self, mask: u16) -> Options {
        let mut rules = RuleModes::all(Mode::Off);
        for rule in RuleId::ALL {
            if mask & rule_bit(rule) != 0 {
                rules.set(rule, Mode::On);
            }
        }
        Options {
            shorten_outputs: true,
            holdout: false,
            rules,
            ..self.clone()
        }
    }

    /// The mode `rule` runs in for a new item: `shadow` for every rule in a
    /// holdout conversation, and for a measure-only rule set to `on`.
    pub fn mode(&self, rule: RuleId) -> Mode {
        match self.rules.get(rule) {
            Mode::Off => Mode::Off,
            Mode::On if !self.holdout && !rule.measure_only() => Mode::On,
            _ => Mode::Shadow,
        }
    }
}

/// Token counts of what the proxy changed, for the savings report. Each item
/// is counted once, when it is first decided.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Saved {
    /// Tool outputs that were shortened.
    pub outputs_shortened: usize,
    pub tokens_before: usize,
    pub tokens_after: usize,
    /// Context packs added, and what they cost.
    pub contexts_added: usize,
    pub context_tokens: usize,
    /// What each rule did, by rule id.
    pub by_rule: BTreeMap<String, RuleStats>,
    /// First reads of indexed files by `{language}:{band}`, for the skeleton rule.
    pub cohorts: BTreeMap<String, CohortStats>,
    /// Outputs another filter (RTK) had condensed already, which got only
    /// lossless rules.
    pub condensed_upstream: usize,
    /// Packs decided in a holdout conversation, which were not added, and their tokens.
    pub contexts_would_add: usize,
    pub context_tokens_would_add: usize,
    /// Follow-up packs not added because every starting point was in a file
    /// the conversation had already shown.
    pub contexts_known: usize,
    /// Packs not added because the conversation budget had no room for them.
    pub contexts_over_budget: usize,
    /// Packs no longer inserted after the client compacted its history.
    pub contexts_retired: usize,
}

impl Saved {
    fn rule(&mut self, rule: RuleId) -> &mut RuleStats {
        self.by_rule.entry(rule.as_str().to_string()).or_default()
    }
}

/// The bit of `rule` in a [`Ledger`] mask: its place in [`RuleId::ALL`].
fn rule_bit(rule: RuleId) -> u16 {
    let at = RuleId::ALL.iter().position(|r| *r == rule).unwrap_or(0);
    1 << at
}

/// Characters of an item's key kept in the [`Ledger`].
const LEDGER_KEY_CHARS: usize = 12;

fn ledger_key(id: &str) -> String {
    id.chars().take(LEDGER_KEY_CHARS).collect()
}

/// What each item of a conversation was decided to, by hash: enough for a
/// daemon that restarted to send the same bytes for the items decided before,
/// whatever the settings say by then, without counting them again. Kept in the
/// conversation's usage record; hashes and rule ids only, never content.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Ledger {
    /// The first 12 hex characters of a tool output's key -> the rules whose
    /// result was sent, one bit per rule in [`RuleId::ALL`] order; 0 = sent as
    /// it was.
    pub outputs: BTreeMap<String, u16>,
    /// The first 12 hex characters of a typed message's key -> a pack was
    /// added after it.
    pub contexts: BTreeMap<String, bool>,
    /// The same key -> the tokens the pack was built to fit in, so it is
    /// built again to the same size. Missing for packs of older versions,
    /// which were built without such a cap.
    pub pack_caps: BTreeMap<String, usize>,
    /// The same keys, for packs no longer inserted after a compaction.
    pub retired: BTreeSet<String>,
    /// The same key -> the hash of the client's items up to and including
    /// the message, when its pack was last inserted (see [`Before::prefix`]).
    /// The same hash again means the history kept its prefix up to the
    /// message, after a restart too. Missing for packs of older versions.
    pub anchors: BTreeMap<String, u64>,
    /// A copy of a message typed earlier in the same request gets no pack.
    /// False in a ledger of an older version, where a copy got its text's
    /// pack: such a conversation keeps doing so, so its bytes replay.
    #[serde(default)]
    pub no_pack_for_copies: bool,
}

impl Default for Ledger {
    /// The ledger of a new conversation.
    fn default() -> Self {
        Ledger {
            outputs: BTreeMap::new(),
            contexts: BTreeMap::new(),
            pack_caps: BTreeMap::new(),
            retired: BTreeSet::new(),
            anchors: BTreeMap::new(),
            no_pack_for_copies: true,
        }
    }
}

/// What was decided for one tool output, replayed on every later turn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Decision {
    /// What to send instead; `None` = send as is.
    pub text: Option<String>,
    /// The rule whose result was sent last in the chain.
    pub rule: Option<RuleId>,
    /// A lossy rule's result is in `text`.
    pub lossy: bool,
}

/// Cache breaks seen in a conversation: requests whose forwarded prefix
/// changed while the client's did not (caused by lge, a bug), and requests
/// whose client prefix changed (an edit, a compaction).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct PrefixStats {
    pub lge_breaks: u64,
    pub client_breaks: u64,
    /// The input item at which lge first broke the prefix.
    pub first_lge_break_item: Option<usize>,
}

/// Opening lines of every injected context. Fixed text: it is part of the
/// cached prompt of every later turn.
pub const CONTEXT_OPEN: &str = "<lge_context>\nCode-graph context for the request above, added by the local graph engine, not written by the user. \
It was worked out from the source: use its conclusions instead of re-deriving them, start from these locations instead of searching, \
and read only the line ranges you need.\n\n";
/// Tokens of remembered facts added to one context.
const MEMORY_BUDGET: usize = 300;
/// Remembered facts are added only when at least this many tokens fit.
const MIN_MEMORY_TOKENS: usize = 50;
/// The heading of the remembered facts in a pack.
const MEMORY_HEADING: &str = "\n\n## From project memory (decisions, conventions, recent commits)\n";
pub const CONTEXT_CLOSE: &str = "</lge_context>";

/// What a later call must do to count as getting back what an item left out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Facts {
    /// Line ranges its markers name: (path, first, last).
    ranges: Vec<(String, usize, usize)>,
    /// Files its notes list as left out.
    files: Vec<String>,
    /// The file it is a read of (skeleton, lock file).
    read_file: Option<String>,
}

/// An item a rule shortened, or would have, waiting for its follow-up calls.
#[derive(Debug, Clone)]
struct Pending {
    rule: RuleId,
    shadow: bool,
    /// The call the item is the output of.
    call_id: String,
    /// The normalised producer that printed it, when known.
    producer: Option<String>,
    facts: Facts,
    /// The skeleton cohort of the read.
    cohort: Option<String>,
}

/// One tool call of a request, as the follow-up check reads it.
#[derive(Debug, Clone, Default)]
pub(crate) struct CallView {
    pub call_id: String,
    /// It may have changed files.
    pub edit: bool,
    /// The shell commands it ran: one for a shell call, each of a script's.
    pub commands: Vec<String>,
    /// The paths an `apply_patch` named.
    pub patched: Vec<String>,
}

/// What was done in the current request, so a retry can undo it.
#[derive(Debug, Default)]
struct RequestLog {
    saved_before: Saved,
    prefix_before: (Vec<(u64, u64)>, PrefixStats),
    /// Items judged in this request, with their entry as it was pending.
    judged_added: Vec<(String, Pending)>,
    pending_added: Vec<String>,
    files_added: Vec<String>,
    /// Tokens not sent because of replaced items present in this request.
    removed_present: i64,
    /// Tokens of the context items present in this request.
    added_present: i64,
    /// What this request's packs added to what the conversation is about.
    symbols_added: Vec<String>,
    relevant_files_added: Vec<String>,
    /// Ledger keys of the packs this request retired.
    retired_added: Vec<String>,
    /// Ledger keys whose anchor this request set, with the anchor before.
    anchors_before: Vec<(String, Option<u64>)>,
    /// Starting points of the packs inserted so far in this request: what the
    /// items before the next message already show.
    present_seeds: BTreeSet<String>,
}

/// What comes before a typed message in its request: what the decision about
/// its pack may depend on. All of it is read from the items before the
/// message, which never change while the history keeps its prefix.
pub struct Before<'a> {
    /// Tokens of the packs present before the message in the request.
    pub pack_tokens: usize,
    /// True when a repository-relative path appears before the message in a
    /// tool call's arguments or output, or in a pack.
    pub mentions: &'a dyn Fn(&str) -> bool,
    /// The client changed an item at or before the message since the previous
    /// request (a compaction, an edit): the history did not keep its prefix
    /// up to the message, so a pack decided in an earlier request is no
    /// longer inserted, unless the message is `newest` and the pack fits the
    /// conversation budget. A change after the message leaves its pack alone.
    pub rewritten: bool,
    /// A hash of the client's items up to and including the message; `None`
    /// outside a request. A pack decided in an earlier request whose anchor
    /// in the [`Ledger`] differs was moved by a rewrite as well, which a
    /// daemon that restarted sees only this way.
    pub prefix: Option<u64>,
    /// The message is the newest typed message of the request.
    pub newest: bool,
}

/// A pack built for a message, before it is kept.
struct Built {
    /// The pack's text; `None` when it does not fit in the cap.
    text: Option<String>,
    /// Ids and files of its starting points, and the files it names.
    seeds: Vec<String>,
    seed_files: Vec<String>,
    files: Vec<String>,
}

/// True when `path` appears in `text` as a whole path: not as the end of a
/// longer name (`data.py` in `metadata.py`) or the start of one.
pub(crate) fn mentions(text: &str, path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    let name = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.');
    text.match_indices(path).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + path.len()..].chars().next();
        !before.is_some_and(name) && !after.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

#[derive(Default)]
pub struct Session {
    /// Item key -> what to send instead.
    outputs: HashMap<String, Decision>,
    /// Item key -> (key of the earlier copy it points at, the pointer text).
    /// Replayed only while that copy is in the request.
    pointers: HashMap<String, (String, String)>,
    /// Hash of a user message -> the context added after it; `None` = none.
    contexts: HashMap<String, Option<String>>,
    /// Files a skeleton was sent for. The next read of such a file is sent
    /// whole: the reader asked again, so it needs what was left out.
    shortened_files: BTreeSet<String>,
    /// Symbols and files the conversation is about, from the packs so far.
    relevant_symbols: BTreeSet<String>,
    relevant_files: BTreeSet<String>,
    /// Message key -> the starting points of the pack added after it. A
    /// message whose starting points are all in packs inserted before it in
    /// the request gets none: the same pack is not added twice.
    pack_seeds: HashMap<String, Vec<String>>,
    /// Words of what the user typed, lower-cased.
    words: BTreeSet<String>,
    /// Items sent whole because they re-ran a shortened command with nothing
    /// changed in between.
    raw_calls: BTreeSet<String>,
    /// Item key -> tokens it removes from every request it is in.
    removed: HashMap<String, i64>,
    /// `{rule}:{item key}` -> the item, until it is judged.
    pending: BTreeMap<String, Pending>,
    /// `{rule}:{item key}` of the items judged.
    judged: BTreeSet<String>,
    /// (client hash, forwarded hash) of each input item of the previous request.
    prefix_pairs: Vec<(u64, u64)>,
    pub prefix: PrefixStats,
    /// Keys first decided in the current request (outputs, pointers, contexts).
    new_this_request: Vec<String>,
    request: RequestLog,
    pub saved: Saved,
    /// Every item's decision by hash, for a restart (see [`Ledger`]).
    ledger: Ledger,
    /// Packs added again from the ledger after a restart: not counted again,
    /// but they count towards [`Options::max_contexts`].
    pinned_contexts: usize,
}

fn key(parts: &[&str]) -> String {
    let mut bytes = Vec::new();
    for p in parts {
        bytes.extend_from_slice(p.as_bytes());
        bytes.push(0);
    }
    content_hash(&bytes)
}

/// The key of the output `text` of the item `id` (a call id, and its part)
/// whose command is `command`.
pub(crate) fn output_key(id: &str, command: &str, text: &str) -> String {
    key(&["output", id, command, text])
}

/// The key of an output judged by content alone.
fn content_key(text: &str, status: RunStatus, minify: bool) -> String {
    let status = format!("{:?}:{}", status.exit, status.running);
    key(&["text", if minify { "tool" } else { "shell" }, &status, text])
}

/// The string two calls share when they ran the same producer, whatever was
/// piped after it.
pub(crate) fn producer_of(n: &Normalised) -> String {
    if n.single {
        let mut out = n.program.clone();
        for a in &n.args {
            out.push('\u{1f}');
            out.push_str(a);
        }
        out
    } else {
        n.command.clone()
    }
}

/// Tokens of the pack for a message whose starting points are all in one file
/// that nothing else uses: beyond that file there is little to say.
pub const LOCAL_CONTEXT_BUDGET: usize = 600;

/// The budget of the pack for `user_text`: [`LOCAL_CONTEXT_BUDGET`] when every
/// starting point the pack would use is in one file and nothing outside that
/// file depends on them (as far as the pack would look), the configured
/// budget otherwise. A smaller configured budget is never raised. Decided from
/// the graph alone: the same graph and message give the same budget.
pub fn pack_budget(store: &dyn GraphStore, user_text: &str, opts: &Options) -> usize {
    if only_one_file(store, user_text).unwrap_or(false) {
        opts.context_budget.min(LOCAL_CONTEXT_BUDGET)
    } else {
        opts.context_budget
    }
}

/// True when the pack's starting points for `user_text` are all in one file
/// and every symbol that depends on them is in that file too. Anything else
/// that depends on them (an endpoint, a table, a queue), an unresolved edge
/// from another file, or a walk cut short, means it is not.
fn only_one_file(store: &dyn GraphStore, user_text: &str) -> Option<bool> {
    let pack = PackOptions::default();
    let seeds = find_seeds(
        store,
        user_text,
        &SeedOptions {
            max_seeds: pack.max_seeds,
            service: pack.service.clone(),
            ..SeedOptions::default()
        },
    )
    .ok()?;
    let file = seeds.first()?.hit.file_path.clone();
    if file.is_empty() || seeds.iter().any(|s| s.hit.file_path != file) {
        return Some(false);
    }
    let ids: Vec<String> = seeds.iter().map(|s| s.hit.id.clone()).collect();
    let users = dependents_of(
        store,
        &ids,
        &WalkOptions {
            depth: pack.depth_used_by,
            max_nodes: pack.max_nodes,
            ..WalkOptions::default()
        },
    )
    .ok()?;
    Some(
        !users.truncated
            && users
                .reached
                .iter()
                .all(|r| r.node.label == NodeLabel::Symbol && r.node.file_path == file)
            && users.unresolved.iter().all(|e| e.evidence_file == file),
    )
}

/// A repository-relative path for what a command named, if it is inside the root.
fn relative(root: &Path, cwd: Option<&Path>, path: &str) -> Option<String> {
    let p = Path::new(path);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.unwrap_or(root).join(p)
    };
    let abs = abs.canonicalize().ok()?;
    let root = root.canonicalize().ok()?;
    Some(abs.strip_prefix(&root).ok()?.to_string_lossy().replace('\\', "/"))
}

/// The code of a numbered read (`nl -ba`, `cat -n`) without the number
/// prefixes, when every line has one.
fn strip_numbers(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let spaces = line.len() - line.trim_start_matches(' ').len();
        let digits = line[spaces..].bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 || line.as_bytes().get(spaces + digits) != Some(&b'\t') {
            return None;
        }
        out.push_str(&line[spaces + digits + 1..]);
        out.push('\n');
    }
    Some(out)
}

/// Two paths as commands wrote them name the same file: equal without a
/// leading `./`, or one is the other with a folder in front.
pub(crate) fn same_path(a: &str, b: &str) -> bool {
    let a = a.trim_start_matches("./");
    let b = b.trim_start_matches("./");
    if a.is_empty() || b.is_empty() {
        return false;
    }
    a == b || a.ends_with(&format!("/{b}")) || b.ends_with(&format!("/{a}"))
}

/// The ranges and files a shortened text names as the way back.
fn facts_of(text: &str) -> Facts {
    static SED: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"sed -n '(\d+)(?:,(\d+))?p' ([A-Za-z0-9._/\-]+|'(?:[^']|'\\'')*')").expect("valid regex")
    });
    static NOTE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?m)^(.+): \(\+\d+ more matching lines in this file\)$").expect("valid regex"));
    static ENTRY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(.+?) \(\d+\)(?:, |$)").expect("valid regex"));
    let mut facts = Facts::default();
    for c in SED.captures_iter(text) {
        let (Some(a), Some(path)) = (c.get(1).and_then(|m| m.as_str().parse().ok()), c.get(3)) else {
            continue;
        };
        let b = c.get(2).and_then(|m| m.as_str().parse().ok()).unwrap_or(a);
        let path = path.as_str();
        let path = match path.strip_prefix('\'').and_then(|p| p.strip_suffix('\'')) {
            Some(quoted) => quoted.replace("'\\''", "'"),
            None => path.to_string(),
        };
        facts.ranges.push((path, a, b));
    }
    for c in NOTE.captures_iter(text) {
        facts.files.push(c[1].to_string());
    }
    for line in text.lines().filter(|l| l.starts_with("lge: ")) {
        let Some(at) = line.find(" Not shown: ") else { continue };
        let list = &line[at + " Not shown: ".len()..];
        let list = list.find(". Narrow").map_or(list, |end| &list[..end]);
        let list = list.find(", and ").map_or(list, |end| &list[..end]);
        for c in ENTRY.captures_iter(list) {
            facts.files.push(c[1].to_string());
        }
    }
    facts
}

/// One tool output decided for the first time: what to send, and everything
/// to count about it once it is kept.
#[derive(Debug, Default)]
pub(crate) struct Draft {
    pub text: Option<String>,
    pub rule: Option<RuleId>,
    pub lossy: bool,
    original_tokens: Option<usize>,
    /// Rules whose result is in `text`: (rule, tokens before, tokens after).
    on: Vec<(RuleId, usize, usize)>,
    /// Rules that would have changed it in shadow.
    shadow: Vec<(RuleId, usize, usize)>,
    /// Items to watch for a recovery: (rule, in shadow, what counts).
    follow: Vec<(RuleId, bool, Facts)>,
    /// A skeleton was sent for this repository-relative file.
    shortened_file: Option<String>,
    /// The skeleton cohort of an eligible first read, and what it would save.
    cohort: Option<(String, u64)>,
    condensed: bool,
    raw_call: bool,
    /// Decided again from the ledger after a restart: counted before, so not
    /// counted, watched or reverted again.
    pub pinned: bool,
}

impl Draft {
    pub(crate) fn original_tokens(&mut self, original: &str) -> usize {
        *self.original_tokens.get_or_insert_with(|| count(original))
    }

    /// Count what `rule` would have sent in place of `original`.
    pub(crate) fn add_shadow(&mut self, rule: RuleId, original: &str, would: &str) {
        let before = self.original_tokens(original);
        self.shadow.push((rule, before, count(would)));
        if rule.is_lossy() || rule.measure_only() {
            self.follow.push((rule, true, facts_of(would)));
        }
    }
}

/// The chain of rules over one output: each `on` rule's result is the next
/// one's input; a `shadow` rule is counted and changes nothing.
struct Chain<'o> {
    opts: &'o Options,
    original: &'o str,
    /// The text after the `on` rules so far, and its tokens; `None` = the original.
    current: Option<(String, usize)>,
    /// The text right after the inert pre-pass, when it fired.
    inert_only: Option<(String, usize)>,
    draft: Draft,
}

impl<'o> Chain<'o> {
    fn new(opts: &'o Options, original: &'o str) -> Self {
        Chain {
            opts,
            original,
            current: None,
            inert_only: None,
            draft: Draft::default(),
        }
    }

    fn input(&self) -> &str {
        self.current.as_ref().map_or(self.original, |(t, _)| t.as_str())
    }

    fn input_tokens(&mut self) -> usize {
        match &self.current {
            Some((_, n)) => *n,
            None => self.draft.original_tokens(self.original),
        }
    }

    /// Run `rule` on the current text. Returns its mode when it produced a
    /// result (`On`: the result is now the current text; `Shadow`: counted).
    fn stage(&mut self, rule: RuleId, f: impl FnOnce(&str) -> Option<String>) -> Option<Mode> {
        let mode = self.opts.mode(rule);
        if mode == Mode::Off {
            return None;
        }
        let out = f(self.input())?;
        let before = self.input_tokens();
        let after = count(&out);
        let facts = (rule.is_lossy() || rule.measure_only()).then(|| facts_of(&out));
        match mode {
            Mode::On => {
                self.draft.on.push((rule, before, after));
                if let Some(facts) = facts {
                    self.draft.follow.push((rule, false, facts));
                }
                if rule == RuleId::Inert && self.current.is_none() {
                    self.inert_only = Some((out.clone(), after));
                }
                self.current = Some((out, after));
            }
            _ => {
                self.draft.shadow.push((rule, before, after));
                if let Some(facts) = facts {
                    self.draft.follow.push((rule, true, facts));
                }
            }
        }
        Some(mode)
    }

    /// Add facts to the follow-up entry of the last stage of `rule`.
    fn amend(&mut self, rule: RuleId, f: impl FnOnce(&mut Facts)) {
        if let Some((_, _, facts)) = self.draft.follow.iter_mut().rev().find(|(r, _, _)| *r == rule) {
            f(facts);
        }
    }

    /// The decision. `accept` judges the final text when a rule other than
    /// the inert pre-pass changed it; inert alone is sent only when it is
    /// worth it. A refused final text falls back to inert alone, then to the
    /// original. Rules whose result is not sent are not counted.
    fn finish(mut self, accept: impl FnOnce(&str) -> bool) -> Draft {
        let main = self.draft.on.iter().any(|(r, _, _)| *r != RuleId::Inert);
        let sent = match self.current.take() {
            Some((text, _)) if main && accept(&text) => Some(text),
            _ => None,
        };
        let sent = match sent {
            Some(text) => Some(text),
            None => {
                let on_inert = self.draft.on.iter().find(|(r, _, _)| *r == RuleId::Inert).copied();
                self.draft.on.clear();
                match (self.inert_only.take(), on_inert) {
                    (Some((text, _)), Some(stat)) if worth_it(self.original, &text) => {
                        self.draft.on.push(stat);
                        Some(text)
                    }
                    _ => None,
                }
            }
        };
        // Follow-ups only for rules whose result is sent, and every shadow one.
        let on: Vec<RuleId> = self.draft.on.iter().map(|(r, _, _)| *r).collect();
        self.draft.follow.retain(|(r, shadow, _)| *shadow || on.contains(r));
        self.draft.rule = on.last().copied();
        self.draft.lossy = on.iter().any(|r| r.is_lossy());
        self.draft.text = sent;
        self.draft
    }
}

impl Session {
    /// The context to add after a message the user typed, or `None`. Decided
    /// once per distinct message and repeated exactly afterwards.
    ///
    /// A pack is added only when the graph clearly has something for the
    /// message: a symbol the message names, or a search hit that shares at
    /// least two words with it. "ok, do it" gets nothing. In a holdout
    /// conversation the pack is decided and counted, and never added.
    ///
    /// The message is taken to come after every message decided so far, each
    /// once, with their packs before it; [`Session::context_after`] is the
    /// same for a message at its place in a request.
    pub fn context_for(&mut self, graph: &Graph, user_text: &str, opts: &Options) -> Option<String> {
        let present: Vec<(&String, String)> = self
            .contexts
            .iter()
            .filter(|(id, _)| !self.ledger.retired.contains(&ledger_key(id)))
            .filter_map(|(id, text)| Some((id, text.clone()?)))
            .collect();
        let pack_tokens = present.iter().map(|(_, t)| count(t)).sum();
        self.request.present_seeds = present
            .iter()
            .filter_map(|(id, _)| self.pack_seeds.get(*id))
            .flatten()
            .cloned()
            .collect();
        let texts: Vec<String> = present.into_iter().map(|(_, t)| t).collect();
        let mentioned = |path: &str| texts.iter().any(|t| mentions(t, path));
        let before = Before {
            pack_tokens,
            mentions: &mentioned,
            rewritten: false,
            prefix: None,
            newest: true,
        };
        self.context_after(Some(graph), user_text, &before, opts)
    }

    /// [`Session::context_for`] for a message at its place in a request,
    /// whether a graph is ready or not: a message first seen without a graph
    /// gets no pack, for good, so a graph that becomes ready (or goes away)
    /// later never changes what was sent after an old message.
    ///
    /// A pack decided in an earlier request is inserted again with the same
    /// bytes while the history keeps its prefix up to its message. After a
    /// rewrite at or before the message (see [`Before::rewritten`]) it is
    /// retired for good, unless its message is the newest one and it still
    /// fits the conversation budget.
    pub fn context_after(
        &mut self,
        graph: Option<&Graph>,
        user_text: &str,
        before: &Before,
        opts: &Options,
    ) -> Option<String> {
        let id = key(&["context", user_text]);
        let lk = ledger_key(&id);
        let kept = self.context_of(graph, &id, &lk, user_text, before, opts);
        // A held-out conversation counts what it would have added the same way.
        if kept.is_some() || opts.holdout {
            let seeds = self.pack_seeds.get(&id).cloned().unwrap_or_default();
            self.request.present_seeds.extend(seeds);
        }
        kept
    }

    fn context_of(
        &mut self,
        graph: Option<&Graph>,
        id: &str,
        lk: &str,
        user_text: &str,
        before: &Before,
        opts: &Options,
    ) -> Option<String> {
        if let Some(known) = self.contexts.get(id).cloned() {
            return self.keep_or_retire(lk, known?, before, opts);
        }
        let words: BTreeSet<String> = split_words(user_text).into_iter().collect();
        self.words.extend(words.iter().cloned());
        // Decided before the daemon restarted: the same answer, built to the
        // same size, counted once.
        if let Some(&had) = self.ledger.contexts.get(lk) {
            let again = match graph {
                Some(graph) if had && !self.ledger.retired.contains(lk) => {
                    let cap = self.ledger.pack_caps.get(lk).copied();
                    self.rebuild_context(graph, id, user_text, cap, opts)
                }
                _ => None,
            };
            self.contexts.insert(id.to_string(), again.clone());
            self.pinned_contexts += usize::from(again.is_some());
            return self.keep_or_retire(lk, again?, before, opts);
        }
        let decided = graph.and_then(|graph| self.decide_context(graph, id, user_text, &words, before, opts));
        let kept = match decided {
            Some((text, _)) if opts.holdout => {
                self.saved.contexts_would_add += 1;
                self.saved.context_tokens_would_add += count(&text);
                None
            }
            Some((text, cap)) => {
                let tokens = count(&text);
                self.saved.contexts_added += 1;
                self.saved.context_tokens += tokens;
                self.request.added_present += tokens as i64;
                self.ledger.pack_caps.insert(lk.to_string(), cap);
                if let Some(prefix) = before.prefix {
                    self.ledger.anchors.insert(lk.to_string(), prefix);
                }
                Some(text)
            }
            None => None,
        };
        self.ledger.contexts.insert(lk.to_string(), kept.is_some());
        self.contexts.insert(id.to_string(), kept.clone());
        self.new_this_request.push(id.to_string());
        kept
    }

    /// A pack decided in an earlier request, inserted again as it was while
    /// the history keeps its prefix up to its message. When the client
    /// rewrote an item at or before the message (a compaction, an edit), or
    /// the hash of the items up to the message is not the one the pack was
    /// last inserted with, only the newest typed message keeps its pack, and
    /// only while it fits the conversation budget; every other pack is
    /// retired for good. Those items are not cached anyway; the items before
    /// the rewrite keep every byte.
    fn keep_or_retire(&mut self, lk: &str, text: String, before: &Before, opts: &Options) -> Option<String> {
        if self.ledger.retired.contains(lk) {
            return None;
        }
        let anchor = self.ledger.anchors.get(lk).copied();
        let moved = matches!((anchor, before.prefix), (Some(a), Some(p)) if a != p);
        let tokens = count(&text);
        if (before.rewritten || moved)
            && !(before.newest && before.pack_tokens + tokens <= opts.pack_conversation_budget)
        {
            self.ledger.retired.insert(lk.to_string());
            self.saved.contexts_retired += 1;
            self.request.retired_added.push(lk.to_string());
            return None;
        }
        // Inserted with the items before it as they are now.
        if let Some(prefix) = before.prefix.filter(|p| anchor != Some(*p)) {
            self.request.anchors_before.push((lk.to_string(), anchor));
            self.ledger.anchors.insert(lk.to_string(), prefix);
        }
        self.request.added_present += tokens as i64;
        Some(text)
    }

    /// The pack for a typed message seen for the first time, and the tokens
    /// it was built to fit in.
    ///
    /// Packs are sent again on every later request, so their cost grows with
    /// the conversation. The packs present in one request never exceed
    /// [`Options::pack_conversation_budget`] together: a new pack gets at most
    /// the room left, and none with less than [`MIN_PACK_ROOM`]. After the
    /// conversation's first pack, a pack is at most [`FOLLOW_UP_PACK_TOKENS`]
    /// and is added only when one of its starting points is in a file that no
    /// tool call, tool output or pack before the message named.
    fn decide_context(
        &mut self,
        graph: &Graph,
        id: &str,
        user_text: &str,
        words: &BTreeSet<String>,
        before: &Before,
        opts: &Options,
    ) -> Option<(String, usize)> {
        let packs = self.saved.contexts_added + self.saved.contexts_would_add + self.pinned_contexts;
        if !opts.inject_context || packs >= opts.max_contexts || user_text.len() > 8000 {
            return None;
        }
        let seeds = find_seeds(graph.store, user_text, &SeedOptions::default()).ok()?;
        let strong = seeds.iter().any(|s| {
            s.exact || {
                let name_words: BTreeSet<String> =
                    split_words(&format!("{} {}", s.hit.qualified_name, s.hit.file_path))
                        .into_iter()
                        .collect();
                name_words.intersection(words).count() >= 2
            }
        });
        if !strong {
            return None;
        }
        // Nothing new to say: the packs before the message give these starting
        // points already. Read from the packs inserted before it in the
        // request, so a pack retired or dropped from the history counts as
        // not given, after a restart the same.
        if seeds.iter().all(|s| self.request.present_seeds.contains(&s.hit.id)) {
            self.saved.contexts_known += 1;
            return None;
        }
        let room = opts.pack_conversation_budget.saturating_sub(before.pack_tokens);
        if room < MIN_PACK_ROOM {
            self.saved.contexts_over_budget += 1;
            return None;
        }
        let follow_up = self.ledger.contexts.values().any(|added| *added) || self.saved.contexts_would_add > 0;
        let cap = if follow_up {
            room.min(FOLLOW_UP_PACK_TOKENS)
        } else {
            room
        };
        let built = self.build_pack(graph, user_text, opts, Some(cap))?;
        if follow_up && !built.seed_files.iter().any(|f| !f.is_empty() && !(before.mentions)(f)) {
            self.saved.contexts_known += 1;
            return None;
        }
        let Some(text) = built.text.clone() else {
            self.saved.contexts_over_budget += 1;
            return None;
        };
        self.remember_pack(id, &built);
        Some((text, cap))
    }

    /// The pack of a message that got one before the daemon restarted, built
    /// to the size it had then: the switch, the cap and the checks, which it
    /// passed then, are not asked again.
    fn rebuild_context(
        &mut self,
        graph: &Graph,
        id: &str,
        user_text: &str,
        cap: Option<usize>,
        opts: &Options,
    ) -> Option<String> {
        if user_text.len() > 8000 {
            return None;
        }
        let built = self.build_pack(graph, user_text, opts, cap)?;
        let text = built.text.clone()?;
        self.remember_pack(id, &built);
        Some(text)
    }

    /// Build the pack for `user_text` in at most `cap` tokens in all, its
    /// frame and remembered facts included; `None` for no cap (packs of
    /// older versions). Remembered facts are left out when they do not fit.
    /// The same graph, facts and cap give the same bytes.
    fn build_pack(&self, graph: &Graph, user_text: &str, opts: &Options, cap: Option<usize>) -> Option<Built> {
        let frame = count(&format!("{CONTEXT_OPEN}\n{CONTEXT_CLOSE}"));
        let mut budget = pack_budget(graph.store, user_text, opts);
        if let Some(cap) = cap {
            budget = budget.min(cap.saturating_sub(frame));
        }
        let pack = evidence_pack(
            graph.store,
            user_text,
            &PackOptions {
                budget,
                ..PackOptions::default()
            },
        )
        .ok()?;
        if pack.seeds.is_empty() {
            return None;
        }
        let body = pack.text.trim_end();
        let bare = format!("{CONTEXT_OPEN}{body}\n{CONTEXT_CLOSE}");
        // Decisions and conventions stored in earlier sessions: the model uses
        // them instead of working them out again.
        let memory_budget = match cap {
            None => MEMORY_BUDGET,
            Some(cap) => MEMORY_BUDGET.min(cap.saturating_sub(count(&bare) + count(MEMORY_HEADING))),
        };
        let mut text = None;
        if memory_budget >= MIN_MEMORY_TOKENS {
            let remembered = lge_memory::recall(
                graph.store,
                user_text,
                &lge_memory::RecallOptions {
                    budget: memory_budget,
                    now: lge_retrieve::unix_now(),
                    limit: 8,
                    ..lge_memory::RecallOptions::default()
                },
                None,
            );
            if let Ok(found) = remembered {
                if !found.items.is_empty() {
                    text = Some(format!(
                        "{CONTEXT_OPEN}{body}{MEMORY_HEADING}{}\n{CONTEXT_CLOSE}",
                        found.text.trim_end()
                    ));
                }
            }
        }
        let fits = |t: &str| cap.is_none_or(|cap| count(t) <= cap);
        let text = match text {
            Some(with_memory) if fits(&with_memory) => Some(with_memory),
            _ => Some(bare).filter(|t| fits(t)),
        };
        Some(Built {
            text,
            seeds: pack.seeds.iter().map(|s| s.id.clone()).collect(),
            seed_files: pack.seeds.iter().map(|s| s.file_path.clone()).collect(),
            files: pack.files,
        })
    }

    /// What a pack that is kept adds to what the conversation is about, and
    /// the starting points of the message `id`'s pack.
    fn remember_pack(&mut self, id: &str, built: &Built) {
        self.pack_seeds.insert(id.to_string(), built.seeds.clone());
        for seed in &built.seeds {
            if self.relevant_symbols.insert(seed.clone()) {
                self.request.symbols_added.push(seed.clone());
            }
        }
        for file in &built.files {
            if self.relevant_files.insert(file.clone()) {
                self.request.relevant_files_added.push(file.clone());
            }
        }
    }

    /// What to send in place of the output of a shell command, or `None` to
    /// send it as it is. `cwd` is the directory the command ran in. Decided
    /// once per distinct (command, output) and repeated exactly afterwards.
    pub fn rewrite_output(
        &mut self,
        graph: Option<&Graph>,
        command: &str,
        cwd: Option<&Path>,
        output: &str,
        opts: &Options,
    ) -> Option<String> {
        self.rewrite_call_output(graph, "", command, cwd, output, opts)
    }

    /// [`Session::rewrite_output`] for the output of one tool call, named by
    /// `call` (its call id). Decided once per call: the same call gets the same
    /// bytes on every turn, whatever the options say by then, while a second
    /// call with the same command and output is decided on its own (a file
    /// read again is sent whole).
    pub fn rewrite_call_output(
        &mut self,
        graph: Option<&Graph>,
        call: &str,
        command: &str,
        cwd: Option<&Path>,
        output: &str,
        opts: &Options,
    ) -> Option<String> {
        let id = output_key(call, command, output);
        if let Some(known) = self.replay(&id, &|_| false) {
            return known;
        }
        let pinned = self.pinned(&id).map(|mask| opts.pinned(mask));
        let mut draft = self.draft_known(
            graph,
            command,
            cwd,
            output,
            RunStatus::default(),
            pinned.as_ref().unwrap_or(opts),
            false,
        );
        draft.pinned = pinned.is_some();
        let producer = normalise(command).map(|n| producer_of(&n));
        self.commit(id, draft, output, call, producer)
    }

    /// What to send in place of a tool output whose command is not known (a
    /// script, a tool the proxy cannot read), judged by its content alone: a
    /// recognised test or lint run, or a long list of numbered search matches.
    /// A recognised run is never also taken for a search. Decided once per
    /// distinct text and repeated exactly afterwards.
    pub fn rewrite_text(&mut self, output: &str, opts: &Options) -> Option<String> {
        self.decide_content("", output, RunStatus::default(), false, opts)
    }

    /// Start a request: forget which items were new in the last one, and keep
    /// what is needed to undo this one ([`Session::revert_new`]).
    /// [`crate::responses::transform`] calls it first.
    pub fn begin_request(&mut self) {
        self.new_this_request.clear();
        self.request = RequestLog {
            saved_before: self.saved.clone(),
            prefix_before: (self.prefix_pairs.clone(), self.prefix.clone()),
            ..RequestLog::default()
        };
    }

    /// Keys of the items first decided in the current request.
    pub fn new_this_request(&self) -> &[String] {
        &self.new_this_request
    }

    /// Items first decided in the current request whose decision sends other
    /// bytes than the client's: a replaced output, a pointer, an added context.
    /// A retry without them would send a different body.
    pub fn new_changes(&self) -> usize {
        self.new_this_request
            .iter()
            .filter(|id| {
                self.outputs.get(*id).is_some_and(|d| d.text.is_some())
                    || self.pointers.contains_key(*id)
                    || self.contexts.get(*id).is_some_and(Option::is_some)
            })
            .count()
    }

    /// The client hashes of the previous request's input items: an item whose
    /// hash is not among them is new in the current request.
    pub(crate) fn previous_client_hashes(&self) -> std::collections::HashSet<u64> {
        self.prefix_pairs.iter().map(|(client, _)| *client).collect()
    }

    /// Undo the current request's new decisions: each item first decided in
    /// it is sent as it is from now on, for good, and every counter goes back
    /// to what it was before the request. Decisions made in earlier requests
    /// are kept. For a retry of a request the provider refused.
    pub fn revert_new(&mut self) {
        for id in std::mem::take(&mut self.new_this_request) {
            if let Some(context) = self.contexts.get_mut(&id) {
                *context = None;
                self.ledger.contexts.insert(ledger_key(&id), false);
                self.ledger.pack_caps.remove(&ledger_key(&id));
                self.ledger.anchors.remove(&ledger_key(&id));
                self.pack_seeds.remove(&id);
                continue;
            }
            self.pointers.remove(&id);
            self.removed.remove(&id);
            self.ledger.outputs.insert(ledger_key(&id), 0);
            self.outputs.insert(id, Decision::default());
        }
        let log = std::mem::take(&mut self.request);
        for symbol in &log.symbols_added {
            self.relevant_symbols.remove(symbol);
        }
        for file in &log.relevant_files_added {
            self.relevant_files.remove(file);
        }
        for (k, pending) in &log.judged_added {
            self.judged.remove(k);
            self.pending.insert(k.clone(), pending.clone());
        }
        for k in &log.pending_added {
            self.pending.remove(k);
        }
        for f in &log.files_added {
            self.shortened_files.remove(f);
        }
        // The retry decides them again, the same way.
        for k in &log.retired_added {
            self.ledger.retired.remove(k);
        }
        for (k, anchor) in log.anchors_before.iter().rev() {
            match anchor {
                Some(a) => self.ledger.anchors.insert(k.clone(), *a),
                None => self.ledger.anchors.remove(k),
            };
        }
        self.saved = log.saved_before.clone();
        (self.prefix_pairs, self.prefix) = log.prefix_before.clone();
        self.request = RequestLog {
            saved_before: log.saved_before,
            prefix_before: log.prefix_before,
            ..RequestLog::default()
        };
    }

    /// The decision for an item decided before, or `None` for a new item. A
    /// pointer is replayed only while `present` says its earlier copy is in
    /// the request; otherwise the item is decided again.
    pub(crate) fn replay(&mut self, id: &str, present: &dyn Fn(&str) -> bool) -> Option<Option<String>> {
        if let Some(d) = self.outputs.get(id) {
            if d.text.is_some() {
                self.request.removed_present += self.removed.get(id).copied().unwrap_or(0);
            }
            return Some(d.text.clone());
        }
        let (target, text) = self.pointers.get(id)?;
        if !present(target) {
            return None;
        }
        let text = text.clone();
        self.request.removed_present += self.removed.get(id).copied().unwrap_or(0);
        Some(Some(text))
    }

    /// True when the item's decision sent a lossy rule's result.
    pub(crate) fn lossy(&self, id: &str) -> bool {
        self.outputs.get(id).is_some_and(|d| d.lossy)
    }

    /// The first decision for the output `text` of `command`, before the
    /// repeat pointer and the measured deltas (which need the request):
    /// steps b to g of the decision. `recovery`: the command re-runs a
    /// shortened one with nothing changed in between, so it is sent whole.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn draft_known(
        &self,
        graph: Option<&Graph>,
        command: &str,
        cwd: Option<&Path>,
        text: &str,
        status: RunStatus,
        opts: &Options,
        recovery: bool,
    ) -> Draft {
        if !opts.shorten_outputs {
            return Draft::default();
        }
        let p = purpose(command);
        if p == Purpose::Raw {
            return Draft::default();
        }
        if recovery {
            return Draft {
                raw_call: true,
                ..Draft::default()
            };
        }
        let norm = normalise(command);
        // The status the shell reports may not be the producer's.
        let status = match &norm {
            Some(n) if n.exit_masked => RunStatus {
                exit: None,
                masked: true,
                ..status
            },
            _ => status,
        };
        let mut chain = Chain::new(opts, text);
        // Reads, patches and machine formats never get the inert pre-pass:
        // their bytes are what the model edits against or parses.
        let pre = !matches!(
            p,
            Purpose::ReadWhole { .. } | Purpose::ReadRange { .. } | Purpose::Diff | Purpose::Json
        );
        // Output RTK condensed already gets only the lossless clean-up. A file
        // that merely contains RTK's markers is still read as a file.
        if p == Purpose::Condensed || (pre && condensed_upstream(text)) {
            chain.stage(RuleId::Inert, inert);
            let mut draft = chain.finish(|_| true);
            draft.condensed = true;
            return draft;
        }
        if pre {
            chain.stage(RuleId::Inert, inert);
        }
        let mut accept_search = false;
        match &p {
            Purpose::Check { tool, detail: false } => {
                chain.stage(RuleId::Check, |t| check_run(tool, tool, t, status).shorter());
            }
            // The agent asked for the program's own prints: they stay.
            Purpose::Check { detail: true, .. } => {}
            Purpose::Search(args) => {
                accept_search = true;
                chain.stage(RuleId::LineClip, |t| clip_match_lines(t, args));
                let files = &self.relevant_files;
                chain.stage(RuleId::SearchCap, |t| {
                    cap_search(t, &|path| files.contains(path.trim_start_matches("./")))
                });
            }
            Purpose::Paths => {
                chain.stage(RuleId::PathFold, crate::fold::fold_paths);
            }
            Purpose::Json => {
                chain.stage(RuleId::JsonMinify, minify_json);
            }
            Purpose::ReadWhole { path, numbered } => {
                if !numbered && is_lockfile(path) {
                    if chain
                        .stage(RuleId::Lockfile, |t| crate::lockfile::lockfile_summary(path, t))
                        .is_some()
                    {
                        chain.amend(RuleId::Lockfile, |f| {
                            f.files.push(path.clone());
                            f.read_file = Some(path.clone());
                        });
                    }
                } else {
                    self.skeleton_stage(&mut chain, graph, path, cwd, *numbered);
                }
            }
            // A range that starts at the top is looking around; a range further
            // down is fetching known lines and is never touched.
            Purpose::ReadRange {
                path,
                from_top: true,
                numbered,
                ..
            } => self.skeleton_stage(&mut chain, graph, path, cwd, *numbered),
            Purpose::ReadRange { .. } | Purpose::Diff => {}
            // A runner the proxy does not know (`npm test`, `make check`) may still
            // print a test or lint run it recognises: judge that by its content.
            // Progress lines go only when the command certainly succeeded.
            Purpose::Other => {
                let decision = check_run("", "", chain.input(), status);
                let not_a_check = decision == CheckDecision::NotACheck;
                chain.stage(RuleId::Check, |_| decision.shorter());
                if not_a_check && status.exit == Some(0) {
                    if let Some(n) = &norm {
                        chain.stage(RuleId::Noise, |t| crate::noise::apply(n, t).map(|(_, out)| out));
                    }
                }
            }
            Purpose::Condensed | Purpose::Raw => {}
        }
        let original = text;
        chain.finish(|final_text| !accept_search || worth_it(original, final_text))
    }

    /// The skeleton of a first read of an indexed file, in its mode, and its
    /// cohort.
    fn skeleton_stage(&self, chain: &mut Chain, graph: Option<&Graph>, path: &str, cwd: Option<&Path>, numbered: bool) {
        if chain.opts.mode(RuleId::Skeleton) == Mode::Off {
            return;
        }
        let Some(graph) = graph else { return };
        let Some((rel, would)) = self.file_skeleton(graph, path, cwd, chain.original, numbered) else {
            return;
        };
        let cohort = cohort_key(&rel, chain.original.len());
        let saving = would.as_ref().map_or(0, |w| {
            let before = chain.draft.original_tokens(chain.original);
            before.saturating_sub(count(w)) as u64
        });
        if let Some(c) = &cohort {
            chain.draft.cohort = Some((c.clone(), saving));
        }
        if chain.stage(RuleId::Skeleton, |_| would) == Some(Mode::On) {
            chain.draft.shortened_file = Some(rel.clone());
        }
        chain.amend(RuleId::Skeleton, |f| f.read_file = Some(rel));
    }

    /// For a read of `path` that may get a skeleton: its repository-relative
    /// path and the skeleton, when the read is eligible (inside the root, not
    /// shortened before, indexed with the content on disk, the output exactly
    /// the file's lines from the top). The skeleton is `None` when nothing is
    /// worth leaving out.
    fn file_skeleton(
        &self,
        graph: &Graph,
        path: &str,
        cwd: Option<&Path>,
        output: &str,
        numbered: bool,
    ) -> Option<(String, Option<String>)> {
        let rel = relative(graph.root, cwd, path)?;
        // Asked for again: the reader needs what the skeleton left out.
        if self.shortened_files.contains(&rel) {
            return None;
        }
        // Line numbers in the graph are only right for the indexed content.
        let on_disk = std::fs::read(graph.root.join(&rel)).ok()?;
        let info = graph.store.file_info(&rel).ok()??;
        if info.content_hash != content_hash(&on_disk) {
            return None;
        }
        // The output must be the file's own lines from the top, nothing else
        // (no wrapper text): otherwise ranges would not match.
        let code = if numbered {
            strip_numbers(output)?
        } else {
            output.to_string()
        };
        let file_text = String::from_utf8_lossy(&on_disk);
        if code.trim().is_empty() || !file_text.starts_with(code.trim_end_matches('\n')) {
            return None;
        }
        let symbols: Vec<NodeRec> = graph.store.symbols_in_file(&rel).ok()?;
        let relevant = &self.relevant_symbols;
        let words = &self.words;
        let keep = |s: &NodeRec| relevant.contains(&s.id) || split_words(&s.name).iter().all(|w| words.contains(w));
        let short = skeleton(&rel, output, 1, &symbols, &keep, numbered);
        Some((rel, short))
    }

    /// The first decision for an output judged by its content alone: terminal
    /// clean-up, a recognised check run, a list of numbered search matches,
    /// and (for a tool that is not a shell or a file reader) JSON minified.
    pub(crate) fn draft_content(&self, text: &str, status: RunStatus, minify: bool, opts: &Options) -> Draft {
        if !opts.shorten_outputs {
            return Draft::default();
        }
        let mut chain = Chain::new(opts, text);
        chain.stage(RuleId::Inert, terminal_only);
        if condensed_upstream(text) {
            let mut draft = chain.finish(|_| true);
            draft.condensed = true;
            return draft;
        }
        let decision = check_run("", "", chain.input(), status);
        let not_a_check = decision == CheckDecision::NotACheck;
        let checked = chain.stage(RuleId::Check, |_| decision.shorter());
        if not_a_check {
            let files = &self.relevant_files;
            let capped = chain.stage(RuleId::SearchCap, |t| {
                cap_numbered_search(t, &|path| files.contains(path.trim_start_matches("./")))
            });
            if minify && checked.is_none() && capped.is_none() {
                chain.stage(RuleId::JsonMinify, minify_json);
            }
        }
        chain.finish(|_| true)
    }

    /// [`Session::draft_content`] decided once per distinct text and status,
    /// and replayed afterwards. `call` is the call the output belongs to.
    pub(crate) fn decide_content(
        &mut self,
        call: &str,
        text: &str,
        status: RunStatus,
        minify: bool,
        opts: &Options,
    ) -> Option<String> {
        let id = content_key(text, status, minify);
        if let Some(known) = self.replay(&id, &|_| false) {
            return known;
        }
        let pinned = self.pinned(&id).map(|mask| opts.pinned(mask));
        let mut draft = self.draft_content(text, status, minify, pinned.as_ref().unwrap_or(opts));
        draft.pinned = pinned.is_some();
        self.commit(id, draft, text, call, None)
    }

    /// Keep a first decision: count it, cache it, and watch it for follow-ups.
    pub(crate) fn commit(
        &mut self,
        id: String,
        mut draft: Draft,
        original: &str,
        call: &str,
        producer: Option<String>,
    ) -> Option<String> {
        let redo = self.pointers.remove(&id).is_some();
        let text = draft.text.take();
        if draft.pinned {
            // Counted before the restart; a skeleton still makes the next read whole.
            if let Some(file) = draft.shortened_file.take() {
                self.shortened_files.insert(file);
            }
        } else {
            if !redo {
                self.count_draft(&mut draft, original, text.as_deref(), &id, call, producer);
            }
            let mask = if text.is_some() {
                draft.on.iter().fold(0, |m, (rule, _, _)| m | rule_bit(*rule))
            } else {
                0
            };
            self.ledger.outputs.insert(ledger_key(&id), mask);
        }
        if let Some(t) = &text {
            let removed = draft.original_tokens(original) as i64 - count(t) as i64;
            self.removed.insert(id.clone(), removed);
            self.request.removed_present += removed;
        }
        if draft.raw_call {
            self.raw_calls.insert(id.clone());
        }
        self.outputs.insert(
            id.clone(),
            Decision {
                text: text.clone(),
                rule: draft.rule,
                lossy: draft.lossy,
            },
        );
        if !draft.pinned {
            self.new_this_request.push(id);
        }
        text
    }

    /// Keep a first decision that is a pointer to the earlier copy `target`.
    pub(crate) fn commit_pointer(
        &mut self,
        id: String,
        target: String,
        pointer: String,
        mut draft: Draft,
        original: &str,
        call: &str,
    ) -> String {
        let redo = self.pointers.contains_key(&id) || draft.pinned;
        let before = draft.original_tokens(original);
        let after = count(&pointer);
        if !draft.pinned {
            self.ledger
                .outputs
                .insert(ledger_key(&id), rule_bit(RuleId::RepeatPointer));
        }
        if !redo {
            draft.on = vec![(RuleId::RepeatPointer, before, after)];
            draft.follow.retain(|(_, shadow, _)| *shadow);
            self.count_draft(&mut draft, original, Some(&pointer), &id, call, None);
        }
        let removed = before as i64 - after as i64;
        self.removed.insert(id.clone(), removed);
        self.request.removed_present += removed;
        self.outputs.remove(&id);
        self.pointers.insert(id.clone(), (target, pointer.clone()));
        if !draft.pinned {
            self.new_this_request.push(id);
        }
        pointer
    }

    fn count_draft(
        &mut self,
        draft: &mut Draft,
        original: &str,
        text: Option<&str>,
        id: &str,
        call: &str,
        producer: Option<String>,
    ) {
        for (rule, before, after) in &draft.on {
            let s = self.saved.rule(*rule);
            s.items += 1;
            s.tokens_before += *before as u64;
            s.tokens_after += *after as u64;
        }
        for (rule, before, after) in &draft.shadow {
            let s = self.saved.rule(*rule);
            s.shadow_items += 1;
            s.shadow_tokens_before += *before as u64;
            s.shadow_tokens_after += *after as u64;
        }
        if draft.condensed {
            self.saved.condensed_upstream += 1;
        }
        if let Some((cohort, saving)) = &draft.cohort {
            let c = self.saved.cohorts.entry(cohort.clone()).or_default();
            c.reads += 1;
            c.would_save_tokens += saving;
        }
        if let Some(file) = draft.shortened_file.take() {
            if self.shortened_files.insert(file.clone()) {
                self.request.files_added.push(file);
            }
        }
        if let Some(t) = text {
            self.saved.outputs_shortened += 1;
            self.saved.tokens_before += draft.original_tokens(original);
            self.saved.tokens_after += count(t);
        }
        let cohort = draft.cohort.as_ref().map(|(c, _)| c.clone());
        for (rule, shadow, facts) in std::mem::take(&mut draft.follow) {
            let k = format!("{rule}:{id}");
            if self.judged.contains(&k) || call.is_empty() {
                continue;
            }
            self.pending.insert(
                k.clone(),
                Pending {
                    rule,
                    shadow,
                    call_id: call.to_string(),
                    producer: producer.clone(),
                    facts,
                    cohort: if rule == RuleId::Skeleton { cohort.clone() } else { None },
                },
            );
            self.request.pending_added.push(k);
        }
    }

    /// Judge the items waiting for their follow-up calls, against the calls
    /// of the current request in order. An item is judged once the calls
    /// after its own include as many as its rule watches (3, 5 for a
    /// skeleton); it is recovered when one of them re-runs its producer with
    /// no edit in between, reads a range a marker named, searches or reads a
    /// file a note listed, or (skeleton) patches the read file.
    pub(crate) fn judge(&mut self, calls: &[CallView]) {
        let index: HashMap<&str, usize> = calls.iter().enumerate().map(|(i, c)| (c.call_id.as_str(), i)).collect();
        // What each later call ran, read once per call: (producer, purpose, args).
        // (producer, purpose, args, run with LGE_RAW=1)
        type Ran = Vec<(Option<String>, Purpose, Vec<String>, bool)>;
        let mut read_calls: HashMap<usize, Ran> = HashMap::new();
        let mut decided: Vec<(String, RuleId, bool, bool, Option<String>, bool)> = Vec::new();
        for (k, p) in &self.pending {
            let Some(&at) = index.get(p.call_id.as_str()) else {
                continue;
            };
            let window = p.rule.follow_up_calls();
            if calls.len() - at - 1 < window {
                continue;
            }
            let mut recovered = false;
            let mut patched = false;
            let mut edited = false;
            for (offset, call) in calls[at + 1..=at + window].iter().enumerate() {
                let ran = read_calls.entry(at + 1 + offset).or_insert_with(|| {
                    call.commands
                        .iter()
                        .map(|c| {
                            let n = normalise(c);
                            (
                                n.as_ref().map(producer_of),
                                purpose(c),
                                n.as_ref().map(|n| n.args.clone()).unwrap_or_default(),
                                n.is_some_and(|n| n.raw),
                            )
                        })
                        .collect()
                });
                for (producer, purpose, args, raw) in ran.iter() {
                    // A re-run after an edit checks the change; asking for the
                    // output whole (LGE_RAW=1) is a recovery either way.
                    if (!edited || *raw) && producer.is_some() && *producer == p.producer {
                        recovered = true;
                    }
                    let listed = |path: &str| p.facts.files.iter().any(|f| same_path(f, path));
                    recovered |= match purpose {
                        // A whole read covers every named range.
                        Purpose::ReadWhole { path, .. } => {
                            p.facts.ranges.iter().any(|(f, _, _)| same_path(f, path)) || listed(path)
                        }
                        Purpose::ReadRange { path, start, end, .. } => {
                            p.facts
                                .ranges
                                .iter()
                                .any(|(f, a, b)| same_path(f, path) && start <= b && a <= end)
                                || listed(path)
                        }
                        Purpose::Search(_) => args.iter().any(|a| listed(a)),
                        _ => false,
                    };
                }
                if let (RuleId::Skeleton, Some(file)) = (p.rule, &p.facts.read_file) {
                    if call.patched.iter().any(|path| same_path(path, file)) {
                        recovered = true;
                        patched = true;
                    }
                }
                edited |= call.edit;
            }
            decided.push((k.clone(), p.rule, p.shadow, recovered, p.cohort.clone(), patched));
        }
        for (k, rule, shadow, recovered, cohort, patched) in decided {
            let s = self.saved.rule(rule);
            if shadow {
                s.shadow_judged += 1;
                s.shadow_followed += u64::from(recovered);
            } else {
                s.judged += 1;
                s.recovered += u64::from(recovered);
            }
            if let (Some(cohort), true) = (cohort, patched) {
                self.saved.cohorts.entry(cohort).or_default().patched_within_5 += 1;
            }
            self.judged.insert(k.clone());
            if let Some(pending) = self.pending.remove(&k) {
                self.request.judged_added.push((k, pending));
            }
        }
    }

    /// The first input item whose client hash differs from the previous
    /// request's, over their common length: the client rewrote its history
    /// there (a compaction, an edit). `None` when the request starts with
    /// every item the previous one had, or there was none.
    pub(crate) fn client_break(&self, client: &[u64]) -> Option<usize> {
        self.prefix_pairs
            .iter()
            .zip(client)
            .position(|((old, _), new)| old != new)
    }

    /// Compare this request's (client hash, forwarded hash) per input item
    /// with the previous request's, over their common length. The first item
    /// whose client hash differs is a client break and ends the walk; before
    /// it, an item whose client hash is the same but whose forwarded hash is
    /// not is a break lge caused (a pack is retired only at or after the
    /// client's break, so a retirement before it is lge's too). At most one
    /// of each is counted per request, at the first such item.
    /// Returns the item index of each break.
    pub(crate) fn compare_prefix(&mut self, pairs: Vec<(u64, u64)>) -> (Option<usize>, Option<usize>) {
        let mut lge = None;
        let mut client = None;
        for (i, (old, new)) in self.prefix_pairs.iter().zip(&pairs).enumerate() {
            if old.0 != new.0 {
                client = Some(i);
                break;
            }
            if old.1 != new.1 && lge.is_none() {
                lge = Some(i);
            }
        }
        if let Some(i) = lge {
            self.prefix.lge_breaks += 1;
            self.prefix.first_lge_break_item.get_or_insert(i);
        }
        if client.is_some() {
            self.prefix.client_breaks += 1;
        }
        self.prefix_pairs = pairs;
        (lge, client)
    }

    /// Tokens removed by the replaced items and added by the context items
    /// present in the current request so far.
    /// Every item's decision by hash, to keep with the conversation's record.
    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    /// Start from the decisions a daemon made before it restarted: an item
    /// they cover is decided again with the rules it got then, and is not
    /// counted again.
    pub fn restore(&mut self, ledger: Ledger) {
        self.ledger = ledger;
    }

    /// A copy of a message typed earlier in the same request gets no pack
    /// (see [`Ledger::no_pack_for_copies`]).
    pub(crate) fn no_pack_for_copies(&self) -> bool {
        self.ledger.no_pack_for_copies
    }

    /// The rules an item decided before a restart got, when the ledger has
    /// it and this session has not decided it yet.
    pub(crate) fn pinned(&self, id: &str) -> Option<u16> {
        if self.outputs.contains_key(id) {
            return None;
        }
        self.ledger.outputs.get(&ledger_key(id)).copied()
    }

    pub(crate) fn present_tokens(&self) -> (i64, i64) {
        (self.request.removed_present, self.request.added_present)
    }

    /// Forget every output decision and the ledger, as if the daemon had
    /// restarted without the conversation's record. For tests of the cache
    /// monitor.
    #[cfg(test)]
    pub(crate) fn forget_outputs(&mut self) {
        self.outputs.clear();
        self.pointers.clear();
        self.ledger = Ledger::default();
    }

    #[cfg(test)]
    pub(crate) fn is_raw_call(&self, id: &str) -> bool {
        self.raw_calls.contains(id)
    }
}

fn is_lockfile(path: &str) -> bool {
    let base = path.rsplit('/').next().unwrap_or(path);
    matches!(
        base,
        "Cargo.lock" | "package-lock.json" | "npm-shrinkwrap.json" | "pnpm-lock.yaml" | "yarn.lock"
    )
}

impl CheckDecision {
    fn shorter(&self) -> Option<String> {
        match self {
            CheckDecision::Shorter(text) => Some(text.clone()),
            CheckDecision::AsIs | CheckDecision::NotACheck => None,
        }
    }
}

/// What every rule would do to `output` of `command` if it were `on`: the
/// rules whose result would be sent, each with its tokens before and after.
/// Without a graph, without the conversation (no repeat pointer, no
/// re-run recovery) and without the measured deltas. An empty `command`
/// judges the output by its content alone. For replaying past sessions.
pub fn shadow_decide(command: &str, output: &str, status: RunStatus) -> Vec<(RuleId, usize, usize)> {
    let session = Session::default();
    let opts = Options {
        inject_context: false,
        rules: RuleModes::all(Mode::On),
        ..Options::default()
    };
    let draft = if command.trim().is_empty() {
        session.draft_content(output, status, false, &opts)
    } else {
        session.draft_known(None, command, None, output, status, &opts, false)
    };
    draft.on
}

#[cfg(test)]
mod tests {
    use super::*;

    fn passing_run() -> String {
        let mut run = String::from(
            "============================= test session starts ==============================\ncollected 300 items\n\n",
        );
        for i in 0..300 {
            run.push_str(&format!(
                "tests/test_orders.py::test_case_number_{i} PASSED                              [{:3}%]\n",
                i / 3
            ));
        }
        run + "\n============================= 300 passed in 9.87s ==============================\n"
    }

    fn matches() -> String {
        let mut out = String::new();
        for f in 0..40 {
            for l in 0..8 {
                out.push_str(&format!(
                    "services/svc_{f}/handlers.py:{}:    order = create_order(payload, request_id=request.id)\n",
                    l * 5 + 2
                ));
            }
        }
        out
    }

    #[test]
    fn modes_follow_holdout_and_measure_only_rules() {
        let mut opts = Options::default();
        assert_eq!(opts.mode(RuleId::Check), Mode::On);
        assert_eq!(opts.mode(RuleId::Skeleton), Mode::Shadow);
        opts.rules.rerun_delta = Mode::On;
        assert_eq!(opts.mode(RuleId::RerunDelta), Mode::Shadow, "measure only");
        opts.rules.check = Mode::Off;
        assert_eq!(opts.mode(RuleId::Check), Mode::Off);
        opts.holdout = true;
        assert_eq!(opts.mode(RuleId::SearchCap), Mode::Shadow);
        assert_eq!(opts.mode(RuleId::Check), Mode::Off, "off stays off in holdout");
    }

    #[test]
    fn a_shadow_rule_records_and_forwards_the_original() {
        let run = passing_run();
        let mut opts = Options::default();
        opts.rules.check = Mode::Shadow;
        let mut session = Session::default();
        assert_eq!(
            session.rewrite_call_output(None, "c1", "pytest -v", None, &run, &opts),
            None
        );
        let s = session.saved.by_rule["check"];
        assert_eq!((s.items, s.shadow_items), (0, 1));
        assert!(s.shadow_tokens_after * 10 < s.shadow_tokens_before, "{s:?}");
        assert_eq!(session.saved.outputs_shortened, 0);
        // Promoted later: the item decided in shadow keeps its bytes, a new one is shortened.
        opts.rules.check = Mode::On;
        assert_eq!(
            session.rewrite_call_output(None, "c1", "pytest -v", None, &run, &opts),
            None
        );
        let new = session
            .rewrite_call_output(None, "c2", "pytest -v", None, &run, &opts)
            .expect("a new item gets the rule in on");
        assert!(new.starts_with("ok"), "{new}");
        let s = session.saved.by_rule["check"];
        assert_eq!((s.items, s.shadow_items), (1, 1), "each item counted once");
    }

    #[test]
    fn the_master_switch_off_caches_new_items_as_they_are() {
        let run = passing_run();
        let mut session = Session::default();
        let off = Options {
            shorten_outputs: false,
            ..Options::default()
        };
        assert_eq!(
            session.rewrite_call_output(None, "c1", "pytest -v", None, &run, &off),
            None
        );
        assert!(session.saved.by_rule.is_empty(), "no rule at all, not even shadow");
        // Switched on again: the item decided while it was off stays as it was.
        assert_eq!(
            session.rewrite_call_output(None, "c1", "pytest -v", None, &run, &Options::default()),
            None
        );
        // And an item shortened while it was on keeps its bytes when it is off.
        let short = session.rewrite_call_output(None, "c2", "pytest -v", None, &run, &Options::default());
        assert!(short.is_some());
        assert_eq!(
            session.rewrite_call_output(None, "c2", "pytest -v", None, &run, &off),
            short
        );
    }

    #[test]
    fn rtk_condensed_output_gets_only_lossless_rules() {
        let opts = Options::default();
        let mut session = Session::default();
        let mut rtk = String::from("cargo test: 300 passed (4 suites)\n");
        for i in 0..200 {
            rtk.push_str(&format!(
                "  ok tests::case_{i} in suite orders with a long enough name\n"
            ));
        }
        rtk.push_str("[full output: ~/.rtk/tee/1234.log]\n");
        assert_eq!(
            session.rewrite_call_output(None, "c1", "rtk cargo test", None, &rtk, &opts),
            None
        );
        assert_eq!(session.saved.condensed_upstream, 1);
        let mut hidden = passing_run();
        hidden.push_str("[+12 hidden: rtk recall 3197d8cf18ed]\n");
        assert_eq!(session.rewrite_text(&hidden, &opts), None);
        assert_eq!(
            session.rewrite_call_output(None, "c2", "pytest -v", None, &hidden, &opts),
            None
        );
        assert_eq!(session.saved.condensed_upstream, 3);
        assert_eq!(session.saved.outputs_shortened, 0);
    }

    #[test]
    fn an_lge_raw_prefix_is_never_shortened() {
        let opts = Options::default();
        let mut session = Session::default();
        assert_eq!(
            session.rewrite_call_output(None, "c1", "LGE_RAW=1 pytest -v", None, &passing_run(), &opts),
            None
        );
        assert_eq!(
            session.rewrite_call_output(None, "c2", "LGE_RAW=1 rg -n create_order", None, &matches(), &opts),
            None
        );
        assert!(session.saved.by_rule.is_empty());
    }

    #[test]
    fn markers_and_notes_name_the_way_back() {
        let facts = facts_of(
            "def a():\n    # lge: 12 lines of function `a` left out; read them with: sed -n '10,21p' svc/a.py\n\
             x:3:...[lge: +900 chars; whole line: sed -n '3p' 'my dir/x.py']\n\
             src/b.py: (+4 more matching lines in this file)\n\
             lge: 60 of 320 matching lines shown, from 10 of 40 files. Not shown: src/c.py (8), src/d, e.py (2), and 3 more files (9 matching lines; rg -l with the same pattern lists them). Narrow the pattern\n",
        );
        assert_eq!(
            facts.ranges,
            vec![("svc/a.py".into(), 10, 21), ("my dir/x.py".into(), 3, 3)]
        );
        assert_eq!(facts.files, vec!["src/b.py", "src/c.py", "src/d, e.py"]);
        assert!(same_path("./src/a.py", "src/a.py"));
        assert!(same_path("/repo/src/a.py", "src/a.py"));
        assert!(!same_path("src/ba.py", "a.py"));
    }

    fn calls(commands: &[&str]) -> Vec<CallView> {
        commands
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let (edit, patched, commands) = match c.strip_prefix("PATCH ") {
                    Some(path) => (true, vec![path.to_string()], vec![]),
                    None => (!crate::command::is_read_only(c), vec![], vec![c.to_string()]),
                };
                CallView {
                    call_id: format!("c{}", i + 1),
                    edit,
                    commands,
                    patched,
                }
            })
            .collect()
    }

    #[test]
    fn a_shortened_check_followed_by_a_rerun_counts_as_recovered_once() {
        let opts = Options::default();
        let mut session = Session::default();
        session.rewrite_call_output(None, "c1", "pytest -v", None, &passing_run(), &opts);
        let history = calls(&["pytest -v", "pytest -v | tail -n 5", "ls", "git status"]);
        session.judge(&history[..3]);
        assert_eq!(session.saved.by_rule["check"].judged, 0, "only 2 calls after it so far");
        session.judge(&history);
        session.judge(&history);
        let s = session.saved.by_rule["check"];
        assert_eq!((s.judged, s.recovered), (1, 1), "judged once");

        // After an edit, a re-run checks the change: not a recovery.
        let mut other = Session::default();
        other.rewrite_call_output(None, "c1", "pytest -v", None, &passing_run(), &opts);
        other.judge(&calls(&["pytest -v", "PATCH src/a.py", "pytest -v", "ls"]));
        let s = other.saved.by_rule["check"];
        assert_eq!((s.judged, s.recovered), (1, 0));
        // Asking for the output whole is a recovery, edit or not.
        let mut raw = Session::default();
        raw.rewrite_call_output(None, "c1", "pytest -v", None, &passing_run(), &opts);
        raw.judge(&calls(&["pytest -v", "PATCH src/a.py", "LGE_RAW=1 pytest -v", "ls"]));
        let s = raw.saved.by_rule["check"];
        assert_eq!((s.judged, s.recovered), (1, 1));
    }

    #[test]
    fn revert_new_undoes_only_this_requests_decisions() {
        let opts = Options::default();
        let mut session = Session::default();
        session.begin_request();
        let old = session.rewrite_call_output(None, "c1", "pytest -v", None, &passing_run(), &opts);
        assert!(old.is_some());
        let saved = session.saved.clone();
        session.begin_request();
        let new = session.rewrite_call_output(None, "c2", "rg -n create_order", None, &matches(), &opts);
        assert!(new.is_some());
        assert_eq!(session.new_this_request().len(), 1);
        session.revert_new();
        assert_eq!(session.saved, saved, "the counters go back");
        assert_eq!(
            session.rewrite_call_output(None, "c2", "rg -n create_order", None, &matches(), &opts),
            None,
            "sent as is for good"
        );
        assert_eq!(
            session.rewrite_call_output(None, "c1", "pytest -v", None, &passing_run(), &opts),
            old,
            "an earlier decision stays"
        );
        assert_eq!(session.saved, saved);
    }

    #[test]
    fn a_ledger_of_an_older_version_keeps_sending_packs_after_copies() {
        assert!(Ledger::default().no_pack_for_copies, "a new conversation");
        let older: Ledger = serde_json::from_str(r#"{"outputs":{},"contexts":{"0123456789ab":true}}"#).unwrap();
        assert!(!older.no_pack_for_copies, "a ledger written before the field existed");
        assert!(older.anchors.is_empty() && older.retired.is_empty());
        let again: Ledger = serde_json::from_str(&serde_json::to_string(&Ledger::default()).unwrap()).unwrap();
        assert_eq!(again, Ledger::default());
        let mut session = Session::default();
        session.restore(older);
        assert!(!session.no_pack_for_copies());
    }

    #[test]
    fn a_path_is_mentioned_only_as_a_whole_path() {
        assert!(mentions("cat orders_api/repo.py", "orders_api/repo.py"));
        assert!(mentions(
            "/work/shop/orders_api/repo.py:23:def find_order",
            "orders_api/repo.py"
        ));
        assert!(mentions(
            "{\"cmd\":\"sed -n '1,9p' ./orders_api/repo.py\"}",
            "orders_api/repo.py"
        ));
        assert!(!mentions("cat orders_api/repo.pyc", "orders_api/repo.py"));
        assert!(!mentions("cat src/metadata.py", "data.py"));
        assert!(!mentions("cat src/my-data.py", "data.py"));
        assert!(!mentions("anything", ""));
    }

    #[test]
    fn shadow_decide_says_what_each_rule_would_remove() {
        let fired = shadow_decide("pytest -v", &passing_run(), RunStatus::default());
        assert_eq!(fired.len(), 1, "{fired:?}");
        assert_eq!(fired[0].0, RuleId::Check);
        assert!(fired[0].2 * 10 < fired[0].1);
        let search = shadow_decide("rg -n create_order", &matches(), RunStatus::default());
        assert_eq!(search.last().map(|f| f.0), Some(RuleId::SearchCap));
        let content = shadow_decide("", &passing_run(), RunStatus::default());
        assert_eq!(content.first().map(|f| f.0), Some(RuleId::Check));
        assert!(shadow_decide(
            "pytest -v",
            &passing_run(),
            RunStatus {
                exit: Some(1),
                running: false,
                masked: false,
            }
        )
        .is_empty());
        assert!(shadow_decide("cat a.py", "x\n", RunStatus::default()).is_empty());
    }
}
