use super::*;
use crate::BODY_CAP;
use lge_core::{FactSite, ImportSite};

fn parse(path: &str, src: &str) -> ParsedFile {
    parse_typescript(path, "svc", src.as_bytes(), "h").unwrap()
}

fn symbol<'a>(p: &'a ParsedFile, qn: &str) -> &'a SymbolRec {
    p.symbols
        .iter()
        .find(|s| s.qualified_name == qn)
        .unwrap_or_else(|| panic!("no symbol {qn} in {:?}", kinds(p)))
}

/// (qualified name, kind) of every symbol but the module, in source order.
fn kinds(p: &ParsedFile) -> Vec<(&str, &str)> {
    p.symbols
        .iter()
        .filter(|s| s.kind != kind::MODULE)
        .map(|s| (s.qualified_name.as_str(), s.kind.as_str()))
        .collect()
}

fn owner(p: &ParsedFile, id: &str) -> String {
    p.symbols
        .iter()
        .find(|s| s.id == id)
        .map(|s| s.qualified_name.clone())
        .unwrap_or_default()
}

/// Call sites as (caller qualified name, receiver, name), in the order they were found.
fn calls(p: &ParsedFile) -> Vec<(String, String, String)> {
    p.calls
        .iter()
        .map(|c| (owner(p, &c.caller_id), c.receiver.clone(), c.name.clone()))
        .collect()
}

fn call(caller: &str, receiver: &str, name: &str) -> (String, String, String) {
    (caller.to_string(), receiver.to_string(), name.to_string())
}

/// Import sites as (module, name, alias).
fn imports(p: &ParsedFile) -> Vec<(&str, &str, &str)> {
    p.imports
        .iter()
        .map(|i: &ImportSite| (i.module.as_str(), i.name.as_str(), i.alias.as_str()))
        .collect()
}

/// Facts of one kind as (owner qualified name, key, detail, literal), in line order.
fn facts_of(p: &ParsedFile, kind: &str) -> Vec<(String, String, String, bool)> {
    let mut found: Vec<&FactSite> = p.facts.iter().filter(|f| f.kind == kind).collect();
    found.sort_by_key(|f| f.line);
    found
        .into_iter()
        .map(|f| (owner(p, &f.symbol_id), f.key.clone(), f.detail.clone(), f.literal))
        .collect()
}

fn row(owner: &str, key: &str, detail: &str, literal: bool) -> (String, String, String, bool) {
    (owner.to_string(), key.to_string(), detail.to_string(), literal)
}

// ---------------------------------------------------------------------------
// Grammars and module names
// ---------------------------------------------------------------------------

#[test]
fn every_extension_parses_with_its_grammar() {
    // Type annotations need the TypeScript grammar, `<T>` casts rule out TSX,
    // and JSX in a `.js` file needs the JavaScript grammar.
    let typed = "export function f(a: number): number { return <number>a; }\n";
    let tsx = "export const App = (p: { n: number }) => <div>{p.n}</div>;\n";
    let jsx = "export const App = (p) => <div>{p.n}</div>;\n";
    let plain = "export function f(a) { return a; }\n";
    for (path, src, name, lang) in [
        ("web/a.ts", typed, "f", lang::TYPESCRIPT),
        ("web/a.tsx", tsx, "App", lang::TYPESCRIPT),
        ("web/a.js", jsx, "App", lang::JAVASCRIPT),
        ("web/a.jsx", jsx, "App", lang::JAVASCRIPT),
        ("web/a.mjs", plain, "f", lang::JAVASCRIPT),
        ("web/a.cjs", plain, "f", lang::JAVASCRIPT),
    ] {
        let tree = {
            let mut parser = Parser::new();
            parser.set_language(&grammar(path)).unwrap();
            parser.parse(src, None).unwrap()
        };
        assert!(!tree.root_node().has_error(), "{path} does not parse cleanly");
        let p = parse(path, src);
        let file = p.file.as_ref().unwrap();
        assert_eq!(file.lang, lang, "{path}");
        assert_eq!(file.id, file_id(path));
        assert_eq!((file.service.as_str(), file.content_hash.as_str()), ("svc", "h"));
        assert_eq!(
            kinds(&p),
            vec![(format!("web.a.{name}").as_str(), kind::FUNCTION)],
            "{path}"
        );
    }
}

#[test]
fn qualified_names_come_from_the_path() {
    assert_eq!(module_path("web_gateway/src/routes.ts"), "web_gateway/src/routes");
    assert_eq!(
        module_path("notifier/src/consumer.test.ts"),
        "notifier/src/consumer.test"
    );
    assert_eq!(module_path("web/src/index.ts"), "web/src");
    assert_eq!(module_path("web/src/index.test.ts"), "web/src/index.test");
    assert_eq!(module_path("index.js"), "index");
    assert_eq!(module_path("main.mjs"), "main");

    let p = parse("web_gateway/src/routes.ts", "export function getOrderHandler() {}\n");
    let module = &p.symbols[0];
    assert_eq!(
        (
            module.kind.as_str(),
            module.qualified_name.as_str(),
            module.name.as_str()
        ),
        (kind::MODULE, "web_gateway.src.routes", "routes")
    );
    assert_eq!(
        module.id,
        symbol_id("web_gateway/src/routes.ts", "web_gateway.src.routes", kind::MODULE)
    );
    assert_eq!((module.start_line, module.body.as_str()), (1, ""));
    let handler = symbol(&p, "web_gateway.src.routes.getOrderHandler");
    assert_eq!(
        handler.id,
        symbol_id(
            "web_gateway/src/routes.ts",
            "web_gateway.src.routes.getOrderHandler",
            kind::FUNCTION
        )
    );
    assert_eq!(
        (handler.file_path.as_str(), handler.service.as_str()),
        ("web_gateway/src/routes.ts", "svc")
    );

    // `index.*` drops the `index` segment, like `__init__.py`.
    let index = parse("web/src/index.ts", "export class App {}\n");
    assert_eq!(index.symbols[0].qualified_name, "web.src");
    assert_eq!(index.symbols[0].name, "src");
    assert_eq!(kinds(&index), vec![("web.src.App", kind::CLASS)]);
}

// ---------------------------------------------------------------------------
// Symbols
// ---------------------------------------------------------------------------

#[test]
fn functions_declared_or_bound_to_a_variable_are_symbols() {
    let src = r#"
export function plain(a: string): void {}
async function later() {}
function* each() {}
export const arrow = async (x: number) => x + 1;
let expression = function named() {};
var legacy = () => {};
const first = () => 1, second = () => 2;
function overloaded(a: string): void;
function overloaded(a: any) {}
declare function ambient(a: string): void;
const notAFunction = compute(() => 1);
function outer() {
  function inner() {}
  const helper = () => inner();
  return helper;
}
"#;
    let p = parse("svc/fns.ts", src);
    assert_eq!(
        kinds(&p),
        vec![
            ("svc.fns.plain", kind::FUNCTION),
            ("svc.fns.later", kind::FUNCTION),
            ("svc.fns.each", kind::FUNCTION),
            ("svc.fns.arrow", kind::FUNCTION),
            ("svc.fns.expression", kind::FUNCTION),
            ("svc.fns.legacy", kind::FUNCTION),
            ("svc.fns.first", kind::FUNCTION),
            ("svc.fns.second", kind::FUNCTION),
            ("svc.fns.overloaded", kind::FUNCTION),
            ("svc.fns.outer", kind::FUNCTION),
            ("svc.fns.outer.inner", kind::FUNCTION),
            ("svc.fns.outer.helper", kind::FUNCTION),
        ]
    );
    assert_eq!(symbol(&p, "svc.fns.plain").signature, "function plain(a: string): void");
    assert_eq!(
        symbol(&p, "svc.fns.arrow").signature,
        "const arrow = async (x: number) =>"
    );
    assert_eq!(
        symbol(&p, "svc.fns.expression").signature,
        "let expression = function named()"
    );
    // Two declarators in one statement: each symbol is its own declarator.
    assert_eq!(symbol(&p, "svc.fns.second").signature, "second = () =>");
    assert_eq!(symbol(&p, "svc.fns.second").body, "second = () => 2");
    // The implementation, not the overload signature in front of it.
    assert_eq!(
        symbol(&p, "svc.fns.overloaded").signature,
        "function overloaded(a: any)"
    );
    assert_eq!(symbol(&p, "svc.fns.overloaded").start_line, 10);
    // The callback inside `compute(...)` belongs to the module.
    assert!(calls(&p).contains(&call("svc.fns", "", "compute")));
    assert!(calls(&p).contains(&call("svc.fns.outer.helper", "", "inner")));
}

