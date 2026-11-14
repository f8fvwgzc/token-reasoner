# Saving tokens with the model and the reasoning level fixed

Decision of 2026-11-12: the Codex model and its reasoning level (high / ultra) do not
change. The saving has to come from sending and generating fewer tokens for the same
work. This file is the contract for that; it extends `docs/ARCHITECTURE.md` and wins
where the two disagree.

No figure in `docs/PLAN.md` is measured yet. `lge usage report` exists so that every
lever below is switched on separately and judged against a baseline.

## Where the tokens go, and what cuts them

| Cost | Why it is large today | What cuts it | Crate |
|---|---|---|---|
| Input tokens re-sent every turn | Long sessions; every exploration result stays in context | One short session per task, started from an evidence pack | lge-session |
| Exploration turns (grep, list, read whole files) | The model has to find the code before it can think about it | Evidence pack up front; MCP tools that answer with ranked snippets | lge-retrieve, lge-mcp |
| Reasoning and output tokens spent on exploration | Each exploration turn is reasoned about | Fewer turns: the first answer already has callers, callees, tests, APIs, tables, queues | lge-retrieve |
| Uncached input | The prompt start changes between sessions | Byte-identical prefix, task text last | lge-session |
| Repeated content | The same snippet is returned by several tool calls | Per-session dedupe: a snippet is sent once, later results refer to it | lge-mcp |
| Raw lint, type and test output | Thousands of passing lines | Failures and the relevant diff hunks only | lge-filter |

The reasoning router stays, in **shadow mode**: it always computes and logs the level
it would choose, and the session runs at the user's fixed level unless
`level_mode = "routed"` is set. The shadow log shows what routing would have saved
without risking quality.

## Using LadybugDB fully (lge-store, schema version 3)

- **Identifier-aware BM25.** `Symbol.search_text` holds the words of the name, the
  qualified name and the file path split on case, `_`, `-`, `.`, `/` and digits
  (`createOrderHandler` -> `create order handler`), plus signature and doc words. The
  FTS index covers `name`, `qualified_name`, `signature`, `doc` and `search_text`.
  Query text is split the same way before it is sent to the index.
- **Rank stored in the graph.** `Symbol.rank DOUBLE` holds a global PageRank computed in
  Rust after each resolve (the `algo` extension does not load on macOS arm64, so the
  algorithm is ours). It is the prior for ranking and the order of the repo map.
- **Traversal in the database.** `reach` (bounded variable-length match with a hop
  count) and `shortest_path` (Cypher shortest path) run inside LadybugDB; mixed-direction
  impact uses one-hop `edges` calls.
- **Bulk load.** The first index of a repository goes through `COPY FROM`; incremental
  updates use prepared statements in one transaction.
- **One process owns the file.** CLI commands open, work and close; the MCP server keeps
  the database open and does its own re-indexing.
- **Vector index.** The `vector` extension loads on this platform, but embeddings need a
  model. Not built; revisit only if the benchmark shows BM25 plus graph misses files.

Trait additions (on top of `graph_store.rs`):

```rust
/// Nodes reachable from `ids` over `types` in one direction within `depth` hops,
/// with the lowest hop count, nearest first, at most `limit`.
fn reach(&self, ids: &[String], types: &[EdgeType], dir: Direction, depth: u8, limit: usize) -> Result<Vec<(NodeRec, u8)>>;
/// A shortest path from `from` to `to` following `types` forwards, as its edges in order.
fn shortest_path(&self, from: &str, to: &str, types: &[EdgeType], max_depth: u8) -> Result<Option<Vec<EdgeRec>>>;
/// Store the global rank of symbols (id, rank). Unknown ids are skipped.
fn set_ranks(&self, ranks: &[(String, f64)]) -> Result<()>;
/// Symbols ordered by stored rank, highest first, optionally within one service. Modules and placeholders excluded.
fn top_ranked(&self, service: Option<&str>, limit: usize) -> Result<Vec<Hit>>;
/// Like `search`, restricted to one service.
fn search_in(&self, text: &str, service: Option<&str>, limit: usize) -> Result<Vec<Hit>>;
/// Every non-CONTAINS, non-unresolved edge between Symbols, for ranking: (from, to, confidence).
fn symbol_graph(&self) -> Result<Vec<(String, String, f64)>>;
```

`UsageRecord` gains `recommended_level` (what the router would have chosen) and `mode`
(`baseline`, `pack`, `pack+mcp`, ... : which levers were on), so the report can compare.

