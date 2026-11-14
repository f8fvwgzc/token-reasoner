//! Shared graph model for the Local Graph Intelligence Engine.
//!
//! This crate is the contract between the other crates: records produced by the
//! indexer, the input and output of the resolver, the node and edge types the
//! store persists, and the usage-log types. It has no database access.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Ids
// ---------------------------------------------------------------------------

/// Stable id: the same inputs always give the same id, so re-indexing an
/// unchanged file produces identical nodes.
pub fn stable_id(parts: &[&str]) -> String {
    let mut h = blake3::Hasher::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update(&[0]);
    }
    h.finalize().to_hex()[..16].to_string()
}

pub fn content_hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex()[..32].to_string()
}

pub fn file_id(path: &str) -> String {
    stable_id(&["file", path])
}

pub fn symbol_id(path: &str, qualified_name: &str, kind: &str) -> String {
    stable_id(&["symbol", path, qualified_name, kind])
}

pub fn service_id(name: &str) -> String {
    stable_id(&["service", name])
}

pub fn repository_id(path: &str) -> String {
    stable_id(&["repository", path])
}

/// `http_method` is upper-cased and `route` is normalized with [`normalize_route`].
pub fn endpoint_id(http_method: &str, route: &str) -> String {
    stable_id(&["endpoint", &http_method.to_uppercase(), &normalize_route(route)])
}

pub fn table_id(name: &str) -> String {
    stable_id(&["table", &name.to_lowercase()])
}

pub fn queue_id(name: &str) -> String {
    stable_id(&["queue", name])
}

/// Id of the placeholder node that stands for "a symbol called `name`, target unknown".
pub fn unresolved_id(name: &str) -> String {
    format!("unresolved:{name}")
}

