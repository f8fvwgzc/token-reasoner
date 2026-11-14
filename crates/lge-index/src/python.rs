//! Python extraction: symbols, call sites, import sites and fact sites.

mod scope;
mod strings;

use crate::facts::{self, fact_site, Piece, UrlShape};
use crate::{cap_body, unique_ids};
use anyhow::{Context, Result};
use lge_core::{fact, file_id, kind, lang, stable_id, symbol_id, CallSite, FileRec, ImportSite, ParsedFile, SymbolRec};
use scope::{bindings_of, is_builtin, method_names, Bindings};
use std::collections::{HashMap, HashSet};
use strings::{dotted, expr_text, text_of};
use tree_sitter::{Node, Parser};

/// Deepest syntax nesting that is followed. Real code stays far below it; the
/// limit keeps a generated file from overflowing the stack.
const MAX_DEPTH: usize = 400;

struct Ctx<'a> {
    src: &'a [u8],
    path: &'a str,
    service: &'a str,
    file_is_test: bool,
    /// Names bound at module level, and the string constants among them.
    module: Bindings,
    /// Router and blueprint variables: name -> (URL prefix, the prefix is a literal).
    prefixes: HashMap<String, (String, bool)>,
    /// Variables bound to an HTTP session or client.
    http_clients: HashSet<String>,
    /// Calls that are a decorator expression, by node id.
    decorator_calls: HashSet<usize>,
    /// Strings already searched for SQL (as part of a larger expression, or a
    /// docstring), by node id.
    sql_seen: HashSet<usize>,
    depth: usize,
    out: ParsedFile,
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
    /// Directly in a class body (not inside one of its methods).
    in_class: bool,
    /// Name of that class; "" elsewhere.
    class_name: &'s str,
    /// The class around that class (Django's `class Meta`).
    outer_class_id: Option<&'s str>,
    /// Methods of the nearest enclosing class, for `self.method` references.
    methods: Option<&'s HashSet<String>>,
    /// Names bound by the enclosing functions; `None` outside any function.
    locals: Option<&'s Bindings>,
}

fn text<'a>(n: Node, src: &'a [u8]) -> &'a str {
    n.utf8_text(src).unwrap_or("")
}

/// First and last line of a node, 1-based.
fn lines(n: Node) -> (i64, i64) {
    (n.start_position().row as i64 + 1, n.end_position().row as i64 + 1)
}

fn module_name(path: &str) -> String {
    let p = path.trim_end_matches(".py").trim_end_matches("/__init__");
    p.replace('/', ".")
}

/// Parse one Python file into symbols, call sites, import sites and fact sites.
pub fn parse_python(rel_path: &str, service: &str, src: &[u8], hash: &str) -> Result<ParsedFile> {
    let path = rel_path;
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_python::LANGUAGE.into())?;
    let tree = parser.parse(src, None).context("tree-sitter parse failed")?;
    let base = path.rsplit('/').next().unwrap_or(path);
    let root = tree.root_node();
    let mut ctx = Ctx {
        src,
        path,
        service,
        file_is_test: base.starts_with("test_")
            || base.ends_with("_test.py")
            || path.contains("/tests/")
            || path.starts_with("tests/"),
        module: bindings_of(root, src, None),
        prefixes: HashMap::new(),
        http_clients: HashSet::new(),
        decorator_calls: HashSet::new(),
        sql_seen: HashSet::new(),
        depth: 0,
        out: ParsedFile::default(),
    };
    ctx.prescan(root);
    ctx.out.file = Some(FileRec {
        id: file_id(path),
        path: path.to_string(),
        service: service.to_string(),
        lang: lang::PYTHON.into(),
        content_hash: hash.to_string(),
    });
    let module = module_name(path);
    let module_id = symbol_id(path, &module, kind::MODULE);
    ctx.out.symbols.push(SymbolRec {
        id: module_id.clone(),
        name: module.rsplit('.').next().unwrap_or(&module).to_string(),
        qualified_name: module.clone(),
        kind: kind::MODULE.into(),
        signature: String::new(),
        doc: docstring(root, src),
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
        in_class: false,
        class_name: "",
        outer_class_id: None,
        methods: None,
        locals: None,
    };
    visit(root, &mut ctx, &scope);
    unique_ids(&mut ctx.out);
    Ok(ctx.out)
}

