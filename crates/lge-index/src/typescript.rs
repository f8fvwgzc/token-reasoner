//! TypeScript and JavaScript extraction: symbols, call sites, import sites and fact sites.

mod blocks;
mod calls;
mod comments;
mod data;
mod http;
mod imports;
mod scope;
mod strings;
mod symbols;

use crate::facts::{self as shared, fact_site, Piece};
use crate::{lang_of, unique_ids};
use anyhow::{Context, Result};
use blocks::{sanitise, test_block, test_callee, TestKind};
use comments::module_doc;
use lge_core::{fact, file_id, kind, lang, symbol_id, FileRec, ParsedFile, SymbolRec};
use scope::{is_builtin, Bindings};
use std::collections::{BTreeSet, HashMap, HashSet};
use strings::{dotted, text_of};
use symbols::{
    assignment, class_declaration, define_function, enumeration, export, function_declaration, interface, namespace,
    type_alias, type_name, variables, FunctionDef,
};
use tree_sitter::{Language, Node, Parser};

/// Deepest syntax nesting that is followed. Real code stays far below it; the
/// limit keeps a generated file from overflowing the stack.
const MAX_DEPTH: usize = 400;

struct Ctx<'a> {
    src: &'a [u8],
    path: &'a str,
    service: &'a str,
    file_is_test: bool,
    /// Qualified name of the module symbol: the root of every qualified name in the file.
    module_qn: String,
    /// Names bound at module level, and the string constants among them.
    module: Bindings,
    /// Local name bound by an import -> (module specifier, imported name).
    import_sources: HashMap<String, (String, String)>,
    /// Variables bound to an Express app or router.
    routers: HashSet<String>,
    /// Variables bound to an HTTP client, with its base URL when one is written
    /// (and whether that is a literal).
    http_clients: HashMap<String, Option<(String, bool)>>,
    /// Variables bound to an AMQP channel.
    channels: HashSet<String>,
    /// Variables bound to a NestJS message client.
    message_clients: HashSet<String>,
    /// Variables bound to a job queue: name -> (queue name, the name is a literal).
    queues: HashMap<String, (String, bool)>,
    /// Strings already searched for SQL as part of a larger expression, by node id.
    sql_seen: HashSet<usize>,
    /// Ids of the functions a bare name can reach: declarations and functions
    /// bound to a variable. Methods and object members are not.
    bare_callable: HashSet<String>,
    /// Facts whose owner is a name that may be declared further down the file.
    reowns: Vec<Reown>,
    /// `export default name;` statements, settled when every symbol is known.
    default_exports: Vec<DefaultExport>,
    depth: usize,
    out: ParsedFile,
}

/// What a class tells the code inside it.
struct ClassInfo {
    id: String,
    /// Methods and function-valued fields, for `this.method` references.
    methods: HashSet<String>,
    /// NestJS `@Controller("prefix")`: the route prefix, and whether it is a literal.
    prefix: Option<(String, bool)>,
}

/// Where the visitor is: which symbol owns what it finds, and which names are variables.
#[derive(Clone, Copy)]
struct Scope<'s> {
    /// Qualified name that definitions found here are prefixed with.
    qn: &'s str,
    /// Symbol that facts found here belong to: the innermost enclosing
    /// function, method or class, or the module.
    owner_id: &'s str,
    /// Symbol that calls found here belong to. Differs from `owner_id` only
    /// inside a decorator: the enclosing scope makes that call, while what the
    /// decorator says is about the symbol it decorates.
    caller_id: &'s str,
    /// The nearest enclosing class.
    class: Option<&'s ClassInfo>,
    /// Names bound by the enclosing functions and classes; `None` at module
    /// level, where the module's own bindings apply.
    locals: Option<&'s Bindings>,
    /// Inside a `describe`, `it` or `test` block.
    in_test: bool,
}

/// Where a definition starts and ends. The two differ when decorators stand in
/// front of a method as siblings of it.
#[derive(Clone, Copy)]
struct Extent<'t> {
    first: Node<'t>,
    last: Node<'t>,
}

impl<'t> Extent<'t> {
    fn of(node: Node<'t>) -> Self {
        Extent {
            first: node,
            last: node,
        }
    }
}

/// Which symbol a name written in the source can stand for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Want {
    /// A function that a bare name reaches.
    Function,
    Class,
    /// Whatever the file can export under that name.
    Exportable,
}

