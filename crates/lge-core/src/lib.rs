//! Shared graph model for the Local Graph Intelligence Engine.

use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRec {
    pub id: String,
    /// Path relative to the indexed root, with forward slashes.
    pub path: String,
    pub service: String,
    pub lang: String,
    pub content_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolRec {
    pub id: String,
    pub name: String,
    pub qualified_name: String,
    /// module | class | function | method
    pub kind: String,
    pub signature: String,
    pub doc: String,
    pub file_path: String,
    pub service: String,
    pub start_line: i64,
    pub end_line: i64,
    pub is_test: bool,
}

/// A call expression as written in the source, before resolution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallSite {
    pub id: String,
    pub caller_id: String,
    pub file_path: String,
    /// Called name: `foo` in `foo()`, `bar` in `x.bar()`.
    pub name: String,
    /// "" for a bare call, "self" for self/cls, otherwise the receiver text.
    pub receiver: String,
    pub line: i64,
}

/// An import statement as written in the source, before resolution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportSite {
    pub id: String,
    pub file_path: String,
    /// Dotted module, with leading dots kept for relative imports.
    pub module: String,
    /// Imported name for `from m import name`, "" for `import m`.
    pub name: String,
    /// Local name bound by the import.
    pub alias: String,
}

#[derive(Debug, Clone, Default)]
pub struct ParsedFile {
    pub file: Option<FileRec>,
    pub symbols: Vec<SymbolRec>,
    pub calls: Vec<CallSite>,
    pub imports: Vec<ImportSite>,
}

/// How an edge was established. An inferred edge is never reported as verified.
pub mod method {
    pub const STATIC: &str = "static";
    pub const INFERRED: &str = "inferred";
    pub const UNRESOLVED: &str = "unresolved";
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
