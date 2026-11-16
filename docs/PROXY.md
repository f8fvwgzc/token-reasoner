# The proxy: lge between the agent and the model

```
Codex desktop app / Codex CLI  ->  lge on 127.0.0.1:8787  ->  OpenAI
```

`lge serve` is one local process (or one container) that is:

- a **proxy** for the provider's Responses API: it forwards every request, shortens
  what it understands, and relays the streamed answer untouched;
- the **owner of the graphs**: one database per folder you add, kept fresh;
- the **graph tools** for the agent, over MCP on HTTP (`/mcp`);
- a **page** (`/`) that shows input, cached, output and reasoning tokens per session,
  what each rule did, the cache monitor, the fixed prefix, the holdout comparison and
  the folders in the graph.

The model and the reasoning level are whatever the agent asked for. The proxy never
changes them.

## What it changes in a request

Only two kinds of item in the conversation, and nothing else in the body:

- **a tool output**, which a rule may replace (below);
- **a message the user typed**, after which one context item may be added: the evidence
  pack for that message, in `<lge_context>` tags, when it names a symbol or clearly
  matches one (a search hit that shares at least two words with it). Small talk gets
  nothing, the same starting points are never added twice, at most 6 packs are added per
  conversation, and a pack whose starting points are all in one file that nothing
  outside it uses is at most 600 tokens (else the context budget, 1,500 by default).

Never touched: the system prompt, tool definitions, reasoning items, tool calls
(code-mode scripts included), patches, diffs, reads of a line range that does not start
at the top, and any output the proxy cannot classify with certainty. A request in which
nothing was replaced or added is forwarded as the exact bytes that came in.

### The rules for tool outputs

