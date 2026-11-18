# Changelog

## v0.0.5 (2026-11-18)

The first full day of real use showed page numbers that mixed versions and kinds of
session, Codex's background requests indexing Codex's own folders, follow-up packs that
could almost never be built, and gaps in the opt-in effort lowering of v0.0.4. This
release fixes them. lge still never changes the model, the system prompt, tool
definitions, reasoning items, tool calls, patches or diffs, never raises the reasoning
effort (and lowers it only with `router_apply` on), and never edits Codex's
configuration. No saving is measured yet.

### The page's numbers

- **Background sessions of older versions.** A stored session without a background label
  whose first typed message (as stored, 200 characters) opens with one of Codex's own
  prompts (the approval reviewer's, the desktop app's title prompt, a memory
  consolidation's), or whose model is `codex-auto-review` while it counted no background
  request, is counted as a background session: out of the cards, the holdout comparison,
  "Before and after" and the long-session note, and in the background table with its
  stored requests and input. The page works this out each time it reads the records and
  writes nothing; the session's next request, if any, stores it so when its first message
  tells it. Before, such sessions stayed ordinary ones for good: lge reclassified a
  session only on a new request, and only when it never had a pack.
- **Unknown request counts.** A session recorded before v0.0.2 stored no request count.
  The table shows "–" for its requests and input per request instead of 0, and the
  holdout comparison leaves it out instead of counting 0 requests, which made the
  median 0.
- **The holdout comparison compares like with like.** Only sessions placed in an arm at
  random are compared: those that started while the holdout share was above 0 (stored as
  `randomized`). Sessions that started while it was 0, or before this version, are left
  out, and the page says how many and why. It also says that the arms differ in
  everything lge changes (rules, packs and, with `router_apply` on, the effort).
- **`lge ab` runs apart.** `lge ab` names its arm in the daemon's settings (`ab_arm`,
  never read back from `daemon.json`), and the conversations started while it is set keep
  it. They are listed in the session table ("lge ab arm on" or "off") and left out of the
  cards, the long-session note and the proxy's row of "Before and after", which counts
  them on a row of their own. Before, the sessions of both arms, the one with lge switched
  off included, counted as sessions through the proxy. Runs recorded before this version
  cannot be told apart.
- **The packs line.** "Not added for the conversation budget" is split in two: no room
  left in the budget (`over_budget`) and larger than a follow-up may be (`too_large`,
  new). "Retired" says what retires a pack: a rewrite of the history (a compaction, an
  edit, a rollback). Packs added are also counted from each session's ledger, so the
  packs of v0.0.2 sessions, which kept no counts, are counted. The token total says since
  when it counts, how much of it is in sessions recorded before v0.0.2 whose packs no
  count covers, and how much in Codex's background sessions.
- **Compactions.** The background table counts the input tokens of each request by label
  (new column "Input tokens of these requests"). The `compaction` row shows "–" for
  sessions instead of 0, since its requests belong to their conversation, and its input
  tokens: each such request re-sends the whole history. Before, it read as costing
  nothing.
- **The long-session note** lists the latest session first, says when each one's last
  request was recorded, and leaves out the sessions of an `lge ab` run.
- The "Sessions" card says what it covers.

### Folders and Codex's background requests

- Codex's background requests never add a folder to the graph (indexing on first use),
  never refresh a folder's index or commit history, and never change the folder the graph
  tools answer for when a call names none. Before, a memory consolidation, which runs in
  Codex's memory folder (a git repository), added that folder, and the graph tools then
  answered for it.
- A git folder Codex keeps for itself is never indexed, even when added on the page: one
  whose `.git` holds Codex's `codex-baseline-version` marker, or `<CODEX_HOME>/memories`
  and `memories_v2`. One saved by an earlier version is dropped from the list when the
  daemon starts, with a `left out` line in `serve.log`.
- The folder a request works in is read only from Codex's environment context, never from
  a `<cwd>` quoted in a typed message or pasted output.
- The page's "Indexing <folder> …" note under the folder list becomes "<name> is
  indexed: N files, M symbols." (or says why it failed) once the folder is done. Before,
  it stayed.
- The warning about folders that searches would read names `.ignore`, not `.gitignore`,
  for a folder inside a git repository nested below the folder you added: `rg` reads
  `.gitignore` only up to the nearest repository root.

### Packs

- **A follow-up can be built.** A follow-up pack may be 600 tokens, often less than a
  pack with 4 starting points needs. A pack too large with 4 is now built with 2, then
  with 1, and kept when it still starts in a file nothing before the message named.
  Before, most follow-ups in a real repository were dropped and counted as over budget.
- **A pack no longer repeats the message.** It opens with "# Context for the message
  above", since it comes right after the message. Before, it repeated the whole typed
  message, which took room from the pack and was re-sent on every later request.
  Conversations started before this version keep their packs as they were.
