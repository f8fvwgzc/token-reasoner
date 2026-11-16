//! The storage contract. Every other crate reaches the database through this
//! trait; only `lge-store` imports `lbug`.

use anyhow::{bail, Result};
use lge_core::{
    Direction, EdgeRec, EdgeType, FileInfo, HandoffNote, Hit, IndexRun, MemoryRec, NodeLabel, NodeRec, PageRec,
    ParsedFile, ResolveInput, Resolved, SymbolMetrics, UsageRecord, VectorTable,
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

    /// The usage record with this id, if there is one.
    fn get_usage(&self, id: &str) -> Result<Option<UsageRecord>>;

    fn put_handoff(&self, note: &HandoffNote) -> Result<()>;

    /// Most recent handoff notes, newest first.
    fn latest_handoffs(&self, limit: usize) -> Result<Vec<HandoffNote>>;

    // ---- ranked search and in-database traversal -------------------------

    /// Like [`search`](Self::search), restricted to one service when given. The
    /// query is split with `lge_core::split_words`, so `order handler` finds
    /// `getOrderHandler`.
    fn search_in(&self, text: &str, service: Option<&str>, limit: usize) -> Result<Vec<Hit>> {
        let _ = (text, service, limit);
        bail!("search_in is not implemented by this store")
    }

    /// Nodes reachable from `ids` over `types` in one direction within `depth`
    /// hops (1..=6), with the lowest hop count, nearest first, at most `limit`.
    /// The start nodes are not returned. Placeholders are not returned.
    fn reach(
        &self,
        ids: &[String],
        types: &[EdgeType],
        dir: Direction,
        depth: u8,
        limit: usize,
    ) -> Result<Vec<(NodeRec, u8)>> {
        let _ = (ids, types, dir, depth, limit);
        bail!("reach is not implemented by this store")
    }

    /// One shortest path from `from` to `to` following `types` along their
    /// direction, at most `max_depth` hops, as its edges in order. `None` when
    /// there is none.
    fn shortest_path(&self, from: &str, to: &str, types: &[EdgeType], max_depth: u8) -> Result<Option<Vec<EdgeRec>>> {
        let _ = (from, to, types, max_depth);
        bail!("shortest_path is not implemented by this store")
    }

    /// Every stored edge between two Symbols that is not CONTAINS and not
    /// unresolved, as (from, to, type, confidence). Input of the ranking step.
    fn symbol_graph(&self) -> Result<Vec<(String, String, EdgeType, f64)>> {
        bail!("symbol_graph is not implemented by this store")
    }

    /// Store rank, community and degree on symbols. Unknown ids are skipped;
    /// symbols not listed keep rank 0, community -1, degree 0.
    fn set_metrics(&self, metrics: &[SymbolMetrics]) -> Result<()> {
        let _ = metrics;
        bail!("set_metrics is not implemented by this store")
    }

    /// Stored metrics of the given symbols. Unknown ids are skipped.
    fn metrics(&self, ids: &[String]) -> Result<Vec<SymbolMetrics>> {
        let _ = ids;
        bail!("metrics is not implemented by this store")
    }

    /// Symbols by stored rank, highest first, optionally within one service or
    /// one community. Modules, tests and placeholders are excluded. `score` is the rank.
    fn top_ranked(&self, service: Option<&str>, community: Option<i64>, limit: usize) -> Result<Vec<Hit>> {
        let _ = (service, community, limit);
        bail!("top_ranked is not implemented by this store")
    }

    /// Fact sites of the given kind attached to the given symbols.
    fn facts_of(&self, symbol_ids: &[String], kind: &str) -> Result<Vec<lge_core::FactSite>> {
        let _ = (symbol_ids, kind);
        bail!("facts_of is not implemented by this store")
    }

    // ---- long-term memory -----------------------------------------------

    /// Insert or replace a fact by id, with its MENTIONS edges to the nodes in
    /// `links` that exist.
    fn put_memory(&self, m: &MemoryRec) -> Result<()> {
        let _ = m;
        bail!("put_memory is not implemented by this store")
    }

    /// Facts by id. Unknown ids are skipped.
    fn get_memories(&self, ids: &[String]) -> Result<Vec<MemoryRec>> {
        let _ = ids;
        bail!("get_memories is not implemented by this store")
    }

    /// Keyword search over fact text, key and tags, best first with a score.
    /// Expired facts are returned only when `include_expired` is set.
    fn search_memories(&self, text: &str, limit: usize, include_expired: bool) -> Result<Vec<(MemoryRec, f64)>> {
        let _ = (text, limit, include_expired);
        bail!("search_memories is not implemented by this store")
    }

    /// Current facts that mention any of the given nodes, with the number of
    /// those nodes each fact mentions, most shared first.
    fn memories_mentioning(&self, node_ids: &[String], limit: usize) -> Result<Vec<(MemoryRec, i64)>> {
        let _ = (node_ids, limit);
        bail!("memories_mentioning is not implemented by this store")
    }

    /// Current facts, newest `valid_at` first, optionally of one kind or
    /// carrying any of the given tags.
    fn recent_memories(&self, kind: Option<&str>, tags: &[String], limit: usize) -> Result<Vec<MemoryRec>> {
        let _ = (kind, tags, limit);
        bail!("recent_memories is not implemented by this store")
    }

    /// Current facts with exactly this key.
    fn memories_by_key(&self, key: &str) -> Result<Vec<MemoryRec>> {
        let _ = key;
        bail!("memories_by_key is not implemented by this store")
    }

    /// Number of stored facts: (current, expired).
    fn memory_counts(&self) -> Result<(i64, i64)> {
        bail!("memory_counts is not implemented by this store")
    }

    // ---- pages ------------------------------------------------------------

    /// Insert or replace a page by slug.
    fn put_page(&self, page: &PageRec) -> Result<()> {
        let _ = page;
        bail!("put_page is not implemented by this store")
    }

    fn get_page(&self, slug: &str) -> Result<Option<PageRec>> {
        let _ = slug;
        bail!("get_page is not implemented by this store")
    }

    /// Every page, ordered by slug.
    fn list_pages(&self) -> Result<Vec<PageRec>> {
        bail!("list_pages is not implemented by this store")
    }

    /// Keyword search over page title and body, best first with a score.
    fn search_pages(&self, text: &str, limit: usize) -> Result<Vec<(PageRec, f64)>> {
        let _ = (text, limit);
        bail!("search_pages is not implemented by this store")
    }

    fn delete_page(&self, slug: &str) -> Result<()> {
        let _ = slug;
        bail!("delete_page is not implemented by this store")
    }

    // ---- vectors (optional) ---------------------------------------------

    /// Store embeddings for rows of `table` by id. All vectors of one table
    /// must have the same length. Unknown ids are skipped.
    fn set_embeddings(&self, table: VectorTable, items: &[(String, Vec<f32>)]) -> Result<()> {
        let _ = (table, items);
        bail!("set_embeddings is not implemented by this store")
    }

    /// Ids of rows in `table` that have no embedding yet, at most `limit`,
    /// with the text to embed.
    fn missing_embeddings(&self, table: VectorTable, limit: usize) -> Result<Vec<(String, String)>> {
        let _ = (table, limit);
        bail!("missing_embeddings is not implemented by this store")
    }

    /// Rows of `table` nearest to `query` by cosine similarity, as (id, similarity),
    /// best first, similarity >= `min_similarity`. Empty when the table has no embeddings.
    fn vector_search(
        &self,
        table: VectorTable,
        query: &[f32],
        limit: usize,
        min_similarity: f64,
    ) -> Result<Vec<(String, f64)>> {
        let _ = (table, query, limit, min_similarity);
        bail!("vector_search is not implemented by this store")
    }
}
