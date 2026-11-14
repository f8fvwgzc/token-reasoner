//! Storage for the code graph: the [`GraphStore`] contract and its LadybugDB
//! adapter. No other crate talks to `lbug` directly.

mod graph_store;
mod index;
mod meta;
mod nodes;
mod resolved;
mod schema;
mod search;
mod traverse;
mod usage;
mod values;

pub use graph_store::GraphStore;
pub use schema::SCHEMA_VERSION;
pub use search::tokens;

use anyhow::{bail, Context, Result};
use lbug::{Connection, Database, SystemConfig};
use lge_core::{
    Direction, EdgeRec, EdgeType, FileInfo, HandoffNote, Hit, IndexRun, NodeLabel, NodeRec, ParsedFile, ResolveInput,
    Resolved, UsageRecord,
};
use resolved::EdgeLoad;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

/// Database files that are open in this process.
///
/// LadybugDB locks its file against other processes only. A second `Database`
/// on the same file in the same process is let in, does not see what the first
/// one commits, and the two overwrite each other's pages. The store refuses
/// the second open instead.
static OPEN_FILES: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());

/// The entry of one open database file in [`OPEN_FILES`], removed when dropped.
/// `None` for an in-memory database, which is never shared.
struct FileClaim(Option<PathBuf>);

impl FileClaim {
    fn new(path: &Path) -> Result<Self> {
        if path.as_os_str().is_empty() || path == Path::new(":memory:") {
            return Ok(FileClaim(None));
        }
        let key = canonical(path);
        let mut open = OPEN_FILES.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if !open.insert(key.clone()) {
            bail!(
                "the database at {} is already open in this process; share the one store instead of opening it again",
                path.display()
            );
        }
        Ok(FileClaim(Some(key)))
    }
}

impl Drop for FileClaim {
    fn drop(&mut self) {
        if let Some(key) = &self.0 {
            OPEN_FILES
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(key);
        }
    }
}

/// One spelling per file, so `a/../graph.lbug`, a relative path and a symlink
/// to the directory all name the same claim. The file itself may not exist yet.
fn canonical(path: &Path) -> PathBuf {
    if let Ok(found) = path.canonicalize() {
        return found;
    }
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    match (dir.canonicalize(), path.file_name()) {
        (Ok(dir), Some(name)) => dir.join(name),
        _ => std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()),
    }
}

/// The LadybugDB adapter. One process owns the database file.
pub struct LadybugStore {
    db: Database,
    /// True when the LadybugDB `fts` extension is loaded (BM25 search).
    /// Interior, because `install_fts` takes `&self`.
    fts: AtomicBool,
    /// LadybugDB allows one write transaction at a time. Writers queue here
    /// instead of failing when two threads share the store.
    write: Mutex<()>,
    /// Edges the last `replace_edges` call left out because an endpoint was missing.
    skipped_edges: AtomicUsize,
    /// Why the last `replace_edges` call could not load its edges in bulk, if it could not.
    bulk_load_error: Mutex<Option<String>>,
    /// True when the database has a full-text index but the extension that
    /// maintains it is not loaded. LadybugDB then refuses every change to the
    /// Symbol table; the store refuses first, in words that say what to do.
    index_needs_fts: AtomicBool,
    /// Declared after `db`, so the file is released only once the database is closed.
    _claim: FileClaim,
}

/// The name the other crates use.
pub type Store = LadybugStore;

impl std::fmt::Debug for LadybugStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LadybugStore")
            .field("fts", &self.fts_enabled())
            .finish_non_exhaustive()
    }
}

