# token-reasoner

token-reasoner (command name `lge`, the Local Graph Intelligence Engine) sits between your
coding agent and the model. It builds a graph of your code, and
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

No saving is claimed here: the page shows the real token counts of your own sessions,
and two ways of measuring a difference are built in: `lge ab` runs your own tasks with lge
off and on, and a holdout share of conversations runs with every rule measured but not
applied.

What v0.0.2 adds (details in [CHANGELOG.md](CHANGELOG.md) and [docs/PROXY.md](docs/PROXY.md)):

- **Certain verdicts.** A test, lint or build run is called passed only when its own
  closing line ends the output, its exit code (when known) is 0 and nothing says it
  failed; warnings are listed with every location.
- **Twelve rules, each with a switch** (`off`, `shadow`, `on`): check results, search
  caps, repeat pointers, lossless clean-up, path folding and JSON minifying are on;
  skeletons, long-line clipping, progress noise and lock files are measured in shadow;
  re-run and re-read deltas are measured only. Running a command again with nothing
  changed in between returns its output whole.
- **Measurement.** A recovery rate per rule (a rule whose cuts are fetched back too often
  moves to shadow by itself), a holdout arm, a prompt-cache monitor, the fixed prefix of
  every request and the tools never called, and `lge usage discover` to replay your past
  Codex sessions through every rule.
- **Codex.** Code-mode scripts with several commands, exec results printed as JSON, RTK's
  output recognised, and one retry without the new rewrites when the provider refuses a
  request.
- **Tools.** `lge init --terse` (an opt-in terse-replies skill), `lge init --batch`, and
  `lge ab --variant`, `--resume`, a preflight, traces and bootstrap intervals.

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
Install token-reasoner (lge) on this Mac and connect Codex to it.
Follow the "Instructions for agents" section of
https://github.com/f8fvwgzc/token-reasoner/blob/main/README.md exactly.
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
   git clone https://github.com/f8fvwgzc/token-reasoner.git ~/token-reasoner
   cd ~/token-reasoner
   ```
   If `~/token-reasoner` already exists, run `git -C ~/token-reasoner pull` instead.
4. **Run the installer.**
   ```bash
   scripts/install-macos.sh --connect-codex
   ```
   It installs Rust and OpenSSL 3 (through Homebrew) if missing, builds lge (a few minutes the first time), copies it to
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
   with `~/token-reasoner/scripts/install-macos.sh --uninstall`, and that lge stops at logout
   unless `--autostart` was used.

The installer is safe to run again. It changes nothing outside `~/.local/bin/lge`,
`~/.lge`, `~/.codex/config.toml` (one line plus the `local_graph` tool entry) and, with
`--autostart`, `~/Library/LaunchAgents/dev.lge.daemon.plist`.

---

## 2. Install by hand on macOS

Requirements: macOS on Apple silicon or Intel, the Xcode Command Line Tools
(`xcode-select --install`), Rust 1.88 or later (`curl https://sh.rustup.rs -sSf | sh`), and
OpenSSL 3 (`brew install openssl@3`).

```bash
git clone https://github.com/f8fvwgzc/token-reasoner.git ~/token-reasoner
cd ~/token-reasoner
scripts/install-macos.sh --connect-codex --autostart
```

Or every step yourself:

