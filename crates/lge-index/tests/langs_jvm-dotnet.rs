//! Review of the tags-query extraction (and its resolution) for Java, Kotlin,
//! Scala, C#, Groovy, Swift, Objective-C and Dart, on code the way real
//! projects write it: nested types, constructors, overloads, lambdas, generic
//! calls, chained calls, extensions, anonymous classes, tests and broken files.
//!
//! Every test says what correct extraction is. A failing test is a defect.

use lge_core::{fact, kind, method, CallSite, EdgeRec, EdgeType, ParsedFile, ResolveInput, Resolved, SymbolRec};

const C: &str = kind::CLASS;
const F: &str = kind::FUNCTION;
const M: &str = kind::METHOD;
const I: &str = kind::INTERFACE;
const E: &str = kind::ENUM;
const MOD: &str = kind::MODULE;

fn parse(path: &str, src: &str) -> ParsedFile {
    lge_index::parse_file(path, "svc", src.as_bytes(), "h")
        .unwrap_or_else(|e| panic!("{path}: {e:#}"))
        .unwrap_or_else(|| panic!("{path}: not indexed"))
}

/// Some files, parsed and resolved together.
struct Repo {
    input: ResolveInput,
    res: Resolved,
}

impl Repo {
    fn new(files: &[(&str, &str)]) -> Repo {
        let mut input = ResolveInput::default();
        for (path, src) in files {
            let p = parse(path, src);
            input.files.extend(p.file);
            input.symbols.extend(p.symbols);
            input.calls.extend(p.calls);
            input.imports.extend(p.imports);
            input.facts.extend(p.facts);
        }
        let res = lge_resolve::resolve(&input);
        Repo { input, res }
    }

    fn find(&self, qn: &str) -> Vec<&SymbolRec> {
        self.input.symbols.iter().filter(|s| s.qualified_name == qn).collect()
    }

    fn sym(&self, qn: &str) -> &SymbolRec {
        match self.find(qn).as_slice() {
            [one] => one,
            other => panic!("{} symbols named {qn}; have:\n{}", other.len(), self.listing()),
        }
    }

