//! What the proxy remembers about one conversation, and the two decisions it
//! makes with it: what to send in place of a tool output, and what context to
//! add after a message the user typed.
//!
//! The provider caches the start of a prompt only while it stays byte-identical,
//! and the agent resends the whole conversation on every turn. So a decision
//! made for an item is made once and remembered: the same item is rewritten
//! the same way in every later request.

use crate::command::{purpose, Purpose};
use crate::shorten::{cap_numbered_search, cap_search, check_run, failures_only, skeleton};
use lge_core::{content_hash, split_words, NodeLabel, NodeRec};
use lge_retrieve::budget::count;
use lge_retrieve::{dependents_of, evidence_pack, find_seeds, PackOptions, SeedOptions, WalkOptions};
use lge_store::GraphStore;
use std::collections::{BTreeSet, HashMap};
use std::path::Path;

/// The graph of the repository a conversation works in.
pub struct Graph<'a> {
    pub store: &'a dyn GraphStore,
    pub root: &'a Path,
}

#[derive(Debug, Clone)]
pub struct Options {
    /// Add the evidence pack after a message the user typed.
    pub inject_context: bool,
    /// Shorten search results and check runs, and point a repeated output
    /// back to its first copy.
    pub shorten_outputs: bool,
    /// Send a skeleton for a whole-file read. Off by default: a shortened read
    /// can cost more in follow-up reads than it saves.
    pub shorten_reads: bool,
    /// Tokens of one injected pack.
    pub context_budget: usize,
    /// Packs added per conversation at most.
    pub max_contexts: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            inject_context: true,
            shorten_outputs: true,
            shorten_reads: false,
            context_budget: 1500,
            max_contexts: 6,
        }
    }
}

/// Token counts of what the proxy changed, for the savings report.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct Saved {
    /// Tool outputs that were shortened.
    pub outputs_shortened: usize,
    pub tokens_before: usize,
    pub tokens_after: usize,
    /// Context packs added, and what they cost.
    pub contexts_added: usize,
    pub context_tokens: usize,
}

/// Opening lines of every injected context. Fixed text: it is part of the
/// cached prompt of every later turn.
pub const CONTEXT_OPEN: &str = "<lge_context>\nCode-graph context for the request above, added by the local graph engine, not written by the user. \
It was worked out from the source: use its conclusions instead of re-deriving them, start from these locations instead of searching, \
and read only the line ranges you need.\n\n";
/// Tokens of remembered facts added to one context.
const MEMORY_BUDGET: usize = 300;
pub const CONTEXT_CLOSE: &str = "</lge_context>";

#[derive(Default)]
pub struct Session {
    /// Hash of (command, output) -> what to send instead; `None` = send as is.
    outputs: HashMap<String, Option<String>>,
    /// Hash of a user message -> the context added after it; `None` = none.
    contexts: HashMap<String, Option<String>>,
    /// Files a skeleton was sent for. The next read of such a file is sent
    /// whole: the reader asked again, so it needs what was left out.
    shortened_files: BTreeSet<String>,
    /// Symbols and files the conversation is about, from the packs so far.
    relevant_symbols: BTreeSet<String>,
    relevant_files: BTreeSet<String>,
    /// Seeds of the packs already added, so the same pack is not added twice.
    seeds_shown: BTreeSet<String>,
    /// Outputs replaced by a pointer to their earlier copy, so each is counted once.
    pointers: BTreeSet<String>,
    /// Words of what the user typed, lower-cased.
    words: BTreeSet<String>,
    pub saved: Saved,
}