| Rule | What it does | Default | Its `lge:` line | How to get the content back |
|---|---|---|---|---|
| `check` | A test, lint, type-check or build run (pytest, cargo `test`/`build`/`check`/`clippy`/`nextest`, jest, vitest, tsc, eslint, pyright, mypy, ruff, flake8, go `test`/`build`/`vet`, golangci-lint `run`, Maven, Gradle, gcc, clang) becomes every failure, every warning grouped by rule with all its locations, or one line when it passed (see "When a pass is stated") | on | ``lge: output of `cargo` reduced to its result and its 3 warnings (every location listed). Running the same command again before changing anything prints it whole.`` | Run the same command again before changing anything |
| `search_cap` | An `rg`, `grep`, `git grep` or `ag` search (alone, or piped only into `head`, `tail`, `sort`, `uniq`, `grep` or `rg`) with 60 or more matching lines that make up at least half of its non-blank lines is capped: the files the task is about first, then the files with the most matches, 6 lines per file, 60 in all. Lines that are not matches stay where they are | on | `path: (+4 more matching lines in this file)` after a capped file, and a closing line: ``lge: 60 of 412 matching lines shown, from 14 of 37 files. Not shown: a.py (12), b.py (3), ... Narrow the pattern or search one file to see the rest, or run the same search again before changing anything to see it whole.`` Every file left out is named with its count while the list fits in about 1,000 tokens, then `, and k more files (m matching lines; rg -l with the same pattern lists them)` | Narrow the pattern, search one file, or run the same search again before changing anything |
| `repeat_pointer` | The same output of 2,000 bytes or more a second time, while the first copy, sent whole, is still in the request, becomes one line. Never a pointer to a shortened copy | on | ``lge: identical to the output of run 2 of `cargo test` earlier in this conversation (304 lines, first line: "running 12 tests"); it is not repeated here.`` | The first copy is in the conversation; once it is gone (after a compaction) the item is decided again |
| `skeleton` | A read of a whole indexed file (`cat`, `cat -n`, `nl -ba`, or a range from line 1): definitions the task is not about keep their signature and first docstring line; each body of 8 or more lines is replaced by a marker. Only when the file inside the repository is exactly what was indexed and the output is exactly its lines from the top | shadow | `# lge: shortened view of svc/orders.py (lines 1-120): 2 definition(s) this task does not involve are reduced to their signature.` and ``# lge: 31 lines of function `create_order` left out; read them with: sed -n '5,35p' svc/orders.py`` | The `sed -n` in the marker; once a file's skeleton was sent, every later read of it is sent whole |
| `inert` | Escape codes removed, carriage-return redraws applied, trailing blanks trimmed, and runs of 3 or more identical lines of 8 or more characters collapsed. Runs first, on every output except reads, diffs and JSON | on | `lge: the line above repeats 41 more times` | Nothing is lost: the line and its count are there |
| `path_fold` | A list of 40 or more plain paths (`rg --files`, `rg -l`, `grep -l`, `git ls-files`, `git ls-tree --name-only`, `fd`, `find` without `-exec`, `-printf`, `-print0`, ...) writes each run of files in one folder as `dir/ a b c`, in the original order. Sent only when unfolding it gives back the input byte for byte | on | `lge: paths grouped by folder; a line "dir/ a b" stands for dir/a and dir/b.` | Nothing is lost |
| `json_minify` | One whole JSON object or array of at most 8 MB loses the whitespace outside its strings; keys, values, number spellings, escapes and key order stay byte for byte. For commands that print JSON (`gh api`, `curl`, `jq`, `kubectl -o json`, `aws`, `--json`, `--format json`, ...) and for tools that are neither a shell nor a file reader | on | none | Nothing is lost |
| `line_clip` | A numbered search match (`path:line:text`, or rg's heading layout) whose text is over 500 characters keeps 300 characters from 100 before the pattern's first match (from the start when the pattern does not match) | shadow | `...[lge: +4210 chars; whole line: sed -n '17p' dist/app.js]` | The `sed -n` in the marker |
| `noise` | Progress lines of a command that exited with code 0, from a table of entries: installs (npm, pnpm, yarn, bun, pip, uv, poetry, bundle, composer, brew, `cargo fetch`/`update`/`install`), `git push`/`pull`/`fetch`/`clone`, the hints of a long `git status`, wget, `docker build`/`pull`, make, `terraform plan`, pre-commit. A line that mentions an error, a warning, a failure, a deprecation, a vulnerability, security, a denial, a conflict, a fatal condition or a URL is always kept, and so is a `remote:` line that is not git's own progress counter. There is never a cap on lines | shadow | ``lge: 214 progress lines of `npm install` left out (exit code 0).`` | Run the same command again before changing anything |
| `lockfile` | A whole read of `Cargo.lock`, `package-lock.json`, `npm-shrinkwrap.json`, `pnpm-lock.yaml` or `yarn.lock` becomes its sorted `name@version` list | shadow | ``lge: Cargo.lock shown as its package list (60 packages, name@version, sorted); hashes, sources and dependency lists are left out. Find an entry with: rg -n -A4 'name = "NAME"' Cargo.lock`` | The `rg` in the marker, or a range read |
| `rerun_delta` | What a diff against the latest copy of the same command sent whole in the request would save. Measured only, never sent | measured | none (never sent) | - |
| `read_delta` | What a diff against an earlier whole read covering the same lines would save. Measured only, never sent | measured | none (never sent) | - |

Every result except the repeat pointer is sent only when the output had at least 400
tokens and the result is at most 70% of it. Which rules run depends on what the command
was for:

| Command | Rules, in order |
|---|---|
| A check run (above) | `inert`, `check`. With a flag that asks for the program's own prints (`--nocapture`, `pytest -s`, `mvn -X`, `gradle --info`, ...) only `inert`; with a fix flag (`--fix`, `--write`, `fmt`, `format`) it is read as any other command |
| A search | `inert`, `line_clip`, `search_cap` |
| A path list | `inert`, `path_fold` |
| JSON | `json_minify` |
| A whole read of a lock file | `lockfile` |
| Another whole read, or a read from line 1 | `skeleton` (when the repository has a graph) |
| A read further down, a diff | none |
| `rtk <cmd>`, or output with RTK's markers | `inert` only |
| `LGE_RAW=1 <cmd>` | none |
| Any other command | `inert`, then `check` judged by the content; if the output is not a check run and the exit code is known to be 0, `noise` |
| No known command (another tool, a script the proxy cannot read) | escape codes and carriage returns only, `check` by content, `search_cap` for numbered matches only, and `json_minify` for a tool that is not a shell, `write_stdin`, or a tool whose name looks like a file reader |

An output still sent whole after its rules may then become a repeat pointer, and the
measured deltas are counted for it.

A command is read after dropping leading statements that print nothing (`cd DIR`,
`export A=b`, `A=b`, `set -euo pipefail`) and wrappers that do not change what is printed
(`A=b`, `env`, `rtk proxy`, `git -C dir`). Only a command that is then one producer,
piped at most into a few display filters, gets a purpose; anything else (two
statements joined by `&&`, a substitution, a subshell, a redirection to a file) is
"any other command".

## When a pass is stated

A check run is replaced by one `ok` line only when all of these hold:

- the tool's own closing line is in the output (`12 passed`, `test result: ok.`,
  `BUILD SUCCESS`) and nothing but the tool's epilogue follows it: output that continues
  (`cargo run`, `pytest && script`) or was cut (`| head`) is sent as it is;