```bash
export OPENSSL_DIR="$(brew --prefix openssl@3)"   # LadybugDB links against OpenSSL 3, not 4
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
git clone https://github.com/f8fvwgzc/token-reasoner.git ~/token-reasoner
cd ~/token-reasoner
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

### The AGENTS.md rule and the terse-replies skill (`lge init`)

`lge init` prints what connects a repository to Codex; with `--write` it puts the lge block
into the repository's `AGENTS.md` (replacing an earlier lge block, keeping everything
outside lge's markers byte for byte). The block tells the agent to ask the graph first,
and that a line starting with `lge:` says what was left out and how to get it back.

```bash
lge init --write                 # the lge block in AGENTS.md
lge init --write --batch         # with one more line: batch related read-only commands into one exec script
lge init --terse                 # the terse-replies skill in .agents/skills/terse-replies/SKILL.md
lge init --terse --global        # the same skill in ~/.agents/skills, for every repository
lge init --terse=always --write  # the skill, and a short section in AGENTS.md so every reply is short
lge init --terse=off             # removes the skill and the section, where lge wrote them
```

The batch line and the terse-replies skill are opt-in and unmeasured: compare with
`lge ab --variant batch` and `--variant terse` before keeping them. A skill file lge did not
write is never overwritten or removed. Codex's own `model_verbosity` setting also changes
how long its replies are; lge never sets it.

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
reasoning tokens, what lge removed from tool output, what graph context it added, and the
tokens not sent over every request net of lge's own packs. Below that:

- **Rules**: each rule's switch (off, shadow, on), what it changed or would have changed,
  its recovery rate and whether it is healthy or was demoted (with a Re-enable button),
  and the skeleton counts by language and file size;
- **Holdout**: the share of new conversations held out, and the comparison of the two arms;
- **Prompt cache**: cache breaks caused by lge (a bug), breaks caused by Codex changing its
  own history, and cold restarts after an idle gap;
- **Fixed prefix**: what Codex sends at the start of every request, by tool group, and the
  tools never called, with the `[mcp_servers.<name>]` entry that defines them (report
  only);
- **Past sessions replayed**: every rule run over your Codex sessions of the last 30 days,
  in memory;
- **Before and after**: "Import past Codex sessions" reads the token counts of your earlier
  sessions from `~/.codex` (no conversation text).

Under each folder the page also warns about folders that searches would wade through:
build output, dependencies, logs or archives that `.gitignore` does not exclude (such as
`artifacts/` or `dist/`, or anything over 50 MB or 5,000 files without source in it),
with their size and the exact line to add to `.gitignore`. `lge index` prints the same
warning.

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
lge usage discover              # past Codex sessions through every rule (reads only)
lge check                       # lint, types, tests: failures only
lge ab tasks.toml               # A/B: each task with lge off and on (spends tokens)
lge init --write                # the AGENTS.md rule; --terse, --batch: see section 4
```

`lge --help` lists everything.

To measure the saving on your own work, `lge ab tasks.toml` runs each task as one Codex
session with lge's features off and one with them on, checks the result with the task's
own command, and reports tokens, success and a verdict against the target: at least 25%
lower median total tokens with no drop in success. Every session runs in its own
temporary git worktree of the base commit (`HEAD`, resolved once at the start, unless
`--base <rev>` names another), the sessions run one after another, and nothing touches
your checkout. Each prompt ends with a note not to install dependencies or run long test
suites (the check runs afterwards; `note` in the task file or `--no-note` changes it), and
when your Codex has `--approve-for-me` the sessions use it in place of
`-s workspace-write`, so approval requests do not stall them (`--no-approve-for-me` turns
that off). On a managed machine: build against OpenSSL 3, not 4
(`export OPENSSL_DIR="$(brew --prefix openssl@3)"`), quote paths with spaces inside
`check` commands, and keep checks fast and offline. The task file and the details are in
[docs/AB.md](docs/AB.md).

What else `lge ab` does:

- **Preflight**, on by default: before any session, each task's setup and check run on a
  fresh copy of the base commit, and the run stops when a check already passes there or
  the shell cannot run it. `--preflight-only` runs only this (no daemon, no session, no
  token); `--no-preflight` skips it.
- **Results and resume**: one line per session goes to `results.jsonl` in the work dir as
  soon as its check ran; `--resume <work-dir>` runs only the sessions that did not finish
  (it refuses earlier results of another base commit, task file, model, variant, prompt
  note or sandbox unless `--force` is given).
- **Traces**: the daemon's record of each session's requests is saved under `traces/`, and
  the report shows the context per request and the largest tool outputs.