/// Canonical form of an HTTP route or URL path, so that a route declaration and a
/// client call can be compared:
///
/// * scheme and host are dropped (`http://orders-api/orders/1` -> `/orders/{}` only
///   when the segment is a parameter, see below; the literal `1` is kept),
/// * the query string and fragment are dropped,
/// * parameter segments `{id}`, `:id`, `<int:id>`, `[id]` and template holes
///   (`${...}`, `{}` , `%s`) become `{}`,
/// * a trailing slash is dropped and a leading slash is added.
pub fn normalize_route(route: &str) -> String {
    let mut r = route.trim();
    if let Some(i) = r.find("://") {
        let rest = &r[i + 3..];
        r = match rest.find('/') {
            Some(j) => &rest[j..],
            None => "/",
        };
    }
    // A template hole at the very start stands for a base URL: `${BASE}/orders/1`.
    for open in ["${", "{"] {
        if r.starts_with(open) {
            if let Some(end) = r.find('}') {
                if r[end + 1..].starts_with('/') || r.len() == end + 1 {
                    r = &r[end + 1..];
                }
            }
            break;
        }
    }
    let r = r.split(['?', '#']).next().unwrap_or("");
    let mut out = String::new();
    for seg in r.split('/').filter(|s| !s.is_empty()) {
        out.push('/');
        let is_param = seg.starts_with(':')
            || (seg.starts_with('{') && seg.ends_with('}'))
            || (seg.starts_with('<') && seg.ends_with('>'))
            || (seg.starts_with('[') && seg.ends_with(']'))
            || seg.contains("${")
            || seg == "%s"
            || seg == "%d";
        if is_param {
            out.push_str("{}");
        } else {
            out.push_str(seg);
        }
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

// ---------------------------------------------------------------------------
// Vocabulary
// ---------------------------------------------------------------------------

/// How an edge was established. An inferred edge is never reported as verified,
/// and a guess is never stored as `static`.
pub mod method {
    /// Resolved through the same file, `self`/`this`, an import, or a literal name.
    pub const STATIC: &str = "static";
    /// A single plausible target found by name only.
    pub const INFERRED: &str = "inferred";
    /// Several candidates or none that can be chosen; stored, never guessed.
    pub const UNRESOLVED: &str = "unresolved";
}

/// Confidence bands. static 0.90 to 0.95, inferred 0.40 to 0.60, unresolved 0.
pub mod confidence {
    pub const SAME_FILE: f64 = 0.95;
    pub const IMPORTED: f64 = 0.90;
    /// A name written as a string literal in the source (route, table, queue).
    pub const LITERAL: f64 = 0.90;
    pub const INFERRED_HIGH: f64 = 0.60;
    pub const INFERRED_BARE: f64 = 0.50;
    pub const INFERRED_RECEIVER: f64 = 0.40;
    pub const UNRESOLVED: f64 = 0.0;
}

/// Symbol kinds.
pub mod kind {
    pub const MODULE: &str = "module";
    pub const CLASS: &str = "class";
    pub const FUNCTION: &str = "function";
    pub const METHOD: &str = "method";
    pub const INTERFACE: &str = "interface";
    pub const TYPE: &str = "type";
    pub const ENUM: &str = "enum";
    /// Placeholder target of an unresolved edge.
    pub const UNRESOLVED: &str = "unresolved";
}

pub mod lang {
    pub const PYTHON: &str = "python";
    pub const TYPESCRIPT: &str = "typescript";
    pub const JAVASCRIPT: &str = "javascript";
}

/// Kinds of [`FactSite`](crate::FactSite).
pub mod fact {
    /// The symbol handles an HTTP route. key = route, detail = HTTP method (upper case, `ANY` if unknown).
    pub const API_EXPOSE: &str = "api_expose";
    /// The symbol calls an HTTP endpoint. key = URL or path as written, detail = HTTP method.
    pub const API_CALL: &str = "api_call";
    /// The symbol reads a database table. key = table name.
    pub const DB_READ: &str = "db_read";
    /// The symbol writes a database table. key = table name.
    pub const DB_WRITE: &str = "db_write";
    /// The class is an ORM model of a table. key = table name. Used to turn
    /// `session.query(Task)` into a table edge.
    pub const TABLE_MODEL: &str = "table_model";
    /// The symbol publishes to a queue, exchange or topic. key = name, detail = kind (`queue`, `exchange`, `topic`).
    pub const QUEUE_PUBLISH: &str = "queue_publish";
    /// The symbol consumes from a queue or topic. key = name, detail = kind.
    pub const QUEUE_CONSUME: &str = "queue_consume";
    /// The class extends a base. key = base name as written (`Base`, `models.Base`).
    pub const INHERITS: &str = "inherits";
    /// The class implements an interface. key = interface name as written.
    pub const IMPLEMENTS: &str = "implements";
    /// The symbol mentions another symbol without calling it (callback, decorator
    /// argument, type annotation). key = name as written.
    pub const REFERENCE: &str = "reference";
    /// The symbol is its file's default export (TypeScript and JavaScript).
    /// key = symbol name. Used to resolve `import x from "./file"`.
    pub const DEFAULT_EXPORT: &str = "default_export";
    /// A comment that says why (`NOTE:`, `WHY:`, `HACK:`, `IMPORTANT:`, `TODO:`,
    /// `FIXME:`) inside the symbol. key = the comment text, at most 200 chars.
    /// Produces no edge; it is indexed for search and shown in evidence packs.
    pub const RATIONALE: &str = "rationale";
}

// ---------------------------------------------------------------------------
// Records produced by the indexer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FileRec {
    pub id: String,
    /// Path relative to the indexed root, with forward slashes.
    pub path: String,
    pub service: String,
    /// One of [`lang`].
    pub lang: String,
    pub content_hash: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SymbolRec {
    pub id: String,
    pub name: String,
    pub qualified_name: String,
    /// One of [`kind`].
    pub kind: String,
    pub signature: String,
    pub doc: String,
    /// Source text of the definition, capped by the indexer. Empty for modules.
    pub body: String,
    pub file_path: String,
    pub service: String,
    pub start_line: i64,
    pub end_line: i64,
    pub is_test: bool,
}

/// A call expression as written in the source, before resolution.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CallSite {
    pub id: String,
    pub caller_id: String,
    pub file_path: String,
    /// Called name: `foo` in `foo()`, `bar` in `x.bar()`.
    pub name: String,
    /// "" for a bare call, "self" for `self`/`cls`/`this`, otherwise the receiver text.
    pub receiver: String,
    pub line: i64,
}

/// An import statement as written in the source, before resolution.
///
/// Python: `module` is the dotted module with leading dots kept for relative
/// imports; `name` is the imported name for `from m import name`, "" for `import m`.
///
/// TypeScript and JavaScript: `module` is the specifier as written (`./service`,
/// `../shared/util`, `express`); `name` is the exported name, `default` for a
/// default import and `*` for a namespace import or `require`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ImportSite {
    pub id: String,
    pub file_path: String,
    pub module: String,
    pub name: String,
    /// Local name bound by the import.
    pub alias: String,
}