#[test]
fn classes_and_their_members_are_symbols() {
    let src = r#"
export abstract class Repo<T> extends Base<T> implements Store, events.Emitter {
  private items: T[] = [];
  static create = () => new Repo();
  handle = async (req: Request) => { this.save(req); };
  constructor(private readonly db: Db) { super(); }
  get size(): number { return this.items.length; }
  set size(n: number) {}
  static of(db: Db): Repo<unknown> { return new Repo(db); }
  async *pages() {}
  abstract load(id: string): T;
  find(id: string): T;
  find(id: number): T;
  find(id: any) { return this.db.get(id); }
  #secret() {}
  [Symbol.iterator]() { return walk(this); }
}
const Anonymous = class extends Repo {};
"#;
    let p = parse("svc/repo.ts", src);
    assert_eq!(
        kinds(&p),
        vec![
            ("svc.repo.Repo", kind::CLASS),
            ("svc.repo.Repo.create", kind::METHOD),
            ("svc.repo.Repo.handle", kind::METHOD),
            ("svc.repo.Repo.constructor", kind::METHOD),
            // The getter and the setter share one name, and so one symbol.
            ("svc.repo.Repo.size", kind::METHOD),
            ("svc.repo.Repo.of", kind::METHOD),
            ("svc.repo.Repo.pages", kind::METHOD),
            ("svc.repo.Repo.load", kind::METHOD),
            ("svc.repo.Repo.find", kind::METHOD),
            ("svc.repo.Repo.#secret", kind::METHOD),
            ("svc.repo.Anonymous", kind::CLASS),
        ]
    );
    let class = symbol(&p, "svc.repo.Repo");
    assert_eq!(
        class.signature,
        "abstract class Repo<T> extends Base<T> implements Store, events.Emitter"
    );
    assert_eq!((class.start_line, class.end_line), (2, 17));
    assert!(class.body.starts_with("export abstract class Repo<T>"));
    assert_eq!(symbol(&p, "svc.repo.Repo.size").signature, "get size(): number");
    assert_eq!(
        symbol(&p, "svc.repo.Repo.of").signature,
        "static of(db: Db): Repo<unknown>"
    );
    assert_eq!(
        symbol(&p, "svc.repo.Repo.handle").signature,
        "handle = async (req: Request) =>"
    );
    assert_eq!(
        symbol(&p, "svc.repo.Repo.load").signature,
        "abstract load(id: string): T"
    );
    assert_eq!(symbol(&p, "svc.repo.Repo.find").signature, "find(id: any)");
    assert_eq!(
        symbol(&p, "svc.repo.Repo.constructor").signature,
        "constructor(private readonly db: Db)"
    );
    assert_eq!(
        symbol(&p, "svc.repo.Anonymous").signature,
        "const Anonymous = class extends Repo"
    );

    let found = calls(&p);
    assert!(found.contains(&call("svc.repo.Repo.handle", "self", "save")));
    assert!(found.contains(&call("svc.repo.Repo.find", "this.db", "get")));
    // A member with a computed name is not a symbol: its code is the class's.
    assert!(found.contains(&call("svc.repo.Repo", "", "walk")));
    assert_eq!(
        facts_of(&p, fact::INHERITS),
        vec![
            row("svc.repo.Repo", "Base", "", true),
            row("svc.repo.Anonymous", "Repo", "", true),
        ]
    );
    assert_eq!(
        facts_of(&p, fact::IMPLEMENTS),
        vec![
            row("svc.repo.Repo", "Store", "", true),
            row("svc.repo.Repo", "events.Emitter", "", true),
        ]
    );
}

#[test]
fn interfaces_type_aliases_and_enums_are_symbols() {
    let src = r#"
export interface Mailer extends Sender, events.Closable<Error> {
  send(to: string, body: string): Promise<void>;
  readonly from: string;
}
type Handlers = { open(): void; name: string };
export type Id = string | number;
enum Color { Red = "red", Green = "green" }
export const enum Level { Low, High }
namespace Shapes {
  export interface Square {}
  export function area(s: Square): number { return 1; }
}
"#;
    let p = parse("svc/types.ts", src);
    assert_eq!(
        kinds(&p),
        vec![
            ("svc.types.Mailer", kind::INTERFACE),
            ("svc.types.Mailer.send", kind::METHOD),
            ("svc.types.Handlers", kind::TYPE),
            ("svc.types.Handlers.open", kind::METHOD),
            ("svc.types.Id", kind::TYPE),
            ("svc.types.Color", kind::ENUM),
            ("svc.types.Level", kind::ENUM),
            // A namespace is part of the names inside it.
            ("svc.types.Shapes.Square", kind::INTERFACE),
            ("svc.types.Shapes.area", kind::FUNCTION),
        ]
    );
    assert_eq!(
        symbol(&p, "svc.types.Mailer").signature,
        "interface Mailer extends Sender, events.Closable<Error>"
    );
    assert_eq!(
        symbol(&p, "svc.types.Mailer.send").signature,
        "send(to: string, body: string): Promise<void>"
    );
    assert_eq!(symbol(&p, "svc.types.Id").signature, "type Id = string | number");
    assert_eq!(symbol(&p, "svc.types.Id").body, "export type Id = string | number;");
    assert_eq!(symbol(&p, "svc.types.Color").signature, "enum Color");
    assert_eq!(symbol(&p, "svc.types.Level").signature, "const enum Level");
    // An interface extends its bases, without their type arguments.
    assert_eq!(
        facts_of(&p, fact::INHERITS),
        vec![
            row("svc.types.Mailer", "Sender", "", true),
            row("svc.types.Mailer", "events.Closable", "", true),
        ]
    );
    // String members of an enum are not SQL, and nothing here is a call.
    assert_eq!(p.calls, vec![]);
}

#[test]
fn functions_in_an_object_bound_to_a_name_are_symbols() {
    let src = r#"
export const handlers = {
  list() { return fetchAll(); },
  create: async (req) => save(req),
  remove: function (id) {},
  nested: { deep: () => 1 },
  ["computed"]: () => skipped(),
  limit: 10,
} as const;
register({ onReady: () => started() });
"#;
    let p = parse("svc/handlers.ts", src);
    assert_eq!(
        kinds(&p),
        vec![
            ("svc.handlers.handlers.list", kind::FUNCTION),
            ("svc.handlers.handlers.create", kind::FUNCTION),
            ("svc.handlers.handlers.remove", kind::FUNCTION),
            ("svc.handlers.handlers.nested.deep", kind::FUNCTION),
        ]
    );
    assert_eq!(
        symbol(&p, "svc.handlers.handlers.create").signature,
        "create: async (req) =>"
    );
    assert_eq!(
        symbol(&p, "svc.handlers.handlers.list").body,
        "list() { return fetchAll(); }"
    );
    assert_eq!(
        calls(&p),
        vec![
            call("svc.handlers.handlers.list", "", "fetchAll"),
            call("svc.handlers.handlers.create", "", "save"),
            // A computed key and an object passed as an argument give no symbol:
            // their callbacks run for the enclosing symbol.
            call("svc.handlers", "", "skipped"),
            call("svc.handlers", "", "register"),
            call("svc.handlers", "", "started"),
        ]
    );
}

#[test]
fn signatures_docs_bodies_and_lines_describe_the_definition() {
    let many: String = (0..60).map(|i| format!("argument{i}: string,\n  ")).collect();
    let long_doc = "d".repeat(300);
    let long_body = "é".repeat(BODY_CAP);
    let src = format!(
        r#"/**
 * Create an order.
 * @param body the order
 */
export   async function
  createOrder(
    body: NewOrder,
  ): Promise<Order> {{
  return save(body);
}}

// Load one order.
// Returns null when it does not exist.
export const fetchOrder = (id: string) => load(id);

const x = 1; // not the doc of what follows
function undocumented() {{}}

// eslint-disable-next-line no-console
/** @deprecated */
function tagged() {{}}

// Detached by a blank line.

function detached() {{}}

/** {long_doc} */
function wide({many}) {{}}

function long() {{
  return "{long_body}";
}}

class Box {{
  /** How many things are inside. */
  @observed()
  count(): number {{ return 1; }}
}}
"#
    );
    let p = parse("svc/orders.ts", &src);

    let create = symbol(&p, "svc.orders.createOrder");
    assert_eq!(
        create.signature,
        "async function createOrder( body: NewOrder, ): Promise<Order>"
    );
    assert_eq!(create.doc, "Create an order.");
    assert_eq!((create.start_line, create.end_line), (5, 10));
    assert!(create.body.starts_with("export   async function\n  createOrder("));
    assert!(create.body.ends_with("return save(body);\n}"));

    let fetch = symbol(&p, "svc.orders.fetchOrder");
    assert_eq!(fetch.doc, "Load one order.");
    assert_eq!(fetch.signature, "const fetchOrder = (id: string) =>");
    assert_eq!(fetch.body, "export const fetchOrder = (id: string) => load(id);");
    assert_eq!((fetch.start_line, fetch.end_line), (14, 14));

    assert_eq!(symbol(&p, "svc.orders.undocumented").doc, "");
    assert_eq!(
        symbol(&p, "svc.orders.tagged").doc,
        "",
        "tags and tool directives say nothing"
    );
    assert_eq!(symbol(&p, "svc.orders.detached").doc, "");

    let wide = symbol(&p, "svc.orders.wide");
    assert_eq!(wide.doc.chars().count(), 200);
    assert_eq!(wide.signature.chars().count(), 240);
    assert!(wide
        .signature
        .starts_with("function wide(argument0: string, argument1: string,"));

    let long = symbol(&p, "svc.orders.long");
    assert!(long.body.starts_with("function long() {\n  return \"é"));
    assert!(
        long.body.len() <= BODY_CAP && long.body.len() >= BODY_CAP - 1,
        "{}",
        long.body.len()
    );
    assert!(long.body.ends_with('é'), "cut on a char boundary");

    // A decorated method starts at its decorator: lines, body and doc; not the signature.
    let count = symbol(&p, "svc.orders.Box.count");
    assert_eq!(count.doc, "How many things are inside.");
    assert_eq!(count.signature, "count(): number");
    assert!(count.body.starts_with("@observed()\n  count(): number"));
    assert_eq!(count.end_line - count.start_line, 1);
}

