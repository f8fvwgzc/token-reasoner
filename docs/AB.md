# A/B test: lge off against lge on

`lge ab` measures what the proxy saves on your own tasks. Every task runs as one Codex
session with lge's features off and one with them on, each in a fresh copy of the
repository, and the task's own check says whether the work was done right.

```
lge ab tasks.toml [--repo DIR] [--base REV] [--runs N] [--model M] [--daemon http://127.0.0.1:8787]
                  [--sandbox MODE] [--no-approve-for-me] [--no-note] [--work-dir DIR] [--json]
```

How the sessions run:

- **Each session in its own temporary git worktree** of the base commit (`git worktree add
  --detach`), made for that session and removed after its check. The base is `HEAD`
  unless `--base` names another commit, branch or tag; it is resolved to a commit once,
  when the run starts, and its full id is printed at the start and in the report. A branch
  that moves during the run does not change the copies.
- **One session after another**, never two at once: the daemon's settings are switched
  for each one.
- **Nothing touches your checkout**: no file in it is changed, no branch is created or
  moved, nothing is stashed. The copies live in a temporary folder outside the repository;
  the only trace in the repository is git's record of the worktrees, which is pruned as
  each copy is removed.

The target it reports against: **at least 25% lower median total tokens with no drop in
success**.

This spends real tokens: `tasks x runs x 2` Codex sessions.

## Before you start

- `lge serve` is running, and `~/.codex/config.toml` sends Codex to it
  (`openai_base_url = "http://127.0.0.1:8787/v1"`, see the README, section 4). `lge ab`
  checks this line before any session and stops when it is missing, because both arms
  would then be the same; `--no-codex-check` skips the check (for a profile or another
  way of setting the address).
- The repository is a git repository and what you want tested is committed: the copies
  are made from `HEAD` (or `--base`), so uncommitted changes are not in them. To test an
  older commit while you keep working, pass `--base <commit>`: no worktree to make by hand.
- Nothing else uses the daemon while the test runs: its settings are switched for each
  session.
- If you added the graph tools to Codex (`codex mcp add local_graph ...`), the arm with
  lge off can still call them: the proxy's settings do not switch MCP. For a clean
  comparison remove them for the test (`codex mcp remove local_graph`) and add them back
  afterwards; with them left in, the test measures what the proxy adds on top of the tools.

## The task file

```toml
# One [[task]] per task. `check` runs in the copy after the session: exit 0 means the
# task was done right. `setup`, optional, runs in the copy before the session.
# `note`, optional, replaces the note appended to every prompt ("" appends none).

note = "Do not install or sync dependencies; the virtualenv is ready. The check runs after you finish."

[[task]]
name = "validate-order-payload"
prompt = "create_order must reject a payload without items with a 400 and a clear message. Add a test for it."
check = "python -m pytest -q orders_api/tests"

[[task]]
name = "retry-notifier"
prompt = "The notifier drops a message when the queue is unavailable. Retry three times with a short backoff, and test it."
setup = "npm ci --silent --prefix notifier"
check = "npm test --silent --prefix notifier"
```

`name` must be unique, `prompt` is what you would type to Codex, `check` is a shell
command (`sh -c`). A good check fails on the base commit and passes when the task is done.

### The prompt note

Every prompt ends with a note, in a paragraph of its own:

> Do not install, download or sync dependencies and do not run long test suites: the
> task's check runs after you finish.

A session that tries to install packages or sync a lock file inside the sandbox stalls (no
network) or spends its turns on it, and the check runs after it anyway. A top-level `note`
in the task file replaces this text, `note = ""` appends nothing, and `--no-note` appends
nothing whatever the file says. The report says which note was used.

## What one session does

1. `git worktree add --detach` makes a fresh copy of the base commit in a temporary folder
   (`--work-dir` puts the copies elsewhere, outside the repository: a folder inside it is
   refused). With `--repo` pointing at a folder inside a repository, the session starts
   in that folder of the copy.
2. The daemon is switched to the arm's settings through its page API
   (`POST /lge/api/settings`):

   | Setting | lge off | lge on |
   |---|---|---|
   | Add graph context | off | on |
   | Shorten outputs | off | on |
   | Skeletons for whole-file reads | off | as you set it on the page |
   | Index on first use | off | off |

   Every other setting stays as it is. For the arm with lge on, the copy is added to the
   daemon and indexed before the session starts, so the first message already gets its
   context; it is removed from the daemon afterwards.
3. `setup` runs, then one session: `codex exec --json` in the copy, with the prompt (and
   the note) on stdin and the sandbox `workspace-write` (`--sandbox` changes it; `codex
   exec` alone is read-only). When `codex exec --help` lists `--approve-for-me`, asked once
   per run, the sessions get `--approve-for-me` in place of `-s workspace-write`: Codex
   reviews approval requests itself, in the workspace-write sandbox that flag selects,
   instead of stopping on them (Codex refuses the two flags together).
   `--no-approve-for-me` passes `-s workspace-write` as before; another `--sandbox` is
   passed as it is. The model is `--model`, else `model` from the `[session]` table of
   `lge.toml`, else Codex's own, and `codex_bin` there names the binary.
4. The token usage is read from the session's event stream: input (of which cached),
   output (of which reasoning), and turns. The daemon's status gives the number of model
   requests, and shows whether the session went through it at all.