/// A fact, already recorded for the enclosing symbol, that belongs to the symbol
/// called `name` if the file declares one in a scope around `scope_qn`.
struct Reown {
    fact: usize,
    name: String,
    scope_qn: String,
    want: Want,
}

struct DefaultExport {
    name: String,
    scope_qn: String,
    line: i64,
}

fn text<'a>(n: Node, src: &'a [u8]) -> &'a str {
    n.utf8_text(src).unwrap_or("")
}

/// First and last line of a node, 1-based.
fn lines(n: Node) -> (i64, i64) {
    (n.start_position().row as i64 + 1, n.end_position().row as i64 + 1)
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The grammar for a path: TSX for `.tsx`, JavaScript (which includes JSX) for
/// `.js`, `.jsx`, `.mjs` and `.cjs`, TypeScript for everything else.
fn grammar(path: &str) -> Language {
    match path.rsplit('.').next().unwrap_or("") {
        "tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
        "js" | "jsx" | "mjs" | "cjs" => tree_sitter_javascript::LANGUAGE.into(),
        _ => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
    }
}

/// The file path without its extension; `index.*` stands for its directory,
/// like `__init__.py`.
fn module_path(path: &str) -> &str {
    let (dir, file) = match path.rsplit_once('/') {
        Some((dir, file)) => (dir, file),
        None => ("", path),
    };
    let stem = file.rsplit_once('.').map_or(file, |(stem, _)| stem);
    if stem == "index" && !dir.is_empty() {
        dir
    } else {
        &path[..path.len() - (file.len() - stem.len())]
    }
}

/// `*.test.*`, `*.spec.*`, or anything under `__tests__/` or `tests/`.
fn is_test_file(path: &str) -> bool {
    let base = path.rsplit('/').next().unwrap_or(path);
    let in_test_dir = ["__tests__/", "tests/"]
        .iter()
        .any(|dir| path.starts_with(dir) || path.contains(&format!("/{dir}")));
    base.contains(".test.") || base.contains(".spec.") || in_test_dir
}

/// Parse one TypeScript or JavaScript file (`.ts`, `.tsx`, `.js`, `.jsx`, `.mjs`,
/// `.cjs`) into symbols, call sites, import sites and fact sites.
pub fn parse_typescript(rel_path: &str, service: &str, src: &[u8], hash: &str) -> Result<ParsedFile> {
    let path = rel_path;
    let mut parser = Parser::new();
    parser.set_language(&grammar(path))?;
    let tree = parser.parse(src, None).context("tree-sitter parse failed")?;
    let root = tree.root_node();
    let module_path = module_path(path);
    let module = module_path.replace('/', ".");
    let module_id = symbol_id(path, &module, kind::MODULE);
    let mut ctx = Ctx {
        src,
        path,
        service,
        file_is_test: is_test_file(path),
        module_qn: module.clone(),
        module: Bindings::of(root, src, None),
        import_sources: HashMap::new(),
        routers: HashSet::new(),
        http_clients: HashMap::new(),
        channels: HashSet::new(),
        message_clients: HashSet::new(),
        queues: HashMap::new(),
        sql_seen: HashSet::new(),
        bare_callable: HashSet::new(),
        reowns: Vec::new(),
        default_exports: Vec::new(),
        depth: 0,
        out: ParsedFile::default(),
    };
    imports::prescan(root, &mut ctx);
    ctx.out.file = Some(FileRec {
        id: file_id(path),
        path: path.to_string(),
        service: service.to_string(),
        lang: lang_of(path).unwrap_or(lang::TYPESCRIPT).into(),
        content_hash: hash.to_string(),
    });
    ctx.out.symbols.push(SymbolRec {
        id: module_id.clone(),
        name: module_path.rsplit('/').next().unwrap_or(module_path).to_string(),
        qualified_name: module.clone(),
        kind: kind::MODULE.into(),
        signature: String::new(),
        doc: module_doc(root, src),
        body: String::new(),
        file_path: path.to_string(),
        service: service.to_string(),
        start_line: 1,
        end_line: root.end_position().row as i64 + 1,
        is_test: ctx.file_is_test,
    });
    let scope = Scope {
        qn: &module,
        owner_id: &module_id,
        caller_id: &module_id,
        class: None,
        locals: None,
        in_test: false,
    };
    visit_children(root, &mut ctx, &scope);
    ctx.settle();
    unique_ids(&mut ctx.out);
    Ok(ctx.out)
}

// ---------------------------------------------------------------------------
// Small syntax helpers
// ---------------------------------------------------------------------------

/// Looks through parentheses, `await`, `as T`, `satisfies T` and `!` to the
/// expression they wrap.
fn unwrap(node: Node) -> Node {
    let mut cur = node;
    for _ in 0..8 {
        let wraps = matches!(
            cur.kind(),
            "parenthesized_expression"
                | "await_expression"
                | "as_expression"
                | "satisfies_expression"
                | "non_null_expression"
        );
        let mut c = cur.walk();
        let inner = cur.named_children(&mut c).find(|n| n.kind() != "comment");
        match inner {
            Some(inner) if wraps => cur = inner,
            _ => break,
        }
    }
    cur
}

fn is_function_value(node: Node) -> bool {
    matches!(
        node.kind(),
        "arrow_function" | "function_expression" | "generator_function"
    )
}

/// A class expression: `const C = class extends Base {}`.
fn is_class_value(node: Node) -> bool {
    node.kind() == "class"
}

/// Argument expressions of a call or a construction, comments left out.
fn arguments<'t>(call: Node<'t>) -> Vec<Node<'t>> {
    let Some(list) = call
        .child_by_field_name("arguments")
        .filter(|a| a.kind() == "arguments")
    else {
        return Vec::new();
    };
    let mut c = list.walk();
    let args: Vec<Node> = list.named_children(&mut c).filter(|n| n.kind() != "comment").collect();
    args
}

