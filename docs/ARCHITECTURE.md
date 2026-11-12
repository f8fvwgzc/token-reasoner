# Architecture and crate contracts

This file is the working contract for the code. `docs/PLAN.md` is the product plan;
this file says how the plan is cut into crates and what each crate promises the others.
The shared types live in `crates/lge-core/src/lib.rs` and the storage trait in
`crates/lge-store/src/graph_store.rs`. Read both before changing anything.

## Rules that never bend

1. **LadybugDB only.** No SQLite, no other database, no Python runtime, no external
   analyzer (LSP, SCIP, CodeQL).
2. **All database access is inside `lge-store`.** No other crate depends on or imports
   `lbug`. Other crates use the `GraphStore` trait.
3. **Every edge keeps its provenance**: `method` is `static`, `inferred` or `unresolved`,
   with a confidence, an evidence file and a line range. A guess is never stored as
   `static`. A call with several candidate targets is stored as `unresolved` (edge to a
   placeholder node, confidence 0); it is never resolved by picking one.
4. Results are snippets and line ranges, not whole files.
5. Tool and prompt text that goes to the model stays short and byte-stable.

## Crates and dependency order

```
lge-core      types, ids, vocabulary                         (no deps on other lge crates)
lge-index     scan, hash, git, tree-sitter extraction        -> core
lge-resolve   pure resolver: ResolveInput -> Resolved        -> core
lge-store     GraphStore trait + LadybugDB adapter           -> core
lge-usage     Codex usage parsing and reports                -> core
lge-filter    local checks and output reduction              (standalone)
lge-retrieve  index pipeline, traversal, evidence packs      -> core, index, resolve, store
lge-router    reasoning router v0 and v1                     -> core, store, retrieve
lge-session   prompt builder and session runner              -> core, store, retrieve, router, usage, filter
lge-mcp       rmcp stdio server, six tools, watcher          -> core, store, retrieve
lge-bench     benchmark runner                               -> core, store, retrieve, router
lge-cli       the `lge` binary                               -> everything
```

## Data flow of one index run

```
scan(root) -> hash -> parse_file -> store.insert_parsed          (per changed file)
store.load_resolve_input -> lge_resolve::resolve -> store.replace_edges   (once per run)
```

Resolution is always repository-wide and always rebuilt from the staged call, import and
fact sites, so an incremental run and a full run give the same graph.

## lge-index

```rust
pub struct ScannedFile { pub rel_path: String, pub abs_path: PathBuf, pub service: String, pub lang: &'static str }
pub fn lang_of(path: &str) -> Option<&'static str>;          // by extension
pub fn scan(root: &Path, service_depth: usize) -> Result<Vec<ScannedFile>>;
pub fn hash_file(path: &Path) -> Result<(Vec<u8>, String)>;
pub fn parse_file(rel_path: &str, service: &str, src: &[u8], hash: &str) -> Result<Option<ParsedFile>>; // None: unsupported
pub fn parse_python(rel_path: &str, service: &str, src: &[u8], hash: &str) -> Result<ParsedFile>;
pub fn parse_typescript(rel_path: &str, service: &str, src: &[u8], hash: &str) -> Result<ParsedFile>;   // .ts .tsx .js .jsx .mjs .cjs
pub fn parse_many(files: &[ScannedFile]) -> Vec<(String, Result<ParsedFile>)>;   // rayon; reads, hashes and parses
pub mod git {
    pub fn head_commit(root: &Path) -> Option<String>;                 // None outside a git repository
    pub fn changed_files(root: &Path) -> Result<Vec<String>>;          // modified, added, deleted and untracked paths, relative to root
}
pub mod facts { pub fn sql_tables(sql: &str) -> Vec<(String, bool)>; } // (table, is_write) for SELECT/INSERT/UPDATE/DELETE/JOIN
```

- `scan` honours `.gitignore` and also skips `node_modules`, `dist`, `build`, `.venv`,
  `venv`, `__pycache__`, `*.d.ts` and `*.min.js`. Languages: `.py` is python; `.ts`,
  `.tsx` are typescript; `.js`, `.jsx`, `.mjs`, `.cjs` are javascript.
- Qualified names. Python: dotted module path (`work_svc.service.TaskService.create_task`).
  TypeScript/JavaScript: the file path without extension, with `/` replaced by `.`,
  then the symbol path (`web_gateway.src.routes.getOrderHandler`, `notifier.src.audit.AuditLog.recordDelivery`).
  A file named `index.*` drops the `index` segment, like `__init__.py`.