#[test]
fn the_comment_that_opens_a_file_documents_the_module() {
    let doc = |src: &str| parse("svc/m.ts", src).symbols[0].doc.clone();
    assert_eq!(
        doc("// Order routes.\n// More.\nimport x from \"./x\";\n"),
        "Order routes."
    );
    assert_eq!(
        doc("#!/usr/bin/env node\n/**\n * CLI entry point.\n */\n\nmain();\n"),
        "CLI entry point."
    );
    assert_eq!(doc("/* Shared types. */\n\nexport interface A {}\n"), "Shared types.");
    // Directly on top of a definition it documents that definition.
    let p = parse("svc/m.ts", "/** Adds. */\nexport function add() {}\n");
    assert_eq!(p.symbols[0].doc, "");
    assert_eq!(symbol(&p, "svc.m.add").doc, "Adds.");
    assert_eq!(doc("import x from \"./x\";\n// not at the top\n"), "");
    assert_eq!(doc(""), "");
}

#[test]
fn test_files_and_test_blocks_are_tests() {
    for path in [
        "web/src/orders.test.ts",
        "web/src/orders.spec.tsx",
        "web/__tests__/orders.ts",
        "web/tests/orders.js",
        "tests/orders.js",
        "__tests__/orders.jsx",
    ] {
        assert!(is_test_file(path), "{path}");
        let p = parse(path, "export function helper() {}\n");
        assert!(p.symbols.iter().all(|s| s.is_test), "{path}");
    }
    for path in [
        "web/src/orders.ts",
        "web/src/contest.ts",
        "web/latest/a.js",
        "web/src/test.ts",
    ] {
        assert!(!is_test_file(path), "{path}");
    }

    let src = r#"
import { createOrder } from "./orders";

describe("orders: API", () => {
  const fixture = build();
  function makeOrder() { return {}; }

  describe("createOrder()", () => {
    it("stores the order & returns its id", async () => {
      const id = await createOrder(makeOrder());
      expect(id).toBe(1);
    });
    it.each([1, 2])("rejects total %d", (total) => { reject(total); });
    test.skip(`handles ${fixture}`, function () { skipped(); }, 500);
    it.todo("later");
  });
});

test("top level", () => { standalone(); });
export function notATest() {}
"#;
    // The file name says nothing: the blocks do.
    let p = parse("web/src/checks.ts", src);
    assert_eq!(
        kinds(&p),
        vec![
            ("web.src.checks.orders_API.makeOrder", kind::FUNCTION),
            (
                "web.src.checks.orders_API.createOrder.stores_the_order_returns_its_id",
                kind::FUNCTION
            ),
            ("web.src.checks.orders_API.createOrder.rejects_total_d", kind::FUNCTION),
            ("web.src.checks.orders_API.createOrder.handles_fixture", kind::FUNCTION),
            ("web.src.checks.top_level", kind::FUNCTION),
            ("web.src.checks.notATest", kind::FUNCTION),
        ]
    );
    let is_test = |qn: &str| symbol(&p, qn).is_test;
    assert!(
        is_test("web.src.checks.orders_API.makeOrder"),
        "declared inside a describe block"
    );
    assert!(is_test("web.src.checks.top_level"));
    assert!(!is_test("web.src.checks.notATest"));
    assert!(!is_test("web.src.checks"));

    let case = symbol(
        &p,
        "web.src.checks.orders_API.createOrder.stores_the_order_returns_its_id",
    );
    assert_eq!(case.name, "stores_the_order_returns_its_id");
    assert_eq!(case.signature, "it(\"stores the order & returns its id\", async () =>");
    assert_eq!((case.start_line, case.end_line), (9, 12));
    assert!(case.body.starts_with("it(\"stores the order") && case.body.ends_with("})"));

    assert_eq!(
        calls(&p),
        vec![
            // Code of a describe block that is in no test runs for the module.
            call("web.src.checks", "", "build"),
            call(
                "web.src.checks.orders_API.createOrder.stores_the_order_returns_its_id",
                "",
                "createOrder"
            ),
            call(
                "web.src.checks.orders_API.createOrder.stores_the_order_returns_its_id",
                "",
                "makeOrder"
            ),
            call(
                "web.src.checks.orders_API.createOrder.stores_the_order_returns_its_id",
                "expect()",
                "toBe"
            ),
            call(
                "web.src.checks.orders_API.createOrder.stores_the_order_returns_its_id",
                "",
                "expect"
            ),
            call("web.src.checks.orders_API.createOrder.rejects_total_d", "", "reject"),
            call("web.src.checks.orders_API.createOrder.handles_fixture", "", "skipped"),
            call("web.src.checks.top_level", "", "standalone"),
        ],
        "describe, it and test are the framework's and are not call sites"
    );
}

#[test]
fn test_titles_become_names() {
    assert_eq!(sanitise("sends an email"), "sends_an_email");
    assert_eq!(sanitise("  GET /orders/:id -> 404!  "), "GET_orders_id_404");
    assert_eq!(sanitise("snake_case stays"), "snake_case_stays");
    assert_eq!(sanitise("a.b.c"), "a_b_c", "a dot would read as a scope");
    assert_eq!(sanitise("été 1"), "été_1");
    assert_eq!(sanitise("?!"), "test");
    assert_eq!(sanitise(""), "test");
    assert_eq!(sanitise(&"word ".repeat(40)).chars().count(), 79);
}

#[test]
fn javascript_symbols_include_commonjs_exports_and_components() {
    let src = r#"
// Order widgets.
const React = require("react");

class OrderList extends React.Component {
  state = { open: false };
  toggle = () => { this.setState({ open: true }); };
  render() { return <List onToggle={this.toggle} />; }
}

function OrderRow({ order }) {
  return <tr><Cell value={order.id} /></tr>;
}

exports.format = (order) => String(order.id);
module.exports.parse = function (text) { return JSON.parse(text); };
module.exports = { OrderList, load() { return fetchOrders(); }, save: async (o) => store(o) };
"#;
    let p = parse("web/src/widgets.jsx", src);
    assert_eq!(p.symbols[0].doc, "Order widgets.");
    assert_eq!(
        kinds(&p),
        vec![
            ("web.src.widgets.OrderList", kind::CLASS),
            ("web.src.widgets.OrderList.toggle", kind::METHOD),
            ("web.src.widgets.OrderList.render", kind::METHOD),
            ("web.src.widgets.OrderRow", kind::FUNCTION),
            ("web.src.widgets.format", kind::FUNCTION),
            ("web.src.widgets.parse", kind::FUNCTION),
            ("web.src.widgets.load", kind::FUNCTION),
            ("web.src.widgets.save", kind::FUNCTION),
        ]
    );
    assert_eq!(
        symbol(&p, "web.src.widgets.format").signature,
        "exports.format = (order) =>"
    );
    assert_eq!(
        symbol(&p, "web.src.widgets.OrderList").signature,
        "class OrderList extends React.Component"
    );
    assert_eq!(
        symbol(&p, "web.src.widgets.OrderRow").signature,
        "function OrderRow({ order })"
    );
    assert_eq!(imports(&p), vec![("react", "*", "React")]);
    assert_eq!(
        facts_of(&p, fact::INHERITS),
        vec![row("web.src.widgets.OrderList", "React.Component", "", true)]
    );
    assert_eq!(
        facts_of(&p, fact::REFERENCE),
        vec![
            row("web.src.widgets.OrderList.render", "List", "", true),
            row("web.src.widgets.OrderList.render", "this.toggle", "", true),
            // `<tr>` is an element, and `order` is a parameter.
            row("web.src.widgets.OrderRow", "Cell", "", true),
        ]
    );
    assert!(calls(&p).contains(&call("web.src.widgets.OrderList.toggle", "self", "setState")));
    assert!(calls(&p).contains(&call("web.src.widgets.load", "", "fetchOrders")));
    assert!(calls(&p).contains(&call("web.src.widgets.save", "", "store")));
}