- **The same bytes after a restart, whatever changed.** The ledger keeps the text of each
  pack still inserted, so a restarted daemon sends it as it was, even after a commit, an
  edit or a new remembered fact. Before, the pack was built again from the current graph
  and could differ: a cache miss caused by lge that the cache monitor could not see. A
  pack of an older ledger is built again once, and its text is kept from then on.
- **A restart while a folder is indexed again.** When a pack decided before the restart
  has to be built again and its folder is still being indexed, the conversation's first
  request after the restart waits for it, up to 10 seconds, once. A request sent without
  the graph leaves the pack out without deciding it again, so it comes back once the
  graph is ready. Before, it stayed out until the next restart.
- **The pack limit survives a restart.** Every pack a conversation added before a
  restart, retired ones included, counts toward its 6, and in a held-out conversation the
  packs it would have added still make the next one a follow-up.

### Lowering the reasoning effort (`router_apply`, off by default)

- Applied only when lge sees a conversation from its genuine first request, one that
  carries no earlier turn (no reasoning, tool call or output, assistant message,
  compaction, second typed message or `previous_response_id`). A conversation lge first
  meets midway (resumed, forked, started before lge was connected) keeps your effort, and
  its reason says so.
- The router reads the first 4,000 characters of the first typed message, not 200, so a
  task whose wider scope is named after its first sentences is routed by all of it.
- The decision is stored at once, before the answer: a restart before the first usage
  keeps it.
- The request for a compaction's summary gets the same effort as every other request of
  its conversation.
- When the provider refuses (400, 413, 422) any request sent at the lowered effort, not
  only the first, it is sent again at your effort, and the conversation keeps your effort
  from then on. Before, a later refusal, for example after a switch to a model that does
  not take that effort, made every later request of the conversation fail.
- The page's setting shows the saved value again when a change could not be saved.
- The docs no longer say that the holdout arm measures the effort: the holdout arm and
  `lge ab` measure all of lge's changes together (rules, packs and the effort), and
  `lge ab` keeps `router_apply` as it is in both arms.

### What is stored

- The usage record also keeps the text of each pack lge added that is still inserted
  (lge's own text built from the graph; in a conversation started before this version it
  opens with the typed message), a pack's starting points when fewer than 4, whether the
  session was placed in an arm at random, its `lge ab` arm, the input tokens of background
  requests by label, and the follow-ups not added as too large. Records of earlier
  versions still load.

### Not verified

- Codex's prompts for the approval reviewer, task titles and memory consolidation are
  matched by their opening words, which Codex may change. What makes a request a
  conversation's genuine first one is built from Codex's source; neither is checked
  against a real Codex capture yet.
- Records written before this version count the follow-ups that were too large under "no
  room".
- A record of a version before v0.0.3 counted no request sent to the approval reviewer,
  so a session someone worked in whose last request went to the reviewer is counted as
  the reviewer's until its next request.
- The long-session note still reads only the 50 newest sessions of the table.
- Other git folders Codex keeps under its home (such as plugin clones) are not
  recognised; they are indexed only when a conversation someone works in runs there.
- What the smaller follow-ups, the shorter pack heading and the lowered effort save is
  unmeasured. The holdout comparison needs a holdout share above 0, and 30 sessions in
  each arm from then on.
- The page's changes are tested against its script and the status, not in a browser, and
  neither the effort lowering of v0.0.4 nor these fixes have run against a real Codex yet.

## v0.0.4 (2026-11-18)

The router's recommendation can now lower the reasoning effort, if you switch it on.
Off by default: lge still never changes the model, the system prompt, tool definitions,
reasoning items, tool calls, patches or diffs, and never edits Codex's configuration.
No saving or quality effect is measured yet.

### The router can lower the reasoning effort (opt-in)