- Every file gets a `module` symbol. Symbol ids use `lge_core::symbol_id`.
- `SymbolRec.body` holds the definition's source text, cut at 4,000 bytes on a char
  boundary. Empty for modules.
- `is_test`: Python as today; TS/JS when the file is `*.test.*`, `*.spec.*` or under
  `__tests__/` or `tests/`, or the call is inside `describe`/`it`/`test` (the arrow
  function passed to `it("name", ...)` becomes a function symbol named after the test).
- Call sites: `receiver` is `""` for a bare call, `"self"` for `self`, `cls` and `this`,
  otherwise the receiver text cut at 80 chars. `new Foo()` is a bare call to `Foo`;
  `new Foo().bar()` has receiver `Foo()`.
- Import sites: see the doc comment on `ImportSite`. `require("./x")` and
  `import x = require("./x")` are namespace imports (`name = "*"`).
  `export { a } from "./b"` is recorded as an import of `a` (re-exports are followed one
  step by the resolver).
- Fact sites (`lge_core::fact`). `literal` is true only when the key is a plain string
  literal with no interpolation.
  - `api_expose`: Flask `@app.route("/x", methods=[...])`, `@bp.get("/x")`; FastAPI
    `@app.get("/x")`, `@router.post("/x")` (join an `APIRouter(prefix=...)` or
    `Blueprint(url_prefix=...)` literal declared in the same file); Express
    `app.get("/x", handler)`, `router.post("/x", mw, handler)` (the fact belongs to the
    handler symbol when it is a named function in the file or an import, otherwise to
    the enclosing symbol); NestJS `@Controller("prefix")` with `@Get(":id")` and friends.
  - `api_call`: `requests.get/post/...`, `httpx.get/...`, `session.get/...` when the
    first argument looks like a URL or path; `fetch(url, { method })`,
    `axios.get/post/...`, `axios({ url, method })`, `http.get`. The key is the URL as
    written; for a template or concatenation, write the pieces with `${}` holes
    (`${BASE}/orders/${id}`) and set `literal = false`.
  - `db_read` / `db_write`: table names from SQL in string literals (`SELECT ... FROM t`,
    `JOIN t`, `INSERT INTO t`, `UPDATE t`, `DELETE FROM t`), via `facts::sql_tables`.
    Prisma `prisma.order.findMany()` reads `order`; `create/update/delete/upsert` write it
    (`literal = false`, because the table name is derived from the property).
  - `table_model`: SQLAlchemy `__tablename__ = "orders"`, Django `db_table = "orders"`,
    TypeORM `@Entity("orders")`, on the class symbol.
  - `queue_publish` / `queue_consume`: pika `basic_publish(exchange=..., routing_key=...)`
    (queue name = routing key when the exchange is `""`, kind `queue`; otherwise the
    exchange, kind `exchange`), `basic_consume(queue=...)`; kombu/celery
    `@app.task(name=...)` is a consumer and `send_task("name")` a publisher; amqplib
    `channel.sendToQueue("q", ...)`, `channel.publish("ex", "key", ...)`,
    `channel.consume("q", handler)` (the fact belongs to the handler when it is a named
    function); NestJS `@EventPattern("x")`, `@MessagePattern("x")` consume and
    `client.emit("x")`, `client.send("x")` publish (kind `topic`).
  - `inherits` / `implements`: class bases, `extends` and `implements`.
  - `reference`: an identifier that names a function or class and is passed as an
    argument or used as a decorator without being called.
  - `default_export`: on the symbol a TS/JS file exports as default
    (`export default function f`, `export default class C`, `export default f;`).
  - For SQL, `literal` describes the table name: a table written out in a template
    string or f-string that interpolates only values is still `literal = true`.

## lge-resolve

```rust
pub fn resolve(input: &ResolveInput) -> Resolved;   // pure and deterministic; output sorted
```

- **Imports.** Python rules are the ones in the original `Store::rebuild_edges`
  (dotted suffix lookup, relative imports, same-service tie-break). TS/JS: a relative
  specifier is tried as written, then with `.ts .tsx .js .jsx .mjs .cjs`, then as a
  directory with `index.*`; a bare specifier is external. A resolved import gives an
  `IMPORTS` edge File -> File, `static`, confidence 0.90.