#[test]
fn react_components_in_tsx() {
    let src = r#"
import { useEffect, useState } from "react";
import { Button } from "./Button";
import * as Layout from "./Layout";

/** Counts clicks. */
export default function Counter({ start }: Props) {
  const [count, setCount] = useState<number>(start);
  const increment = () => setCount(count + 1);
  useEffect(() => { track(count); }, [count]);
  return (
    <Layout.Page title="Counter">
      <Button onClick={increment} label={count} />
      <span>{format(count)}</span>
    </Layout.Page>
  );
}
"#;
    let p = parse("web/src/Counter.tsx", src);
    let counter = "web.src.Counter.Counter";
    assert_eq!(
        kinds(&p),
        vec![
            (counter, kind::FUNCTION),
            ("web.src.Counter.Counter.increment", kind::FUNCTION)
        ]
    );
    assert_eq!(symbol(&p, counter).doc, "Counts clicks.");
    assert_eq!(symbol(&p, counter).signature, "function Counter({ start }: Props)");
    assert_eq!(
        calls(&p),
        vec![
            call(counter, "", "useState"),
            call("web.src.Counter.Counter.increment", "", "setCount"),
            // The effect callback has no name: what it calls, the component calls.
            call(counter, "", "useEffect"),
            call(counter, "", "track"),
            call(counter, "", "format"),
        ]
    );
    assert_eq!(
        facts_of(&p, fact::REFERENCE),
        vec![
            // Components that are rendered and handlers that are passed on.
            // `count` and `start` are variables, `span` is an element.
            row(counter, "Layout.Page", "", true),
            row(counter, "Button", "", true),
            row(counter, "increment", "", true),
        ]
    );
    assert_eq!(
        facts_of(&p, fact::DEFAULT_EXPORT),
        vec![row(counter, "Counter", "", true)]
    );
}

// ---------------------------------------------------------------------------
// Call sites
// ---------------------------------------------------------------------------

#[test]
fn call_sites_keep_the_name_and_the_receiver() {
    let src = r#"
class Service {
  async run(repo: Repo, items: Item[]) {
    plain();
    repo.save(1);
    this.validate();
    this.repo.find(2);
    new Mailer();
    new Mailer(host).send("x");
    Factory(1, 2).build();
    new ns.Client().connect();
    repo?.maybe();
    repo?.inner?.deep();
    optional?.(3);
    repo!.sure();
    await awaited();
    const value = await repo.load();
    items.map((item) => transform(item)).filter(Boolean);
    sql`SELECT 1`;
    super.run();
    generic<string>(4);
    table[0].row();
    (await pick()).use();
    handlers[name]();
    require("./late");
  }
}
"#;
    let p = parse("svc/service.ts", src);
    let run = "svc.service.Service.run";
    assert_eq!(
        calls(&p),
        vec![
            call(run, "", "plain"),
            call(run, "repo", "save"),
            call(run, "self", "validate"),
            call(run, "this.repo", "find"),
            // `new Foo()` is a bare call to `Foo`.
            call(run, "", "Mailer"),
            call(run, "Mailer()", "send"),
            call(run, "", "Mailer"),
            call(run, "Factory()", "build"),
            call(run, "", "Factory"),
            call(run, "ns.Client()", "connect"),
            call(run, "ns", "Client"),
            // Optional chaining and `!` do not change who is called.
            call(run, "repo", "maybe"),
            call(run, "repo.inner", "deep"),
            call(run, "", "optional"),
            call(run, "repo", "sure"),
            call(run, "", "awaited"),
            call(run, "repo", "load"),
            call(run, "items.map()", "filter"),
            call(run, "items", "map"),
            // A callback's calls belong to the enclosing named symbol.
            call(run, "", "transform"),
            call(run, "", "sql"),
            call(run, "super", "run"),
            call(run, "", "generic"),
            call(run, "table[0]", "row"),
            call(run, "(await pick())", "use"),
            call(run, "", "pick"),
        ],
        "a computed callee and `require` are not call sites"
    );
    let save = p.calls.iter().find(|c| c.name == "save").unwrap();
    assert_eq!((save.line, save.file_path.as_str()), (5, "svc/service.ts"));
    assert_eq!(save.caller_id, symbol(&p, run).id);
    // The same call twice on one line is two sites.
    let twice = parse("svc/twice.ts", "f(); f();\n");
    assert_eq!(twice.calls.len(), 2);
    assert_ne!(twice.calls[0].id, twice.calls[1].id);
}

#[test]
fn long_receivers_are_cut() {
    let chain = format!("builder{}.done();\n", ".step(1)".repeat(30));
    let p = parse("svc/chain.js", &chain);
    let done = p.calls.iter().find(|c| c.name == "done").unwrap();
    assert_eq!(done.receiver.chars().count(), 80);
    assert!(done.receiver.starts_with("builder.step(1).step(1)"));
}

#[test]
fn javascript_call_sites_and_decorators() {
    let src = r#"
@Component({ selector: "x" })
class Widget {
  @Input() name;
  @track
  @Debounce(50)
  refresh() { this.load().then((rows) => render(rows)); }
}
module.exports = function start(app) { app.listen(3000); boot(); };
"#;
    let p = parse("web/widget.js", src);
    assert_eq!(
        calls(&p),
        vec![
            // A decorator is called by the scope around what it decorates.
            call("web.widget", "", "Component"),
            call("web.widget.Widget", "", "Input"),
            call("web.widget.Widget", "", "Debounce"),
            call("web.widget.Widget.refresh", "this.load()", "then"),
            call("web.widget.Widget.refresh", "self", "load"),
            call("web.widget.Widget.refresh", "", "render"),
            call("web.widget.start", "app", "listen"),
            call("web.widget.start", "", "boot"),
        ]
    );
    // `@track` without a call is a reference from the method it decorates.
    assert_eq!(
        facts_of(&p, fact::REFERENCE),
        vec![row("web.widget.Widget.refresh", "track", "", true)]
    );
    assert_eq!(
        facts_of(&p, fact::DEFAULT_EXPORT),
        vec![row("web.widget.start", "start", "", true)]
    );
}

// ---------------------------------------------------------------------------
// Import sites
// ---------------------------------------------------------------------------

#[test]
fn esm_imports_record_what_each_local_name_stands_for() {
    let src = r#"
import express from "express";
import React, { useState, useEffect as onMount } from "react";
import * as path from "node:path";
import "./polyfills";
import type { Order } from "./types";
import { type Customer, loadCustomer } from "../shared/customers";
import legacy = require("./legacy");
import {} from "./nothing";
"#;
    let p = parse("web/src/app.ts", src);
    assert_eq!(
        imports(&p),
        vec![
            ("express", "default", "express"),
            ("react", "default", "React"),
            ("react", "useState", "useState"),
            ("react", "useEffect", "onMount"),
            ("node:path", "*", "path"),
            // `import "./polyfills"` binds nothing and is skipped.
            ("./types", "Order", "Order"),
            ("../shared/customers", "Customer", "Customer"),
            ("../shared/customers", "loadCustomer", "loadCustomer"),
            ("./legacy", "*", "legacy"),
        ]
    );
    let site = &p.imports[0];
    assert_eq!(site.file_path, "web/src/app.ts");
    assert_eq!(
        site.id,
        lge_core::stable_id(&["import", "web/src/app.ts", "express", "default", "express"])
    );
    assert_eq!(p.calls, vec![], "an import is not a call");
}

#[test]
fn reexports_are_imports_of_the_exported_names() {
    let src = r#"
export { createOrder as create, fetchOrder } from "./orders";
export * from "./types";
export * as views from "./views";
export { default } from "./App";
export { default as Button } from "./Button";
export type { Order } from "./types";
export { local };
"#;
    let p = parse("web/src/index.ts", src);
    assert_eq!(
        imports(&p),
        vec![
            ("./orders", "createOrder", "create"),
            ("./orders", "fetchOrder", "fetchOrder"),
            ("./types", "*", ""),
            ("./views", "*", "views"),
            ("./App", "default", "default"),
            ("./Button", "default", "Button"),
            ("./types", "Order", "Order"),
        ]
    );
    assert_eq!(kinds(&p), vec![]);
    assert_eq!(p.facts, vec![]);
}

#[test]
fn require_and_dynamic_import_are_imports() {
    let src = r#"
const fs = require("fs");
const { readFile, writeFile: write, ...rest } = require("fs/promises");
const helper = require("./util").helper;
var settings = require(`./settings`);
const unknown = require(name);
require("./register");

async function load() {
  const page = await import("./pages/Home");
  const { default: Admin, guard } = await import("./pages/Admin");
  import("./unbound");
  return page;
}
"#;
    let p = parse("web/src/boot.js", src);
    assert_eq!(
        imports(&p),
        vec![
            ("fs", "*", "fs"),
            ("fs/promises", "readFile", "readFile"),
            ("fs/promises", "writeFile", "write"),
            ("./util", "helper", "helper"),
            ("./settings", "*", "settings"),
            ("./pages/Home", "*", "page"),
            ("./pages/Admin", "default", "Admin"),
            ("./pages/Admin", "guard", "guard"),
        ]
    );
    // Loading a module is never a call site, even when the module is not known.
    assert_eq!(calls(&p), vec![]);
    // ES modules in a JavaScript file read the same as in TypeScript.
    let esm = parse(
        "web/src/esm.mjs",
        "import app, { start as boot } from \"./app.js\";\nimport * as config from \"./config.js\";\nexport * from \"./types.js\";\nexport { boot as default };\n",
    );
    assert_eq!(
        imports(&esm),
        vec![
            ("./app.js", "default", "app"),
            ("./app.js", "start", "boot"),
            ("./config.js", "*", "config"),
            ("./types.js", "*", ""),
        ]
    );
    // An imported name can be a function; a required one is not a variable either.
    let p = parse(
        "web/src/use.js",
        "const { handler } = require(\"./handlers\");\nconst local = 1;\nregister(handler, local);\n",
    );
    assert_eq!(
        facts_of(&p, fact::REFERENCE),
        vec![row("web.src.use", "handler", "", true)]
    );
}