- New setting `router_apply` (page: "Lower the reasoning effort when the router
  recommends it"; `daemon.json`): `off` (default), `low-only` or `low-and-medium`.
  `low-only` sends a conversation at `low` effort when the change impact of its first
  typed message stays in one file; `low-and-medium` also sends `medium` when it stays in
  one service.
- Only ever lowers `reasoning.effort`. A request already at or below the target, one
  without an effort, or one with an effort this version does not know is left alone.
- Decided once, on the conversation's first request, and kept for every request of the
  conversation: a later change of the setting or a daemon restart does not change it,
  so the prompt cache is never broken by it. A restored conversation is not routed
  again.
- Held-out conversations, Codex's background requests and background sessions are never
  changed. The holdout arm and `lge ab` measure the effort together with the rest of
  lge's changes, not on its own.
- When the provider refuses the first request (400, 413, 422), it is sent again with the
  client's effort, and the conversation keeps it.
- The page marks applied sessions ("router: low (applied)"), the status has
  `applied_level` per session, the usage record's `level` is the applied level, and
  `serve.log` has a `router applied` line.

### Tests

- The A/B preflight test accepts dash's wording (`not found`) for a missing command as
  well as bash's (`command not found`), so it passes where `/bin/sh` is dash.
- The test of a copy folder that cannot be removed is skipped when the user can delete
  read-only folders (root, as in many CI containers).

### Not verified

- What lowering the effort saves, and what it costs in task success, is unmeasured. Run
  `lge ab` or a holdout share with `router_apply` on before relying on it; both measure
  all of lge's changes together, the effort included.
- Whether every model accepts `low` and `medium` is not checked; a refusal falls back to
  the client's effort for that conversation.

## v0.0.3 (2026-11-17)

Packs cost less in long conversations, and Codex's background requests no longer get
them. lge still never changes the model, the reasoning level, the system prompt, tool
definitions, reasoning items, tool calls, patches or diffs, and never edits Codex's
configuration. No saving is measured yet: the holdout arm and `lge ab` measure it.

### Packs in long conversations

A pack is re-sent on every later request, so its cost grows with the conversation. For a
typed message seen for the first time:

- **One budget per request.** The packs present in one request are at most
  `pack_conversation_budget` tokens together (new setting, 3,000 by default, on the page
  and in `daemon.json`), counted over the whole pack, frame and remembered facts
  included. A new pack is built to fit in the room left, remembered facts dropped first;
  with less than 300 tokens of room it is not added.
- **Small follow-ups.** After a conversation's first pack, a pack is at most 600 tokens.
- **Only something new.** A follow-up is added only when one of its starting points is in
  a file that no tool call, tool output or pack before the message in the request named
  (its whole repository-relative path). A message whose starting points are all in packs
  already present gets none, and so does a copy of a message typed earlier in the same
  request.
- **Retired after a compaction.** When the client changes its history at or before a
  message (a compaction, an edit, a rollback), that message's pack is no longer inserted,
  unless it is the newest typed message's and still fits the budget. A retired pack stays
  out for good. Items before the client's change keep every byte, so an edit or rollback
  of the last turn keeps the packs of earlier messages; a compaction, which rewrites the
  history from its first item, retires every older pack. The cache monitor counts it as
  the client's break.
- **Same bytes after a restart.** The ledger keeps each pack's size, which packs were
  retired, and a hash of the client's items up to each message (hashes only), so a
  restarted daemon rebuilds the same packs and retires the same ones, a compaction made
  while it was down included. A ledger from v0.0.2 still loads; its conversations keep
  their packs as before.
- Codex's compaction summary is no longer taken for a message the user typed.

### Codex's background requests

- Requests of the approval reviewer, `/review`, task titles and recaps, and memory
  consolidation are told apart by the `x-openai-subagent` header or `client_metadata`
  (any sub-agent except a spawned agent, `collab_spawn`), the turn metadata's
  `thread_source`, the `codex-auto-review` model, or the desktop app's title prompt.
- They get no new pack. Their outputs are shortened as before, their usage is recorded
  under their label, and their `serve.log` line names it.
- A conversation that starts with one is a background session (`proxy-background`): never
  held out, no pack, no router call, tagged on the page, and left out of the holdout
  comparison, "Before and after" and the long-session note. The page counts background
  sessions and requests by label.
- "Import past Codex sessions" leaves out Codex's background threads, read from each
  rollout's `session_meta`, and says how many.
- The request for a compaction's summary (turn metadata `request_kind` `compaction`) gets
  no new pack and is counted under `compaction`; its conversation stays what it was.

### Counters and long sessions

- The page and the status count packs added, not added as already known, not added for
  the budget, and retired, per session and over the sessions not held out (what
  held-out sessions would have added is shown apart). The counts survive a restart.
- Each session shows its mean input tokens per request. A note names the sessions of 50
  or more requests over 150,000 input tokens per request and says what makes requests
  smaller: a new session per task, or a lower `model_auto_compact_token_limit` in
  Codex's config, at the cost of Codex working from a summary. A report only; the
  choice is yours.

### Fixes

- The page's folder picker starts at the home folder when nothing is typed (and no
  workspace limit is set), accepts `~/`, and shows the nearest existing folder with a
  note when the typed one was deleted or moved. Before, it showed an empty list.
- A folder added to lge and later deleted or moved shows as `missing` with a hint to
  remove it, instead of `ready` with its old counts.
- The warning about folders that searches would read names `.ignore` instead of
  `.gitignore` when the folder is not a git repository: `rg` does not read `.gitignore`
  there, so the old advice quieted the warning without changing any search.

### Not verified

- Background requests and the compacted history are recognised as Codex's source builds
  them (codex-rs), and the title request by the desktop app's prompt, whose wording may
  change; none of it is checked against a real Codex capture yet.
- What the pack budget, the follow-up rules and the retirement save is unmeasured until
  the holdout arm or `lge ab` measures it.

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
