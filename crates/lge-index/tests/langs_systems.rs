//! Review tests for the Tree-sitter tags extraction of the systems languages
//! (Rust, C, C++, Go, Zig) and its resolution: realistic code that a lazy
//! extractor gets wrong, checked for symbols (kind, qualified name, lines),
//! call sites (caller, receiver) and edge provenance. A same-file bare call is
//! `static`, a call matched by name across files is `inferred` or `unresolved`,
//! and nothing matched by name alone is ever `static`.

use lge_core::{fact, kind, method, unresolved_id, EdgeRec, EdgeType, ParsedFile, ResolveInput, Resolved, SymbolRec};

const C: &str = kind::CLASS;
const F: &str = kind::FUNCTION;
const M: &str = kind::METHOD;
const I: &str = kind::INTERFACE;
const MOD: &str = kind::MODULE;
const T: &str = kind::TYPE;
const E: &str = kind::ENUM;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse(path: &str, src: &str) -> ParsedFile {
    parse_bytes(path, src.as_bytes())
}

fn parse_bytes(path: &str, src: &[u8]) -> ParsedFile {
    lge_index::parse_file(path, "svc", src, "h")
        .unwrap_or_else(|e| panic!("{path}: {e:#}"))
        .unwrap_or_else(|| panic!("{path}: no extractor"))
}

fn resolve(files: &[&ParsedFile]) -> (ResolveInput, Resolved) {
    let mut input = ResolveInput::default();
    for p in files {
        input.files.extend(p.file.clone());
        input.symbols.extend(p.symbols.iter().cloned());
        input.calls.extend(p.calls.iter().cloned());
        input.imports.extend(p.imports.iter().cloned());
        input.facts.extend(p.facts.iter().cloned());
    }
    let res = lge_resolve::resolve(&input);
    (input, res)
}

fn listing(symbols: &[SymbolRec]) -> String {
    symbols
        .iter()
        .map(|s| {
            format!(
                "  {} {} {}-{} test={}",
                s.qualified_name, s.kind, s.start_line, s.end_line, s.is_test
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn sym<'a>(symbols: &'a [SymbolRec], qn: &str) -> &'a SymbolRec {
    let found: Vec<&SymbolRec> = symbols.iter().filter(|s| s.qualified_name == qn).collect();
    match found.as_slice() {
        [one] => one,
        _ => panic!("{} symbols named {qn}; symbols:\n{}", found.len(), listing(symbols)),
    }
}

/// 1-based number of the first line that contains `needle`.
fn line(src: &str, needle: &str) -> i64 {
    src.lines()
        .position(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("{needle:?} not in source")) as i64
        + 1
}

/// The first line that contains `first`, and the first line from there on that
/// is exactly `last` (trailing spaces ignored). An empty `last` is the first line.
fn span(src: &str, first: &str, last: &str) -> (i64, i64) {
    let start = line(src, first);
    if last.is_empty() {
        return (start, start);
    }
    let end = src
        .lines()
        .enumerate()
        .skip(start as usize - 1)
        .find(|(_, l)| l.trim_end() == last)
        .map(|(i, _)| i as i64 + 1)
        .unwrap_or_else(|| panic!("{last:?} not after {first:?}"));
    (start, end)
}

/// A symbol with this qualified name, kind and lines.
fn has(p: &ParsedFile, src: &str, qn: &str, kind: &str, first: &str, last: &str) {
    let s = sym(&p.symbols, qn);
    let (start, end) = span(src, first, last);
    assert_eq!(
        (s.kind.as_str(), s.start_line, s.end_line),
        (kind, start, end),
        "{qn}; symbols:\n{}",
        listing(&p.symbols)
    );
    assert!(!s.body.is_empty(), "{qn}: body");
}

fn calls_of(p: &ParsedFile, caller: &str) -> Vec<(String, String)> {
    let id = &sym(&p.symbols, caller).id;
    p.calls
        .iter()
        .filter(|c| c.caller_id == *id)
        .map(|c| (c.name.clone(), c.receiver.clone()))
        .collect()
}

fn has_call(p: &ParsedFile, caller: &str, name: &str, receiver: &str) {
    let calls = calls_of(p, caller);
    assert!(
        calls.contains(&(name.to_string(), receiver.to_string())),
        "{caller}: no call {receiver:?}.{name}; its calls: {calls:?}"
    );
}

fn qn_of(input: &ResolveInput, id: &str) -> String {
    input
        .symbols
        .iter()
        .find(|s| s.id == id)
        .map_or_else(|| id.to_string(), |s| s.qualified_name.clone())
}

fn edges_from(input: &ResolveInput, res: &Resolved, from: &str) -> Vec<(String, String, f64)> {
    let id = &sym(&input.symbols, from).id;
    res.edges
        .iter()
        .filter(|e| e.edge_type == EdgeType::Calls && e.from == *id)
        .map(|e| (qn_of(input, &e.to), e.method.clone(), e.confidence))
        .collect()
}

fn call_edge<'r>(input: &ResolveInput, res: &'r Resolved, from: &str, to: &str) -> &'r EdgeRec {
    let (from_id, to_id) = (&sym(&input.symbols, from).id, &sym(&input.symbols, to).id);
    res.edges
        .iter()
        .find(|e| e.edge_type == EdgeType::Calls && e.from == *from_id && e.to == *to_id)
        .unwrap_or_else(|| {
            panic!(
                "no call {from} -> {to}; calls from {from}: {:?}",
                edges_from(input, res, from)
            )
        })
}

/// A call the source names exactly: same file, through `self`/`this` or a type
/// of the same file.
fn static_call(input: &ResolveInput, res: &Resolved, from: &str, to: &str, confidence: f64) {
    let e = call_edge(input, res, from, to);
    assert_eq!(
        (e.method.as_str(), e.confidence),
        (method::STATIC, confidence),
        "{from} -> {to}"
    );
}

/// A call matched by name alone, to its only candidate.
fn inferred_call(input: &ResolveInput, res: &Resolved, from: &str, to: &str) {
    let e = call_edge(input, res, from, to);
    assert_eq!(e.method, method::INFERRED, "{from} -> {to}: {e:?}");
    assert!(e.confidence <= 0.6, "{from} -> {to}: {e:?}");
}

/// A call with several candidates: an edge to the placeholder, never to one of them.
fn unresolved_call(input: &ResolveInput, res: &Resolved, from: &str, name: &str) {
    let id = &sym(&input.symbols, from).id;
    let e = res
        .edges
        .iter()
        .find(|e| e.edge_type == EdgeType::Calls && e.from == *id && e.to == unresolved_id(name))
        .unwrap_or_else(|| {
            panic!(
                "no unresolved call {from} -> {name}; calls from {from}: {:?}",
                edges_from(input, res, from)
            )
        });
    assert_eq!((e.method.as_str(), e.confidence), (method::UNRESOLVED, 0.0));
}

/// These languages have no import sites, so only the same file can make a call
/// static: a static call edge that crosses files was matched by name alone.
fn static_stays_in_file(input: &ResolveInput, res: &Resolved) {
    let file = |id: &str| input.symbols.iter().find(|s| s.id == id).map(|s| s.file_path.clone());
    for e in res
        .edges
        .iter()
        .filter(|e| e.edge_type == EdgeType::Calls && e.method == method::STATIC)
    {
        assert_eq!(
            file(&e.from),
            file(&e.to),
            "static call across files: {} -> {}",
            qn_of(input, &e.from),
            qn_of(input, &e.to)
        );
    }
}

fn rationale(p: &ParsedFile) -> Vec<(String, String)> {
    p.facts
        .iter()
        .filter(|f| f.kind == fact::RATIONALE)
        .map(|f| {
            let owner = p.symbols.iter().find(|s| s.id == f.symbol_id).unwrap();
            (owner.qualified_name.clone(), f.key.clone())
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Rust
// ---------------------------------------------------------------------------

const RUST_CART: &str = r##"//! Prices a shopping cart.

use std::fmt;

/// The prices of the items a customer picked.
#[derive(Debug, Clone, Default)]
pub struct Cart {
    items: Vec<u64>,
}

/// Something with a price.
pub trait Priced {
    fn price(&self) -> u64;

    /// The price with tax.
    fn gross(&self) -> u64 {
        apply_tax(self.price())
    }
}

impl Cart {
    /// An empty cart.
    pub fn new() -> Self {
        Self::with_items(Vec::new())
    }

    pub fn with_items(items: Vec<u64>) -> Self {
        Cart { items }
    }

    /// Adds up the item prices.
    #[inline]
    pub fn subtotal(&self) -> u64 {
        // NOTE: an empty cart costs nothing
        self.items.iter().copied().sum::<u64>()
    }

    pub fn discounted(&self, pct: u64) -> u64 {
        let apply = |x: u64| x - x * pct / 100;
        self.items.iter().map(|p| apply(round_cents(*p))).sum()
    }
}

impl Priced for Cart {
    fn price(&self) -> u64 {
        self.subtotal()
    }
}

impl fmt::Display for Cart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.subtotal().to_string())
    }
}

/// Adds the sales tax to an amount.
pub fn apply_tax(amount: u64) -> u64 {
    amount * 108 / 100
}

fn round_cents(x: u64) -> u64 {
    x
}

pub fn checkout(cart: &Cart) -> String {
    let total = Cart::new().subtotal() + cart.gross();
    crate::money::format_money(apply_tax(total))
}
"##;

const RUST_MONEY: &str = r##"/// Renders cents as dollars.
pub fn format_money(cents: u64) -> String {
    format!("${}.{:02}", cents / 100, cents % 100)
}
"##;