- **Variants**: `--variant features` (default: lge's features off against on),
  `--variant terse` (AGENTS.md without against with the terse-replies section) and
  `--variant batch` (the lge block without against with the batch line).
- **Readings beside the verdict**: billed-equivalent tokens (`--weights IN,CACHED,OUT`,
  default `1,0.1,8`), a bootstrap interval of the median change clustered by task, the
  geometric mean of the per-task ratios, and tasks whose runs varied too much.

---

## 6. How it works

### The problem

A coding agent spends most of its tokens finding its way: listing folders, grepping,
reading whole files, rereading the same output, and reading thousands of lines of passing
test output. Each of these turns is sent again on every later turn of the conversation.

### One request, step by step

1. **You ask Codex** for a fix, a feature or a change, in your own words.
2. **lge refreshes the graph.** Files that changed since the last request (including the
   ones Codex just edited) are parsed again and their edges resolved again, so the graph
   always matches the code on disk. Unchanged files cost nothing.
3. **lge finds what your message is about** in the graph: symbols you named, then
   full-text search over names, split identifier words, signatures, docs and comments.
4. **lge gathers the answer Codex would otherwise go looking for:** the files and line
   ranges, the source of the starting symbols, what they call, what calls them, the tests
   that cover them, the endpoints, queues and tables they reach, decisions remembered
   from earlier sessions, and the recent commits and merged pull requests that touched
   that code.
5. **lge adds that to your message** as one short context block, under a token budget,
   with the conclusions first ("the change starts at X, it affects Y, tests Z cover it").
6. **Codex starts from the answer** instead of listing folders, grepping and reading
   whole files. Its later tool output (test runs, searches, repeated reads) is shortened
   on the way out, and the conversation's cached prefix is kept intact.

### Codex code mode

Current Codex models run their commands inside a small JavaScript script
(`tools.exec_command({cmd: "rg -n ..."})`). lge reads the commands out of such a script and
shortens each printed output as it would a plain command's: one command, or several when
each is awaited once at the top level and printed once with its own `text()` call, and
exec results the script prints as JSON lines. The script itself is never changed. Anything
it cannot read with certainty is judged by its content or left alone.

### Why reasoning tokens drop too

The model and its reasoning level do not change, but the model reasons again on every
turn, over everything in front of it. lge lowers that in four ways:

- **Fewer turns.** Every search or file read the context replaces is one round of
  reasoning that never happens.
- **Conclusions, not raw material.** What a change affects, who calls it and which tests
  cover it are computed by the graph and stated first, so the model does not derive them.
- **No re-checking.** Edges resolved from the source are marked as such; the model is
  told that only the edges marked inferred or unresolved need checking.
- **No re-deciding.** Decisions and conventions stored in earlier sessions come with the
  context instead of being worked out again.

How much this saves depends on the task; the page shows reasoning tokens per session next
to your sessions from before lge, so you can see it on your own work.

### What lge does with each request

lge reads the request Codex sends, changes only what it understands, and forwards it:

| In the conversation | What lge sends instead | Rule, default |
|---|---|---|
| Output of a test, lint, type-check or build run | Every failure and every warning with its locations, or one line when it certainly passed | `check`, on. Formats it recognises: pytest, jest, vitest, cargo (and nextest), tsc, eslint, ruff, flake8, mypy, pyright, go, golangci-lint, Maven, Gradle, gcc, clang |
| A search with 60 or more matching lines that make up at least half of the output | The matches capped (6 per file, 60 in all), the files the task is about first, every other line kept in place, and a line naming every file left out | `search_cap`, on. `rg`, `grep`, `git grep`, `ag`, also inside Codex's code-mode scripts |
| The same large output a second time | One line naming the earlier run it repeats | `repeat_pointer`, on. While the first copy is still in the request |
| Escape codes, progress redraws, runs of identical lines | The text as a terminal shows it, each run once with its count | `inert`, on |
| A long list of paths | Each folder written once (`dir/ a b c`); unfolding gives back every byte | `path_fold`, on |
| JSON output | The same JSON without whitespace between its tokens | `json_minify`, on |
| A read of a whole file in the graph | Functions the task is not about keep their signature, with the `sed -n` that reads the rest | `skeleton`, measured in shadow |
| Search matches of hundreds of characters (minified code) | 300 characters around the match, with the `sed -n` for the whole line | `line_clip`, measured in shadow |
| Progress lines of installs, downloads and builds that exited 0 | The rest of the output, with lines about errors and warnings always kept | `noise`, measured in shadow |
| A lock file read whole | Its package list | `lockfile`, measured in shadow |
| A message you typed about the code | Your message, followed by the graph's context for it (600 tokens for a one-file question, up to 1,500 when the change spans files) | "Add graph context", on. When it names a symbol or clearly matches one |
| Anything else | Exactly the same bytes | Always |

Never touched: the system prompt, tool definitions, reasoning, tool calls, patches, diffs,
and any output lge cannot classify with certainty. Every `lge:` line says what was left out
and how to get it back, and running the same command again with nothing changed in between
returns the output whole (so does `LGE_RAW=1 <command>`).

**Modes and measurement.** In `shadow` a rule runs and its would-be saving is counted, but
the output is sent as it is; switch a rule to `on` on the page once its counts look right.
For every item a lossy rule shortens, lge watches the next 3 tool calls (5 for a skeleton)
for a recovery: a re-run, a read of a range a marker named, a search of a file a note
listed. A rule in `on` recovered more than 15% of the time over at least 200 items moves to
shadow for new items by itself. With a holdout share above 0, that share of new
conversations runs every rule in shadow, and the page compares the two arms.

**Prompt caching.** The agent resends the whole conversation every turn, and the provider
charges much less for the unchanged start of it. lge decides how to rewrite each item
once, then sends the same bytes for it on every later turn, whatever the settings say by
then, so the cached start stays cached; the page's cache monitor checks this on every
request. A request in which nothing changed is forwarded byte for byte. If the provider
refuses a rewritten request (400, 413 or 422), lge sends it once more without the rewrites
first made in that request.

### Privacy: what is written to disk

lge runs on your machine; the daemon forwards requests only to the provider your agent
already uses. Per conversation it stores token counts, the model name, the first message
you typed cut to 200 characters, the router's recommendation, counts per rule and of the
cache monitor, two token totals, and a ledger of decisions made of hash prefixes and rule
ids (no content). Settings go to `daemon.json`, and `serve.log` gets one line per model request
without any content. Request traces, the replay of past sessions and the fixed-prefix
report stay in memory. Credentials pass through and are never stored or logged.
`lge usage discover` stores nothing unless you add `--retain`, which keeps corrected
commands as facts in the repository's graph. `lge ab` writes `results.jsonl` (token
counts, success, the end of a failing check's output) and `traces/` (token counts per
request, commands cut to 120 characters, no output text) in its work dir. The full list is
in `docs/PROXY.md`, "What is stored".