- **Calls.** Port the original rules unchanged: `self` receiver -> method of the same
  class (static 0.95); bare name -> callable in the same file (static 0.95) or imported
  name (static 0.90); receiver that is an imported module -> callable in that module
  (static 0.90); receiver that names an imported or same-file class (`Repo()`, `Repo`) ->
  its method (static 0.90). Otherwise repository-wide by name: one candidate -> `inferred`
  (0.50 bare, 0.40 with a receiver); several -> `unresolved` to the placeholder
  `unresolved_id(name)`; none, or a too-common method name on a receiver -> external
  (counted, not stored). A TS default import resolves to the target file's default
  export when the indexer marked one, otherwise to the callable with the alias name.
- **Facts.**
  - `api_expose` -> ApiEndpoint node (`endpoint_id(method, route)`, name `"GET /orders/{}"`,
    service of the handler) and `EXPOSES_API`; `static` 0.90 if literal, else `inferred` 0.50.
  - `api_call` -> match the normalized path against exposed endpoints segment by
    segment (`{}` in the declaration matches any segment; methods must agree unless
    either is `ANY`). One endpoint -> `CALLS_API`, always `inferred` (0.60 when the call
    is literal, else 0.50): a URL path is a name match, not proof. Several ->
    `unresolved` edge to a placeholder ApiEndpoint (`unresolved:GET /x`, kind
    `unresolved`). None -> external, counted under `"CALLS_API/external"`, not stored.
  - `db_read` / `db_write` -> DbTable node and `READS_FROM` / `WRITES_TO`; `static` 0.90
    if literal, else `inferred` 0.50.
  - `table_model` -> the class gets `READS_FROM` and `WRITES_TO` to its table (`static`
    0.90). Any symbol with a stored `CALLS` or `REFERENCES` edge to that class gets
    `READS_FROM` the table as `inferred` 0.50 (read or write is not known).
  - `queue_publish` / `queue_consume` -> Queue node and `PUBLISHES` / `CONSUMES`; `static`
    0.90 if literal, else `inferred` 0.50.
  - `inherits` / `implements` / `reference` -> looked up like a bare call but against
    classes and interfaces (any symbol for `reference`): same file `static` 0.95,
    imported `static` 0.90, single repository-wide match `inferred` 0.50, several
    `unresolved` (not stored for `reference`), none external.
- **Derived edges.**
  - `TESTED_BY` symbol -> test, for every stored non-unresolved `CALLS` edge from a test
    symbol to a non-test symbol, with the call's provenance.
  - `DEPENDS_ON` service A -> service B (A != B) when A has a non-unresolved `CALLS`,
    `IMPORTS`, `INHERITS`, `IMPLEMENTS` or `REFERENCES` edge into B; when A calls an
    endpoint B exposes; when A consumes a queue B publishes; when A reads a table B
    writes. Provenance is that of the strongest underlying edge.
- One edge per (from, to, type): the highest confidence wins. Self edges are dropped.
- `stats` counts stored edges by `"TYPE/method"` plus `calls_external` and `imports_external`.

## lge-store

`LadybugStore` implements `GraphStore` (`pub type Store = LadybugStore` is kept).
`LadybugStore::open(path)` creates the schema, records `schema_version` and tries
`LOAD fts`. Schema version 2 follows `docs/PLAN.md` section 4 with these additions:
`File` and `Symbol` keep `service` and `file_path` columns for cheap lookups; staging
tables `CallSite`, `ImportSite` and `FactSite` hold what the indexer extracted;
`UsageRecord` and `HandoffNote` hold the usage log; timestamps are INT64 unix seconds.
Every relationship table except CONTAINS has the columns
`confidence DOUBLE, method STRING, evidence_file STRING, start_line INT64, end_line INT64, commit STRING, created_at INT64`.
An older schema version is migrated by dropping and recreating the graph tables (they
are rebuilt by the next `lge index`); usage and handoff tables are kept.

## lge-usage

Parses Codex token usage (`codex exec --json` event stream and `rollout-*.jsonl` session
files) into `TokenUsage { input_tokens, cached_input_tokens, reasoning_tokens, output_tokens }`,
builds `UsageRecord`s, and aggregates them into a report: per level and overall, tasks,
attempts, escalations, success rate, tokens by class, cached share of input, and cost
from a configurable price table. No database access; callers persist through `GraphStore`.