fn docstring(body: Node, src: &[u8]) -> String {
    let mut c = body.walk();
    let first = body.named_children(&mut c).find(|n| n.kind() != "comment");
    if let Some(stmt) = first {
        if stmt.kind() == "expression_statement" {
            if let Some(s) = stmt.named_child(0) {
                if s.kind() == "string" {
                    let t = text(s, src).trim_matches(|ch| ch == '"' || ch == '\'');
                    let line = t.trim().lines().next().unwrap_or("").trim();
                    return line.chars().take(200).collect();
                }
            }
        }
    }
    String::new()
}

fn visit(node: Node, ctx: &mut Ctx, scope: &Scope) {
    if ctx.depth >= MAX_DEPTH {
        return;
    }
    ctx.depth += 1;
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "decorated_definition" => {
                // Decorator expressions are calls made by the enclosing scope.
                let mut c2 = child.walk();
                let decorators: Vec<Node> = child
                    .named_children(&mut c2)
                    .filter(|d| d.kind() == "decorator")
                    .collect();
                let def = child.child_by_field_name("definition");
                // What a decorator says (a route, a task name, a dependency) is
                // about the symbol it decorates, whose lines include it.
                let decorated = def.and_then(|d| symbol_key(d, ctx, scope)).map(|k| k.id);
                let inside = Scope {
                    owner_id: decorated.as_deref().unwrap_or(scope.owner_id),
                    ..*scope
                };
                for d in &decorators {
                    if let Some(call) = decorator_expr(*d).filter(|e| e.kind() == "call") {
                        ctx.decorator_calls.insert(call.id());
                    }
                    visit(*d, ctx, &inside);
                }
                if let Some(id) = def.and_then(|d| define(d, child, ctx, scope)) {
                    for d in &decorators {
                        decorator_facts(*d, ctx, scope, &id);
                    }
                }
            }
            "function_definition" | "class_definition" => {
                define(child, child, ctx, scope);
            }
            "call" => {
                record_call(child, ctx, scope.caller_id);
                call_facts(child, ctx, scope);
                visit(child, ctx, scope);
            }
            "import_statement" => record_import(child, ctx),
            "import_from_statement" => record_import_from(child, ctx),
            "expression_statement" => {
                skip_docstring(child, ctx);
                if scope.in_class {
                    model_table(child, ctx, scope);
                }
                visit(child, ctx, scope);
            }
            "string" | "concatenated_string" | "binary_operator" => {
                sql_facts(child, ctx, scope);
                // Interpolations can hold calls.
                visit(child, ctx, scope);
            }
            _ => visit(child, ctx, scope),
        }
    }
    ctx.depth -= 1;
}

/// Identity of the symbol a definition creates.
struct SymbolKey {
    name: String,
    kind: &'static str,
    qn: String,
    id: String,
}

fn symbol_key(def: Node, ctx: &Ctx, scope: &Scope) -> Option<SymbolKey> {
    let name = text(def.child_by_field_name("name")?, ctx.src).to_string();
    let kind = if def.kind() == "class_definition" {
        kind::CLASS
    } else if scope.in_class {
        kind::METHOD
    } else {
        kind::FUNCTION
    };
    let qn = format!("{}.{name}", scope.qn);
    let id = symbol_id(ctx.path, &qn, kind);
    Some(SymbolKey { name, kind, qn, id })
}