### The code graph

```
files ─► scan ─► parse (tree-sitter) ─► resolve ─► LadybugDB ─► rank ─► evidence pack
```

1. **Scan.** Source in 36 languages, honouring `.gitignore`, plus HTML, CSS, Markdown,
   SQL, YAML, TOML and JSON as outlines (see the table below). Only new and changed files
   are parsed again (content hash), and a changed working tree is picked up before each
   answer.
2. **Parse.** With tree-sitter, every function, method, class, interface and type with its
   signature, doc line, body and line range, and every call with its receiver. Python,
   TypeScript and JavaScript go deeper: imports, HTTP routes and calls, SQL tables, ORM
   models, queue publish and consume, base classes. `NOTE:` / `WHY:` / `HACK:` comments are
   kept with their symbol in every language. Documents are outlined: page titles, element
   ids, headings, selectors, tables and top-level keys, each with its lines.

   | Depth | Languages |
   |---|---|
   | Full (imports, routes, tables, queues) | Python, TypeScript, JavaScript |
   | Definitions and calls | Rust, Go, Java, Kotlin, Scala, Groovy, C, C++, CUDA, C#, Swift, Objective-C, Dart, Ruby, PHP, Perl, Lua, Luau, Bash, PowerShell, R, Julia, Elixir, Erlang, Gleam, Haskell, OCaml, F#, Elm, Clojure, Zig, Nix, Solidity |
   | Outline | HTML (also Vue, Svelte), CSS, SCSS, Less, Markdown, SQL, YAML, TOML, JSON |

   In the languages without import resolution, a call to the same file or to the own class
   is verified; a call matched by name across files is marked inferred (one candidate) or
   unresolved (several), never verified.
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
also written as markdown pages under `~/.lge/graphs/<repo>/pages/`. Every git commit, merges (merged pull requests) included, is
also stored as a fact linked to the files it changed, so asking about code brings its
recent history along without `git log`. No model is called to store or recall memory.