// ---------------------------------------------------------------------------
// Fact sites
// ---------------------------------------------------------------------------

#[test]
fn default_exports_mark_the_exported_symbol() {
    let default_of = |path: &str, src: &str| facts_of(&parse(path, src), fact::DEFAULT_EXPORT);
    assert_eq!(
        default_of("svc/a.ts", "export default function main() {}\n"),
        vec![row("svc.a.main", "main", "", true)]
    );
    assert_eq!(
        default_of("svc/a.ts", "@Injectable()\nexport default class Service {}\n"),
        vec![row("svc.a.Service", "Service", "", true)]
    );
    // Named further down, or further up.
    assert_eq!(
        default_of("svc/a.ts", "export default later;\nfunction later() {}\n"),
        vec![row("svc.a.later", "later", "", true)]
    );
    assert_eq!(
        default_of(
            "svc/a.ts",
            "const App = () => null;\nexport { App as default, other };\n"
        ),
        vec![row("svc.a.App", "App", "", true)]
    );
    assert_eq!(
        default_of("svc/a.ts", "class Legacy {}\nexport = Legacy;\n"),
        vec![row("svc.a.Legacy", "Legacy", "", true)]
    );
    // A class and the interface merged with it: the value is what is exported.
    assert_eq!(
        default_of("svc/a.ts", "interface Box {}\nclass Box {}\nexport default Box;\n"),
        vec![row("svc.a.Box", "Box", "", true)]
    );
    // Nothing to name: the export itself becomes the symbol `default`.
    let p = parse("svc/a.tsx", "export default () => <App />;\n");
    assert_eq!(kinds(&p), vec![("svc.a.default", kind::FUNCTION)]);
    assert_eq!(
        facts_of(&p, fact::DEFAULT_EXPORT),
        vec![row("svc.a.default", "default", "", true)]
    );
    let p = parse("svc/a.js", "export default class {}\n");
    assert_eq!(kinds(&p), vec![("svc.a.default", kind::CLASS)]);
    assert_eq!(
        facts_of(&p, fact::DEFAULT_EXPORT),
        vec![row("svc.a.default", "default", "", true)]
    );
    assert_eq!(
        default_of("svc/a.js", "function main() {}\nmodule.exports = main;\n"),
        vec![row("svc.a.main", "main", "", true)]
    );
    // A variable, an import or an object has no symbol to mark.
    assert_eq!(
        default_of("svc/a.ts", "const router = Router();\nexport default router;\n"),
        vec![]
    );
    assert_eq!(
        default_of("svc/a.ts", "import x from \"./x\";\nexport default x;\n"),
        vec![]
    );
    assert_eq!(default_of("svc/a.ts", "export default { name: \"x\" };\n"), vec![]);
    assert_eq!(default_of("svc/a.ts", "export function named() {}\n"), vec![]);

    let p = parse("svc/a.ts", "\nexport default function main() {\n}\n");
    let f = &p.facts[0];
    assert_eq!((f.line, f.end_line), (2, 2));
    assert_eq!(
        f.id,
        lge_core::stable_id(&["fact", "svc/a.ts", &f.symbol_id, "default_export", "main", "", "2"])
    );
}

#[test]
fn express_routes_belong_to_their_handlers() {
    let src = r#"
import express, { Router } from "express";
import { importedHandler } from "./handlers";
import * as controller from "./controller";
import { auth } from "./auth";

const app = express();
const orders = Router();
const BASE = "/v1";

app.get("/early", declaredBelow);
function declaredBelow(req, res) {}
const arrowHandler = async (req, res) => {};
app.post("/arrow", auth, arrowHandler);
app.put("/imported", importedHandler);
app.delete("/inline", (req, res) => { res.json(load()); });
app.all("/any", controller.anything);
orders.route("/chain").get(declaredBelow).post(arrowHandler);
orders.patch(`${BASE}/items/:id`, declaredBelow);
app.get("view engine");
app.use("/mounted", orders);
cache.get("/not-a-route", declaredBelow);

export function register(server) {
  function local(req, res) {}
  server.head("/local", local);
  server.options("/shadowed", (declaredBelow) => declaredBelow);
}
"#;
    let p = parse("web/src/routes.ts", src);
    let m = "web.src.routes";
    assert_eq!(
        facts_of(&p, fact::API_EXPOSE),
        vec![
            // A function is hoisted: the route can be written above it.
            row("web.src.routes.declaredBelow", "/early", "GET", true),
            row("web.src.routes.arrowHandler", "/arrow", "POST", true),
            // An imported handler and an inline one leave the route with the enclosing symbol.
            row(m, "/imported", "PUT", true),
            row(m, "/inline", "DELETE", true),
            row(m, "/any", "ANY", true),
            row("web.src.routes.arrowHandler", "/chain", "POST", true),
            row("web.src.routes.declaredBelow", "/chain", "GET", true),
            // A constant is read through, and what is read through is not a literal.
            row("web.src.routes.declaredBelow", "/v1/items/:id", "PATCH", false),
            row("web.src.routes.register.local", "/local", "HEAD", true),
            row("web.src.routes.register", "/shadowed", "OPTIONS", true),
        ]
    );
    let references = facts_of(&p, fact::REFERENCE);
    for name in [
        "importedHandler",
        "controller.anything",
        "auth",
        "declaredBelow",
        "arrowHandler",
    ] {
        assert!(references.contains(&row(m, name, "", true)), "{name}: {references:?}");
    }
    assert!(references.contains(&row("web.src.routes.register", "local", "", true)));
    // `orders` and `app` are variables, not symbols.
    assert!(!references.iter().any(|r| r.1 == "orders" || r.1 == "app"));
    // The inline handler's calls are the module's.
    assert!(calls(&p).contains(&call(m, "", "load")));
    assert_eq!(facts_of(&p, fact::API_CALL), vec![]);

    let early = p.facts.iter().find(|f| f.key == "/early").unwrap();
    assert_eq!((early.line, early.end_line), (11, 11));
    assert_eq!(early.symbol_id, symbol(&p, "web.src.routes.declaredBelow").id);
    assert_eq!(
        early.id,
        lge_core::stable_id(&[
            "fact",
            "web/src/routes.ts",
            &early.symbol_id,
            "api_expose",
            "/early",
            "GET",
            "11"
        ])
    );
}

#[test]
fn express_routes_in_javascript() {
    let src = r#"
const express = require("express");
const { listOrders } = require("./orders");
const router = express.Router();

async function getOrder(req, res) { res.json(await find(req.params.id)); }

router.get("/orders/:id", requireAuth, getOrder);
router.get("/orders", listOrders);
router.post("/orders", async function (req, res) { res.status(201).end(); });

module.exports = router;
"#;
    let p = parse("api/routes/orders.js", src);
    assert_eq!(
        facts_of(&p, fact::API_EXPOSE),
        vec![
            row("api.routes.orders.getOrder", "/orders/:id", "GET", true),
            row("api.routes.orders", "/orders", "GET", true),
            row("api.routes.orders", "/orders", "POST", true),
        ]
    );
    assert_eq!(
        facts_of(&p, fact::REFERENCE),
        vec![
            row("api.routes.orders", "requireAuth", "", true),
            row("api.routes.orders", "getOrder", "", true),
            row("api.routes.orders", "listOrders", "", true),
        ]
    );
    assert_eq!(facts_of(&p, fact::DEFAULT_EXPORT), vec![], "a router is a variable");
}