    fn listing(&self) -> String {
        self.input
            .symbols
            .iter()
            .map(|s| format!("  {} {} {}-{}", s.qualified_name, s.kind, s.start_line, s.end_line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn qn(&self, id: &str) -> String {
        self.input
            .symbols
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.qualified_name.clone())
            .unwrap_or_else(|| id.to_string())
    }

    /// (qualified name, kind, first line, last line) of each.
    fn expect_symbols(&self, want: &[(&str, &str, i64, i64)]) {
        for (qn, k, start, end) in want {
            let s = self.sym(qn);
            assert_eq!(
                (s.kind.as_str(), s.start_line, s.end_line),
                (*k, *start, *end),
                "{qn}\n{}",
                self.listing()
            );
            assert!(!s.signature.is_empty() || s.kind == MOD, "{qn}: signature");
            assert!(!s.body.is_empty() || s.kind == MOD, "{qn}: body");
        }
    }

    /// (name, receiver) of each call `caller` makes.
    fn calls_of(&self, caller: &str) -> Vec<(String, String)> {
        let id = &self.sym(caller).id;
        self.input
            .calls
            .iter()
            .filter(|c| c.caller_id == *id)
            .map(|c| (c.name.clone(), c.receiver.clone()))
            .collect()
    }

    fn expect_call(&self, caller: &str, name: &str, receiver: &str) {
        let calls = self.calls_of(caller);
        assert!(
            calls.contains(&(name.to_string(), receiver.to_string())),
            "{caller}: no call {receiver:?}.{name}; calls: {calls:?}"
        );
    }

    /// Calls named `name` on `line`, in any file.
    fn calls_at(&self, name: &str, line: i64) -> Vec<&CallSite> {
        self.input
            .calls
            .iter()
            .filter(|c| c.name == name && c.line == line)
            .collect()
    }

    fn calls_named(&self, name: &str) -> Vec<&CallSite> {
        self.input.calls.iter().filter(|c| c.name == name).collect()
    }

    fn caller_at(&self, name: &str, line: i64) -> &SymbolRec {
        let calls = self.calls_at(name, line);
        let Some(c) = calls.first() else {
            panic!("no call {name} on line {line}; calls: {:?}", self.input.calls)
        };
        self.input
            .symbols
            .iter()
            .find(|s| s.id == c.caller_id)
            .unwrap_or_else(|| panic!("caller of {c:?}"))
    }

    fn call_edges(&self) -> impl Iterator<Item = &EdgeRec> {
        self.res.edges.iter().filter(|e| e.edge_type == EdgeType::Calls)
    }

    fn edge(&self, from: &str, to: &str) -> Option<&EdgeRec> {
        let from = &self.sym(from).id;
        let to = &self.sym(to).id;
        self.call_edges().find(|e| e.from == *from && e.to == *to)
    }

    fn edges_listing(&self) -> String {
        self.call_edges()
            .map(|e| {
                format!(
                    "  {} -> {} {} {}",
                    self.qn(&e.from),
                    self.qn(&e.to),
                    e.method,
                    e.confidence
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn expect_edge(&self, from: &str, to: &str, how: &str, confidence: Option<f64>) {
        let Some(e) = self.edge(from, to) else {
            panic!("no call edge {from} -> {to}; edges:\n{}", self.edges_listing())
        };
        assert_eq!(e.method, how, "{from} -> {to}");
        if let Some(c) = confidence {
            assert_eq!(e.confidence, c, "{from} -> {to}");
        }
    }

    fn expect_no_edge(&self, from: &str, to: &str) {
        if let Some(e) = self.edge(from, to) {
            panic!(
                "unexpected {} edge {from} -> {to}; edges:\n{}",
                e.method,
                self.edges_listing()
            );
        }
    }

    /// Edges from `from` to the unresolved placeholder of `name`.
    fn unresolved(&self, from: &str, name: &str) -> Option<&EdgeRec> {
        let from = &self.sym(from).id;
        let to = format!("unresolved:{name}");
        self.call_edges().find(|e| e.from == *from && e.to == to)
    }

    /// No import sites in these languages: a static edge never leaves its file,
    /// and every edge into another file was matched by name.
    fn expect_static_stays_in_its_file(&self) {
        let file = |id: &str| {
            self.input
                .symbols
                .iter()
                .find(|s| s.id == id)
                .map(|s| s.file_path.clone())
        };
        for e in self.res.edges.iter().filter(|e| e.method == method::STATIC) {
            assert_eq!(
                file(&e.from),
                file(&e.to),
                "static edge across files: {} -> {}",
                self.qn(&e.from),
                self.qn(&e.to)
            );
        }
        for e in self.call_edges() {
            if file(&e.from) != file(&e.to) {
                assert!(e.method == method::INFERRED || e.method == method::UNRESOLVED, "{e:?}");
            }
        }
    }

    fn rationale(&self) -> Vec<(String, String)> {
        self.input
            .facts
            .iter()
            .filter(|f| f.kind == fact::RATIONALE)
            .map(|f| (self.qn(&f.symbol_id), f.key.clone()))
            .collect()
    }

    /// Static edges into `to`, by caller.
    fn static_into(&self, to: &str) -> Vec<String> {
        let Some(to) = self.find(to).first().map(|s| s.id.clone()) else {
            return Vec::new();
        };
        self.call_edges()
            .filter(|e| e.to == to && e.method == method::STATIC)
            .map(|e| self.qn(&e.from))
            .collect()
    }
}

/// One language's check: `Err` says what is wrong.
type Check<'a> = &'a dyn Fn() -> Result<(), String>;

/// Runs `check` on each case and reports every case that fails, so one
/// test covers one defect across the languages it shows in.
fn each_case(cases: &[(&str, Check)]) {
    let failed: Vec<String> = cases
        .iter()
        .filter_map(|(lang, check)| check().err().map(|e| format!("{lang}: {e}")))
        .collect();
    assert!(failed.is_empty(), "\n{}", failed.join("\n"));
}

// ---------------------------------------------------------------------------
// Java
// ---------------------------------------------------------------------------

const JAVA_ORDER: &str = r#"package shop;

import java.util.List;
import java.util.function.Function;

/**
 * An order of a customer.
 */
public class Order {
    private final List<Line> lines;

    /** Builds an empty order. */
    public Order() {
        this(List.of());
    }

    public Order(List<Line> lines) {
        this.lines = lines;
    }

    /** A line of an order. */
    public static class Line {
        int price;

        int amount() {
            return price;
        }
    }

    interface Discount {
        int apply(int amount);
    }

    enum Status {
        OPEN, PAID;

        boolean isFinal() {
            return this == PAID;
        }
    }

    record Summary(int total, int count) {}

    int total() {
        return total(0);
    }

    int total(int discount) {
        int sum = lines.stream().mapToInt(Line::amount).sum();
        return applyDiscount(sum - discount);
    }

    private int applyDiscount(int amount) {
        // NOTE: rounding happens in one place
        Function<Integer, Integer> f = x -> round(x);
        return f.apply(amount);
    }

    static int round(int x) { return Math.round(x); }

    public static <T extends Comparable<T>> T max(List<T> items) {
        return items.stream().max(java.util.Comparator.naturalOrder()).orElseThrow();
    }

    @Override
    public String toString() {
        return Formatter.format(this).trim();
    }

    void pay() {
        new Payment(this).charge();
        Order.round(1);
        this.total();
        ledger.settle();
    }
}
"#;

const JAVA_PAYMENT: &str = r#"package shop;

class Payment {
    Payment(Order o) {}

    void charge() {
        audit();
    }

    void audit() {}

    void settle() {}
}
"#;

const JAVA_REFUND: &str = r#"package shop;

class Refund {
    void settle() {}
}
"#;

fn java_repo() -> Repo {
    Repo::new(&[
        ("shop/Order.java", JAVA_ORDER),
        ("shop/Payment.java", JAVA_PAYMENT),
        ("shop/Refund.java", JAVA_REFUND),
    ])
}

#[test]
fn java_symbols_have_kinds_qualified_names_and_lines() {
    let r = java_repo();
    r.expect_symbols(&[
        ("shop.Order", MOD, 1, 76),
        ("shop.Order.Order", C, 9, 76),
        ("shop.Order.Order.Order", M, 13, 15),
        ("shop.Order.Order.Line", C, 22, 28),
        ("shop.Order.Order.Line.amount", M, 25, 27),
        ("shop.Order.Order.Discount", I, 30, 32),
        ("shop.Order.Order.Discount.apply", M, 31, 31),
        ("shop.Order.Order.Status", E, 34, 40),
        ("shop.Order.Order.Status.isFinal", M, 37, 39),
        ("shop.Order.Order.Summary", C, 42, 42),
        // Overloads share one symbol: the first one.
        ("shop.Order.Order.total", M, 44, 46),
        ("shop.Order.Order.applyDiscount", M, 53, 57),
        ("shop.Order.Order.round", M, 59, 59),
        ("shop.Order.Order.max", M, 61, 63),
        // The annotation is part of the declaration.
        ("shop.Order.Order.toString", M, 65, 68),
        ("shop.Payment.Payment.Payment", M, 4, 4),
    ]);
    assert_eq!(r.sym("shop.Order.Order").doc, "An order of a customer.");
    assert_eq!(r.sym("shop.Order.Order.Order").doc, "Builds an empty order.");
    assert_eq!(r.sym("shop.Order.Order.Line").doc, "A line of an order.");
    assert_eq!(
        r.sym("shop.Order.Order.max").signature,
        "public static <T extends Comparable<T>> T max(List<T> items)"
    );
    assert!(r.input.symbols.iter().all(|s| !s.is_test), "no test here");
    assert_eq!(
        r.rationale(),
        [(
            "shop.Order.Order.applyDiscount".to_string(),
            "NOTE: rounding happens in one place".to_string()
        )]
    );
}

#[test]
fn java_calls_have_their_caller_and_receiver() {
    let r = java_repo();
    r.expect_call("shop.Order.Order.Order", "of", "List");
    r.expect_call("shop.Order.Order.total", "total", "");
    r.expect_call("shop.Order.Order.total", "stream", "lines");
    r.expect_call("shop.Order.Order.total", "mapToInt", "lines.stream()");
    r.expect_call("shop.Order.Order.total", "sum", "lines.stream().mapToInt(Line::amount)");
    r.expect_call("shop.Order.Order.total", "applyDiscount", "");
    // A call in a lambda belongs to the method around it.
    r.expect_call("shop.Order.Order.applyDiscount", "round", "");
    r.expect_call("shop.Order.Order.applyDiscount", "apply", "f");
    r.expect_call("shop.Order.Order.round", "round", "Math");
    r.expect_call("shop.Order.Order.toString", "format", "Formatter");
    r.expect_call("shop.Order.Order.toString", "trim", "Formatter.format(this)");
    r.expect_call("shop.Order.Order.pay", "Payment", "");
    r.expect_call("shop.Order.Order.pay", "charge", "Payment(this)");
    r.expect_call("shop.Order.Order.pay", "round", "Order");
    r.expect_call("shop.Order.Order.pay", "total", "self");
}

#[test]
fn java_resolution_keeps_provenance() {
    let r = java_repo();
    r.expect_edge(
        "shop.Order.Order.total",
        "shop.Order.Order.applyDiscount",
        method::STATIC,
        Some(0.95),
    );
    r.expect_edge(
        "shop.Order.Order.applyDiscount",
        "shop.Order.Order.round",
        method::STATIC,
        Some(0.95),
    );
    r.expect_edge(
        "shop.Order.Order.pay",
        "shop.Order.Order.round",
        method::STATIC,
        Some(0.9),
    );
    r.expect_edge(
        "shop.Order.Order.pay",
        "shop.Order.Order.total",
        method::STATIC,
        Some(0.95),
    );
    r.expect_edge(
        "shop.Payment.Payment.charge",
        "shop.Payment.Payment.audit",
        method::STATIC,
        Some(0.95),
    );
    // Into another file: by name only.
    r.expect_edge("shop.Order.Order.pay", "shop.Payment.Payment", method::INFERRED, None);
    r.expect_edge(
        "shop.Order.Order.pay",
        "shop.Payment.Payment.charge",
        method::INFERRED,
        None,
    );
    let settle = r
        .unresolved("shop.Order.Order.pay", "settle")
        .expect("settle: two candidates");
    assert_eq!((settle.method.as_str(), settle.confidence), (method::UNRESOLVED, 0.0));
    r.expect_static_stays_in_its_file();
}

// ---------------------------------------------------------------------------
// Kotlin
// ---------------------------------------------------------------------------

const KOTLIN_SHOP: &str = r#"package shop

/** Something with a price. */
interface Priced {
    fun total(): Int
}

/**
 * The cart.
 */
data class Cart(val items: List<Int>) : Priced {
    override fun total(): Int = items.sum()

    fun <T> pick(xs: List<T>): T = xs.first()

    companion object {
        fun empty(): Cart = Cart(emptyList())
    }

    class Line(val price: Int) {
        fun amount(): Int = price
    }
}

object Registry {
    fun register(c: Cart) = c.total()
}

/** Extension on Cart. */
fun Cart.discounted(): Int = total() - 1

fun <T> List<T>.second(): T = this[1]

fun checkout(cart: Cart): String {
    // WHY: discounts apply before tax
    val fmt = { x: Int -> "$x" }
    val t = cart.discounted()
    cart?.total()
    Registry.register(cart)
    Cart.empty()
    formatMoney(applyTax(t))
    return fmt(t)
}

fun applyTax(x: Int): Int = x

fun größe(): Int = checkout(Cart(listOf(1))).length
"#;

const KOTLIN_MONEY: &str = "package shop\n\nfun formatMoney(cents: Int): String = \"$cents\"\n";

fn kotlin_repo() -> Repo {
    Repo::new(&[("shop/Shop.kt", KOTLIN_SHOP), ("shop/Money.kt", KOTLIN_MONEY)])
}

#[test]
fn kotlin_symbols_have_kinds_qualified_names_and_lines() {
    let r = kotlin_repo();
    r.expect_symbols(&[
        ("shop.Shop.Priced", I, 4, 6),
        ("shop.Shop.Priced.total", M, 5, 5),
        ("shop.Shop.Cart", C, 11, 23),
        ("shop.Shop.Cart.total", M, 12, 12),
        ("shop.Shop.Cart.pick", M, 14, 14),
        // A companion object's members are the class's.
        ("shop.Shop.Cart.empty", M, 17, 17),
        ("shop.Shop.Cart.Line", C, 20, 22),
        ("shop.Shop.Cart.Line.amount", M, 21, 21),
        ("shop.Shop.Registry", C, 25, 27),
        ("shop.Shop.Registry.register", M, 26, 26),
        // An extension function is a method of the type it extends.
        ("shop.Shop.Cart.discounted", M, 30, 30),
        ("shop.Shop.second", F, 32, 32),
        ("shop.Shop.checkout", F, 34, 43),
        ("shop.Shop.applyTax", F, 45, 45),
        ("shop.Shop.größe", F, 47, 47),
        ("shop.Money.formatMoney", F, 3, 3),
    ]);
    assert_eq!(r.sym("shop.Shop.Priced").doc, "Something with a price.");
    assert_eq!(r.sym("shop.Shop.Cart").doc, "The cart.");
    assert_eq!(r.sym("shop.Shop.Cart.discounted").doc, "Extension on Cart.");
    assert_eq!(r.sym("shop.Shop.Cart.pick").signature, "fun <T> pick(xs: List<T>): T");
    assert!(r.input.symbols.iter().all(|s| !s.is_test));
    assert_eq!(
        r.rationale(),
        [(
            "shop.Shop.checkout".to_string(),
            "WHY: discounts apply before tax".to_string()
        )]
    );
}

#[test]
fn kotlin_calls_resolve_with_provenance() {
    let r = kotlin_repo();
    r.expect_call("shop.Shop.checkout", "discounted", "cart");
    r.expect_call("shop.Shop.checkout", "total", "cart");
    r.expect_call("shop.Shop.checkout", "register", "Registry");
    r.expect_call("shop.Shop.Cart.discounted", "total", "");
    // The extension's receiver is the implicit `this`.
    r.expect_edge(
        "shop.Shop.Cart.discounted",
        "shop.Shop.Cart.total",
        method::STATIC,
        Some(0.95),
    );
    r.expect_edge(
        "shop.Shop.checkout",
        "shop.Shop.Registry.register",
        method::STATIC,
        Some(0.9),
    );
    r.expect_edge("shop.Shop.checkout", "shop.Shop.Cart.empty", method::STATIC, Some(0.9));
    r.expect_edge("shop.Shop.checkout", "shop.Shop.applyTax", method::STATIC, Some(0.95));
    r.expect_edge("shop.Shop.größe", "shop.Shop.checkout", method::STATIC, Some(0.95));
    r.expect_edge("shop.Shop.größe", "shop.Shop.Cart", method::STATIC, Some(0.95));
    // A receiver whose type is unknown is matched by name.
    r.expect_edge(
        "shop.Shop.checkout",
        "shop.Shop.Cart.discounted",
        method::INFERRED,
        None,
    );
    r.expect_edge("shop.Shop.checkout", "shop.Shop.Cart.total", method::INFERRED, None);
    r.expect_edge("shop.Shop.checkout", "shop.Money.formatMoney", method::INFERRED, None);
    // A local lambda is no symbol.
    assert!(r.call_edges().all(|e| r.qn(&e.to) != "fmt"));
    r.expect_static_stays_in_its_file();
}

/// Inside `buildString { }`, `sb.apply { }` and `buildList { }` the lambda's
/// receiver is the innermost implicit `this`: `append`/`add` there are the
/// StringBuilder's and the list's, not the enclosing class's.
#[test]
fn kotlin_a_call_in_a_lambda_with_receiver_is_not_a_static_own_member_call() {
    let src = r#"class Printer {
    fun append(s: String) {}
    fun add(x: Int) {}
    fun render(): String {
        val sb = StringBuilder()
        sb.apply { append("x") }
        return buildList { add(1) }.toString()
    }
}
"#;
    let r = Repo::new(&[("ui/Printer.kt", src)]);
    r.expect_no_edge("ui.Printer.Printer.render", "ui.Printer.Printer.append");
    r.expect_no_edge("ui.Printer.Printer.render", "ui.Printer.Printer.add");
}

// ---------------------------------------------------------------------------
// Scala
// ---------------------------------------------------------------------------

const SCALA_SHOP: &str = r#"package shop

/** Taxes. */
object Pricing {
  /** Adds the sales tax. */
  def applyTax(amount: Int): Int = amount * 108 / 100
  def total(items: List[Int]): Int = applyTax(items.sum)
}

trait Priced {
  def total: Int
}

case class Cart(items: List[Int]) extends Priced {
  def total: Int = items.foldLeft(0)(_ + _)
  def summary(): String = format(total)
  private def format(n: Int): String = n.toString
  def pick[T](xs: List[T]): T = xs.head
  def run(): Unit = {
    // NOTE: map keeps the order
    items.map(i => format(i)).filter(_.nonEmpty)
    Pricing.applyTax(1)
    Cart(Nil).summary()
    formatMoney(1)
  }
}

enum Color {
  case Red, Green
  def rgb: Int = 0
}

def topLevel[T](x: T): T = identity[T](x)
"#;

const SCALA_MONEY: &str = "package shop\n\ndef formatMoney(cents: Int): String = cents.toString\n";

fn scala_repo() -> Repo {
    Repo::new(&[("shop/Shop.scala", SCALA_SHOP), ("shop/Money.scala", SCALA_MONEY)])
}

#[test]
fn scala_symbols_have_kinds_qualified_names_and_lines() {
    let r = scala_repo();
    r.expect_symbols(&[
        ("shop.Shop.Pricing", C, 4, 8),
        ("shop.Shop.Pricing.applyTax", M, 6, 6),
        ("shop.Shop.Pricing.total", M, 7, 7),
        ("shop.Shop.Priced", I, 10, 12),
        ("shop.Shop.Priced.total", M, 11, 11),
        ("shop.Shop.Cart", C, 14, 26),
        ("shop.Shop.Cart.total", M, 15, 15),
        ("shop.Shop.Cart.summary", M, 16, 16),
        ("shop.Shop.Cart.format", M, 17, 17),
        ("shop.Shop.Cart.pick", M, 18, 18),
        ("shop.Shop.Cart.run", M, 19, 25),
        ("shop.Shop.Color", E, 28, 31),
        ("shop.Shop.Color.rgb", M, 30, 30),
        ("shop.Shop.topLevel", F, 33, 33),
        ("shop.Money.formatMoney", F, 3, 3),
    ]);
    assert_eq!(r.sym("shop.Shop.Pricing").doc, "Taxes.");
    assert_eq!(r.sym("shop.Shop.Pricing.applyTax").doc, "Adds the sales tax.");
    assert_eq!(r.sym("shop.Shop.Cart.pick").signature, "def pick[T](xs: List[T]): T");
    assert_eq!(
        r.rationale(),
        [(
            "shop.Shop.Cart.run".to_string(),
            "NOTE: map keeps the order".to_string()
        )]
    );
}

#[test]
fn scala_calls_resolve_with_provenance() {
    let r = scala_repo();
    r.expect_call("shop.Shop.Cart.run", "map", "items");
    r.expect_call("shop.Shop.Cart.run", "format", "");
    r.expect_call("shop.Shop.Cart.run", "filter", "items.map(i => format(i))");
    r.expect_call("shop.Shop.Cart.run", "applyTax", "Pricing");
    r.expect_call("shop.Shop.Cart.run", "summary", "Cart(Nil)");
    r.expect_call("shop.Shop.topLevel", "identity", "");
    r.expect_edge(
        "shop.Shop.Pricing.total",
        "shop.Shop.Pricing.applyTax",
        method::STATIC,
        Some(0.95),
    );
    r.expect_edge(
        "shop.Shop.Cart.summary",
        "shop.Shop.Cart.format",
        method::STATIC,
        Some(0.95),
    );
    r.expect_edge(
        "shop.Shop.Cart.run",
        "shop.Shop.Cart.format",
        method::STATIC,
        Some(0.95),
    );
    r.expect_edge(
        "shop.Shop.Cart.run",
        "shop.Shop.Pricing.applyTax",
        method::STATIC,
        Some(0.9),
    );
    r.expect_edge("shop.Shop.Cart.run", "shop.Shop.Cart", method::STATIC, Some(0.95));
    r.expect_edge("shop.Shop.Cart.run", "shop.Money.formatMoney", method::INFERRED, None);
    r.expect_static_stays_in_its_file();
}

/// `new Cart(Nil)` constructs a Cart exactly as Java's `new Cart()` does, which
/// the Java, C# and Groovy queries tag as a call of the class.
#[test]
fn scala_new_instance_is_a_call_of_the_class() {
    let src = "class Cart(items: List[Int]) {\n  def total: Int = 1\n}\n\nobject Shop {\n  def make(): Cart = new Cart(Nil)\n}\n";
    let r = Repo::new(&[("shop/Make.scala", src)]);
    r.expect_call("shop.Make.Shop.make", "Cart", "");
    r.expect_edge("shop.Make.Shop.make", "shop.Make.Cart", method::STATIC, Some(0.95));
}

// ---------------------------------------------------------------------------
// C#
// ---------------------------------------------------------------------------

const CSHARP_BILLER: &str = r#"using System;
using System.Linq;

namespace Shop.Billing
{
    /// <summary>Bills orders.</summary>
    public class Biller : IBiller
    {
        private readonly ITaxes _taxes;

        public Biller(ITaxes taxes) { _taxes = taxes; }

        public decimal Bill(Order order) => Round(order.Lines.Sum(l => l.Amount()));

        public decimal Bill(Order order, decimal discount)
        {
            return Bill(order) - discount;
        }

        private static decimal Round(decimal x) => Math.Round(x, 2);

        public T Pick<T>(System.Collections.Generic.IEnumerable<T> items) where T : class => items.First();

        decimal Compute() => 1m;

        public async System.Threading.Tasks.Task SaveAsync()
        {
            // NOTE: the repository owns the transaction
            await _repo.SaveAsync(this).ConfigureAwait(false);
            this.Compute();
            Biller.Round(1m);
            Pick<string>(null);
            Formats.FormatMoney(1m);
            new Ledger().Post();
        }

        void Local()
        {
            int Square(int v) => v * v;
            Square(2);
            Func<int, int> twice = x => Square(x) * 2;
        }

        public class Nested
        {
            public void Run() { }
        }
    }

    public interface IBiller
    {
        decimal Bill(Order order);
    }

    public struct Point { public int X; public int Len() => X; }
    public record Money(decimal Amount);
    public enum Kind { A, B }
}
"#;

const CSHARP_FORMATS: &str = r#"namespace Shop.Billing
{
    public static class Formats
    {
        public static string FormatMoney(decimal d) => d.ToString();
    }

    public class Ledger
    {
        public void Post() { }
    }
}
"#;

fn csharp_repo() -> Repo {
    Repo::new(&[
        ("Billing/Biller.cs", CSHARP_BILLER),
        ("Billing/Formats.cs", CSHARP_FORMATS),
    ])
}

const CS: &str = "Billing.Biller.Shop.Billing";

fn cs(rest: &str) -> String {
    format!("{CS}.{rest}")
}

#[test]
fn csharp_symbols_have_kinds_qualified_names_and_lines() {
    let r = csharp_repo();
    let want: Vec<(String, &str, i64, i64)> = vec![
        (CS.to_string(), MOD, 4, 58),
        (cs("Biller"), C, 7, 48),
        (cs("Biller.Biller"), M, 11, 11),
        (cs("Biller.Bill"), M, 13, 13),
        (cs("Biller.Round"), M, 20, 20),
        (cs("Biller.Pick"), M, 22, 22),
        (cs("Biller.Compute"), M, 24, 24),
        (cs("Biller.SaveAsync"), M, 26, 35),
        (cs("Biller.Local"), M, 37, 42),
        (cs("Biller.Local.Square"), F, 39, 39),
        (cs("Biller.Nested"), C, 44, 47),
        (cs("Biller.Nested.Run"), M, 46, 46),
        (cs("IBiller"), I, 50, 53),
        (cs("IBiller.Bill"), M, 52, 52),
        (cs("Point"), C, 55, 55),
        (cs("Point.Len"), M, 55, 55),
        (cs("Money"), C, 56, 56),
        (cs("Kind"), E, 57, 57),
    ];
    let want: Vec<(&str, &str, i64, i64)> = want.iter().map(|(q, k, s, e)| (q.as_str(), *k, *s, *e)).collect();
    r.expect_symbols(&want);
    assert_eq!(r.sym(&cs("Biller")).doc, "Bills orders.");
    assert_eq!(
        r.sym(&cs("Biller.Pick")).signature,
        "public T Pick<T>(System.Collections.Generic.IEnumerable<T> items) where T : class"
    );
    assert_eq!(
        r.rationale(),
        [(
            cs("Biller.SaveAsync"),
            "NOTE: the repository owns the transaction".to_string()
        )]
    );
}

#[test]
fn csharp_calls_resolve_with_provenance() {
    let r = csharp_repo();
    r.expect_call(&cs("Biller.Bill"), "Sum", "order.Lines");
    r.expect_call(&cs("Biller.Bill"), "Amount", "l");
    r.expect_call(&cs("Biller.SaveAsync"), "SaveAsync", "_repo");
    r.expect_call(&cs("Biller.SaveAsync"), "ConfigureAwait", "_repo.SaveAsync(this)");
    r.expect_call(&cs("Biller.SaveAsync"), "Compute", "self");
    r.expect_call(&cs("Biller.SaveAsync"), "Pick", "");
    r.expect_edge(&cs("Biller.Bill"), &cs("Biller.Round"), method::STATIC, Some(0.95));
    r.expect_edge(
        &cs("Biller.SaveAsync"),
        &cs("Biller.Compute"),
        method::STATIC,
        Some(0.95),
    );
    r.expect_edge(&cs("Biller.SaveAsync"), &cs("Biller.Round"), method::STATIC, Some(0.9));
    r.expect_edge(&cs("Biller.SaveAsync"), &cs("Biller.Pick"), method::STATIC, Some(0.95));
    r.expect_edge(
        &cs("Biller.Local"),
        &cs("Biller.Local.Square"),
        method::STATIC,
        Some(0.95),
    );
    r.expect_edge(
        &cs("Biller.SaveAsync"),
        "Billing.Formats.Shop.Billing.Formats.FormatMoney",
        method::INFERRED,
        None,
    );
    r.expect_edge(
        &cs("Biller.SaveAsync"),
        "Billing.Formats.Shop.Billing.Ledger",
        method::INFERRED,
        None,
    );
    r.expect_edge(
        &cs("Biller.SaveAsync"),
        "Billing.Formats.Shop.Billing.Ledger.Post",
        method::INFERRED,
        None,
    );
    r.expect_static_stays_in_its_file();
}

/// `namespace Shop.Scoped;` (C# 10, the default in new projects) scopes the
/// file exactly as `namespace Shop.Scoped { ... }` does, which is a module.
#[test]
fn csharp_file_scoped_namespace_is_a_module_like_a_block_namespace() {
    let src = "namespace Shop.Scoped;\n\npublic class Cart\n{\n    public int Total() => 1;\n}\n";
    let r = Repo::new(&[("Shop/Cart.cs", src)]);
    r.expect_symbols(&[
        ("Shop.Cart.Shop.Scoped", MOD, 1, 6),
        ("Shop.Cart.Shop.Scoped.Cart", C, 3, 6),
        ("Shop.Cart.Shop.Scoped.Cart.Total", M, 5, 5),
    ]);
}

/// Attributes sit between a doc comment and its member the way `@Override`
/// and `#[test]` do.
#[test]
fn csharp_doc_comment_above_attributes_is_the_doc() {
    let src = "public class CartController\n{\n    /// <summary>Lists the carts.</summary>\n    [HttpGet(\"carts\")]\n    public string List() => \"\";\n}\n";
    let r = Repo::new(&[("Api/CartController.cs", src)]);
    assert_eq!(r.sym("Api.CartController.CartController.List").doc, "Lists the carts.");
}

// ---------------------------------------------------------------------------
// Groovy
// ---------------------------------------------------------------------------

const GROOVY_CART: &str = r#"package shop

import groovy.transform.CompileStatic

/** The cart. */
@CompileStatic
class Cart {
    List<Integer> items = []
    String name
    int port = 8080

    /** Adds up the item prices. */
    int subtotal() {
        items.sum() as int
    }

    def total() {
        // TODO: read the rate from config
        applyTax(subtotal())
    }

    static int applyTax(int amount) {
        amount * 108 / 100
    }

    def each(Closure c) {
        items.collect { format(it) }
    }

    private String format(int n) { "$n" }

    Cart() {
        this.name = 'x'
        init()
    }

    void init() {}

    static class Line {
        int amount() { 1 }
    }
}

interface Priced {
    int total()
}

enum Color { RED, GREEN }

class Checkout {
    String run(Cart cart) {
        Cart.applyTax(1)
        new Cart().total()
        Money.formatMoney(cart.total())
    }
}
"#;

const GROOVY_MONEY: &str =
    "package shop\n\nclass Money {\n    static String formatMoney(int cents) { \"$cents\" }\n}\n";

fn groovy_repo() -> Repo {
    Repo::new(&[("shop/Cart.groovy", GROOVY_CART), ("shop/Money.groovy", GROOVY_MONEY)])
}

#[test]
fn groovy_symbols_have_kinds_qualified_names_and_lines() {
    let r = groovy_repo();
    r.expect_symbols(&[
        ("shop.Cart.Cart", C, 6, 42),
        ("shop.Cart.Cart.subtotal", M, 13, 15),
        ("shop.Cart.Cart.total", M, 17, 20),
        ("shop.Cart.Cart.applyTax", M, 22, 24),
        ("shop.Cart.Cart.each", M, 26, 28),
        ("shop.Cart.Cart.format", M, 30, 30),
        ("shop.Cart.Cart.Cart", M, 32, 35),
        ("shop.Cart.Cart.init", M, 37, 37),
        ("shop.Cart.Cart.Line", C, 39, 41),
        ("shop.Cart.Cart.Line.amount", M, 40, 40),
        ("shop.Cart.Priced", I, 44, 46),
        ("shop.Cart.Priced.total", M, 45, 45),
        ("shop.Cart.Color", E, 48, 48),
        ("shop.Cart.Checkout", C, 50, 56),
        ("shop.Cart.Checkout.run", M, 51, 55),
        ("shop.Money.Money.formatMoney", M, 4, 4),
    ]);
    assert_eq!(r.sym("shop.Cart.Cart").doc, "The cart.");
    assert_eq!(r.sym("shop.Cart.Cart.subtotal").doc, "Adds up the item prices.");
    assert_eq!(
        r.rationale(),
        [(
            "shop.Cart.Cart.total".to_string(),
            "TODO: read the rate from config".to_string()
        )]
    );
}

#[test]
fn groovy_calls_resolve_with_provenance() {
    let r = groovy_repo();
    r.expect_call("shop.Cart.Checkout.run", "applyTax", "Cart");
    r.expect_call("shop.Cart.Checkout.run", "total", "Cart()");
    r.expect_call("shop.Cart.Checkout.run", "formatMoney", "Money");
    r.expect_call("shop.Cart.Checkout.run", "total", "cart");
    r.expect_edge(
        "shop.Cart.Cart.total",
        "shop.Cart.Cart.applyTax",
        method::STATIC,
        Some(0.95),
    );
    r.expect_edge(
        "shop.Cart.Cart.total",
        "shop.Cart.Cart.subtotal",
        method::STATIC,
        Some(0.95),
    );
    // A call in a closure belongs to the method around it.
    r.expect_edge(
        "shop.Cart.Cart.each",
        "shop.Cart.Cart.format",
        method::STATIC,
        Some(0.95),
    );
    r.expect_edge("shop.Cart.Cart.Cart", "shop.Cart.Cart.init", method::STATIC, Some(0.95));
    r.expect_edge(
        "shop.Cart.Checkout.run",
        "shop.Cart.Cart.applyTax",
        method::STATIC,
        Some(0.9),
    );
    r.expect_edge("shop.Cart.Checkout.run", "shop.Cart.Cart", method::STATIC, Some(0.95));
    r.expect_edge(
        "shop.Cart.Checkout.run",
        "shop.Money.Money.formatMoney",
        method::INFERRED,
        None,
    );
    r.expect_static_stays_in_its_file();
}

/// A Groovy script (a Jenkinsfile, a build script) defines methods at the top
/// level: `def checkout(Cart cart) { ... }`.
#[test]
fn groovy_script_methods_are_functions() {
    let src = "class Cart {\n    int total() { 1 }\n}\n\ndef checkout(Cart cart) {\n    cart.total()\n}\n\ncheckout(new Cart())\n";
    let r = Repo::new(&[("ci/deploy.groovy", src)]);
    r.expect_symbols(&[("ci.deploy.checkout", F, 5, 7)]);
    assert_eq!(r.caller_at("total", 6).qualified_name, "ci.deploy.checkout");
    r.expect_edge("ci.deploy", "ci.deploy.checkout", method::STATIC, Some(0.95));
}

/// Spock feature methods are named by a string. Whatever name they get, it is
/// not the keyword `def`, and two features are two symbols.
#[test]
fn groovy_spock_feature_methods_are_not_one_symbol_named_def() {
    let src = r#"import spock.lang.Specification

class CartSpec extends Specification {
    def "totals the items"() {
        expect:
        new Cart().total() == 1
    }

    def "is empty at first"() {
        expect:
        new Cart().empty()
    }
}
"#;
    let r = Repo::new(&[("src/test/groovy/shop/CartSpec.groovy", src)]);
    let named_def: Vec<&str> = r
        .input
        .symbols
        .iter()
        .filter(|s| s.name == "def")
        .map(|s| s.qualified_name.as_str())
        .collect();
    assert!(named_def.is_empty(), "symbols named def: {named_def:?}");
    assert_ne!(
        r.caller_at("total", 6).id,
        r.caller_at("empty", 11).id,
        "two feature methods share one symbol"
    );
}

// ---------------------------------------------------------------------------
// Swift
// ---------------------------------------------------------------------------

const SWIFT_CART: &str = r#"import Foundation

/// Something with a price.
protocol Priced {
    func total() -> Int
}

/// The cart.
struct Cart: Priced {
    var items: [Int]

    /// Adds up.
    func subtotal() -> Int {
        return items.reduce(0, +)
    }

    func total() -> Int {
        // NOTE: tax is applied once
        return applyTax(subtotal())
    }

    static func make() -> Cart {
        return Cart(items: [])
    }

    func pick<T>(_ xs: [T]) -> T { xs.first! }

    struct Line {
        func amount() -> Int { 1 }
    }
}

extension Cart {
    func discounted() -> Int {
        return total() - 1
    }
}

@MainActor
final class ViewModel {
    weak var delegate: Delegate?
    let format = { (x: Int) in "\(x)" }

    func load() async throws {
        let cart = Cart.make()
        delegate?.didLoad(cart)
        try await api.fetch(cart).validate()
        _ = cart.items.map { applyTax($0) }
        _ = format(1)
        _ = formatMoney(cart.total())
    }
}

func applyTax(_ amount: Int) -> Int {
    return amount * 108 / 100
}

func größe() -> Int { applyTax(1) }
"#;

const SWIFT_MONEY: &str =
    "import Foundation\n\nfunc formatMoney(_ cents: Int) -> String {\n    return String(cents)\n}\n";

fn swift_repo() -> Repo {
    Repo::new(&[
        ("Sources/Shop/Cart.swift", SWIFT_CART),
        ("Sources/Shop/Money.swift", SWIFT_MONEY),
    ])
}

const SW: &str = "Sources.Shop.Cart";

fn sw(rest: &str) -> String {
    format!("{SW}.{rest}")
}

#[test]
fn swift_symbols_have_kinds_qualified_names_and_lines() {
    let r = swift_repo();
    let want: Vec<(String, &str, i64, i64)> = vec![
        (sw("Priced"), I, 4, 6),
        (sw("Priced.total"), M, 5, 5),
        (sw("Cart"), C, 9, 31),
        (sw("Cart.subtotal"), M, 13, 15),
        (sw("Cart.total"), M, 17, 20),
        (sw("Cart.make"), M, 22, 24),
        (sw("Cart.pick"), M, 26, 26),
        (sw("Cart.Line"), C, 28, 30),
        (sw("Cart.Line.amount"), M, 29, 29),
        // A method of an extension is a method of the extended type.
        (sw("Cart.discounted"), M, 34, 36),
        (sw("ViewModel"), C, 39, 52),
        (sw("ViewModel.load"), M, 44, 51),
        (sw("applyTax"), F, 54, 56),
        (sw("größe"), F, 58, 58),
        ("Sources.Shop.Money.formatMoney".to_string(), F, 3, 5),
    ];
    let want: Vec<(&str, &str, i64, i64)> = want.iter().map(|(q, k, s, e)| (q.as_str(), *k, *s, *e)).collect();
    r.expect_symbols(&want);
    assert_eq!(r.sym(&sw("Priced")).doc, "Something with a price.");
    assert_eq!(r.sym(&sw("Cart")).doc, "The cart.");
    assert_eq!(r.sym(&sw("Cart.subtotal")).doc, "Adds up.");
    assert_eq!(r.sym(&sw("Cart.pick")).signature, "func pick<T>(_ xs: [T]) -> T");
    assert_eq!(
        r.rationale(),
        [(sw("Cart.total"), "NOTE: tax is applied once".to_string())]
    );
}

#[test]
fn swift_calls_resolve_with_provenance() {
    let r = swift_repo();
    r.expect_call(&sw("ViewModel.load"), "make", "Cart");
    r.expect_call(&sw("ViewModel.load"), "didLoad", "delegate");
    r.expect_call(&sw("ViewModel.load"), "validate", "api.fetch(cart)");
    r.expect_call(&sw("ViewModel.load"), "map", "cart.items");
    r.expect_call(&sw("ViewModel.load"), "applyTax", "");
    r.expect_edge(&sw("Cart.total"), &sw("Cart.subtotal"), method::STATIC, Some(0.95));
    r.expect_edge(&sw("Cart.total"), &sw("applyTax"), method::STATIC, Some(0.95));
    r.expect_edge(&sw("Cart.make"), &sw("Cart"), method::STATIC, Some(0.95));
    r.expect_edge(&sw("Cart.discounted"), &sw("Cart.total"), method::STATIC, Some(0.95));
    r.expect_edge(&sw("ViewModel.load"), &sw("Cart.make"), method::STATIC, Some(0.9));
    r.expect_edge(&sw("ViewModel.load"), &sw("applyTax"), method::STATIC, Some(0.95));
    r.expect_edge(&sw("ViewModel.load"), &sw("Cart.total"), method::INFERRED, None);
    r.expect_edge(
        &sw("ViewModel.load"),
        "Sources.Shop.Money.formatMoney",
        method::INFERRED,
        None,
    );
    r.expect_edge(&sw("größe"), &sw("applyTax"), method::STATIC, Some(0.95));
    // `format` is a closure property, not a symbol.
    assert!(r.call_edges().all(|e| !r.qn(&e.to).ends_with(".format")));
    r.expect_static_stays_in_its_file();
}

/// `init` is Swift's constructor; Java's constructors are methods, so is it.
#[test]
fn swift_initializers_are_methods_that_own_their_calls() {
    let src = "struct Cart {\n    var items: [Int]\n\n    init(items: [Int]) {\n        self.items = items\n        validate()\n    }\n\n    private func validate() {}\n}\n";
    let r = Repo::new(&[("Shop/Cart.swift", src)]);
    r.expect_symbols(&[("Shop.Cart.Cart.init", M, 4, 7)]);
    assert_eq!(r.caller_at("validate", 6).qualified_name, "Shop.Cart.Cart.init");
    r.expect_edge(
        "Shop.Cart.Cart.init",
        "Shop.Cart.Cart.validate",
        method::STATIC,
        Some(0.95),
    );
}

/// `xs[0]` and `table["a"]` read a collection; they call nothing named `xs`.
#[test]
fn swift_subscripts_are_not_calls() {
    let src = "struct Stats {\n    func first(_ xs: [Int], _ table: [String: Int]) -> Int {\n        return xs[0] + table[\"a\"]!\n    }\n}\n";
    let r = Repo::new(&[("Shop/Stats.swift", src)]);
    let names: Vec<&str> = r.input.calls.iter().map(|c| c.name.as_str()).collect();
    assert!(!names.contains(&"xs") && !names.contains(&"table"), "calls: {names:?}");
}

/// SwiftPM keeps every test target under `Tests/`; a helper file there is test code.
#[test]
fn swift_files_under_the_swiftpm_tests_directory_are_tests() {
    let src = "import XCTest\n\nfunc makeCart() -> Int { 1 }\n";
    let r = Repo::new(&[("Tests/ShopTests/Fixtures.swift", src)]);
    assert!(r.sym("Tests.ShopTests.Fixtures").is_test);
    assert!(r.sym("Tests.ShopTests.Fixtures.makeCart").is_test);
}

/// Swift calls Objective-C methods of the same target by the same name
/// (`cart.recompute()` sends `[cart recompute]`), as Kotlin calls Java.
#[test]
fn swift_calls_into_objective_c_by_name() {
    let objc = "@implementation Cart\n- (void)recompute {}\n@end\n";
    let swift = "func refresh(_ cart: Cart) {\n    cart.recompute()\n}\n";
    let r = Repo::new(&[("App/Cart.m", objc), ("App/Refresh.swift", swift)]);
    r.expect_edge("App.Refresh.refresh", "App.Cart.Cart.recompute", method::INFERRED, None);
}

// ---------------------------------------------------------------------------
// Objective-C
// ---------------------------------------------------------------------------

const OBJC_CART: &str = r#"#import "Cart.h"

// The prices of the items a customer picked.
@interface Cart : NSObject
- (NSInteger)subtotal;
@end

@implementation Cart

// Builds a cart.
+ (instancetype)cart {
    Cart *cart = [[Cart alloc] init];
    [cart recompute];
    return cart;
}

- (void)recompute {
    // NOTE: cached until the items change
    self.cached = [self subtotal];
    void (^log)(NSInteger) = ^(NSInteger x) {
        [self report:x];
        NSLog(@"%ld", (long)x);
    };
    log(self.cached);
}

- (void)report:(NSInteger)x {}

- (NSInteger)subtotal {
    return ApplyTax([[self.items valueForKeyPath:@"@sum.self"] integerValue]);
}

@end

@implementation Cart (Pricing)

- (NSInteger)discounted {
    return [self subtotal] - 1;
}

@end

NSInteger ApplyTax(NSInteger amount) {
    return amount * 108 / 100;
}

static NSString *Describe(Cart *cart) {
    return FormatMoney([cart discounted]);
}
"#;

const OBJC_MONEY: &str = "#import <Foundation/Foundation.h>\n\nNSString *FormatMoney(NSInteger cents) {\n    return [NSString stringWithFormat:@\"%ld\", (long)cents];\n}\n";

fn objc_repo() -> Repo {
    Repo::new(&[("Shop/Cart.m", OBJC_CART), ("Shop/Money.m", OBJC_MONEY)])
}

#[test]
fn objc_symbols_have_kinds_qualified_names_and_lines() {
    let r = objc_repo();
    r.expect_symbols(&[
        ("Shop.Cart.Cart", C, 4, 6),
        ("Shop.Cart.Cart.cart", M, 11, 15),
        ("Shop.Cart.Cart.recompute", M, 17, 25),
        ("Shop.Cart.Cart.report", M, 27, 27),
        ("Shop.Cart.Cart.subtotal", M, 29, 31),
        // A category adds methods to its class.
        ("Shop.Cart.Cart.discounted", M, 37, 39),
        ("Shop.Cart.ApplyTax", F, 43, 45),
        ("Shop.Cart.Describe", F, 47, 49),
        ("Shop.Money.FormatMoney", F, 3, 5),
    ]);
    assert_eq!(
        r.sym("Shop.Cart.Cart").doc,
        "The prices of the items a customer picked."
    );
    assert_eq!(r.sym("Shop.Cart.Cart.cart").doc, "Builds a cart.");
    assert_eq!(
        r.rationale(),
        [(
            "Shop.Cart.Cart.recompute".to_string(),
            "NOTE: cached until the items change".to_string()
        )]
    );
}

#[test]
fn objc_messages_resolve_with_provenance() {
    let r = objc_repo();
    r.expect_call("Shop.Cart.Cart.cart", "alloc", "Cart");
    r.expect_call("Shop.Cart.Cart.cart", "init", "[Cart alloc]");
    r.expect_call("Shop.Cart.Cart.cart", "recompute", "cart");
    // A message in a block belongs to the method around it.
    r.expect_call("Shop.Cart.Cart.recompute", "report", "self");
    r.expect_call("Shop.Cart.Cart.recompute", "NSLog", "");
    r.expect_call("Shop.Cart.Cart.subtotal", "valueForKeyPath", "self.items");
    r.expect_edge(
        "Shop.Cart.Cart.recompute",
        "Shop.Cart.Cart.subtotal",
        method::STATIC,
        Some(0.95),
    );
    r.expect_edge(
        "Shop.Cart.Cart.recompute",
        "Shop.Cart.Cart.report",
        method::STATIC,
        Some(0.95),
    );
    r.expect_edge(
        "Shop.Cart.Cart.subtotal",
        "Shop.Cart.ApplyTax",
        method::STATIC,
        Some(0.95),
    );
    r.expect_edge(
        "Shop.Cart.Cart.discounted",
        "Shop.Cart.Cart.subtotal",
        method::STATIC,
        Some(0.95),
    );
    r.expect_edge(
        "Shop.Cart.Cart.cart",
        "Shop.Cart.Cart.recompute",
        method::INFERRED,
        None,
    );
    r.expect_edge("Shop.Cart.Describe", "Shop.Money.FormatMoney", method::INFERRED, None);
    r.expect_edge(
        "Shop.Cart.Describe",
        "Shop.Cart.Cart.discounted",
        method::INFERRED,
        None,
    );
    r.expect_static_stays_in_its_file();
}

/// `- (void)addItem:(NSInteger)price quantity:(NSInteger)qty` is one method
/// (selector `addItem:quantity:`), and `[cart addItem:1 quantity:2]` is one
/// message to `cart`. The keyword `count:` of `initWithObjects:count:` is no
/// call of the C function `count` that happens to live in the same file.
#[test]
fn objc_a_multi_part_selector_is_one_method_and_one_message() {
    let src = r#"#import <Foundation/Foundation.h>

static NSInteger count(NSArray *items) {
    return (NSInteger)items.count;
}

@implementation Cart

- (void)addItem:(NSInteger)price quantity:(NSInteger)qty {
    [self recompute];
}

- (void)recompute {}

@end

NSArray *Build(id *objs, NSUInteger n) {
    Cart *cart = [Cart new];
    [cart addItem:1 quantity:2];
    return [[NSArray alloc] initWithObjects:objs count:n];
}
"#;
    let r = Repo::new(&[("Shop/Build.m", src)]);
    let at_9: Vec<&str> = r
        .input
        .symbols
        .iter()
        .filter(|s| s.start_line == 9)
        .map(|s| s.qualified_name.as_str())
        .collect();
    let mut problems = Vec::new();
    if at_9.len() != 1 {
        problems.push(format!("one selector gives {} methods: {at_9:?}", at_9.len()));
    }
    let at_19: Vec<(String, String)> = r
        .calls_named("addItem")
        .iter()
        .chain(r.calls_named("quantity").iter())
        .filter(|c| c.line == 19)
        .map(|c| (c.name.clone(), c.receiver.clone()))
        .collect();
    if at_19 != [("addItem".to_string(), "cart".to_string())] {
        problems.push(format!("one message gives the calls {at_19:?}"));
    }
    if !r.calls_at("count", 20).is_empty() {
        problems.push("the selector keyword `count:` is a bare call `count`".to_string());
    }
    let wrong = r.static_into("Shop.Build.count");
    if !wrong.is_empty() {
        problems.push(format!("static edge into the C function count from {wrong:?}"));
    }
    assert!(problems.is_empty(), "\n{}", problems.join("\n"));
}

/// Idiomatic Groovy writes no `;`. A field without one (`def name = 'x'`,
/// `String name` after a method) must not swallow or hide the methods after it.
#[test]
fn groovy_fields_without_semicolons_do_not_hide_methods() {
    let check = |src: &'static str, want: &'static [(&'static str, i64, i64)]| {
        move || -> Result<(), String> {
            let r = Repo::new(&[("g/A.groovy", src)]);
            let got: Vec<(String, i64, i64)> = r
                .input
                .symbols
                .iter()
                .filter(|s| s.kind == M)
                .map(|s| (s.name.clone(), s.start_line, s.end_line))
                .collect();
            let want: Vec<(String, i64, i64)> = want.iter().map(|(n, s, e)| (n.to_string(), *s, *e)).collect();
            if got == want {
                Ok(())
            } else {
                Err(format!("methods {got:?}, want {want:?}"))
            }
        }
    };
    let def_field = check(
        "class A {\n    def name = 'x'\n    int g() { 2 }\n    int h() { 3 }\n}\n",
        &[("g", 3, 3), ("h", 4, 4)],
    );
    let field_after_method = check(
        "class A {\n    int g() { 2 }\n    String name\n    int h() { 3 }\n}\n",
        &[("g", 2, 2), ("h", 4, 4)],
    );
    let last_field = check("class A {\n    int f() { 1 }\n    int x = 1\n}\n", &[("f", 2, 2)]);
    each_case(&[
        ("def field first", &def_field),
        ("field after a method", &field_after_method),
        ("field last", &last_field),
    ]);
}

// ---------------------------------------------------------------------------
// Dart
// ---------------------------------------------------------------------------

const DART_CART: &str = r#"import 'package:shop/money.dart';

/// A cart.
class Cart {
  final List<int> items = [];

  int sum(List<int> xs) => xs.fold(0, (a, b) => a + b);

  /// The total with tax.
  int total() => applyTax(sum(items));

  static int applyTax(int amount) => amount * 108 ~/ 100;

  Future<void> save() async {
    // NOTE: the repository retries
    await repo.save(this);
    _log('saved');
  }

  void _log(String m) => print(m);

  T pick<T>(List<T> xs) => xs.first;

  @override
  String toString() => 'Cart(${total()})';
}

mixin Discount on Cart {
  int discounted() => 1;
}

extension CartX on Cart {
  int doubled() => 2;
}

enum Status {
  open,
  paid;

  bool isDone() => this == paid;
}

int topLevel() => Cart.applyTax(1);

void main() {
  final c = Cart();
  print(c.total());
  formatMoney(topLevel());
  Cart().save().then((_) => print('ok'));
}
"#;

const DART_MONEY: &str = "String formatMoney(int cents) => '$cents';\n";

fn dart_repo() -> Repo {
    Repo::new(&[("lib/cart.dart", DART_CART), ("lib/money.dart", DART_MONEY)])
}

#[test]
fn dart_symbols_have_kinds_qualified_names_and_lines() {
    let r = dart_repo();
    r.expect_symbols(&[
        ("lib.cart.Cart", C, 4, 26),
        ("lib.cart.Cart.sum", M, 7, 7),
        ("lib.cart.Cart.total", M, 10, 10),
        ("lib.cart.Cart.applyTax", M, 12, 12),
        ("lib.cart.Cart.save", M, 14, 18),
        ("lib.cart.Cart._log", M, 20, 20),
        ("lib.cart.Cart.pick", M, 22, 22),
        ("lib.cart.Cart.toString", M, 24, 25),
        ("lib.cart.Discount", C, 28, 30),
        ("lib.cart.Discount.discounted", M, 29, 29),
        ("lib.cart.CartX", C, 32, 34),
        ("lib.cart.CartX.doubled", M, 33, 33),
        ("lib.cart.Status", E, 36, 41),
        ("lib.cart.Status.isDone", M, 40, 40),
        ("lib.cart.topLevel", F, 43, 43),
        ("lib.cart.main", F, 45, 50),
        ("lib.money.formatMoney", F, 1, 1),
    ]);
    assert_eq!(r.sym("lib.cart.Cart").doc, "A cart.");
    assert_eq!(r.sym("lib.cart.Cart.total").doc, "The total with tax.");
    assert_eq!(r.sym("lib.cart.Cart.pick").signature, "T pick<T>(List<T> xs)");
    assert_eq!(
        r.rationale(),
        [(
            "lib.cart.Cart.save".to_string(),
            "NOTE: the repository retries".to_string()
        )]
    );
}

#[test]
fn dart_calls_resolve_with_provenance() {
    let r = dart_repo();
    r.expect_call("lib.cart.Cart.save", "save", "repo");
    r.expect_call("lib.cart.main", "total", "c");
    r.expect_call("lib.cart.main", "save", "Cart()");
    r.expect_call("lib.cart.main", "then", "Cart().save()");
    r.expect_edge(
        "lib.cart.Cart.total",
        "lib.cart.Cart.applyTax",
        method::STATIC,
        Some(0.95),
    );
    r.expect_edge("lib.cart.Cart.total", "lib.cart.Cart.sum", method::STATIC, Some(0.95));
    r.expect_edge("lib.cart.Cart.save", "lib.cart.Cart._log", method::STATIC, Some(0.95));
    // A call in string interpolation.
    r.expect_edge(
        "lib.cart.Cart.toString",
        "lib.cart.Cart.total",
        method::STATIC,
        Some(0.95),
    );
    r.expect_edge("lib.cart.topLevel", "lib.cart.Cart.applyTax", method::STATIC, Some(0.9));
    r.expect_edge("lib.cart.main", "lib.cart.Cart", method::STATIC, Some(0.95));
    r.expect_edge("lib.cart.main", "lib.cart.topLevel", method::STATIC, Some(0.95));
    r.expect_edge("lib.cart.main", "lib.cart.Cart.save", method::STATIC, Some(0.9));
    r.expect_edge("lib.cart.main", "lib.cart.Cart.total", method::INFERRED, None);
    r.expect_edge("lib.cart.main", "lib.money.formatMoney", method::INFERRED, None);
    r.expect_static_stays_in_its_file();
}

/// Dart constructors: unnamed (`Cart(this.items);`), named (`Cart.empty()`)
/// and factory (`factory Cart.of(...)`). Java's constructors are methods; so
/// are these, and a call in a factory body belongs to the factory.
#[test]
fn dart_constructors_are_methods_that_own_their_calls() {
    let src = r#"class Cart {
  final List<int> items;

  Cart(this.items);

  Cart.empty() : items = const [];

  factory Cart.of(List<int> xs) {
    return Cart(validate(xs));
  }

  static List<int> validate(List<int> xs) => xs;
}
"#;
    let r = Repo::new(&[("lib/ctor.dart", src)]);
    let methods = |name: &str| -> Vec<(i64, i64)> {
        r.input
            .symbols
            .iter()
            .filter(|s| s.name == name && s.kind == M)
            .map(|s| (s.start_line, s.end_line))
            .collect()
    };
    let mut problems = Vec::new();
    for (what, name, lines) in [
        ("unnamed constructor", "Cart", (4, 4)),
        ("named constructor", "empty", (6, 6)),
        ("factory constructor", "of", (8, 10)),
    ] {
        if methods(name) != [lines] {
            problems.push(format!("{what} {name}: methods at {:?}", methods(name)));
        }
    }
    let owner = r.caller_at("validate", 9);
    if owner.name != "of" {
        problems.push(format!(
            "the call in the factory belongs to {} ({})",
            owner.qualified_name, owner.kind
        ));
    }
    assert!(
        problems.is_empty(),
        "\n{}\nsymbols:\n{}",
        problems.join("\n"),
        r.listing()
    );
}

/// `sb..write('a')` calls `write` on `sb` (a cascade), and `c.pick<int>(xs)`
/// calls `pick` on `c` (explicit type arguments).
#[test]
fn dart_cascade_and_generic_calls_are_calls_on_their_receiver() {
    let src = r#"class Report {
  void write(String s) {}
  String render(StringBuffer sb, Cart c) {
    sb..write('a')..write('b');
    c.pick<int>([1]).toString();
    return sb.toString();
  }
}
"#;
    let r = Repo::new(&[("lib/report.dart", src)]);
    let mut problems = Vec::new();
    let writes: Vec<String> = r.calls_at("write", 4).iter().map(|c| c.receiver.clone()).collect();
    if writes != ["sb", "sb"] {
        problems.push(format!(
            "cascade calls of write on line 4 have the receivers {writes:?}"
        ));
    }
    // Neither is the class's own `write`.
    if let Some(e) = r.edge("lib.report.Report.render", "lib.report.Report.write") {
        problems.push(format!("{} edge to the class's own write", e.method));
    }
    let picks: Vec<String> = r.calls_at("pick", 5).iter().map(|c| c.receiver.clone()).collect();
    if picks != ["c"] {
        problems.push(format!("pick<int> on line 5 has the receivers {picks:?}"));
    }
    let to_string: Vec<String> = r.calls_at("toString", 5).iter().map(|c| c.receiver.clone()).collect();
    if to_string != ["c.pick<int>([1])"] {
        problems.push(format!("toString on line 5 has the receivers {to_string:?}"));
    }
    assert!(problems.is_empty(), "\n{}", problems.join("\n"));
}

/// Inside `extension CartX on Cart` and `mixin Discount on Cart`, a bare
/// `total()` is `this.total()` on the Cart the clause names. It is not dropped.
#[test]
fn dart_extension_and_mixin_bodies_reach_the_type_they_are_on() {
    let src = r#"class Cart {
  int total() => 1;
}

extension CartX on Cart {
  int doubled() => total() * 2;
}

mixin Discount on Cart {
  int discounted() => total() - 1;
}
"#;
    let r = Repo::new(&[("lib/ext.dart", src)]);
    let mut problems = Vec::new();
    for from in ["lib.ext.CartX.doubled", "lib.ext.Discount.discounted"] {
        match r.edge(from, "lib.ext.Cart.total") {
            Some(e) if e.method == method::STATIC || e.method == method::INFERRED => {}
            Some(e) => problems.push(format!("{from} -> Cart.total is {}", e.method)),
            None => problems.push(format!("no edge {from} -> Cart.total")),
        }
    }
    assert!(problems.is_empty(), "\n{}", problems.join("\n"));
}

// ---------------------------------------------------------------------------
// Across the languages
// ---------------------------------------------------------------------------

/// A Java inner class and a Kotlin `inner class` call the enclosing class's
/// methods without a receiver. Such a call is not dropped.
#[test]
fn a_bare_call_in_an_inner_class_reaches_the_enclosing_class() {
    let check = |path: &'static str, src: &'static str, from: &'static str, to: &'static str| {
        move || -> Result<(), String> {
            let r = Repo::new(&[(path, src)]);
            match r.edge(from, to) {
                Some(e) if e.method == method::STATIC || e.method == method::INFERRED => Ok(()),
                Some(e) => Err(format!("{from} -> {to} is {}", e.method)),
                None => Err(format!("no edge {from} -> {to}")),
            }
        }
    };
    let java = check(
        "i/Cart.java",
        "class Cart {\n    int total() { return 1; }\n\n    class View {\n        int show() {\n            return total();\n        }\n    }\n}\n",
        "i.Cart.Cart.View.show",
        "i.Cart.Cart.total",
    );
    let kotlin = check(
        "i/Cart.kt",
        "class Cart {\n    fun total(): Int = 1\n\n    inner class View {\n        fun show() = total()\n    }\n}\n",
        "i.Cart.Cart.View.show",
        "i.Cart.Cart.total",
    );
    each_case(&[("java", &java), ("kotlin", &kotlin)]);
}

/// A field initializer or computed property of a nested type that calls
/// `total()` calls the nested type's own `total` (or, in a language without an
/// implicit `this` there, nothing known). It never reaches the outer type's
/// `total`, least of all as a `static` edge.
#[test]
fn a_nested_type_member_initializer_never_reaches_the_outer_type() {
    let check = |path: &'static str, src: &'static str, outer_total: &'static str| {
        move || -> Result<(), String> {
            let r = Repo::new(&[(path, src)]);
            let wrong = r.static_into(outer_total);
            if wrong.is_empty() {
                Ok(())
            } else {
                Err(format!("static edges into {outer_total} from {wrong:?}"))
            }
        }
    };
    let java = check(
        "n/Outer.java",
        "class Outer {\n    int total() { return 1; }\n    static class Inner {\n        int total() { return 2; }\n        int doubled = total() * 2;\n    }\n}\n",
        "n.Outer.Outer.total",
    );
    let kotlin = check(
        "n/Outer.kt",
        "class Outer {\n    fun total(): Int = 1\n    class Inner {\n        fun total(): Int = 2\n        val doubled = total() * 2\n    }\n}\n",
        "n.Outer.Outer.total",
    );
    let scala = check(
        "n/Outer.scala",
        "class Outer {\n  def total(): Int = 1\n  class Inner {\n    def total(): Int = 2\n    val doubled = total() * 2\n  }\n}\n",
        "n.Outer.Outer.total",
    );
    let csharp = check(
        "n/Outer.cs",
        "class Outer {\n    int Total() => 1;\n    class Inner {\n        int Total() => 2;\n        int Doubled => Total() * 2;\n    }\n}\n",
        "n.Outer.Outer.Total",
    );
    let swift = check(
        "n/Outer.swift",
        "struct Outer {\n    func total() -> Int { 1 }\n    struct Inner {\n        func total() -> Int { 2 }\n        var doubled: Int { total() * 2 }\n    }\n}\n",
        "n.Outer.Outer.total",
    );
    let groovy = check(
        "n/Outer.groovy",
        "class Outer {\n    int total() { 1 }\n    static class Inner {\n        int total() { 2 }\n        int doubled = total() * 2\n    }\n}\n",
        "n.Outer.Outer.total",
    );
    each_case(&[
        ("java", &java),
        ("kotlin", &kotlin),
        ("scala", &scala),
        ("csharp", &csharp),
        ("swift", &swift),
        ("groovy", &groovy),
    ]);
}

/// A method of an anonymous class (`new Runnable() { public void run() }`,
/// Kotlin `object : Runnable { }`, Scala `new Runnable { }`) is a method of
/// that class, not a free function of the file: a bare `run()` elsewhere in the
/// file (here the inherited `Thread.run`) must not become a static call to it.
#[test]
fn an_anonymous_class_method_is_no_bare_call_target() {
    let check = |path: &'static str, src: &'static str, go: &'static str| {
        move || -> Result<(), String> {
            let r = Repo::new(&[(path, src)]);
            let mut problems = Vec::new();
            for s in r.input.symbols.iter().filter(|s| s.name == "run") {
                if s.kind == F {
                    problems.push(format!("{} is a function", s.qualified_name));
                }
                if let Some(e) = r.edge(go, &s.qualified_name) {
                    problems.push(format!("{} edge {go} -> {}", e.method, s.qualified_name));
                }
            }
            if problems.is_empty() {
                Ok(())
            } else {
                Err(problems.join("; "))
            }
        }
    };
    let java = check(
        "a/Svc.java",
        "class Svc {\n    void start() {\n        executor.submit(new Runnable() {\n            @Override\n            public void run() {\n                work();\n            }\n        });\n    }\n\n    void work() {}\n}\n\nclass Worker extends Thread {\n    void go() {\n        run();\n    }\n}\n",
        "a.Svc.Worker.go",
    );
    let kotlin = check(
        "a/Svc.kt",
        "class Svc {\n    fun start() {\n        val r = object : Runnable {\n            override fun run() { work() }\n        }\n    }\n    fun work() {}\n}\n\nclass Worker : Thread() {\n    fun go() { run() }\n}\n",
        "a.Svc.Worker.go",
    );
    let scala = check(
        "a/Svc.scala",
        "class Svc {\n  def start(): Unit = {\n    val r = new Runnable { def run(): Unit = work() }\n  }\n  def work(): Unit = ()\n}\n\nclass Worker extends Thread {\n  def go(): Unit = run()\n}\n",
        "a.Svc.Worker.go",
    );
    each_case(&[("java", &java), ("kotlin", &kotlin), ("scala", &scala)]);
}

/// Java default methods, Kotlin interface methods with a body, Scala trait
/// methods with a body, Swift protocol-extension methods and C# default
/// interface methods are code that runs. A sibling call between two of them is
/// as static as one between two methods of a class.
#[test]
fn concrete_methods_of_interfaces_traits_and_protocol_extensions_are_call_targets() {
    let check = |path: &'static str, src: &'static str, from: &'static str, to: &'static str| {
        move || -> Result<(), String> {
            let r = Repo::new(&[(path, src)]);
            match r.edge(from, to) {
                Some(e) if e.method == method::STATIC => Ok(()),
                Some(e) => Err(format!("{from} -> {to} is {}", e.method)),
                None => Err(format!("no edge {from} -> {to}")),
            }
        }
    };
    let java = check(
        "p/Priced.java",
        "interface Priced {\n    int total();\n\n    default String describe() {\n        return label();\n    }\n\n    default String label() {\n        return \"x\";\n    }\n}\n",
        "p.Priced.Priced.describe",
        "p.Priced.Priced.label",
    );
    let kotlin = check(
        "p/Priced.kt",
        "interface Priced {\n    fun total(): Int\n    fun describe(): String = label()\n    fun label(): String = \"x\"\n}\n",
        "p.Priced.Priced.describe",
        "p.Priced.Priced.label",
    );
    let scala = check(
        "p/Priced.scala",
        "trait Priced {\n  def total: Int\n  def describe(): String = label()\n  def label(): String = \"x\"\n}\n",
        "p.Priced.Priced.describe",
        "p.Priced.Priced.label",
    );
    let swift = check(
        "p/Priced.swift",
        "protocol Priced {\n    func total() -> Int\n}\n\nextension Priced {\n    func describe() -> String { label() }\n    func label() -> String { \"x\" }\n}\n",
        "p.Priced.Priced.describe",
        "p.Priced.Priced.label",
    );
    let csharp = check(
        "p/Priced.cs",
        "interface IPriced\n{\n    int Total();\n    string Describe() => Label();\n    string Label() => \"x\";\n}\n",
        "p.Priced.IPriced.Describe",
        "p.Priced.IPriced.Label",
    );
    each_case(&[
        ("java", &java),
        ("kotlin", &kotlin),
        ("scala", &scala),
        ("swift", &swift),
        ("csharp", &csharp),
    ]);
}

/// In a language with an implicit `this`, a bare `helper()` reaches a member
/// of the class written in another file: an inherited method (Java, Kotlin),
/// the other half of a C# `partial class`, or the type's declaration seen from
/// a Swift extension file. The source does not say which file, so it is a
/// match by name: `inferred` with one candidate. It is not dropped as external
/// while `this.helper()` is matched.
#[test]
fn a_bare_call_to_a_member_defined_in_another_file_is_matched_by_name() {
    let check = |files: &'static [(&'static str, &'static str)], from: &'static str, to: &'static str| {
        move || -> Result<(), String> {
            let r = Repo::new(files);
            match r.edge(from, to) {
                Some(e) if e.method == method::INFERRED => Ok(()),
                Some(e) => Err(format!("{from} -> {to} is {}", e.method)),
                None => Err(format!("no edge {from} -> {to}")),
            }
        }
    };
    let java = check(
        &[
            ("j/Base.java", "class Base {\n    void helper() {}\n}\n"),
            (
                "j/Child.java",
                "class Child extends Base {\n    void run() {\n        helper();\n    }\n}\n",
            ),
        ],
        "j.Child.Child.run",
        "j.Base.Base.helper",
    );
    let kotlin = check(
        &[
            ("k/Base.kt", "open class Base {\n    fun helper() {}\n}\n"),
            (
                "k/Child.kt",
                "class Child : Base() {\n    fun run() {\n        helper()\n    }\n}\n",
            ),
        ],
        "k.Child.Child.run",
        "k.Base.Base.helper",
    );
    let csharp = check(
        &[
            (
                "c/Cart.cs",
                "public partial class Cart\n{\n    public int Total() => Subtotal() + 1;\n}\n",
            ),
            (
                "c/Cart.Items.cs",
                "public partial class Cart\n{\n    public int Subtotal() => 41;\n}\n",
            ),
        ],
        "c.Cart.Cart.Total",
        "c.Cart.Items.Cart.Subtotal",
    );
    let swift = check(
        &[
            ("s/Cart.swift", "struct Cart {\n    func subtotal() -> Int { 41 }\n}\n"),
            (
                "s/Cart+Total.swift",
                "extension Cart {\n    func total() -> Int { subtotal() + 1 }\n}\n",
            ),
        ],
        "s.Cart+Total.Cart.total",
        "s.Cart.Cart.subtotal",
    );
    each_case(&[
        ("java", &java),
        ("kotlin", &kotlin),
        ("csharp", &csharp),
        ("swift", &swift),
    ]);
}

