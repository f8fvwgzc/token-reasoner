//! Review of the Tree-sitter tags extraction for the scripting languages:
//! Ruby, PHP, Lua, Bash, R and Julia (and Perl, which was dropped).
//!
//! Each test parses code shaped like what real projects contain and asserts
//! what correct extraction gives: symbols with kinds, qualified names and
//! lines; call sites with their caller and receiver; and, through
//! `lge_resolve::resolve`, that a same-file bare call is `static`, a call into
//! another file is `inferred` or `unresolved`, and nothing matched by name
//! alone is ever `static`. A failing test is a defect report.

use lge_core::{fact, kind, method, EdgeRec, EdgeType, ParsedFile, ResolveInput, Resolved, SymbolRec};

const C: &str = kind::CLASS;
const F: &str = kind::FUNCTION;
const M: &str = kind::METHOD;
const I: &str = kind::INTERFACE;
const MOD: &str = kind::MODULE;
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
        .unwrap_or_else(|| panic!("{path}: not indexed"))
}

fn find<'a>(p: &'a ParsedFile, qn: &str) -> Option<&'a SymbolRec> {
    let found: Vec<&SymbolRec> = p.symbols.iter().filter(|s| s.qualified_name == qn).collect();
    match found.as_slice() {
        [] => None,
        [one] => Some(one),
        _ => panic!("{} symbols named {qn}", found.len()),
    }
}

fn sym<'a>(p: &'a ParsedFile, qn: &str) -> &'a SymbolRec {
    find(p, qn).unwrap_or_else(|| {
        let all: Vec<&str> = p.symbols.iter().map(|s| s.qualified_name.as_str()).collect();
        panic!("no symbol {qn}; have {all:?}")
    })
}

/// Asserts each (qualified name, kind, first line, last line).
fn assert_symbols(p: &ParsedFile, expected: &[(&str, &str, i64, i64)]) {
    for (qn, k, start, end) in expected {
        let s = sym(p, qn);
        assert_eq!((s.kind.as_str(), s.start_line, s.end_line), (*k, *start, *end), "{qn}");
    }
}

/// (name, receiver) of every call made by `caller`.
fn calls_of(p: &ParsedFile, caller: &str) -> Vec<(String, String)> {
    let id = &sym(p, caller).id;
    p.calls
        .iter()
        .filter(|c| c.caller_id == *id)
        .map(|c| (c.name.clone(), c.receiver.clone()))
        .collect()
}

fn assert_calls(p: &ParsedFile, caller: &str, expected: &[(&str, &str)]) {
    let got = calls_of(p, caller);
    for (name, recv) in expected {
        assert!(
            got.contains(&(name.to_string(), recv.to_string())),
            "{caller}: expected call {name:?} on {recv:?}; got {got:?}"
        );
    }
}

fn assert_no_call(p: &ParsedFile, caller: &str, name: &str) {
    let got = calls_of(p, caller);
    assert!(
        !got.iter().any(|(n, _)| n == name),
        "{caller}: {name:?} is not a call; got {got:?}"
    );
}

/// The symbol with this short name, when exactly one has it.
fn by_name<'a>(p: &'a ParsedFile, name: &str) -> &'a SymbolRec {
    let found: Vec<&SymbolRec> = p.symbols.iter().filter(|s| s.name == name).collect();
    match found.as_slice() {
        [one] => one,
        _ => panic!("{} symbols called {name}", found.len()),
    }
}

struct Repo {
    input: ResolveInput,
    res: Resolved,
}

fn resolve(files: &[&ParsedFile]) -> Repo {
    let mut input = ResolveInput::default();
    for p in files {
        input.files.extend(p.file.clone());
        input.symbols.extend(p.symbols.iter().cloned());
        input.calls.extend(p.calls.iter().cloned());
        input.imports.extend(p.imports.iter().cloned());
        input.facts.extend(p.facts.iter().cloned());
    }
    let res = lge_resolve::resolve(&input);
    let repo = Repo { input, res };
    repo.assert_provenance();
    repo
}

impl Repo {
    fn symbol(&self, qn: &str) -> &SymbolRec {
        let found: Vec<&SymbolRec> = self.input.symbols.iter().filter(|s| s.qualified_name == qn).collect();
        match found.as_slice() {
            [one] => one,
            _ => panic!("{} symbols named {qn}", found.len()),
        }
    }

    fn call(&self, from: &str, to: &str) -> Option<&EdgeRec> {
        let (f, t) = (&self.symbol(from).id, &self.symbol(to).id);
        self.res
            .edges
            .iter()
            .find(|e| e.edge_type == EdgeType::Calls && e.from == *f && e.to == *t)
    }

    fn edge(&self, from: &str, to: &str) -> &EdgeRec {
        self.call(from, to).unwrap_or_else(|| {
            let f = &self.symbol(from).id;
            let out: Vec<&str> = self
                .res
                .edges
                .iter()
                .filter(|e| e.from == *f)
                .map(|e| e.to.as_str())
                .collect();
            panic!("no call edge {from} -> {to}; edges from it go to {out:?}")
        })
    }

    fn assert_static(&self, from: &str, to: &str, confidence: f64) {
        let e = self.edge(from, to);
        assert_eq!(
            (e.method.as_str(), e.confidence),
            (method::STATIC, confidence),
            "{from} -> {to}"
        );
    }

    fn assert_inferred(&self, from: &str, to: &str) {
        let e = self.edge(from, to);
        assert_eq!(e.method, method::INFERRED, "{from} -> {to}: {e:?}");
    }

    fn assert_unresolved(&self, from: &str, name: &str) {
        let f = &self.symbol(from).id;
        let to = lge_core::unresolved_id(name);
        let e = self
            .res
            .edges
            .iter()
            .find(|e| e.edge_type == EdgeType::Calls && e.from == *f && e.to == to)
            .unwrap_or_else(|| panic!("no unresolved call {from} -> {name}"));
        assert_eq!((e.method.as_str(), e.confidence), (method::UNRESOLVED, 0.0));
    }

    /// Provenance rules: no import sites exist for these languages, so a
    /// static edge never crosses files; inferred is 0.40 to 0.60; unresolved
    /// points at a placeholder with confidence 0.
    fn assert_provenance(&self) {
        let file = |id: &str| {
            self.input
                .symbols
                .iter()
                .find(|s| s.id == id)
                .map(|s| s.file_path.as_str())
        };
        for e in &self.res.edges {
            match e.method.as_str() {
                method::STATIC => {
                    assert!(e.confidence >= 0.90, "{e:?}");
                    assert!(file(&e.to).is_some(), "static edge to a placeholder: {e:?}");
                    assert_eq!(file(&e.from), file(&e.to), "a static edge crosses files: {e:?}");
                }
                method::INFERRED => assert!((0.40..=0.60).contains(&e.confidence), "{e:?}"),
                method::UNRESOLVED => {
                    assert_eq!(e.confidence, 0.0, "{e:?}");
                    assert!(e.to.starts_with("unresolved:"), "{e:?}");
                }
                other => panic!("unknown method {other}: {e:?}"),
            }
        }
    }
}