#[test]
fn nest_controllers_expose_their_decorated_methods() {
    let src = r#"
import { Controller, Get, Post, Put, Patch, Delete, All } from "@nestjs/common";

@Controller("orders")
export class OrdersController {
  @Get()
  list() {}
  @Get(":id")
  @UseGuards(AuthGuard)
  one(@Param("id") id: string) {}
  @Post("/bulk")
  bulk() {}
  @Put(ROUTE)
  replace() {}
  @Patch(`${PREFIX}/x`)
  patch() {}
  @Delete(":id")
  remove() {}
  @All("*")
  any() {}
  plain() {}
}

@Controller({ path: "/admin/", version: "1" })
class AdminController { @Get("users") users() {} }

@Controller()
class RootController { @Get() index() {} @Get("health") health() {} }

@Controller(BASE)
class DynamicController { @Get("x") x() {} }

class NotAController { @Get("ignored") ignored() {} }
"#;
    let p = parse("api/src/orders.controller.ts", src);
    let c = "api.src.orders.controller";
    assert_eq!(
        facts_of(&p, fact::API_EXPOSE),
        vec![
            row(&format!("{c}.OrdersController.list"), "/orders", "GET", true),
            row(&format!("{c}.OrdersController.one"), "/orders/:id", "GET", true),
            row(&format!("{c}.OrdersController.bulk"), "/orders/bulk", "POST", true),
            row(
                &format!("{c}.OrdersController.replace"),
                "/orders/${ROUTE}",
                "PUT",
                false
            ),
            row(
                &format!("{c}.OrdersController.patch"),
                "/orders/${PREFIX}/x",
                "PATCH",
                false
            ),
            row(&format!("{c}.OrdersController.remove"), "/orders/:id", "DELETE", true),
            row(&format!("{c}.OrdersController.any"), "/orders/*", "ANY", true),
            row(&format!("{c}.AdminController.users"), "/admin/users", "GET", true),
            row(&format!("{c}.RootController.index"), "/", "GET", true),
            row(&format!("{c}.RootController.health"), "/health", "GET", true),
            row(&format!("{c}.DynamicController.x"), "/${BASE}/x", "GET", false),
        ]
    );
    // The decorated method starts at its first decorator.
    let one = symbol(&p, &format!("{c}.OrdersController.one"));
    assert_eq!((one.start_line, one.end_line), (8, 10));
    assert_eq!(one.signature, "one(@Param(\"id\") id: string)");
    // The class calls the decorators of its methods; the module calls the class's.
    assert!(calls(&p).contains(&call(&format!("{c}.OrdersController"), "", "Get")));
    assert!(calls(&p).contains(&call(c, "", "Controller")));
    assert!(calls(&p).contains(&call(&format!("{c}.OrdersController.one"), "", "Param")));
    // What a decorator is given is about the method it decorates.
    assert!(facts_of(&p, fact::REFERENCE).contains(&row(&format!("{c}.OrdersController.one"), "AuthGuard", "", true)));
}

#[test]
fn http_client_calls_are_api_calls() {
    let src = r#"
import axios from "axios";
import got from "got";
import ky from "ky";
import http from "http";
import https from "https";
import superagent from "superagent";

const BASE = "http://billing";
const billing = axios.create({ baseURL: "http://billing/api/" });

export async function calls(id: string, verb: string, options: RequestInit) {
  await fetch("http://users/api/users");
  await fetch(`/api/users/${id}`, { method: "post", body: "{}" });
  await fetch("/api/dynamic", { method: verb });
  await fetch("/api/options", options);
  await fetch("/api/spread", { ...options });
  await fetch(url);
  await axios.get("http://orders-api/orders");
  await axios.delete(BASE + "/invoices/" + id);
  await axios({ url: "/api/config", method: "put" });
  await axios("http://orders-api/plain");
  await axios.request({ url: "http://orders-api/request", method: "PATCH" });
  await billing.post("/invoices", {});
  await billing.get("http://elsewhere/x");
  await got.post("http://a/got");
  await got("http://a/got-plain", { method: "DELETE" });
  await ky.put("http://a/ky");
  await superagent.get("http://a/superagent");
  http.get("http://a/http", (res) => {});
  https.request("https://a/https", { method: "POST" });
  https.request({ host: "a", path: "/no-url" });
  await this.httpClient.get("/client-path");
  await window.fetch("/window");
  await axios.all([]);
}

function notHttp(cache, map) {
  cache.get("/key");
  map.get("orders");
  axios.get(someUrl);
  fetch("data.json");
  unknown.post("http://full/url");
}
"#;
    let p = parse("web/src/client.ts", src);
    let c = "web.src.client.calls";
    assert_eq!(
        facts_of(&p, fact::API_CALL),
        vec![
            row(c, "http://users/api/users", "GET", true),
            row(c, "/api/users/${id}", "POST", false),
            row(c, "/api/dynamic", "ANY", true),
            row(c, "/api/options", "ANY", true),
            row(c, "/api/spread", "ANY", true),
            row(c, "http://orders-api/orders", "GET", true),
            // A constant of the file is filled in; the rest of a template stays a hole.
            row(c, "http://billing/invoices/${id}", "DELETE", false),
            row(c, "/api/config", "PUT", true),
            row(c, "http://orders-api/plain", "GET", true),
            row(c, "http://orders-api/request", "PATCH", true),
            // A client made with a base URL: the path is joined to it.
            row(c, "http://billing/api/invoices", "POST", true),
            row(c, "http://elsewhere/x", "GET", true),
            row(c, "http://a/got", "POST", true),
            row(c, "http://a/got-plain", "DELETE", true),
            row(c, "http://a/ky", "PUT", true),
            row(c, "http://a/superagent", "GET", true),
            row(c, "http://a/http", "GET", true),
            row(c, "https://a/https", "POST", true),
            row(c, "/client-path", "GET", true),
            row(c, "/window", "GET", true),
            // A full URL is an HTTP call whatever it is called on; a bare path is not.
            row("web.src.client.notHttp", "http://full/url", "POST", true),
        ]
    );
    assert_eq!(facts_of(&p, fact::API_EXPOSE), vec![]);
    let template = p.facts.iter().find(|f| f.key == "/api/users/${id}").unwrap();
    assert_eq!(lge_core::normalize_route(&template.key), "/api/users/{}");
}

#[test]
fn http_client_calls_in_javascript() {
    let src = r#"
const axios = require("axios");
const ORDERS_URL = process.env.ORDERS_URL || "http://orders-api";

exports.createOrder = async (body) => (await axios.post("http://orders-api/orders", body)).data;

async function fetchOrder(id) {
  const response = await axios.get(`${ORDERS_URL}/orders/${id}`);
  return response.data;
}

const loadUser = (id) => fetch("/api/users/" + id).then((r) => r.json());
"#;
    let p = parse("web/src/ordersClient.js", src);
    assert_eq!(
        facts_of(&p, fact::API_CALL),
        vec![
            row(
                "web.src.ordersClient.createOrder",
                "http://orders-api/orders",
                "POST",
                true
            ),
            // `ORDERS_URL` may come from the environment: it stays a hole.
            row(
                "web.src.ordersClient.fetchOrder",
                "${ORDERS_URL}/orders/${id}",
                "GET",
                false
            ),
            row("web.src.ordersClient.loadUser", "/api/users/${id}", "GET", false),
        ]
    );
    let key = "${ORDERS_URL}/orders/${id}";
    assert_eq!(lge_core::normalize_route(key), "/orders/{}");
}

#[test]
fn sql_in_strings_reads_and_writes_tables() {
    let src = r#"
const TABLE = "audit_log";
const GET_ORDER = "SELECT * FROM orders WHERE id = $1";

export async function run(db, id, total) {
  await db.query("SELECT id, total FROM orders WHERE id = $1", [id]);
  await db.query(`UPDATE orders SET total = ${total} WHERE id = ${id}`);
  await db.query(`DELETE FROM ${TABLE} WHERE id = 1`);
  await db.query("INSERT INTO " + TABLE + " (id) VALUES (1)");
  await db.query(`SELECT * FROM ${dynamicTable}`);
  await db.query(GET_ORDER, [id]);
  await sql`
    SELECT c.name
    FROM customers c
    JOIN orders o ON o.customer_id = c.id`;
  log("could not load data from orders");
  log(`Select a plan from the list`);
}
"#;
    let p = parse("svc/repo.ts", src);
    assert_eq!(
        facts_of(&p, fact::DB_READ),
        vec![
            // The constant itself, at module level.
            row("svc.repo", "orders", "", true),
            row("svc.repo.run", "orders", "", true),
            // Run through a constant: known, but not written here.
            row("svc.repo.run", "orders", "", false),
            row("svc.repo.run", "customers", "", true),
            row("svc.repo.run", "orders", "", true),
        ]
    );
    assert_eq!(
        facts_of(&p, fact::DB_WRITE),
        vec![
            // Interpolated values do not make the table name less literal.
            row("svc.repo.run", "orders", "", true),
            // An interpolated table name is known only through the constant.
            row("svc.repo.run", "audit_log", "", false),
            row("svc.repo.run", "audit_log", "", false),
        ]
    );
    let join = p.facts.iter().find(|f| f.key == "customers").unwrap();
    assert_eq!((join.line, join.end_line), (12, 15));
}

#[test]
fn sql_and_prisma_in_javascript() {
    let src = r#"
class OrderRepo {
  constructor(prisma, pool) { this.prisma = prisma; this.pool = pool; }
  async all() { return this.prisma.order.findMany({ where: {} }); }
  async add(data) { return this.prisma.order.create({ data }); }
  async total() { return this.pool.query('select sum(total) from "Orders"'); }
}
async function work(prisma) {
  await prisma.user.findUnique({ where: { id: 1 } });
  await prisma.user.count();
  await prisma.orderItem.upsert({});
  await prisma.invoice.deleteMany();
  await prisma.$transaction([]);
  await prisma.$queryRaw`SELECT id FROM payments`;
  await prisma.user.validate();
  await other.user.findMany();
}
"#;
    let p = parse("svc/orders.js", src);
    assert_eq!(
        facts_of(&p, fact::DB_READ),
        vec![
            // The table name is derived from the property, so it is not a literal.
            row("svc.orders.OrderRepo.all", "order", "", false),
            row("svc.orders.OrderRepo.total", "Orders", "", true),
            row("svc.orders.work", "user", "", false),
            row("svc.orders.work", "user", "", false),
            row("svc.orders.work", "payments", "", true),
        ]
    );
    assert_eq!(
        facts_of(&p, fact::DB_WRITE),
        vec![
            row("svc.orders.OrderRepo.add", "order", "", false),
            row("svc.orders.work", "orderItem", "", false),
            row("svc.orders.work", "invoice", "", false),
        ]
    );
}