- the process is not still running;
- its exit code, when known, is 0;
- the shell did not hide the status: a pipe without `set -o pipefail`, a final
  `|| true`, any `||`, or statements joined by `;` or a line break make the reported
  status someone else's, so such a run is never called passed;
- no line says the run failed: cargo's `error: 2 targets failed`, `error: test failed`,
  `error: could not compile`, go's bare `FAIL` or `--- FAIL:`, make's `*** ... Error 1`,
  `npm ERR!`, `ELIFECYCLE`, yarn's `error Command failed`, Gradle's `BUILD FAILED`,
  Maven's `BUILD FAILURE`;
- the tool did not count failures that no parser read (`1 failed` with no failure
  found);
- the command runs the tool itself, or behind a launcher (`npx`, `python -m`, `uv run`,
  `timeout 600`, ...), and passes its output on whole (`| cat` at most). `cargo` is a check
  only for `test`, `build`, `check`, `clippy` and `nextest`, `go` for `test`, `build` and
  `vet`, `golangci-lint` for `run`.

A run with failures is reduced to its failures and warnings even when its status is
hidden. A non-zero exit with no failure found is sent as it is: lge does not know why it
failed. When no exit code is known (a code-mode part that printed only the output, a tool
that is not a shell), a run can be called passed only by its closing line and the other
checks above.

## Getting back what was left out

Every `lge:` line names a way back (the table above). Two ways work for any command the
proxy can read:

- **Run the same command again with nothing changed in between.** When an earlier call
  in the conversation ran the same normalised command, its output was shortened by a
  lossy rule, and no call that may have changed files lies between them, the new output
  is sent whole. A change is any shell command that is not read-only, `apply_patch`, a
  code-mode script that may write, and any tool except lge's own graph tools and Codex's
  `update_plan`, `view_image`, `list_mcp_resources`, `list_mcp_resource_templates`,
  `read_mcp_resource` and `tool_search`. A third identical run points at the second, which
  was sent whole. A re-run after an edit is shortened again: it checks the change.
- **`LGE_RAW=1 <command>`** is never shortened, in a chain too. Codex matches its saved
  approvals against the start of a command, so a command that starts with `LGE_RAW=1` may
  not match an approval saved for the command alone and may ask again.

## Code mode

Current models run tools from a JavaScript script (a `custom_tool_call` named `exec`).
Its result is a status part (`Script completed`, wall time) followed by one `input_text`
part per `text()` call; the script is only read, and the status part, the number of
parts and every part that is not text stay as they are.

- **One command.** When the script calls `tools.exec_command` exactly once, outside any
  loop or function, with a `cmd` written as a plain string, the printed text is treated
  as the output of that command, as if it had been run directly.
- **Several commands.** Accepted only when every call has a literal `cmd` and runs once
  at the top level, each as `const|let|var NAME = await tools.exec_command({...})` or in
  one top-level `const [a, b] = await Promise.all([tools.exec_command({...}), ...])`;
  every `text(...)` is a top-level statement that prints `NAME.output` or
  `(await tools.exec_command({...})).output`; each command is printed exactly once; and
  nothing else calls `tools.`, uses `console`, or uses the names bound to the results.
  Part k is then the output of the command the k-th `text()` call prints, but only when
  the number of text parts equals the number of `text()` calls. Codex merges every part
  into one under a truncation warning when the output is over its budget, and a script
  error adds a part; in either case the parts are judged by content alone.