## Retrieval (lge-retrieve)

```
task text
  -> seeds: exact names quoted or written in the task, BM25 over search_text, prefix matches
  -> expand: callees (what the seed needs), callers and tests (what the change affects),
             endpoints, tables and queues, bounded by depth and node cap
  -> rank:   personalized PageRank on the expanded subgraph, restarted at the seeds,
             edge weight = confidence; mixed with BM25 score and the stored global rank
  -> tier:   1 = seeds: signature, doc, body
             2 = strong neighbours: signature, doc, line range
             3 = the rest: one line each (path:line name)
  -> budget: tiktoken count, default 6,000, filled in rank order, never exceeded
  -> pack:   compact text grouped by file, merged line ranges, edges listed once with
             provenance; inferred and unresolved edges are marked so the model checks them
```

- The pack is plain text, not JSON, with a stable order (rank, then path, then line).
- `repo_map(store, service, budget)`: the highest-ranked symbols per file as signatures,
  a fixed-size orientation block.
- `impact`, `trace`, `dependencies` and `architecture` as in `docs/ARCHITECTURE.md`.
- Everything is deterministic for a given graph, so packs are cacheable and testable.

## MCP server (lge-mcp)

- Six tools, one-line descriptions, compact text results, each under a token budget
  (default 1,500, caller may raise it) and each ending with `commit <sha>`.
- Session dedupe: the server remembers which symbol snippets it has already returned
  (by symbol id and content hash) and answers `see above: path:lines` instead of
  repeating them, unless the file changed.
- Freshness: changed files are re-indexed before a tool answers; the watcher runs in
  the same process.

## Sessions and Codex CLI on macOS (lge-session, lge-cli)

- `lge init`: prints (and with `--write` applies) the `[mcp_servers.local_graph]` entry
  for `~/.codex/config.toml` and an `AGENTS.md` block that tells Codex to ask the graph
  first and to read snippets, not whole files.
- `lge run "<task>"`: route (shadow), build the pack, start one Codex session with
  `STABLE_PREFIX + pack + handoff + task`, run the checks, write the usage record and
  the handoff note. `level_mode = "fixed"` (default) passes no reasoning override, so
  Codex keeps the configured model and effort; `level_mode = "routed"` applies the
  router's level and escalates on failed checks.
- `lge pack "<task>"`: print the evidence pack only, for use with any agent.
- `lge usage report`: tokens per task by class, cached share, by mode, plus what the
  shadow router would have chosen.

## Long-term memory and pages (lge-memory)

Modelled on hindsight and graphiti (see `docs/PRIOR_ART.md`), without an LLM in the loop.

- **retain(text, kind, key, tags, source)**: the agent writes a fact at the end of a
  task. Entity links are found by matching identifiers, qualified names and paths in the
  text against the graph. Dedup ladder: normalized exact match, entropy gate, MinHash
  (32 hashes, 3-character shingles) at Jaccard >= 0.9; a repeat raises `proof_count`.
  A fact with a key supersedes current facts with the same key (`invalid_at = new.valid_at`,
  `expired_at = now`); nothing is deleted.
- **recall(query, budget)**: four arms, each a ranked id list: keyword (BM25), entity
  overlap with the code symbols the query hits (`tanh(0.5 x shared)`), recent, and vector
  when embeddings exist. Fused with `sum 1/(60 + rank)`; rank score `1 - 0.9 x rank/(n-1)`
  multiplied by `(1 + 0.2 (recency - 0.5))` with `recency = clamp(1 - days/365, 0.1, 1)`
  and `(1 + 0.1 (proof - 0.5))` with `proof = clamp(0.5 + ln(proof_count)/10, 0, 1)`.
  Packed in rank order to the token budget: an oversized fact is skipped, packing
  continues, the top fact is always returned.
- **Pages**: markdown views. Memory pages are seeded by tag (`component`, `concept`,
  `convention`, `decision`, `feature-work`) and rebuilt, deterministically, only when a
  newer in-scope fact exists (`last_memory_seen_at` watermark). Graph pages
  (`graph-overview`, one per community) are rebuilt after each index run. Pages are
  exported to `.lge/pages/<slug>.md` with YAML front matter and searched by keyword;
  a search returns 280-character snippets, never a list of titles.
- **Vectors** are optional. The store keeps embeddings and answers cosine search; an
  embedder is plugged in behind a trait. Without one, recall and search use the other arms.
