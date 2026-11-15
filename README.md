# lge: Local Graph Intelligence Engine

lge sits between your coding agent and the model. It builds a graph of your code, and
uses it to send the model less text for the same work:

```
Codex (desktop app or CLI)  ──►  lge on 127.0.0.1:8787  ──►  OpenAI
                                  │
                                  ├─ code graph of your repositories (LadybugDB)
                                  ├─ long-term project memory
                                  └─ page with tokens per session
```

- The model and the reasoning level stay exactly what you chose.
- Your login stays as it is. Credentials pass through and are never stored.
- If anything goes wrong inside lge, the original request is sent unchanged.
- Everything runs on your machine. One Rust binary, one embedded database.

No saving is claimed here: the page shows the real token counts of your own sessions, so
you can see what lge does for you.

---

## Contents

1. [Install with one prompt (for AI agents)](#1-install-with-one-prompt-for-ai-agents)
2. [Install by hand on macOS](#2-install-by-hand-on-macos)
3. [Install with Docker](#3-install-with-docker)
4. [Connect your agent](#4-connect-your-agent)
5. [Use it](#5-use-it)
6. [How it works](#6-how-it-works)
7. [Configuration](#7-configuration)
8. [Uninstall](#8-uninstall)
9. [Troubleshooting](#9-troubleshooting)
10. [Limits](#10-limits)
11. [Project layout](#11-project-layout)

---

## 1. Install with one prompt (for AI agents)

Paste this into Codex, Qwen Code, Claude Code or any agent that can run shell commands on
your Mac:

```text
Install lge (Local Graph Intelligence Engine) on this Mac and connect Codex to it.
Follow the "Instructions for agents" section of
https://github.com/f8fvwgzc/local-graph-intelli-engine/blob/main/README.md exactly.
Do not improvise other steps. When done, show me the page URL and the last lines of
the install output.
```

### Instructions for agents

Run these steps in order. Stop and report if a step fails; do not work around it.

1. **Check the platform.** `uname -s` must print `Darwin`. On Linux, use
   [Install with Docker](#3-install-with-docker) instead.
2. **Check the Xcode Command Line Tools.** `xcode-select -p` must print a path. If it
   fails, tell the user to run `xcode-select --install` (it opens a dialog only they can
   confirm), wait for them, then continue.
3. **Get the code.**
   ```bash
   git clone https://github.com/f8fvwgzc/local-graph-intelli-engine.git ~/lge-src
   cd ~/lge-src
   ```
   If `~/lge-src` already exists, run `git -C ~/lge-src pull` instead.
4. **Run the installer.**
   ```bash
   scripts/install-macos.sh --connect-codex
   ```
   It installs Rust if missing, builds lge (a few minutes the first time), copies it to
   `~/.local/bin/lge`, starts the daemon on `127.0.0.1:8787`, adds one line to
   `~/.codex/config.toml` (backup in `~/.codex/config.toml.before-lge`), and registers the
   graph tools with Codex.
   - Add `--autostart` only if the user asked for lge to start at login.
   - If it reports that port 8787 is in use (another proxy such as Headroom), ask the user
     whether to stop that program or to use `--port 8788`.
   - If it reports that `openai_base_url` is already set to something else, ask the user
     before running again with `--force`.
5. **Verify.**
   ```bash
   curl -s http://127.0.0.1:8787/health
   ```
   must print `{"ok":true}`. `head -3 ~/.codex/config.toml` must show
   `openai_base_url = "http://127.0.0.1:8787/v1"`.
6. **Report** to the user: the page URL `http://127.0.0.1:8787`, that the change is undone
   with `~/lge-src/scripts/install-macos.sh --uninstall`, and that lge stops at logout
   unless `--autostart` was used.

The installer is safe to run again. It changes nothing outside `~/.local/bin/lge`,
`~/.lge`, `~/.codex/config.toml` (one line plus the `local_graph` tool entry) and, with
`--autostart`, `~/Library/LaunchAgents/dev.lge.daemon.plist`.

---

## 2. Install by hand on macOS

Requirements: macOS on Apple silicon or Intel, the Xcode Command Line Tools
(`xcode-select --install`), and Rust 1.88 or later (`curl https://sh.rustup.rs -sSf | sh`).

```bash
git clone https://github.com/f8fvwgzc/local-graph-intelli-engine.git ~/lge-src
cd ~/lge-src
scripts/install-macos.sh --connect-codex --autostart
```

Or every step yourself:

```bash
cargo build --release -p lge-cli         # downloads the prebuilt LadybugDB library
cp target/release/lge ~/.local/bin/lge   # any folder on your PATH
lge --db ~/.lge/setup/graph.lbug setup   # full-text search extension, once
lge serve                                # 127.0.0.1:8787, data in ~/.lge
```

The build downloads LadybugDB 0.22.3 from its GitHub releases (the version is pinned in
`.cargo/config.toml`). No cmake and no database server are needed.

---

## 3. Install with Docker

The same layout as a Headroom container: one container, the page and proxy on port 8787.

```bash
git clone https://github.com/f8fvwgzc/local-graph-intelli-engine.git ~/lge-src
cd ~/lge-src
export LGE_WORKSPACE=$HOME/code     # the folder that holds your repositories
docker compose up -d --build
```

- Your workspace is mounted read-only at the same path it has on the Mac, so the paths
  Codex reports are the paths lge reads.
- `~/.codex` is mounted read-only, for the "import past sessions" button.
- Graphs and the usage log live in the `lge-data` volume.
- The image needs about 3 GB of free space in Docker Desktop to build.

Then connect Codex as in the next section.

---

## 4. Connect your agent

### Codex (desktop app and CLI)

Add one line at the top of `~/.codex/config.toml`, before any `[section]`:

```toml
openai_base_url = "http://127.0.0.1:8787/v1"
```

The desktop app and the CLI both read this file, and both your ChatGPT login and an API
key keep working. Remove the line to stop. Optional, the graph tools for the agent:

```bash
codex mcp add local_graph --url http://127.0.0.1:8787/mcp
```

### Other agents (Qwen Code, Claude Code, Cursor, ...)

The proxy understands the OpenAI Responses API, which is what Codex speaks. Agents that
use other APIs get the graph through MCP or the command line instead:

| Agent | How |
|---|---|
| Any MCP client | Add the HTTP MCP server `http://127.0.0.1:8787/mcp` (or the stdio server `lge mcp`, started in the repository) |
| Any agent with a shell | `lge pack "<task>"` prints the evidence pack for a task; `lge impact <symbol>` the change impact |
| Another provider that speaks the Responses API | Start the daemon with `LGE_UPSTREAM_API=https://your-provider/v1` and point the agent's base URL at `http://127.0.0.1:8787/v1` |

Tools the agent gets over MCP:

| Tool | Answers |
|---|---|
| `search_code_context` | The starting symbols for a task with their source, what they use, what uses them, the tests |
| `analyze_change_impact` | What a change to a symbol or file affects: code, tests, APIs, services |
| `trace_execution_flow` | The call path from or to a symbol or endpoint, across services and queues |
| `find_dependencies` | The direct edges of a symbol, file, service, endpoint, table or queue |
| `get_relevant_source` | The exact source of symbols or a line range, never sent twice in one session |
| `get_architecture_context` | Services and what connects them |
| `recall_memory` | Decisions, conventions and facts from earlier sessions |
| `retain_memory` | Store one fact for later sessions |

---

## 5. Use it

1. Open **http://127.0.0.1:8787**.
2. Add the folders of your projects, or just start Codex in a git repository: lge indexes
   it the first time Codex works there.
3. Use Codex as usual.

The page shows, per session: input tokens, the share that was cached, output and
reasoning tokens, what lge removed from tool output, and what graph context it added.
"Import past Codex sessions" reads the token counts of your earlier sessions from
`~/.codex` (no conversation text) so you have a before and after.

Every model request also leaves one line in `~/.lge/serve.log`, without any content:

```
lge: POST /v1/responses 200 | zstd 23075B read -> 65089B | | usage in 13143 (cached 6144) out 5 (reasoning 0)
```

Commands, for scripts and other agents:

```bash
lge index                       # index the repository you are in
lge pack "create_order must validate the payload"   # evidence pack for a task
lge impact create_order         # what a change affects
lge trace "POST /api/orders"    # execution flow across services
lge arch                        # services, endpoints, queues, tables
lge memory retain "Tokens expire after 15 minutes" --kind decision --key auth.token-expiry
lge memory recall "token expiry"
lge usage report                # tokens per task
lge check                       # lint, types, tests: failures only
```

`lge --help` lists everything.

---

## 6. How it works

### The problem

A coding agent spends most of its tokens finding its way: listing folders, grepping,
reading whole files, rereading the same output, and reading thousands of lines of passing
test output. Each of these turns is sent again on every later turn of the conversation.

### What lge does with each request

lge reads the request Codex sends, changes only what it understands, and forwards it:

| In the conversation | What lge sends instead | When |
|---|---|---|
| Output of a test, lint or type-check run | The failures, or one line when it passed | Only for formats it recognises (pytest, jest, vitest, cargo, tsc, eslint, ruff, mypy, pyright, ...) |
| A search with more than 60 matching lines | The matches capped, the files the task is about first, and a line saying what was left out | `rg`, `grep`, `git grep` |
| The same large output a second time | One line pointing to the first copy | While the first copy is still in the request |
| A message you typed about the code | Your message, followed by the graph's context for it | When it names a symbol or clearly matches one |
| Anything else | Exactly the same bytes | Always |

Never touched: the system prompt, tool definitions, reasoning, tool calls, patches, and any
output lge cannot classify with certainty.

**Prompt caching.** The agent resends the whole conversation every turn, and the provider
charges much less for the unchanged start of it. lge decides how to rewrite each item
once, then sends the same bytes for it on every later turn, so the cached start stays
cached. A request in which nothing changed is forwarded byte for byte.

### The code graph

```
files ─► scan ─► parse (tree-sitter) ─► resolve ─► LadybugDB ─► rank ─► evidence pack
```

1. **Scan.** Python, TypeScript and JavaScript files, honouring `.gitignore`. Only new and
   changed files are parsed again (content hash), and a changed working tree is picked up
   before each answer.
2. **Parse.** Every function, method, class, interface and type with its signature, doc
   line, body and line range; every call, import, HTTP route, HTTP call, SQL table, ORM
   model, queue publish and consume, base class, and `NOTE:` / `WHY:` / `HACK:` comment.
3. **Resolve.** Calls and imports are matched to their targets. Every edge records how it
   was found:

   | method | confidence | meaning |
   |---|---|---|
   | static | 0.90 to 0.95 | the source says so: same file, `self`/`this`, an import, a literal name |
   | inferred | 0.40 to 0.60 | one match by name only; an HTTP call matched to a route is always inferred |
   | unresolved | 0 | several candidates; stored, never guessed |

   An HTTP client call is joined to the route that serves it, a publisher to the consumer
   of the same queue, a writer to the readers of the same table, so the graph crosses
   service boundaries.
4. **Store.** Everything goes into LadybugDB, an embedded graph database, with a BM25
   full-text index over names, split identifier words (`createOrderHandler` is found by
   "order handler"), signatures, doc lines and those comments.
5. **Rank.** PageRank over the call graph says which symbols the rest of the code depends
   on; Louvain groups symbols that work together.
6. **Evidence pack.** For a task: find the starting symbols (names written in the task,
   then search), walk what they use and what uses them, rank by personalized PageRank from
   the starting points, and fill a token budget: the starting symbols with their source,
   neighbours with their signature, the rest as one line each, the tests to run, the APIs,
   tables and queues involved, and a list of edges that are not verified. The starting
   symbols are never cut; a pack that leaves things out says so.

### Long-term memory

The agent stores facts with `retain_memory` (a decision, a convention, what a task found).
A repeated fact raises a counter instead of adding a copy; a newer fact with the same key
closes the older one, which is kept, not deleted. `recall_memory` searches by keyword, by
the code the facts mention, and by recency, merges the lists by rank, and packs the
result into a token budget. Facts tagged as decisions, conventions or feature work are
also written as markdown pages under `~/.lge/graphs/<repo>/pages/`. No model is called to
store or recall memory.

### Where the ideas come from

graphify (a code graph for coding agents), hindsight (agent memory), graphiti (facts that
change over time), omnigraph (a typed graph database in Rust), Headroom (a
context-optimizing proxy) and RTK (command-output filtering). No code was copied; what was
taken from each, and what was left out on purpose, is in `docs/PRIOR_ART.md`.

---

## 7. Configuration

### The daemon (`lge serve`)

| Option | Environment | Default | Meaning |
|---|---|---|---|
| `--port` | `LGE_PORT` | 8787 | Port of the proxy, page and MCP endpoint |
| `--host` | `LGE_HOST` | 127.0.0.1 | Address to listen on; keep it local |
| `--data-dir` | `LGE_HOME` | `~/.lge` | Graphs, usage log, settings |
| `--workspace` | `LGE_WORKSPACE` | any folder | Only folders under these may be indexed (`:`-separated) |
| `--codex-home` | `LGE_CODEX_HOME` | `~/.codex` | Where past Codex sessions are read from |
| `--upstream-api` | `LGE_UPSTREAM_API` | `https://api.openai.com/v1` | Where API-key requests go |
| `--upstream-chatgpt` | `LGE_UPSTREAM_CHATGPT` | `https://chatgpt.com/backend-api/codex` | Where ChatGPT-login requests go |

### Settings on the page

| Setting | Default | Meaning |
|---|---|---|
| Add graph context | on | The evidence pack after a typed message about the code |
| Shorten outputs | on | Check results, search caps, pointers for repeated output |
| Skeletons for whole-file reads | off | Experimental: functions the task does not need keep only their signature |
| Index on first use | on | A git repository is indexed the first time Codex works in it |

### Per repository (`lge.toml`, optional)

```toml
[[check]]                 # what `lge check` and `lge run` run
name = "tests"
kind = "tests"            # lint | types | tests
command = "pytest -q"

[prices]                  # per million tokens; adds cost columns to `lge usage report`
input_per_mtok = 0.0
cached_input_per_mtok = 0.0
output_per_mtok = 0.0
```

---

## 8. Uninstall

```bash
~/lge-src/scripts/install-macos.sh --uninstall           # keeps ~/.lge
~/lge-src/scripts/install-macos.sh --uninstall --purge   # also deletes ~/.lge
```

This stops lge, removes the start-at-login entry, removes the lge line and the
`local_graph` tool from `~/.codex/config.toml`, and deletes `~/.local/bin/lge`. Your
config from before lge stays at `~/.codex/config.toml.before-lge`.

Docker: `docker compose down` (add `-v` to delete the data volume).

---

## 9. Troubleshooting

| Symptom | Cause and fix |
|---|---|
| Codex hangs or keeps retrying | The line is in `~/.codex/config.toml` but lge is not running. Start it (`lge serve`, or install with `--autostart`), or remove the line |
| `The '<model>' model is not supported when using Codex with a ChatGPT account` | This comes from OpenAI, not lge: the model in your config is not available to your account in this Codex version. Pick one from `~/.codex/models_cache.json`, e.g. `codex -m <model>` |
| Log shows `failed to connect to websocket: 426` | Expected: lge declines the WebSocket and Codex switches to HTTP, which lge can read |
| Requests go to another proxy | `HTTP_PROXY` or `ALL_PROXY` is set in your shell: add `127.0.0.1` to `NO_PROXY` |
| `port 8787 is in use` | Another proxy (for example Headroom) holds the port. Stop it, or install with `--port 8788` and use that port in the config line |
| Build error `fatal error: 'format' file not found` | The C++ compiler is too old for LadybugDB's headers: update the Xcode Command Line Tools (on Linux, GCC 13 or later) |
| Build tries to run cmake | The LadybugDB download failed. Check the network; the version is pinned in `.cargo/config.toml` |
| The page shows no session after a request | Check `~/.lge/serve.log`: each model request leaves one line with the usage it read |
| Search is weak | `lge --db ~/.lge/setup/graph.lbug setup` installs the full-text extension (needs network once) |

---

## 10. Limits

- **Languages:** Python, TypeScript and JavaScript are indexed. Other files are not in the
  graph (Rust, Go, Java, ... are not yet supported).
- **Proxy:** only the OpenAI Responses API is rewritten (Codex). Other APIs pass through
  unchanged.
- **Savings are not measured yet.** The page shows real counts per session; judge from
  your own numbers. Both Headroom and RTK report that shrinking tool output by 90% saves
  far less than 90% of the whole cost, because most of a request is the conversation
  itself.
- **Edges marked inferred or unresolved are not verified.** lge says so in every answer.
- **One process owns each database.** While the daemon runs, use its page, API and MCP
  tools rather than `lge index` on the same repository.
- The LadybugDB `algo` extension does not load on macOS arm64, so ranking and communities
  are computed in Rust.

---

## 11. Project layout

| Crate | Role |
|---|---|
| lge-core | Shared types: records, edges, provenance, ids |
| lge-index | Scanner, git change detection, Tree-sitter extraction |
| lge-resolve | Resolver: imports, calls, API, table and queue edges with provenance |
| lge-store | `GraphStore` trait and the LadybugDB adapter; the only crate that touches the database |
| lge-retrieve | Index pipeline, seeds, traversal, ranking, evidence packs, graph views |
| lge-memory | Long-term memory: retain, recall, pages |
| lge-router | Reasoning-level recommendation from change impact (logged, not applied) |
| lge-session | `lge run`: one Codex session per task from its evidence pack |
| lge-filter | Lint, type and test output reduced to failures |
| lge-usage | Codex token usage parsing and reports |
| lge-mcp | MCP server |
| lge-proxy | The daemon: proxy, page, folder API, MCP over HTTP |
| lge-bench | Benchmark runner |
| lge-cli | The `lge` binary |

Development:

```bash
cargo build
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

More: `docs/PROXY.md` (the proxy in detail), `docs/ARCHITECTURE.md` (what each crate
promises), `docs/CONTEXT_GRAPH.md` (how tokens are saved at a fixed model and reasoning
level), `docs/PRIOR_ART.md` (what came from which project), `docs/PLAN.md` (the plan).

## License

MIT. See `LICENSE`.
