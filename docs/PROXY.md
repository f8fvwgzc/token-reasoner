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
  what each rule did, the packs, Codex's background requests, the cache monitor, the
  fixed prefix, the holdout comparison and the folders in the graph.
  `lge report` prints the same page as plain text (`--redact` to share it, `--json` for
  the status JSON).

The model is whatever the agent asked for. So is the reasoning level, unless
`router_apply` is switched on (off by default; see "Lowering the reasoning effort").

## Lowering the reasoning effort

`router_apply` lets the router's recommendation change `reasoning.effort` in the request:

| Value | What is applied |
|---|---|
| `off` (default) | Nothing. The recommendation is logged on the page and in the usage record |
| `low-only` | `low` when the task's change impact stays in one file |
| `low-and-medium` | Also `medium` when it stays in one service |

- The effort is only ever lowered: a request already at the target or below it, one that
  names no effort, or one with an effort this version does not know is left alone, and
  the conversation is marked as not applied.
- It is decided once, on the conversation's genuine first request, from the router's
  result for the first typed message (its first 4,000 characters of what the user asked: see "What the user asked" below), and needs the graph
  ready at that moment. A genuine first request carries no earlier turn: no reasoning,
  tool call or tool output, assistant message, compaction item or item reference, no
  second typed message, no compaction summary or turn-aborted notice, no
  `previous_response_id`, and it is not the request for a compaction's summary. A
  conversation lge first meets midway (a resumed or forked thread, one started before lge
  was connected, a lost record) keeps the client's effort for good; its reason says "not
  applied: the conversation was already under way when lge first saw it".
- The decision is stored at once, before the answer, and every later request of the
  conversation, the request for a compaction's summary included, gets the same effort,
  even if the setting changes or the daemon restarts, so the prompt cache is never broken
  by it.
- Held-out conversations and Codex's background requests (and background sessions) are
  never changed. The holdout arm and `lge ab` measure all of lge's changes together
  (rules, packs and, with `router_apply` on, the effort); neither isolates the effort.
- If the provider refuses a request sent at the lowered effort (400, 413, 422), the first
  one or a later one (for example after the thread moved to a model that does not take
  that effort), it is sent again with the client's effort, and the conversation keeps the
  client's effort from then on, stored at once. That costs one uncached request.