/// Records the symbol for a function or class and visits its inside. `outer` is
/// the definition with its decorators. Returns the symbol id.
fn define(def: Node, outer: Node, ctx: &mut Ctx, scope: &Scope) -> Option<String> {
    let SymbolKey { name, kind, qn, id } = symbol_key(def, ctx, scope)?;
    let is_class = kind == kind::CLASS;
    let body = def.child_by_field_name("body");
    let sig_end = body.map(|b| b.start_byte()).unwrap_or(def.end_byte());
    let signature: String = String::from_utf8_lossy(&ctx.src[def.start_byte()..sig_end])
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_end_matches(':')
        .chars()
        .take(240)
        .collect();
    ctx.out.symbols.push(SymbolRec {
        id: id.clone(),
        name: name.clone(),
        qualified_name: qn.clone(),
        kind: kind.into(),
        signature,
        doc: body.map(|b| docstring(b, ctx.src)).unwrap_or_default(),
        body: cap_body(&ctx.src[outer.start_byte()..outer.end_byte()]),
        file_path: ctx.path.to_string(),
        service: ctx.service.to_string(),
        start_line: outer.start_position().row as i64 + 1,
        end_line: outer.end_position().row as i64 + 1,
        is_test: ctx.file_is_test || name.starts_with("test_") || (is_class && name.starts_with("Test")),
    });
    // Base classes and default arguments are evaluated in the outer scope,
    // but for the call graph we attribute everything inside to this symbol.
    if is_class {
        inherits(def, ctx, scope, &id);
        let methods = method_names(def, ctx.src);
        let inner = Scope {
            qn: &qn,
            owner_id: &id,
            caller_id: &id,
            in_class: true,
            class_name: &name,
            outer_class_id: scope.in_class.then_some(scope.owner_id),
            methods: Some(&methods),
            locals: scope.locals,
        };
        visit(def, ctx, &inner);
    } else {
        let locals = bindings_of(def, ctx.src, scope.locals);
        let inner = Scope {
            qn: &qn,
            owner_id: &id,
            caller_id: &id,
            in_class: false,
            class_name: "",
            outer_class_id: None,
            methods: scope.methods,
            locals: Some(&locals),
        };
        visit(def, ctx, &inner);
    }
    Some(id)
}

fn record_call(call: Node, ctx: &mut Ctx, owner_id: &str) {
    let Some(f) = call.child_by_field_name("function") else {
        return;
    };
    let (name, receiver) = match f.kind() {
        "identifier" => (text(f, ctx.src).to_string(), String::new()),
        "attribute" => {
            let attr = f
                .child_by_field_name("attribute")
                .map(|a| text(a, ctx.src))
                .unwrap_or("");
            let obj = f.child_by_field_name("object").map(|o| text(o, ctx.src)).unwrap_or("");
            let recv = if obj == "self" || obj == "cls" {
                "self".to_string()
            } else {
                obj.chars().take(80).collect()
            };
            (attr.to_string(), recv)
        }
        _ => return,
    };
    if name.is_empty() {
        return;
    }
    let line = call.start_position().row as i64 + 1;
    let col = call.start_position().column.to_string();
    ctx.out.calls.push(CallSite {
        id: stable_id(&["call", ctx.path, owner_id, &name, &line.to_string(), &col]),
        caller_id: owner_id.to_string(),
        file_path: ctx.path.to_string(),
        name,
        receiver,
        line,
    });
}

fn push_import(ctx: &mut Ctx, module: &str, name: &str, alias: &str) {
    ctx.out.imports.push(ImportSite {
        id: stable_id(&["import", ctx.path, module, name, alias]),
        file_path: ctx.path.to_string(),
        module: module.to_string(),
        name: name.to_string(),
        alias: alias.to_string(),
    });
}

fn record_import(node: Node, ctx: &mut Ctx) {
    let mut c = node.walk();
    for n in node.children_by_field_name("name", &mut c) {
        match n.kind() {
            "dotted_name" => {
                let m = text(n, ctx.src).to_string();
                // `import a.b` binds `a`, but calls are written `a.b.f()`.
                push_import(ctx, &m, "", &m);
            }
            "aliased_import" => {
                let m = n
                    .child_by_field_name("name")
                    .map(|x| text(x, ctx.src))
                    .unwrap_or("")
                    .to_string();
                let a = n
                    .child_by_field_name("alias")
                    .map(|x| text(x, ctx.src))
                    .unwrap_or("")
                    .to_string();
                push_import(ctx, &m, "", &a);
            }
            _ => {}
        }
    }
}