## lge-filter

Runs the configured or auto-detected lint, type and test commands for a repository and
reduces their output to failures: file, line, rule or test name, message, and a short
excerpt. Adds the `git diff` hunks of the files named in failures. Reports raw and
filtered sizes so the saving can be measured. Never drops a failing test.

## lge-retrieve

- `index_repo(store, root, opts) -> IndexReport` and `reindex_stale(store, root) -> usize`
  (the pipeline above; the only place that calls both the indexer and the resolver).
- `impact(store, ids, opts) -> Impact`: who is affected by a change, walking dependency
  edges backwards (`EdgeType::from_depends_on_to`), bounded by depth and node cap, with
  the path provenance (lowest confidence and weakest method on the path).
- `trace(store, id, direction, depth) -> Trace`: call paths with line references.
- `dependencies(store, id) -> Deps`: upstream and downstream edges with provenance.
- `evidence_pack(store, task, opts) -> EvidencePack`: seeds (BM25 plus exact and prefix
  name matches), expansion, ordering (score, hops, confidence), compression (signature
  and doc first, bodies for the top symbols, merged line ranges), token budget
  (tiktoken-rs, default 6,000).
- `architecture(store, service) -> Architecture`: services, their dependencies, APIs,
  queues and tables.

## lge-router

`route(store, task, cfg) -> Route`. v1 uses `lge_retrieve::impact` from the BM25 seeds:
one file -> low, several files in one service -> medium, more than one service -> top.
Safety rule: unresolved edges or low-confidence (inferred) paths around the seeds raise
the level by one. No seeds -> top. `Level::escalate` gives the retry level.

## lge-session

Builds the prompt as `STABLE_PREFIX` (a versioned constant, byte-identical across
sessions) + evidence pack + latest handoff note + task, starts one session per task at
the routed level (Codex by default, behind a `SessionRunner` trait; `--dry-run` prints
the prompt and command), runs the local checks, retries one level higher on failure,
writes a `UsageRecord` per attempt and a `HandoffNote` at the end.

## lge-mcp

rmcp stdio server exposing `search_code_context`, `trace_execution_flow`,
`analyze_change_impact`, `find_dependencies`, `get_relevant_source` and
`get_architecture_context`. Every result carries the indexed commit. Before answering,
files that changed since they were indexed are re-indexed. The server process owns all
writes, including the file watcher.

## Fixtures

- `tests/fixtures/services/`: the original Python fixture. Do not change it; existing
  tests assert its exact shape.
- `tests/fixtures/polyglot/`: Python and TypeScript services that exercise every edge
  type and every provenance. The names below are fixed so the two halves connect:
  - `orders_api/` (Python, FastAPI style): `routes.py` exposes `POST /orders`
    (`create_order`) and `GET /orders/{order_id}` (`get_order`); `repo.py` runs
    `INSERT INTO orders` and `SELECT ... FROM orders`; `events.py` publishes to the queue
    `order.created` with pika; `models.py` has a SQLAlchemy model `Customer` with
    `__tablename__ = "customers"`; `tests/test_routes.py` tests `create_order`.
  - `web_gateway/` (TypeScript, Express style): `src/routes.ts` exposes
    `GET /api/orders/:id` (`getOrderHandler`) and `POST /api/orders` (`createOrderHandler`);
    `src/ordersClient.ts` calls `axios.post("http://orders-api/orders", body)` (literal)
    and ``axios.get(`${ORDERS_URL}/orders/${id}`)`` (template); `src/views.ts` exports
    `render`.
  - `notifier/` (TypeScript, amqplib style): `src/consumer.ts` consumes `order.created`
    in `handleOrderCreated`, which calls `sendEmail` imported from `./mailer`, reads
    `SELECT email FROM customers`, calls `audit.recordDelivery(...)` on an untyped
    parameter (inferred: `recordDelivery` is defined once, in `src/audit.ts`) and
    `tpl.render(...)` on an untyped parameter (unresolved: `render` exists in
    `web_gateway/src/views.ts` and `notifier/src/templates.ts`); `src/mailer.ts` has
    `class SmtpMailer extends BaseMailer implements Mailer`; `src/consumer.test.ts`
    tests `handleOrderCreated`.
