# Local Graph Intelligence Engine (lge)

A local code graph for coding agents, written in Rust on LadybugDB. It indexes a
repository, resolves calls and imports with provenance, and chooses the reasoning
level for a task from the change impact.

Status: phase 0 spike and the first slice of phase 1 (Python only).

## Build

    cargo build --release        # binary: target/release/lge
    cargo test

The `lbug` crate downloads a prebuilt LadybugDB library during the build. If the
download fails it compiles LadybugDB from C++ source, which is slow. To use a
library you downloaded yourself:

    export LBUG_LIBRARY_DIR=/path/to/liblbug LBUG_INCLUDE_DIR=/path/to/liblbug

## Use

    lge index <dir>                 # incremental: only changed files are re-parsed
    lge search "task creation request"
    lge impact save_task            # callers and tests, by hop
    lge route "verify token must check expiry"   # JSON: low | medium | top
    lge stats
    lge query "MATCH (s:Symbol) RETURN s.qualified_name LIMIT 5;"
    lge setup                       # install the BM25 (fts) extension, once

The database defaults to `.lge/graph.lbug`; change it with `--db`.
`lge index --service-depth 1` (default) treats each top-level directory as a
service; `0` treats the whole directory as one service.

## Crates

| Crate | Role |
|---|---|
| lge-core | Node and edge types, stable ids |
| lge-index | Scanner and Tree-sitter extraction (Python) |
| lge-store | LadybugDB schema, import and call resolution, search, traversal |
| lge-router | Reasoning router v0 |
| lge-cli | The `lge` binary |

## Edge provenance

| method | confidence | meaning |
|---|---|---|
| static | 0.90 to 0.95 | resolved through the same file, `self`, or an import |
| inferred | 0.40 to 0.50 | the only symbol in the repository with that name |
| unresolved | 0 | several candidates; stored, never guessed |

Calls to names that do not exist in the repository are counted as external and
not stored.

## Not done yet

- BM25 search is wired to the fts extension but was not run: the build
  environment could not download extensions. Search falls back to a substring
  match until `lge setup` succeeds.
- TypeScript, API routes, database and queue edges, the MCP server, the usage
  logger, the session builder and the output filter.
- The inferred and unresolved paths are implemented but the fixture does not
  exercise them.

`crates/lge-store/examples/spike.rs` is the phase 0 extension check:

    cargo run -p lge-store --example spike