#[test]
fn orm_models_map_their_table() {
    let src = r#"
import { Entity, Column } from "typeorm";
import { Model, Table } from "sequelize-typescript";

const NAME = "order_lines";

@Entity("orders")
export class Order {
  @Column() total: number;
}
@Entity({ name: "order_items", schema: "shop" })
class OrderItem {}
@Entity(NAME)
class OrderLine {}
@Entity()
class Unnamed {}
@Table({ tableName: "customers" })
class Customer extends Model {}

class Invoice extends Model {}
Invoice.init({ total: Number }, { sequelize, tableName: "invoices" });
class Payment extends Model {
  static setup(sequelize) {
    return super.init({}, { tableName: "payments", sequelize });
  }
}
const Refund = sequelize.define("Refund", {}, { tableName: "refunds" });
Imported.init({}, { tableName: "imported" });
"#;
    let p = parse("svc/models.ts", src);
    assert_eq!(
        facts_of(&p, fact::TABLE_MODEL),
        vec![
            row("svc.models.Order", "orders", "", true),
            row("svc.models.OrderItem", "order_items", "", true),
            row("svc.models.OrderLine", "order_lines", "", false),
            row("svc.models.Customer", "customers", "", true),
            row("svc.models.Invoice", "invoices", "", true),
            row("svc.models.Payment", "payments", "", true),
            // No class stands for these models: the fact stays where it is written.
            row("svc.models", "refunds", "", true),
            row("svc.models", "imported", "", true),
        ]
    );
    assert_eq!(facts_of(&p, fact::DB_READ), vec![]);
    assert_eq!(facts_of(&p, fact::DB_WRITE), vec![]);
}

#[test]
fn amqplib_publishes_and_consumes_queues() {
    let src = r#"
import amqp from "amqplib";
import { importedHandler } from "./handlers";

const QUEUE = "order.created";

export async function publish(channel, ch) {
  channel.sendToQueue("emails.out", Buffer.from("x"));
  channel.sendToQueue(QUEUE, Buffer.from("x"));
  channel.sendToQueue(dynamic, Buffer.from("x"));
  channel.publish("events", "order.paid", Buffer.from("x"));
  ch.publish("", "direct.queue", Buffer.from("x"));
  pubsub.publish("not-amqp", payload);
  channel.publish("two-arguments", payload);
}

export async function subscribe(connection) {
  const channel = await connection.createChannel();
  await channel.consume("order.created", onOrderCreated);
  await channel.consume(QUEUE, (message) => { handle(message); });
  await channel.consume("imported.queue", importedHandler, { noAck: true });
  await bus.consume("inline.queue", async (message) => {});
  await limiter.consume("rate-key", 2);
  await limiter.consume("rate-key", points);
}

function onOrderCreated(message) {}
"#;
    let p = parse("svc/bus.ts", src);
    assert_eq!(
        facts_of(&p, fact::QUEUE_PUBLISH),
        vec![
            row("svc.bus.publish", "emails.out", "queue", true),
            row("svc.bus.publish", "order.created", "queue", false),
            row("svc.bus.publish", "events", "exchange", true),
            // On the default exchange the routing key is the queue.
            row("svc.bus.publish", "direct.queue", "queue", true),
        ]
    );
    assert_eq!(
        facts_of(&p, fact::QUEUE_CONSUME),
        vec![
            // The named handler declared in this file consumes the queue.
            row("svc.bus.onOrderCreated", "order.created", "queue", true),
            row("svc.bus.subscribe", "order.created", "queue", false),
            row("svc.bus.subscribe", "imported.queue", "queue", true),
            row("svc.bus.subscribe", "inline.queue", "queue", true),
        ]
    );
    let references = facts_of(&p, fact::REFERENCE);
    assert!(references.contains(&row("svc.bus.subscribe", "importedHandler", "", true)));
    assert!(references.contains(&row("svc.bus.subscribe", "onOrderCreated", "", true)));
    assert!(calls(&p).contains(&call("svc.bus.subscribe", "", "handle")));
}

#[test]
fn nest_messaging_and_bullmq_queues() {
    let src = r#"
import { Queue, Worker } from "bullmq";
import { Worker as Thread } from "worker_threads";
import { ClientProxy, EventPattern, MessagePattern } from "@nestjs/microservices";
import { processReport } from "./reports";

const emailQueue = new Queue("emails", { connection });
const thread = new Thread("./worker.js");

export class OrdersConsumer {
  private reports = new Queue("reports");
  constructor(private readonly broker: ClientProxy, private readonly client: ClientProxy) {}

  @EventPattern("order.created")
  async onCreated(data) {
    this.broker.emit("order.confirmed", data);
    await this.client.send("billing.charge", data);
    await emailQueue.add("welcome", data);
    await this.reports.addBulk([]);
    await new Queue("direct").add("job", {});
    socket.emit("ignored", data);
    response.send("ignored");
  }

  @MessagePattern("orders.sum")
  sum(data) {}

  @MessagePattern({ cmd: "sum" })
  objectPattern(data) {}
}

new Worker("emails", processEmail);
new Worker("reports", processReport);
new Worker("direct", async (job) => { run(job); });
async function processEmail(job) {}
"#;
    let p = parse("svc/consumer.ts", src);
    let on_created = "svc.consumer.OrdersConsumer.onCreated";
    assert_eq!(
        facts_of(&p, fact::QUEUE_PUBLISH),
        vec![
            // `broker` is a message client by its declared type, `client` by its name too.
            row(on_created, "order.confirmed", "topic", true),
            row(on_created, "billing.charge", "topic", true),
            row(on_created, "emails", "queue", true),
            row(on_created, "reports", "queue", true),
            row(on_created, "direct", "queue", true),
        ]
    );
    assert_eq!(
        facts_of(&p, fact::QUEUE_CONSUME),
        vec![
            row(on_created, "order.created", "topic", true),
            row("svc.consumer.OrdersConsumer.sum", "orders.sum", "topic", true),
            row("svc.consumer.processEmail", "emails", "queue", true),
            row("svc.consumer", "reports", "queue", true),
            row("svc.consumer", "direct", "queue", true),
        ],
        "a worker thread is not a queue worker"
    );
    assert!(facts_of(&p, fact::REFERENCE).contains(&row("svc.consumer", "processReport", "", true)));

    // Outside NestJS, `client.emit` is an event emitter.
    let emitter = parse(
        "svc/pool.js",
        "function close(client) { client.emit(\"disconnect\", client); client.send(\"bye\"); }\n",
    );
    assert_eq!(facts_of(&emitter, fact::QUEUE_PUBLISH), vec![]);
    let nest = parse(
        "svc/notify.ts",
        "import { ClientKafka } from \"@nestjs/microservices\";\nfunction notify(client) { client.emit(\"user.created\", {}); }\n",
    );
    assert_eq!(
        facts_of(&nest, fact::QUEUE_PUBLISH),
        vec![row("svc.notify.notify", "user.created", "topic", true)]
    );

    // Without the import, `Queue` and `Worker` are somebody else's classes.
    let other = parse(
        "svc/other.js",
        "const q = new Queue(\"emails\");\nq.add(\"x\");\nnew Worker(\"emails\", run);\n",
    );
    assert_eq!(facts_of(&other, fact::QUEUE_PUBLISH), vec![]);
    assert_eq!(facts_of(&other, fact::QUEUE_CONSUME), vec![]);
    // CommonJS is an import too.
    let cjs = parse(
        "svc/jobs.cjs",
        "const { Queue } = require(\"bullmq\");\nconst jobs = new Queue(\"jobs\");\nmodule.exports.enqueue = (job) => jobs.add(\"run\", job);\n",
    );
    assert_eq!(
        facts_of(&cjs, fact::QUEUE_PUBLISH),
        vec![row("svc.jobs.enqueue", "jobs", "queue", true)]
    );
}

#[test]
fn models_and_queues_in_javascript() {
    let src = r#"
const amqp = require("amqplib");
const { Model } = require("sequelize");
const QUEUE = "invoice.created";

class Invoice extends Model {}
Invoice.init({ total: "DECIMAL" }, { sequelize, tableName: "invoices" });

async function main() {
  const connection = await amqp.connect(process.env.AMQP_URL);
  const channel = await connection.createChannel();
  await channel.consume(QUEUE, onInvoice);
  channel.sendToQueue("invoice.mailed", Buffer.from("{}"));
}

const onInvoice = async (message) => {
  await Invoice.create(JSON.parse(message.content));
};

module.exports = { main };
"#;
    let p = parse("billing/worker.js", src);
    assert_eq!(
        facts_of(&p, fact::TABLE_MODEL),
        vec![row("billing.worker.Invoice", "invoices", "", true)]
    );
    assert_eq!(
        facts_of(&p, fact::QUEUE_CONSUME),
        // The handler is an arrow function bound further down the file.
        vec![row("billing.worker.onInvoice", "invoice.created", "queue", false)]
    );
    assert_eq!(
        facts_of(&p, fact::QUEUE_PUBLISH),
        vec![row("billing.worker.main", "invoice.mailed", "queue", true)]
    );
    assert_eq!(
        facts_of(&p, fact::INHERITS),
        vec![row("billing.worker.Invoice", "Model", "", true)]
    );
    assert_eq!(
        imports(&p),
        vec![("amqplib", "*", "amqp"), ("sequelize", "Model", "Model")]
    );
    assert!(calls(&p).contains(&call("billing.worker.onInvoice", "Invoice", "create")));
}

