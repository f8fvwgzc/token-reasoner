//! The storage contract. Every other crate reaches the database through this
//! trait; only `lge-store` imports `lbug`.

use anyhow::Result;
use lge_core::{
    Direction, EdgeRec, EdgeType, FileInfo, HandoffNote, Hit, IndexRun, NodeLabel, NodeRec, ParsedFile, ResolveInput,
    Resolved, UsageRecord,
};
use std::collections::HashMap;

pub trait GraphStore: Send + Sync {
    // ---- indexing -------------------------------------------------------

    /// path -> (file id, content hash) for every indexed file.
    fn file_hashes(&self) -> Result<HashMap<String, (String, String)>>;

    fn file_info(&self, path: &str) -> Result<Option<FileInfo>>;

    /// Remove a file and everything extracted from it (symbols, call sites,
    /// import sites, fact sites and the edges that touch its symbols).
    fn delete_file(&self, path: &str) -> Result<()>;

    /// Insert one parsed file in a single transaction: the File node, its
    /// Symbols, the staged call, import and fact sites, the Service node for
    /// `file.service` if missing, and the CONTAINS edges
    /// (Repository -> Service -> File -> Symbol). `commit` is the repository
    /// HEAD ("" outside git); `now` is unix seconds.
    fn insert_parsed(&self, parsed: &ParsedFile, commit: &str, now: i64) -> Result<()>;

    /// Insert many parsed files. The default calls [`insert_parsed`](Self::insert_parsed)
    /// per file; an adapter may override it with a bulk load (`COPY FROM`).
    fn insert_parsed_bulk(&self, parsed: &[ParsedFile], commit: &str, now: i64) -> Result<()> {
        for p in parsed {
            self.insert_parsed(p, commit, now)?;
        }
        Ok(())
    }

    /// Everything the resolver needs. Symbol bodies are left empty.
    fn load_resolve_input(&self) -> Result<ResolveInput>;

    /// Replace all derived data with `resolved`: delete every non-CONTAINS edge
    /// and every ApiEndpoint, DbTable, Queue and placeholder (`kind = 'unresolved'`)
    /// Symbol node, write the new nodes and edges with their provenance, and
    /// refresh the full-text index. Calling it twice with the same input leaves
    /// the same graph.
    fn replace_edges(&self, resolved: &Resolved, commit: &str, now: i64) -> Result<()>;

    /// Record the indexed repository root and its HEAD commit.
    fn set_repository(&self, path: &str, head_commit: &str) -> Result<()>;

    /// (path, head commit) of the indexed repository, if any.
    fn repository(&self) -> Result<Option<(String, String)>>;

    /// Start an IndexRun with status `running`; returns its id.
    fn begin_run(&self, now: i64) -> Result<String>;

    fn finish_run(&self, run_id: &str, now: i64, status: &str, files_changed: i64) -> Result<()>;

    fn last_run(&self) -> Result<Option<IndexRun>>;

    // ---- search and lookup ----------------------------------------------

    /// True when the `fts` extension is loaded and search uses BM25.
    fn fts_enabled(&self) -> bool;

    /// Download and install the `fts` extension, then build the index.
    fn install_fts(&self) -> Result<()>;

    /// Seed search over symbol name, qualified name, signature and doc. BM25
    /// when the fts extension is loaded, otherwise a substring match. Modules
    /// and placeholders are not returned. Highest score first.
    fn search(&self, text: &str, limit: usize) -> Result<Vec<Hit>>;

    /// Symbols whose id, qualified name or name equals `needle`.
    fn find(&self, needle: &str) -> Result<Vec<Hit>>;

    /// Symbols whose name starts with `prefix` (case-insensitive).
    fn find_prefix(&self, prefix: &str, limit: usize) -> Result<Vec<Hit>>;

    /// Nodes of any label by id. Unknown ids are skipped. Order is not guaranteed.
    fn nodes(&self, ids: &[String]) -> Result<Vec<NodeRec>>;

    fn nodes_by_label(&self, label: NodeLabel, limit: usize) -> Result<Vec<NodeRec>>;

    /// Symbols defined in a file, ordered by start line.
    fn symbols_in_file(&self, path: &str) -> Result<Vec<NodeRec>>;

    /// Stored source text of a symbol.
    fn symbol_body(&self, id: &str) -> Result<Option<String>>;

    // ---- traversal ------------------------------------------------------

    /// One hop. All edges of the given types that start at (`Out`) or end at
    /// (`In`) any of `ids`. An empty `types` means every type except CONTAINS.
    fn edges(&self, ids: &[String], types: &[EdgeType], dir: Direction) -> Result<Vec<EdgeRec>>;

    /// Reverse CALLS traversal with a variable-length match: every symbol that
    /// reaches `id` within `depth` hops (1..=6). `score` is the lowest hop count.
    fn callers(&self, id: &str, depth: u8) -> Result<Vec<Hit>>;

    /// Number of unresolved edges leaving a symbol.
    fn unresolved_out(&self, id: &str) -> Result<i64>;

    // ---- meta -----------------------------------------------------------

    /// Counts of nodes and of edges by type and provenance, as (label, count).
    fn stats(&self) -> Result<Vec<(String, i64)>>;

    /// Run a raw Cypher query and return the printed result table.
    fn raw(&self, cypher: &str) -> Result<String>;

    // ---- usage log and handoff notes ------------------------------------

    /// Insert or replace a usage record by id.
    fn put_usage(&self, rec: &UsageRecord) -> Result<()>;

    /// Usage records with `started_at >= since`, oldest first.
    fn list_usage(&self, since: i64) -> Result<Vec<UsageRecord>>;

    fn put_handoff(&self, note: &HandoffNote) -> Result<()>;

    /// Most recent handoff notes, newest first.
    fn latest_handoffs(&self, limit: usize) -> Result<Vec<HandoffNote>>;
}