/// Something a symbol does that becomes a non-call edge: exposing or calling an
/// HTTP route, reading or writing a table, publishing or consuming a queue,
/// inheriting, or referencing another symbol. Kinds are in [`fact`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FactSite {
    pub id: String,
    /// The symbol the fact belongs to (the enclosing function, method or class;
    /// the module symbol for top-level code).
    pub symbol_id: String,
    pub file_path: String,
    /// One of [`fact`].
    pub kind: String,
    /// Route, URL, table name, queue name or symbol name. See [`fact`].
    pub key: String,
    /// HTTP method, queue kind, or "" when not applicable.
    pub detail: String,
    pub line: i64,
    pub end_line: i64,
    /// True when `key` is written as a plain string literal (or a bare identifier
    /// for `inherits`/`implements`/`reference`). False when it was assembled from
    /// a template, concatenation or variable; such a fact can be at most `inferred`.
    pub literal: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ParsedFile {
    pub file: Option<FileRec>,
    pub symbols: Vec<SymbolRec>,
    pub calls: Vec<CallSite>,
    pub imports: Vec<ImportSite>,
    pub facts: Vec<FactSite>,
}

// ---------------------------------------------------------------------------
// Graph nodes and edges
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EdgeType {
    Contains,
    Imports,
    Calls,
    Inherits,
    Implements,
    References,
    ExposesApi,
    CallsApi,
    ReadsFrom,
    WritesTo,
    Publishes,
    Consumes,
    TestedBy,
    DependsOn,
}

impl EdgeType {
    pub const ALL: [EdgeType; 14] = [
        EdgeType::Contains,
        EdgeType::Imports,
        EdgeType::Calls,
        EdgeType::Inherits,
        EdgeType::Implements,
        EdgeType::References,
        EdgeType::ExposesApi,
        EdgeType::CallsApi,
        EdgeType::ReadsFrom,
        EdgeType::WritesTo,
        EdgeType::Publishes,
        EdgeType::Consumes,
        EdgeType::TestedBy,
        EdgeType::DependsOn,
    ];

    /// Name of the relationship table.
    pub fn as_str(self) -> &'static str {
        match self {
            EdgeType::Contains => "CONTAINS",
            EdgeType::Imports => "IMPORTS",
            EdgeType::Calls => "CALLS",
            EdgeType::Inherits => "INHERITS",
            EdgeType::Implements => "IMPLEMENTS",
            EdgeType::References => "REFERENCES",
            EdgeType::ExposesApi => "EXPOSES_API",
            EdgeType::CallsApi => "CALLS_API",
            EdgeType::ReadsFrom => "READS_FROM",
            EdgeType::WritesTo => "WRITES_TO",
            EdgeType::Publishes => "PUBLISHES",
            EdgeType::Consumes => "CONSUMES",
            EdgeType::TestedBy => "TESTED_BY",
            EdgeType::DependsOn => "DEPENDS_ON",
        }
    }

    pub fn parse(s: &str) -> Option<EdgeType> {
        EdgeType::ALL.into_iter().find(|t| t.as_str().eq_ignore_ascii_case(s))
    }

    /// Dependency direction of the edge, used for change impact.
    ///
    /// `Some(true)`: the `from` node depends on the `to` node (a caller depends on
    /// its callee, a reader on its table, a consumer on its queue).
    /// `Some(false)`: the `to` node depends on the `from` node (an endpoint on its
    /// handler, a table's content on its writer, a queue's content on its
    /// publisher, a test on the symbol it tests).
    /// `None`: structural, not a dependency (CONTAINS).
    pub fn from_depends_on_to(self) -> Option<bool> {
        match self {
            EdgeType::Contains => None,
            EdgeType::Imports
            | EdgeType::Calls
            | EdgeType::Inherits
            | EdgeType::Implements
            | EdgeType::References
            | EdgeType::CallsApi
            | EdgeType::ReadsFrom
            | EdgeType::Consumes
            | EdgeType::DependsOn => Some(true),
            EdgeType::ExposesApi | EdgeType::WritesTo | EdgeType::Publishes | EdgeType::TestedBy => Some(false),
        }
    }
}