#[test]
fn rust_impl_blocks_traits_and_closures() {
    let src = RUST_CART;
    let p = parse("shop/cart.rs", src);
    assert_eq!(p.file.as_ref().unwrap().lang, "rust");
    let module = sym(&p.symbols, "shop.cart");
    assert_eq!(
        (module.kind.as_str(), module.doc.as_str()),
        (MOD, "Prices a shopping cart.")
    );
    // Three impl blocks of a struct the file defines add no symbol of their own.
    has(&p, src, "shop.cart.Cart", C, "pub struct Cart", "}");
    has(&p, src, "shop.cart.Priced", I, "pub trait Priced", "}");
    has(&p, src, "shop.cart.Priced.gross", M, "fn gross", "    }");
    has(&p, src, "shop.cart.Cart.new", M, "pub fn new", "    }");
    has(&p, src, "shop.cart.Cart.with_items", M, "pub fn with_items", "    }");
    has(&p, src, "shop.cart.Cart.subtotal", M, "pub fn subtotal", "    }");
    has(&p, src, "shop.cart.Cart.discounted", M, "pub fn discounted", "    }");
    has(&p, src, "shop.cart.Cart.price", M, "fn price(&self) -> u64 {", "    }");
    has(&p, src, "shop.cart.Cart.fmt", M, "fn fmt", "    }");
    has(&p, src, "shop.cart.apply_tax", F, "pub fn apply_tax", "}");
    has(&p, src, "shop.cart.round_cents", F, "fn round_cents", "}");
    has(&p, src, "shop.cart.checkout", F, "pub fn checkout", "}");
    // A closure bound with `let` is no symbol.
    assert!(!p.symbols.iter().any(|s| s.name == "apply"), "{}", listing(&p.symbols));

    // Doc lines skip attributes; signatures stop at the body.
    let doc = |qn: &str| sym(&p.symbols, qn).doc.clone();
    assert_eq!(doc("shop.cart.Cart"), "The prices of the items a customer picked.");
    assert_eq!(doc("shop.cart.Cart.subtotal"), "Adds up the item prices.");
    assert_eq!(doc("shop.cart.Cart.new"), "An empty cart.");
    assert_eq!(doc("shop.cart.Priced.gross"), "The price with tax.");
    assert_eq!(
        sym(&p.symbols, "shop.cart.Cart.subtotal").signature,
        "pub fn subtotal(&self) -> u64"
    );
    assert_eq!(
        sym(&p.symbols, "shop.cart.checkout").signature,
        "pub fn checkout(cart: &Cart) -> String"
    );

    // Calls: `Self::` is self, `Type::f` and module paths keep the path, chains
    // keep the call they are made on, closures belong to their function.
    has_call(&p, "shop.cart.Cart.new", "with_items", "self");
    has_call(&p, "shop.cart.Cart.new", "new", "Vec");
    has_call(&p, "shop.cart.Cart.subtotal", "iter", "self.items");
    has_call(&p, "shop.cart.Cart.subtotal", "sum", "self.items.iter().copied()");
    has_call(&p, "shop.cart.Cart.discounted", "round_cents", "");
    has_call(&p, "shop.cart.Cart.discounted", "apply", "");
    has_call(&p, "shop.cart.Cart.discounted", "map", "self.items.iter()");
    has_call(&p, "shop.cart.Cart.price", "subtotal", "self");
    has_call(&p, "shop.cart.Cart.fmt", "subtotal", "self");
    has_call(&p, "shop.cart.Cart.fmt", "write_str", "f");
    has_call(&p, "shop.cart.Priced.gross", "apply_tax", "");
    has_call(&p, "shop.cart.Priced.gross", "price", "self");
    has_call(&p, "shop.cart.checkout", "new", "Cart");
    has_call(&p, "shop.cart.checkout", "subtotal", "Cart::new()");
    has_call(&p, "shop.cart.checkout", "gross", "cart");
    has_call(&p, "shop.cart.checkout", "format_money", "crate::money");
    has_call(&p, "shop.cart.checkout", "apply_tax", "");
    assert!(p.imports.is_empty());

    assert_eq!(
        rationale(&p),
        [(
            "shop.cart.Cart.subtotal".to_string(),
            "NOTE: an empty cart costs nothing".to_string()
        )]
    );
}

#[test]
fn rust_resolution_keeps_provenance() {
    let cart = parse("shop/cart.rs", RUST_CART);
    let money = parse("shop/money.rs", RUST_MONEY);
    let report = parse(
        "shop/report.rs",
        "pub fn report(cart: &crate::cart::Cart) -> String {\n    let s = summarize(cart.subtotal());\n    format_money(s)\n}\n",
    );
    let a = parse("shop/a.rs", "pub fn summarize(x: u64) -> u64 {\n    x\n}\n");
    let b = parse("shop/b.rs", "pub fn summarize(x: u64) -> u64 {\n    x + 1\n}\n");
    let (input, res) = resolve(&[&cart, &money, &report, &a, &b]);

    static_call(&input, &res, "shop.cart.Cart.new", "shop.cart.Cart.with_items", 0.95);
    static_call(&input, &res, "shop.cart.Cart.price", "shop.cart.Cart.subtotal", 0.95);
    static_call(&input, &res, "shop.cart.Cart.fmt", "shop.cart.Cart.subtotal", 0.95);
    static_call(&input, &res, "shop.cart.Priced.gross", "shop.cart.apply_tax", 0.95);
    static_call(&input, &res, "shop.cart.Cart.discounted", "shop.cart.round_cents", 0.95);
    static_call(&input, &res, "shop.cart.checkout", "shop.cart.apply_tax", 0.95);
    // `Cart::new()`: a type of the same file names the method.
    let e = call_edge(&input, &res, "shop.cart.checkout", "shop.cart.Cart.new");
    assert_eq!(e.method, method::STATIC);

    // `self.price()` in a trait default method: which impl runs is not written.
    let e = call_edge(&input, &res, "shop.cart.Priced.gross", "shop.cart.Cart.price");
    assert_ne!(e.method, method::STATIC);
    // Into another file: by name only.
    inferred_call(&input, &res, "shop.cart.checkout", "shop.money.format_money");
    inferred_call(&input, &res, "shop.report.report", "shop.money.format_money");
    inferred_call(&input, &res, "shop.report.report", "shop.cart.Cart.subtotal");
    unresolved_call(&input, &res, "shop.report.report", "summarize");
    static_stays_in_file(&input, &res);
}

const RUST_GEO: &str = r##"pub mod geometry {
    pub mod shapes {
        pub struct Stack<T> {
            items: Vec<T>,
        }

        impl<T: Clone> Stack<T> {
            pub fn push(&mut self, item: T) {
                self.items.push(item);
                self.grow();
            }

            fn grow(&mut self) {}
        }

        pub fn größe(s: &Stack<u8>) -> usize {
            s.items.len()
        }
    }

    pub fn area_of(s: &shapes::Stack<u8>) -> usize {
        shapes::größe(s)
    }
}

macro_rules! square {
    ($x:expr) => {
        $x * $x
    };
}

pub fn parse_all<T: std::str::FromStr>(xs: &[&str]) -> Vec<T> {
    xs.iter().filter_map(|s| s.parse::<T>().ok()).collect::<Vec<_>>()
}

pub fn run() -> u64 {
    let n: u64 = square!(3);
    let v: Vec<u32> = parse_all::<u32>(&["1"]);
    n + v.len() as u64
}

pub type Price = u64;

pub enum Either<L, R> {
    Left(L),
    Right(R),
}

pub(crate) async fn fetch(id: u64) -> Option<u64> {
    Some(zero() + id)
}

const fn zero() -> u64 {
    0
}

pub fn process<T>(items: &[T]) -> Vec<T>
where
    T: Clone,
{
    items.to_vec()
}
"##;

#[test]
fn rust_nested_modules_generic_impls_macros_and_unicode() {
    let src = RUST_GEO;
    let p = parse("geo/lib.rs", src);
    has(&p, src, "geo.lib.geometry", MOD, "pub mod geometry", "}");
    has(&p, src, "geo.lib.geometry.shapes", MOD, "pub mod shapes", "    }");
    has(
        &p,
        src,
        "geo.lib.geometry.shapes.Stack",
        C,
        "pub struct Stack",
        "        }",
    );
    has(
        &p,
        src,
        "geo.lib.geometry.shapes.Stack.push",
        M,
        "pub fn push",
        "            }",
    );
    has(&p, src, "geo.lib.geometry.shapes.Stack.grow", M, "fn grow", "");
    has(&p, src, "geo.lib.geometry.shapes.größe", F, "pub fn größe", "        }");
    has(&p, src, "geo.lib.geometry.area_of", F, "pub fn area_of", "    }");
    has(&p, src, "geo.lib.square", F, "macro_rules! square", "}");
    has(&p, src, "geo.lib.parse_all", F, "pub fn parse_all", "}");
    has(&p, src, "geo.lib.run", F, "pub fn run", "}");
    has(&p, src, "geo.lib.Price", T, "pub type Price", "");
    has(&p, src, "geo.lib.Either", E, "pub enum Either", "}");
    has(&p, src, "geo.lib.fetch", F, "pub(crate) async fn fetch", "}");
    has(&p, src, "geo.lib.zero", F, "const fn zero", "}");
    has(&p, src, "geo.lib.process", F, "pub fn process", "}");
    assert_eq!(
        sym(&p.symbols, "geo.lib.process").signature,
        "pub fn process<T>(items: &[T]) -> Vec<T>"
    );
    assert_eq!(
        sym(&p.symbols, "geo.lib.fetch").signature,
        "pub(crate) async fn fetch(id: u64) -> Option<u64>"
    );
    assert_eq!(sym(&p.symbols, "geo.lib.geometry.shapes.größe").name, "größe");

    has_call(&p, "geo.lib.geometry.shapes.Stack.push", "grow", "self");
    has_call(&p, "geo.lib.geometry.shapes.Stack.push", "push", "self.items");
    has_call(&p, "geo.lib.geometry.shapes.größe", "len", "s.items");
    has_call(&p, "geo.lib.geometry.area_of", "größe", "shapes");
    has_call(&p, "geo.lib.parse_all", "filter_map", "xs.iter()");
    assert!(calls_of(&p, "geo.lib.parse_all").iter().any(|(n, _)| n == "collect"));
    assert!(calls_of(&p, "geo.lib.parse_all")
        .iter()
        .any(|(n, r)| n == "parse" && r == "s"));
    has_call(&p, "geo.lib.run", "square", "");
    has_call(&p, "geo.lib.run", "parse_all", "");

    let (input, res) = resolve(&[&p]);
    static_call(
        &input,
        &res,
        "geo.lib.geometry.shapes.Stack.push",
        "geo.lib.geometry.shapes.Stack.grow",
        0.95,
    );
    static_call(&input, &res, "geo.lib.run", "geo.lib.square", 0.95);
    static_call(&input, &res, "geo.lib.run", "geo.lib.parse_all", 0.95);
    static_call(&input, &res, "geo.lib.fetch", "geo.lib.zero", 0.95);
    call_edge(
        &input,
        &res,
        "geo.lib.geometry.area_of",
        "geo.lib.geometry.shapes.größe",
    );
    static_stays_in_file(&input, &res);
}