### Where the ideas come from

graphify (a code graph for coding agents), hindsight (agent memory), graphiti (facts that
change over time), omnigraph (a typed graph database in Rust), Headroom (a
context-optimizing proxy), RTK (command-output filtering), Caveman (terse replies) and
Token Optimizer (measuring what a cut costs). No code was copied; what was taken from
each, and what was left out on purpose, is in `docs/PRIOR_ART.md`.

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

### Settings on the page (`daemon.json`)

| Setting | Key | Default | Meaning |
|---|---|---|---|
| Add graph context | `inject_context` | on | The evidence pack after a typed message about the code |
| Apply the rules to tool output | `shorten_outputs` | on | The master switch of every rule below; off, new outputs get no rule, not even in shadow |
| Rules | `rules` | `check`, `search_cap`, `repeat_pointer`, `inert`, `path_fold`, `json_minify` on; `skeleton`, `line_clip`, `noise`, `lockfile` shadow; `rerun_delta`, `read_delta` measured only | The mode of each rule: off, shadow or on |
| Holdout | `holdout_share` | 0 | The share of new conversations (0 to 1) that run every rule in shadow, for comparison |
| Index on first use | `auto_index` | on | A git repository is indexed the first time Codex works in it |
| - | `context_budget` | 1,500 | Tokens of one pack (at most 600 for a question about one file) |

A change applies to outputs seen from then on; outputs already sent keep their bytes.
`shorten_reads` from v0.0.1 still loads (true means `skeleton` on, false means shadow).
Demotions (`demoted`, `rule_since`) are written by the daemon and lifted with Re-enable.
An unreadable `daemon.json` is copied to `daemon.json.unreadable` and the defaults are
used, with a warning on the page, until settings are saved from the page.

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
~/token-reasoner/scripts/install-macos.sh --uninstall           # keeps ~/.lge
~/token-reasoner/scripts/install-macos.sh --uninstall --purge   # also deletes ~/.lge
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
| Build error `library 'ssl' not found` | OpenSSL 3 is missing or not found: `brew install openssl@3` and `export OPENSSL_DIR="$(brew --prefix openssl@3)"`, then `cargo clean -p lbug` and build again |
| Build tries to run cmake | The LadybugDB download failed. Check the network; the version is pinned in `.cargo/config.toml` |
| The page shows no session after a request | Check `~/.lge/serve.log`: each model request leaves one line with the usage it read |
| Search is weak | `lge --db ~/.lge/setup/graph.lbug setup` installs the full-text extension (needs network once) |

---

## 10. Limits

- **Languages:** 36 languages are indexed, at the depths in the table in section 6. Files
  in other languages are not in the graph. Source files over 2 MiB (almost always
  generated) keep their definitions but not their calls; files over 16 MiB are skipped.
- `.h` headers are read as C, C++ or Objective-C by their content, and `.m` files that are
  MATLAB rather than Objective-C are left out, as graphify does.
- **Proxy:** only the OpenAI Responses API is rewritten (Codex). Other APIs pass through
  unchanged.
- **Savings are not measured yet.** The page shows real counts per session; judge from
  your own numbers, or measure with `lge ab` and the holdout arm. The terse-replies skill
  and the batch line are unmeasured until `lge ab --variant` measures them. The code-mode
  layouts for several commands and JSON exec lines are built from Codex's source, not from
  a real capture. Both Headroom and RTK report that shrinking tool output by 90% saves
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
| lge-filter | Lint, type, test and build output reduced to failures and warnings |
| lge-usage | Codex token usage parsing and reports |
| lge-mcp | MCP server |
| lge-proxy | The daemon: proxy, rules, page, folder API, MCP over HTTP, replay of past sessions |
| lge-bench | Benchmark runner, `lge ab`, and the AGENTS.md texts `lge init` writes |
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