fn children_of_kind<'t>(node: Node<'t>, kind: &str) -> Vec<Node<'t>> {
    let mut c = node.walk();
    let found: Vec<Node> = node.named_children(&mut c).filter(|n| n.kind() == kind).collect();
    found
}

/// Where a definition starts when its decorators are left out.
fn after_decorators(node: Node) -> usize {
    let mut c = node.walk();
    let start = node
        .children(&mut c)
        .find(|n| !matches!(n.kind(), "decorator" | "comment"))
        .map_or(node.start_byte(), |n| n.start_byte());
    start
}

/// The expression after `@`.
fn decorator_expr(decorator: Node) -> Option<Node> {
    let mut c = decorator.walk();
    let expr = decorator.named_children(&mut c).find(|n| n.kind() != "comment");
    expr
}

/// The last segment of what a call calls: `Get` in `@Get()` and in `@nest.Get()`.
fn callee_name<'a>(call: Node, src: &'a [u8]) -> Option<&'a str> {
    let f = call.child_by_field_name("function")?;
    match f.kind() {
        "identifier" => Some(text(f, src)),
        "member_expression" => f.child_by_field_name("property").map(|p| text(p, src)),
        _ => None,
    }
}

/// The name of a class member or an object key, when it is written as a name.
/// Computed keys, numbers and strings that are not identifiers have none.
fn member_name(node: Option<Node>, src: &[u8]) -> Option<String> {
    let node = node?;
    match node.kind() {
        "property_identifier" | "private_property_identifier" | "identifier" => Some(text(node, src).to_string()),
        "string" => strings::plain_string(node, src).filter(|s| {
            !s.is_empty()
                && !s.starts_with(|c: char| c.is_ascii_digit())
                && s.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '$')
        }),
        _ => None,
    }
}

/// The value of `name` in an object literal: `{ name: value }` or the shorthand `{ name }`.
fn object_prop<'t>(object: Node<'t>, name: &str, src: &[u8]) -> Option<Node<'t>> {
    let mut c = object.walk();
    let found = object.named_children(&mut c).find_map(|member| match member.kind() {
        "pair" => {
            let key = member_name(member.child_by_field_name("key"), src)?;
            (key == name).then(|| member.child_by_field_name("value")).flatten()
        }
        "shorthand_property_identifier" => (text(member, src) == name).then_some(member),
        _ => None,
    });
    found
}

fn has_spread(object: Node) -> bool {
    !children_of_kind(object, "spread_element").is_empty()
}

/// What an assignment to `exports` or `module.exports` exports.
#[derive(Debug, PartialEq, Eq)]
enum ExportsTarget {
    /// `module.exports = ...`
    Whole,
    /// `exports.name = ...`, `module.exports.name = ...`
    Named(String),
}

fn exports_target(left: Node, src: &[u8]) -> Option<ExportsTarget> {
    let written = dotted(left, src)?;
    if written == "module.exports" {
        return Some(ExportsTarget::Whole);
    }
    let name = written
        .strip_prefix("module.exports.")
        .or_else(|| written.strip_prefix("exports."))?;
    (!name.contains('.')).then(|| ExportsTarget::Named(name.to_string()))
}