const RUST_PRICING: &str = r##"pub fn total(items: &[u64]) -> u64 {
    items.iter().sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_every_item() {
        let t = total(&[1, 2]);
        assert!(t == 3);
    }
}
"##;

/// Rust marks tests with `#[test]`, not with a name: `fn sums_every_item` in a
/// `#[cfg(test)] mod tests` is a test, and what it calls is tested by it.
#[test]
fn rust_test_functions_inside_cfg_test_modules_are_tests() {
    let p = parse("shop/pricing.rs", RUST_PRICING);
    assert!(sym(&p.symbols, "shop.pricing.tests").is_test);
    assert!(!sym(&p.symbols, "shop.pricing.total").is_test);
    has_call(&p, "shop.pricing.tests.sums_every_item", "total", "");
    let (input, res) = resolve(&[&p]);
    static_call(
        &input,
        &res,
        "shop.pricing.tests.sums_every_item",
        "shop.pricing.total",
        0.95,
    );
    assert!(
        sym(&p.symbols, "shop.pricing.tests.sums_every_item").is_test,
        "a #[test] fn inside mod tests is a test:\n{}",
        listing(&p.symbols)
    );
    let (total, test) = (
        &sym(&input.symbols, "shop.pricing.total").id,
        &sym(&input.symbols, "shop.pricing.tests.sums_every_item").id,
    );
    assert!(
        res.edges
            .iter()
            .any(|e| e.edge_type == EdgeType::TestedBy && e.from == *total && e.to == *test),
        "total is tested by sums_every_item"
    );
}

const RUST_MACRO_ARGS: &str = r##"pub fn total(items: &[u64]) -> u64 {
    items.iter().sum()
}

pub fn describe(items: &[u64]) -> String {
    format!("total {}", total(items))
}

#[test]
fn test_total_of_nothing() {
    assert_eq!(total(&[]), 0);
}
"##;

/// `assert_eq!(f(x), ..)`, `format!("{}", f(x))`, `vec![f(x)]`: the arguments
/// of a macro are a token tree, but they are calls all the same, and in tests
/// they are most of the calls.
#[test]
fn rust_calls_inside_macro_arguments_are_extracted() {
    let p = parse("shop/describe.rs", RUST_MACRO_ARGS);
    assert!(sym(&p.symbols, "shop.describe.test_total_of_nothing").is_test);
    has_call(&p, "shop.describe.describe", "total", "");
    has_call(&p, "shop.describe.test_total_of_nothing", "total", "");
}

const RUST_TRAIT: &str = r##"pub trait Shape {
    fn area(&self) -> f64;

    fn summary(&self) -> String {
        let a = self.area();
        a.to_string()
    }
}

pub struct Square(f64);

impl Shape for Square {
    fn area(&self) -> f64 {
        self.0 * self.0
    }
}

pub fn show(s: &Square) -> String {
    s.summary()
}
"##;

/// A trait method with a body is code that runs: `s.summary()` has exactly one
/// candidate in the repository, the default method.
#[test]
fn rust_trait_default_methods_are_call_targets() {
    let p = parse("geo/shape.rs", RUST_TRAIT);
    has(&p, RUST_TRAIT, "geo.shape.Shape", I, "pub trait Shape", "}");
    has(&p, RUST_TRAIT, "geo.shape.Shape.summary", M, "fn summary", "    }");
    has(
        &p,
        RUST_TRAIT,
        "geo.shape.Square.area",
        M,
        "fn area(&self) -> f64 {",
        "    }",
    );
    has_call(&p, "geo.shape.show", "summary", "s");
    let (input, res) = resolve(&[&p]);
    inferred_call(&input, &res, "geo.shape.show", "geo.shape.Shape.summary");
}

const RUST_ENUM: &str = r##"pub enum Shape {
    Circle(f64),
    Square(f64),
}

impl Shape {
    pub fn area(&self) -> f64 {
        match self {
            Shape::Circle(r) => 3.14 * r * r,
            Shape::Square(s) => s * s,
        }
    }

    pub fn doubled(&self) -> f64 {
        self.area() * 2.0
    }
}
"##;

/// `self.area()` inside `impl Shape` names `Shape::area` as surely for an enum
/// as for a struct.
#[test]
fn rust_self_calls_in_an_enum_impl_are_static() {
    let p = parse("geo/kind.rs", RUST_ENUM);
    has(&p, RUST_ENUM, "geo.kind.Shape", E, "pub enum Shape", "}");
    has(&p, RUST_ENUM, "geo.kind.Shape.area", M, "pub fn area", "    }");
    has(&p, RUST_ENUM, "geo.kind.Shape.doubled", M, "pub fn doubled", "    }");
    has_call(&p, "geo.kind.Shape.doubled", "area", "self");
    // Patterns are not calls.
    assert!(!p.calls.iter().any(|c| c.name == "Circle"), "{:?}", p.calls);
    let (input, res) = resolve(&[&p]);
    static_call(&input, &res, "geo.kind.Shape.doubled", "geo.kind.Shape.area", 0.95);
}

const RUST_PATH_IMPL: &str = r##"use std::fmt;

mod model {
    pub struct Cart {
        pub items: Vec<u64>,
    }
}

impl model::Cart {
    pub fn subtotal(&self) -> u64 {
        self.items.iter().sum()
    }

    pub fn total(&self) -> u64 {
        self.subtotal() + 1
    }
}

impl fmt::Display for model::Cart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("cart")
    }
}

pub fn subtotal(xs: &[u64]) -> u64 {
    xs.iter().sum()
}

pub fn run(xs: &[u64]) -> u64 {
    subtotal(xs)
}

impl<'a> IntoIterator for &'a model::Cart {
    type Item = &'a u64;
    type IntoIter = std::slice::Iter<'a, u64>;
    fn into_iter(self) -> Self::IntoIter {
        self.items.iter()
    }
}
"##;

/// `impl model::Cart`, `impl Display for model::Cart` and `impl IntoIterator
/// for &'a Cart`: the functions are methods, not free functions, and the method
/// `subtotal` does not merge with the free `subtotal` (which `run` calls).
#[test]
fn rust_impl_of_a_path_or_reference_type_holds_methods() {
    let p = parse("shop/path.rs", RUST_PATH_IMPL);
    let mut wrong = Vec::new();
    for name in ["total", "fmt", "into_iter"] {
        for s in p.symbols.iter().filter(|s| s.name == name && s.kind != M) {
            wrong.push(format!("{} is a {}", s.qualified_name, s.kind));
        }
    }
    let subtotals = p.symbols.iter().filter(|s| s.name == "subtotal").count();
    if subtotals != 2 {
        wrong.push(format!(
            "{subtotals} symbols named subtotal, not a method and a function"
        ));
    }
    let free = line(RUST_PATH_IMPL, "pub fn subtotal(xs");
    let (input, res) = resolve(&[&p]);
    for e in res.edges.iter().filter(|e| e.edge_type == EdgeType::Calls) {
        let to = input.symbols.iter().find(|s| s.id == e.to);
        if let Some(to) = to.filter(|t| t.name == "subtotal" && e.method == method::STATIC) {
            if qn_of(&input, &e.from) == "shop.path.run" && to.start_line != free {
                wrong.push(format!(
                    "run -> subtotal is static to lines {}-{}",
                    to.start_line, to.end_line
                ));
            }
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}\n{}", listing(&p.symbols));
}

const RUST_MACRO_NAME: &str = r##"pub fn format(code: &str) -> String {
    format!("{code}\n")
}

pub fn query(sql: &str) -> usize {
    sql.len()
}

pub fn load() -> usize {
    query!("SELECT 1")
}
"##;

/// `format!` and `query!` are macros: they live in another namespace than the
/// functions `format` and `query`, so the source does not name those functions.
#[test]
fn rust_macro_invocations_do_not_resolve_to_a_function_of_the_same_name() {
    let p = parse("db/repo.rs", RUST_MACRO_NAME);
    let (input, res) = resolve(&[&p]);
    for (from, to) in [("db.repo.format", "db.repo.format"), ("db.repo.load", "db.repo.query")] {
        let (from_id, to_id) = (&sym(&input.symbols, from).id, &sym(&input.symbols, to).id);
        let wrong = res
            .edges
            .iter()
            .find(|e| e.edge_type == EdgeType::Calls && e.from == *from_id && e.to == *to_id);
        assert!(wrong.is_none(), "{from} -> {to}: {wrong:?}");
    }
}