- **Exec results printed as JSON lines.** A script that prints a whole exec result gets
  a line such as `{"chunk_id":"..","wall_time_seconds":0.4,"exit_code":1,"output":".."}`.
  A line is read only when it is one JSON object with a string `output`, an `exit_code`
  or a `chunk_id`, and no key other than Codex's exec-result keys (`chunk_id`,
  `wall_time_seconds`, `exit_code`, `session_id`, `original_token_count`, `output`). Its
  output is decided on its own (by the script's command when it runs one, by content
  otherwise) and spliced back between the line's original bytes, and only when encoding
  the decoded output again gives back the exact bytes; a line cut short or not exact stays
  as it is. The exit code comes from the line, and a `session_id` without an exit code
  means the process is still running. Parts of a script with several commands are not
  read this way: they print `.output`, not whole results.
- **Anything else** (a computed command, a loop, a script the proxy cannot read) is
  judged by the content of its output alone, which only cleans escape codes, shortens
  recognised check runs and long lists of numbered search matches (`path:line:text`, or
  rg's heading layout); a line that merely starts with `name:` (YAML keys, test ids,
  `host:port`) is never taken for a match there.

## RTK

[RTK](https://github.com/rtk-ai/rtk) condenses command output before the agent sees it.
lge never shortens an output twice:

- `rtk proxy <cmd>` and `rtk bypass <cmd>` print the raw output: read as `<cmd>`.
- `rtk <cmd>` has been condensed by RTK: only `inert` applies.
- Output that carries RTK's markers (`rtk recall <hex>`, `[rtk:`, `lines omitted by rtk`,
  `[full output: `, `full output in recall file]`, `[full diff: rtk `) also gets only
  `inert`, and is counted as condensed upstream. A file read or a diff that merely
  contains these words is still treated as a read or a diff.

## Rule modes and shadow

Every rule runs in one of three modes, set per rule on the page or in `daemon.json`
(`rules`):

- **off**: not run.
- **shadow**: run, and what it would have sent is counted (items, tokens before and
  after, and whether its follow-up calls recovered the content); the original is sent.
- **on**: its result is sent.

A mode applies to items seen from then on; an item already sent keeps its bytes. The
master switch "Apply the rules to tool output" (`shorten_outputs`) off means new items get
no rule at all, not even in shadow. `rerun_delta` and `read_delta` are measure-only: `on`
is treated as `shadow`. The lossy rules are `check`, `search_cap`, `skeleton`,
`line_clip`, `noise` and `lockfile`; `inert`, `path_fold`, `json_minify` and
`repeat_pointer` are lossless.

In shadow or on, the skeleton rule also records, for every eligible read of an indexed
file of 16 KB to 2 MB, its cohort (language and size band: 16-64K, 64-256K, 256K-1M,
1-2M), what a skeleton would save, and whether the file was patched within the next 5
calls. Nothing promotes a rule by itself: you switch a rule to `on` after reviewing its
counts.

## Recovery rate and demotion

For each item a lossy or measure-only rule changed (or, in shadow, would have changed),
lge watches the next calls in the same conversation, read from the history each request
carries: 3 calls, 5 for a skeleton. The item is judged once that many calls exist. It
counts as recovered when one of those calls:

- runs its producer again (program and arguments) with no change in between; an
  `LGE_RAW=1` re-run counts even after a change;
- reads a range one of its markers named (`sed -n 'A,Bp' file`; a whole read of that file
  covers it);
- searches or reads a file its note listed as left out;
- for a skeleton, patches the file that was read.