fn rationale_owner<'a>(p: &'a ParsedFile, note: &str) -> Option<&'a str> {
    p.facts
        .iter()
        .find(|f| f.kind == fact::RATIONALE && f.key == note)
        .and_then(|f| p.symbols.iter().find(|s| s.id == f.symbol_id))
        .map(|s| s.qualified_name.as_str())
}

// ---------------------------------------------------------------------------
// Ruby
// ---------------------------------------------------------------------------

const RUBY_CART: &str = r#"# Shop models.
module Shop
  # A line in an order.
  class LineItem
    attr_reader :price

    def initialize(price)
      @price = price
    end
  end

  # The prices of the items a customer picked.
  class Cart
    # Builds a cart from rows.
    def self.build(rows)
      new(rows.map { |r| LineItem.new(r) })
    end

    def initialize(items)
      @items = items
    end

    # Adds up the item prices.
    def subtotal
      @items.sum(&:price)
    end

    def total
      # NOTE: tax applies after discounts
      discounted = subtotal - self.discount
      Tax.apply(discounted)
    end

    def valid?
      @items.all? { |item| item.price.positive? }
    end

    def discount
      coupon&.amount || 0
    end

    class << self
      def empty
        build([])
      end
    end

    private

    def coupon
      nil
    end
  end

  class Admin::Report
    def render
      Shop::Cart.build([]).total
    end
  end
end

# Prices the cart and formats the result.
def checkout(cart)
  total = cart.total
  print_receipt(cart)
  format_money(total)
end
"#;

#[test]
fn ruby_nested_modules_classes_and_singleton_methods() {
    let p = parse("app/models/cart.rb", RUBY_CART);
    assert_eq!(p.file.as_ref().unwrap().lang, lge_core::lang::RUBY);
    assert_symbols(
        &p,
        &[
            ("app.models.cart", MOD, 1, 67),
            ("app.models.cart.Shop", MOD, 2, 60),
            ("app.models.cart.Shop.LineItem", C, 4, 10),
            ("app.models.cart.Shop.LineItem.initialize", M, 7, 9),
            ("app.models.cart.Shop.Cart", C, 13, 53),
            ("app.models.cart.Shop.Cart.build", M, 15, 17),
            ("app.models.cart.Shop.Cart.initialize", M, 19, 21),
            ("app.models.cart.Shop.Cart.subtotal", M, 24, 26),
            ("app.models.cart.Shop.Cart.total", M, 28, 32),
            ("app.models.cart.Shop.Cart.valid?", M, 34, 36),
            ("app.models.cart.Shop.Cart.discount", M, 38, 40),
            ("app.models.cart.Shop.Cart.empty", M, 43, 45),
            ("app.models.cart.Shop.Cart.coupon", M, 50, 52),
            ("app.models.cart.Shop.Admin.Report", C, 55, 59),
            ("app.models.cart.Shop.Admin.Report.render", M, 56, 58),
            ("app.models.cart.checkout", F, 63, 67),
        ],
    );
    assert_eq!(
        sym(&p, "app.models.cart.Shop.Cart").doc,
        "The prices of the items a customer picked."
    );
    assert_eq!(
        sym(&p, "app.models.cart.Shop.Cart.build").doc,
        "Builds a cart from rows."
    );
    assert_eq!(
        sym(&p, "app.models.cart.Shop.Cart.build").signature,
        "def self.build(rows)"
    );
    assert_eq!(
        sym(&p, "app.models.cart.checkout").doc,
        "Prices the cart and formats the result."
    );
    assert!(p.symbols.iter().all(|s| !s.is_test));
    assert!(p.imports.is_empty());
}

#[test]
fn ruby_calls_have_caller_and_receiver() {
    let p = parse("app/models/cart.rb", RUBY_CART);
    let cart = "app.models.cart.Shop.Cart";
    assert_calls(
        &p,
        &format!("{cart}.build"),
        &[("new", ""), ("map", "rows"), ("new", "LineItem")],
    );
    assert_calls(&p, &format!("{cart}.subtotal"), &[("sum", "@items")]);
    assert_calls(
        &p,
        &format!("{cart}.total"),
        &[("subtotal", ""), ("discount", "self"), ("apply", "Tax")],
    );
    // Calls inside a block belong to the method; the block parameter is no call.
    assert_calls(
        &p,
        &format!("{cart}.valid?"),
        &[("all?", "@items"), ("price", "item"), ("positive?", "item.price")],
    );
    assert_no_call(&p, &format!("{cart}.valid?"), "item");
    // Safe navigation `coupon&.amount`.
    assert_calls(&p, &format!("{cart}.discount"), &[("coupon", ""), ("amount", "coupon")]);
    assert_calls(&p, &format!("{cart}.empty"), &[("build", "")]);
    assert_calls(
        &p,
        "app.models.cart.Shop.Admin.Report.render",
        &[("build", "Shop::Cart"), ("total", "Shop::Cart.build([])")],
    );
    // `total` is a local variable in checkout, not a call to a method `total`.
    assert_calls(
        &p,
        "app.models.cart.checkout",
        &[("total", "cart"), ("format_money", ""), ("print_receipt", "")],
    );
    assert!(!calls_of(&p, "app.models.cart.checkout").contains(&("total".into(), String::new())));
    assert_no_call(&p, &format!("{cart}.total"), "discounted");
}

#[test]
fn ruby_definition_names_are_not_calls() {
    let p = parse("app/models/cart.rb", RUBY_CART);
    for s in p.symbols.iter().filter(|s| s.kind == M || s.kind == F) {
        assert_no_call(&p, &s.qualified_name, &s.name);
    }
}

#[test]
fn ruby_constants_are_not_call_sites() {
    let src = r#"class CartError < StandardError
end

class Base
end

class Cart < Base
  LIMIT = 10

  def check(n)
    raise CartError, "too many" if n > LIMIT
    n
  end
end
"#;
    let p = parse("app/cart.rb", src);
    // A superclass is inheritance and a constant is a value: neither is a call.
    assert_no_call(&p, "app.cart.Cart", "Base");
    assert_no_call(&p, "app.cart.Cart.check", "LIMIT");
    assert_no_call(&p, "app.cart.Cart.check", "CartError");
    let repo = resolve(&[&p]);
    assert!(
        repo.call("app.cart.Cart", "app.cart.Base").is_none(),
        "`class Cart < Base` is stored as a CALLS edge"
    );
}

#[test]
fn ruby_rescue_and_top_level_variables_are_not_call_sites() {
    let src = r#"config = load_config
puts config

def fetch(url)
  get(url)
rescue Timeout::Error => error
  report(error)
end
"#;
    let p = parse("bin/fetch.rb", src);
    assert_calls(&p, "bin.fetch", &[("load_config", ""), ("puts", "")]);
    assert_no_call(&p, "bin.fetch", "config");
    assert_calls(&p, "bin.fetch.fetch", &[("get", ""), ("report", "")]);
    assert_no_call(&p, "bin.fetch.fetch", "error");
}