#[test]
fn rust_broken_empty_and_non_utf8_files_do_not_panic() {
    let src = "pub fn ok() -> u32 {\n    helper()\n}\n\nfn helper() -> u32 {\n    1\n}\n\npub fn broken( -> {\n    let x = ;\nimpl Foo {\n";
    let p = parse("x/broken.rs", src);
    sym(&p.symbols, "x.broken.ok");
    sym(&p.symbols, "x.broken.helper");
    let (input, res) = resolve(&[&p]);
    static_call(&input, &res, "x.broken.ok", "x.broken.helper", 0.95);

    let empty = parse("x/empty.rs", "");
    assert_eq!(empty.symbols.len(), 1);
    assert_eq!(empty.symbols[0].kind, MOD);
    assert!(empty.calls.is_empty());
    let comments = parse("x/only.rs", "// nothing here\n/* at all */\n");
    assert_eq!(comments.symbols.len(), 1);

    let bytes = b"// caf\xff\xfe\nfn a() { b() }\nfn b() {}\n";
    let p = parse_bytes("x/bytes.rs", bytes);
    has_call(&p, "x.bytes.a", "b", "");
}

/// A name that starts with `test` is a test only where the language says so:
/// `test_and_set` is an atomic, `TestConnection` a health check, and
/// `testing_enabled` a flag. None of them is a test, and nothing is tested by them.
#[test]
fn names_starting_with_test_outside_test_files_are_not_tests() {
    let c = parse(
        "lib/spinlock.c",
        "int test_and_set(volatile int *lock) {\n    return __sync_lock_test_and_set(lock, 1);\n}\n\nvoid spin_lock(volatile int *lock) {\n    while (test_and_set(lock)) {}\n}\n",
    );
    let go = parse(
        "client/client.go",
        "package client\n\ntype Client struct{}\n\nfunc (c *Client) TestConnection() error { return c.ping() }\n\nfunc (c *Client) ping() error { return nil }\n",
    );
    let rs = parse(
        "src/config.rs",
        "pub fn testing_enabled() -> bool {\n    std::env::var(\"X\").is_ok()\n}\n",
    );
    let mut wrong: Vec<String> = [
        (&c, "lib.spinlock.test_and_set"),
        (&go, "client.client.Client.TestConnection"),
        (&rs, "src.config.testing_enabled"),
    ]
    .into_iter()
    .filter(|(p, qn)| sym(&p.symbols, qn).is_test)
    .map(|(_, qn)| format!("{qn} is marked a test"))
    .collect();
    let (input, res) = resolve(&[&c, &go, &rs]);
    wrong.extend(
        res.edges
            .iter()
            .filter(|e| e.edge_type == EdgeType::TestedBy)
            .map(|e| format!("{} tested by {}", qn_of(&input, &e.from), qn_of(&input, &e.to))),
    );
    assert!(wrong.is_empty(), "{wrong:#?}");
}

// ---------------------------------------------------------------------------
// Go
// ---------------------------------------------------------------------------

const GO_STORE: &str = r##"// Package store keeps values.
package store

import (
    "fmt"
    "strings"
)

// Stack is a LIFO.
type Stack struct {
    items []int
}

// Push adds v on top.
func (s *Stack) Push(v int) {
    s.items = append(s.items, v)
    s.grow()
}

func (s *Stack) grow() {}

// Map applies f to every element.
func Map[T, U any](xs []T, f func(T) U) []U {
    out := make([]U, 0, len(xs))
    for _, x := range xs {
        out = append(out, f(x))
    }
    return out
}

// Pricer has a price.
type Pricer interface {
    Price() int
}

type Cents int

func Labels(xs []int) string {
    names := Map(xs, func(x int) string {
        // WHY: labels are decimal so they sort as text
        return fmt.Sprintf("%d", double(x))
    })
    return strings.Join(names, ",")
}

func double(x int) int { return x * 2 }

func NewStack() *Stack {
    s := &Stack{}
    s.Push(1)
    defer fmt.Println("done")
    go double(2)
    return s
}

func Größe() int { return double(1) }
"##;

#[test]
fn go_receivers_generics_interfaces_and_function_literals() {
    let src = GO_STORE;
    let p = parse("store/stack.go", src);
    assert_eq!(p.file.as_ref().unwrap().lang, "go");
    has(&p, src, "store.stack.Stack", C, "type Stack struct", "}");
    has(&p, src, "store.stack.Stack.Push", M, "func (s *Stack) Push", "}");
    has(&p, src, "store.stack.Stack.grow", M, "func (s *Stack) grow", "");
    has(&p, src, "store.stack.Map", F, "func Map[", "}");
    has(&p, src, "store.stack.Pricer", I, "type Pricer interface", "}");
    has(&p, src, "store.stack.Cents", T, "type Cents int", "");
    has(&p, src, "store.stack.Labels", F, "func Labels", "}");
    has(&p, src, "store.stack.double", F, "func double", "");
    has(&p, src, "store.stack.NewStack", F, "func NewStack", "}");
    has(&p, src, "store.stack.Größe", F, "func Größe", "");
    // An interface's method list declares, it defines nothing.
    assert!(!p.symbols.iter().any(|s| s.name == "Price"), "{}", listing(&p.symbols));

    let doc = |qn: &str| sym(&p.symbols, qn).doc.clone();
    assert_eq!(doc("store.stack.Stack"), "Stack is a LIFO.");
    assert_eq!(doc("store.stack.Stack.Push"), "Push adds v on top.");
    assert_eq!(doc("store.stack.Map"), "Map applies f to every element.");
    assert_eq!(
        sym(&p.symbols, "store.stack.Stack.Push").signature,
        "func (s *Stack) Push(v int)"
    );
    assert_eq!(
        sym(&p.symbols, "store.stack.Map").signature,
        "func Map[T, U any](xs []T, f func(T) U) []U"
    );

    // The receiver variable is self; package-qualified calls keep the package.
    has_call(&p, "store.stack.Stack.Push", "grow", "self");
    has_call(&p, "store.stack.Stack.Push", "append", "");
    has_call(&p, "store.stack.Map", "f", "");
    has_call(&p, "store.stack.Labels", "Map", "");
    has_call(&p, "store.stack.Labels", "Sprintf", "fmt");
    has_call(&p, "store.stack.Labels", "double", "");
    has_call(&p, "store.stack.Labels", "Join", "strings");
    has_call(&p, "store.stack.NewStack", "Push", "s");
    has_call(&p, "store.stack.NewStack", "Println", "fmt");
    has_call(&p, "store.stack.NewStack", "double", "");
    has_call(&p, "store.stack.Größe", "double", "");
    assert_eq!(
        rationale(&p),
        [(
            "store.stack.Labels".to_string(),
            "WHY: labels are decimal so they sort as text".to_string()
        )]
    );
}

#[test]
fn go_resolution_keeps_provenance() {
    let stack = parse("store/stack.go", GO_STORE);
    let money = parse(
        "store/money.go",
        "package store\n\nimport \"fmt\"\n\n// FormatMoney renders cents.\nfunc FormatMoney(c int) string { return fmt.Sprint(c) }\n",
    );
    let report = parse(
        "store/report.go",
        "package store\n\nfunc Report() string {\n    return FormatMoney(Größe()) + Summarize()\n}\n",
    );
    let a = parse(
        "store/a.go",
        "package store\n\nfunc Summarize() string { return \"a\" }\n",
    );
    let b = parse(
        "store/b.go",
        "package store\n\nfunc Summarize() string { return \"b\" }\n",
    );
    let (input, res) = resolve(&[&stack, &money, &report, &a, &b]);
    static_call(&input, &res, "store.stack.Stack.Push", "store.stack.Stack.grow", 0.95);
    static_call(&input, &res, "store.stack.Labels", "store.stack.Map", 0.95);
    static_call(&input, &res, "store.stack.Labels", "store.stack.double", 0.95);
    static_call(&input, &res, "store.stack.NewStack", "store.stack.double", 0.95);
    static_call(&input, &res, "store.stack.Größe", "store.stack.double", 0.95);
    // `s.Push(1)` on a local variable: matched by name.
    inferred_call(&input, &res, "store.stack.NewStack", "store.stack.Stack.Push");
    inferred_call(&input, &res, "store.report.Report", "store.money.FormatMoney");
    inferred_call(&input, &res, "store.report.Report", "store.stack.Größe");
    unresolved_call(&input, &res, "store.report.Report", "Summarize");
    static_stays_in_file(&input, &res);
}

const GO_CACHE: &str = r##"package store

// Cache maps keys to values.
type Cache[K comparable, V any] struct {
    data map[K]V
}

// Get returns the value of k.
func (c *Cache[K, V]) Get(k K) (V, bool) {
    v, ok := c.data[k]
    c.touch(k)
    return v, ok
}

func (c *Cache[K, V]) touch(k K) {}
"##;

/// A receiver type with two type parameters: `(c *Cache[K, V])` is a method of
/// `Cache`.
#[test]
fn go_receivers_with_several_type_parameters_name_their_type() {
    let p = parse("store/cache.go", GO_CACHE);
    has(
        &p,
        GO_CACHE,
        "store.cache.Cache.Get",
        M,
        "func (c *Cache[K, V]) Get",
        "}",
    );
    has(
        &p,
        GO_CACHE,
        "store.cache.Cache.touch",
        M,
        "func (c *Cache[K, V]) touch",
        "",
    );
    assert_eq!(
        sym(&p.symbols, "store.cache.Cache.Get").doc,
        "Get returns the value of k."
    );
    has_call(&p, "store.cache.Cache.Get", "touch", "self");
    let (input, res) = resolve(&[&p]);
    static_call(&input, &res, "store.cache.Cache.Get", "store.cache.Cache.touch", 0.95);
}

