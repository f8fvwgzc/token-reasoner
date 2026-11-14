//! HTTP facts: routes a file exposes (Express and its look-alikes, NestJS
//! controllers) and endpoints it calls (`fetch`, axios, got, ky, superagent,
//! node's `http`).

use super::strings::{dotted, expr_text};
use super::{has_spread, object_prop, text, Ctx, Scope};
use crate::facts::{self as shared, UrlShape};
use lge_core::fact;
use tree_sitter::Node;

/// What the object of a `.get("/x", ...)` call is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// Declares routes: an Express app or router, or something named like one.
    Router,
    /// Sends requests.
    HttpClient,
    Unknown,
}

/// Modules and packages whose name alone says "HTTP client".
const HTTP_MODULES: &[&str] = &["axios", "http", "https", "got", "ky", "superagent", "needle"];

/// Packages that are called directly: `axios(config)`, `got(url)`, `ky(url)`.
const HTTP_CALLABLES: &[&str] = &["axios", "got", "ky"];

fn is_http_verb(word: &str) -> bool {
    ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"]
        .iter()
        .any(|v| word.eq_ignore_ascii_case(v))
}

/// The HTTP method a verb-named function stands for.
fn verb_method(verb: &str) -> String {
    match verb {
        "all" | "All" => "ANY".to_string(),
        "del" => "DELETE".to_string(),
        _ => verb.to_ascii_uppercase(),
    }
}

fn is_route_verb(name: &str) -> bool {
    matches!(
        name,
        "get" | "post" | "put" | "patch" | "delete" | "del" | "head" | "options" | "all"
    )
}

impl Ctx<'_> {
    fn role(&self, object: Node) -> Role {
        let Some(name) = dotted(object, self.src) else {
            return Role::Unknown;
        };
        if self.routers.contains(&name) {
            return Role::Router;
        }
        if self.http_clients.contains_key(&name) {
            return Role::HttpClient;
        }
        let last = name
            .rsplit('.')
            .next()
            .unwrap_or(&name)
            .trim_start_matches(['_', '$'])
            .to_ascii_lowercase();
        if HTTP_MODULES.contains(&last.as_str()) {
            return Role::HttpClient;
        }
        let routes = last.ends_with("app")
            || last.contains("router")
            || last.ends_with("routes")
            || matches!(last.as_str(), "server" | "express" | "fastify");
        if routes {
            return Role::Router;
        }
        let requests = ["axios", "http", "client"].iter().any(|w| last.contains(w)) || last.ends_with("api");
        if requests {
            return Role::HttpClient;
        }
        Role::Unknown
    }

    /// The URL written at `url`. A path given to a client made with
    /// `axios.create({ baseURL })` is joined to that base.
    fn url_of(&self, url: Node, client: Option<&str>, scope: &Scope) -> Option<(String, bool)> {
        let (key, literal) = self.key_of(url, scope.locals)?;
        let base = client.and_then(|c| self.http_clients.get(c)).and_then(|b| b.as_ref());
        match base {
            Some((base, base_literal)) if shared::url_shape(&key) != Some(UrlShape::Absolute) => Some((
                format!("{}/{}", base.trim_end_matches('/'), key.trim_start_matches('/')),
                literal && *base_literal,
            )),
            _ => Some((key, literal)),
        }
    }

    /// The method named by the options of `fetch(url, options)`: `GET` when
    /// nothing says otherwise, `ANY` when the options cannot be read.
    fn options_method(&self, options: Option<Node>, scope: &Scope) -> String {
        let Some(options) = options else {
            return "GET".to_string();
        };
        match options.kind() {
            // `http.get(url, callback)`
            "arrow_function" | "function_expression" => "GET".to_string(),
            "object" => match object_prop(options, "method", self.src) {
                Some(value) => match self.key_of(value, scope.locals) {
                    Some((verb, _)) if is_http_verb(&verb) => verb.to_ascii_uppercase(),
                    _ => "ANY".to_string(),
                },
                None if has_spread(options) => "ANY".to_string(),
                None => "GET".to_string(),
            },
            _ => "ANY".to_string(),
        }
    }
}