#[test]
fn ruby_resolution_follows_the_source_and_never_guesses() {
    let cart = parse("app/models/cart.rb", RUBY_CART);
    let tax = parse(
        "app/models/tax.rb",
        "class Tax\n  def self.apply(amount)\n    amount * 108 / 100\n  end\nend\n",
    );
    let money = parse(
        "app/helpers/money.rb",
        "def format_money(cents)\n  format(\"$%d\", cents)\nend\n",
    );
    let a = parse("app/a/receipt.rb", "def print_receipt(cart)\n  puts cart\nend\n");
    let b = parse("app/b/receipt.rb", "def print_receipt(cart)\n  p cart\nend\n");
    let repo = resolve(&[&cart, &tax, &money, &a, &b]);
    let c = "app.models.cart.Shop.Cart";
    // Implicit self, explicit self, and a singleton method calling a sibling.
    repo.assert_static(&format!("{c}.total"), &format!("{c}.subtotal"), 0.95);
    repo.assert_static(&format!("{c}.total"), &format!("{c}.discount"), 0.95);
    repo.assert_static(&format!("{c}.discount"), &format!("{c}.coupon"), 0.95);
    repo.assert_static(&format!("{c}.empty"), &format!("{c}.build"), 0.95);
    // Into other files: by name only.
    repo.assert_inferred(&format!("{c}.total"), "app.models.tax.Tax.apply");
    repo.assert_inferred("app.models.cart.checkout", "app.helpers.money.format_money");
    repo.assert_inferred("app.models.cart.checkout", &format!("{c}.total"));
    repo.assert_unresolved("app.models.cart.checkout", "print_receipt");
}

#[test]
fn ruby_tests_rationale_unicode_and_line_endings() {
    let spec = parse(
        "spec/models/cart_spec.rb",
        "RSpec.describe Cart do\n  it \"totals\" do\n    expect(Cart.new([1]).total).to eq(1)\n  end\nend\n",
    );
    assert!(spec.symbols[0].is_test);
    let unit = parse(
        "test/cart_test.rb",
        "class CartTest < Minitest::Test\n  def test_total\n    assert_equal 1, 1\n  end\nend\n",
    );
    assert!(sym(&unit, "test.cart_test.CartTest").is_test);
    assert!(sym(&unit, "test.cart_test.CartTest.test_total").is_test);

    let p = parse("app/models/cart.rb", RUBY_CART);
    assert_eq!(
        rationale_owner(&p, "NOTE: tax applies after discounts"),
        Some("app.models.cart.Shop.Cart.total")
    );

    let u = parse(
        "app/café.rb",
        "class Café\n  def größe\n    berechne(1)\n  end\n\n  def berechne(x)\n    x\n  end\nend\n",
    );
    assert_symbols(&u, &[("app.café.Café", C, 1, 9), ("app.café.Café.größe", M, 2, 4)]);
    let repo = resolve(&[&u]);
    repo.assert_static("app.café.Café.größe", "app.café.Café.berechne", 0.95);

    // `private def` is a definition passed to a call; it is still a method.
    let d = parse(
        "app/secret.rb",
        "class Vault\n  private def secret\n    reveal\n  end\n\n  def reveal\n    1\n  end\nend\n",
    );
    assert_symbols(&d, &[("app.secret.Vault.secret", M, 2, 4)]);
    resolve(&[&d]).assert_static("app.secret.Vault.secret", "app.secret.Vault.reveal", 0.95);

    let crlf = parse("app/crlf.rb", "class A\r\n  def f\r\n    g\r\n  end\r\nend\r\n");
    assert_symbols(&crlf, &[("app.crlf.A", C, 1, 5), ("app.crlf.A.f", M, 2, 4)]);
    assert_eq!(sym(&crlf, "app.crlf.A.f").signature, "def f");
}

// ---------------------------------------------------------------------------
// PHP
// ---------------------------------------------------------------------------

const PHP_ORDER: &str = r#"<?php
declare(strict_types=1);

namespace App\Orders;

use App\Support\Money;

interface Priced
{
    public function total(): int;
}

trait Audits
{
    protected function audit(string $msg): void
    {
        $this->record("[order] " . $msg);
    }

    private function record(string $line): void
    {
        error_log($line);
    }
}

abstract class Base
{
    abstract protected function rows(): array;

    public static function make(array $rows): static
    {
        return new static($rows);
    }
}

/**
 * An order a customer placed.
 */
final class Order extends Base implements Priced
{
    use Audits;

    public function __construct(private array $items, private ?Repo $repo = null)
    {
        parent::__construct();
    }

    protected function rows(): array
    {
        return $this->items;
    }

    public function total(): int
    {
        // NOTE: totals are in cents
        $sum = array_reduce($this->rows(), fn ($c, $i) => $c + price_of($i), 0);
        $this->audit("total");
        $this->repo?->persist($this);
        return self::rounded(static::withTax($sum));
    }

    private static function rounded(int $x): int
    {
        return $x;
    }

    public static function withTax(int $x): int
    {
        return Money::addTax($x);
    }
}

enum Status: string
{
    case Open = 'open';

    public function label(): string
    {
        return ucfirst($this->value);
    }
}

function price_of(array $item): int
{
    return $item['price'];
}

function place(array $rows): int
{
    $order = new Order($rows);
    $copy = new \App\Orders\Order($rows);
    $fmt = function (int $x) { return format_cents($x); };
    return \App\Support\helper($order->total()) + $copy->total();
}
"#;

const PHP_SUPPORT: &str = r#"<?php
namespace App\Support;

class Money
{
    public static function addTax(int $x): int
    {
        return intdiv($x * 108, 100);
    }
}

function format_cents(int $x): string
{
    return sprintf('%d', $x);
}
"#;

#[test]
fn php_classes_interfaces_enums_and_static_methods() {
    let p = parse("src/Orders/Order.php", PHP_ORDER);
    assert_eq!(p.file.as_ref().unwrap().lang, lge_core::lang::PHP);
    let o = "src.Orders.Order";
    assert_symbols(
        &p,
        &[
            (&format!("{o}.Priced"), I, 8, 11),
            (&format!("{o}.Audits.audit"), M, 15, 18),
            (&format!("{o}.Audits.record"), M, 20, 23),
            (&format!("{o}.Base"), C, 26, 34),
            (&format!("{o}.Base.rows"), M, 28, 28),
            (&format!("{o}.Base.make"), M, 30, 33),
            (&format!("{o}.Order"), C, 39, 71),
            (&format!("{o}.Order.__construct"), M, 43, 46),
            (&format!("{o}.Order.rows"), M, 48, 51),
            (&format!("{o}.Order.total"), M, 53, 60),
            (&format!("{o}.Order.rounded"), M, 62, 65),
            (&format!("{o}.Order.withTax"), M, 67, 70),
            (&format!("{o}.Status"), E, 73, 81),
            (&format!("{o}.Status.label"), M, 77, 80),
            (&format!("{o}.price_of"), F, 83, 86),
            (&format!("{o}.place"), F, 88, 94),
        ],
    );
    assert_eq!(sym(&p, &format!("{o}.Order")).doc, "An order a customer placed.");
    assert_eq!(
        sym(&p, &format!("{o}.Order.withTax")).signature,
        "public static function withTax(int $x): int"
    );
    assert_eq!(
        rationale_owner(&p, "NOTE: totals are in cents"),
        Some("src.Orders.Order.Order.total")
    );
}