fn record_import_from(node: Node, ctx: &mut Ctx) {
    let module = node
        .child_by_field_name("module_name")
        .map(|m| text(m, ctx.src))
        .unwrap_or("")
        .to_string();
    let mut c = node.walk();
    for n in node.children_by_field_name("name", &mut c) {
        match n.kind() {
            "dotted_name" => {
                let name = text(n, ctx.src).to_string();
                push_import(ctx, &module, &name, &name);
            }
            "aliased_import" => {
                let name = n
                    .child_by_field_name("name")
                    .map(|x| text(x, ctx.src))
                    .unwrap_or("")
                    .to_string();
                let a = n
                    .child_by_field_name("alias")
                    .map(|x| text(x, ctx.src))
                    .unwrap_or("")
                    .to_string();
                push_import(ctx, &module, &name, &a);
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Facts
// ---------------------------------------------------------------------------

/// Arguments of a call.
struct Args<'t, 's> {
    positional: Vec<Node<'t>>,
    keyword: Vec<(&'s str, Node<'t>)>,
}

impl<'t, 's> Args<'t, 's> {
    fn of(call: Node<'t>, src: &'s [u8]) -> Self {
        let mut args = Args {
            positional: Vec::new(),
            keyword: Vec::new(),
        };
        // `f(x for x in xs)` has a generator in place of the argument list.
        let Some(list) = call
            .child_by_field_name("arguments")
            .filter(|l| l.kind() == "argument_list")
        else {
            return args;
        };
        let mut c = list.walk();
        for n in list.named_children(&mut c) {
            match n.kind() {
                "comment" | "list_splat" | "dictionary_splat" => {}
                "keyword_argument" => {
                    if let (Some(k), Some(v)) = (n.child_by_field_name("name"), n.child_by_field_name("value")) {
                        args.keyword.push((text(k, src), v));
                    }
                }
                _ => args.positional.push(n),
            }
        }
        args
    }

    fn keyword(&self, name: &str) -> Option<Node<'t>> {
        self.keyword.iter().find(|(k, _)| *k == name).map(|(_, v)| *v)
    }

    /// The argument passed by keyword `name` or at position `pos`.
    fn get(&self, pos: usize, name: &str) -> Option<Node<'t>> {
        self.keyword(name).or_else(|| self.positional.get(pos).copied())
    }

    fn values(&self) -> impl Iterator<Item = Node<'t>> + '_ {
        self.positional
            .iter()
            .copied()
            .chain(self.keyword.iter().map(|(_, v)| *v))
    }
}

fn is_http_verb(word: &str) -> bool {
    ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"]
        .iter()
        .any(|v| word.eq_ignore_ascii_case(v))
}

/// The expression after `@`.
fn decorator_expr(decorator: Node) -> Option<Node> {
    let mut c = decorator.walk();
    let expr = decorator.named_children(&mut c).find(|n| n.kind() != "comment");
    expr
}

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

    /// The string a variable was assigned, when it was assigned once in the
    /// nearest scope that binds it.
    fn const_pieces<'b>(&'b self, name: &str, locals: Option<&'b Bindings>) -> Option<&'b Vec<Piece>> {
        match locals {
            Some(l) if l.names.contains(name) => l.strings.get(name),
            _ => self.module.strings.get(name),
        }
    }

    fn const_text(&self, name: &str, locals: Option<&Bindings>) -> Option<String> {
        self.const_pieces(name, locals).and_then(|p| facts::plain_text(p))
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
                    None => out.push_str(&facts::template_key(std::slice::from_ref(p))),
                },
            }
        }
        out
    }

    /// A name written at `node` (route, URL, queue, table) as `(key, literal)`.
    /// A variable assigned once to a string is read through, and is not literal.
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

    /// Finds router prefixes and HTTP client variables anywhere in the file.
    fn prescan(&mut self, root: Node) {
        let mut stack = vec![root];
        while let Some(n) = stack.pop() {
            match n.kind() {
                "assignment" => {
                    if let (Some(left), Some(right)) = (n.child_by_field_name("left"), n.child_by_field_name("right")) {
                        self.note_binding(left, right);
                    }
                }
                // `with requests.Session() as s:`
                "as_pattern" => {
                    let alias = n.child_by_field_name("alias").and_then(|a| a.named_child(0));
                    if let (Some(value), Some(alias)) = (n.named_child(0), alias) {
                        self.note_binding(alias, value);
                    }
                }
                _ => {}
            }
            let mut c = n.walk();
            stack.extend(n.named_children(&mut c));
        }
    }

    fn note_binding(&mut self, target: Node, value: Node) {
        if value.kind() != "call" {
            return;
        }
        let Some(name) = dotted(target, self.src) else { return };
        let Some(callee) = value.child_by_field_name("function").and_then(|f| dotted(f, self.src)) else {
            return;
        };
        let class = callee.rsplit('.').next().unwrap_or("");
        match class {
            "APIRouter" | "Blueprint" => {
                let args = Args::of(value, self.src);
                let keyword = if class == "APIRouter" { "prefix" } else { "url_prefix" };
                let prefix = match args.keyword(keyword) {
                    None => (String::new(), true),
                    // A prefix this file does not spell out stays as a hole.
                    Some(n) => self
                        .key_of(n, None)
                        .unwrap_or_else(|| (format!("${{{}}}", expr_text(n, self.src)), false)),
                };
                self.prefixes.insert(name, prefix);
            }
            "Session" | "session" | "Client" | "AsyncClient" | "ClientSession" => {
                self.http_clients.insert(name);
            }
            _ => {}
        }
    }

    /// `requests`, `httpx`, a variable bound to a session or client, or a name that says so.
    fn is_http_receiver(&self, object: &str) -> bool {
        if self.http_clients.contains(object) {
            return true;
        }
        let last = object
            .rsplit('.')
            .next()
            .unwrap_or(object)
            .trim_start_matches('_')
            .to_ascii_lowercase();
        ["requests", "httpx", "session", "client", "http"]
            .iter()
            .any(|w| last.contains(w))
    }

    /// `requests.get(url)`, `session.post(url)`, `conn.request("PUT", url)`.
    fn api_call(&mut self, call: Node, object: Node, method: &str, args: &Args, scope: &Scope) {
        let (verb, url) = if method == "request" {
            let verb = args
                .get(0, "method")
                .and_then(|n| self.key_of(n, scope.locals))
                .map(|(m, _)| m);
            (verb.filter(|m| is_http_verb(m)), args.get(1, "url"))
        } else {
            (Some(method.to_string()), args.get(0, "url"))
        };
        let Some((key, literal)) = url.and_then(|n| self.key_of(n, scope.locals)) else {
            return;
        };
        let is_call = match facts::url_shape(&key) {
            Some(UrlShape::Absolute) => true,
            // `cache.get("/k")` is not HTTP: a bare path needs an HTTP client in
            // front, or the unmistakable `request("GET", "/k")`.
            Some(UrlShape::Path) => {
                (method == "request" && verb.is_some()) || self.is_http_receiver(text(object, self.src))
            }
            None => false,
        };
        if is_call {
            let http_method = verb.map_or_else(|| "ANY".to_string(), |v| v.to_ascii_uppercase());
            self.fact(scope.owner_id, fact::API_CALL, &key, &http_method, lines(call), literal);
        }
    }

    /// pika `basic_publish(exchange, routing_key, body)`. On the default exchange
    /// (`""`) the routing key is the queue name; otherwise the exchange is the target.
    fn pika_publish(&mut self, call: Node, args: &Args, scope: &Scope) {
        let Some((exchange, exchange_literal)) = args.get(0, "exchange").and_then(|n| self.key_of(n, scope.locals))
        else {
            return;
        };
        if !exchange.is_empty() {
            self.fact(
                scope.owner_id,
                fact::QUEUE_PUBLISH,
                &exchange,
                "exchange",
                lines(call),
                exchange_literal,
            );
            return;
        }
        let Some((queue, queue_literal)) = args.get(1, "routing_key").and_then(|n| self.key_of(n, scope.locals)) else {
            return;
        };
        self.fact(
            scope.owner_id,
            fact::QUEUE_PUBLISH,
            &queue,
            "queue",
            lines(call),
            exchange_literal && queue_literal,
        );
    }

    /// pika `basic_consume(queue, on_message_callback)`; before pika 1.0 the
    /// callback came first.
    fn pika_consume(&mut self, call: Node, args: &Args, scope: &Scope) {
        let queue = args
            .get(0, "queue")
            .and_then(|n| self.key_of(n, scope.locals))
            .or_else(|| args.positional.get(1).and_then(|n| self.key_of(*n, scope.locals)));
        if let Some((queue, literal)) = queue {
            self.fact(
                scope.owner_id,
                fact::QUEUE_CONSUME,
                &queue,
                "queue",
                lines(call),
                literal,
            );
        }
    }

    /// celery `app.send_task("name")`.
    fn celery_send(&mut self, call: Node, args: &Args, scope: &Scope) {
        if let Some((name, literal)) = args.get(0, "name").and_then(|n| self.key_of(n, scope.locals)) {
            self.fact(
                scope.owner_id,
                fact::QUEUE_PUBLISH,
                &name,
                "topic",
                lines(call),
                literal,
            );
        }
    }

    /// A route decorator on the symbol `def_id`. `object` is what the decorator
    /// was called on (`app`, `router`, `bp`).
    fn expose(&mut self, at: Node, object: Node, methods: &[String], route: Option<Node>, scope: &Scope, def_id: &str) {
        let Some((route, route_literal)) = route.and_then(|n| self.key_of(n, scope.locals)) else {
            return;
        };
        let prefix = dotted(object, self.src).and_then(|name| self.prefixes.get(&name).cloned());
        // `@mock.patch("pkg.mod.name")` has the same shape; a route starts with a slash.
        let is_route = route.starts_with('/') || route.starts_with("${") || (route.is_empty() && prefix.is_some());
        if !is_route {
            return;
        }
        let (prefix, prefix_literal) = prefix.unwrap_or((String::new(), true));
        let key = facts::join_route(&prefix, &route);
        for method in methods {
            self.fact(
                def_id,
                fact::API_EXPOSE,
                &key,
                method,
                lines(at),
                route_literal && prefix_literal,
            );
        }
    }

    /// The key of a `reference` fact for a name passed around without being
    /// called, or `None` when the name cannot be a symbol: a local variable, a
    /// module-level variable, a builtin, or an attribute of `self` that is not a method.
    fn reference_key(&self, name: &str, scope: &Scope) -> Option<String> {
        let mut parts = name.split('.');
        let root = parts.next()?;
        if root == "self" || root == "cls" {
            let method = parts.next()?;
            let is_method = parts.next().is_none() && scope.methods.is_some_and(|m| m.contains(method));
            return is_method.then(|| format!("self.{method}"));
        }
        if self.is_variable(root, scope.locals) || is_builtin(root) {
            return None;
        }
        Some(name.to_string())
    }

    /// Names passed as arguments: `register(handler)`, `Depends(get_db)`, `session.query(Order)`.
    fn references(&mut self, args: &Args, scope: &Scope) {
        for arg in args.values() {
            let Some(name) = dotted(arg, self.src) else { continue };
            if let Some(key) = self.reference_key(&name, scope) {
                self.fact(scope.owner_id, fact::REFERENCE, &key, "", lines(arg), true);
                continue;
            }
            // `cur.execute(GET_ORDER)`: SQL kept in a module constant is run by
            // the function that passes it on. Reached through a variable, so not literal.
            let module_constant = scope.locals.is_some_and(|l| !l.names.contains(&name));
            if let Some(pieces) = self.module.strings.get(&name).filter(|_| module_constant).cloned() {
                self.sql_pieces(&pieces, lines(arg), scope, false);
            }
        }
    }

    /// Table facts for SQL text. `direct` is false when the text came through a variable.
    fn sql_pieces(&mut self, pieces: &[Piece], at: (i64, i64), scope: &Scope, direct: bool) {
        for r in facts::sql_refs(pieces) {
            let (table, literal) = match r.hole {
                None => (r.table, direct),
                // The table name is interpolated: known only when it is a constant of this file.
                Some(i) => {
                    let Some(Piece::Hole(expr)) = pieces.get(i) else {
                        continue;
                    };
                    let Some(table) = self.const_text(expr, scope.locals).and_then(|t| facts::table_name(&t)) else {
                        continue;
                    };
                    (table, false)
                }
            };
            let kind = if r.write { fact::DB_WRITE } else { fact::DB_READ };
            self.fact(scope.owner_id, kind, &table, "", at, literal);
        }
    }
}