/// `type Stack[T any] struct` is one class and `type Set[T comparable] interface`
/// one interface, as without type parameters: not also a `type` symbol of the
/// same name.
#[test]
fn go_generic_types_are_one_symbol() {
    let src = "package ds\n\ntype Stack[T any] struct {\n    items []T\n}\n\ntype Set[T comparable] interface {\n    Has(v T) bool\n}\n\ntype Pair[K comparable, V any] struct {\n    k K\n    v V\n}\n";
    let p = parse("ds/generic.go", src);
    has(&p, src, "ds.generic.Stack", C, "type Stack[T any]", "}");
    has(&p, src, "ds.generic.Set", I, "type Set[T comparable]", "}");
    has(&p, src, "ds.generic.Pair", C, "type Pair[", "}");
}

/// `Map[int, string](xs, f)` and `Must[Config](load())` instantiate a generic
/// function explicitly; both are calls of it.
#[test]
fn go_explicit_generic_instantiations_are_calls() {
    let src = "package conv\n\nfunc Map[T, U any](xs []T, f func(T) U) []U { return nil }\n\nfunc Must[T any](v T) T { return v }\n\nfunc Labels(xs []int) []string {\n    _ = Must[int](1)\n    return Map[int, string](xs, nil)\n}\n";
    let p = parse("conv/conv.go", src);
    has_call(&p, "conv.conv.Labels", "Map", "");
    has_call(&p, "conv.conv.Labels", "Must", "");
    let (input, res) = resolve(&[&p]);
    static_call(&input, &res, "conv.conv.Labels", "conv.conv.Map", 0.95);
    static_call(&input, &res, "conv.conv.Labels", "conv.conv.Must", 0.95);
}

/// Go spreads the methods of a type over the files of its package. In
/// `cart_ops.go`, `c` is a `*Cart` and `Cart.Subtotal` is defined in the same
/// file: the source names it.
#[test]
fn go_methods_of_a_type_declared_in_another_file_call_each_other_statically() {
    let cart = parse("store/cart.go", "package store\n\ntype Cart struct{ items []int }\n");
    let ops = parse(
        "store/cart_ops.go",
        "package store\n\nfunc (c *Cart) Total() int { return c.Subtotal() + 1 }\n\nfunc (c *Cart) Subtotal() int { return len(c.items) }\n",
    );
    has_call(&ops, "store.cart_ops.Cart.Total", "Subtotal", "self");
    let (input, res) = resolve(&[&cart, &ops]);
    static_call(
        &input,
        &res,
        "store.cart_ops.Cart.Total",
        "store.cart_ops.Cart.Subtotal",
        0.95,
    );
}

const GO_CART: &str = r##"package store

type Cart struct{ n int }

func (c *Cart) Total() int { return c.Subtotal() }

func (c *Cart) Subtotal() int { return c.n }

func Process(c *Cart) int { return c.Total() }

func (Cart) Name() string { return "cart" }
"##;

const GO_CART_TEST: &str = r##"package store

import "testing"

func TestTotal(t *testing.T) {
    c := &Cart{}
    if c.Total() != 1 {
        t.Fatal("bad total")
    }
}

func BenchmarkTotal(b *testing.B) {
    for i := 0; i < b.N; i++ {
        helperTotal()
    }
}

func helperTotal() int { return 1 }
"##;

#[test]
fn go_receiver_variable_is_self_only_inside_its_method() {
    let p = parse("store/cart.go", GO_CART);
    has_call(&p, "store.cart.Cart.Total", "Subtotal", "self");
    // A parameter called `c` in a plain function is just a variable.
    has_call(&p, "store.cart.Process", "Total", "c");
    has(&p, GO_CART, "store.cart.Cart.Name", M, "func (Cart) Name", "");
    let (input, res) = resolve(&[&p]);
    static_call(&input, &res, "store.cart.Cart.Total", "store.cart.Cart.Subtotal", 0.95);
    inferred_call(&input, &res, "store.cart.Process", "store.cart.Cart.Total");
}

#[test]
fn go_test_files_benchmarks_and_broken_files() {
    let cart = parse("store/cart.go", GO_CART);
    let t = parse("store/cart_test.go", GO_CART_TEST);
    for qn in [
        "store.cart_test",
        "store.cart_test.TestTotal",
        "store.cart_test.BenchmarkTotal",
        "store.cart_test.helperTotal",
    ] {
        assert!(sym(&t.symbols, qn).is_test, "{qn}");
    }
    has_call(&t, "store.cart_test.TestTotal", "Total", "c");
    has_call(&t, "store.cart_test.TestTotal", "Fatal", "t");
    let (input, res) = resolve(&[&cart, &t]);
    static_call(
        &input,
        &res,
        "store.cart_test.BenchmarkTotal",
        "store.cart_test.helperTotal",
        0.95,
    );
    let (total, test) = (
        &sym(&input.symbols, "store.cart.Cart.Total").id,
        &sym(&input.symbols, "store.cart_test.TestTotal").id,
    );
    let tested = res
        .edges
        .iter()
        .find(|e| e.edge_type == EdgeType::TestedBy && e.from == *total && e.to == *test)
        .expect("Cart.Total is tested by TestTotal");
    assert_eq!(tested.method, method::INFERRED);

    let broken = parse(
        "store/broken.go",
        "package store\n\nfunc Broken( {\n    return\n}\n\nfunc ok() int { return helper() }\n",
    );
    assert_eq!(broken.symbols[0].kind, MOD);
    let empty = parse("store/empty.go", "");
    assert_eq!(empty.symbols.len(), 1);
}

// ---------------------------------------------------------------------------
// C
// ---------------------------------------------------------------------------

const C_CART: &str = r##"/* cart.c - prices a cart. */
#include <stdio.h>
#include "cart.h"

typedef struct cart {
    int *items;
    int count;
    int (*price_of)(const struct cart *, int);
} cart_t;

typedef struct {
    int cents;
} money_t;

enum color { RED, GREEN };

union value { int i; float f; };

int cart_total(const cart_t *c);

/**
 * Adds the sales tax to an amount.
 */
static inline int apply_tax(int amount) {
    return amount * 108 / 100;
}

static int **matrix_new(int n) {
    return NULL;
}

char *cart_label(const cart_t *c) {
    static char buf[32];
    snprintf(buf, sizeof buf, "%d", cart_total(c));
    return buf;
}

int cart_total(const cart_t *c) {
    int sum = 0;
    for (int i = 0; i < c->count; i++) {
        sum += c->price_of(c, i);
    }
    /* WHY: tax is applied once, on the sum */
    return apply_tax(sum);
}

#ifdef DEBUG
void cart_dump(const cart_t *c) { printf("%d\n", cart_total(c)); }
#endif
"##;

const C_HEADER: &str = r##"#ifndef CART_H
#define CART_H

typedef struct cart cart_t;

int cart_total(const cart_t *c);

static inline int cart_empty(const cart_t *c) {
    return cart_total(c) == 0;
}

#endif
"##;

#[test]
fn c_functions_structs_typedefs_and_calls() {
    let src = C_CART;
    let p = parse("lib/cart.c", src);
    assert_eq!(p.file.as_ref().unwrap().lang, "c");
    assert_eq!(p.symbols[0].doc, "cart.c - prices a cart.");
    has(&p, src, "lib.cart.cart", C, "typedef struct cart {", "} cart_t;");
    has(&p, src, "lib.cart.cart_t", T, "typedef struct cart {", "} cart_t;");
    has(&p, src, "lib.cart.money_t", T, "typedef struct {", "} money_t;");
    has(&p, src, "lib.cart.color", E, "enum color", "");
    has(&p, src, "lib.cart.value", C, "union value", "");
    has(&p, src, "lib.cart.apply_tax", F, "static inline int apply_tax", "}");
    has(&p, src, "lib.cart.matrix_new", F, "static int **matrix_new", "}");
    has(&p, src, "lib.cart.cart_label", F, "char *cart_label", "}");
    // The prototype is no definition: one symbol, the definition.
    has(
        &p,
        src,
        "lib.cart.cart_total",
        F,
        "int cart_total(const cart_t *c) {",
        "}",
    );
    has(&p, src, "lib.cart.cart_dump", F, "void cart_dump", "");
    assert_eq!(
        sym(&p.symbols, "lib.cart.apply_tax").doc,
        "Adds the sales tax to an amount."
    );
    assert_eq!(
        sym(&p.symbols, "lib.cart.apply_tax").signature,
        "static inline int apply_tax(int amount)"
    );
    assert_eq!(
        sym(&p.symbols, "lib.cart.cart_label").signature,
        "char *cart_label(const cart_t *c)"
    );

    has_call(&p, "lib.cart.cart_label", "snprintf", "");
    has_call(&p, "lib.cart.cart_label", "cart_total", "");
    has_call(&p, "lib.cart.cart_total", "price_of", "c");
    has_call(&p, "lib.cart.cart_total", "apply_tax", "");
    has_call(&p, "lib.cart.cart_dump", "cart_total", "");
    assert_eq!(
        rationale(&p),
        [(
            "lib.cart.cart_total".to_string(),
            "WHY: tax is applied once, on the sum".to_string()
        )]
    );
}