/// HTTP facts from one call.
pub(super) fn call_facts(call: Node, args: &[Node], ctx: &mut Ctx, scope: &Scope) {
    let Some(f) = call.child_by_field_name("function") else {
        return;
    };
    match f.kind() {
        "identifier" => {
            let name = text(f, ctx.src);
            if name == "fetch" {
                fetch(call, args, ctx, scope);
            } else if HTTP_CALLABLES.contains(&name) || ctx.http_clients.contains_key(name) {
                client_call(call, name, args, ctx, scope);
            }
        }
        "member_expression" => {
            let (Some(object), Some(property)) = (f.child_by_field_name("object"), f.child_by_field_name("property"))
            else {
                return;
            };
            match text(property, ctx.src) {
                verb if is_route_verb(verb) => verb_call(call, object, verb, args, ctx, scope),
                "request" => request_call(call, object, args, ctx, scope),
                "fetch" if matches!(text(object, ctx.src), "window" | "globalThis" | "self") => {
                    fetch(call, args, ctx, scope);
                }
                _ => {}
            }
        }
        _ => {}
    }
}

/// `x.get(...)`: a route when `x` is a router, a request when it is a client.
fn verb_call(call: Node, object: Node, verb: &str, args: &[Node], ctx: &mut Ctx, scope: &Scope) {
    if let Some((router, route)) = route_chain(object, ctx.src) {
        if ctx.role(router) != Role::HttpClient {
            expose(call, route, verb, args, ctx, scope);
        }
        return;
    }
    match ctx.role(object) {
        Role::Router => {
            if let Some((route, handlers)) = args.split_first() {
                expose(call, *route, verb, handlers, ctx, scope);
            }
        }
        Role::HttpClient => client_verb(call, object, verb, args, true, ctx, scope),
        // `cache.get("/k")` is not HTTP, but nothing else takes a full URL.
        Role::Unknown => client_verb(call, object, verb, args, false, ctx, scope),
    }
}

/// The `.route("/x")` call that a chain of verb calls hangs off
/// (`router.route("/x").get(a).post(b)`), as the router and the route argument.
fn route_chain<'t>(object: Node<'t>, src: &[u8]) -> Option<(Node<'t>, Node<'t>)> {
    let mut cur = object;
    for _ in 0..16 {
        if cur.kind() != "call_expression" {
            return None;
        }
        let f = cur
            .child_by_field_name("function")
            .filter(|f| f.kind() == "member_expression")?;
        let property = text(f.child_by_field_name("property")?, src);
        let inner = f.child_by_field_name("object")?;
        if property == "route" {
            let route = super::arguments(cur).first().copied()?;
            return Some((inner, route));
        }
        if !is_route_verb(property) {
            return None;
        }
        cur = inner;
    }
    None
}

/// Express `app.get("/x", middleware, handler)`: the last argument handles the route.
fn expose(call: Node, route: Node, verb: &str, handlers: &[Node], ctx: &mut Ctx, scope: &Scope) {
    let Some((key, literal)) = ctx.key_of(route, scope.locals) else {
        return;
    };
    // `app.get("view engine")` reads a setting; a route has a handler and starts with a slash.
    let Some(handler) = handlers.last() else { return };
    let is_route = key.starts_with('/') || key.starts_with("${") || key == "*";
    if !is_route {
        return;
    }
    let recorded = ctx.out.facts.len();
    ctx.fact(
        scope.owner_id,
        fact::API_EXPOSE,
        &key,
        &verb_method(verb),
        super::lines(call),
        literal,
    );
    ctx.handled_by(*handler, recorded, scope);
}

/// `axios.get(url)`, `client.post(url, body)`, `http.get(url, callback)`.
fn client_verb(call: Node, object: Node, verb: &str, args: &[Node], is_client: bool, ctx: &mut Ctx, scope: &Scope) {
    if verb == "all" {
        return;
    }
    let Some(url) = args.first() else { return };
    let client = dotted(object, ctx.src);
    let Some((key, literal)) = ctx.url_of(*url, client.as_deref(), scope) else {
        return;
    };
    let is_call = match shared::url_shape(&key) {
        Some(UrlShape::Absolute) => true,
        Some(UrlShape::Path) => is_client,
        None => false,
    };
    if is_call {
        ctx.fact(
            scope.owner_id,
            fact::API_CALL,
            &key,
            &verb_method(verb),
            super::lines(call),
            literal,
        );
    }
}

/// `fetch(url, { method })`.
fn fetch(call: Node, args: &[Node], ctx: &mut Ctx, scope: &Scope) {
    let Some((key, literal)) = args.first().and_then(|url| ctx.key_of(*url, scope.locals)) else {
        return;
    };
    if shared::url_shape(&key).is_none() {
        return;
    }
    let method = ctx.options_method(args.get(1).copied(), scope);
    ctx.fact(
        scope.owner_id,
        fact::API_CALL,
        &key,
        &method,
        super::lines(call),
        literal,
    );
}