#[test]
fn bases_and_interfaces_are_inherits_and_implements() {
    let src = r#"
import { Model } from "./orm";
import * as events from "events";

const Mixed = mixin(Model);

interface Mailer {}
class BaseMailer {}
export class SmtpMailer extends BaseMailer implements Mailer {}
class Store<T> extends Model<T> implements Repo<T>, events.EventEmitter, Closable {}
class FromVariable extends Mixed {}
class FromCall extends mixin(Model) {}
class Plain {}
"#;
    let p = parse("svc/mailer.ts", src);
    assert_eq!(
        facts_of(&p, fact::INHERITS),
        vec![
            row("svc.mailer.SmtpMailer", "BaseMailer", "", true),
            row("svc.mailer.Store", "Model", "", true),
        ],
        "a base that is a variable or a call has no symbol to stand for it"
    );
    assert_eq!(
        facts_of(&p, fact::IMPLEMENTS),
        vec![
            row("svc.mailer.SmtpMailer", "Mailer", "", true),
            row("svc.mailer.Store", "Repo", "", true),
            row("svc.mailer.Store", "events.EventEmitter", "", true),
            row("svc.mailer.Store", "Closable", "", true),
        ]
    );
    let base = p.facts.iter().find(|f| f.key == "BaseMailer").unwrap();
    assert_eq!((base.line, base.end_line), (9, 9));
    assert_eq!(base.symbol_id, symbol(&p, "svc.mailer.SmtpMailer").id);

    let js = parse(
        "svc/mailer.js",
        "class A extends B {}\nclass C extends ns.D {}\nclass E {}\n",
    );
    assert_eq!(
        facts_of(&js, fact::INHERITS),
        vec![
            row("svc.mailer.A", "B", "", true),
            row("svc.mailer.C", "ns.D", "", true)
        ]
    );
    assert_eq!(facts_of(&js, fact::IMPLEMENTS), vec![]);
}

#[test]
fn names_passed_around_are_references() {
    let src = r#"
import { validate } from "./validate";
import * as hooks from "./hooks";

const limit = 10;
function onMessage() {}

@Injectable
@Guarded(AdminGuard)
export class Worker {
  @observable count = 0;

  @Trace
  run(job, callback) {
    schedule(onMessage, limit, job, callback);
    pipeline(validate, hooks.before, this.finish, this.count, this);
    retry(undefined, console.log, JSON, deep.inside.value, () => onMessage, onMessage());
    new Runner(onMessage);
  }

  finish() {}
}
"#;
    let p = parse("svc/worker.ts", src);
    assert_eq!(
        facts_of(&p, fact::REFERENCE),
        vec![
            // A decorator without a call is referenced by what it decorates.
            row("svc.worker.Worker", "Injectable", "", true),
            row("svc.worker.Worker", "AdminGuard", "", true),
            row("svc.worker.Worker.run", "Trace", "", true),
            // Variables, parameters, globals and deeper paths are not symbols.
            row("svc.worker.Worker.run", "onMessage", "", true),
            row("svc.worker.Worker.run", "validate", "", true),
            row("svc.worker.Worker.run", "hooks.before", "", true),
            row("svc.worker.Worker.run", "this.finish", "", true),
            row("svc.worker.Worker.run", "onMessage", "", true),
        ]
    );
    let reference = p.facts.iter().find(|f| f.key == "validate").unwrap();
    assert_eq!((reference.line, reference.end_line), (16, 16));
}

#[test]
fn parameters_of_callbacks_are_variables_wherever_the_callback_is() {
    let src = r#"
const { constants: { R_OK }, ...others } = require("fs");

@Register((target) => track(target))
class Display {
  static usage = groups.filter((value, index) => keep(value, index));
  #handler = withMeta((level, meta) => { this.write(level, meta, Formatter); });
  static { boot((error) => report(error)); }
  get title() { return "x"; }
  write() {}

  @Transform(({ value }) => parse(value))
  show(@Inject((token) => lookup(token)) dep, fallback = (reason) => log(reason)) {
    check(R_OK, others, this.title, this.write);
  }
}
"#;
    let p = parse("svc/display.js", src);
    assert_eq!(
        facts_of(&p, fact::REFERENCE),
        vec![
            row("svc.display.Display", "Formatter", "", true),
            // A getter is read, not passed on: only the method is a reference.
            row("svc.display.Display.show", "this.write", "", true),
        ]
    );
}

// ---------------------------------------------------------------------------
// Whole files
// ---------------------------------------------------------------------------

#[test]
fn ids_are_unique_and_facts_point_at_symbols() {
    let src = r#"
import { a } from "./a";
import { a } from "./a";
class Box {
  get value() { return read("SELECT v FROM boxes"); }
  set value(v) { write("UPDATE boxes SET v = 1"); }
}
function twice() {}
function twice() { run(); run(); }
app.get("/x", twice); app.get("/x", twice);
"#;
    let p = parse("svc/dup.js", src);
    let unique = |ids: Vec<&str>| {
        let all = ids.len();
        ids.into_iter().collect::<HashSet<_>>().len() == all
    };
    assert!(unique(p.symbols.iter().map(|s| s.id.as_str()).collect()));
    assert!(unique(p.calls.iter().map(|c| c.id.as_str()).collect()));
    assert!(unique(p.imports.iter().map(|i| i.id.as_str()).collect()));
    assert!(unique(p.facts.iter().map(|f| f.id.as_str()).collect()));
    assert_eq!(p.imports.len(), 1);
    assert_eq!(kinds(&p).iter().filter(|(qn, _)| *qn == "svc.dup.twice").count(), 1);
    for f in &p.facts {
        assert!(p.symbols.iter().any(|s| s.id == f.symbol_id), "{f:?}");
        assert_eq!(f.file_path, "svc/dup.js");
        assert!(f.line >= 1 && f.end_line >= f.line && !f.key.is_empty(), "{f:?}");
    }
    for c in &p.calls {
        assert!(p.symbols.iter().any(|s| s.id == c.caller_id), "{c:?}");
    }
    // Both accessors share the symbol, and both statements are its facts.
    assert_eq!(
        facts_of(&p, fact::DB_READ),
        vec![row("svc.dup.Box.value", "boxes", "", true)]
    );
    assert_eq!(
        facts_of(&p, fact::DB_WRITE),
        vec![row("svc.dup.Box.value", "boxes", "", true)]
    );
}

#[test]
fn parsing_twice_gives_the_same_records() {
    let src = "import x from \"./x\";\nexport default function f() { return x.get(\"http://a/b\"); }\n";
    let (a, b) = (parse("svc/same.ts", src), parse("svc/same.ts", src));
    assert_eq!(a.symbols, b.symbols);
    assert_eq!(a.calls, b.calls);
    assert_eq!(a.imports, b.imports);
    assert_eq!(a.facts, b.facts);
    assert_eq!(a.file, b.file);
}

#[test]
fn broken_and_hostile_input_does_not_panic() {
    // A syntax error does not lose what was read before it.
    let p = parse(
        "svc/broken.ts",
        "export function ok() { return 1; }\nfunction broken( { call(;\nclass {\n",
    );
    assert!(p.symbols.iter().any(|s| s.qualified_name == "svc.broken.ok"));
    parse("svc/garbage.ts", "}}}} ((( @@ `${ class extends { => import from");
    // Not UTF-8.
    let p = parse_typescript("svc/bytes.js", "svc", &[0xff, 0xfe, b'f', b'(', b')', b';'], "h").unwrap();
    assert_eq!(p.symbols.len(), 1);
    // Nesting deeper than the visitor follows.
    let deep = format!("{}f(){}", "(".repeat(2000), ")".repeat(2000));
    let p = parse("svc/deep.js", &deep);
    assert_eq!(p.symbols.len(), 1);
    let nested = format!("x = {}1{};", "[".repeat(3000), "]".repeat(3000));
    assert_eq!(parse("svc/nested.js", &nested).symbols.len(), 1);
    // A very long concatenation is read without recursion.
    let concat = format!("const q = \"SELECT * FROM t WHERE a = \"{};\n", " + x".repeat(5000));
    assert_eq!(
        facts_of(&parse("svc/concat.js", &concat), fact::DB_READ),
        vec![row("svc.concat", "t", "", true)]
    );
    assert_eq!(parse("svc/empty.ts", "").symbols.len(), 1);
}