#[test]
fn c_resolution_keeps_provenance() {
    let cart = parse("lib/cart.c", C_CART);
    let header = parse("lib/cart.h", C_HEADER);
    let clamp =
        "static int clamp(int x) {\n    return x < 0 ? 0 : x;\n}\n\nint use_clamp(int x) {\n    return clamp(x);\n}\n";
    let a = parse("lib/util_a.c", clamp);
    let b = parse("lib/util_b.c", clamp);
    let other = parse("lib/other.c", "int other(void) {\n    return clamp(3);\n}\n");
    assert!(
        header.symbols.iter().all(|s| s.name != "cart_total"),
        "a prototype is no symbol"
    );
    let (input, res) = resolve(&[&cart, &header, &a, &b, &other]);
    static_call(&input, &res, "lib.cart.cart_label", "lib.cart.cart_total", 0.95);
    static_call(&input, &res, "lib.cart.cart_total", "lib.cart.apply_tax", 0.95);
    static_call(&input, &res, "lib.cart.cart_dump", "lib.cart.cart_total", 0.95);
    // Two `static` functions with one name: each file calls its own.
    static_call(&input, &res, "lib.util_a.use_clamp", "lib.util_a.clamp", 0.95);
    static_call(&input, &res, "lib.util_b.use_clamp", "lib.util_b.clamp", 0.95);
    inferred_call(&input, &res, "lib.cart.cart_empty", "lib.cart.cart_total");
    unresolved_call(&input, &res, "lib.other.other", "clamp");
    static_stays_in_file(&input, &res);
}

/// `#define SQUARE(x)` is called like a function and is what `SQUARE(side)` runs.
#[test]
fn c_function_like_macros_are_symbols() {
    let src = "#define SQUARE(x) ((x) * (x))\n\nint area(int side) {\n    return SQUARE(side);\n}\n";
    let p = parse("lib/geom.c", src);
    has_call(&p, "lib.geom.area", "SQUARE", "");
    let m = sym(&p.symbols, "lib.geom.SQUARE");
    assert_eq!(m.start_line, 1);
    let (input, res) = resolve(&[&p]);
    static_call(&input, &res, "lib.geom.area", "lib.geom.SQUARE", 0.95);
}

const CPP_IN_DOT_H: &str = r##"#pragma once
#include <vector>

namespace shop {

class Cart {
 public:
  int total() const { return subtotal() * 2; }
  int subtotal() const;

 private:
  std::vector<int> items_;
};

}  // namespace shop
"##;

/// Most C++ projects name their headers `.h`: a class with inline methods in
/// one must still be read as a class with methods.
#[test]
fn cpp_class_in_a_dot_h_header_is_read() {
    let p = parse("include/cart.h", CPP_IN_DOT_H);
    has(
        &p,
        CPP_IN_DOT_H,
        "include.cart.shop",
        MOD,
        "namespace shop {",
        "}  // namespace shop",
    );
    has(&p, CPP_IN_DOT_H, "include.cart.shop.Cart", C, "class Cart {", "};");
    has(&p, CPP_IN_DOT_H, "include.cart.shop.Cart.total", M, "int total()", "");
    has_call(&p, "include.cart.shop.Cart.total", "subtotal", "");
}

// ---------------------------------------------------------------------------
// C++
// ---------------------------------------------------------------------------

const CPP_CART: &str = r##"#include "cart.hpp"
#include <memory>

namespace shop {

/// Adds the sales tax to an amount.
int apply_tax(int amount) { return amount * 108 / 100; }

class Cart {
 public:
  Cart() : items_() {}
  explicit Cart(int n) { reserve(n); }

  int subtotal() const;
  int total() const { return apply_tax(subtotal()); }
  void reserve(int n) { this->items_.reserve(n); }
  void clear() { items_.clear(); }
  static Cart make() { return Cart(); }
  int area(int r) const { return r * r; }
  int area(int w, int h) const { return w * h; }

  class Line {
   public:
    int cost() const { return helper(); }
    int helper() const { return 1; }
  };

 private:
  std::vector<int> items_;
};

int Cart::subtotal() const {
  int sum = 0;
  for (int p : items_) sum += p;
  return sum + area(1);
}

template <typename T>
T max_of(T a, T b) { return a > b ? a : b; }

enum class Color { Red, Green };
using Price = int;

}  // namespace shop

int checkout() {
  auto cart = std::make_unique<shop::Cart>();
  auto twice = [&](int x) { return shop::apply_tax(x) * 2; };
  int t = cart->total();
  t += shop::Cart::make().total();
  t += shop::max_of<int>(t, 1);
  // NOTE: the lambda runs once per checkout
  return twice(t);
}
"##;

#[test]
fn cpp_namespaces_classes_constructors_overloads_and_lambdas() {
    let src = CPP_CART;
    let p = parse("shop/cart.cpp", src);
    assert_eq!(p.file.as_ref().unwrap().lang, "cpp");
    has(
        &p,
        src,
        "shop.cart.shop",
        MOD,
        "namespace shop {",
        "}  // namespace shop",
    );
    has(&p, src, "shop.cart.shop.apply_tax", F, "int apply_tax", "");
    has(&p, src, "shop.cart.shop.Cart", C, "class Cart {", "};");
    // Two constructors and two `area` overloads: one symbol each, at the first.
    has(&p, src, "shop.cart.shop.Cart.Cart", M, "Cart() : items_() {}", "");
    has(&p, src, "shop.cart.shop.Cart.total", M, "int total() const", "");
    has(&p, src, "shop.cart.shop.Cart.reserve", M, "void reserve", "");
    has(&p, src, "shop.cart.shop.Cart.clear", M, "void clear", "");
    has(&p, src, "shop.cart.shop.Cart.make", M, "static Cart make", "");
    has(&p, src, "shop.cart.shop.Cart.area", M, "int area(int r)", "");
    has(&p, src, "shop.cart.shop.Cart.Line", C, "class Line {", "  };");
    has(&p, src, "shop.cart.shop.Cart.Line.cost", M, "int cost()", "");
    has(&p, src, "shop.cart.shop.Cart.Line.helper", M, "int helper()", "");
    has(&p, src, "shop.cart.shop.Cart.subtotal", M, "int Cart::subtotal", "}");
    has(&p, src, "shop.cart.shop.max_of", F, "T max_of", "");
    has(&p, src, "shop.cart.shop.Color", E, "enum class Color", "");
    has(&p, src, "shop.cart.shop.Price", T, "using Price", "");
    has(&p, src, "shop.cart.checkout", F, "int checkout()", "}");
    assert!(!p.symbols.iter().any(|s| s.name == "twice"), "a lambda is no symbol");
    assert_eq!(
        sym(&p.symbols, "shop.cart.shop.apply_tax").doc,
        "Adds the sales tax to an amount."
    );
    assert_eq!(
        sym(&p.symbols, "shop.cart.shop.Cart.subtotal").signature,
        "int Cart::subtotal() const"
    );

    has_call(&p, "shop.cart.shop.Cart.Cart", "reserve", "");
    has_call(&p, "shop.cart.shop.Cart.total", "apply_tax", "");
    has_call(&p, "shop.cart.shop.Cart.total", "subtotal", "");
    has_call(&p, "shop.cart.shop.Cart.reserve", "reserve", "this->items_");
    has_call(&p, "shop.cart.shop.Cart.clear", "clear", "items_");
    has_call(&p, "shop.cart.shop.Cart.make", "Cart", "");
    has_call(&p, "shop.cart.shop.Cart.Line.cost", "helper", "");
    has_call(&p, "shop.cart.shop.Cart.subtotal", "area", "");
    has_call(&p, "shop.cart.checkout", "make_unique", "std");
    has_call(&p, "shop.cart.checkout", "apply_tax", "shop");
    has_call(&p, "shop.cart.checkout", "total", "cart");
    has_call(&p, "shop.cart.checkout", "total", "shop::Cart::make()");
    has_call(&p, "shop.cart.checkout", "max_of", "shop");
    has_call(&p, "shop.cart.checkout", "twice", "");
    assert_eq!(
        rationale(&p),
        [(
            "shop.cart.checkout".to_string(),
            "NOTE: the lambda runs once per checkout".to_string()
        )]
    );
}

#[test]
fn cpp_resolution_keeps_provenance() {
    let cart = parse("shop/cart.cpp", CPP_CART);
    let report = parse(
        "shop/report.cpp",
        "int report() {\n  return checkout() + shop::apply_tax(1);\n}\n",
    );
    let (input, res) = resolve(&[&cart, &report]);
    static_call(
        &input,
        &res,
        "shop.cart.shop.Cart.total",
        "shop.cart.shop.apply_tax",
        0.95,
    );
    static_call(
        &input,
        &res,
        "shop.cart.shop.Cart.total",
        "shop.cart.shop.Cart.subtotal",
        0.95,
    );
    static_call(
        &input,
        &res,
        "shop.cart.shop.Cart.Cart",
        "shop.cart.shop.Cart.reserve",
        0.95,
    );
    static_call(
        &input,
        &res,
        "shop.cart.shop.Cart.Line.cost",
        "shop.cart.shop.Cart.Line.helper",
        0.95,
    );
    static_call(
        &input,
        &res,
        "shop.cart.shop.Cart.subtotal",
        "shop.cart.shop.Cart.area",
        0.95,
    );
    // `cart->total()` on a local variable: by name only.
    inferred_call(&input, &res, "shop.cart.checkout", "shop.cart.shop.Cart.total");
    inferred_call(&input, &res, "shop.report.report", "shop.cart.checkout");
    inferred_call(&input, &res, "shop.report.report", "shop.cart.shop.apply_tax");
    static_stays_in_file(&input, &res);
}

const CPP_QUALIFIED: &str = r##"#include <chrono>
#include "cart.hpp"

namespace shop {
class Cart {
 public:
  explicit Cart(int n) {}
  static Cart make() { return Cart(0); }
};
}  // namespace shop

long checkout() {
  auto c = shop::Cart::make();
  auto now = std::chrono::steady_clock::now();
  shop::Cart* raw = new shop::Cart(3);
  return now.time_since_epoch().count();
}
"##;