Every 50 recorded responses the daemon adds up each rule over the stored sessions of the
last 30 days (each session counted for a rule only when it started at or after the rule's
`rule_since`). A rule set to `on` with at least **200** judged items and a recovery rate
above **15%** is demoted: it runs in `shadow` for new items, items already sent keep their
bytes, the reason (`recovery 18.5% over 214 items since 2026-11-02`) is saved in
`daemon.json` (`demoted`) and logged. A rule with at least 200 judged items and a rate
below **5%** is shown as healthy. "Re-enable" on the page removes the demotion and sets
`rule_since` to now, so earlier items no longer count. These numbers are fixed in code
(`crates/lge-proxy/src/rules.rs`: `FOLLOW_UP_CALLS`, `SKELETON_FOLLOW_UP_CALLS`,
`MIN_JUDGED`, `DEMOTE_ABOVE`, `HEALTHY_BELOW`) and were set before any data.

If `daemon.json` cannot be read, the daemon starts from the default settings (rules at
their default modes, not all in shadow), keeps the list of folders when it can still be
read, copies the file to `daemon.json.unreadable`, logs a warning and shows it on the
page. The file's settings are left as written until settings are saved from the page.

## Holdout

`holdout_share` (0 by default, from 0 to 1) holds out a share of new conversations: in
them every rule runs in shadow and packs are decided and counted but not added, so the
client's bytes are forwarded. A conversation is assigned once, when it is first seen, from
the first 16 hex characters of the hash of its `prompt_cache_key` (which Codex shares
between a parent and its sub-agents); a share of 1 holds out every conversation. The arm
is stored with the session (`proxy` or `proxy-holdout`) and kept across restarts.

The page compares the two arms over the sessions of the last 30 days: per-session medians
of input, cached, uncached, output and reasoning tokens, model requests, and tokens not
sent, with the difference (proxy less holdout) and its 95% bootstrap interval (2,000
resamples, fixed seed). No difference is shown while either arm has fewer than 30
sessions. A held-out conversation gives up whatever the rules would have saved in it.

## Cache monitor and fixed prefix

For each conversation lge keeps, in memory, a hash per input item as Codex sent it and as
lge forwarded it. On each request it compares them with the previous request's: the
first item Codex changed is a **client break** (a compaction, an edit of the history);
before it, an item Codex left alone but lge forwarded differently is a **break caused by
lge**, which is a bug: it is logged (`lge: cache break caused by lge at item 12
(conversation 1a2b3c4d)`) and counted. A **cold restart** is a response more than 5
minutes after the previous one of the conversation, after a request of at least 4,096
input tokens, that read less than half of that input from the cache; the uncached tokens
are counted.

The **fixed prefix** is what Codex sends at the start of every request: its base
instructions (the `instructions` field, or the developer message that opens the input, as
current Codex sends them) and its tool definitions (`tools`, and the tools of
`additional_tools` input items). They are counted in tokens once per distinct version,
and the tools are grouped as Codex names them: its own tools as built-in, an MCP server's
tools (`mcp__server__` namespaces, or flat `mcp__server__tool` names) by server. Tools
never called since the daemon started are listed with their tokens and, for an MCP server,
the `[mcp_servers.<name>]` entry of `~/.codex/config.toml` that defines them. This is a
report only: lge never changes tool definitions or your configuration.

The page also shows, per session, the tokens not sent: over every request, the tokens
before less the tokens after of each replaced item present in it, next to the tokens of
lge's own packs present in it, and the net of the two.

## Retry after a rejected request

When the provider answers 400, 413 or 422 to a body lge changed, and the request made new
changes (a replaced output, a pointer or a pack first decided in it), before anything is
relayed, lge:

1. reads at most 64 KB of the error body and logs only its status, error type and code;
2. makes the request's new decisions "send as is" for the rest of the conversation;
   decisions from earlier requests stay, so the cached prefix survives;
3. sends the request once more: rewritten with the older decisions, or the client's own
   bytes and headers when nothing is changed any more;
4. relays that answer and counts the retry.

429 and 5xx answers are never retried, and nothing is retried after a byte was relayed.
This assumes a conversation sends one request at a time, as Codex does.

## Why the prompt cache keeps working

The agent resends the whole conversation on every turn, and the provider bills the
unchanged start of it at the cached rate. A proxy that rewrote an old item differently
on a later turn would turn cached tokens into full-price tokens and raise the cost.