fn key(parts: &[&str]) -> String {
    let mut bytes = Vec::new();
    for p in parts {
        bytes.extend_from_slice(p.as_bytes());
        bytes.push(0);
    }
    content_hash(&bytes)
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

impl Session {
    /// The context to add after a message the user typed, or `None`. Decided
    /// once per distinct message and repeated exactly afterwards.
    ///
    /// A pack is added only when the graph clearly has something for the
    /// message: a symbol the message names, or a search hit that shares at
    /// least two words with it. "ok, do it" gets nothing.
    pub fn context_for(&mut self, graph: &Graph, user_text: &str, opts: &Options) -> Option<String> {
        let id = key(&["context", user_text]);
        if let Some(known) = self.contexts.get(&id) {
            return known.clone();
        }
        let words: BTreeSet<String> = split_words(user_text).into_iter().collect();
        self.words.extend(words.iter().cloned());
        let decided = self.decide_context(graph, user_text, &words, opts);
        if let Some(text) = &decided {
            self.saved.contexts_added += 1;
            self.saved.context_tokens += count(text);
        }
        self.contexts.insert(id, decided.clone());
        decided
    }

    fn decide_context(
        &mut self,
        graph: &Graph,
        user_text: &str,
        words: &BTreeSet<String>,
        opts: &Options,
    ) -> Option<String> {
        if !opts.inject_context || self.saved.contexts_added >= opts.max_contexts || user_text.len() > 8000 {
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
        // Nothing new to say: these starting points were already given.
        if seeds.iter().all(|s| self.seeds_shown.contains(&s.hit.id)) {
            return None;
        }
        let pack = evidence_pack(
            graph.store,
            user_text,
            &PackOptions {
                budget: pack_budget(graph.store, user_text, opts),
                ..PackOptions::default()
            },
        )
        .ok()?;
        if pack.seeds.is_empty() {
            return None;
        }
        for seed in &pack.seeds {
            self.seeds_shown.insert(seed.id.clone());
            self.relevant_symbols.insert(seed.id.clone());
        }
        self.relevant_files.extend(pack.files.iter().cloned());
        // Decisions and conventions stored in earlier sessions: the model uses
        // them instead of working them out again.
        let mut text = pack.text.trim_end().to_string();
        let remembered = lge_memory::recall(
            graph.store,
            user_text,
            &lge_memory::RecallOptions {
                budget: MEMORY_BUDGET,
                now: lge_retrieve::unix_now(),
                limit: 8,
                ..lge_memory::RecallOptions::default()
            },
            None,
        );
        if let Ok(found) = remembered {
            if !found.items.is_empty() {
                text.push_str("\n\n## From project memory (decisions, conventions, recent commits)\n");
                text.push_str(found.text.trim_end());
            }
        }
        Some(format!("{CONTEXT_OPEN}{text}\n{CONTEXT_CLOSE}"))
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
    /// bytes on every turn, while a second call with the same command and
    /// output is decided on its own (a file read again is sent whole).
    pub fn rewrite_call_output(
        &mut self,
        graph: Option<&Graph>,
        call: &str,
        command: &str,
        cwd: Option<&Path>,
        output: &str,
        opts: &Options,
    ) -> Option<String> {
        if !opts.shorten_outputs {
            return None;
        }
        let id = key(&["output", call, command, output]);
        if let Some(known) = self.outputs.get(&id) {
            return known.clone();
        }
        let decided = self.decide_output(graph, command, cwd, output, opts);
        if let Some(text) = &decided {
            self.saved.outputs_shortened += 1;
            self.saved.tokens_before += count(output);
            self.saved.tokens_after += count(text);
        }
        self.outputs.insert(id, decided.clone());
        decided
    }

    /// What to send in place of a tool output whose command is not known (a
    /// script, a tool the proxy cannot read), judged by its content alone: a
    /// recognised test or lint run, or a long list of numbered search matches.
    /// A recognised run is never also taken for a search. Decided once per
    /// distinct text and repeated exactly afterwards.
    pub fn rewrite_text(&mut self, output: &str, opts: &Options) -> Option<String> {
        if !opts.shorten_outputs {
            return None;
        }
        let id = key(&["text", output]);
        if let Some(known) = self.outputs.get(&id) {
            return known.clone();
        }
        let files = &self.relevant_files;
        let decided = match check_run("", output) {
            Some(decision) => decision,
            None => cap_numbered_search(output, &|path| files.contains(path.trim_start_matches("./"))),
        };
        if let Some(text) = &decided {
            self.saved.outputs_shortened += 1;
            self.saved.tokens_before += count(output);
            self.saved.tokens_after += count(text);
        }
        self.outputs.insert(id, decided.clone());
        decided
    }

    /// Count an output that was replaced by a pointer to its earlier copy.
    /// The same item comes back on every turn; it is counted once.
    pub fn count_pointer(&mut self, id: &str, before: &str, after: &str) {
        if self.pointers.insert(id.to_string()) {
            self.saved.outputs_shortened += 1;
            self.saved.tokens_before += count(before);
            self.saved.tokens_after += count(after);
        }
    }

    fn decide_output(
        &mut self,
        graph: Option<&Graph>,
        command: &str,
        cwd: Option<&Path>,
        output: &str,
        opts: &Options,
    ) -> Option<String> {
        match purpose(command) {
            Purpose::Check { tool } => failures_only(&tool, output),
            Purpose::Search => {
                let files = &self.relevant_files;
                cap_search(output, &|path| files.contains(path.trim_start_matches("./")))
            }
            // A range that starts at the top is looking around; a range further
            // down is fetching known lines and is never touched.
            Purpose::ReadWhole { path } if opts.shorten_reads => self.file_skeleton(graph?, &path, cwd, output),
            Purpose::ReadRange {
                path, from_top: true, ..
            } if opts.shorten_reads => self.file_skeleton(graph?, &path, cwd, output),
            Purpose::ReadWhole { .. } | Purpose::ReadRange { .. } => None,
            // A runner the proxy does not know (`npm test`, `make check`) may still
            // print a test or lint run it recognises: judge that by its content.
            Purpose::Other => failures_only("", output),
        }
    }

    fn file_skeleton(&mut self, graph: &Graph, path: &str, cwd: Option<&Path>, output: &str) -> Option<String> {
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
        // (no wrapper text, no line numbers): otherwise ranges would not match.
        let file_text = String::from_utf8_lossy(&on_disk);
        if !file_text.starts_with(output.trim_end_matches('\n')) {
            return None;
        }
        let symbols: Vec<NodeRec> = graph.store.symbols_in_file(&rel).ok()?;
        let relevant = &self.relevant_symbols;
        let words = &self.words;
        let keep = |s: &NodeRec| relevant.contains(&s.id) || split_words(&s.name).iter().all(|w| words.contains(w));
        let short = skeleton(&rel, output, 1, &symbols, &keep)?;
        self.shortened_files.insert(rel);
        Some(short)
    }
}