/// `axios({ url, method })`, `axios(url, config)`, `got(url, options)`.
fn client_call(call: Node, client: &str, args: &[Node], ctx: &mut Ctx, scope: &Scope) {
    let Some((url, options)) = url_and_options(args, ctx.src) else {
        return;
    };
    let Some((key, literal)) = ctx.url_of(url, Some(client), scope) else {
        return;
    };
    if shared::url_shape(&key).is_none() {
        return;
    }
    let method = ctx.options_method(options, scope);
    ctx.fact(
        scope.owner_id,
        fact::API_CALL,
        &key,
        &method,
        super::lines(call),
        literal,
    );
}

/// `axios.request({ url, method })`, `http.request(url, { method })`.
fn request_call(call: Node, object: Node, args: &[Node], ctx: &mut Ctx, scope: &Scope) {
    let Some((url, options)) = url_and_options(args, ctx.src) else {
        return;
    };
    let client = dotted(object, ctx.src);
    let Some((key, literal)) = ctx.url_of(url, client.as_deref(), scope) else {
        return;
    };
    let is_call = match shared::url_shape(&key) {
        Some(UrlShape::Absolute) => true,
        Some(UrlShape::Path) => ctx.role(object) == Role::HttpClient,
        None => false,
    };
    if is_call {
        let method = ctx.options_method(options, scope);
        ctx.fact(
            scope.owner_id,
            fact::API_CALL,
            &key,
            &method,
            super::lines(call),
            literal,
        );
    }
}

/// The URL and the options among the arguments of a request: either
/// `(url, options)` or one object with a `url` in it.
fn url_and_options<'t>(args: &[Node<'t>], src: &[u8]) -> Option<(Node<'t>, Option<Node<'t>>)> {
    let first = *args.first()?;
    if first.kind() == "object" {
        return Some((object_prop(first, "url", src)?, Some(first)));
    }
    Some((first, args.get(1).copied()))
}

// ---------------------------------------------------------------------------
// NestJS controllers
// ---------------------------------------------------------------------------

/// A route piece written as the first argument of a decorator: `("", literal)`
/// when there is none, a `${}` hole when it cannot be read.
fn route_piece(arg: Option<Node>, ctx: &Ctx, scope: &Scope) -> (String, bool) {
    let Some(arg) = arg else {
        return (String::new(), true);
    };
    // `@Controller({ path: "orders", version: "1" })`
    let written = if arg.kind() == "object" {
        match object_prop(arg, "path", ctx.src) {
            Some(path) => path,
            None => return (String::new(), true),
        }
    } else {
        arg
    };
    ctx.key_of(written, scope.locals)
        .unwrap_or_else(|| (format!("${{{}}}", expr_text(written, ctx.src)), false))
}

/// The route prefix a `@Controller(...)` decorator gives a class, if it has one.
pub(super) fn controller_prefix(decorators: &[Node], ctx: &Ctx, scope: &Scope) -> Option<(String, bool)> {
    decorators.iter().find_map(|d| {
        let call = super::decorator_expr(*d).filter(|e| e.kind() == "call_expression")?;
        let name = super::callee_name(call, ctx.src)?;
        name.ends_with("Controller")
            .then(|| route_piece(super::arguments(call).first().copied(), ctx, scope))
    })
}

/// `@Get(":id")` and friends on a method of a controller class.
pub(super) fn route_decorator(call: Node, name: &str, args: &[Node], def_id: &str, ctx: &mut Ctx, scope: &Scope) {
    if !matches!(
        name,
        "Get" | "Post" | "Put" | "Patch" | "Delete" | "Head" | "Options" | "All"
    ) {
        return;
    }
    // Without `@Controller` on the class these are somebody else's decorators.
    let Some((prefix, prefix_literal)) = scope.class.and_then(|c| c.prefix.clone()) else {
        return;
    };
    let (route, route_literal) = route_piece(args.first().copied(), ctx, scope);
    let key = shared::join_route(&format!("/{}", prefix.trim_matches('/')), &route);
    let key = if key.starts_with('/') { key } else { format!("/{key}") };
    ctx.fact(
        def_id,
        fact::API_EXPOSE,
        &key,
        &verb_method(name),
        super::lines(call),
        prefix_literal && route_literal,
    );
}