- The page marks an applied session with "router: low (applied)", and `serve.log` has a
  `router applied` line with the effort before and after. The usage record's `level` is
  the applied level (`top` = the client's effort kept).

There is no automatic retry at a higher effort: the proxy cannot tell whether a task
succeeded. `lge run` in routed mode does retry after failed local checks.

## What it changes in a request

Only two kinds of item in the conversation, and nothing else in the body:

- **a tool output**, which a rule may replace (below);
- **a message the user typed**, after which one context item may be added: the evidence
  pack for that message, in `<lge_context>` tags, when it names a symbol or clearly
  matches one (a search hit that shares at least two words with it). Small talk gets
  nothing, and Codex's background requests get no new pack (see "Codex's background
  requests"). When a pack is added, how large it is and how long it stays: "Packs: when
  they are added and how long they stay".

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
| `read_delta` | What a diff against an earlier whole read covering the same lines would save: a read inside an earlier one is compared with its own lines of it, so a narrower re-read after an edit counts. Measured only, never sent | measured | none (never sent) | - |

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

## Packs: when they are added and how long they stay

A pack is one context item, a user message in `<lge_context>` tags placed right after the
typed message it is for: what the code graph says about the message (its conclusions
first, then the places to start from) and, when they fit, up to 300 tokens of remembered
facts (decisions, conventions, recent commits). Codex re-sends it on every later request,
so a pack costs its tokens once per request after it, and a long conversation pays for
its packs hundreds of times. The rules below bound that cost.

The graph is that of the folder in lge that holds the request's working folder: the
`<cwd>` of Codex's environment context, read only from a message part that opens with
`<environment_context>`. A `<cwd>` quoted in a typed message or in pasted output never
decides it.

### When a pack is added

Decided once, the first time a typed message is seen in the conversation. A pack is added
only when all of these hold (in brackets, what the page counts when that check fails):

- "Add graph context" (`inject_context`) is on, and the request is neither one of Codex's
  background requests nor the request for a compaction's summary (see "Codex's background
  requests");
- a graph of the repository Codex works in is ready; a message first seen while none is
  gets no pack, for good;
- the message is at most 8,000 bytes, and the conversation has had fewer than 6 packs;
- the message names a symbol or clearly matches one (a search hit that shares at least
  two words with it);
- it is not a copy of a message typed earlier in the same request;
- the packs present before it in the request do not already give all its starting points
  ("already known");
- those packs leave at least 300 tokens of `pack_conversation_budget` (3,000 by default)
  ("no room", `over_budget`);
- for a follow-up (any pack after the conversation's first), one of its starting points
  is in a file that nothing before the message in the request named: no tool call's
  arguments, no tool output as it was sent, no pack. A file counts as named when its
  whole path relative to the repository root appears, not as part of a longer name
  ("already known");
- the pack fits in its size, at least without remembered facts and, when it does not with
  its 4 starting points, with 2 or with 1 of them; a follow-up built with fewer must
  still start in a file nothing before the message named ("no room" when the room left
  in the budget was the limit, "too large", `too_large`, when a follow-up's 600 tokens
  was).

Its size: the code-graph part is built to at most `context_budget` (1,500 by default), or
600 when its starting points are all in one file that nothing outside it uses. The frame
(the `<lge_context>` tags and their opening lines) and up to 300 tokens of remembered
facts come on top. The whole item is at most the room the packs before it leave in
`pack_conversation_budget` and, for a follow-up, at most 600. Remembered facts are left
out first when they do not fit. A pack opens with "# Context for the message above": the
message is the item right before it and is not repeated (a conversation of an older
version keeps its packs opening with the message, `# Context for: <message>`).

In a held-out conversation a pack is decided the same way and counted as one it would
have added, and never added; such packs take no room in the budget.

### How long a pack stays

A pack is sent again, with the same bytes and at the same place, on every later request
while the history keeps its prefix up to its message, whatever the settings say by then.
It is retired, no longer inserted for the rest of the conversation, when

- the client changed an item at or before its message since the previous request (the
  cache monitor's client break: a compaction, an edit, a rollback), or
- after a restart, the hash of the client's items up to its message is not the one the
  pack was last inserted with (the history changed while the daemon was down),

unless its message is the newest typed message of the request and the pack still fits in
`pack_conversation_budget` with the packs before it.

Codex's compacted history (codex-rs/core/src/compact.rs: the user messages it keeps, the
initial context before the last of them, its summary) differs from its first item on, so
a compaction retires every older pack. An edit of the last message, a rollback of the
last turns or a rewritten late output changes the history only from there: the packs of
earlier messages keep being sent. Codex's compaction summary (it opens with "Another
language model started to solve this problem") is not a typed message: it gets no pack
and is never the newest message.

A retired pack, or one whose message left the history, no longer counts as giving its
starting points, so a later message about the same code may get a pack again.

### Why this keeps the prompt cache

- A message's pack is decided once, from the items before it in the request (the packs
  present, the room they leave, the files named) and the conversation's packs so far,
  then stored and replayed before the settings are read: a changed
  `pack_conversation_budget`, `context_budget` or `inject_context` applies only to
  messages seen afterwards.
- A pack is retired only at or after the item where the client itself changed its
  history. The provider's cache cannot match from there on anyway, so leaving the pack
  out costs no cached token, and every item before it, packs included, keeps its bytes.
  The newest message's pack, which may stay, is at or after that item too. The cache
  monitor counts such a request as the client's break; any change lge made before the
  client's break is counted as lge's (a bug).
- After a restart, the ledger (see "Why the prompt cache keeps working") gives each pack
  that is still inserted its exact text, the packs retired, and a hash of the client's
  items up to each message, so the daemon sends the same packs and retires the same ones
  as if it had kept running, whatever changed in the graph since (a commit, an edit, a
  new fact). A pack of an older version's ledger, which keeps no text, is built again
  from the graph to the size and starting points it had, and its text is kept from then
  on. If that graph is still being indexed, the conversation's first request after the
  restart waits for it (up to 10 seconds, once), a background request too (a session an
  older version recorded as an ordinary one had packs; waiting adds no folder); a request
  sent without it leaves the pack out without deciding it again, so the pack comes back
  once the graph is ready.
- Every pack the conversation added before a restart, retired ones included, counts
  toward its 6 packs, and the packs a held-out conversation would have added count the
  same way and make its next one a follow-up.

The page and the status count, over the sessions not held out and per session, the packs
added (also from the ledger, so a session of v0.0.2, which had no counts, is counted),
not added as already known, not added for lack of room (`over_budget`), not added as too
large for a follow-up (`too_large`; records written before this version count those
under `over_budget`), and retired after the history was rewritten (a compaction, an
edit, a rollback), and apart what held-out sessions would have added. The tokens of packs
present in the requests are the session's "Added"; their total over those sessions
(`tokens_sent`, every version) comes with how much of it is in sessions whose packs no
count covers (`tokens_sent_uncounted`: no pack count and no pack in the ledger, recorded
before v0.0.2), how much in Codex's background sessions (`tokens_sent_background`), and
the start of the earliest of them (`since`). A session recorded by v0.0.2 has its packs
counted as added, from its ledger, and never as already known, no room or too large.

## Codex's background requests

Codex makes requests that no one works in: the approval reviewer, `/review`, task titles
and recaps, memory consolidation. lge takes a request for one when

- its `x-openai-subagent` header (or the same key of the body's `client_metadata`) names
  a sub-agent other than `collab_spawn`: Codex sends `review`, `memory_consolidation`,
  and `guardian` for the approval reviewer (an empty value is labelled `subagent`). A
  spawned agent, `collab_spawn`, does real work and is a conversation like any other;
- the `thread_source` of its turn metadata (`client_metadata["x-codex-turn-metadata"]`
  or the `x-codex-turn-metadata` header, JSON) is `thread_title` (the TUI's task title,
  label `title`), `system` (the TUI's recap, label `recap`), `guardian_review` (label
  `guardian`) or `memory_consolidation`;
- its model is `codex-auto-review` (any case), the reviewer's default model (label
  `guardian`); or
- its instructions, or one of its messages up to and including the first one the user
  typed, open with one of Codex's own prompts: the desktop app's title prompt ("You are a
  helpful assistant. You will be presented with a user prompt, and your job is to provide
  a short title for a task ...", label `title`), the approval reviewer's ("The following
  is the Codex agent history whose request action you are assessing.", label `guardian`)
  or a memory consolidation's ("Consolidate the supplied rollout summaries into
  `memory_summary.md`", label `memory_consolidation`).

Such a request gets no new pack, and so no remembered facts; in a conversation that had
packs before, those are sent again. The rules for tool output apply as usual, its usage
is recorded, it is counted under its label with its input tokens, and its `serve.log`
line ends with `(background: <label>)`. It never adds a folder to the graph, never
refreshes a folder's index or history, and never changes the folder the graph tools
answer for; the graph of a folder already added is read as for any request.

A conversation whose first request is a background one is a background session (record
mode `proxy-background`, with its label): it is never held out, none of its requests gets
a pack, the router's recommendation is not worked out for it, and it is left out of the
holdout comparison, of "Before and after" and of the long-session note. The page tags it
in the session table and counts background sessions, requests and the input tokens of
those requests by label. A stored session with no label (one recorded by a version that
did not tell background requests apart) is a background session when its first typed
message as stored (200 characters) opens with one of Codex's prompts above, or its model
is `codex-auto-review` and it counted no background request by label. The model stored is
the last request's: from v0.0.3 on, a session someone works in counts the requests it
sent to the reviewer and is not taken for one, but a record of an earlier version counted
none, so such a session whose last request went to the reviewer is counted as the
reviewer's. The page counts these sessions so at once, their stored requests and input
under their label, without writing anything. On its next request, if any, a session
told by its first message is stored so, packs or not (packs decided before are sent
again), and its stored requests and input stay counted under its label. One told only by
its model, or only by the headers of a later request, is stored so only when that
request is a background one and the session never had a pack. Otherwise it keeps its arm
and the page counts it as an ordinary session from then on: its model is now that
request's, or that request was counted by label.

"Import past Codex sessions" leaves the same threads out of "Before and after": a rollout
whose first `session_meta` names such a `thread_source`, a sub-agent source other than
`thread_spawn` (`review`, `compact`, `memory_consolidation`, `guardian`, ...), or an
internal source. It reads `rollout-*.jsonl` and the `rollout-*.jsonl.zst` files Codex keeps
older sessions in (decompressed in memory), and reports how many it left out and how many
it could not read (a damaged file, a `.zst` that does not decompress).

Codex's compactions are not background requests: codex-rs maps a `compact` sub-agent, but
no request sends one. Codex asks for a compaction's summary within the conversation
itself, with `request_kind` `compaction` in the turn metadata. That request gets no new
pack (packs decided before are sent again) and is counted under the label `compaction`
with its input tokens: it re-sends the whole history, though it never forms a session of
its own (the page shows "–" for its sessions). The conversation stays what it was, and an
effort `router_apply` lowered holds for it as for every request of the conversation.

## What the user asked

Codex and its IDE integration put context in front of what you type: the open files and
the selection under `# Context from my IDE setup:`, referenced chats, and (in the desktop
app) mentioned files. The part you typed follows the line `## My request for Codex:`.
When a typed message starts with a `#` heading and has such a line, lge reads what comes
after the first one for the task shown on the page, for the router, and for a new
decision about a pack (starting points, the size check, the words of the conversation). A
message that does not start with a `#` heading, or has no such line, is read whole, as
before; a message with nothing after the line (attachments only) is not the task, and the
next typed message is. The pack is still named in the ledger by the whole message, so a
pack decided from the whole message before this version is sent again as it was, and a
conversation with no cache key and no thread header keeps its key (from the first 200
characters of the whole message). The layout of the desktop app's file preamble is not
public: until one is known, a message without the line is read whole.

## Long sessions

The page shows each session's requests and its input tokens per request (its input
tokens over its requests); a session recorded before v0.0.2 stored no request count and
shows "–" for both. The table lists the newest 50 sessions that are not Codex's background
ones and the newest 10 background ones, so hundreds of approval reviews cannot push a long
session out of it. Under it, a note names every stored session (not only those of the
table), background ones and those of an `lge ab` run left out, with at least 50 requests and more
than 150,000 input tokens per request on average, the latest first, each with when its
last request was recorded: every request of such a session re-sends that many tokens. The
status gives the same list (`long_sessions`, with `started_at` and `finished_at`).

It is a report only. lge never edits Codex's configuration; what to do is your choice.
Two things make each request smaller:

- a new session per task;
- a lower `model_auto_compact_token_limit` in `~/.codex/config.toml`. Codex compacts a
  conversation when its context reaches this limit (when it is not set, the model's own
  limit), at most 90% of the model's context window, so a lower value compacts sooner and
  keeps later requests smaller. The cost: after a compaction Codex works from its summary
  and the messages it kept, not the whole history, so it may have to read files again;
  each compaction is one more model request over the whole history; and the compacted
  history is new to the provider's cache. At a compaction lge retires the packs of older
  messages (see "How long a pack stays").

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
calls. In shadow a file counts once, as in `on`, where its next read is sent whole: a
re-read of the same file (identical or another window from line 1) adds nothing to the
shadow total or the cohort, so the total says what `on` would save. Only a restart can
count a file again. Nothing promotes a rule by itself: you switch a rule to `on` after reviewing its
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

## Measurements added in v0.0.6 (counts only, nothing is changed)

- **Scripts lge could not read.** An output of a code-mode script that lge cannot split
  per command (a loop over a command array, outputs joined in one `text(...)`, computed
  `cmd` or `workdir`, an `apply_patch` and an `exec` in one script) is judged by its
  content only. The page counts those outputs of 400 tokens or more (smaller ones could
  never be shortened), once each, and shows their tokens and share of tool output ("Scripts
  lge could not read"). Restoring an item from the ledger or a later request counts
  nothing again. The count does not enter any rule's items or "Removed from tool output".
  Nothing is built to read such scripts until this count shows a large share.
- **The approval reviewer (guardian).** The page says its sessions are reviewer threads (one
  per fork or replaced trunk), not approvals, and its requests include resampled denials and
  retries, and shows parents, threads, single-request threads and requests per parent. For
  each guardian request lge also compares, per item (instructions, tools, each input item),
  its start with the previous request of the same parent and sets that against the
  provider's cached tokens: "shareable but not cached" is an upper bound, split by overlap,
  gap over 300 s, same or different cache key, same or new thread, and whether lge changed
  bytes. Memory keeps only hashes and token estimates for the last request of up to 64
  parents, each dropped after an hour. No text is stored and no request changes.
- **Packs.** The packs line also counts typed messages that reached the 6-pack limit of a
  conversation (retired packs count toward it) with those a live-only count would have
  added, and typed messages first seen while their folder had no graph yet (no pack, for
  good). The folder list shows how long its last index took.
- **Graphs on disk.** Each graph folder records the project folder it was built for
  (`root.txt`). The page and `lge graphs` list every graph with its size and whether its
  folder is listed; removing a folder from the list says how big its graph is and where.
  Nothing deletes a graph by itself: `lge graphs --delete <name> --yes` (or the page's
  button) does, after you confirm, and refuses a graph of a listed folder.
- **The background table** shows cached and uncached input per label (a "-" for sessions
  recorded without it), and the compaction row shows "-" instead of 0 when none of its
  requests counted input.
- **The holdout comparison** shows "-" for an arm with no sessions, not medians of 0.
- **The Configuration entry column** of the fixed-prefix table carries advice for the
  groups Codex has a switch for (all sub-agent tools: `[agents] enabled = false`;
  `request_user_input`; an MCP server or the plugin that declares it). Keys were checked
  against Codex 0.160.0 and may differ in other versions; a tool with no known switch has an
  empty cell. lge never writes the file.
- **Task labels.** On the page, a mention link such as `[$name](path)` shows as `$name`.
  Image and audio label lines of an attachment are left out of the text the task and the
  pack are read from. What is stored and sent does not change.
- **Slow status.** A status call over 200 ms logs `status_ms` and the record count in
  `serve.log`, to measure before the page's reads are cached.

## Holdout

`holdout_share` (0 by default, from 0 to 1) holds out a share of new conversations: in
them every rule runs in shadow and packs are decided and counted but not added, so the
client's bytes are forwarded. A conversation is assigned once, when it is first seen,
from the first 16 hex characters of the hash of its `prompt_cache_key` (which Codex
shares between a parent and its sub-agents); a share of 1 holds out every conversation.
The arm is stored with the session (`proxy` or `proxy-holdout`) and kept across restarts.
A background session (see "Codex's background requests") is never held out and is left
out of the comparison.

The page compares the two arms over the sessions of the last 30 days that were placed in
an arm at random: those that started while the holdout share was above 0 (stored as
`randomized`). A session that started while the share was 0, or was stored before lge
recorded this, is left out, and so is one recorded before v0.0.2, which has no request
count; the page says how many were left out and why (`left_out`, `left_out_note`). It
shows per-session medians of input, cached, uncached, output and reasoning tokens, model
requests, and tokens not sent, with the difference (proxy less holdout) and its 95%
bootstrap interval (2,000 resamples, fixed seed). No difference is shown while either arm
has fewer than 30 sessions. A held-out conversation gives up whatever the rules would
have saved in it. The arms differ in everything lge changes (rules, packs and, with
`router_apply` on, the effort), so the difference is that of all of it together.

`lge ab` names its arm in the settings (`ab_arm`, `on` or `off`, never read back from
`daemon.json`, so a restart ends it). A conversation that starts while it is set keeps it:
it is listed in the session table, and left out of the cards (sessions, tokens, tool
output removed and packs added; `saved` in the status), the long-session note and the
proxy's row of "Before and after", which shows it on a row of its own. Runs recorded
before lge tagged them cannot be told apart.

## Cache monitor and fixed prefix

For each conversation lge keeps, in memory, a hash per input item as Codex sent it and as
lge forwarded it. On each request it compares them with the previous request's: the
first item Codex changed is a **client break** (a compaction, an edit of the history);
before it, an item Codex left alone but lge forwarded differently is a **break caused by
lge**, which is a bug: it is logged (`lge: cache break caused by lge at item 12
(conversation 1a2b3c4d)`) and counted. A **cold restart** is a response to a request sent more
than 5 minutes after the previous response of the conversation ended (the time the model
and the tools took is not idle time), after a request of at least 4,096 input tokens,
that read less than half of that input from the cache; the uncached tokens are counted.

The **fixed prefix** is what Codex sends at the start of every request: its base
instructions (the `instructions` field, or the developer message that opens the input, as
current Codex sends them) and its tool definitions (`tools`, and the tools of
`additional_tools` input items). They are counted in tokens once per distinct version,
and the tools are grouped as Codex names them: its own tools as built-in, an MCP server's
tools (`mcp__server__` namespaces, or flat `mcp__server__tool` names) by server. Tools
never called since the daemon started are listed with their tokens and, for an MCP server,
the `[mcp_servers.<name>]` entry of `~/.codex/config.toml` that defines them. A tool
called from inside an `exec` script (code mode: `tools.name(...)`) counts as called, by
the name Codex gives it there (the bare name, `namespace__name`, or the namespace and the
name joined when the namespace ends with `_` or the name starts with `_`); a name in a
comment or a string counts too. This is a
report only: lge never changes tool definitions or your configuration.

The page also shows, per session, the tokens not sent: over every request, the tokens
before less the tokens after of each replaced item present in it, next to the tokens of
lge's own packs present in it, and the net of the two.

## Retry after a rejected request

When the provider answers 400, 413 or 422 to a body lge changed, and the request made new
changes (a replaced output, a pointer or a pack first decided in it) or went out at a
reasoning effort `router_apply` lowered, before anything is relayed, lge:

1. reads at most 64 KB of the error body and logs only its status, error type and code;
2. makes the request's new decisions "send as is" for the rest of the conversation, and
   gives the conversation the client's effort from then on when lge had lowered it;
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
first seen while no graph is ready gets no pack, for good. A pack is left out only at or
after the item where the client itself changed its history, where the cache cannot match
anyway (see "Packs: when they are added and how long they stay"). Tests (`the cached
prefix must not change`, `turning_shortening_off_mid_conversation_keeps_past_items`,
`after_a_compaction_older_packs_are_retired_and_every_other_byte_replays`,
`an_edit_a_rollback_or_a_rewrite_late_in_the_history_keeps_every_byte_before_it`) hold
this, and the cache monitor checks it on every request.

The decisions are kept in memory, and each conversation's record keeps a ledger of
them: for each item, the first 12 hex characters of its hash and the rules whose
result was sent (for a typed message, whether a pack was added, the size it was built
to fit in, its starting points when fewer than 4, the pack's text while it is inserted,
a hash of the client's items up to the message when the pack was last inserted, and
whether it was retired). After the daemon restarts, an item the ledger names is decided
again with exactly those rules, whatever the settings say by then, and is not counted
again; a pack is sent with its stored text. The result is the same unless what an item
is built from changed in between (the file for a skeleton; for a pack of a ledger that
keeps no text, its files, the remembered facts, the indexed commit and
`context_budget`); then that conversation pays one uncached turn. A ledger written by
v0.0.2 still loads: its packs are built again without a size cap, as they were then, a
copy of an earlier message keeps getting its text's pack, and a ledger of v0.0.4 or
older keeps its packs opening with the message.

## Failing open

If the body cannot be read, parsed or rewritten, for any reason, the original bytes are
forwarded with the original headers. The response is relayed as it arrives; the proxy
only reads the token usage out of the stream. If the provider cannot be reached, the
client gets a 502 with a message that names the proxy.

## What is stored

In the usage log (`usage.lbug` in the data directory), one record per conversation:

- token counts (input, cached, output, reasoning), the model name, the first typed
  message cut to 200 characters, the router's recommendation, the tokens of lge's packs,
  timestamps, and the arm (`proxy`, `proxy-holdout`, or `proxy-background` for Codex's
  background requests);
- per-rule counts: items, tokens before and after, the same in shadow, judged and
  recovered items, the skeleton cohorts (language and size band), and how many outputs
  arrived condensed by RTK;
- monitor counts: cache breaks and the item index of the first one lge caused, retries,
  cold restarts and their tokens, packs a held-out conversation would have added, packs
  added, not added as already known, for lack of room or as too large, and retired,
  model requests, the background label, background requests and their input tokens by
  label, whether the session was placed in an arm at random, the `lge ab` arm, and the
  decision ledger above (hash prefixes, rule ids, pack sizes and starting points, for each
  pack a hash of the client's items up to its message, and the text of each pack lge
  added that is still inserted: lge's own text built from the graph, which in a
  conversation of v0.0.4 or older opens with the typed message; no tool output);
- two token totals: tokens not sent and tokens added, summed over every request.

In `daemon.json`: the folders and the settings below, including demotions and their
dates; `daemon.json.unreadable` only when the file could not be read. In `serve.log`: one
line per model request without any content (sizes, status, usage, and the label of a
background request), and lines for cache breaks lge caused, retries (status, error type
and code) and demotions.

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
| `GET /lge/api/status` | Settings, folders, `saved` (what the conversations live in this daemon saved, each change counted once, with `since`, the unix time the daemon started), sessions (the newest 50 ordinary and 10 background ones; with their background label, `lge ab` arm, requests and mean input per request, null when unknown, and pack counts), rules with their counts and rates, cohorts, holdout comparison (with the sessions left out and why), cache monitor, fixed prefix, unused tools, packs (over the sessions not held out, with what held-out ones would have added, and the tokens sent split into those of sessions whose packs no count covers and those of background sessions), background requests by label (with their input tokens), long sessions (with when each ran) |
| `POST /lge/api/settings` | Changes only the keys the JSON body names. `clear_demotion: "<rule>"` re-enables a demoted rule; `demoted` and `rule_since` in a body are ignored; an unknown rule id is ignored, an unknown mode is an error. `ab_arm` (`"on"`, `"off"` or `""`) is set by `lge ab`. Saving clears an unreadable-file warning |
| `POST /lge/api/discover` | Replays the Codex sessions of the last 30 days through every rule, in memory |
| `GET /lge/api/sessions/<thread-id>/requests` | The traces of one conversation, oldest first: per request its status, whether it was retried, the usage, the tokens lge added and removed, and its three largest new tool outputs (command cut to 120 characters, tokens, whether lge shortened it; no output text). Keyed by Codex's `thread-id` header, else the `prompt_cache_key`; the last 500 requests of the last 200 conversations, in memory |
| `POST /lge/api/import-history` | Reads the token counts of past Codex sessions, leaving out Codex's background threads |
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
takes its default, so a file written by v0.0.1 or v0.0.2 still loads.

| Key | On the page | Default | Meaning |
|---|---|---|---|
| `inject_context` | Add graph context | `true` | The evidence pack after a typed message about the code; never for Codex's background requests |
| `shorten_outputs` | Apply the rules to tool output | `true` | The master switch of every rule; off, new items get no rule, not even in shadow |
| `rules` | The rules table | `check`, `search_cap`, `repeat_pointer`, `inert`, `path_fold`, `json_minify`: `"on"`; `skeleton`, `line_clip`, `noise`, `lockfile`: `"shadow"`; `rerun_delta`, `read_delta`: `"shadow"` (measure only) | The mode of each rule: `"off"`, `"shadow"` or `"on"` |
| `shorten_reads` | - | `false` | The older name of the skeleton switch: always `rules.skeleton == "on"`. In a file without `rules.skeleton`, `true` means `on` and `false` means `shadow` |
| `context_budget` | - | `1500` | Tokens of one pack; a message about one file that nothing else uses gets at most 600 |
| `pack_conversation_budget` | Tokens of graph context in one request | `3000` | Tokens of the packs present in one request together, each whole (frame and remembered facts included). A new pack is built to fit in the room the packs before it leave and is not added with less than 300 tokens of room, so a value under 300 adds no pack; a follow-up pack is at most 600. Packs already sent keep their bytes |
| `auto_index` | Index a git repository the first time | `true` | A git repository inside the workspace is indexed the first time the agent works in it in a conversation someone works in: never for one of Codex's background requests, and never a folder Codex keeps for itself (a `.git` with Codex's `codex-baseline-version` marker, or any folder inside `<CODEX_HOME>` such as `memories`, `memories_v2`, `.tmp/plugins` and `vendor_imports/skills`, except `<CODEX_HOME>/worktrees`, where the desktop app checks out your own repositories), which is not indexed even when added on the page and is dropped from the saved list when the daemon starts |
| `holdout_share` | Holdout | `0` | The share of new conversations held out, from 0 to 1; any other value is 0 |
| `router_apply` | Lower the reasoning effort when the router recommends it | `"off"` | `"off"`, `"low-only"` or `"low-and-medium"`; any other value is refused. See "Lowering the reasoning effort" |
| `ab_arm` | - | `""` | The arm of a running `lge ab` (`"on"` or `"off"`); set by `lge ab` and never read from the file. See "Holdout" |
| `demoted` | The rule's status | `{}` | Rules the recovery check moved to shadow, with the reason. Written by the daemon |
| `rule_since` | - | `{}` | Per rule, the time from which its recovery rate counts. Set by "Re-enable" |

## What is not verified

- The proxy is tested against a stand-in provider over real sockets, and with real
  Codex 0.160 sessions for the v0.0.1 behaviour. The several-command code-mode layout,
  the JSON exec lines, the place of the base instructions and the MCP tool names are
  built from Codex's source; they need a real Codex 0.160 capture.
- No saving has been measured. The page shows real token counts per session and what
  each rule removed; the holdout arm and `lge ab` are how a difference is measured, of
  all of lge's changes together.
- The rules in shadow (`skeleton`, `line_clip`, `noise`, `lockfile`) are measured, not
  sent, until their counts and recovery rates have been reviewed.
- The terse-replies skill and the batch line of `lge init` are unmeasured until
  `lge ab --variant terse` and `--variant batch` measure them.
- Background requests are told apart by what Codex's source sends (the sub-agent header
  and `client_metadata`, the turn metadata's `thread_source` and `request_kind`, the
  reviewer's model, a rollout's `session_meta` source) and by the prompts of the desktop
  app's titles, the approval reviewer and memory consolidation, whose wording Codex may
  change between versions; the compacted history that retires packs, and what makes a
  request a conversation's genuine first one, are built as Codex's source builds them.
  None of it is checked against a real Codex capture yet. What the pack budget, the
  follow-up rules and the retirement save is unmeasured until the holdout arm or
  `lge ab` measures it.