#[test]
fn php_calls_have_caller_and_receiver() {
    let p = parse("src/Orders/Order.php", PHP_ORDER);
    let o = "src.Orders.Order";
    assert_calls(&p, &format!("{o}.Audits.audit"), &[("record", "self")]);
    assert_calls(&p, &format!("{o}.Order.__construct"), &[("__construct", "parent")]);
    // A call inside an arrow function belongs to the method around it.
    assert_calls(
        &p,
        &format!("{o}.Order.total"),
        &[
            ("array_reduce", ""),
            ("rows", "self"),
            ("price_of", ""),
            ("audit", "self"),
            ("rounded", "self"),
        ],
    );
    assert_calls(&p, &format!("{o}.Order.withTax"), &[("addTax", "Money")]);
    assert_calls(
        &p,
        &format!("{o}.place"),
        &[
            ("Order", ""),
            ("format_cents", ""),
            ("helper", ""),
            ("total", "$order"),
            ("total", "$copy"),
        ],
    );
}

#[test]
fn php_nullsafe_method_call_is_a_call() {
    let p = parse("src/Orders/Order.php", PHP_ORDER);
    assert_calls(&p, "src.Orders.Order.Order.total", &[("persist", "$this->repo")]);
}

#[test]
fn php_new_with_a_qualified_class_name_is_a_call() {
    let p = parse("src/Orders/Order.php", PHP_ORDER);
    let id = &sym(&p, "src.Orders.Order.place").id;
    let lines: Vec<i64> = p
        .calls
        .iter()
        .filter(|c| c.caller_id == *id && c.name == "Order")
        .map(|c| c.line)
        .collect();
    assert_eq!(lines, [90, 91], "`new Order` and `new \\App\\Orders\\Order`");
}

#[test]
fn php_static_receiver_is_the_own_class() {
    let p = parse("src/Orders/Order.php", PHP_ORDER);
    // `static::withTax()` names the class the method runs in, like `self::`.
    assert_calls(&p, "src.Orders.Order.Order.total", &[("withTax", "self")]);
    let repo = resolve(&[&p]);
    repo.assert_static("src.Orders.Order.Order.total", "src.Orders.Order.Order.withTax", 0.95);
}

#[test]
fn php_trait_methods_are_call_targets() {
    let p = parse("src/Orders/Order.php", PHP_ORDER);
    let s = parse("src/Support/Money.php", PHP_SUPPORT);
    let repo = resolve(&[&p, &s]);
    let o = "src.Orders.Order";
    // A trait holds code, not signatures: `$this->record()` inside the trait
    // names its sibling method.
    repo.assert_static(&format!("{o}.Audits.audit"), &format!("{o}.Audits.record"), 0.95);
    // `$this->audit()` in a class that uses the trait: the trait method is the
    // only `audit` in the repository.
    let e = repo.edge(&format!("{o}.Order.total"), &format!("{o}.Audits.audit"));
    assert_ne!(e.method, method::UNRESOLVED);
}

#[test]
fn php_resolution_follows_the_source_and_never_guesses() {
    let p = parse("src/Orders/Order.php", PHP_ORDER);
    let s = parse("src/Support/Money.php", PHP_SUPPORT);
    let repo = resolve(&[&p, &s]);
    let o = "src.Orders.Order";
    repo.assert_static(&format!("{o}.Order.total"), &format!("{o}.Order.rows"), 0.95);
    repo.assert_static(&format!("{o}.Order.total"), &format!("{o}.Order.rounded"), 0.95);
    repo.assert_static(&format!("{o}.Order.total"), &format!("{o}.price_of"), 0.95);
    repo.assert_static(&format!("{o}.place"), &format!("{o}.Order"), 0.95);
    repo.assert_inferred(&format!("{o}.Order.withTax"), "src.Support.Money.Money.addTax");
    repo.assert_inferred(&format!("{o}.place"), "src.Support.Money.format_cents");
    // The interface declares `total`, it does not define it.
    assert!(repo.call(&format!("{o}.place"), &format!("{o}.Priced.total")).is_none());
}

#[test]
fn php_bare_call_in_a_method_is_the_function_not_the_method() {
    let src = r#"<?php
class Exporter
{
    public function export(array $rows): string
    {
        return export($rows);
    }
}

function export(array $rows): string
{
    return json_encode($rows);
}
"#;
    let p = parse("src/Exporter.php", src);
    let repo = resolve(&[&p]);
    // PHP has no implicit `$this`: a bare `export()` is the global function.
    repo.assert_static("src.Exporter.Exporter.export", "src.Exporter.export", 0.95);
    assert!(repo
        .call("src.Exporter.Exporter.export", "src.Exporter.Exporter.export")
        .is_none());
}

#[test]
fn php_attributes_and_conditionally_declared_functions() {
    let src = r#"<?php
if (!function_exists('money')) {
    function money(int $x): string
    {
        return fmt($x);
    }
}

class OrderController
{
    #[Route('/orders', methods: ['GET'])]
    public function index(): array
    {
        return $this->load();
    }

    private function load(): array { return []; }
}
"#;
    let p = parse("src/helpers.php", src);
    assert_symbols(&p, &[("src.helpers.money", F, 3, 6)]);
    assert_eq!(sym(&p, "src.helpers.money").signature, "function money(int $x): string");
    let index = sym(&p, "src.helpers.OrderController.index");
    assert_eq!(index.kind, M);
    assert_eq!(index.end_line, 15);
    assert_eq!(index.signature, "public function index(): array");
    let repo = resolve(&[&p]);
    repo.assert_static(
        "src.helpers.OrderController.index",
        "src.helpers.OrderController.load",
        0.95,
    );
}

#[test]
fn php_hash_comments_are_rationale() {
    let src = "<?php\nfunction charge(int $cents): int\n{\n    # NOTE: amounts are in cents\n    return $cents;\n}\n";
    let p = parse("src/charge.php", src);
    assert_eq!(
        rationale_owner(&p, "NOTE: amounts are in cents"),
        Some("src.charge.charge")
    );
}

#[test]
fn php_test_classes_and_unicode_names() {
    let t = parse(
        "tests/OrderTest.php",
        "<?php\nclass OrderTest extends TestCase\n{\n    public function testTotal(): void\n    {\n        $this->assertSame(1, place([]));\n    }\n}\n",
    );
    assert!(sym(&t, "tests.OrderTest.OrderTest").is_test);
    assert!(sym(&t, "tests.OrderTest.OrderTest.testTotal").is_test);
    let u = parse(
        "src/größe.php",
        "<?php\nfunction größe(int $x): int { return $x; }\nfunction run(): int { return größe(2); }\n",
    );
    let repo = resolve(&[&u]);
    repo.assert_static("src.größe.run", "src.größe.größe", 0.95);
}

// ---------------------------------------------------------------------------
// Lua
// ---------------------------------------------------------------------------

const LUA_INVENTORY: &str = r#"-- Inventory helpers.
local M = {}