impl LadybugStore {
    /// Open or create the database at `path`, migrate an older schema, and
    /// load the `fts` extension when it is installed.
    ///
    /// A database file can be open once per process: the store is `Send + Sync`
    /// and is meant to be shared.
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with(path, true)
    }

    /// Open as on a machine where the `fts` extension is not installed: it is
    /// not loaded, and search uses the substring scorer. For tests of that path.
    #[doc(hidden)]
    pub fn open_without_fts(path: &Path) -> Result<Self> {
        Self::open_with(path, false)
    }

    fn open_with(path: &Path, load_fts: bool) -> Result<Self> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        }
        let claim = FileClaim::new(path)?;
        let db = Database::new(path, SystemConfig::default())
            .with_context(|| format!("cannot open the database at {}", path.display()))?;
        let store = LadybugStore {
            db,
            fts: AtomicBool::new(false),
            write: Mutex::new(()),
            skipped_edges: AtomicUsize::new(0),
            bulk_load_error: Mutex::new(None),
            index_needs_fts: AtomicBool::new(false),
            _claim: claim,
        };
        {
            let conn = store.conn()?;
            // The extension must have been installed once with `lge setup`.
            // It is loaded before the schema is touched: a table that has a
            // full-text index cannot be written or dropped without it.
            let fts = load_fts && conn.query("LOAD fts;").is_ok();
            schema::prepare(&conn, fts)?;
            if fts {
                search::ensure_fts_index(&conn)?;
            } else if !schema::fts_indexes(&conn)?.is_empty() {
                store.index_needs_fts.store(true, Ordering::Relaxed);
            }
            store.fts.store(fts, Ordering::Relaxed);
        }
        Ok(store)
    }

    /// Fail before a transaction starts when the Symbol table cannot be changed.
    fn check_symbols_writable(&self) -> Result<()> {
        if self.index_needs_fts.load(Ordering::Relaxed) {
            bail!(
                "this database has a full-text index, but the fts extension is not loaded in this process, so its \
                 symbols cannot be changed; install the extension with `lge setup` (a program must be linked with \
                 -rdynamic to load it) or index into a new database"
            );
        }
        Ok(())
    }

    /// Number of edges the last [`GraphStore::replace_edges`] call did not
    /// store because one of their endpoints does not exist.
    pub fn last_skipped_edges(&self) -> usize {
        self.skipped_edges.load(Ordering::Relaxed)
    }

    /// Why the last [`GraphStore::replace_edges`] call fell back from the bulk
    /// load to one statement per edge, or `None` when it did not. The graph is
    /// the same either way; the fallback is only slower.
    pub fn last_bulk_load_error(&self) -> Option<String> {
        self.bulk_load_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub(crate) fn conn(&self) -> Result<Connection<'_>> {
        Connection::new(&self.db).context("cannot connect to the database")
    }

    pub(crate) fn write_lock(&self) -> MutexGuard<'_, ()> {
        // The lock guards no data, so a writer that panicked leaves nothing broken behind.
        self.write.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Run `f` as the only writer, on its own connection, without a transaction.
    pub(crate) fn write<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let _writer = self.write_lock();
        f(&self.conn()?)
    }

    /// Run `f` as the only writer inside one transaction: all of it is stored, or none.
    pub(crate) fn write_txn<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.write(|conn| {
            values::run(conn, "BEGIN TRANSACTION;")?;
            match f(conn) {
                Ok(v) => {
                    values::run(conn, "COMMIT;")?;
                    Ok(v)
                }
                Err(e) => {
                    let _ = conn.query("ROLLBACK;");
                    Err(e)
                }
            }
        })
    }
}

impl GraphStore for LadybugStore {
    fn file_hashes(&self) -> Result<HashMap<String, (String, String)>> {
        index::file_hashes(&self.conn()?)
    }

    fn file_info(&self, path: &str) -> Result<Option<FileInfo>> {
        index::file_info(&self.conn()?, path)
    }

    fn delete_file(&self, path: &str) -> Result<()> {
        self.check_symbols_writable()?;
        self.write_txn(|conn| index::delete_file(conn, path, None))
    }

    fn insert_parsed(&self, parsed: &ParsedFile, commit: &str, now: i64) -> Result<()> {
        self.insert_parsed_bulk(std::slice::from_ref(parsed), commit, now)
    }

    /// Same rows as the per-file path, with fewer commits: files are written
    /// in batches, one transaction per batch.
    fn insert_parsed_bulk(&self, parsed: &[ParsedFile], commit: &str, now: i64) -> Result<()> {
        self.check_symbols_writable()?;
        for batch in parsed.chunks(index::BULK_BATCH) {
            self.write_txn(|conn| {
                let mut writer = index::FileWriter::new(conn, commit, now)?;
                batch.iter().try_for_each(|p| writer.write(p))
            })?;
        }
        Ok(())
    }

    fn load_resolve_input(&self) -> Result<ResolveInput> {
        index::load_resolve_input(&self.conn()?)
    }

