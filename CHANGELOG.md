# Changelog

## Unreleased

- The page's folder picker starts at the home folder when nothing is typed (and no
  workspace limit is set), accepts `~/`, and shows the nearest existing folder with a
  note when the typed one was deleted or moved. Before, it showed an empty list.
- A folder added to lge and later deleted or moved shows as `missing` with a hint to
  remove it, instead of `ready` with its old counts.
- The warning about folders that searches would read names `.ignore` instead of
  `.gitignore` when the folder is not a git repository: `rg` does not read `.gitignore`
  there, so the old advice quieted the warning without changing any search.

## v0.0.2 (2026-11-16)

lge still never changes the model, the reasoning level, the system prompt, tool
definitions, reasoning items, tool calls, patches or diffs. No saving is claimed: the page
shows each session's numbers, and `lge ab` and the holdout arm measure the difference.

### Fixes

- **A pass is stated only when it is certain.** A test, lint or build run becomes one
  `ok` line only when the tool's own closing line ends the output, the process is not
  still running, its exit code is 0 or unknown, the shell did not hide the status (a pipe
  without `pipefail`, `|| true`, `||`, `;`), no line says the run failed (cargo's
  `error: 1 target failed`, go's `FAIL`, `make: *** ... Error 1`, `BUILD FAILED`, ...), and
  the command is one producer. `cargo` counts as a check only for `test`, `build`,
  `check`, `clippy` and `nextest`; `go` for `test`, `build` and `vet`; `golangci-lint` for
  `run`. v0.0.1 could send a `cargo run` that panicked after a warning, or
  `pytest && script`, as `ok`.
- **Warnings are listed with every location**, grouped by rule, instead of
  `ok: N warnings`.
- **Changing a setting no longer re-decides items already sent.** Each item's decision
  is replayed before the settings are read, so switching a rule (on the page, or `lge ab`
  switching arms while another session runs) keeps the prompt cache.
- **A conversation's usage record survives a daemon restart.** Its counters continue,
  its holdout arm is kept, and a ledger of hash prefixes and rule ids (no content) makes
  the items decided before the restart get the same rules again without being counted
  twice.
- The page's API refuses requests whose `Host` or `Origin` is not the daemon's own, and
  POST requests that are not JSON.

### Shortening

Each rule has a mode (`off`, `shadow`, `on`); see `docs/PROXY.md` for the full table.

| Rule | Default | What is new |
|---|---|---|
| `check` | on | Parsers for go test (text and `-json`), go build, go vet, golangci-lint, Maven, Gradle, gcc and clang, and cargo nextest. Every failed Gradle task and Maven goal is kept |
| `search_cap` | on | The closing line names every file left out with its number of matches (up to about 1,000 tokens), then how many more |
| `repeat_pointer` | on | Names the run it repeats (run 2 of `cargo test`) and quotes its first line |
| `inert` | on | New, lossless: escape codes, carriage-return redraws, trailing blanks, runs of 3 or more identical lines |
| `path_fold` | on | New, lossless: path lists written with each folder once, sent only when unfolding gives back the input byte for byte |
| `json_minify` | on | New, lossless: whitespace outside strings removed from JSON outputs |
| `skeleton` | shadow | Was off: now measured on every eligible read of an indexed file, by language and size |
| `line_clip` | shadow | New: search matches over 500 characters clipped around the match |
| `noise` | shadow | New: progress lines of installs, transfers and builds that exited 0 |
| `lockfile` | shadow | New: a lock file read whole shown as its package list |
| `rerun_delta`, `read_delta` | measured only | New: what a diff against an earlier copy would save; never sent |

Running the same command again with nothing changed in between returns its output whole.

### Measuring

- A switch per rule on the page and in `daemon.json` (`rules`); `shadow` counts what a
  rule would save and sends the original.
- A recovery tripwire: a rule in `on` whose outputs are followed by a recovery (a re-run,
  a read of a range a marker named, ...) more than 15% of the time over at least 200
  judged outputs moves to `shadow` for new items by itself. The thresholds are fixed in
  code.
- A holdout arm (`holdout_share`, default 0): a share of new conversations runs every
  rule in shadow, and the page compares the two arms with bootstrap intervals.
- A cache monitor: cache breaks caused by lge (a bug) told apart from Codex changing its
  own history, and cold restarts after an idle gap.
- The fixed prefix per request by tool group, and the tools never called (report only).
- The tokens not sent, summed over every request, net of lge's own packs.
- `lge usage discover` and the page's "Replay past sessions": past Codex sessions run
  through every rule, in memory.
- Request traces in memory, for `lge ab`.

### Codex

- Code-mode scripts with several commands: each printed part is matched to its command
  when the script's shape makes that certain.
- Exec results a script prints as JSON lines: each line's output is decided on its own
  and spliced back with every other byte of the line kept.
- RTK interop: `rtk proxy`/`rtk bypass` are read as the command they run; output RTK
  condensed gets only the lossless clean-up.
- A rewritten request the provider refuses with 400, 413 or 422 is sent once more
  without the rewrites first made in it.
- The fixed-prefix report reads the base instructions from the opening developer message
  and the tools from `additional_tools` items, as Codex's source sends them, and groups MCP
  tools as Codex names them.

### Tools

- `lge init --terse[=on|always|off] [--global]`: an opt-in terse-replies Codex skill,
  and with `always` a short AGENTS.md section. Never on by default.
- `lge init`: the AGENTS.md block gains a line on condensed output; `--batch` adds an
  opt-in line on batching read-only commands.
- `lge ab`: `--variant features|terse|batch`; a preflight before any session
  (`--no-preflight`, `--preflight-only`); `results.jsonl` and `--resume`; the daemon's
  traces per session; `--weights` for billed-equivalent tokens; a bootstrap interval
  clustered by task, the geometric mean of the per-task ratios, and noisy tasks flagged.
- `lge usage discover [--days N] [--json] [--retain]`.

### Not verified

- The multi-command and JSON-line layouts of code mode are built from Codex's source, not
  from a real Codex 0.160 capture.
- No saving is measured yet. The terse and batch lines are unmeasured until
  `lge ab --variant` measures them.

## v0.0.1 (2026-11-15)

First release of token-reasoner (`lge`): a local code graph that sits between Codex and
OpenAI and sends the model less text for the same work, at the same model and reasoning
level.

- **Proxy** (`lge serve`, 127.0.0.1:8787): one line in `~/.codex/config.toml`
  (`openai_base_url`) routes Codex through lge. Adds the graph's context to a message about
  the code, with the conclusions first; shortens test, lint and type-check output to
  failures; caps long searches; points repeated output to its first copy; keeps the prompt
  cache intact; fails open. A page shows input, cached, output and reasoning tokens per
  session.
- **Code graph** on LadybugDB (embedded): 36 languages through tree-sitter (Python,
  TypeScript and JavaScript with imports, routes, tables and queues; 33 more with
  definitions and calls), plus outlines of HTML, CSS, Markdown, SQL, YAML, TOML and JSON.
  Every edge records whether it is verified, inferred or unresolved. Changed files are
  re-indexed before each request.
- **Memory**: decisions and conventions stored by the agent, and every commit and merged
  pull request, recalled with the code they concern.
- **Graph tools** over MCP (HTTP and stdio): search, change impact, execution flow,
  dependencies, exact source, architecture, recall, retain.
- **Install**: `scripts/install-macos.sh` (Rust, OpenSSL 3, build, daemon, Codex
  connection, optional start at login), or Docker.

Tested with real Codex sessions on macOS (codex-cli 0.160.0, ChatGPT login). Token savings
are not measured across a body of work yet; the page shows each session's numbers.