local Stock = {}
Stock.__index = Stock

--- Creates a stock record.
function Stock.new(count)
  return setmetatable({ count = count }, Stock)
end

function Stock:take(n)
  -- WHY: never go below zero
  self.count = math.max(0, self.count - n)
  return self:remaining()
end

function Stock:remaining()
  return self.count
end

local function check(x)
  assert(type(x) == "number")
  return x
end

local handlers = {
  add = function(a, b)
    return check(a) + b
  end,
  sub = function(a, b)
    return a - b
  end,
}

M.restock = function(stock, n)
  local s = Stock.new(stock.count + n)
  return s:take(check(n))
end

function M.report(stock)
  local parts = {}
  for i = 1, 3 do
    table.insert(parts, tostring(i))
  end
  local label = function() return "n=" .. describe(stock) end
  return a.b.c:render(label())
end

return M
"#;

#[test]
fn lua_methods_module_functions_and_locals() {
    let p = parse("lib/inventory.lua", LUA_INVENTORY);
    assert_eq!(p.file.as_ref().unwrap().lang, lge_core::lang::LUA);
    let l = "lib.inventory";
    assert_symbols(
        &p,
        &[
            (l, MOD, 1, 50),
            (&format!("{l}.Stock.new"), M, 8, 10),
            (&format!("{l}.Stock.take"), M, 12, 16),
            (&format!("{l}.Stock.remaining"), M, 18, 20),
            (&format!("{l}.check"), F, 22, 25),
            (&format!("{l}.M.restock"), M, 36, 39),
            (&format!("{l}.M.report"), M, 41, 48),
            (&format!("{l}.M.report.label"), F, 46, 46),
        ],
    );
    assert_eq!(sym(&p, &format!("{l}.Stock.new")).doc, "Creates a stock record.");
    assert_eq!(sym(&p, l).doc, "Inventory helpers.");
    assert_eq!(
        rationale_owner(&p, "WHY: never go below zero"),
        Some("lib.inventory.Stock.take")
    );
}

#[test]
fn lua_calls_have_caller_and_receiver() {
    let p = parse("lib/inventory.lua", LUA_INVENTORY);
    let l = "lib.inventory";
    assert_calls(
        &p,
        &format!("{l}.Stock.take"),
        &[("max", "math"), ("remaining", "self")],
    );
    assert_calls(
        &p,
        &format!("{l}.M.restock"),
        &[("new", "Stock"), ("take", "s"), ("check", "")],
    );
    assert_calls(
        &p,
        &format!("{l}.M.report"),
        &[
            ("insert", "table"),
            ("tostring", ""),
            ("render", "a.b.c"),
            ("label", ""),
        ],
    );
    // After a `..` concatenation the call is bare.
    assert_calls(&p, &format!("{l}.M.report.label"), &[("describe", "")]);
}

#[test]
fn lua_table_constructor_functions_have_their_own_range_and_calls() {
    let p = parse("lib/inventory.lua", LUA_INVENTORY);
    let add = by_name(&p, "add");
    let sub = by_name(&p, "sub");
    assert_eq!(
        (add.start_line, add.end_line),
        (28, 30),
        "add = function(a, b) ... end,"
    );
    assert_eq!(
        (sub.start_line, sub.end_line),
        (31, 33),
        "sub = function(a, b) ... end,"
    );
    // `check(a)` is written inside `add`.
    assert_calls(&p, &add.qualified_name, &[("check", "")]);
    assert_no_call(&p, &sub.qualified_name, "check");
}

#[test]
fn lua_resolution_follows_the_source_and_never_guesses() {
    let p = parse("lib/inventory.lua", LUA_INVENTORY);
    let a = parse("lib/a.lua", "local function describe(s)\n  return tostring(s)\nend\n");
    let b = parse("lib/b.lua", "function describe(s)\n  return 'x'\nend\n");
    let report = parse(
        "lib/report.lua",
        "local inv = require(\"inventory\")\nlocal function summary(s)\n  return inv.report(s)\nend\n",
    );
    let repo = resolve(&[&p, &a, &b, &report]);
    let l = "lib.inventory";
    repo.assert_static(&format!("{l}.M.restock"), &format!("{l}.check"), 0.95);
    repo.assert_static(&format!("{l}.M.report"), &format!("{l}.M.report.label"), 0.95);
    repo.assert_unresolved(&format!("{l}.M.report.label"), "describe");
    repo.assert_inferred("lib.report.summary", &format!("{l}.M.report"));
    let e = repo.edge(&format!("{l}.Stock.take"), &format!("{l}.Stock.remaining"));
    assert_ne!(e.method, method::UNRESOLVED);
}

// ---------------------------------------------------------------------------
// Bash
// ---------------------------------------------------------------------------

const BASH_DEPLOY: &str = r#"#!/usr/bin/env bash
# Deploys the service.
set -euo pipefail

# Prints a log line.
log() {
  echo "[deploy] $*" >&2
}

function die {
  log "fatal: $1"
  exit 1
}

function build_image() {
  # NOTE: tags must be lowercase
  local tag
  tag=$(git rev-parse --short HEAD | tr '[:upper:]' '[:lower:]')
  docker build -t "app:${tag}" . || die "build failed"
  echo "$tag"
}

deploy::push () {
  local tag="$1"
  if ! docker push "app:${tag}"; then
    die "push failed"
  fi
  for host in "${HOSTS[@]}"; do
    ssh "$host" "systemctl restart app" &
  done
  wait
}

main() {
  local tag
  tag="$(build_image)"
  deploy::push "$tag"
  [[ -n "${NOTIFY:-}" ]] && notify_slack "deployed $tag"
  $CUSTOM_HOOK "$tag"
  render_report
}

main "$@"
"#;

#[test]
fn bash_functions_in_every_form() {
    let p = parse("scripts/deploy.sh", BASH_DEPLOY);
    assert_eq!(p.file.as_ref().unwrap().lang, lge_core::lang::BASH);
    let d = "scripts.deploy";
    assert_symbols(
        &p,
        &[
            (d, MOD, 1, 43),
            (&format!("{d}.log"), F, 6, 8),
            (&format!("{d}.die"), F, 10, 13),
            (&format!("{d}.build_image"), F, 15, 21),
            (&format!("{d}.deploy::push"), F, 23, 32),
            (&format!("{d}.main"), F, 34, 41),
        ],
    );
    assert_eq!(sym(&p, d).doc, "Deploys the service.");
    assert_eq!(sym(&p, &format!("{d}.log")).doc, "Prints a log line.");
    assert_eq!(
        rationale_owner(&p, "NOTE: tags must be lowercase"),
        Some("scripts.deploy.build_image")
    );
    let sub = parse("scripts/sub.bash", "f() ( echo sub )\ng() { f; }\n");
    assert_symbols(&sub, &[("scripts.sub.f", F, 1, 1), ("scripts.sub.g", F, 2, 2)]);
}