/// A C# property getter, a Swift computed property, a Dart getter and a Kotlin
/// custom getter are code that runs (SwiftUI's `var body: some View { ... }`
/// holds a whole view). Their calls belong to them, not to the class, and a
/// sibling call from them is static.
#[test]
fn property_getters_own_their_calls() {
    let check = |path: &'static str, src: &'static str, line: i64, callee: &'static str, getter: &'static str| {
        move || -> Result<(), String> {
            let r = Repo::new(&[(path, src)]);
            let called = callee.rsplit('.').next().unwrap_or(callee);
            let owner = r.caller_at(called, line);
            if owner.qualified_name != getter {
                return Err(format!(
                    "the call on line {line} belongs to {} ({})",
                    owner.qualified_name, owner.kind
                ));
            }
            match r.edge(getter, callee) {
                Some(e) if e.method == method::STATIC => Ok(()),
                _ => Err(format!("no static edge {getter} -> {callee}")),
            }
        }
    };
    let csharp = check(
        "g/Biller.cs",
        "class Biller\n{\n    decimal Compute() => 1m;\n    public decimal Total { get { return Compute(); } }\n}\n",
        4,
        "g.Biller.Biller.Compute",
        "g.Biller.Biller.Total",
    );
    let csharp_arrow = check(
        "g/Biller.cs",
        "class Biller\n{\n    decimal Compute() => 1m;\n    public decimal Doubled => Compute() * 2;\n}\n",
        4,
        "g.Biller.Biller.Compute",
        "g.Biller.Biller.Doubled",
    );
    let swift = check(
        "g/Cart.swift",
        "struct Cart {\n    func total() -> Int { 1 }\n    var doubled: Int {\n        return total() * 2\n    }\n}\n",
        4,
        "g.Cart.Cart.total",
        "g.Cart.Cart.doubled",
    );
    let dart = check(
        "g/cart.dart",
        "class Cart {\n  int sum() => 1;\n  int get subtotal => sum();\n}\n",
        3,
        "g.cart.Cart.sum",
        "g.cart.Cart.subtotal",
    );
    let kotlin = check(
        "g/Cart.kt",
        "class Cart {\n    fun total(): Int = 1\n    val doubled: Int\n        get() = total() * 2\n}\n",
        4,
        "g.Cart.Cart.total",
        "g.Cart.Cart.doubled",
    );
    each_case(&[
        ("csharp property", &csharp),
        ("csharp expression property", &csharp_arrow),
        ("swift", &swift),
        ("dart", &dart),
        ("kotlin", &kotlin),
    ]);
}

