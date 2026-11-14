use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use lge_core::{method, EdgeType};
use lge_retrieve::{index_repo, IndexOptions};
use lge_router::{route, RouterConfig};
use lge_store::{GraphStore, Store};
use std::path::PathBuf;

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
    let store = Store::open(&cli.db)?;
    match cli.cmd {
        Cmd::Index { root, service_depth } => {
            let r = index_repo(
                &store,
                &root,
                &IndexOptions {
                    service_depth,
                    ..IndexOptions::default()
                },
            )?;
            println!(
                "files: {} indexed, {} unchanged, {} removed",
                r.indexed, r.unchanged, r.removed
            );
            for (path, why) in &r.failed {
                println!("skipped {path}: {why}");
            }
            if let Some(s) = &r.stats {
                println!(
                    "calls: {} static, {} inferred, {} unresolved, {} external (not in repo)",
                    s.get(EdgeType::Calls, method::STATIC),
                    s.get(EdgeType::Calls, method::INFERRED),
                    s.get(EdgeType::Calls, method::UNRESOLVED),
                    s.calls_external
                );
                println!(
                    "imports: {} resolved, {} external",
                    s.total(EdgeType::Imports),
                    s.imports_external
                );
                let other: Vec<String> = s
                    .counts
                    .iter()
                    .filter(|(k, _)| !k.starts_with("CALLS/") && !k.starts_with("IMPORTS/"))
                    .map(|(k, v)| format!("{k} {v}"))
                    .collect();
                if !other.is_empty() {
                    println!("edges: {}", other.join(", "));
                }
            }
            println!(
                "search: {}",
                if store.fts_enabled() {
                    "BM25 (fts extension)"
                } else {
                    "substring fallback (run `lge setup` for BM25)"
                }
            );
            println!("done in {:.2}s", r.seconds);
        }
        Cmd::Search { text, limit } => {
            for h in store.search(&text, limit)? {
                println!(
                    "{:6.2}  {:8}  {}  ({}:{}-{})",
                    h.score, h.kind, h.qualified_name, h.file_path, h.start_line, h.end_line
                );
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
                    println!(
                        "  hop {}  {tag}  {}  ({}:{})",
                        c.score as i64, c.qualified_name, c.file_path, c.start_line
                    );
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
