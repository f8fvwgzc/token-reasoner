//! What the proxy remembers about one conversation, and the two decisions it
//! makes with it: what to send in place of a tool output, and what context to
//! add after a message the user typed.
//!
//! The provider caches the start of a prompt only while it stays byte-identical,
//! and the agent resends the whole conversation on every turn. So a decision
//! made for an item is made once and remembered: the same item is rewritten
//! the same way in every later request.

use crate::command::{purpose, Purpose};
use crate::shorten::{cap_search, failures_only, skeleton};
use lge_core::{content_hash, split_words, NodeRec};
use lge_retrieve::budget::count;
use lge_retrieve::{evidence_pack, find_seeds, PackOptions, SeedOptions};
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
Use it as the map: start from these locations instead of searching, and read only the line ranges you need. \
Edges marked inferred or unresolved are not verified.\n\n";
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
                budget: opts.context_budget,
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
        Some(format!("{CONTEXT_OPEN}{}\n{CONTEXT_CLOSE}", pack.text.trim_end()))
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
        if !opts.shorten_outputs {
            return None;
        }
        let id = key(&["output", command, output]);
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
            Purpose::ReadWhole { .. } | Purpose::ReadRange { .. } | Purpose::Other => None,
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