/// `enum class Color` (Kotlin) and `enum Kind` (Swift) are enums, as Java's,
/// C#'s, Scala's, Groovy's and Dart's are.
#[test]
fn enums_are_enums_in_every_language() {
    let check = |path: &'static str, src: &'static str, qn: &'static str| {
        move || -> Result<(), String> {
            let r = Repo::new(&[(path, src)]);
            let k = r.sym(qn).kind.clone();
            if k == E {
                Ok(())
            } else {
                Err(format!("{qn} is a {k}"))
            }
        }
    };
    let kotlin = check(
        "e/Color.kt",
        "enum class Color { RED, GREEN;\n    fun rgb(): Int = ordinal\n}\n",
        "e.Color.Color",
    );
    let swift = check(
        "e/Kind.swift",
        "enum Kind {\n    case a, b\n    func flip() -> Kind { self == .a ? .b : .a }\n}\n",
        "e.Kind.Kind",
    );
    each_case(&[("kotlin", &kotlin), ("swift", &swift)]);
}

/// `listOf<Int>(1).map { }` calls `map` on `listOf<Int>(1)`, and C#
/// `Pick<string>(x).Trim()` calls `Trim` on `Pick<string>(x)`: type arguments
/// are part of the receiver expression, not where it starts.
#[test]
fn a_receiver_with_type_arguments_is_the_whole_call() {
    let kotlin = || -> Result<(), String> {
        let r = Repo::new(&[("t/Use.kt", "fun use() {\n    listOf<Int>(1).map { it * 2 }\n}\n")]);
        let got: Vec<String> = r.calls_at("map", 2).iter().map(|c| c.receiver.clone()).collect();
        if got == ["listOf<Int>(1)"] {
            Ok(())
        } else {
            Err(format!("receiver of map: {got:?}"))
        }
    };
    let csharp = || -> Result<(), String> {
        let r = Repo::new(&[(
            "t/Use.cs",
            "class Use\n{\n    string Pick<T>(T x) => \"\";\n    string Run() => Pick<string>(null).Trim();\n}\n",
        )]);
        let got: Vec<String> = r.calls_at("Trim", 4).iter().map(|c| c.receiver.clone()).collect();
        if got == ["Pick<string>(null)"] {
            Ok(())
        } else {
            Err(format!("receiver of Trim: {got:?}"))
        }
    };
    each_case(&[("kotlin", &kotlin), ("csharp", &csharp)]);
}