5. `check` runs in the copy. Exit 0 is a success.
6. The copy is removed (`git worktree remove --force`).

The sessions run one after another, each in its own copy.

The two arms of a task alternate which goes first: off then on for the first task, on
then off for the second, and so on; with `--runs N` each task also swaps its order from
one run to the next, whatever the number of tasks. Whatever the first session of a pair
warms (the provider's prompt cache, the disk, the daemon) helps each arm equally often.

When the run ends, for any reason, the daemon gets its original settings back: after an
error, a panic, or Ctrl-C. The first Ctrl-C stops the session and the check that are
running (they receive it too), removes the copy and restores the settings; whatever
failed because of it is listed as interrupted, never as a result. A second Ctrl-C ends
`lge ab` at once, without restoring the settings: set them back on the daemon's page.
`SIGTERM` and `SIGHUP` stop the run the same way, but they reach only `lge ab`: a
session already running is not stopped and ends first.

## The report

```
base commit: 4f9c2a1e0b7d4c3a9e8f1b2c3d4e5f60718293a4
note appended to every prompt: Do not install, download or sync dependencies and do not run long test suites: the task's check runs after you finish.
sessions ran with --approve-for-me (workspace-write sandbox, approvals reviewed automatically)

task                    run  arm        input       cached     output  reasoning        total  turns   reqs  result
validate-order-payload    1  off      182,410      120,064      6,120      3,904      188,530      9     14  pass
validate-order-payload    1  on       121,377       90,112      4,870      2,950      126,247      6      9  pass
...

task                       off median      on median    change  off ok   on ok
validate-order-payload        188,530        126,247    -33.0%     1/1     1/1
...

compared over 2 of 2 task(s) where both arms succeeded
total tokens with lge against without: median -29.4%, mean -29.4%
success: without lge 2/2 (100%), with lge 2/2 (100%)
verdict: target met (at least 25% lower median total tokens with no drop in success): ...
```

- Total tokens are input plus output; cached input is part of input, reasoning part of
  output, as Codex counts them.
- The change of a task is `(on - off) / off` of the median total tokens of its successful
  runs. Only tasks where both arms succeeded are compared; the median and the mean are
  taken over those.
- Success is counted per arm over every pair where both arms ran: their checks gave an
  answer. A session that ended in an error before its check (the copy, the setup, the
  index or the agent could not run) is listed with its error and counted under
  `errored`; its pair is not compared and does not count for success, in either arm.
- A successful session whose event stream reported no token usage (cut off before
  `turn.completed`) counts for success but is left out of the medians (`unmeasured`):
  it would read as a session that cost nothing.
- The target is met when the median change is -25% or lower and the arm with lge
  succeeded at least as often as the arm without. A change printed in the verdict
  never rounds across the target: a miss by a hair reads `24.99% lower`, not `25.0%`.
- With `--runs N` each task runs N times in each arm, and a task's numbers are medians
  over its runs.
- `--json` prints the same report, every session included, as JSON. Every key is always
  present; a number that does not exist (no comparison) is `null`.
- A session whose requests never reached the daemon is flagged: the arm's settings did
  not apply to it.
- The first lines name the base commit (full id), the note appended to the prompts (or
  `none`), and whether the sessions ran with `--approve-for-me`; `--json` has them as
  `base`, `note` (`null` for none) and `approve_for_me`.

## Enterprise machines

What a run on a managed Mac needed, so yours does not:

- **OpenSSL 3, not 4.** LadybugDB links against OpenSSL 3; a build against OpenSSL 4
  (Homebrew's `openssl` may already be 4.x) fails. Build with

  ```bash
  brew install openssl@3
  export OPENSSL_DIR="$(brew --prefix openssl@3)"
  cargo build --release
  ```

  After a failed build, `cargo clean -p lbug` before building again.
- **Approval requests.** Some environments reject approval requests in `codex exec`, and a
  session that asks for one stalls. `lge ab` passes `--approve-for-me` automatically when
  your Codex has it (see step 3); no wrapper script is needed. With an older Codex the
  sessions get `-s workspace-write` as before.
- **Paths with spaces in checks.** `check` and `setup` are run by `sh -c`, so a path with
  spaces must be quoted inside the command, and TOML needs its own quotes around the whole:

  ```toml
  check = "python -m pytest -q 'tests/order flow/test_totals.py'"
  setup = "cp \"/Users/me/Shared Tests/test_total.py\" tests/"
  ```

  Or use a TOML literal string, where nothing is escaped:
  `check = '"/Users/me/My Tools/run-check.sh" --fast'`.
- **Keep checks fast and offline.** A check runs once per session (`tasks x runs x 2`
  times): run the tests that decide the task, not the whole suite, and nothing that
  downloads (`pip install`, `npm ci`, `uv sync`). Install what the checks need once, before
  the run, outside the copies, or in a `setup` that uses a local cache.

## Leftovers

The copies and their folder are removed. A copy that cannot be removed (the session or
the check left a folder without write permission) is named at the end of the run, with
the `git worktree prune` to run after deleting it. The graph the daemon built for each copy stays
in its data folder, named after the copy: `~/.lge/graphs/lge-ab-*`. Delete those folders
after the run, while the daemon is stopped or after it dropped the copies (it does so at
the end of each session).