impl std::fmt::Display for EdgeType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum NodeLabel {
    Repository,
    Service,
    File,
    Symbol,
    ApiEndpoint,
    DbTable,
    Queue,
}

impl NodeLabel {
    pub const ALL: [NodeLabel; 7] = [
        NodeLabel::Repository,
        NodeLabel::Service,
        NodeLabel::File,
        NodeLabel::Symbol,
        NodeLabel::ApiEndpoint,
        NodeLabel::DbTable,
        NodeLabel::Queue,
    ];

    /// Name of the node table.
    pub fn as_str(self) -> &'static str {
        match self {
            NodeLabel::Repository => "Repository",
            NodeLabel::Service => "Service",
            NodeLabel::File => "File",
            NodeLabel::Symbol => "Symbol",
            NodeLabel::ApiEndpoint => "ApiEndpoint",
            NodeLabel::DbTable => "DbTable",
            NodeLabel::Queue => "Queue",
        }
    }

    pub fn parse(s: &str) -> Option<NodeLabel> {
        NodeLabel::ALL.into_iter().find(|t| t.as_str().eq_ignore_ascii_case(s))
    }
}

/// Direction of a one-hop expansion, relative to the given node ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    /// Edges that start at the given nodes.
    Out,
    /// Edges that end at the given nodes.
    In,
}

/// One node of any label, in a common shape.
///
/// | label | name | qualified_name | kind |
/// |---|---|---|---|
/// | Symbol | short name | dotted name | [`kind`] |
/// | File | path | path | language |
/// | Service | service name | root directory | "" |
/// | Repository | path | path | head commit |
/// | ApiEndpoint | `GET /orders/{}` | normalized route | HTTP method |
/// | DbTable | table name | table name | "" |
/// | Queue | queue name | queue name | `queue`, `exchange` or `topic` |
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeRec {
    pub id: String,
    pub label: NodeLabel,
    pub name: String,
    pub qualified_name: String,
    pub kind: String,
    pub signature: String,
    pub doc: String,
    pub file_path: String,
    pub service: String,
    pub start_line: i64,
    pub end_line: i64,
    pub is_test: bool,
}

impl NodeRec {
    /// A node with only id, label, name, qualified name and kind set.
    pub fn bare(id: &str, label: NodeLabel, name: &str, qualified_name: &str, kind: &str) -> NodeRec {
        NodeRec {
            id: id.to_string(),
            label,
            name: name.to_string(),
            qualified_name: qualified_name.to_string(),
            kind: kind.to_string(),
            signature: String::new(),
            doc: String::new(),
            file_path: String::new(),
            service: String::new(),
            start_line: 0,
            end_line: 0,
            is_test: false,
        }
    }
}

/// One edge with its provenance. Every edge carries all of these fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EdgeRec {
    pub from: String,
    pub to: String,
    pub edge_type: EdgeType,
    pub confidence: f64,
    /// One of [`method`].
    pub method: String,
    /// File and lines of the source text that justifies the edge.
    pub evidence_file: String,
    pub start_line: i64,
    pub end_line: i64,
}

/// A search or lookup result for a symbol.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hit {
    pub id: String,
    pub name: String,
    pub qualified_name: String,
    pub kind: String,
    pub file_path: String,
    pub service: String,
    pub start_line: i64,
    pub end_line: i64,
    pub is_test: bool,
    pub score: f64,
}

// ---------------------------------------------------------------------------
// Resolver input and output
// ---------------------------------------------------------------------------

/// Everything the resolver needs, loaded from the store. Symbol bodies may be
/// empty here; the resolver does not read them.
#[derive(Debug, Clone, Default)]
pub struct ResolveInput {
    pub files: Vec<FileRec>,
    pub symbols: Vec<SymbolRec>,
    pub calls: Vec<CallSite>,
    pub imports: Vec<ImportSite>,
    pub facts: Vec<FactSite>,
}