/// Test detection: files under a test directory or named like a test, and
/// test methods there.
#[test]
fn test_files_and_their_methods_are_tests() {
    let cases: &[(&str, &str, &str)] = &[
        (
            "src/test/java/shop/OrderTest.java",
            "class OrderTest {\n    @Test\n    void totals() {}\n}\n",
            "src.test.java.shop.OrderTest.OrderTest.totals",
        ),
        (
            "shop/CartTest.kt",
            "class CartTest {\n    @Test\n    fun `totals the items`() {}\n}\n",
            "shop.CartTest.CartTest",
        ),
        (
            "src/test/scala/shop/CartSpec.scala",
            "class CartSpec {\n  def totals(): Unit = ()\n}\n",
            "src.test.scala.shop.CartSpec.CartSpec.totals",
        ),
        (
            "Shop.Tests/BillerTests.cs",
            "public class BillerTests\n{\n    [Fact]\n    public void Bills() { }\n}\n",
            "Shop.Tests.BillerTests.BillerTests.Bills",
        ),
        (
            "src/test/groovy/shop/CartTest.groovy",
            "class CartTest {\n    void totals() {}\n}\n",
            "src.test.groovy.shop.CartTest.CartTest.totals",
        ),
        (
            "Tests/ShopTests/CartTests.swift",
            "final class CartTests: XCTestCase {\n    func testTotal() {}\n}\n",
            "Tests.ShopTests.CartTests.CartTests.testTotal",
        ),
        (
            "ShopTests/CartTests.m",
            "@implementation CartTests\n- (void)testTotal {}\n@end\n",
            "ShopTests.CartTests.CartTests.testTotal",
        ),
        (
            "test/cart_test.dart",
            "void main() {\n  test('totals', () {});\n}\n",
            "test.cart_test.main",
        ),
    ];
    for (path, src, qn) in cases {
        let r = Repo::new(&[(path, src)]);
        assert!(r.sym(qn).is_test, "{qn}");
    }
}