#[test]
fn bash_commands_are_calls_of_the_enclosing_function() {
    let p = parse("scripts/deploy.sh", BASH_DEPLOY);
    let d = "scripts.deploy";
    assert_calls(&p, d, &[("set", ""), ("main", "")]);
    // Pipelines, command substitution, `||`, `if` conditions, loops, `&`.
    assert_calls(
        &p,
        &format!("{d}.build_image"),
        &[("git", ""), ("tr", ""), ("docker", ""), ("die", ""), ("echo", "")],
    );
    assert_calls(
        &p,
        &format!("{d}.deploy::push"),
        &[("docker", ""), ("die", ""), ("ssh", ""), ("wait", "")],
    );
    assert_calls(
        &p,
        &format!("{d}.main"),
        &[
            ("build_image", ""),
            ("deploy::push", ""),
            ("notify_slack", ""),
            ("render_report", ""),
        ],
    );
    // A command run from a variable names nothing.
    assert!(p.calls.iter().all(|c| !c.name.contains("CUSTOM_HOOK")));
}

#[test]
fn bash_resolution_follows_the_source_and_never_guesses() {
    let p = parse("scripts/deploy.sh", BASH_DEPLOY);
    let slack = parse(
        "scripts/lib/slack.sh",
        "notify_slack() {\n  curl -s \"$URL\" -d \"$1\"\n}\n",
    );
    let a = parse("scripts/a.sh", "render_report() { echo a; }\n");
    let b = parse("scripts/b.sh", "render_report() { echo b; }\n");
    let repo = resolve(&[&p, &slack, &a, &b]);
    let d = "scripts.deploy";
    repo.assert_static(&format!("{d}.main"), &format!("{d}.build_image"), 0.95);
    repo.assert_static(&format!("{d}.main"), &format!("{d}.deploy::push"), 0.95);
    repo.assert_static(&format!("{d}.die"), &format!("{d}.log"), 0.95);
    repo.assert_static(d, &format!("{d}.main"), 0.95);
    repo.assert_inferred(&format!("{d}.main"), "scripts.lib.slack.notify_slack");
    repo.assert_unresolved(&format!("{d}.main"), "render_report");
    let t = parse("test/deploy_test.sh", "test_build() {\n  build_image\n}\n");
    assert!(sym(&t, "test.deploy_test.test_build").is_test);
}

// ---------------------------------------------------------------------------
// R
// ---------------------------------------------------------------------------

const R_MODEL: &str = r#"# Model fitting helpers.

#' Scale a numeric vector.
#'
#' @param x A numeric vector.
scale_vec <- function(x) {
  (x - mean(x)) / stats::sd(x)
}

fit_model = function(df, formula) {
  # NOTE: lm drops NA rows silently
  df <- clean(df)
  fit <- lm(formula, data = df)
  df |> summarise_fit()
  sapply(df, function(col) scale_vec(col))
  helper <- function(y) y * 2
  helper(fit$df.residual)
}

clean <- \(df) na.omit(df)

Account <- R6::R6Class("Account",
  public = list(
    balance = 0,
    deposit = function(x) {
      self$balance <- self$balance + x
      self$log_tx(x)
      invisible(self)
    },
    log_tx = function(x) {
      message("tx ", x)
    }
  )
)

Person <- setRefClass("Person",
  fields = list(name = "character"),
  methods = list(
    greet = function() {
      cat("Hi", name)
      shout()
      .self$whisper()
    },
    shout = function() {
      toupper(name)
    },
    whisper = function() {
      tolower(name)
    }
  )
)

acc <- Account$new()
acc$deposit(10)$log_tx(1)
"#;

#[test]
fn r_functions_and_reference_classes() {
    let p = parse("R/model.R", R_MODEL);
    assert_eq!(p.file.as_ref().unwrap().lang, lge_core::lang::R);
    let m = "R.model";
    assert_symbols(
        &p,
        &[
            (m, MOD, 1, 54),
            (&format!("{m}.scale_vec"), F, 6, 8),
            (&format!("{m}.fit_model"), F, 10, 18),
            (&format!("{m}.fit_model.helper"), F, 16, 16),
            (&format!("{m}.clean"), F, 20, 20),
            (&format!("{m}.Person"), C, 36, 51),
            (&format!("{m}.Person.greet"), M, 39, 43),
            (&format!("{m}.Person.shout"), M, 44, 46),
            (&format!("{m}.Person.whisper"), M, 47, 49),
        ],
    );
    assert_eq!(sym(&p, &format!("{m}.scale_vec")).doc, "Scale a numeric vector.");
    assert_eq!(sym(&p, &format!("{m}.scale_vec")).signature, "scale_vec <- function(x)");
    assert_eq!(
        rationale_owner(&p, "NOTE: lm drops NA rows silently"),
        Some("R.model.fit_model")
    );
    // Both extensions are R.
    assert_eq!(lge_index::lang_of("R/util.r"), Some(lge_core::lang::R));
}

#[test]
fn r_calls_have_caller_and_receiver() {
    let p = parse("R/model.R", R_MODEL);
    let m = "R.model";
    assert_calls(&p, &format!("{m}.scale_vec"), &[("mean", ""), ("sd", "stats")]);
    // The native pipe and a call inside an anonymous function.
    assert_calls(
        &p,
        &format!("{m}.fit_model"),
        &[
            ("clean", ""),
            ("lm", ""),
            ("summarise_fit", ""),
            ("sapply", ""),
            ("scale_vec", ""),
            ("helper", ""),
        ],
    );
    assert_calls(
        &p,
        &format!("{m}.Person.greet"),
        &[("cat", ""), ("shout", ""), ("whisper", "self")],
    );
    assert_calls(
        &p,
        m,
        &[("new", "Account"), ("deposit", "acc"), ("log_tx", "acc$deposit(10)")],
    );
}

#[test]
fn r_r6_classes_and_methods_are_extracted() {
    let p = parse("R/model.R", R_MODEL);
    let m = "R.model";
    assert_symbols(
        &p,
        &[
            (&format!("{m}.Account"), C, 22, 34),
            (&format!("{m}.Account.deposit"), M, 25, 29),
            (&format!("{m}.Account.log_tx"), M, 30, 32),
        ],
    );
    assert_calls(&p, &format!("{m}.Account.deposit"), &[("log_tx", "self")]);
    let repo = resolve(&[&p]);
    repo.assert_static(&format!("{m}.Account.deposit"), &format!("{m}.Account.log_tx"), 0.95);
}

#[test]
fn r_reference_class_method_calls_a_sibling_by_name() {
    let p = parse("R/model.R", R_MODEL);
    let repo = resolve(&[&p]);
    let m = "R.model";
    repo.assert_static(&format!("{m}.Person.greet"), &format!("{m}.Person.whisper"), 0.95);
    // In a reference class method a sibling method is called by its name alone.
    repo.assert_static(&format!("{m}.Person.greet"), &format!("{m}.Person.shout"), 0.95);
}