    fn replace_edges(&self, resolved: &Resolved, commit: &str, now: i64) -> Result<()> {
        self.check_symbols_writable()?;
        let write = |load| self.write_txn(|conn| resolved::replace(conn, resolved, commit, now, load));
        let mut bulk_error = None;
        let skipped = match write(EdgeLoad::Bulk) {
            Ok(skipped) => skipped,
            // The bulk load is only the faster way. If it fails (no room or no
            // rights for its files, say), the transaction was rolled back and
            // the edges are written by statements, which need the database alone.
            Err(e) if resolved.edges.len() >= resolved::BULK_EDGES => {
                bulk_error = Some(format!("{e:#}"));
                write(EdgeLoad::Statements)?
            }
            Err(e) => return Err(e),
        };
        *self
            .bulk_load_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = bulk_error;
        self.skipped_edges.store(skipped, Ordering::Relaxed);
        if self.fts_enabled() {
            self.write(search::ensure_fts_index)?;
        }
        Ok(())
    }

    fn set_repository(&self, path: &str, head_commit: &str) -> Result<()> {
        self.write_txn(|conn| meta::set_repository(conn, path, head_commit))
    }

    fn repository(&self) -> Result<Option<(String, String)>> {
        meta::repository(&self.conn()?)
    }

    fn begin_run(&self, now: i64) -> Result<String> {
        self.write_txn(|conn| meta::begin_run(conn, now))
    }

    fn finish_run(&self, run_id: &str, now: i64, status: &str, files_changed: i64) -> Result<()> {
        self.write(|conn| meta::finish_run(conn, run_id, now, status, files_changed))
    }

    fn last_run(&self) -> Result<Option<IndexRun>> {
        meta::last_run(&self.conn()?)
    }

    fn fts_enabled(&self) -> bool {
        self.fts.load(Ordering::Relaxed)
    }

    fn install_fts(&self) -> Result<()> {
        self.write(|conn| {
            values::run(conn, "INSTALL fts;")?;
            values::run(conn, "LOAD fts;")?;
            search::ensure_fts_index(conn)
        })?;
        self.fts.store(true, Ordering::Relaxed);
        self.index_needs_fts.store(false, Ordering::Relaxed);
        Ok(())
    }

    fn search(&self, text: &str, limit: usize) -> Result<Vec<Hit>> {
        search::search(&self.conn()?, self.fts_enabled(), text, limit)
    }

    fn find(&self, needle: &str) -> Result<Vec<Hit>> {
        search::find(&self.conn()?, needle)
    }

    fn find_prefix(&self, prefix: &str, limit: usize) -> Result<Vec<Hit>> {
        search::find_prefix(&self.conn()?, prefix, limit)
    }

    fn nodes(&self, ids: &[String]) -> Result<Vec<NodeRec>> {
        nodes::by_ids(&self.conn()?, ids)
    }

    fn nodes_by_label(&self, label: NodeLabel, limit: usize) -> Result<Vec<NodeRec>> {
        nodes::by_label(&self.conn()?, label, limit)
    }

    fn symbols_in_file(&self, path: &str) -> Result<Vec<NodeRec>> {
        nodes::symbols_in_file(&self.conn()?, path)
    }

    fn symbol_body(&self, id: &str) -> Result<Option<String>> {
        nodes::symbol_body(&self.conn()?, id)
    }

    fn edges(&self, ids: &[String], types: &[EdgeType], dir: Direction) -> Result<Vec<EdgeRec>> {
        traverse::edges(&self.conn()?, ids, types, dir)
    }

    fn callers(&self, id: &str, depth: u8) -> Result<Vec<Hit>> {
        traverse::callers(&self.conn()?, id, depth)
    }

    fn unresolved_out(&self, id: &str) -> Result<i64> {
        traverse::unresolved_out(&self.conn()?, id)
    }

    fn stats(&self) -> Result<Vec<(String, i64)>> {
        meta::stats(&self.conn()?)
    }

    fn raw(&self, cypher: &str) -> Result<String> {
        // The query may write, so it waits its turn like any other writer.
        self.write(|conn| Ok(conn.query(cypher)?.to_string()))
    }

    fn put_usage(&self, rec: &UsageRecord) -> Result<()> {
        self.write(|conn| usage::put_usage(conn, rec))
    }

    fn list_usage(&self, since: i64) -> Result<Vec<UsageRecord>> {
        usage::list_usage(&self.conn()?, since)
    }

    fn put_handoff(&self, note: &HandoffNote) -> Result<()> {
        self.write(|conn| usage::put_handoff(conn, note))
    }

    fn latest_handoffs(&self, limit: usize) -> Result<Vec<HandoffNote>> {
        usage::latest_handoffs(&self.conn()?, limit)
    }
}