/// A string that stands alone as a statement is documentation, not a query.
fn skip_docstring(stmt: Node, ctx: &mut Ctx) {
    if stmt.named_child_count() != 1 {
        return;
    }
    let Some(expr) = stmt
        .named_child(0)
        .filter(|e| matches!(e.kind(), "string" | "concatenated_string"))
    else {
        return;
    };
    if let Some(t) = text_of(expr, ctx.src) {
        ctx.sql_seen.extend(t.nodes);
    }
}

/// SQL in a string, an f-string or a concatenation.
fn sql_facts(node: Node, ctx: &mut Ctx, scope: &Scope) {
    if ctx.sql_seen.contains(&node.id()) {
        return;
    }
    let Some(t) = text_of(node, ctx.src) else { return };
    ctx.sql_seen.extend(t.nodes.iter().copied());
    ctx.sql_pieces(&t.pieces, lines(node), scope, true);
}

/// Facts from one call: HTTP client calls, queue calls, and names passed as arguments.
fn call_facts(call: Node, ctx: &mut Ctx, scope: &Scope) {
    let Some(f) = call.child_by_field_name("function") else {
        return;
    };
    let args = Args::of(call, ctx.src);
    if f.kind() == "attribute" {
        let method = f
            .child_by_field_name("attribute")
            .map(|a| text(a, ctx.src))
            .unwrap_or("");
        match method {
            // `@router.get("/x")` declares a route; it is not a client call.
            "get" | "post" | "put" | "patch" | "delete" | "request" if !ctx.decorator_calls.contains(&call.id()) => {
                if let Some(object) = f.child_by_field_name("object") {
                    ctx.api_call(call, object, method, &args, scope);
                }
            }
            "basic_publish" => ctx.pika_publish(call, &args, scope),
            "basic_consume" => ctx.pika_consume(call, &args, scope),
            "send_task" => ctx.celery_send(call, &args, scope),
            _ => {}
        }
    }
    ctx.references(&args, scope);
}

