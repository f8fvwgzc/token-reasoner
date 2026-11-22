# Prior art: what lge takes from eight open-source projects

Studied from shallow clones kept outside this repository (`../references/`): the four
context and memory projects below on 2026-11-15, Headroom and RTK for the proxy, and
Caveman and Token Optimizer (with RTK again) for v0.0.2 on 2026-11-16. On 2026-11-22
hindsight was read again for the memory techniques listed in the dated sections at the
end, and one idea (inject nothing when the evidence is weak) is credited to memorilla
(Apache-2.0), which is not among those clones and was taken as an outcome only. No code
was copied from any of them; the mechanisms below are reimplemented in Rust, in lge's own
code and words, on LadybugDB. None of the first four can be used as a dependency: they
need a Python runtime, PostgreSQL or another storage engine, which the project rules
exclude.

| Project | Licence | What it is | Fit for lge |
|---|---|---|---|
| [graphify](https://github.com/Graphify-Labs/graphify) | Apache-2.0 | Folder of code and documents -> knowledge graph that coding assistants query before reading files | Closest to the goal: a context graph for a coding agent |
| [hindsight](https://github.com/vectorize-io/hindsight) | MIT | Agent long-term memory: retain, recall, reflect, on PostgreSQL + pgvector | Best memory design; has a subset that needs no LLM |
| [graphiti](https://github.com/getzep/graphiti) | Apache-2.0 | Temporal knowledge graph for agent memory | Best model of facts that change; ingestion needs 2+ LLM calls per item |
| [omnigraph](https://github.com/modernrelay/omnigraph) | MIT | Typed graph database on Lance with branches and merges | A database, not a context builder; strong engineering practices |

## Which one is the best context graph

graphify, for this goal. It is the only one built to answer a coding agent's question
from a graph of the code instead of from the files, and it ships the Codex integration
(an `AGENTS.md` rule) that makes the agent ask the graph first.

Its limits, which lge must not repeat:

- An answer carries names and line numbers only, so the agent still opens the files.
  lge answers with signatures, doc lines and bodies under a token budget.
- Seeds are lexical only. lge adds identifier splitting, exact and prefix matches, graph
  expansion and (optionally) vectors, fused by rank.
- A call with several candidate targets is resolved by path proximity or dropped. lge
  stores it as `unresolved` and never picks.
- "71.5x fewer tokens" divides the size of the corpus by the size of the answer. It does
  not measure what an agent spends. lge measures Codex's own token counts per task.
- One JSON file loaded whole into memory. lge keeps the graph in LadybugDB.

## Mechanisms adopted

### Code context graph

| Mechanism | From | In lge |
|---|---|---|
| Seed-first answer under a budget; seeds are never cut; a truncated answer says which budget returns everything | graphify | `lge-retrieve` pack and every MCP tool |
| At most 3 seeds within 20% of the best score, one per name; tests and docs weighted 0.3 unless the task names them | graphify | `lge-retrieve::seeds` |
| Hub cut-off: a non-seed node with degree >= max(50, 99th percentile) is shown but not expanded | graphify | `lge-retrieve::expand` |
| Communities, cohesion, most-connected symbols, an orientation report the agent can read once | graphify | `lge-retrieve::report`, stored as pages |
| Rationale comments (`NOTE:`, `WHY:`, `HACK:`, `TODO:`) kept with the symbol | graphify | `rationale` facts, shown in packs |
| `AGENTS.md` rule: ask the graph before the first grep or file read; read only located lines; repeat the rule in sub-agent prompts | graphify | `lge init` |
| Rank fusion `score = sum of 1 / (60 + rank)` over several ranked lists, ties broken by id | hindsight, omnigraph | `lge-retrieve::fusion` |
| Graph expansion as one more ranked list; proximity to the seeds raises a hit | graphiti | personalized PageRank from the seeds |
| Drop near-duplicate snippets before they cost tokens (maximal marginal relevance) | graphiti | `lge-retrieve::fusion::mmr` |
| Results bounded by a byte or token cap, with a "narrow it" notice instead of silent truncation | omnigraph | MCP layer |
| A checked-in table of tokens per tool per scenario; a change fails the tests | omnigraph | `lge-bench` golden file |
| Tool instructions compiled into the binary and pinned to the tool list by a test | omnigraph | `lge-mcp` |

### Long-term memory

| Mechanism | From | In lge |
|---|---|---|
| Memory units of kinds world, experience, decision, convention, observation, with tags and links to the code entities they mention | hindsight | `Memory` nodes, `MENTIONS` edges to Symbol, File, Service |
| Facts written by the agent itself; no extraction model in the loop | hindsight ("chunks" mode) | `retain` |
| Supersede, never delete: `valid_at`, `invalid_at`, `expired_at`; a new fact with the same key closes the old one; reads skip expired facts | graphiti | `lge-memory::retain` |
| Dedup ladder: normalized exact match, entropy gate, character-3-gram Jaccard >= 0.9 (exact, no MinHash); a repeat raises the proof count | graphiti, hindsight | `lge-memory::dedup` |
| Guard before the ladder: facts that differ in a number, an identifier (with its case) or a negation are different facts, whatever the similarity (2026-11-22) | hindsight (the principle; its check is a model call, lge's is a word list) | `lge-memory::dedup::guard_agrees` |
| Recall in parallel arms: keyword, entity overlap, recent, optional vector; fused by rank | hindsight | `lge-memory::recall` |
| Entity arm ordered by `shared / sqrt(links)`, so a fact about few things beats one about many (2026-11-22; before, by shared nodes alone, newest first) | hindsight (inverse square root of a node's degree) | `lge-memory::recall::damped` |
| Recency re-ranks a fact another arm found and never admits one: injected packs withhold a fact only the recent arm returned (2026-11-22) | hindsight (recency is a multiplier), memorilla (outcome only: inject nothing on weak evidence) | `lge-memory::recall`, `RecallOptions::admit_recent_only`, `Recall::recency_only` |
| A commit that changes more than 200 files is stored without links and tagged `mass-commit`; a fact with more than 200 links is never returned by the keyword, entity or vector arm (2026-11-22) | lge's own | `lge-memory::history`, `lge-memory::recall` (`MASS_FILES`, `MASS_LINKS`) |
| Boosts: recency `clamp(1 - days/365, 0.1, 1)` and proof count, each a few percent | hindsight | `lge-memory::recall` |
| Token packing in rank order: an oversized fact is skipped and packing continues; the top fact is always returned | hindsight | shared budget code |
| Pages: markdown views over tagged facts with a watermark; rebuilt only when a newer in-scope fact exists | hindsight | `Page` nodes, exported to `.lge/pages/*.md` |
| Routing by tag: component, concept, convention, decision, feature-work | hindsight | seeded pages |
| Search-first injection: return ranked 280-character snippets, never a list of titles | hindsight | `recall` and `search_pages` tools |

### The proxy

Studied later, for `lge serve` (see `docs/PROXY.md`): [headroom](https://github.com/chopratejas/headroom)
(Apache-2.0), a context-optimizing proxy, and [rtk](https://github.com/rtk-ai/rtk), a
command-output condenser (Apache-2.0 at the root; its `engine` and `proxy` directories
are BUSL-1.1, so only ideas were taken). The request format was checked against the
[Codex](https://github.com/openai/codex) source (Apache-2.0).

| Mechanism | From | In lge |
|---|---|---|
| Decide each rewrite once and replay the same bytes on later turns, so the provider's prefix cache keeps hitting | headroom | `lge-proxy::session` |
| Forward a body that was not changed as the exact bytes that came; relay the response stream as bytes and parse it only for usage | headroom | `lge-proxy::server` |
| Point a repeated output to its first copy | headroom | `lge-proxy::responses` |
| Leave file reads alone by default; outlining a read is opt-in | headroom, rtk | the `skeleton` rule: off in v0.0.1, measured in `shadow` (never sent) from v0.0.2 until you switch it on |
| Test, lint and type-check runs reduced to failures and summary | rtk, headroom | `lge-filter` through the proxy |
| Never send a result that is not smaller; never summarise an unknown format | rtk (`never_worse`), headroom | `lge-proxy::shorten` |
| No truncation notice without a way back: each marker names the exact command that returns what was left out | rtk | skeleton and search markers |
| Count a saving once per conversation, not once per request | headroom | the page's "removed" figure |
| Fail open | both | the proxy forwards the original on any error |

Both projects report that the saving on whole requests is far below the saving on the
compressed parts (rtk: "a 90% cut in command output is not a 90% cut in cost";
headroom: 0% on plain source files, and a net loss when small results are compressed).
lge's page therefore shows provider-reported tokens per session next to what was removed.

## v0.0.2: three token-cutting tools

Studied on 2026-11-16 for v0.0.2: what each tool cuts, how it measures, and which of
its ideas fit lge's rules (never change the model, the reasoning level, the system prompt,
tool definitions, tool calls or patches; decide each item once; fail open; never
summarise a format lge does not recognise).

| Project | Licence | What lge takes |
|---|---|---|
| [RTK](https://github.com/rtk-ai/rtk) (rtk-ai/rtk v0.49.0) | Apache-2.0 at the root (the CLI in `src/`, `text/`); its `engine/` and `proxy/` directories are BUSL-1.1 | Ideas only, reimplemented in lge's own code: per-command output parsers, line filters and command recognition from the Apache-2.0 part; normalisation, shadow rules, lock-file lists, JSON minifying and the proxy's markers from the BUSL part, as ideas only. No code, regex list or filter table was copied |
| [Caveman](https://github.com/JuliusBrussee/caveman) (Julius Brussee) | Apache-2.0 | Ideas only: terse replies, frozen decisions, a prefix tripwire, resending a refused request, code-mode result unwrapping. "Caveman" is a trademark, so lge's skill is called terse-replies and no lge feature or file uses the name |
| [Token Optimizer](https://github.com/alexgreensh/token-optimizer) (Alex Greenshpun) | PolyForm Noncommercial 1.0.0 | Ideas only: recovery rates with thresholds fixed in advance, a holdout arm, skeleton cohorts, fixed-prefix and cache-break attribution, deltas for re-runs. No code, regex lists, tables, prompts or wording |

| Mechanism | From | In lge |
|---|---|---|
| A pass is stated only when the tool's own closing line ends the output and one producer printed it; a failure is never hidden | RTK (never hide a failure, bypass), Token Optimizer (one producer must remain), Caveman (fail closed) | `lge-filter::filter_for_proxy` (closed, trailing, unparsed), `lge-proxy::shorten::check_run` |
| Warnings grouped by rule with every location | RTK (clippy grouped by rule) | `lge-filter::format::group_by_rule` |
| Failure parsers for go test, go build, go vet, golangci-lint, Maven, Gradle, gcc and clang; cargo suites and nextest | RTK's per-command filters, rebuilt as parsers that never drop a failure | `lge-filter::parse::{go, golangci, maven, gradle, cc, rust}` |
| Lossless clean-up: escape codes, carriage-return redraws, trailing blanks, runs of identical lines | RTK (its proxy's default normalisation), Caveman (terminal and repetition clean-up) | `lge-filter::{normalize_terminal, collapse_repeats}`, rule `inert` |
| Path lists written with each folder once, sent only when unfolding gives the input back | RTK (folding by folder), Token Optimizer (recount against the input) | `lge-proxy::fold`, rule `path_fold` |
| JSON without whitespace outside strings, byte-exact otherwise | RTK (JSON minifying) | `shorten::minify_json`, rule `json_minify` |
| Long search lines clipped around the match, with the exact command for the whole line | RTK (match window in its search filter) | `shorten::clip_match_lines`, rule `line_clip` |
| A capped search names every file it left out | Token Optimizer (every name kept after a cap), RTK (count of files left out) | `shorten::cap_search` |
| A repeat pointer names the run it repeats and quotes its first line | RTK (its proxy's marker names a block by its first lines) | `responses::pointer_text` |
| Progress lines of installs, transfers and builds dropped by a table, with lines that mention errors always kept and no line cap | RTK (declarative line filters, install and build filters), Token Optimizer (build-output collapse that keeps error lines) | `lge-proxy::noise`, rule `noise` |
| A lock file shown as its package list | RTK (lock-file method) | `lge-proxy::lockfile`, rule `lockfile` |
| Deltas for a re-run and a re-read, measured only | Token Optimizer (cross-turn deltas) | `lge-proxy::delta`, rules `rerun_delta`, `read_delta` |
| Skeleton reads counted by language and size band | Token Optimizer (first-read cohorts) | `rules::cohort_key`, the page's cohort table |
| RTK's output recognised and never shortened twice; `rtk proxy` read as the command | RTK (its proxy skips blocks it stamped; its bypass) | `command::normalise`, `shorten::condensed_upstream` |
| Harmless leading statements dropped before a command is classified | Token Optimizer (post-run command classifier), RTK (its interop and bypass rules) | `command::normalise` |
| A re-run with nothing changed in between returns the output whole | Token Optimizer (a repeat with no edit between is deliberate), RTK (no cut without a way back) | `responses::Known::recovery` |
| A mode per rule: off, shadow, on | RTK (shadow rules), Caveman (record mode), Token Optimizer (opportunity tier) | `lge-proxy::rules`, `session::Chain` |
| Recovery within the next calls, thresholds fixed before any data, automatic demotion | Token Optimizer | `session::judge`, `server::check_rules` |
| A holdout arm compared with a bootstrap interval | Token Optimizer | `server::in_holdout`, `lge_usage::compare_arms` |
| Cache breaks caused by the proxy told apart from the client's | Caveman (prefix tripwire), Token Optimizer (cache-break attribution) | `session::compare_prefix` |
| The fixed prefix per request and the tools never called; cold restarts | Token Optimizer (fixed-overhead audit, unused servers, cache drops) | `server::tool_defs`, `server::Conv::observe_response` |
| Earlier items keep their bytes whatever the settings say now | Caveman (frozen substitutions) | `session::Session::replay`, looked up first |
| A refused request sent once more without the new rewrites | Caveman (resends the original); lge reverts only the request's new decisions | `server::Daemon::proxy`, `session::Session::revert_new` |
| Code-mode results: several commands, exec results printed as JSON | Caveman (code-mode result unwrapping), checked against Codex's source | `command::script_plan`, `responses::unframe` |
| Past sessions replayed through the rules; commands corrected after a usage error | RTK (`discover`, and `learn` with a window of 3 calls), Token Optimizer (reads Codex rollouts) | `lge-proxy::discover`, `lge usage discover` |
| A terse-replies skill and AGENTS.md section | Caveman (terse reply rules), RTK (terse directive); written in lge's own words | `lge_bench::agents`, `lge init --terse` |
| AGENTS.md lines on condensed output and on batching read-only commands | RTK (awareness note), Caveman (tool-run rule) | `lge_bench::agents`, `lge init`, `lge init --batch` |

### Deliberately not adopted from these three

- Rewriting commands through a Codex hook: it breaks Codex's saved approvals and runs
  commands again, and the proxy already sees every output.
- A retrieve tool answered inside the proxy: it changes tool definitions and calls, and
  would make the proxy edit the response stream.
- Pointers for written file bodies, truncated tool-call arguments: lge never touches tool
  calls or patches.
- Model routing, cheaper models for reads, effort hints, output caps: each changes the
  model or the reasoning, or cuts answers.
- Setting `model_verbosity` or prompt-cache retention in flight: body fields outside lge's
  forward-as-is contract; `lge init` only mentions `model_verbosity` as your choice.
- Compacting `git diff` and `git show`: the model writes patches from diffs.
- Head and tail caps on unknown output, log-template merging, folding library stack
  frames: they summarise formats lge does not recognise.
- An in-memory store of originals with a recall tool: needed only for caps lge does not
  adopt, and it would add a tool to the cached tool list.
- Diffs or structure maps in place of a re-read: the model re-reads after a patch to get
  the exact text; v0.0.2 only measures `read_delta`.
- Cohorts that switch skeletons on by themselves: promoting a lossy rule automatically is
  not safe; you switch it on.
- Re-encoding JSON arrays as tables, truncating cells: changes what the model reads, or
  loses values.
- Top-N lint lists, head-biased line caps, first-N stack frames, tree depth cuts, row caps
  on listings, shortened paths: each can drop a finding or make a path unopenable.
- Eliding base64, slimming HTML and Markdown bodies, compacting `env`, `psql`, `git log`
  and branch lists: rare in coding sessions, or lossy without a way back.
- Parsers for JUnit XML or JSON test reports: agents write them to files; JSON printed to
  stdout still gets `json_minify`.
- Attributing `write_stdin` polls to their command: a poll is a fragment of the run.
- A re-orientation item after a compaction: it repeats Codex's own summary.
- Re-deciding old items once the cache is cold: a wrong guess bills the whole prefix
  uncached.
- A terse or scope sentence added every turn, nudges based on context fill: they change
  behaviour every turn and add input; only the opt-in skill ships, measured with
  `lge ab --variant terse`.
- Quiet-flag advice and "no commentary" in the default AGENTS.md block: unmeasured, and
  every line costs prefix tokens; the commentary rule is in the opt-in skill.
- Thrash notes, quality warnings, a quality score: they write into outputs the model
  trusts; the page shows raw signals instead.
- Shrinking lge's own packs as the context fills, rewording lge's own short texts: too
  small a gain.
- Keep-warm pings, cache warming, injecting `prompt_cache_key`: spend tokens on a guess,
  or Codex sends its own key.
- Shrinking other servers' tool schemas: tool definitions start the cached prefix; lge
  only reports their cost.
- Rewriting AGENTS.md or memory files with a model: spends tokens and sends files to a
  provider.
- Replies in classical Chinese: the tokeniser erases the saving.
- Invariant markers and rarity guards for lossy collapses: they only serve collapses lge
  does not adopt.
- Writing rewrite decisions to disk with their content: against lge's storage rule; the
  ledger keeps only hash prefixes and rule ids.
- Pinning a block after repeated retrieves: re-rendering a past item breaks the cache.
- Refusing identical repeated MCP calls: a proxy cannot refuse a tool call; identical
  outputs already become pointers.
- Image downscaling, text rendered as images, secret redaction: lge never changes
  non-text parts, and redaction is a privacy matter, not a token one.
- Waste coaches, fleet audits, compaction advice, a second memory store: they count
  savings with fixed guesses, or duplicate `lge-memory`.

## 2026-11-22: a guard on the dedup ladder

- **Number, identifier and negation guard** (hindsight, MIT): its consolidation sends a
  near-duplicate observation to a model that must keep it apart when a number, a
  negation or an entity differs. lge adopts the principle without a model, in its own
  words: before the ladder runs, the numbers, identifiers (names
  with `_`, `/` or an inner dot, camelCase, acronyms, compared with their case) and
  negations of the two raw texts are compared, and any difference means a different
  fact. This keeps "timeout 15 seconds" and "timeout 45 seconds" from being folded
  together, so a fact with the same key supersedes the old one instead. No code was
  copied; the tokenising rules and the word list are lge's own. No tokens are added.

## 2026-11-22: recency re-ranks, it never admits

- **Recency is a boost, not a way in** (hindsight, MIT): there, recency multiplies the
  score of a fact that retrieval already found; it is not a retrieval arm. lge kept its
  recent arm (it still gives rank to a fact another arm returned) but a fact only that
  arm returned is no longer admitted into an injected pack; explicit recall still lists
  it. Reimplemented in lge's own code; nothing was copied.
- **Inject nothing when the evidence is weak** (memorilla, Apache-2.0): taken as an
  outcome only, not as an algorithm. lge does not add a score threshold: no constant is
  chosen without data. `Recall::recency_only` counts what the floor withheld so a later
  benchmark can say whether a keyword floor is needed.

## 2026-11-22: mass commits and hub damping

- **Degree damping on the entity arm** (hindsight, MIT): its entity resolution weights a
  shared neighbour by the inverse square root of its degree, so hubs do not drown out
  specific links. lge applies the idea in its own words, on a different object and on
  what the store exposes cheaply, a fact's own link count: facts that mention the query's code are ordered by
  `shared / sqrt(links)`, ties by newest then id. A fact about two things that both
  matter beats one about forty that touches one of them. Damping a node that many facts
  mention is a separate, later item. No code was copied.
- **No links for a mass commit, and no admission** (lge's own, after measuring one
  initial commit that wrote thousands of `MENTIONS` edges): a commit that changes more
  than 200 files is stored without links and tagged `mass-commit`; the fact text still
  names eight files and counts the rest. Rows stored earlier are handled at read time:
  a fact with more than 200 links is never returned by the keyword, entity or vector
  arm. Nothing is migrated, and no edge is invented or rewritten. No tokens are added;
  the change removes a fact that matched every query about a file.

## Deliberately not adopted

- LLM extraction, contradiction and consolidation calls (graphiti, hindsight): they spend
  the tokens this project exists to save. The agent writes its own facts at the end of a
  task; consolidation is deterministic. An LLM pass can be added later as an explicit,
  off-by-default command.
- Cross-encoder reranking (hindsight, graphiti): needs a second local model.
- Lance storage, branches and merges (omnigraph): the database stays LadybugDB. Re-index
  after a branch switch is incremental by content hash.
- Picking one of several call candidates by path proximity (graphify): it would store a
  guess.
