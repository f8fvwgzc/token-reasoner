# Prior art: what lge takes from four open-source projects

Studied on 2026-11-15 from shallow clones kept outside this repository
(`../references/`). No code was copied; the mechanisms below are reimplemented in
Rust on LadybugDB. None of the four can be used as a dependency: they need a Python
runtime, PostgreSQL or another storage engine, which the project rules exclude.

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
| Dedup ladder: normalized exact match, entropy gate, MinHash at Jaccard >= 0.9; a repeat raises the proof count | graphiti, hindsight | `lge-memory::dedup` |
| Recall in parallel arms: keyword, entity overlap `tanh(0.5 x shared entities)`, recent, optional vector; fused by rank | hindsight | `lge-memory::recall` |
| Boosts: recency `clamp(1 - days/365, 0.1, 1)` and proof count, each a few percent | hindsight | `lge-memory::recall` |
| Token packing in rank order: an oversized fact is skipped and packing continues; the top fact is always returned | hindsight | shared budget code |
| Pages: markdown views over tagged facts with a watermark; rebuilt only when a newer in-scope fact exists | hindsight | `Page` nodes, exported to `.lge/pages/*.md` |
| Routing by tag: component, concept, convention, decision, feature-work | hindsight | seeded pages |
| Search-first injection: return ranked 280-character snippets, never a list of titles | hindsight | `recall` and `search_pages` tools |

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