/// Everything the resolver derives. The store deletes all previously derived
/// nodes and edges and writes these.
///
/// `nodes` holds only derived nodes: ApiEndpoint, DbTable, Queue, and placeholder
/// Symbols of kind `unresolved`. File, Symbol, Service and Repository nodes and
/// the CONTAINS edges are written by the store when a file is inserted.
#[derive(Debug, Clone, Default)]
pub struct Resolved {
    pub nodes: Vec<NodeRec>,
    pub edges: Vec<EdgeRec>,
    pub stats: EdgeStats,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EdgeStats {
    /// Edge count by `"<EDGE_TYPE>/<method>"`, for example `"CALLS/static"`.
    pub counts: BTreeMap<String, usize>,
    /// Calls to names that do not exist in the repository; counted, not stored.
    pub calls_external: usize,
    /// Imports of modules that are not in the repository; counted, not stored.
    pub imports_external: usize,
}

impl EdgeStats {
    pub fn add(&mut self, edge_type: EdgeType, method: &str) {
        *self
            .counts
            .entry(format!("{}/{}", edge_type.as_str(), method))
            .or_insert(0) += 1;
    }

    pub fn get(&self, edge_type: EdgeType, method: &str) -> usize {
        self.counts
            .get(&format!("{}/{}", edge_type.as_str(), method))
            .copied()
            .unwrap_or(0)
    }

    /// Total over all methods for one edge type.
    pub fn total(&self, edge_type: EdgeType) -> usize {
        let prefix = format!("{}/", edge_type.as_str());
        self.counts
            .iter()
            .filter(|(k, _)| k.starts_with(&prefix))
            .map(|(_, v)| *v)
            .sum()
    }
}

// ---------------------------------------------------------------------------
// Index metadata
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FileInfo {
    pub id: String,
    pub path: String,
    pub service: String,
    pub lang: String,
    pub content_hash: String,
    /// Commit the repository was at when the file was indexed; "" outside git.
    pub indexed_commit: String,
    /// Unix seconds.
    pub indexed_at: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct IndexRun {
    pub id: String,
    pub started_at: i64,
    pub finished_at: i64,
    /// `running`, `ok` or `failed`.
    pub status: String,
    pub files_changed: i64,
    pub schema_version: i64,
}

// ---------------------------------------------------------------------------
// Reasoning level, usage log, handoff notes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Low,
    Medium,
    Top,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Low => "low",
            Level::Medium => "medium",
            Level::Top => "top",
        }
    }

    pub fn parse(s: &str) -> Option<Level> {
        match s.to_ascii_lowercase().as_str() {
            "low" => Some(Level::Low),
            "medium" => Some(Level::Medium),
            "top" | "high" => Some(Level::Top),
            _ => None,
        }
    }

    /// The next level up, or `None` at the top. Used when local checks fail.
    pub fn escalate(self) -> Option<Level> {
        match self {
            Level::Low => Some(Level::Medium),
            Level::Medium => Some(Level::Top),
            Level::Top => None,
        }
    }
}

impl std::fmt::Display for Level {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

pub mod outcome {
    pub const SUCCESS: &str = "success";
    pub const FAILURE: &str = "failure";
    pub const UNKNOWN: &str = "unknown";
}

/// One session attempt for one task. A task that is escalated has several
/// records with the same `task_id` and rising `attempt`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageRecord {
    pub id: String,
    /// Groups the attempts of one task.
    pub task_id: String,
    pub task: String,
    /// 1 for the first attempt; higher values are escalation retries.
    pub attempt: i64,
    /// `low`, `medium` or `top`.
    pub level: String,
    /// The router's reason for the level.
    pub reason: String,
    /// `codex`, `dry-run`, or the name of another runner.
    pub runner: String,
    pub model: String,
    pub session_id: String,
    /// Unix seconds.
    pub started_at: i64,
    pub finished_at: i64,
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub reasoning_tokens: i64,
    pub output_tokens: i64,
    /// Size of the evidence pack the session started from.
    pub pack_tokens: i64,
    /// One of [`outcome`].
    pub outcome: String,
    /// True when this attempt exists because the previous one failed local checks.
    pub escalated: bool,
    pub files: Vec<String>,
    pub services: Vec<String>,
    /// The level the router would have chosen. Equal to `level` in routed mode;
    /// in fixed mode it shows what routing would have done.
    pub recommended_level: String,
    /// Which levers were on, for comparison: `baseline`, `pack`, `pack+mcp`, ...
    pub mode: String,
}