So every decision is made once and remembered for the conversation: the first time an
item is seen it is rewritten (or left alone), and on every later turn the same item
gets the same bytes. The remembered decision is looked up before the settings are read,
so switching a rule or the master switch changes only items seen afterwards. A pointer
or a re-run sent whole depends only on earlier items of the same request, which never
change. Context is inserted at the same place with the same text, and a typed message
first seen while no graph is ready gets no pack, for good. Tests (`the cached prefix must
not change`, `turning_shortening_off_mid_conversation_keeps_past_items`) hold this, and
the cache monitor checks it on every request.

The decisions are kept in memory, and each conversation's record keeps a ledger of
them: for each item, the first 12 hex characters of its hash and the rules whose
result was sent (for a typed message, whether a pack was added). After the daemon
restarts, an item the ledger names is decided again with exactly those rules, whatever
the settings say by then, and is not counted again. The result is the same unless the
files changed in between (a pack or a skeleton is built from them); then that
conversation pays one uncached turn.

## Failing open

If the body cannot be read, parsed or rewritten, for any reason, the original bytes are
forwarded with the original headers. The response is relayed as it arrives; the proxy
only reads the token usage out of the stream. If the provider cannot be reached, the
client gets a 502 with a message that names the proxy.

## What is stored

In the usage log (`usage.lbug` in the data directory), one record per conversation:

- token counts (input, cached, output, reasoning), the model name, the first typed
  message cut to 200 characters, the router's recommendation, the tokens of lge's packs,
  timestamps, and the arm (`proxy` or `proxy-holdout`);
- per-rule counts: items, tokens before and after, the same in shadow, judged and
  recovered items, the skeleton cohorts (language and size band), and how many outputs
  arrived condensed by RTK;
- monitor counts: cache breaks and the item index of the first one lge caused, retries,
  cold restarts and their tokens, packs a held-out conversation would have added, model
  requests, and the decision ledger above (hash prefixes and rule ids, no content);
- two token totals: tokens not sent and tokens added, summed over every request.

In `daemon.json`: the folders and the settings below, including demotions and their
dates; `daemon.json.unreadable` only when the file could not be read. In `serve.log`: one
line per model request without any content (sizes, status, usage), and lines for cache
breaks lge caused, retries (status, error type and code) and demotions.

Kept in memory only, never written: the request traces (below), the item hashes of the
cache monitor, the tool list of the fixed-prefix report, and the results of replaying
past sessions. "Replay past sessions" on the page and `lge usage discover` read
`~/.codex` and store nothing; `lge usage discover --retain`, only when asked, stores each
command correction seen at least twice as a convention fact in the repository's graph
(``Use `right` instead of `wrong` (class; seen N times).``, commands cut to 200
characters, from sessions that worked inside the repository).

Nothing else of a conversation is written to disk. Credentials are passed through and
never stored or logged. `lge ab` writes its own files in its work dir; see `docs/AB.md`.

## The page's API

| Endpoint | What it does |
|---|---|
| `GET /lge/api/status` | Settings, folders, sessions, rules with their counts and rates, cohorts, holdout comparison, cache monitor, fixed prefix, unused tools |
| `POST /lge/api/settings` | Changes only the keys the JSON body names. `clear_demotion: "<rule>"` re-enables a demoted rule; `demoted` and `rule_since` in a body are ignored; an unknown rule id is ignored, an unknown mode is an error. Saving clears an unreadable-file warning |
| `POST /lge/api/discover` | Replays the Codex sessions of the last 30 days through every rule, in memory |
| `GET /lge/api/sessions/<thread-id>/requests` | The traces of one conversation, oldest first: per request its status, whether it was retried, the usage, the tokens lge added and removed, and its three largest new tool outputs (command cut to 120 characters, tokens, whether lge shortened it; no output text). Keyed by Codex's `thread-id` header, else the `prompt_cache_key`; the last 500 requests of the last 200 conversations, in memory |
| `POST /lge/api/import-history` | Reads the token counts of past Codex sessions |
| `GET /lge/api/browse` | Folders under a path, for the folder picker |
| `POST`/`DELETE /lge/api/repos` | Adds or removes a folder |

A request whose `Host` is not a loopback name or the configured host is refused, and so
is a non-GET request from another origin or a POST that is not `application/json`.