/// `ns::Class::method()` and `std::chrono::steady_clock::now()` are calls of
/// `make` and `now`; `new shop::Cart(3)` constructs a `Cart` like `new Cart(3)`.
#[test]
fn cpp_calls_through_nested_qualifiers_are_calls() {
    let p = parse("shop/checkout.cpp", CPP_QUALIFIED);
    let calls = calls_of(&p, "shop.checkout.checkout");
    let mut missing = Vec::new();
    for (name, receiver) in [
        ("make", "shop::Cart"),
        ("now", "std::chrono::steady_clock"),
        ("Cart", ""),
    ] {
        if !calls.iter().any(|(n, r)| n == name && r == receiver) {
            missing.push(format!("{receiver}::{name}"));
        }
    }
    assert!(missing.is_empty(), "missing calls {missing:?}; calls: {calls:?}");
}

const CPP_SWAP: &str = r##"class Buffer {
 public:
  void swap(Buffer& other) noexcept;
  void reset() {
    Buffer empty;
    swap(empty);
  }
};

inline void swap(Buffer& a, Buffer& b) noexcept { a.swap(b); }
"##;

/// Copy-and-swap: `swap(empty)` inside `Buffer::reset` finds the member
/// `Buffer::swap` declared in the class (it is defined in the `.cpp`), not the
/// free `swap` below. C++ looks in the class first, so a static edge to the free
/// function is wrong.
#[test]
fn cpp_a_bare_call_to_a_declared_member_is_not_the_free_function() {
    let p = parse("buf/buffer.hpp", CPP_SWAP);
    has_call(&p, "buf.buffer.Buffer.reset", "swap", "");
    let (input, res) = resolve(&[&p]);
    let wrong: Vec<(String, String, f64)> = edges_from(&input, &res, "buf.buffer.Buffer.reset")
        .into_iter()
        .filter(|(to, m, _)| to == "buf.buffer.swap" && m == method::STATIC)
        .collect();
    assert!(wrong.is_empty(), "{wrong:?}");
}

const CPP_BASE: &str = r##"#include "base.hpp"

int log(int x) { return x; }

class Child : public Base {
 public:
  int run() { return log(1); }
};
"##;

/// `log(1)` inside a method of `Child : public Base` is `Base::log` when the
/// base declares it: C++ searches the class and its bases before the
/// namespace. The base is in another file, so which one is called is not
/// written here; a static edge to the free `log` is a guess.
#[test]
fn cpp_a_bare_call_in_a_class_with_unknown_bases_is_not_static() {
    let p = parse("app/child.cpp", CPP_BASE);
    has_call(&p, "app.child.Child.run", "log", "");
    let (input, res) = resolve(&[&p]);
    let wrong: Vec<(String, String, f64)> = edges_from(&input, &res, "app.child.Child.run")
        .into_iter()
        .filter(|(_, m, _)| m == method::STATIC)
        .collect();
    assert!(wrong.is_empty(), "{wrong:?}");
}

const CPP_ORDER: &str = r##"#include "order.hpp"

namespace shop {

int Order::total() const {
  return subtotal() + this->shipping();
}

int Order::subtotal() const { return 1; }

int Order::shipping() const { return 2; }

}  // namespace shop
"##;

/// The usual C++ layout: the class in a header, its methods defined out of line
/// in the `.cpp`. In `Order::total`, `subtotal()` and `this->shipping()` name the
/// `Order::` methods defined right below.
#[test]
fn cpp_out_of_line_methods_without_their_class_call_each_other_statically() {
    let p = parse("shop/order.cpp", CPP_ORDER);
    has(&p, CPP_ORDER, "shop.order.shop.Order.total", M, "int Order::total", "}");
    has_call(&p, "shop.order.shop.Order.total", "subtotal", "");
    has_call(&p, "shop.order.shop.Order.total", "shipping", "self");
    let (input, res) = resolve(&[&p]);
    static_call(
        &input,
        &res,
        "shop.order.shop.Order.total",
        "shop.order.shop.Order.shipping",
        0.95,
    );
    static_call(
        &input,
        &res,
        "shop.order.shop.Order.total",
        "shop.order.shop.Order.subtotal",
        0.95,
    );
}

const CPP_STACK: &str = r##"template <typename T>
class Stack {
 public:
  void push(T v);
  void grow();
};

template <typename T>
void Stack<T>::push(T v) {
  grow();
}

template <typename T>
void Stack<T>::grow() {}
"##;

/// `void Stack<T>::push(T v)` is a method of `Stack`, like `void Stack::push`.
#[test]
fn cpp_template_out_of_line_methods_belong_to_their_class() {
    let p = parse("tpl/stack.hpp", CPP_STACK);
    has(&p, CPP_STACK, "tpl.stack.Stack", C, "class Stack {", "};");
    has(&p, CPP_STACK, "tpl.stack.Stack.push", M, "void Stack<T>::push", "}");
    has(&p, CPP_STACK, "tpl.stack.Stack.grow", M, "void Stack<T>::grow", "");
    let (input, res) = resolve(&[&p]);
    static_call(&input, &res, "tpl.stack.Stack.push", "tpl.stack.Stack.grow", 0.95);
}

const CPP_NESTED_NS: &str = r##"namespace shop::pricing {
int apply_tax(int a) { return a * 2; }
}

namespace shop::legacy {
int apply_tax(int a) { return a + 1; }
}

int run() {
  using namespace shop::legacy;
  return apply_tax(2);
}
"##;

/// C++17 `namespace a::b { }`: two `apply_tax` in two namespaces are two
/// symbols, and a bare call with two candidates in the file is not static.
#[test]
fn cpp_nested_namespace_definitions_qualify_names() {
    let p = parse("ns/prices.cpp", CPP_NESTED_NS);
    let (input, res) = resolve(&[&p]);
    let statics: Vec<(String, String, f64)> = edges_from(&input, &res, "ns.prices.run")
        .into_iter()
        .filter(|(_, m, _)| m == method::STATIC)
        .collect();
    assert!(
        statics.is_empty(),
        "one of two candidates picked: {statics:?}; symbols:\n{}",
        listing(&p.symbols)
    );
    has(
        &p,
        CPP_NESTED_NS,
        "ns.prices.shop.pricing.apply_tax",
        F,
        "return a * 2",
        "",
    );
    has(
        &p,
        CPP_NESTED_NS,
        "ns.prices.shop.legacy.apply_tax",
        F,
        "return a + 1",
        "",
    );
}

const CPP_DTOR: &str = r##"class File {
 public:
  ~File() { close(); }
  bool operator==(const File& o) const { return same(o); }
  void close() {}
  bool same(const File& o) const { return true; }
};
"##;

/// A destructor and an operator are methods: what they call is called by them,
/// not by the class.
#[test]
fn cpp_destructors_and_operators_are_methods() {
    let p = parse("io/file.cpp", CPP_DTOR);
    for name in ["close", "same"] {
        let call = p
            .calls
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("no call {name}"));
        let caller = p.symbols.iter().find(|s| s.id == call.caller_id).unwrap();
        assert_eq!(
            caller.kind,
            M,
            "{name} is called by {} ({})\n{}",
            caller.qualified_name,
            caller.kind,
            listing(&p.symbols)
        );
    }
}

const CPP_GTEST: &str = r##"#include <gtest/gtest.h>
#include "cart.hpp"

TEST(CartTest, TotalIsZero) {
  EXPECT_EQ(shop::Cart().total(), 0);
}

TEST(CartTest, AddsTax) {
  EXPECT_EQ(shop::apply_tax(100), 108);
}
"##;

#[test]
fn cpp_googletest_calls_belong_to_tests() {
    let p = parse("tests/cart_test.cc", CPP_GTEST);
    for name in ["total", "apply_tax"] {
        let call = p
            .calls
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("no call {name}: {:?}", p.calls));
        let caller = p.symbols.iter().find(|s| s.id == call.caller_id).unwrap();
        assert!(caller.is_test, "{name}: {}", caller.qualified_name);
    }
}

/// Each `TEST(Suite, Name)` is its own test: the two cases are two symbols.
#[test]
fn cpp_googletest_cases_are_separate_symbols() {
    let p = parse("tests/cart_test.cc", CPP_GTEST);
    let caller = |name: &str| p.calls.iter().find(|c| c.name == name).map(|c| c.caller_id.clone());
    let (a, b) = (caller("total"), caller("apply_tax"));
    assert!(a.is_some() && b.is_some());
    assert_ne!(a, b, "both TEST cases are one symbol:\n{}", listing(&p.symbols));
}

// ---------------------------------------------------------------------------
// Zig
// ---------------------------------------------------------------------------

const ZIG_CART: &str = r##"//! Prices a shopping cart.
const std = @import("std");
const money = @import("money.zig");

/// The prices of the items a customer picked.
pub const Cart = struct {
    const Self = @This();
    items: std.ArrayList(u64),

    pub fn init(allocator: std.mem.Allocator) Self {
        return .{ .items = std.ArrayList(u64).init(allocator) };
    }

    pub fn deinit(self: *Self) void {
        self.items.deinit();
    }

    /// Adds up the item prices.
    pub fn subtotal(self: Self) u64 {
        var sum: u64 = 0;
        for (self.items.items) |p| sum += p;
        return sum;
    }

    pub fn total(self: Self) !u64 {
        // NOTE: tax applies to the subtotal only
        return try applyTax(self.subtotal());
    }

    pub fn empty() Self {
        return Self.init(std.heap.page_allocator);
    }
};

pub const Kind = enum {
    food,
    tool,

    pub fn label(self: Kind) []const u8 {
        return self.name();
    }

    fn name(self: Kind) []const u8 {
        return @tagName(self);
    }
};

pub const Value = union(enum) {
    int: i64,

    pub fn isInt(self: Value) bool {
        return self == .int;
    }
};

/// Adds the sales tax to an amount.
pub fn applyTax(amount: u64) !u64 {
    return amount * 108 / 100;
}

