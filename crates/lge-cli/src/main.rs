use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
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

mod discover;
mod init;

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
        /// Add the batch line to the rule: related read-only commands in one exec script.
        /// Not measured yet: compare with `lge ab --variant batch`.
        #[arg(long)]
        batch: bool,
        /// The terse-replies Codex skill: `on` writes it, `always` also puts a short section in
        /// AGENTS.md (with --write), `off` removes both where lge wrote them. Never on by default.
        #[arg(long, value_enum, num_args = 0..=1, default_missing_value = "on")]
        terse: Option<init::TerseMode>,
        /// With --terse: the skill in your own skills folder (~/.agents/skills) for every repository.
        #[arg(long, requires = "terse")]
        global: bool,
    },
    /// Measure packs and routing against a file of tasks with known files.
    Bench {
        tasks: PathBuf,
        #[arg(long, default_value_t = 6000)]
        budget: usize,
    },
    /// A/B test through the daemon: each task as one Codex session with lge's
    /// features off and one with them on, each in a fresh git worktree of the
    /// base commit, one after another, then tokens, success and a verdict.
    /// Spends real tokens.
    Ab(AbArgs),
}

#[derive(Args)]
struct AbArgs {
    /// The task file: `[[task]]` entries with name, prompt, check and an optional setup,
    /// and an optional top-level `note` that replaces the default prompt note ("" turns it off).
    tasks: PathBuf,
    /// Sessions per task and arm.
    #[arg(long, default_value_t = 1)]
    runs: usize,
    /// The address of the running daemon (`lge serve`).
    #[arg(long, env = "LGE_DAEMON", default_value = "http://127.0.0.1:8787")]
    daemon: String,
    /// Where the copies are made. Default: a new folder in the system's temporary folder.
    #[arg(long)]
    work_dir: Option<PathBuf>,
    /// The commit (or branch, tag, revision) the copies are made from, resolved once at the start.
    #[arg(long, default_value = "HEAD")]
    base: String,
    /// Codex sandbox for the sessions. "" keeps Codex's default, which is read-only.
    #[arg(long, default_value = "workspace-write")]
    sandbox: String,
    /// Pass `-s workspace-write` even when Codex has `--approve-for-me`. By default the sessions
    /// use `--approve-for-me` in its place, so approval requests do not stall them.
    #[arg(long)]
    no_approve_for_me: bool,
    /// Append no note to the prompts. By default each prompt ends with a note not to install
    /// dependencies or run long test suites (the check runs after the session).
    #[arg(long)]
    no_note: bool,
    /// Start even when ~/.codex/config.toml does not send Codex's requests to the daemon.
    #[arg(long)]
    no_codex_check: bool,
    /// Codex model for both arms. Default: `[session] model` in lge.toml, else Codex's own default.
    #[arg(long)]
    model: Option<String>,
    /// What the arms differ in: `features` (lge's features off against on), `terse` (AGENTS.md
    /// without against with the terse-replies section) or `batch` (the lge block without against
    /// with the batch line). For terse and batch both arms use the daemon's settings as you set them.
    #[arg(long, default_value = "features", value_parser = ["features", "terse", "batch"])]
    variant: String,
    /// Skip the preflight: by default each task's setup and check run on a fresh copy of the base
    /// commit before any session, and the run stops when a check passes there or cannot run.
    #[arg(long)]
    no_preflight: bool,
    /// Run only the preflight and print its table: no daemon, no session, no token spent.
    #[arg(long, conflicts_with = "no_preflight")]
    preflight_only: bool,
    /// Resume a stopped run: the work dir it printed. Sessions in its results.jsonl that finished
    /// are not run again; the report covers them all.
    #[arg(long, value_name = "WORK_DIR", conflicts_with = "work_dir")]
    resume: Option<PathBuf>,
    /// With --resume: accept earlier results of another base commit, task file, model or variant.
    #[arg(long, requires = "resume")]
    force: bool,
    /// Weights of billed-equivalent tokens in the report: uncached input, cached input, output.
    #[arg(long, value_name = "IN,CACHED,OUT", default_value = "1,0.1,8")]
    weights: String,
    /// Print the report as JSON.
    #[arg(long)]
    json: bool,
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
    /// Replay past Codex sessions (~/.codex, or CODEX_HOME) through lge's rules: what each rule
    /// would remove, what no rule recognises, and commands that had to be corrected. Reads only.
    Discover {
        /// Sessions of the last N days.
        #[arg(long, default_value_t = 30)]
        days: i64,
        #[arg(long)]
        json: bool,
        /// Store each correction seen at least twice as a convention fact in this repository's graph.
        #[arg(long)]
        retain: bool,
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
    if let Cmd::Init {
        write,
        register,
        batch,
        terse,
        global,
    } = &cli.cmd
    {
        let exe = std::env::current_exe().context("cannot find the lge binary")?;
        let exe = exe.canonicalize().unwrap_or(exe);
        let places = init::Places {
            repo: repo.clone(),
            home: std::env::home_dir(),
            exe: exe.to_string_lossy().to_string(),
        };
        let opts = init::InitOptions {
            write: *write,
            register: *register,
            batch: *batch,
            terse: *terse,
            global: *global,
        };
        return init::run(&places, &opts, &mut std::io::stdout().lock());
    }
    // Discover reads Codex's session files; only --retain opens the graph.
    if let Cmd::Usage {
        cmd: UsageCmd::Discover { days, json, retain },
    } = &cli.cmd
    {
        return usage_discover(&repo, &db, *days, *json, *retain);
    }
    // The A/B runner works in copies through the daemon; it never opens the repository's graph.
    if let Cmd::Ab(args) = &cli.cmd {
        return ab(&repo, args);
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
            "lge: point Codex at it: add  openai_base_url = \"http://127.0.0.1:{}/v1\"  to ~/.codex/config.toml",
            addr.port()
        );
        lge_proxy::server::run(daemon, server);
        return Ok(());
    }

