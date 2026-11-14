use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use lge_core::{method, EdgeType};
use lge_retrieve::{
    architecture, compute_metrics, dependencies, evidence_pack, impact, index_repo, overview, refresh_if_stale,
    render_deps, render_impact, render_trace, repo_map, resolve_targets, trace, unix_now, Freshness, IndexOptions,
    PackOptions, WalkOptions, OVERVIEW_SLUG,
};
use lge_router::{route, RouterConfig};
use lge_session::{codex_args, plan_task, run_task, CodexRunner, LevelMode, SessionRequest};
use lge_store::{GraphStore, Store};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "lge",
    version,
    about = "Local Graph Intelligence Engine: a code graph and project memory for coding agents"
)]
struct Cli {
    /// Repository root. Default: the git repository that holds the current directory, else the current directory.
    #[arg(long, global = true)]
    repo: Option<PathBuf>,
    /// Database file. Default: <repo>/.lge/graph.lbug
    #[arg(long, global = true)]
    db: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Index a directory. Only new, changed and deleted files are processed.
    Index {
        /// Directory to index. Default: the repository root.
        root: Option<PathBuf>,
        /// 0 = the root is one service, 1 = each top-level directory is a service.
        #[arg(long, default_value_t = 1)]
        service_depth: usize,
        /// Resolve edges again even when no file changed.
        #[arg(long)]
        force: bool,
    },
    /// Find symbols for a task description or name.
    Search {
        text: String,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// The evidence pack for a task: what a session should start from.
    Pack {
        task: String,
        #[arg(long, default_value_t = 6000)]
        budget: usize,
        /// Restrict the starting symbols to one service.
        #[arg(long)]
        service: Option<String>,
        /// Print the pack with its numbers as JSON.
        #[arg(long)]
        json: bool,
    },
    /// What is affected by changing a symbol or file.
    Impact {
        target: String,
        #[arg(long, default_value_t = 3)]
        depth: u8,
        #[arg(long, default_value_t = 4000)]
        budget: usize,
    },
    /// The execution flow from a symbol or endpoint (or to it, with --upstream).
    Trace {
        symbol: String,
        #[arg(long)]
        upstream: bool,
        #[arg(long, default_value_t = 4)]
        depth: u8,
        #[arg(long, default_value_t = 4000)]
        budget: usize,
    },
    /// Direct upstream and downstream edges of a symbol, file, service, endpoint, table or queue.
    Deps {
        target: String,
        #[arg(long, default_value_t = 4000)]
        budget: usize,
    },
    /// The reasoning level the router recommends for a task. Prints JSON.
    Route { task: String },
    /// Services and what connects them.
    Arch { service: Option<String> },
    /// The most depended-on symbols as signatures, by file.
    Map {
        #[arg(long)]
        service: Option<String>,
        #[arg(long, default_value_t = 1500)]
        budget: usize,
    },
    /// The orientation page: size, key symbols, communities, where the graph is unsure.
    Overview,
    /// Counts of files, symbols and edges by provenance.
    Stats,
    /// Run a raw Cypher query.
    Query { cypher: String },
    /// Install the LadybugDB full-text (BM25) extension. Needs network once.
    Setup,
    /// Run the daemon: the proxy between the agent and the model provider, the
    /// page that shows usage and savings, and the graph tools over HTTP.
    Serve {
        #[arg(long, env = "LGE_HOST", default_value = "127.0.0.1")]
        host: String,
        #[arg(long, env = "LGE_PORT", default_value_t = 8787)]
        port: u16,
        /// Where graphs, the usage log and settings are kept. Default: ~/.lge
        #[arg(long, env = "LGE_HOME")]
        data_dir: Option<PathBuf>,
        /// A folder the daemon may index (repeatable, or separated by `:` in LGE_WORKSPACE). Default: any folder.
        #[arg(long = "workspace", env = "LGE_WORKSPACE", value_delimiter = ':')]
        workspaces: Vec<PathBuf>,
        /// The agent's home, for importing the usage of past sessions. Default: ~/.codex if it exists.
        #[arg(long, env = "LGE_CODEX_HOME")]
        codex_home: Option<PathBuf>,
        /// Provider base URL for API-key requests.
        #[arg(long, env = "LGE_UPSTREAM_API")]
        upstream_api: Option<String>,
        /// Provider base URL for requests made with a ChatGPT login.
        #[arg(long, env = "LGE_UPSTREAM_CHATGPT")]
        upstream_chatgpt: Option<String>,
    },
    /// Serve the graph and the memory to a coding agent over MCP (stdio).
    Mcp {
        #[arg(long, default_value_t = 1)]
        service_depth: usize,
    },
    /// Keep the index up to date while files change.
    Watch {
        /// Seconds between checks.
        #[arg(long, default_value_t = 2)]
        interval: u64,
        #[arg(long, default_value_t = 1)]
        service_depth: usize,
    },
    /// Run one task as one Codex session started from its evidence pack.
    Run {
        task: String,
        /// Print the plan, the command and the prompt; start nothing.
        #[arg(long)]
        dry_run: bool,
        /// Apply the router's reasoning level instead of keeping your own.
        #[arg(long)]
        routed: bool,
        /// Do not run the local checks after the session.
        #[arg(long)]
        no_checks: bool,
    },
    /// Run lint, type checks and tests; print only failures and the relevant diff hunks.
    Check,
    /// The usage log.
    Usage {
        #[command(subcommand)]
        cmd: UsageCmd,
    },
    /// Long-term memory: facts and pages.
    Memory {
        #[command(subcommand)]
        cmd: MemoryCmd,
    },
    /// Connect this repository to Codex: the MCP registration and the AGENTS.md rule.
    Init {
        /// Write the rule into AGENTS.md (replacing an earlier lge block).
        #[arg(long)]
        write: bool,
        /// Register the MCP server by running `codex mcp add`.
        #[arg(long)]
        register: bool,
    },
    /// Measure packs and routing against a file of tasks with known files.
    Bench {
        tasks: PathBuf,
        #[arg(long, default_value_t = 6000)]
        budget: usize,
    },
}

#[derive(Subcommand)]
enum UsageCmd {
    /// Tokens per task by class, cached share, escalations, and cost when prices are configured.
    Report {
        /// Only sessions started in the last N days.
        #[arg(long)]
        days: Option<i64>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum MemoryCmd {
    /// Store one fact.
    Retain {
        text: String,
        /// world, experience, decision, convention or observation.
        #[arg(long, default_value = "world")]
        kind: String,
        /// What it is about; a newer fact with the same key replaces it.
        #[arg(long, default_value = "")]
        key: String,
        #[arg(long = "tag")]
        tags: Vec<String>,
    },
    /// Facts that match a query, packed to a token budget.
    Recall {
        query: String,
        #[arg(long, default_value_t = 800)]
        budget: usize,
    },
    /// Rebuild the memory pages and mirror all pages to .lge/pages.
    Pages,
    /// Print one page.
    Page { slug: String },
}

/// The rule Codex follows in this repository. Kept short: it is read in every session.
const AGENTS_BLOCK: &str = "<!-- lge:start -->
## Code graph (lge)

This repository is indexed by a local code graph, served by the `local_graph` MCP tools.

- Before the first grep, directory listing or file read for a code question, call `search_code_context` with the task or question. Put this rule in every sub-agent prompt that explores code.
- Before editing a symbol or file, call `analyze_change_impact`: it lists the affected code and the tests to run.
- Use `trace_execution_flow`, `find_dependencies` and `get_architecture_context` for how code connects, and `get_relevant_source` for exact lines.
- Read raw files only for line ranges these tools located.
- Edges marked inferred or unresolved are not verified: check them in the source.
- `recall_memory` returns decisions and conventions from earlier sessions. At the end of a task, store one with `retain_memory`.
<!-- lge:end -->
";

/// The git repository that holds `start`, else `start` itself.
fn discover_repo(start: &Path) -> PathBuf {
    let mut dir = start;
    loop {
        if dir.join(".git").exists() {
            return dir.to_path_buf();
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => return start.to_path_buf(),
        }
    }
}

fn open(db: &Path) -> Result<Store> {
    if let Some(dir) = db.parent() {
        std::fs::create_dir_all(dir)?;
        // The database is a local build product; the pages beside it are not.
        let ignore = dir.join(".gitignore");
        if dir.file_name().is_some_and(|n| n == ".lge") && !ignore.exists() {
            let _ = std::fs::write(&ignore, "graph.lbug*\n");
        }
    }
    Store::open(db).with_context(|| {
        format!(
            "cannot open {} (is another lge process, such as the MCP server, using it?)",
            db.display()
        )
    })
}

fn print_index(r: &lge_retrieve::IndexReport, store: &Store) {
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
    for w in &r.warnings {
        println!("warning: {w}");
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

fn commit_of(store: &Store) -> String {
    store.repository().ok().flatten().map(|(_, c)| c).unwrap_or_default()
}

fn ids_of(store: &Store, text: &str) -> Result<Vec<String>> {
    let nodes = resolve_targets(store, text)?;
    if nodes.is_empty() {
        bail!("nothing in the graph is called `{text}`");
    }
    Ok(nodes.into_iter().map(|n| n.id).collect())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cwd = std::env::current_dir()?;
    let repo = match &cli.repo {
        Some(r) => r
            .canonicalize()
            .with_context(|| format!("cannot open {}", r.display()))?,
        None => discover_repo(&cwd),
    };
    let db = cli.db.clone().unwrap_or_else(|| repo.join(".lge/graph.lbug"));
    let pages_dir = db
        .parent()
        .map(|d| d.join("pages"))
        .unwrap_or_else(|| repo.join(".lge/pages"));

    // `init` only prints or edits text files: it must work while the MCP
    // server holds the database.
    if let Cmd::Init { write, register } = &cli.cmd {
        return init(&repo, *write, *register);
    }
    // The daemon owns its own databases, one per folder, under its data directory.
    if let Cmd::Serve {
        host,
        port,
        data_dir,
        workspaces,
        codex_home,
        upstream_api,
        upstream_chatgpt,
    } = &cli.cmd
    {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| cwd.clone());
        let mut cfg = lge_proxy::server::Config::new(data_dir.clone().unwrap_or_else(|| home.join(".lge")));
        cfg.host = host.clone();
        cfg.port = *port;
        cfg.workspaces = workspaces
            .iter()
            .filter(|w| !w.as_os_str().is_empty())
            .map(|w| w.canonicalize().unwrap_or_else(|_| w.clone()))
            .collect();
        cfg.codex_home = codex_home
            .clone()
            .or_else(|| Some(home.join(".codex")).filter(|p| p.is_dir()));
        if let Some(u) = upstream_api {
            cfg.upstream_api = u.clone();
        }
        if let Some(u) = upstream_chatgpt {
            cfg.upstream_chatgpt = u.clone();
        }
        let daemon = lge_proxy::server::Daemon::open(cfg)?;
        let (server, addr) = lge_proxy::server::bind(&daemon)?;
        eprintln!(
            "lge: proxy and page on http://{addr}  (data in {})",
            daemon.cfg.data_dir.display()
        );
        eprintln!(
            "lge: point Codex at it with base_url = \"http://127.0.0.1:{}/v1\"; open the page for the exact lines",
            addr.port()
        );
        lge_proxy::server::run(daemon, server);
        return Ok(());
    }

    let store = open(&db)?;
    match cli.cmd {
        Cmd::Init { .. } | Cmd::Serve { .. } => unreachable!("handled above"),
        Cmd::Index {
            root,
            service_depth,
            force,
        } => {
            let root = root.unwrap_or_else(|| repo.clone());
            let r = index_repo(
                &store,
                &root,
                &IndexOptions {
                    service_depth,
                    force_resolve: force,
                },
            )?;
            print_index(&r, &store);
            if let Err(e) = lge_memory::export_pages(&store, &pages_dir) {
                println!("warning: pages not written to {}: {e}", pages_dir.display());
            }
        }
        Cmd::Search { text, limit } => {
            for h in store.search(&text, limit)? {
                println!(
                    "{:6.2}  {:8}  {}  ({}:{}-{})",
                    h.score, h.kind, h.qualified_name, h.file_path, h.start_line, h.end_line
                );
            }
        }
        Cmd::Pack {
            task,
            budget,
            service,
            json,
        } => {
            let pack = evidence_pack(
                &store,
                &task,
                &PackOptions {
                    budget,
                    service,
                    ..PackOptions::default()
                },
            )?;
            if json {
                println!("{}", serde_json::to_string_pretty(&pack)?);
            } else {
                print!("{}", pack.text);
                eprintln!(
                    "{} tokens of {} ({} symbols shown, {} left out)",
                    pack.tokens, pack.budget, pack.shown, pack.omitted
                );
            }
        }
        Cmd::Impact { target, depth, budget } => {
            let found = impact(
                &store,
                &ids_of(&store, &target)?,
                &WalkOptions {
                    depth,
                    ..WalkOptions::default()
                },
            )?;
            print!("{}", render_impact(&found, &commit_of(&store), budget));
        }
        Cmd::Trace {
            symbol,
            upstream,
            depth,
            budget,
        } => {
            for id in ids_of(&store, &symbol)?.into_iter().take(3) {
                if let Some(t) = trace(
                    &store,
                    &id,
                    !upstream,
                    &WalkOptions {
                        depth,
                        ..WalkOptions::default()
                    },
                )? {
                    print!("{}", render_trace(&t, &commit_of(&store), budget));
                }
            }
        }
        Cmd::Deps { target, budget } => {
            for id in ids_of(&store, &target)?.into_iter().take(3) {
                if let Some(d) = dependencies(&store, &id)? {
                    print!("{}", render_deps(&d, &commit_of(&store), budget));
                }
            }
        }
        Cmd::Route { task } => {
            let r = route(&store, &task, &RouterConfig::default())?;
            println!("{}", serde_json::to_string_pretty(&r)?);
        }
        Cmd::Arch { service } => print!("{}", architecture(&store, service.as_deref())?),
        Cmd::Map { service, budget } => print!("{}", repo_map(&store, service.as_deref(), budget)?),
        Cmd::Overview => match store.get_page(OVERVIEW_SLUG)? {
            Some(page) => print!("{}", page.body),
            None => print!("{}", overview(&store, &compute_metrics(&store)?.1)?),
        },
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
        Cmd::Mcp { service_depth } => {
            let mut server = lge_mcp::Server::new(
                Box::new(store),
                &repo,
                IndexOptions {
                    service_depth,
                    ..IndexOptions::default()
                },
            );
            server.pages_dir = Some(pages_dir);
            // stdout carries the protocol; everything else goes to stderr.
            eprintln!("lge mcp: serving {} from {}", repo.display(), db.display());
            server.serve(std::io::stdin().lock(), std::io::stdout().lock())?;
        }
        Cmd::Watch {
            interval,
            service_depth,
        } => {
            let opts = IndexOptions {
                service_depth,
                ..IndexOptions::default()
            };
            let mut fresh = Freshness::default();
            println!("watching {} (ctrl-c to stop)", repo.display());
            loop {
                match refresh_if_stale(&store, &repo, &opts, &mut fresh, Duration::ZERO) {
                    Ok(Some(r)) if r.indexed + r.removed > 0 => {
                        println!("{} indexed, {} removed in {:.2}s", r.indexed, r.removed, r.seconds)
                    }
                    Ok(_) => {}
                    Err(e) => println!("index failed: {e}"),
                }
                std::thread::sleep(Duration::from_secs(interval.max(1)));
            }
        }
        Cmd::Run {
            task,
            dry_run,
            routed,
            no_checks,
        } => {
            let mut cfg = lge_session::load_config(&repo)?;
            if routed {
                cfg.level_mode = LevelMode::Routed;
            }
            if no_checks {
                cfg.run_checks = false;
            }
            let index = IndexOptions::default();
            if dry_run {
                let plan = plan_task(&store, &repo, &task, &cfg, &index)?;
                let request = SessionRequest {
                    prompt: String::new(),
                    cwd: repo.clone(),
                    model: (!cfg.model.is_empty()).then(|| cfg.model.clone()),
                    effort: cfg.effort_for(plan.level),
                    sandbox: (!cfg.sandbox.is_empty()).then(|| cfg.sandbox.clone()),
                };
                println!(
                    "router: {} ({}){}",
                    plan.route.level,
                    plan.route.reason,
                    if cfg.level_mode == LevelMode::Fixed {
                        "; not applied, your level is kept"
                    } else {
                        ""
                    }
                );
                println!(
                    "prompt: {} tokens, of which {} are the fixed prefix and {} the pack",
                    plan.prompt_tokens, plan.prefix_tokens, plan.pack.tokens
                );
                println!(
                    "command: {} {}  (prompt on stdin)",
                    cfg.codex_bin,
                    codex_args(&request).join(" ")
                );
                println!("----- prompt -----\n{}", plan.prompt);
            } else {
                let runner = CodexRunner {
                    bin: cfg.codex_bin.clone(),
                };
                let out = run_task(&store, &repo, &task, &cfg, &index, &runner)?;
                for a in &out.attempts {
                    println!(
                        "attempt {}: {} | input {} (cached {}) reasoning {} output {} | level {} (router: {})",
                        a.attempt,
                        a.outcome,
                        a.input_tokens,
                        a.cached_input_tokens,
                        a.reasoning_tokens,
                        a.output_tokens,
                        a.level,
                        a.recommended_level
                    );
                }
                if !out.check_text.is_empty() {
                    println!("----- checks -----\n{}", out.check_text);
                }
                println!("----- result -----\n{}", out.final_message);
                let _ = lge_memory::export_pages(&store, &pages_dir);
            }
        }
        Cmd::Check => {
            drop(store);
            let specs = match lge_filter::load_config(&repo)? {
                Some(specs) => specs,
                None => lge_filter::detect_checks(&repo),
            };
            if specs.is_empty() {
                bail!(
                    "no checks configured in lge.toml and none detected in {}",
                    repo.display()
                );
            }
            let report = lge_filter::run_all(&repo, &specs)?;
            print!("{}", lge_filter::render(&report));
            if !report.passed {
                std::process::exit(1);
            }
        }
        Cmd::Usage {
            cmd: UsageCmd::Report { days, json },
        } => {
            let since = days.map(|d| unix_now() - d * 86_400).unwrap_or(0);
            let records = store.list_usage(since)?;
            let prices = lge_usage::load_prices(&repo)?;
            let report = lge_usage::report(&records, prices.as_ref());
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", lge_usage::render_report(&report));
            }
        }
        Cmd::Memory { cmd } => match cmd {
            MemoryCmd::Retain { text, kind, key, tags } => {
                let now = unix_now();
                let input = lge_memory::RetainInput {
                    text,
                    kind,
                    key,
                    tags,
                    source: "cli".to_string(),
                    valid_at: None,
                };
                let kept = lge_memory::retain(&store, &input, now)?;
                lge_memory::refresh_pages(&store, now)?;
                lge_memory::export_pages(&store, &pages_dir)?;
                println!(
                    "{:?}: {} (linked to {} code entities)",
                    kept.outcome,
                    kept.id,
                    kept.links.len()
                );
            }
            MemoryCmd::Recall { query, budget } => {
                let opts = lge_memory::RecallOptions {
                    budget,
                    now: unix_now(),
                    ..lge_memory::RecallOptions::default()
                };
                let found = lge_memory::recall(&store, &query, &opts, None)?;
                print!("{}", found.text);
                for hit in lge_memory::search_pages(&store, &query, 3)? {
                    println!("page {}: {}", hit.slug, hit.snippet.replace('\n', " "));
                }
                eprintln!(
                    "{} facts, {} tokens, {} left out",
                    found.items.len(),
                    found.tokens,
                    found.omitted
                );
            }
            MemoryCmd::Pages => {
                let rebuilt = lge_memory::refresh_pages(&store, unix_now())?;
                let written = lge_memory::export_pages(&store, &pages_dir)?;
                println!(
                    "{} page(s) rebuilt, {} file(s) written to {}",
                    rebuilt.len(),
                    written,
                    pages_dir.display()
                );
            }
            MemoryCmd::Page { slug } => match store.get_page(&slug)? {
                Some(page) => print!("{}", page.body),
                None => bail!("no page `{slug}`; `lge memory recall <words>` searches pages and facts"),
            },
        },
        Cmd::Bench { tasks, budget } => {
            let list = lge_bench::load_tasks(&tasks)?;
            index_repo(&store, &repo, &IndexOptions::default())?;
            print!("{}", lge_bench::render(&lge_bench::run(&store, &repo, &list, budget)?));
        }
    }
    Ok(())
}

/// Print, and on request apply, what connects this repository to Codex.
fn init(repo: &Path, write: bool, register: bool) -> Result<()> {
    let exe = std::env::current_exe().context("cannot find the lge binary")?;
    let exe = exe.canonicalize().unwrap_or(exe);
    println!("A. The proxy: every Codex session goes through lge, nothing else to remember.\n");
    println!("   1. Start the daemon:  lge serve      (or, in Docker:  docker compose up -d)");
    println!("   2. Add one line to ~/.codex/config.toml (the CLI and the desktop app read the same file;");
    println!("      your ChatGPT or API-key login stays as it is):\n");
    println!("        openai_base_url = \"http://127.0.0.1:8787/v1\"\n");
    println!("   3. Open http://127.0.0.1:8787 to add folders and see tokens per session.");
    println!("      Graph tools for the agent:  codex mcp add local_graph --url http://127.0.0.1:8787/mcp\n");

    let command = ["mcp", "add", "local_graph", "--", &exe.to_string_lossy(), "mcp"].map(str::to_string);
    println!("B. Without the daemon: the graph tools only, started by Codex in the repository it works in.\n");
    println!("   lge index");
    println!("   codex {}\n", command.join(" "));
    if register {
        let status = std::process::Command::new("codex")
            .args(&command)
            .status()
            .context("cannot run `codex`; is the Codex CLI on PATH?")?;
        if !status.success() {
            bail!("`codex mcp add` failed");
        }
        println!("   registered.\n");
    }

    let agents = repo.join("AGENTS.md");
    println!(
        "C. The rule that makes the agent ask the graph first belongs in {}:\n",
        agents.display()
    );
    println!("{AGENTS_BLOCK}");
    if write {
        let old = std::fs::read_to_string(&agents).unwrap_or_default();
        let new = match (old.find("<!-- lge:start -->"), old.find("<!-- lge:end -->")) {
            (Some(start), Some(end)) if end > start => {
                let after = &old[end + "<!-- lge:end -->".len()..];
                format!("{}{}{}", &old[..start], AGENTS_BLOCK.trim_end(), after)
            }
            _ if old.trim().is_empty() => AGENTS_BLOCK.to_string(),
            _ => format!("{}\n\n{AGENTS_BLOCK}", old.trim_end()),
        };
        if new != old {
            std::fs::write(&agents, new).with_context(|| format!("cannot write {}", agents.display()))?;
            println!("   written to {}.", agents.display());
        } else {
            println!("   {} already has this block.", agents.display());
        }
    }
    Ok(())
}