/// Short note written at the end of a task, read at the start of the next.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HandoffNote {
    pub id: String,
    pub task_id: String,
    pub task: String,
    /// Unix seconds.
    pub created_at: i64,
    pub text: String,
}

// ---------------------------------------------------------------------------
// Graph metrics, long-term memory, pages
// ---------------------------------------------------------------------------

/// Numbers computed over the whole symbol graph after each resolve.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SymbolMetrics {
    pub id: String,
    /// Global PageRank over calls, references and inheritance; sums to 1 over symbols.
    pub rank: f64,
    /// Community id, 0 = the largest; -1 = none.
    pub community: i64,
    /// Number of distinct neighbours over non-CONTAINS edges between symbols.
    pub degree: i64,
}

/// Kinds of [`MemoryRec`].
pub mod memory_kind {
    /// A fact about the system or the codebase.
    pub const WORLD: &str = "world";
    /// What was done in a task, and how it went.
    pub const EXPERIENCE: &str = "experience";
    /// A choice that was made, with its reason.
    pub const DECISION: &str = "decision";
    /// How things are done in this repository.
    pub const CONVENTION: &str = "convention";
    /// A consolidated statement derived from several facts.
    pub const OBSERVATION: &str = "observation";
    pub const ALL: [&str; 5] = [WORLD, EXPERIENCE, DECISION, CONVENTION, OBSERVATION];
}

/// One remembered fact. Facts are superseded, never deleted: a newer fact with
/// the same key sets `invalid_at` and `expired_at` on the older one.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MemoryRec {
    pub id: String,
    /// One of [`memory_kind`].
    pub kind: String,
    pub text: String,
    /// What the fact is about, as a stable key (`auth.token-expiry`). A newer fact
    /// with the same non-empty key supersedes this one. "" = never superseded.
    pub key: String,
    pub tags: Vec<String>,
    /// Where it came from: a task id, a session id, a file path or `user`.
    pub source: String,
    /// When it was stored. Unix seconds, like every time below.
    pub created_at: i64,
    /// When it became true.
    pub valid_at: i64,
    /// When it stopped being true; 0 while it holds.
    pub invalid_at: i64,
    /// When the store learned it was superseded; 0 while current.
    pub expired_at: i64,
    /// How often it was asserted. A repeat raises this instead of inserting.
    pub proof_count: i64,
    /// Ids of the Symbol, File and Service nodes the text mentions.
    pub links: Vec<String>,
}

/// A markdown page: a curated view over tagged facts, or a generated view of the graph.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PageRec {
    pub id: String,
    /// File-name-safe unique name: `decisions`, `graph-overview`.
    pub slug: String,
    pub title: String,
    /// Markdown.
    pub body: String,
    /// Facts carrying any of these tags belong to the page.
    pub tags: Vec<String>,
    /// `memory` (built from facts), `graph` (built from the code graph) or `manual`.
    pub kind: String,
    pub updated_at: i64,
    /// `created_at` of the newest fact the body includes. The page is stale when
    /// a newer in-scope fact exists.
    pub last_memory_seen_at: i64,
}

/// Which table a vector belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VectorTable {
    Symbol,
    Memory,
}

// ---------------------------------------------------------------------------
// Search text
// ---------------------------------------------------------------------------