    let store = open(&db)?;
    match cli.cmd {
        Cmd::Init { .. } | Cmd::Serve { .. } | Cmd::Ab(_) => unreachable!("handled above"),
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
            for d in lge_index::bulky_dirs(&root) {
                println!(
                    "warning: searches would read {}/ ({}, {} files): {}; add `{}` to {}",
                    d.path,
                    size(d.bytes),
                    d.files,
                    d.reason,
                    d.ignore_line(),
                    root.join(lge_index::ignore_file(&root, &d.path)).display()
                );
            }
            match lge_memory::ingest_repo_history(&store, &root, lge_retrieve::unix_now()) {
                Ok(0) => {}
                Ok(n) => println!("history: {n} new commit(s) remembered"),
                Err(e) => println!("warning: git history not read: {e}"),
            }
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
            let runner = CodexRunner::new(cfg.codex_bin.clone());
            if dry_run {
                let plan = plan_task(&store, &repo, &task, &cfg, &index)?;
                let (sandbox, approve_for_me) = cfg.sandbox_for(&runner);
                let request = SessionRequest {
                    prompt: String::new(),
                    cwd: repo.clone(),
                    model: (!cfg.model.is_empty()).then(|| cfg.model.clone()),
                    effort: cfg.effort_for(plan.level),
                    sandbox,
                    approve_for_me,
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
            cmd: UsageCmd::Discover { .. },
        } => unreachable!("handled above"),
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

/// `52.3 MB`.
fn size(bytes: u64) -> String {
    let mut value = bytes as f64;
    let mut unit = 0;
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// `lge ab`: check what can be checked before any token is spent, then run
/// both arms of every task and print the report.
fn ab(repo: &Path, args: &AbArgs) -> Result<()> {
    use lge_bench::ab::{self, DaemonApi};
    if args.runs == 0 {
        bail!("--runs must be at least 1");
    }
    let file = ab::load_task_file(&args.tasks)?;
    let tasks = file.tasks;
    let weights = ab::Weights::parse(&args.weights)?;
    let variant: ab::Variant = args.variant.parse()?;
    let session = lge_session::load_config(repo)?;
    // Before the daemon is asked anything: a wrong revision is a typo.
    let base = ab::resolve_commit(repo, &args.base)?;
    let work_dir = args
        .resume
        .clone()
        .or_else(|| args.work_dir.clone())
        .unwrap_or_else(ab::default_work_dir);
    let mut opts = ab::AbOptions::new(work_dir.clone());
    opts.runs = args.runs;
    opts.base = base.clone();
    opts.model = args
        .model
        .clone()
        .or_else(|| (!session.model.is_empty()).then(|| session.model.clone()));
    opts.sandbox = (!args.sandbox.is_empty()).then(|| args.sandbox.clone());
    opts.approve_for_me = !args.no_approve_for_me;
    opts.note = ab::resolve_note(file.note.as_deref(), args.no_note);
    opts.variant = variant;
    opts.weights = weights;
    opts.preflight = !args.no_preflight;
    opts.resume = args.resume.is_some();
    opts.force = args.force;
    opts.tasks_hash = file.hash.clone();
    let log = |m: &str| eprintln!("lge ab: {m}");
    if args.preflight_only {
        ab::interrupt::install();
        let checked = ab::run_preflight(repo, &tasks, &opts, &ab::interrupt::requested, &log)?;
        print!("{}", checked.render());
        if !checked.ok() {
            std::process::exit(1);
        }
        return Ok(());
    }
    let daemon = ab::HttpDaemon::new(&args.daemon)?;
    daemon.status()?;
    if !args.no_codex_check {
        let config = lge_usage::codex_home().join("config.toml");
        let text = std::fs::read_to_string(&config).unwrap_or_default();
        if !ab::codex_points_at(&text, daemon.base()) {
            bail!(
                "Codex does not send its requests to {base}, so both arms would be the same: add  \
                 openai_base_url = \"{base}/v1\"  at the top of {} (before any [section]); \
                 --no-codex-check skips this check",
                config.display(),
                base = daemon.base()
            );
        }
    }
    let runner = CodexRunner::new(session.codex_bin.clone());
    eprintln!(
        "lge ab: {} task(s) x {} run(s) x 2 arms = {} Codex sessions{}, one after another, each in its own \
         git worktree of commit {base} of {} under {}",
        tasks.len(),
        opts.runs,
        tasks.len() * opts.runs * 2,
        if opts.resume {
            " at most (finished ones are not run again)"
        } else {
            ""
        },
        repo.display(),
        opts.work_dir.display()
    );
    eprintln!("lge ab: variant {variant}");
    eprintln!(
        "lge ab: results go to {}; to resume after a stop: {}",
        work_dir.join(ab::RESULTS_FILE).display(),
        resume_command(repo, args, &base, &work_dir)
    );
    match &opts.note {
        Some(note) => eprintln!("lge ab: note appended to every prompt: {note}"),
        None => eprintln!("lge ab: no note appended to the prompts"),
    }
    eprintln!("lge ab: the daemon's settings are switched during the run and restored at the end (Ctrl-C too)");
    ab::interrupt::install();
    let env = ab::AbEnv {
        daemon: &daemon,
        runner: &runner,
        stop: &ab::interrupt::requested,
        log: &log,
    };
    let report = ab::run_ab(repo, &tasks, &opts, &env)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", ab::render(&report));
    }
    if report.interrupted {
        eprintln!(
            "lge ab: stopped; to run what is missing: {}",
            resume_command(repo, args, &base, &work_dir)
        );
        std::process::exit(130);
    }
    if !report.settings_restored {
        std::process::exit(1);
    }
    Ok(())
}

/// A word as the shell reads it: quoted when it holds anything but plain characters.
fn shell_word(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-./:=,@%+".contains(c));
    if plain {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

/// The command that resumes this run: the same task file, repository, base
/// commit and options, with `--resume <work dir>`.
fn resume_command(repo: &Path, args: &AbArgs, base: &str, work_dir: &Path) -> String {
    let tasks = args.tasks.canonicalize().unwrap_or_else(|_| args.tasks.clone());
    let mut words: Vec<String> = vec![
        "lge".into(),
        "ab".into(),
        shell_word(&tasks.to_string_lossy()),
        "--repo".into(),
        shell_word(&repo.to_string_lossy()),
        "--base".into(),
        base.to_string(),
        "--runs".into(),
        args.runs.to_string(),
    ];
    let mut option = |name: &str, value: &str| {
        words.push(name.to_string());
        words.push(shell_word(value));
    };
    if let Some(model) = &args.model {
        option("--model", model);
    }
    if args.variant != "features" {
        option("--variant", &args.variant);
    }
    if args.sandbox != "workspace-write" {
        option("--sandbox", &args.sandbox);
    }
    if args.daemon != "http://127.0.0.1:8787" {
        option("--daemon", &args.daemon);
    }
    if args.weights != "1,0.1,8" {
        option("--weights", &args.weights);
    }
    for (set, flag) in [
        (args.no_approve_for_me, "--no-approve-for-me"),
        (args.no_note, "--no-note"),
        (args.no_codex_check, "--no-codex-check"),
        (args.no_preflight, "--no-preflight"),
        (args.json, "--json"),
    ] {
        if set {
            words.push(flag.to_string());
        }
    }
    words.push("--resume".into());
    words.push(shell_word(&work_dir.to_string_lossy()));
    words.join(" ")
}

/// `lge usage discover`: the replay of past Codex sessions, and with
/// `--retain` the corrected commands seen at least twice in sessions that
/// worked inside `repo`, as facts in its graph.
fn usage_discover(repo: &Path, db: &Path, days: i64, json: bool, retain: bool) -> Result<()> {
    if days <= 0 {
        bail!("--days must be at least 1");
    }
    let home = lge_usage::codex_home();
    // Facts go into this repository's graph: only its own sessions' corrections.
    let report = lge_proxy::discover::replay_for(&home, days, retain.then_some(repo))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", discover::render(&report));
    }
    if report.files == 0 {
        eprintln!(
            "no Codex session files of the last {days} day(s) under {}",
            home.display()
        );
    }
    if retain {
        let store = open(db)?;
        let stored = discover::retain_corrections(&store, &report, unix_now())?;
        if stored.is_empty() {
            eprintln!(
                "nothing retained: no correction was seen {} times or more",
                discover::MIN_SEEN
            );
        }
        for (key, outcome) in stored {
            eprintln!("retained {key}: {outcome:?}");
        }
    } else if !discover::corrections(&report).is_empty() {
        eprintln!("--retain stores the corrections seen at least twice as convention facts in this repository's graph");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_command_line_is_well_formed() {
        Cli::command().debug_assert();
    }

    fn ab_args(args: &[&str]) -> AbArgs {
        let mut all = vec!["lge", "ab"];
        all.extend_from_slice(args);
        match Cli::try_parse_from(all).unwrap().cmd {
            Cmd::Ab(a) => a,
            _ => unreachable!(),
        }
    }

    #[test]
    fn the_resume_command_repeats_the_options_that_matter() {
        let args = ab_args(&[
            "tasks.toml",
            "--runs",
            "3",
            "--model",
            "gpt-x",
            "--variant",
            "terse",
            "--no-note",
            "--weights",
            "1,0.2,6",
            "--work-dir",
            "/tmp/old",
        ]);
        let cmd = resume_command(
            Path::new("/work/my repo"),
            &args,
            "4f9c2a1e",
            Path::new("/tmp/lge-ab-1"),
        );
        assert_eq!(
            cmd,
            "lge ab tasks.toml --repo '/work/my repo' --base 4f9c2a1e --runs 3 --model gpt-x --variant terse \
             --weights 1,0.2,6 --no-note --resume /tmp/lge-ab-1"
        );
        let plain = ab_args(&["t.toml"]);
        assert_eq!(
            resume_command(Path::new("/r"), &plain, "abc", Path::new("/w")),
            "lge ab t.toml --repo /r --base abc --runs 1 --resume /w"
        );
        assert_eq!(shell_word("it's"), "'it'\\''s'");
    }

    #[test]
    fn ab_flags_parse_and_conflict_where_they_should() {
        let a = ab_args(&["t.toml", "--resume", "/w", "--force", "--no-preflight"]);
        assert_eq!(a.resume.as_deref(), Some(Path::new("/w")));
        assert!(a.force && a.no_preflight && !a.preflight_only);
        assert_eq!((a.variant.as_str(), a.weights.as_str()), ("features", "1,0.1,8"));
        for bad in [
            vec!["lge", "ab", "t.toml", "--force"],
            vec!["lge", "ab", "t.toml", "--resume", "/w", "--work-dir", "/x"],
            vec!["lge", "ab", "t.toml", "--preflight-only", "--no-preflight"],
            vec!["lge", "ab", "t.toml", "--variant", "other"],
            vec!["lge", "init", "--global"],
        ] {
            assert!(Cli::try_parse_from(&bad).is_err(), "{bad:?}");
        }
        for (args, terse) in [
            (vec!["lge", "init", "--terse"], Some(init::TerseMode::On)),
            (vec!["lge", "init", "--terse", "--write"], Some(init::TerseMode::On)),
            (vec!["lge", "init", "--terse=always"], Some(init::TerseMode::Always)),
            (vec!["lge", "init", "--terse", "off"], Some(init::TerseMode::Off)),
            (vec!["lge", "init"], None),
        ] {
            match Cli::try_parse_from(&args).unwrap().cmd {
                Cmd::Init { terse: t, .. } => assert_eq!(t, terse, "{args:?}"),
                _ => unreachable!(),
            }
        }
        match Cli::try_parse_from(["lge", "usage", "discover"]).unwrap().cmd {
            Cmd::Usage {
                cmd: UsageCmd::Discover { days, json, retain },
            } => assert_eq!((days, json, retain), (30, false, false)),
            _ => unreachable!(),
        }
    }
}
