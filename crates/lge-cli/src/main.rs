use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use lge_router::{route, RouterConfig};
use lge_store::Store;
use std::path::PathBuf;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[derive(Parser)]
#[command(name = "lge", version, about = "Local Graph Intelligence Engine")]
struct Cli {
    /// Path of the LadybugDB database file.
    #[arg(long, global = true, default_value = ".lge/graph.lbug")]
    db: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Index a directory. Only new, changed and deleted files are processed.
    Index {
        root: PathBuf,
        /// 0 = the root is one service, 1 = each top-level directory is a service.
        #[arg(long, default_value_t = 1)]
        service_depth: usize,
    },
    /// Find symbols for a task description or name.
    Search {
        text: String,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Callers and tests affected by changing a symbol (name, qualified name or id).
    Impact {
        symbol: String,
        #[arg(long, default_value_t = 3)]
        depth: u8,
    },
    /// Choose the reasoning level for a task. Prints JSON.
    Route { task: String },
    /// Counts of files, symbols and edges by provenance.
    Stats,
    /// Run a raw Cypher query.
    Query { cypher: String },
    /// Install the LadybugDB full-text (BM25) extension. Needs network once.
    Setup,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut store = Store::open(&cli.db)?;
    match cli.cmd {
        Cmd::Index { root, service_depth } => {
            let started = Instant::now();
            let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
            let scanned = lge_index::scan(&root, service_depth)?;
            let known = store.file_hashes()?;
            let (mut changed, mut unchanged, mut removed) = (0, 0, 0);
            let seen: std::collections::HashSet<&str> = scanned.iter().map(|f| f.rel_path.as_str()).collect();
            for path in known.keys().filter(|p| !seen.contains(p.as_str())) {
                store.delete_file(path)?;
                removed += 1;
            }
            for f in &scanned {
                let (bytes, hash) = lge_index::hash_file(&f.abs_path)?;
                match known.get(&f.rel_path) {
                    Some((_, h)) if *h == hash => {
                        unchanged += 1;
                        continue;
                    }
                    Some(_) => store.delete_file(&f.rel_path)?,
                    None => {}
                }
                let parsed = lge_index::parse_python(&f.rel_path, &f.service, &bytes, &hash)?;
                store.insert_parsed(&parsed, now)?;
                changed += 1;
            }
            println!("files: {changed} indexed, {unchanged} unchanged, {removed} removed");
            if changed + removed > 0 {
                let s = store.rebuild_edges()?;
                println!(
                    "calls: {} static, {} inferred, {} unresolved, {} external (not in repo)",
                    s.calls_static, s.calls_inferred, s.calls_unresolved, s.calls_external
                );
                println!("imports: {} resolved, {} external", s.imports_resolved, s.imports_external);
            }
            println!("search: {}", if store.fts { "BM25 (fts extension)" } else { "substring fallback (run `lge setup` for BM25)" });
            println!("done in {:.2}s", started.elapsed().as_secs_f64());
        }
        Cmd::Search { text, limit } => {
            for h in store.search(&text, limit)? {
                println!("{:6.2}  {:8}  {}  ({}:{}-{})", h.score, h.kind, h.qualified_name, h.file_path, h.start_line, h.end_line);
            }
        }
        Cmd::Impact { symbol, depth } => {
            let targets = store.find(&symbol)?;
            if targets.is_empty() {
                bail!("no symbol named `{symbol}`");
            }
            for t in targets {
                println!("{} {}  ({}:{})", t.kind, t.qualified_name, t.file_path, t.start_line);
                for c in store.callers(&t.id, depth)? {
                    let tag = if c.is_test { "test" } else { "code" };
                    println!("  hop {}  {tag}  {}  ({}:{})", c.score as i64, c.qualified_name, c.file_path, c.start_line);
                }
            }
        }
        Cmd::Route { task } => {
            let r = route(&store, &task, &RouterConfig::default())?;
            println!("{}", serde_json::to_string_pretty(&r)?);
        }
        Cmd::Stats => {
            for (k, v) in store.stats()? {
                println!("{k:18} {v}");
            }
        }
        Cmd::Query { cypher } => print!("{}", store.raw(&cypher)?),
        Cmd::Setup => {
            store.install_fts()?;
            println!("fts extension installed and index built");
        }
    }
    Ok(())
}