/// Lower-cased words of an identifier, path or sentence, in order.
///
/// Splits on every non-alphanumeric character, on a lower-to-upper case change
/// (`createOrder` -> `create order`), at the end of an acronym (`HTTPServer` ->
/// `http server`) and between letters and digits (`sha256sum` -> `sha 256 sum`).
/// One-character words are dropped. The index side and the query side both use
/// this function, so `getOrderHandler` is found by "order handler".
pub fn split_words(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if !c.is_alphanumeric() {
            flush_word(&mut cur, &mut words);
            continue;
        }
        if let Some(&prev) = i.checked_sub(1).and_then(|j| chars.get(j)) {
            let next_is_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            let boundary = (prev.is_lowercase() && c.is_uppercase())
                || (prev.is_uppercase() && c.is_uppercase() && next_is_lower)
                || (prev.is_alphabetic() && c.is_numeric())
                || (prev.is_numeric() && c.is_alphabetic());
            if boundary {
                flush_word(&mut cur, &mut words);
            }
        }
        cur.extend(c.to_lowercase());
    }
    flush_word(&mut cur, &mut words);
    words
}

fn flush_word(cur: &mut String, words: &mut Vec<String>) {
    if cur.chars().count() > 1 {
        words.push(std::mem::take(cur));
    } else {
        cur.clear();
    }
}

/// The text the full-text index stores for a symbol besides its raw fields: the
/// distinct words of the name, the qualified name, the file path, the signature
/// and the doc line, in that order, at most 120 words.
pub fn search_text(name: &str, qualified_name: &str, file_path: &str, signature: &str, doc: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for part in [name, qualified_name, file_path, signature, doc] {
        for w in split_words(part) {
            if out.len() < 120 && !out.contains(&w) {
                out.push(w);
            }
        }
    }
    out.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_stable_and_distinct() {
        assert_eq!(stable_id(&["a", "b"]), stable_id(&["a", "b"]));
        assert_ne!(stable_id(&["a", "b"]), stable_id(&["ab", ""]));
        assert_eq!(file_id("x/y.py"), stable_id(&["file", "x/y.py"]));
        assert_eq!(
            symbol_id("x.py", "x.f", "function"),
            stable_id(&["symbol", "x.py", "x.f", "function"])
        );
        assert_eq!(table_id("Orders"), table_id("orders"));
    }

    #[test]
    fn routes_normalize_to_one_form() {
        for r in [
            "/orders/{order_id}",
            "/orders/:id",
            "/orders/<int:order_id>",
            "/orders/[id]/",
            "http://orders-api/orders/${id}?verbose=1",
            "${ORDERS_URL}/orders/${id}",
            "orders/%s",
        ] {
            assert_eq!(normalize_route(r), "/orders/{}", "{r}");
        }
        assert_eq!(normalize_route("http://orders-api/orders"), "/orders");
        assert_eq!(normalize_route("https://host"), "/");
        assert_eq!(normalize_route(""), "/");
        assert_eq!(endpoint_id("get", "/orders/:id"), endpoint_id("GET", "/orders/{x}"));
        assert_ne!(endpoint_id("GET", "/orders"), endpoint_id("POST", "/orders"));
    }

    #[test]
    fn edge_types_round_trip() {
        for t in EdgeType::ALL {
            assert_eq!(EdgeType::parse(t.as_str()), Some(t));
        }
        assert_eq!(EdgeType::Calls.from_depends_on_to(), Some(true));
        assert_eq!(EdgeType::ExposesApi.from_depends_on_to(), Some(false));
        assert_eq!(EdgeType::Contains.from_depends_on_to(), None);
        assert_eq!(Level::Low.escalate(), Some(Level::Medium));
        assert_eq!(Level::Top.escalate(), None);
    }

    #[test]
    fn identifiers_split_into_words() {
        assert_eq!(split_words("createOrderHandler"), ["create", "order", "handler"]);
        assert_eq!(split_words("save_task"), ["save", "task"]);
        assert_eq!(split_words("HTTPServer"), ["http", "server"]);
        assert_eq!(split_words("sha256sum"), ["sha", "256", "sum"]);
        assert_eq!(
            split_words("web_gateway/src/ordersClient.ts"),
            ["web", "gateway", "src", "orders", "client", "ts"]
        );
        assert_eq!(split_words("a b  -- x"), Vec::<String>::new());
        assert_eq!(split_words("Ünïcode Straße"), ["ünïcode", "straße"]);
        assert_eq!(
            search_text(
                "getOrder",
                "web.routes.getOrder",
                "web/routes.ts",
                "function getOrder(id: string)",
                "Fetch one order."
            ),
            "get order web routes ts function id string fetch one"
        );
    }
}