/// A production name that merely starts with "test" (`Testimonial`,
/// `testimonials()`, `testable`) is no test; Python's rule is `test_` for
/// functions and `Test` for classes.
#[test]
fn names_that_only_start_with_test_are_not_tests() {
    let r = Repo::new(&[
        (
            "src/main/java/shop/Testimonial.java",
            "class Testimonial {\n    String testimonialText() { return \"\"; }\n}\n",
        ),
        ("shop/Review.kt", "fun testable(): Boolean = true\n"),
        ("Sources/Shop/Reviews.swift", "func testimonials() -> [String] { [] }\n"),
    ]);
    let marked: Vec<&str> = r
        .input
        .symbols
        .iter()
        .filter(|s| s.is_test)
        .map(|s| s.qualified_name.as_str())
        .collect();
    assert!(marked.is_empty(), "marked as tests: {marked:?}");
}

/// An empty file is one module symbol; a file with a syntax error still parses
/// (Tree-sitter recovers) and never panics, in every language.
#[test]
fn empty_and_broken_files_parse_without_panicking() {
    let broken: &[(&str, &str)] = &[
        (
            "x/Broken.java",
            "class Broken {\n    void ok() { fine(); }\n    void bad( {\n    void fine() {}\n",
        ),
        (
            "x/Broken.kt",
            "class Broken {\n    fun ok() = fine()\n    fun bad( =\n    fun fine() = 1\n",
        ),
        (
            "x/Broken.scala",
            "object Broken {\n  def ok(): Int = fine()\n  def bad( =\n  def fine(): Int = 1\n",
        ),
        (
            "x/Broken.cs",
            "class Broken {\n    void Ok() { Fine(); }\n    void Bad( {\n    void Fine() {}\n",
        ),
        (
            "x/Broken.groovy",
            "class Broken {\n    def ok() { fine() }\n    def bad( {\n    def fine() {}\n",
        ),
        (
            "x/Broken.swift",
            "struct Broken {\n    func ok() -> Int { fine() }\n    func bad( ->\n    func fine() -> Int { 1 }\n",
        ),
        (
            "x/Broken.m",
            "@implementation Broken\n- (void)ok { [self fine]; }\n- (void)bad: {\n- (void)fine {}\n",
        ),
        (
            "x/broken.dart",
            "class Broken {\n  int ok() => fine();\n  int bad( =>\n  int fine() => 1;\n",
        ),
    ];
    for (path, src) in broken {
        let p = parse(path, src);
        assert_eq!(p.symbols[0].kind, MOD, "{path}");
        assert!(p.symbols.len() > 1, "{path}: something is read");
        let empty_path = path.replace("Broken", "Empty").replace("broken", "empty");
        let e = parse(&empty_path, "");
        assert_eq!(e.symbols.len(), 1, "{empty_path}");
        assert_eq!((e.symbols[0].start_line, e.symbols[0].end_line), (1, 1), "{empty_path}");
        assert!(e.calls.is_empty(), "{empty_path}");
        // Bytes that are not UTF-8 are no panic either.
        let mut bad = src.as_bytes().to_vec();
        bad.extend_from_slice(&[0xff, 0xfe, b'\n']);
        lge_index::parse_file(path, "svc", &bad, "h").unwrap_or_else(|e| panic!("{path}: {e:#}"));
    }
}