// ---------------------------------------------------------------------------
// Visitor
// ---------------------------------------------------------------------------

fn visit_children(node: Node, ctx: &mut Ctx, scope: &Scope) {
    if ctx.depth >= MAX_DEPTH {
        return;
    }
    ctx.depth += 1;
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        visit(child, ctx, scope);
    }
    ctx.depth -= 1;
}

fn visit(node: Node, ctx: &mut Ctx, scope: &Scope) {
    match node.kind() {
        // Types make no calls, an overload has no body, and imports were read by the prescan.
        "comment" | "import_statement" | "type_annotation" | "type_arguments" | "type_parameters"
        | "function_signature" => {}
        "export_statement" => export(node, ctx, scope),
        "function_declaration" | "generator_function_declaration" => {
            function_declaration(node, node, ctx, scope);
        }
        "class_declaration" | "abstract_class_declaration" => {
            class_declaration(node, node, &[], ctx, scope);
        }
        "interface_declaration" => {
            interface(node, node, ctx, scope);
        }
        "type_alias_declaration" => {
            type_alias(node, node, ctx, scope);
        }
        "enum_declaration" => {
            enumeration(node, node, ctx, scope);
        }
        "internal_module" | "module" => namespace(node, ctx, scope),
        "lexical_declaration" | "variable_declaration" => variables(node, node, ctx, scope),
        "assignment_expression" => assignment(node, ctx, scope),
        "call_expression" => call(node, ctx, scope),
        "new_expression" => {
            let args = arguments(node);
            calls::record_new(node, ctx, scope.caller_id);
            data::new_facts(node, &args, ctx, scope);
            ctx.references(&args, scope);
            visit_children(node, ctx, scope);
        }
        "string" | "template_string" | "binary_expression" => {
            data::sql_facts(node, ctx, scope);
            // Interpolations and operands can hold calls.
            visit_children(node, ctx, scope);
        }
        // `<Layout.Header />` mentions the component it renders.
        "jsx_opening_element" | "jsx_self_closing_element" => {
            if let Some(name) = node.child_by_field_name("name") {
                ctx.component(name, scope);
            }
            visit_children(node, ctx, scope);
        }
        // `onClick={handleClick}` passes a name on, like an argument.
        "jsx_attribute" => {
            let passed: Vec<Node> = children_of_kind(node, "jsx_expression")
                .into_iter()
                .filter_map(|e| e.named_child(0))
                .collect();
            ctx.references(&passed, scope);
            visit_children(node, ctx, scope);
        }
        _ => visit_children(node, ctx, scope),
    }
}

// ---------------------------------------------------------------------------
// Calls
// ---------------------------------------------------------------------------

/// A call: a test block, or a call site with the facts it carries.
fn call(node: Node, ctx: &mut Ctx, scope: &Scope) {
    let Some(block) = test_block(node, ctx.src) else {
        let args = arguments(node);
        // `it.todo("later")` and `describe.each(table)` are the test framework's, not calls into the code.
        let framework = node
            .child_by_field_name("function")
            .is_some_and(|f| f.kind() != "identifier" && test_callee(f, ctx.src).is_some());
        if !framework {
            calls::record_call(node, ctx, scope.caller_id);
        }
        http::call_facts(node, &args, ctx, scope);
        data::call_facts(node, &args, ctx, scope);
        ctx.references(&args, scope);
        visit_children(node, ctx, scope);
        return;
    };
    // `it.each(table)`: the table is evaluated by the enclosing scope. The test
    // function itself is the framework's and is not recorded as a call.
    let mut callee = node.child_by_field_name("function");
    while let Some(f) = callee {
        if let Some(table) = f.child_by_field_name("arguments") {
            visit(table, ctx, scope);
        }
        callee = f
            .child_by_field_name("function")
            .or_else(|| f.child_by_field_name("object"));
    }
    let name = sanitise(&block.title);
    match block.kind {
        // The titles of the suites around a test are part of its qualified name.
        TestKind::Suite => {
            let qn = format!("{}.{name}", scope.qn);
            let inside = Scope {
                qn: &qn,
                in_test: true,
                ..*scope
            };
            if let Some(args) = node.child_by_field_name("arguments") {
                visit(args, ctx, &inside);
            }
        }
        // The callback is a function symbol named after the test.
        TestKind::Case => {
            for arg in arguments(node) {
                if arg.id() != block.callback.id() {
                    visit(arg, ctx, scope);
                }
            }
            let def = FunctionDef {
                func: block.callback,
                name: &name,
                kind: kind::FUNCTION,
                extent: Extent::of(node),
                sig_start: node.start_byte(),
                is_test: true,
                bare: false,
            };
            define_function(def, ctx, scope);
        }
    }
}