## What if Codex compacted sooner? (an estimate)

`lge usage compact-whatif [--days 30] [--limits 100000,150000,200000] [--session <id-prefix>]
[--rework-tokens 0] [--json] [--explain]` and the collapsed block under the long-session note
(`GET /lge/api/compact-whatif`, read only, cached in memory for ten minutes and made again when a
rollout changes, never computed inside the status call) replay the long sessions (at least 50
requests and more than 150,000 input tokens each on average, background threads left out) of
Codex's own session files at other values of `model_auto_compact_token_limit`.

How: the per-request series comes from the rollouts (`token_usage_record` lines, or `token_count`
events in older files; a compaction request is the response named by a `compacted` line's
`compaction_response_id`, never guessed from its neighbours). Codex compacts before the request
that follows a response whose total (input plus output) reaches the limit (at most 90% of the
model's window, 95% of it being the hard cap). The replay keeps each request's output and the
growth of its history as they were, restarts the history at the size the session itself shows
(or a rule gives), and prices uncached input 1, cached 0.1, output 8. It is first run at the
session's own limit and shown next to the measured totals, with its error; every change is
against that run, not against the measured totals. A candidate that would compact every few
requests is marked thrash and shows no saving.

The limit is read from `config.toml` (read only), else inferred from the totals before the
compactions seen (approximate: within about one growth step, possibly low by tokens Codex counts
that the last response did not report), else the default. A session whose own error is over 10%,
whose limit is only assumed or with under 200 requests is marked low confidence. It is exact on
a clean synthetic series and within 5% on a noisy one. Codex would work from a summary after each
compaction, which cannot be modelled: the figure is an estimate and lge never writes your
configuration. `--explain` says what each rollout held (compacted lines, events, usage records).
A session is matched to a stored session by id; when there is no match the page says so.

## What if lge trimmed old tool outputs? (an estimate)

`lge usage trim-whatif [--days 30] [--session <id-prefix>] [--json]` and `GET /lge/api/trim-whatif`
(read only, cached in memory for ten minutes like the compaction what-if) replay the same long
sessions under a "checkpoint trim": once a request's input reaches a threshold T, every tool output
(`function_call_output`, `custom_tool_call_output`) at least S tokens large from a user turn at least
K turns back becomes a 20-token stub, if that removes at least R tokens. The prompt cache keeps the
prefix up to the first changed byte, so the request after each checkpoint pays uncached only from the
first trimmed output to the end of its shortened cached prefix (an output's place is estimated from
the request before it; after a compaction it is unknown and the whole prefix is charged); every
later request saves the trimmed tokens at the cached price; a later call with the same command or file as a trimmed output counts as a recovery
and adds that output uncached. A compaction (or input falling below half) empties the history.
Policies: T 100k/150k, K 2/4, S 500/2000, R 20k. An estimate from a replay, not a measured saving;
quality effects are not measured, and lge does not trim anything today.