/// Facts a decorator declares about the symbol `def_id` it decorates.
fn decorator_facts(decorator: Node, ctx: &mut Ctx, scope: &Scope, def_id: &str) {
    let Some(expr) = decorator_expr(decorator) else { return };
    if expr.kind() != "call" {
        // `@login_required`: the decorated symbol references the decorator.
        let Some(name) = dotted(expr, ctx.src) else { return };
        let is_accessor = [".setter", ".getter", ".deleter"].iter().any(|s| name.ends_with(s));
        if let Some(key) = ctx.reference_key(&name, scope).filter(|_| !is_accessor) {
            ctx.fact(def_id, fact::REFERENCE, &key, "", lines(expr), true);
        }
        return;
    }
    let Some(f) = expr.child_by_field_name("function") else {
        return;
    };
    let args = Args::of(expr, ctx.src);
    let (object, name) = match f.kind() {
        "attribute" => (
            f.child_by_field_name("object"),
            f.child_by_field_name("attribute")
                .map(|a| text(a, ctx.src))
                .unwrap_or(""),
        ),
        "identifier" => (None, text(f, ctx.src)),
        _ => return,
    };
    match (object, name) {
        // FastAPI `@app.get("/x")`, Flask `@bp.post("/x")`.
        (Some(object), "get" | "post" | "put" | "patch" | "delete" | "head" | "options") => {
            ctx.expose(
                expr,
                object,
                &[name.to_ascii_uppercase()],
                args.get(0, "path"),
                scope,
                def_id,
            );
        }
        // Flask `@app.route("/x", methods=[...])`, FastAPI `@router.api_route(...)`.
        (Some(object), "route" | "api_route") => {
            let methods = route_methods(&args, ctx.src);
            let route = args.get(0, if name == "route" { "rule" } else { "path" });
            ctx.expose(expr, object, &methods, route, scope, def_id);
        }
        // celery `@app.task(name="orders.process")`, `@shared_task(name=...)`: the function consumes that task.
        (_, "task" | "shared_task") => {
            if let Some((task, literal)) = args.keyword("name").and_then(|n| ctx.key_of(n, scope.locals)) {
                ctx.fact(def_id, fact::QUEUE_CONSUME, &task, "topic", lines(expr), literal);
            }
        }
        _ => {}
    }
}