pub fn checkout(cart: Cart, buf: []u8) ![]u8 {
    const t = try cart.total();
    defer std.debug.print("done\n", .{});
    const c = Cart.empty();
    _ = c.subtotal();
    return money.formatMoney(try applyTax(t), buf);
}
"##;

const ZIG_MONEY: &str = r##"const std = @import("std");

/// Renders cents as dollars.
pub fn formatMoney(cents: u64, buf: []u8) ![]u8 {
    return std.fmt.bufPrint(buf, "{d}", .{cents});
}
"##;

#[test]
fn zig_structs_enums_unions_and_methods() {
    let src = ZIG_CART;
    let p = parse("shop/cart.zig", src);
    assert_eq!(p.file.as_ref().unwrap().lang, "zig");
    assert_eq!(p.symbols[0].doc, "Prices a shopping cart.");
    has(&p, src, "shop.cart.Cart", C, "pub const Cart = struct", "};");
    has(&p, src, "shop.cart.Cart.init", M, "pub fn init", "    }");
    has(&p, src, "shop.cart.Cart.deinit", M, "pub fn deinit", "    }");
    has(&p, src, "shop.cart.Cart.subtotal", M, "pub fn subtotal", "    }");
    has(&p, src, "shop.cart.Cart.total", M, "pub fn total", "    }");
    has(&p, src, "shop.cart.Cart.empty", M, "pub fn empty", "    }");
    has(&p, src, "shop.cart.Kind", E, "pub const Kind = enum", "};");
    has(&p, src, "shop.cart.Kind.label", M, "pub fn label", "    }");
    has(&p, src, "shop.cart.Kind.name", M, "fn name(self: Kind)", "    }");
    has(&p, src, "shop.cart.Value", C, "pub const Value = union", "};");
    has(&p, src, "shop.cart.Value.isInt", M, "pub fn isInt", "    }");
    has(&p, src, "shop.cart.applyTax", F, "pub fn applyTax", "}");
    has(&p, src, "shop.cart.checkout", F, "pub fn checkout", "}");
    // `const Self = @This()` and `const std = @import(..)` are no symbols.
    assert!(!p.symbols.iter().any(|s| s.name == "Self" || s.name == "std"));
    let doc = |qn: &str| sym(&p.symbols, qn).doc.clone();
    assert_eq!(doc("shop.cart.Cart"), "The prices of the items a customer picked.");
    assert_eq!(doc("shop.cart.Cart.subtotal"), "Adds up the item prices.");
    assert_eq!(doc("shop.cart.applyTax"), "Adds the sales tax to an amount.");
    assert_eq!(
        sym(&p.symbols, "shop.cart.applyTax").signature,
        "pub fn applyTax(amount: u64) !u64"
    );

    has_call(&p, "shop.cart.Cart.init", "init", "std.ArrayList(u64)");
    has_call(&p, "shop.cart.Cart.deinit", "deinit", "self.items");
    has_call(&p, "shop.cart.Cart.total", "applyTax", "");
    has_call(&p, "shop.cart.Cart.total", "subtotal", "self");
    // `Self` inside the struct is the struct.
    has_call(&p, "shop.cart.Cart.empty", "init", "self");
    has_call(&p, "shop.cart.Kind.label", "name", "self");
    has_call(&p, "shop.cart.checkout", "total", "cart");
    has_call(&p, "shop.cart.checkout", "print", "std.debug");
    has_call(&p, "shop.cart.checkout", "empty", "Cart");
    has_call(&p, "shop.cart.checkout", "subtotal", "c");
    has_call(&p, "shop.cart.checkout", "formatMoney", "money");
    has_call(&p, "shop.cart.checkout", "applyTax", "");
    assert_eq!(
        rationale(&p),
        [(
            "shop.cart.Cart.total".to_string(),
            "NOTE: tax applies to the subtotal only".to_string()
        )]
    );
}

#[test]
fn zig_resolution_keeps_provenance() {
    let cart = parse("shop/cart.zig", ZIG_CART);
    let money = parse("shop/money.zig", ZIG_MONEY);
    let (input, res) = resolve(&[&cart, &money]);
    static_call(&input, &res, "shop.cart.Cart.total", "shop.cart.Cart.subtotal", 0.95);
    static_call(&input, &res, "shop.cart.Cart.total", "shop.cart.applyTax", 0.95);
    static_call(&input, &res, "shop.cart.Cart.empty", "shop.cart.Cart.init", 0.95);
    static_call(&input, &res, "shop.cart.checkout", "shop.cart.applyTax", 0.95);
    let e = call_edge(&input, &res, "shop.cart.checkout", "shop.cart.Cart.empty");
    assert_eq!(e.method, method::STATIC, "Cart.empty(): a type of the same file");
    inferred_call(&input, &res, "shop.cart.checkout", "shop.money.formatMoney");
    inferred_call(&input, &res, "shop.cart.checkout", "shop.cart.Cart.subtotal");
    inferred_call(&input, &res, "shop.cart.checkout", "shop.cart.Cart.total");
    static_stays_in_file(&input, &res);
}

/// `self.name()` in a method of `enum Kind` names `Kind.name`.
#[test]
fn zig_self_calls_in_enum_methods_are_static() {
    let p = parse("shop/cart.zig", ZIG_CART);
    let (input, res) = resolve(&[&p]);
    static_call(&input, &res, "shop.cart.Kind.label", "shop.cart.Kind.name", 0.95);
}

const ZIG_TEST: &str = r##"const std = @import("std");

pub fn total(xs: []const u64) u64 {
    var sum: u64 = 0;
    for (xs) |x| sum += x;
    return sum;
}

test "total adds every item" {
    const t = total(&[_]u64{ 1, 2 });
    try std.testing.expectEqual(@as(u64, 3), t);
}
"##;

/// Zig tests are `test "name" { ... }` blocks next to the code: a test block is
/// a test symbol and owns its calls, so `total` is tested by it.
#[test]
fn zig_test_declarations_are_test_symbols() {
    let p = parse("shop/sum.zig", ZIG_TEST);
    assert!(!sym(&p.symbols, "shop.sum.total").is_test);
    let at = line(ZIG_TEST, "const t = total(");
    let call = p
        .calls
        .iter()
        .find(|c| c.name == "total" && c.line == at)
        .expect("the call in the test block");
    let caller = p.symbols.iter().find(|s| s.id == call.caller_id).unwrap();
    assert!(
        caller.is_test,
        "called by {} ({}), not a test:\n{}",
        caller.qualified_name,
        caller.kind,
        listing(&p.symbols)
    );
    let (input, res) = resolve(&[&p]);
    let total = &sym(&input.symbols, "shop.sum.total").id;
    assert!(
        res.edges
            .iter()
            .any(|e| e.edge_type == EdgeType::TestedBy && e.from == *total),
        "total is tested"
    );
}

const ZIG_GENERIC: &str = r##"pub fn Stack(comptime T: type) type {
    return struct {
        items: []T,
        const Self = @This();

        pub fn push(self: *Self, x: T) void {
            self.grow();
            _ = x;
        }

        fn grow(self: *Self) void {
            _ = self;
        }
    };
}
"##;

/// A Zig generic type is a function returning `struct { ... }`: the functions in
/// that struct are methods, and `self.grow()` names the `grow` beside `push`.
#[test]
fn zig_generic_type_functions_hold_methods() {
    let p = parse("ds/stack.zig", ZIG_GENERIC);
    let push = p.symbols.iter().find(|s| s.name == "push").expect("push");
    assert_eq!(push.kind, M, "{}", listing(&p.symbols));
    has_call(&p, &push.qualified_name.clone(), "grow", "self");
    let grow = p
        .symbols
        .iter()
        .find(|s| s.name == "grow")
        .expect("grow")
        .qualified_name
        .clone();
    let (input, res) = resolve(&[&p]);
    static_call(&input, &res, &push.qualified_name, &grow, 0.95);
}

#[test]
fn zig_broken_and_empty_files_do_not_panic() {
    let src = "pub fn ok() u32 {\n    return helper();\n}\n\nfn helper() u32 {\n    return 1;\n}\n\npub fn broken( {\n    const x = ;\n";
    let p = parse("x/broken.zig", src);
    sym(&p.symbols, "x.broken.ok");
    sym(&p.symbols, "x.broken.helper");
    let (input, res) = resolve(&[&p]);
    static_call(&input, &res, "x.broken.ok", "x.broken.helper", 0.95);
    let empty = parse("x/empty.zig", "");
    assert_eq!(empty.symbols.len(), 1);
    assert!(empty.calls.is_empty());
}

// ---------------------------------------------------------------------------
// Across languages
// ---------------------------------------------------------------------------

/// A name is matched only among languages that can call each other by name: a
/// Rust call never lands on a C or Go function, a C call can land on C++.
#[test]
fn name_matches_stay_in_their_language_family() {
    let rs = parse(
        "rs/src/run.rs",
        "pub fn run() -> u32 {\n    c_only_helper() + go_helper()\n}\n",
    );
    let c = parse("lib/b.c", "int c_only_helper(void) {\n    return 1;\n}\n");
    let go = parse("gosvc/x.go", "package x\n\nfunc go_helper() int { return 1 }\n");
    let cpp = parse("lib/c.cpp", "int cpp_helper() {\n  return 1;\n}\n");
    let user = parse("lib/d.c", "int use_cpp(void) {\n    return cpp_helper();\n}\n");
    let zig = parse("zg/z.zig", "pub fn z() u32 {\n    return c_only_helper();\n}\n");
    let (input, res) = resolve(&[&rs, &c, &go, &cpp, &user, &zig]);
    assert!(edges_from(&input, &res, "rs.src.run.run").is_empty());
    assert!(edges_from(&input, &res, "zg.z.z").is_empty());
    inferred_call(&input, &res, "lib.d.use_cpp", "lib.c.cpp_helper");
}
