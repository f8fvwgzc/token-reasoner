# Local Graph Intelligence Engine (lge)

A local code graph and project memory for coding agents, written in Rust on LadybugDB.
It indexes a repository, resolves how the code connects, and gives an agent a small,
ranked, provenance-marked context instead of letting it explore the repository with
grep and whole-file reads.

The goal is fewer tokens for the same work at the same model and reasoning level. No
saving is measured yet; `lge usage report` exists to measure it on your own sessions.

## Build

    cargo build --release        # binary: target/release/lge
    cargo test --workspace

The `lbug` crate tries to download a prebuilt LadybugDB library. If that fails it
compiles LadybugDB from C++ source (slow, needs cmake). To use a library you downloaded
yourself, create `.cargo/config.toml` (ignored by git):

    [env]
    LBUG_LIBRARY_DIR = "/path/to/liblbug"
    LBUG_INCLUDE_DIR = "/path/to/liblbug"

## Use as a proxy for Codex (recommended)

    docker compose up -d --build   # or: lge serve

Then add one line to `~/.codex/config.toml` (CLI and desktop app; your login stays):

    openai_base_url = "http://127.0.0.1:8787/v1"

Open http://127.0.0.1:8787 to add folders and see input, cached, output and reasoning
tokens per session. Every Codex session then goes through lge, which shortens test and
search output, points repeated output to its first copy, and adds graph context after a
message about the code. Details and limits: `docs/PROXY.md`.

## Use with Codex through MCP only

    lge setup                      # once: install the LadybugDB full-text extension
    lge index                      # index the repository you are in
    lge init                       # prints the two things below
    lge init --register --write    # or does them: registers the MCP server, writes the AGENTS.md rule

`lge init` registers one MCP server (`codex mcp add local_graph -- <path>/lge mcp`) that
serves whichever repository Codex is started in, for both the Codex CLI and the desktop
app, and adds a short rule to `AGENTS.md` telling the agent to ask the graph before it
searches or reads files. The server re-indexes a changed working tree before it answers.

Tools the agent gets:

| Tool | Answers |
|---|---|
| `search_code_context` | Ranked context for a task: starting symbols with source, what they use, what uses them, tests |
| `analyze_change_impact` | What a change to a symbol or file affects: code, tests, APIs, services |
| `trace_execution_flow` | The call path from or to a symbol or endpoint, across services and queues |
| `find_dependencies` | Direct edges of a symbol, file, service, endpoint, table or queue |
| `get_relevant_source` | Exact source of symbols or a line range; never sent twice in one session |
| `get_architecture_context` | Services and what connects them |
| `recall_memory` | Decisions, conventions and facts from earlier sessions |
| `retain_memory` | Store one fact for later sessions |

Every answer is compact text under a token budget, marks edges that are not verified, and
ends with the indexed commit.

## Commands

    lge pack "<task>"              # the evidence pack a session should start from
    lge impact <symbol|file>       # change impact
    lge trace <symbol|endpoint>    # execution flow (--upstream for the reverse)
    lge deps <target>              # direct edges with provenance
    lge arch [service]             # services, endpoints, queues, tables
    lge map                        # most depended-on symbols as signatures
    lge overview                   # orientation page: key symbols, communities, weak spots
    lge route "<task>"             # the reasoning level the router recommends (JSON)
    lge run "<task>" [--dry-run]   # one Codex session started from the pack
    lge check                      # lint, types, tests: failures and relevant diff hunks only
    lge usage report               # tokens per task by class, cached share, by mode
    lge memory retain|recall|pages|page
    lge bench tasks.toml           # pack recall and router agreement on known tasks
    lge mcp | watch | stats | query | search | setup

The database defaults to `<repo>/.lge/graph.lbug`. Pages are mirrored to `.lge/pages/`.
Only one process can hold the database: while the MCP server runs, use its tools rather
than the CLI.

`lge run` keeps your model and reasoning level. The router's recommendation is logged
next to each session, so `lge usage report` shows what routing would have changed;
`--routed` applies it.

## Configuration (`lge.toml` in the repository root, all optional)

    [session]
    level_mode = "fixed"        # or "routed"
    pack_budget = 6000
    memory_budget = 600
    max_attempts = 2

    [[check]]
    name = "tests"
    kind = "tests"              # lint | types | tests
    command = "pytest -q"

    [prices]                    # per million tokens; enables cost columns in the usage report
    input_per_mtok = 0.0
    cached_input_per_mtok = 0.0
    output_per_mtok = 0.0

## Crates

| Crate | Role |
|---|---|
| lge-core | Shared types: records, edges, provenance vocabulary, ids |
| lge-index | Scanner, git change detection, Tree-sitter extraction (Python, TypeScript, JavaScript) |
| lge-resolve | Pure resolver: imports, calls, API, table and queue edges with provenance |
| lge-store | `GraphStore` trait and the LadybugDB adapter; the only crate that imports `lbug` |
| lge-retrieve | Index pipeline, seeds, traversal, ranking, evidence packs, graph views |
| lge-memory | Long-term memory: retain, recall, pages |
| lge-router | Reasoning router on change impact |
| lge-session | Prompt builder and session runner |
| lge-filter | Local checks reduced to failures and relevant diff hunks |
| lge-usage | Codex token usage parsing and reports |
| lge-proxy | The daemon: proxy in front of the provider, usage page, folder API, MCP over HTTP |
| lge-mcp | MCP stdio server |
| lge-bench | Benchmark runner |
| lge-cli | The `lge` binary |

## Edge provenance

| method | confidence | meaning |
|---|---|---|
| static | 0.90 to 0.95 | resolved through the same file, `self`/`this`, an import, or a literal name |
| inferred | 0.40 to 0.60 | a single match by name only; an HTTP call matched to a route is always inferred |
| unresolved | 0 | several candidates; stored, never guessed |

Calls to names that do not exist in the repository are counted as external and not stored.

## Documents

- `docs/PLAN.md`: the development plan.
- `docs/ARCHITECTURE.md`: what each crate promises the others.
- `docs/CONTEXT_GRAPH.md`: how tokens are saved with the model and reasoning level fixed.
- `docs/PROXY.md`: the proxy: what it changes, what it never touches, how to run it.
- `docs/PRIOR_ART.md`: what was learned from graphify, hindsight, graphiti, omnigraph, headroom and rtk.