#[test]
fn r_resolution_follows_the_source_and_never_guesses() {
    let p = parse("R/model.R", R_MODEL);
    let money = parse("R/money.r", "format_money <- function(x) {\n  sprintf(\"%d\", x)\n}\n");
    let report = parse("R/report.R", "report <- function(m) {\n  format_money(m)\n}\n");
    let repo = resolve(&[&p, &money, &report]);
    let m = "R.model";
    repo.assert_static(&format!("{m}.fit_model"), &format!("{m}.clean"), 0.95);
    repo.assert_static(&format!("{m}.fit_model"), &format!("{m}.scale_vec"), 0.95);
    repo.assert_static(&format!("{m}.fit_model"), &format!("{m}.fit_model.helper"), 0.95);
    repo.assert_inferred("R.report.report", "R.money.format_money");
    let t = parse(
        "tests/testthat/test-model.R",
        "test_that(\"scale_vec centres\", {\n  expect_equal(mean(scale_vec(c(1, 2, 3))), 0)\n})\n",
    );
    assert!(t.symbols[0].is_test);
}

#[test]
fn assigned_function_signatures_keep_their_parameters() {
    let r = parse("R/model.R", R_MODEL);
    assert_eq!(
        sym(&r, "R.model.fit_model").signature,
        "fit_model = function(df, formula)"
    );
    let lua = parse("lib/inventory.lua", LUA_INVENTORY);
    assert_eq!(
        sym(&lua, "lib.inventory.M.restock").signature,
        "M.restock = function(stock, n)"
    );
}

#[test]
fn r_reference_class_signature_is_its_header() {
    let p = parse("R/model.R", R_MODEL);
    let sig = &sym(&p, "R.model.Person").signature;
    assert!(
        !sig.contains("function") && !sig.contains("shout"),
        "the class signature runs into its method bodies: {sig:?}"
    );
}

// ---------------------------------------------------------------------------
// Julia
// ---------------------------------------------------------------------------

const JULIA_GEOMETRY: &str = r#"# Geometry kernels.
module Geometry

export Point, norm2

abstract type Shape end

"""
    Point{T}

A point in the plane.
"""
struct Point{T<:Real} <: Shape
    x::T
    y::T
end

mutable struct Counter
    n::Int
end

struct Circle <: Shape
    r::Float64
end

"""
    norm2(p)

Squared distance from the origin.
"""
function norm2(p::Point)
    return p.x^2 + p.y^2
end

area(c::Circle) = π * c.r^2
area(p::Point) = zero_area(p)

function scale(p::Point{T}, k::T) where {T}
    # NOTE: keeps the element type
    return Point(p.x * k, p.y * k)
end

function total(xs::Vector)::Float64
    s = sum(norm2.(xs))
    foreach(xs) do x
        bump!(COUNTER)
    end
    return LinearAlgebra.norm(s) + Base.sum(area, xs)
end

bump!(c::Counter) = (c.n += 1)

σ(x) = 1 / (1 + exp(-x))

zero_area(p) = 0.0

module Inner
inner_helper() = 1
end

end
"#;

#[test]
fn julia_modules_structs_and_functions() {
    let p = parse("src/Geometry.jl", JULIA_GEOMETRY);
    assert_eq!(p.file.as_ref().unwrap().lang, lge_core::lang::JULIA);
    let g = "src.Geometry.Geometry";
    assert_symbols(
        &p,
        &[
            (g, MOD, 2, 61),
            (&format!("{g}.Shape"), I, 6, 6),
            (&format!("{g}.Counter"), C, 18, 20),
            (&format!("{g}.Circle"), C, 22, 24),
            (&format!("{g}.norm2"), F, 31, 33),
            (&format!("{g}.area"), F, 35, 35),
            (&format!("{g}.scale"), F, 38, 41),
            (&format!("{g}.total"), F, 43, 49),
            (&format!("{g}.bump!"), F, 51, 51),
            (&format!("{g}.σ"), F, 53, 53),
            (&format!("{g}.zero_area"), F, 55, 55),
            (&format!("{g}.Inner"), MOD, 57, 59),
            (&format!("{g}.Inner.inner_helper"), F, 58, 58),
        ],
    );
    assert_eq!(
        rationale_owner(&p, "NOTE: keeps the element type"),
        Some("src.Geometry.Geometry.scale")
    );
    let m = parse(
        "src/Macros.jl",
        "macro twice(ex)\n    return :($(esc(ex)); $(esc(ex)))\nend\n",
    );
    assert_symbols(&m, &[("src.Macros.twice", F, 1, 3)]);
}

#[test]
fn julia_where_clause_is_part_of_the_signature() {
    let p = parse("src/Geometry.jl", JULIA_GEOMETRY);
    // `{T}` after `where` is a type parameter list, not a body.
    assert_eq!(
        sym(&p, "src.Geometry.Geometry.scale").signature,
        "function scale(p::Point{T}, k::T) where {T}"
    );
}

#[test]
fn julia_parametric_structs_are_classes() {
    let p = parse("src/Geometry.jl", JULIA_GEOMETRY);
    assert_symbols(&p, &[("src.Geometry.Geometry.Point", C, 13, 16)]);
    let repo = resolve(&[&p]);
    repo.assert_static("src.Geometry.Geometry.scale", "src.Geometry.Geometry.Point", 0.95);
}

#[test]
fn julia_triple_quoted_docstrings_give_the_doc_line() {
    let p = parse("src/Geometry.jl", JULIA_GEOMETRY);
    let doc = &sym(&p, "src.Geometry.Geometry.norm2").doc;
    assert!(!doc.is_empty(), "the docstring above norm2 gives no doc");
}

#[test]
fn julia_calls_have_caller_and_receiver() {
    let p = parse("src/Geometry.jl", JULIA_GEOMETRY);
    let g = "src.Geometry.Geometry";
    // A call inside a `do` block belongs to the function around it.
    assert_calls(
        &p,
        &format!("{g}.total"),
        &[
            ("sum", ""),
            ("foreach", ""),
            ("bump!", ""),
            ("norm", "LinearAlgebra"),
            ("sum", "Base"),
        ],
    );
    // The second method of `area` is still `area`.
    assert_calls(&p, &format!("{g}.area"), &[("zero_area", "")]);
    assert_calls(&p, &format!("{g}.scale"), &[("Point", "")]);
    // The call written in a signature is the definition, not a call site.
    for name in ["norm2", "scale", "total", "area", "zero_area", "σ", "bump!"] {
        assert_no_call(&p, g, name);
        assert_no_call(&p, &format!("{g}.{name}"), name);
    }
}

#[test]
fn julia_broadcast_calls_are_calls() {
    let p = parse("src/Geometry.jl", JULIA_GEOMETRY);
    assert_calls(&p, "src.Geometry.Geometry.total", &[("norm2", "")]);
}

#[test]
fn julia_short_form_where_and_return_typed_functions_are_definitions() {
    let src = r#"module Generic
first_of(xs::Vector{T}) where {T} = xs[1]
count_of(xs)::Int = length(xs)
function clamp01(x::T)::T where {T<:Real}
    return clamp(x, zero(T), one(T))
end
run_all(xs) = (first_of(xs), count_of(xs), clamp01(xs[1]))
end
"#;
    let p = parse("src/Generic.jl", src);
    let g = "src.Generic.Generic";
    for name in ["first_of", "count_of", "clamp01"] {
        assert_no_call(&p, g, name);
    }
    assert_symbols(
        &p,
        &[
            (&format!("{g}.first_of"), F, 2, 2),
            (&format!("{g}.count_of"), F, 3, 3),
            (&format!("{g}.clamp01"), F, 4, 6),
            (&format!("{g}.run_all"), F, 7, 7),
        ],
    );
    let repo = resolve(&[&p]);
    repo.assert_static(&format!("{g}.run_all"), &format!("{g}.clamp01"), 0.95);
}