// ---------------------------------------------------------------------------
// Facts
// ---------------------------------------------------------------------------

impl Ctx<'_> {
    fn fact(&mut self, symbol_id: &str, kind: &str, key: &str, detail: &str, at: (i64, i64), literal: bool) {
        if key.is_empty() {
            return;
        }
        self.out
            .facts
            .push(fact_site(self.path, symbol_id, kind, key, detail, at, literal));
    }

    fn is_variable(&self, name: &str, locals: Option<&Bindings>) -> bool {
        locals.is_some_and(|l| l.names.contains(name)) || self.module.names.contains(name)
    }

    /// The string a constant holds, when the nearest scope that binds the name
    /// declares it once, as a `const` string.
    fn const_pieces<'b>(&'b self, name: &str, locals: Option<&'b Bindings>) -> Option<&'b Vec<Piece>> {
        match locals {
            Some(l) if l.names.contains(name) => l.strings.get(name),
            _ => self.module.strings.get(name),
        }
    }

    fn const_text(&self, name: &str, locals: Option<&Bindings>) -> Option<String> {
        self.const_pieces(name, locals).and_then(|p| shared::plain_text(p))
    }

    /// Pieces as one key. A hole that is a constant of this file is filled in;
    /// any other hole stays as `${expr}`.
    fn render(&self, pieces: &[Piece], locals: Option<&Bindings>) -> String {
        let mut out = String::new();
        for p in pieces {
            match p {
                Piece::Text(t) => out.push_str(t),
                Piece::Hole(e) => match self.const_text(e, locals) {
                    Some(value) => out.push_str(&value),
                    None => out.push_str(&shared::template_key(std::slice::from_ref(p))),
                },
            }
        }
        out
    }

    /// A name written at `node` (route, URL, queue, table) as `(key, literal)`.
    /// A constant holding a string is read through, and is not literal.
    /// `None` when the expression is not a string this file spells out.
    fn key_of(&self, node: Node, locals: Option<&Bindings>) -> Option<(String, bool)> {
        if let Some(t) = text_of(node, self.src) {
            return Some((self.render(&t.pieces, locals), t.plain));
        }
        if node.kind() == "identifier" {
            let pieces = self.const_pieces(text(node, self.src), locals)?;
            return Some((self.render(pieces, locals), false));
        }
        None
    }

    /// The key of a `reference` fact for a name passed around without being
    /// called, or `None` when the name cannot be a symbol: a variable, a global,
    /// a member of `this` that is not a method, or a path deeper than `Name.member`.
    fn reference_key(&self, name: &str, scope: &Scope) -> Option<String> {
        let mut parts = name.split('.');
        let root = parts.next()?;
        let member = parts.next();
        if parts.next().is_some() {
            return None;
        }
        if root == "this" {
            let member = member?;
            let is_method = scope.class.is_some_and(|c| c.methods.contains(member));
            return is_method.then(|| name.to_string());
        }
        if root == "super" || self.is_variable(root, scope.locals) || is_builtin(root) {
            return None;
        }
        Some(name.to_string())
    }

    /// Names passed as arguments: `router.use(auth)`, `consume("q", onMessage)`,
    /// `query(GET_ORDER)`.
    fn references(&mut self, args: &[Node], scope: &Scope) {
        for arg in args {
            let Some(name) = dotted(*arg, self.src) else { continue };
            if let Some(key) = self.reference_key(&name, scope) {
                self.fact(scope.owner_id, fact::REFERENCE, &key, "", lines(*arg), true);
                continue;
            }
            // `db.query(GET_ORDER)`: SQL kept in a module constant is run by the
            // function that passes it on. Reached through a variable, so not literal.
            let module_constant = scope.locals.is_some_and(|l| !l.names.contains(&name));
            if let Some(pieces) = self.module.strings.get(&name).filter(|_| module_constant).cloned() {
                self.sql_pieces(&pieces, lines(*arg), scope, false);
            }
        }
    }

    /// `<Card />` and `<Layout.Header />` mention the component they render.
    /// A lower-case tag is an HTML element.
    fn component(&mut self, name: Node, scope: &Scope) {
        let Some(written) = dotted(name, self.src) else { return };
        let is_component = written.contains('.') || written.starts_with(char::is_uppercase);
        if !is_component {
            return;
        }
        if let Some(key) = self.reference_key(&written, scope) {
            self.fact(scope.owner_id, fact::REFERENCE, &key, "", lines(name), true);
        }
    }

    /// An `inherits` or `implements` fact for the base written at `node`. A base
    /// that is a variable of this file (`const Base = mixin(Model)`) has no
    /// symbol to stand for it and is left out.
    fn base(&mut self, owner: &str, kind: &str, node: Node, scope: &Scope) {
        let Some(name) = type_name(node, self.src) else { return };
        let root = name.split('.').next().unwrap_or("");
        if self.is_variable(root, scope.locals) {
            return;
        }
        self.fact(owner, kind, &name, "", lines(node), true);
    }

    /// Says that the facts recorded since `from` are about what `handler` does:
    /// the route it serves, the queue it consumes. They move to the handler when
    /// that is a function declared in this file. With an inline callback or an
    /// imported name they stay with the enclosing symbol.
    fn handled_by(&mut self, handler: Node, from: usize, scope: &Scope) {
        if handler.kind() != "identifier" {
            return;
        }
        let name = text(handler, self.src);
        for index in from..self.out.facts.len() {
            self.reown(index, name, scope, Want::Function);
        }
    }

    /// Asks for the fact at `index` to be moved to the symbol called `name`,
    /// once the whole file has been read: a function can be used above its declaration.
    fn reown(&mut self, index: usize, name: &str, scope: &Scope, want: Want) {
        if self.is_variable(name, scope.locals) {
            return;
        }
        self.reowns.push(Reown {
            fact: index,
            name: name.to_string(),
            scope_qn: scope.qn.to_string(),
            want,
        });
    }

    /// The symbol that `name` stands for when written in the scope `scope_qn`:
    /// the nearest enclosing scope that declares it wins. Two candidates in one
    /// scope are never narrowed down to one.
    fn declared(
        &self,
        by_qn: &HashMap<String, BTreeSet<(String, String)>>,
        name: &str,
        scope_qn: &str,
        want: Want,
    ) -> Option<String> {
        let mut scope = scope_qn;
        loop {
            if let Some(found) = by_qn.get(&format!("{scope}.{name}")) {
                let of_kind = |kinds: &[&str]| -> Vec<&String> {
                    found
                        .iter()
                        .filter(|(kind, _)| kinds.contains(&kind.as_str()))
                        .map(|(_, id)| id)
                        .collect()
                };
                let fits = match want {
                    Want::Function => found
                        .iter()
                        .map(|(_, id)| id)
                        .filter(|id| self.bare_callable.contains(*id))
                        .collect(),
                    Want::Class => of_kind(&[kind::CLASS]),
                    // A value before a type of the same name: `export default Foo`
                    // exports the class, not the interface merged with it.
                    Want::Exportable => {
                        let values = of_kind(&[kind::FUNCTION, kind::CLASS, kind::ENUM]);
                        if values.is_empty() {
                            of_kind(&[kind::INTERFACE, kind::TYPE])
                        } else {
                            values
                        }
                    }
                };
                if let [only] = fits.as_slice() {
                    return Some((*only).clone());
                }
            }
            if scope.len() <= self.module_qn.len() {
                return None;
            }
            scope = scope.rsplit_once('.')?.0;
        }
    }

    /// Settles what had to wait for the whole file: handler facts move to their
    /// handler, and `export default name` marks the symbol it names.
    fn settle(&mut self) {
        let mut by_qn: HashMap<String, BTreeSet<(String, String)>> = HashMap::new();
        for s in &self.out.symbols {
            by_qn
                .entry(s.qualified_name.clone())
                .or_default()
                .insert((s.kind.clone(), s.id.clone()));
        }
        for r in std::mem::take(&mut self.reowns) {
            let Some(owner) = self.declared(&by_qn, &r.name, &r.scope_qn, r.want) else {
                continue;
            };
            if let Some(f) = self.out.facts.get_mut(r.fact) {
                *f = fact_site(
                    self.path,
                    &owner,
                    &f.kind,
                    &f.key,
                    &f.detail,
                    (f.line, f.end_line),
                    f.literal,
                );
            }
        }
        for export in std::mem::take(&mut self.default_exports) {
            if let Some(id) = self.declared(&by_qn, &export.name, &export.scope_qn, Want::Exportable) {
                self.fact(
                    &id,
                    fact::DEFAULT_EXPORT,
                    &export.name,
                    "",
                    (export.line, export.line),
                    true,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests;