## Run it

Directly:

    lge serve                       # 127.0.0.1:8787, data in ~/.lge

In Docker (the same layout as a Headroom container):

    export LGE_WORKSPACE=$HOME/code # the folder that holds your repositories
    docker compose up -d --build
    open http://127.0.0.1:8787

The workspace is mounted read-only at the same path it has on the host, so the paths
the agent reports are the paths the daemon reads. `~/.codex` is mounted read-only for
the "import past sessions" and "replay past sessions" buttons. Graphs and the usage
log live in the `lge-data` volume.

## Connect Codex

Add one line to `~/.codex/config.toml`. The CLI and the desktop app read the same file,
and your ChatGPT or API-key login stays as it is:

```toml
openai_base_url = "http://127.0.0.1:8787/v1"
```

Remove the line to stop using the proxy.

How this works with Codex 0.160 (from its source):

- Codex first tries a WebSocket to the provider. The proxy answers `426`, and Codex uses
  HTTP streaming for the rest of the session. A plain HTTP request is what the proxy can
  read and rewrite.
- With a ChatGPT login Codex sends `chatgpt-account-id`; the proxy forwards those requests
  to `https://chatgpt.com/backend-api/codex`, and API-key requests to
  `https://api.openai.com/v1`. Credentials pass through and are never stored.
- With a ChatGPT login Codex compresses request bodies with zstd; the proxy decodes them
  and forwards plain JSON.
- If you set `HTTP_PROXY` or `ALL_PROXY` in your shell, add `127.0.0.1` to `NO_PROXY`, or
  Codex sends the local requests to that proxy.
- If the daemon is stopped while the line is set, Codex keeps retrying. Start the daemon or
  remove the line.

Optional, for the graph tools:

    codex mcp add local_graph --url http://127.0.0.1:8787/mcp

## Settings

On the page, or `daemon.json` in the data directory. Every key may be missing: it then
takes its default, so a file written by v0.0.1 still loads.

| Key | On the page | Default | Meaning |
|---|---|---|---|
| `inject_context` | Add graph context | `true` | The evidence pack after a typed message about the code |
| `shorten_outputs` | Apply the rules to tool output | `true` | The master switch of every rule; off, new items get no rule, not even in shadow |
| `rules` | The rules table | `check`, `search_cap`, `repeat_pointer`, `inert`, `path_fold`, `json_minify`: `"on"`; `skeleton`, `line_clip`, `noise`, `lockfile`: `"shadow"`; `rerun_delta`, `read_delta`: `"shadow"` (measure only) | The mode of each rule: `"off"`, `"shadow"` or `"on"` |
| `shorten_reads` | - | `false` | The older name of the skeleton switch: always `rules.skeleton == "on"`. In a file without `rules.skeleton`, `true` means `on` and `false` means `shadow` |
| `context_budget` | - | `1500` | Tokens of one pack; a message about one file that nothing else uses gets at most 600 |
| `auto_index` | Index a git repository the first time | `true` | A git repository inside the workspace is indexed the first time the agent works in it |
| `holdout_share` | Holdout | `0` | The share of new conversations held out, from 0 to 1; any other value is 0 |
| `demoted` | The rule's status | `{}` | Rules the recovery check moved to shadow, with the reason. Written by the daemon |
| `rule_since` | - | `{}` | Per rule, the time from which its recovery rate counts. Set by "Re-enable" |

## What is not verified

- The proxy is tested against a stand-in provider over real sockets, and with real
  Codex 0.160 sessions for the v0.0.1 behaviour. The several-command code-mode layout,
  the JSON exec lines, the place of the base instructions and the MCP tool names are
  built from Codex's source; they need a real Codex 0.160 capture.
- No saving has been measured. The page shows real token counts per session and what
  each rule removed; the holdout arm and `lge ab` are how a difference is measured.
- The rules in shadow (`skeleton`, `line_clip`, `noise`, `lockfile`) are measured, not
  sent, until their counts and recovery rates have been reviewed.
- The terse-replies skill and the batch line of `lge init` are unmeasured until
  `lge ab --variant terse` and `--variant batch` measure them.