#[test]
fn julia_resolution_follows_the_source_and_never_guesses() {
    let p = parse("src/Geometry.jl", JULIA_GEOMETRY);
    let report = parse("src/Report.jl", "report(xs) = norm2(xs[1]) + describe(xs)\n");
    let a = parse("src/A.jl", "describe(x) = string(x)\n");
    let b = parse("src/B.jl", "function describe(x)\n    repr(x)\nend\n");
    let repo = resolve(&[&p, &report, &a, &b]);
    let g = "src.Geometry.Geometry";
    repo.assert_static(&format!("{g}.area"), &format!("{g}.zero_area"), 0.95);
    repo.assert_static(&format!("{g}.total"), &format!("{g}.bump!"), 0.95);
    repo.assert_inferred("src.Report.report", &format!("{g}.norm2"));
    repo.assert_unresolved("src.Report.report", "describe");
    let t = parse(
        "test/runtests.jl",
        "using Test\n@testset \"geometry\" begin\n    @test norm2(Point(3, 4)) == 25\nend\n",
    );
    assert!(t.symbols[0].is_test);
}

// ---------------------------------------------------------------------------
// Every language of the group
// ---------------------------------------------------------------------------

#[test]
fn perl_is_indexed() {
    // ts-parser-perl 2.0.6 (the tree-sitter-perl organisation's crate) depends
    // only on tree-sitter-language, has ABI 15, and loads and tags with
    // tree-sitter 0.27.2.
    for path in ["lib/Shop/Cart.pm", "script/run.pl"] {
        assert_eq!(lge_index::lang_of(path), Some("perl"), "{path}");
    }
    let src = "package Shop::Cart;\nuse strict;\n\nsub total {\n    my $self = shift;\n    return apply_tax($self->subtotal());\n}\n\nsub apply_tax { my $x = shift; return $x * 1.08 }\n\n1;\n";
    let p = parse("lib/Shop/Cart.pm", src);
    let total = by_name(&p, "total");
    assert_eq!((total.start_line, total.end_line), (4, 7));
    assert_calls(&p, &total.qualified_name, &[("apply_tax", ""), ("subtotal", "self")]);
    let repo = resolve(&[&p]);
    repo.assert_static(&total.qualified_name, &by_name(&p, "apply_tax").qualified_name, 0.95);
}

#[test]
fn test_prefixed_words_are_not_tests() {
    let ruby = parse(
        "app/models/review.rb",
        "class Review\n  def testimonials\n    []\n  end\n\n  def test_total\n    1\n  end\nend\n",
    );
    assert!(sym(&ruby, "app.models.review.Review.test_total").is_test);
    assert!(
        !sym(&ruby, "app.models.review.Review.testimonials").is_test,
        "`testimonials` is not a test"
    );
    let php = parse(
        "src/Testimonial.php",
        "<?php\nclass Testimonial\n{\n    public function quote(): string { return ''; }\n}\n",
    );
    assert!(
        !sym(&php, "src.Testimonial.Testimonial").is_test,
        "`Testimonial` is not a test"
    );
}

#[test]
fn broken_and_empty_files_never_panic() {
    let broken: [(&str, &[u8]); 6] = [
        ("app/broken.rb", b"class Broken\n  def ok\n    helper(\n  end\n  def other\n"),
        ("src/broken.php", b"<?php\nclass Ok\n{\n    public function a(): int { return b(); }\n    public function broken( {\n}\nfunction b() { return 1; }\n"),
        ("lib/broken.lua", b"local function ok()\n  return 1\nend\nfunction broken(\n  x = \nend\n"),
        ("scripts/broken.sh", b"ok() { echo ok; }\nbroken() {\n  if [ x; then\n}\nafter() { ok; }\n"),
        ("R/broken.R", b"f <- function(x {\n  g(x\n}\nh <- function() 1\n"),
        ("src/broken.jl", b"function f(x\n    g(x)\nstruct\nend end end\n"),
    ];
    for (path, src) in broken {
        let p = parse_bytes(path, src);
        assert_eq!(p.symbols[0].kind, MOD, "{path}");
        for c in &p.calls {
            assert!(p.symbols.iter().any(|s| s.id == c.caller_id), "{path}: {c:?}");
        }
    }
    for path in [
        "app/empty.rb",
        "src/empty.php",
        "lib/empty.lua",
        "scripts/empty.sh",
        "R/empty.R",
        "src/empty.jl",
    ] {
        let p = parse(path, "");
        assert_eq!(p.symbols.len(), 1, "{path}");
        assert_eq!((p.symbols[0].start_line, p.symbols[0].end_line), (1, 1), "{path}");
        assert!(p.calls.is_empty() && p.facts.is_empty(), "{path}");
    }
    // Bytes that are not UTF-8 inside a definition.
    for (path, src) in [
        ("app/bytes.rb", &b"def f\n  g(\"\xff\xfe\")\nend\n"[..]),
        ("lib/bytes.lua", &b"local function f()\n  return g(\"\xff\")\nend\n"[..]),
        ("src/bytes.jl", &b"f() = g(\"\xc3\")\n"[..]),
    ] {
        let p = parse_bytes(path, src);
        assert_eq!(p.symbols.len(), 2, "{path}: {:?}", p.symbols);
        assert_eq!(p.calls.len(), 1, "{path}: {:?}", p.calls);
    }
}

#[test]
fn a_mixed_repository_never_matches_by_name_as_static() {
    let files = [
        parse("app/models/cart.rb", RUBY_CART),
        parse("app/helpers/money.rb", "def format_money(cents)\n  cents.to_s\nend\n"),
        parse("src/Orders/Order.php", PHP_ORDER),
        parse("src/Support/Money.php", PHP_SUPPORT),
        parse("lib/inventory.lua", LUA_INVENTORY),
        parse("scripts/deploy.sh", BASH_DEPLOY),
        parse("R/model.R", R_MODEL),
        parse("src/Geometry.jl", JULIA_GEOMETRY),
    ];
    let refs: Vec<&ParsedFile> = files.iter().collect();
    // `resolve` checks the provenance rules on every edge.
    let repo = resolve(&refs);
    assert!(repo.res.stats.get(EdgeType::Calls, method::STATIC) > 0);
    // A Ruby call is never answered by a PHP or Lua definition of the same name.
    let ruby = |id: &str| {
        repo.input
            .symbols
            .iter()
            .find(|s| s.id == id)
            .is_some_and(|s| s.file_path.ends_with(".rb"))
    };
    for e in repo.res.edges.iter().filter(|e| e.edge_type == EdgeType::Calls) {
        if ruby(&e.from) && !e.to.starts_with("unresolved:") {
            assert!(ruby(&e.to), "{e:?}");
        }
    }
}
