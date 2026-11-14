//! The index pipeline: scan, hash, parse, store, resolve. This is the only place
//! that calls both the indexer and the resolver.

use anyhow::{Context, Result};
use lge_core::{EdgeStats, ParsedFile};
use lge_index::ScannedFile;
use lge_store::GraphStore;
use std::collections::HashSet;
use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct IndexOptions {
    /// 0 = the root is one service, 1 = each top-level directory is a service.
    pub service_depth: usize,
    /// Resolve edges even when no file changed (after a resolver upgrade).
    pub force_resolve: bool,
}

impl Default for IndexOptions {
    fn default() -> Self {
        IndexOptions {
            service_depth: 1,
            force_resolve: false,
        }
    }
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct IndexReport {
    pub indexed: usize,
    pub unchanged: usize,
    pub removed: usize,
    /// Files that could not be read or parsed, with the reason. They are skipped.
    pub failed: Vec<(String, String)>,
    /// Edge counts of the resolve step; `None` when nothing changed.
    pub stats: Option<EdgeStats>,
    /// Derived views (rank, communities, generated pages) that could not be
    /// refreshed. The graph itself is complete; these catch up on the next run.
    pub warnings: Vec<String>,
    /// HEAD of the repository, "" outside git.
    pub commit: String,
    pub seconds: f64,
}

pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Bring the graph in line with the files under `root`. Only new, changed and
/// deleted files are processed; edges are resolved again for the whole
/// repository, so an incremental run and a full run give the same graph.
pub fn index_repo(store: &dyn GraphStore, root: &Path, opts: &IndexOptions) -> Result<IndexReport> {
    let started = Instant::now();
    let now = unix_now();
    let run = store.begin_run(now)?;
    let result = index_inner(store, root, opts, now);
    let (status, changed) = match &result {
        Ok(r) => ("ok", (r.indexed + r.removed) as i64),
        Err(_) => ("failed", 0),
    };
    // The run record must not hide the real error.
    let finished = store.finish_run(&run, unix_now(), status, changed);
    let mut report = result?;
    finished?;
    report.seconds = started.elapsed().as_secs_f64();
    Ok(report)
}

fn index_inner(store: &dyn GraphStore, root: &Path, opts: &IndexOptions, now: i64) -> Result<IndexReport> {
    let root = root
        .canonicalize()
        .with_context(|| format!("cannot open {}", root.display()))?;
    let commit = lge_index::git::head_commit(&root).unwrap_or_default();
    store.set_repository(&root.to_string_lossy(), &commit)?;

    let scanned = lge_index::scan(&root, opts.service_depth)?;
    let known = store.file_hashes()?;
    let mut report = IndexReport {
        commit: commit.clone(),
        ..IndexReport::default()
    };

    let seen: HashSet<&str> = scanned.iter().map(|f| f.rel_path.as_str()).collect();
    for path in known.keys().filter(|p| !seen.contains(p.as_str())) {
        store.delete_file(path)?;
        report.removed += 1;
    }

    let mut changed: Vec<ScannedFile> = Vec::new();
    for f in scanned {
        match lge_index::hash_file(&f.abs_path) {
            Ok((_, hash)) if known.get(&f.rel_path).is_some_and(|(_, h)| *h == hash) => report.unchanged += 1,
            Ok(_) => changed.push(f),
            Err(e) => report.failed.push((f.rel_path.clone(), e.to_string())),
        }
    }

    let mut parsed: Vec<ParsedFile> = Vec::new();
    for (path, result) in lge_index::parse_many(&changed) {
        match result {
            Ok(p) => {
                if known.contains_key(&path) {
                    store.delete_file(&path)?;
                }
                parsed.push(p);
            }
            Err(e) => report.failed.push((path, e.to_string())),
        }
    }
    store.insert_parsed_bulk(&parsed, &commit, now)?;
    report.indexed = parsed.len();

    if report.indexed + report.removed > 0 || opts.force_resolve {
        report.stats = Some(resolve_edges(store, &commit, now)?);
        // Derived state may lag: a failure here must not fail the index run.
        if let Err(e) = crate::report::refresh_graph_views(store, now) {
            report.warnings.push(format!("graph views not refreshed: {e}"));
        }
    }
    Ok(report)
}

/// Rebuild every derived node and edge from the staged sites.
pub fn resolve_edges(store: &dyn GraphStore, commit: &str, now: i64) -> Result<EdgeStats> {
    let input = store.load_resolve_input()?;
    let resolved = lge_resolve::resolve(&input);
    store.replace_edges(&resolved, commit, now)?;
    Ok(resolved.stats)
}