/// HTTP methods of a `route` decorator: the `methods` list, `GET` when there is
/// none, `ANY` when it cannot be read.
fn route_methods(args: &Args, src: &[u8]) -> Vec<String> {
    let any = || vec!["ANY".to_string()];
    let Some(list) = args.keyword("methods") else {
        return vec!["GET".to_string()];
    };
    if !matches!(list.kind(), "list" | "tuple" | "set") {
        return any();
    }
    let mut out = Vec::new();
    let mut c = list.walk();
    for item in list.named_children(&mut c).filter(|n| n.kind() != "comment") {
        match text_of(item, src)
            .filter(|t| t.plain)
            .and_then(|t| facts::plain_text(&t.pieces))
        {
            Some(verb) if is_http_verb(&verb) => out.push(verb.to_ascii_uppercase()),
            _ => return any(),
        }
    }
    if out.is_empty() {
        return any();
    }
    out
}

/// `__tablename__ = "orders"` in a class body (SQLAlchemy), or `db_table = "orders"`
/// in the `Meta` class of a model (Django): the model class maps that table.
fn model_table(stmt: Node, ctx: &mut Ctx, scope: &Scope) {
    let Some(assign) = stmt.named_child(0).filter(|a| a.kind() == "assignment") else {
        return;
    };
    let (Some(left), Some(right)) = (assign.child_by_field_name("left"), assign.child_by_field_name("right")) else {
        return;
    };
    if left.kind() != "identifier" {
        return;
    }
    let owner = match text(left, ctx.src) {
        "__tablename__" => scope.owner_id,
        "db_table" if scope.class_name == "Meta" => match scope.outer_class_id {
            Some(model) => model,
            None => return,
        },
        _ => return,
    };
    let Some((name, literal)) = ctx.key_of(right, scope.locals) else {
        return;
    };
    if let Some(table) = facts::table_name(&name) {
        ctx.fact(owner, fact::TABLE_MODEL, &table, "", lines(assign), literal);
    }
}

/// Base classes of a class. `object` is skipped, and so is a base that is a
/// variable of this file (`Base = declarative_base()`): no class symbol stands for it.
fn inherits(def: Node, ctx: &mut Ctx, scope: &Scope, class_id: &str) {
    let Some(bases) = def.child_by_field_name("superclasses") else {
        return;
    };
    let mut c = bases.walk();
    for base in bases.named_children(&mut c) {
        // `Generic[T]` inherits `Generic`.
        let named = if base.kind() == "subscript" {
            base.child_by_field_name("value")
        } else {
            Some(base)
        };
        let Some(name) = named.and_then(|n| dotted(n, ctx.src)) else {
            continue;
        };
        let root = name.split('.').next().unwrap_or("");
        if name == "object" || ctx.is_variable(root, scope.locals) {
            continue;
        }
        ctx.fact(class_id, fact::INHERITS, &name, "", lines(base), true);
    }
}

#[cfg(test)]
mod tests;
