//! Functional review of the tags-query extraction for Elixir, Erlang, Haskell,
//! OCaml, Elm, Nix, Solidity (and Gleam, F#, Clojure): realistic code that a
//! lazy extractor gets wrong, checked symbol by symbol and call by call, then
//! resolved with `lge_resolve::resolve` to check provenance.
//!
//! Every assertion states what correct extraction is. A failing test is a
//! defect report.

use lge_core::{kind, method, EdgeRec, EdgeType, ParsedFile, ResolveInput, Resolved, SymbolRec};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse(path: &str, src: &str) -> ParsedFile {
    lge_index::parse_file(path, "svc", src.as_bytes(), "h")
        .unwrap_or_else(|e| panic!("{path}: {e:#}"))
        .unwrap_or_else(|| panic!("{path}: not a supported language"))
}

/// The one symbol of `p` with this qualified name.
fn sym<'a>(p: &'a ParsedFile, qn: &str) -> &'a SymbolRec {
    let found: Vec<&SymbolRec> = p.symbols.iter().filter(|s| s.qualified_name == qn).collect();
    match found.as_slice() {
        [one] => one,
        _ => panic!(
            "{} symbols named {qn}; symbols: {:?}",
            found.len(),
            p.symbols
                .iter()
                .map(|s| (&s.qualified_name, &s.kind, s.start_line, s.end_line))
                .collect::<Vec<_>>()
        ),
    }
}

/// (kind, first line, last line) of a symbol.
fn shape(p: &ParsedFile, qn: &str) -> (String, i64, i64) {
    let s = sym(p, qn);
    (s.kind.clone(), s.start_line, s.end_line)
}

fn k(kind: &str, start: i64, end: i64) -> (String, i64, i64) {
    (kind.to_string(), start, end)
}

/// (name, receiver) of every call site whose caller is `caller_qn`.
fn calls_of(p: &ParsedFile, caller_qn: &str) -> Vec<(String, String)> {
    let id = &sym(p, caller_qn).id;
    p.calls
        .iter()
        .filter(|c| c.caller_id == *id)
        .map(|c| (c.name.clone(), c.receiver.clone()))
        .collect()
}

fn has_call(p: &ParsedFile, caller_qn: &str, name: &str, receiver: &str) -> bool {
    calls_of(p, caller_qn).contains(&(name.to_string(), receiver.to_string()))
}

#[track_caller]
fn assert_call(p: &ParsedFile, caller_qn: &str, name: &str, receiver: &str) {
    assert!(
        has_call(p, caller_qn, name, receiver),
        "expected call {receiver:?}.{name} in {caller_qn}; calls: {:?}",
        calls_of(p, caller_qn)
    );
}

/// Every call site has a caller that is a symbol of its file, and the file has
/// its module symbol first.
fn assert_well_formed(p: &ParsedFile) {
    let file = p.file.as_ref().expect("file record");
    let module = p.symbols.first().expect("module symbol");
    assert_eq!(module.kind, kind::MODULE, "{}", file.path);
    for c in &p.calls {
        assert!(p.symbols.iter().any(|s| s.id == c.caller_id), "{}: {c:?}", file.path);
        assert_eq!(c.file_path, file.path);
    }
    for s in &p.symbols {
        assert!(s.start_line >= 1 && s.start_line <= s.end_line, "{}: {s:?}", file.path);
    }
}

struct World {
    input: ResolveInput,
    res: Resolved,
}

fn resolve(files: &[&ParsedFile]) -> World {
    let mut input = ResolveInput::default();
    for p in files {
        input.files.extend(p.file.clone());
        input.symbols.extend(p.symbols.iter().cloned());
        input.calls.extend(p.calls.iter().cloned());
        input.imports.extend(p.imports.iter().cloned());
        input.facts.extend(p.facts.iter().cloned());
    }
    let res = lge_resolve::resolve(&input);
    World { input, res }
}

impl World {
    fn sym(&self, qn: &str) -> &SymbolRec {
        let found: Vec<&SymbolRec> = self.input.symbols.iter().filter(|s| s.qualified_name == qn).collect();
        match found.as_slice() {
            [one] => one,
            _ => panic!("{} symbols named {qn}", found.len()),
        }
    }

    fn qn_of(&self, id: &str) -> String {
        self.input
            .symbols
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.qualified_name.clone())
            .unwrap_or_else(|| id.to_string())
    }

    /// (callee, method, confidence) of every CALLS edge from `from_qn`.
    fn calls_from(&self, from_qn: &str) -> Vec<(String, String, f64)> {
        let id = &self.sym(from_qn).id;
        self.res
            .edges
            .iter()
            .filter(|e| e.edge_type == EdgeType::Calls && e.from == *id)
            .map(|e| (self.qn_of(&e.to), e.method.clone(), e.confidence))
            .collect()
    }

    /// The CALLS edge from one symbol to another, if any.
    fn call(&self, from_qn: &str, to_qn: &str) -> Option<&EdgeRec> {
        let (from, to) = (&self.sym(from_qn).id, &self.sym(to_qn).id);
        self.res
            .edges
            .iter()
            .find(|e| e.edge_type == EdgeType::Calls && e.from == *from && e.to == *to)
    }

    #[track_caller]
    fn expect_call(&self, from_qn: &str, to_qn: &str, method: &str, confidence: f64) {
        let e = self
            .call(from_qn, to_qn)
            .unwrap_or_else(|| panic!("no call {from_qn} -> {to_qn}; calls: {:?}", self.calls_from(from_qn)));
        assert_eq!(
            (e.method.as_str(), e.confidence),
            (method, confidence),
            "{from_qn} -> {to_qn}"
        );
    }

    /// The edge from `from_qn` to the placeholder of `name`.
    #[track_caller]
    fn expect_unresolved(&self, from_qn: &str, name: &str) {
        let from = &self.sym(from_qn).id;
        let to = format!("unresolved:{name}");
        let e = self
            .res
            .edges
            .iter()
            .find(|e| e.edge_type == EdgeType::Calls && e.from == *from && e.to == to)
            .unwrap_or_else(|| {
                panic!(
                    "no unresolved {name} from {from_qn}; calls: {:?}",
                    self.calls_from(from_qn)
                )
            });
        assert_eq!((e.method.as_str(), e.confidence), (method::UNRESOLVED, 0.0));
    }

    /// These languages have no import sites: a static edge never leaves its file,
    /// and every CALLS edge carries one of the three provenances.
    fn assert_provenance(&self) {
        let file = |id: &str| {
            self.input
                .symbols
                .iter()
                .find(|s| s.id == id)
                .map(|s| s.file_path.clone())
        };
        for e in &self.res.edges {
            assert!(
                [method::STATIC, method::INFERRED, method::UNRESOLVED].contains(&e.method.as_str()),
                "{e:?}"
            );
            if e.method == method::STATIC {
                assert_eq!(file(&e.from), file(&e.to), "static edge across files: {e:?}");
                assert!(e.confidence >= 0.9, "{e:?}");
            }
            if e.method == method::INFERRED {
                assert!((0.4..=0.6).contains(&e.confidence), "{e:?}");
            }
        }
    }
}

fn rationale(p: &ParsedFile) -> Vec<(String, String)> {
    p.facts
        .iter()
        .filter(|f| f.kind == lge_core::fact::RATIONALE)
        .map(|f| {
            let owner = p
                .symbols
                .iter()
                .find(|s| s.id == f.symbol_id)
                .map(|s| s.qualified_name.clone())
                .unwrap_or_default();
            (owner, f.key.clone())
        })
        .collect()
}

const F: &str = kind::FUNCTION;
const M: &str = kind::METHOD;
const C: &str = kind::CLASS;
const I: &str = kind::INTERFACE;
const T: &str = kind::TYPE;
const E: &str = kind::ENUM;
const MOD: &str = kind::MODULE;

// ---------------------------------------------------------------------------
// Elixir
// ---------------------------------------------------------------------------

const ELIXIR_CART: &str = r#"defmodule Shop.Cart do
  @moduledoc "Prices a cart."

  defmodule Item do
    defstruct [:price]
    def price(%Item{price: p}), do: p
  end

  @doc "Adds up the item prices."
  def subtotal(items) do
    items |> Enum.map(fn i -> i.price end) |> Enum.sum()
  end

  defp normalize(items), do: Enum.reject(items, &is_nil/1)

  def total(items) do
    # NOTE: tax is applied after normalize
    items
    |> normalize()
    |> subtotal
    |> apply_tax()
    |> Shop.Money.format_money()
  end

  def apply_tax(amount), do: div(amount * 108, 100)

  def each_price(items) do
    Enum.each(items, fn i -> log_item(i) end)
    report(items)
    audit_log(items)
  end

  def valid?(items), do: items != []
  def check!(items), do: valid?(items) || raise("empty")

  defp log_item(i), do: IO.inspect(i)
end
"#;

const ELIXIR_MONEY: &str = r#"defmodule Shop.Money do
  def format_money(cents), do: "$#{cents}"
  def report(items), do: items
  def audit_log(items), do: items
end
"#;

const ELIXIR_AUDIT: &str = r#"defmodule Shop.Audit do
  def audit_log(items), do: items
end
"#;

#[test]
fn elixir_nested_modules_pipes_and_closures() {
    let p = parse("lib/shop/cart.ex", ELIXIR_CART);
    assert_well_formed(&p);
    let q = "lib.shop.cart.Shop.Cart";
    assert_eq!(shape(&p, q), k(MOD, 1, 37));
    assert_eq!(shape(&p, &format!("{q}.Item")), k(MOD, 4, 7));
    assert_eq!(shape(&p, &format!("{q}.Item.price")), k(F, 6, 6));
    assert_eq!(shape(&p, &format!("{q}.subtotal")), k(F, 10, 12));
    assert_eq!(sym(&p, &format!("{q}.subtotal")).doc, "Adds up the item prices.");
    assert_eq!(sym(&p, &format!("{q}.subtotal")).signature, "def subtotal(items)");
    assert_eq!(shape(&p, &format!("{q}.normalize")), k(F, 14, 14));
    assert_eq!(shape(&p, &format!("{q}.total")), k(F, 16, 23));
    assert_eq!(shape(&p, &format!("{q}.each_price")), k(F, 27, 31));
    assert_eq!(shape(&p, &format!("{q}.valid?")), k(F, 33, 33));
    assert_eq!(shape(&p, &format!("{q}.check!")), k(F, 34, 34));

    let total = format!("{q}.total");
    assert_call(&p, &total, "normalize", "");
    // A pipe into a name without parentheses is still a call.
    assert_call(&p, &total, "subtotal", "");
    assert_call(&p, &total, "apply_tax", "");
    assert_call(&p, &total, "format_money", "Shop.Money");
    assert_call(&p, &format!("{q}.subtotal"), "map", "Enum");
    assert_call(&p, &format!("{q}.subtotal"), "sum", "Enum");
    // A call inside an anonymous function belongs to the enclosing def.
    assert_call(&p, &format!("{q}.each_price"), "log_item", "");
    assert_call(&p, &format!("{q}.check!"), "valid?", "");
    assert_eq!(
        rationale(&p),
        [(total.clone(), "NOTE: tax is applied after normalize".to_string())]
    );

    let money = parse("lib/shop/money.ex", ELIXIR_MONEY);
    let audit = parse("lib/shop/audit.ex", ELIXIR_AUDIT);
    let w = resolve(&[&p, &money, &audit]);
    w.assert_provenance();
    w.expect_call(&total, &format!("{q}.apply_tax"), method::STATIC, 0.95);
    w.expect_call(&total, &format!("{q}.subtotal"), method::STATIC, 0.95);
    w.expect_call(&total, &format!("{q}.normalize"), method::STATIC, 0.95);
    w.expect_call(
        &format!("{q}.each_price"),
        &format!("{q}.log_item"),
        method::STATIC,
        0.95,
    );
    w.expect_call(&format!("{q}.check!"), &format!("{q}.valid?"), method::STATIC, 0.95);
    // Into another file: by name only.
    w.expect_call(&total, "lib.shop.money.Shop.Money.format_money", method::INFERRED, 0.40);
    w.expect_call(
        &format!("{q}.each_price"),
        "lib.shop.money.Shop.Money.report",
        method::INFERRED,
        0.50,
    );
    w.expect_unresolved(&format!("{q}.each_price"), "audit_log");
}

#[test]
fn elixir_test_files_and_unicode_function_names() {
    let t = parse(
        "test/shop/cart_test.exs",
        "defmodule Shop.CartTest do\n  use ExUnit.Case\n\n  test \"prices\" do\n    assert Shop.Cart.total([1]) == 1\n  end\nend\n",
    );
    assert_well_formed(&t);
    assert!(t.symbols.iter().all(|s| s.is_test), "{:?}", t.symbols);
    assert_call(&t, "test.shop.cart_test.Shop.CartTest", "total", "Shop.Cart");

    let src = "defmodule Shop.Units do\n  def größe(x), do: maß(x)\n  def maß(x), do: x * 2\nend\n";
    let p = parse("lib/units.ex", src);
    assert_eq!(shape(&p, "lib.units.Shop.Units.größe"), k(F, 2, 2));
    assert_eq!(shape(&p, "lib.units.Shop.Units.maß"), k(F, 3, 3));
    assert_call(&p, "lib.units.Shop.Units.größe", "maß", "");
    let w = resolve(&[&p]);
    w.expect_call(
        "lib.units.Shop.Units.größe",
        "lib.units.Shop.Units.maß",
        method::STATIC,
        0.95,
    );
}

/// `@spec name(args) :: type` is a typespec, not a call of `name`.
#[test]
fn elixir_spec_attributes_are_not_calls() {
    let src = r#"defmodule Shop.Cart do
  @spec subtotal([integer]) :: integer
  def subtotal(items), do: Enum.sum(items)

  @spec total([integer]) :: integer
  def total(items), do: subtotal(items)
end
"#;
    let p = parse("lib/cart.ex", src);
    assert_eq!(
        calls_of(&p, "lib.cart.Shop.Cart"),
        Vec::<(String, String)>::new(),
        "the module itself calls nothing"
    );
    let w = resolve(&[&p]);
    let into_subtotal: Vec<String> = w
        .res
        .edges
        .iter()
        .filter(|e| e.edge_type == EdgeType::Calls && e.to == w.sym("lib.cart.Shop.Cart.subtotal").id)
        .map(|e| w.qn_of(&e.from))
        .collect();
    assert_eq!(into_subtotal, ["lib.cart.Shop.Cart.total"]);
}

/// `user.email` on a variable is a map/struct field read, not a remote call
/// (a remote call needs a module alias or atom on the left).
#[test]
fn elixir_field_access_on_a_variable_is_not_a_call() {
    let src = r#"defmodule Shop.Mailer do
  def deliver(user) do
    send_mail(user.email, user.name)
  end

  defp send_mail(to, name), do: {to, name}
end
"#;
    let p = parse("lib/mailer.ex", src);
    assert_call(&p, "lib.mailer.Shop.Mailer.deliver", "send_mail", "");
    assert_eq!(
        calls_of(&p, "lib.mailer.Shop.Mailer.deliver"),
        [("send_mail".to_string(), String::new())],
        "user.email and user.name read fields"
    );
    let accounts = parse(
        "lib/accounts.ex",
        "defmodule Shop.Accounts do\n  def email(user), do: user\nend\n",
    );
    let w = resolve(&[&p, &accounts]);
    assert!(
        w.call("lib.mailer.Shop.Mailer.deliver", "lib.accounts.Shop.Accounts.email")
            .is_none(),
        "a field read made a CALLS edge: {:?}",
        w.calls_from("lib.mailer.Shop.Mailer.deliver")
    );
}

/// The usual Elixir doc is a heredoc: `@doc """` ... `"""`.
#[test]
fn elixir_heredoc_doc_is_the_doc_line() {
    let src = r#"defmodule Shop.Cart do
  @doc """
  Adds up the item prices.

  Ignores nil entries.
  """
  @spec subtotal([integer]) :: integer
  def subtotal(items), do: Enum.sum(items)
end
"#;
    let p = parse("lib/cart.ex", src);
    assert_eq!(sym(&p, "lib.cart.Shop.Cart.subtotal").doc, "Adds up the item prices.");
}

/// `__MODULE__.f()` names the enclosing module as surely as a bare `f()`.
#[test]
fn elixir_module_macro_receiver_is_the_own_module() {
    let src = "defmodule Shop.Cart do\n  def total(items), do: __MODULE__.subtotal(items)\n  def subtotal(items), do: Enum.sum(items)\nend\n";
    let p = parse("lib/cart.ex", src);
    let other = parse(
        "lib/other.ex",
        "defmodule Shop.Other do\n  def subtotal(x), do: x\nend\n",
    );
    let w = resolve(&[&p, &other]);
    w.expect_call(
        "lib.cart.Shop.Cart.total",
        "lib.cart.Shop.Cart.subtotal",
        method::STATIC,
        0.95,
    );
}

/// A multi-clause function is one function: its symbol covers every clause.
#[test]
fn elixir_multi_clause_function_covers_every_clause() {
    let src = "defmodule Shop.Math do\n  def fact(0), do: 1\n\n  def fact(n) when n > 0 do\n    n * fact(n - 1)\n  end\nend\n";
    let p = parse("lib/math.ex", src);
    assert_eq!(shape(&p, "lib.math.Shop.Math.fact"), k(F, 2, 6));
}

// ---------------------------------------------------------------------------
// Erlang
// ---------------------------------------------------------------------------

const ERLANG_CART: &str = r#"%% Prices a cart.
-module(cart).
-export([total/1, checkout/1]).
-record(cart, {items = []}).

%% Adds up the prices.
subtotal(Items) ->
    lists:foldl(fun(X, Acc) -> add(X, Acc) end, 0, Items).

add(X, Acc) -> X + Acc.

total(#cart{items = Items}) ->
    %% WHY: tax after the subtotal
    apply_tax(subtotal(Items)).

apply_tax(A) -> A * 108 div 100.

checkout(C) ->
    money:format_money(total(C)),
    report(C),
    audit_log(C).
"#;

#[test]
fn erlang_functions_records_lambdas_and_remote_calls() {
    let p = parse("src/cart.erl", ERLANG_CART);
    assert_well_formed(&p);
    assert_eq!(shape(&p, "src.cart.cart"), k(C, 4, 4));
    assert_eq!(shape(&p, "src.cart.subtotal"), k(F, 7, 8));
    assert_eq!(sym(&p, "src.cart.subtotal").doc, "Adds up the prices.");
    assert_eq!(shape(&p, "src.cart.add"), k(F, 10, 10));
    assert_eq!(shape(&p, "src.cart.total"), k(F, 12, 14));
    assert_eq!(shape(&p, "src.cart.apply_tax"), k(F, 16, 16));
    assert_eq!(shape(&p, "src.cart.checkout"), k(F, 18, 21));
    assert_eq!(sym(&p, "src.cart").doc, "Prices a cart.");

    assert_call(&p, "src.cart.subtotal", "foldl", "lists");
    assert_call(&p, "src.cart.subtotal", "add", "");
    assert_call(&p, "src.cart.total", "apply_tax", "");
    assert_call(&p, "src.cart.total", "subtotal", "");
    assert_call(&p, "src.cart.checkout", "format_money", "money");
    assert_eq!(
        rationale(&p),
        [("src.cart.total".to_string(), "WHY: tax after the subtotal".to_string())]
    );

    let money = parse(
        "src/money.erl",
        "-module(money).\nformat_money(C) -> C.\nreport(C) -> C.\naudit_log(C) -> C.\n",
    );
    let audit = parse("src/audit.erl", "-module(audit).\naudit_log(C) -> C.\n");
    let w = resolve(&[&p, &money, &audit]);
    w.assert_provenance();
    w.expect_call("src.cart.subtotal", "src.cart.add", method::STATIC, 0.95);
    w.expect_call("src.cart.total", "src.cart.apply_tax", method::STATIC, 0.95);
    w.expect_call("src.cart.checkout", "src.money.format_money", method::INFERRED, 0.40);
    w.expect_call("src.cart.checkout", "src.money.report", method::INFERRED, 0.50);
    w.expect_unresolved("src.cart.checkout", "audit_log");
}

/// `-spec f(type()) -> type().` names types, not calls.
#[test]
fn erlang_spec_types_are_not_calls() {
    let src = "-module(cart).\n-type amount() :: integer().\n-spec total([amount()]) -> amount().\ntotal(Items) -> lists:sum(Items).\n\namount() -> 0.\n";
    let p = parse("src/cart.erl", src);
    assert_eq!(calls_of(&p, "src.cart"), Vec::<(String, String)>::new());
    let w = resolve(&[&p]);
    assert!(
        w.calls_from("src.cart").is_empty(),
        "type names became calls: {:?}",
        w.calls_from("src.cart")
    );
}

/// The comment above a `-spec` is the doc of the function under it.
#[test]
fn erlang_doc_comment_above_a_spec() {
    let src = "-module(cart).\n\n%% Adds up the prices.\n-spec total([integer()]) -> integer().\ntotal(Items) -> lists:sum(Items).\n";
    let p = parse("src/cart.erl", src);
    assert_eq!(sym(&p, "src.cart.total").doc, "Adds up the prices.");
}

/// `?MODULE:f()` calls `f` of the module being defined.
#[test]
fn erlang_module_macro_receiver_is_the_own_module() {
    let p = parse(
        "src/cart.erl",
        "-module(cart).\ntotal(C) -> ?MODULE:subtotal(C).\nsubtotal(C) -> C.\n",
    );
    let other = parse("src/other.erl", "-module(other).\nsubtotal(C) -> C.\n");
    let w = resolve(&[&p, &other]);
    w.expect_call("src.cart.total", "src.cart.subtotal", method::STATIC, 0.95);
}

/// A function of several clauses is one function: its symbol covers them all.
#[test]
fn erlang_multi_clause_function_covers_every_clause() {
    let src = "-module(math).\nfact(0) -> 1;\nfact(N) when N > 0 ->\n    N * fact(N - 1).\n";
    let p = parse("src/math.erl", src);
    assert_eq!(shape(&p, "src.math.fact"), k(F, 2, 4));
}

/// EUnit: test modules are `*_tests.erl`, test functions `*_test()`.
#[test]
fn erlang_eunit_tests_are_tests() {
    let src = "-module(cart_tests).\n-include_lib(\"eunit/include/eunit.hrl\").\ntotal_test() -> ?assertEqual(3, cart:total([1, 2])).\n";
    let p = parse("src/cart_tests.erl", src);
    assert!(sym(&p, "src.cart_tests").is_test, "module");
    assert!(sym(&p, "src.cart_tests.total_test").is_test, "total_test");
}

// ---------------------------------------------------------------------------
// Haskell
// ---------------------------------------------------------------------------

const HASKELL_CART: &str = r#"-- | Prices a cart.
module Cart where

import qualified Data.Map as Map

-- | A cart.
data Cart = Cart { items :: [Int] }

newtype Price = Price Int

type Prices = [Int]

-- | Things with a total.
class Priced a where
  total :: a -> Int

-- | Adds up the prices.
subtotal :: Cart -> Int
subtotal cart = go (items cart)
  where
    go xs = foldr add 0 xs

add :: Int -> Int -> Int
add a b = a + b -- NOTE: no overflow check

lookupPrice :: String -> Map.Map String Int -> Maybe Int
lookupPrice name prices = Map.lookup name prices

checkout :: Cart -> String
checkout cart = formatMoney (subtotal cart)
"#;

#[test]
fn haskell_types_classes_where_and_qualified_calls() {
    let p = parse("src/Cart.hs", HASKELL_CART);
    assert_well_formed(&p);
    assert_eq!(shape(&p, "src.Cart.Cart"), k(C, 7, 7));
    assert_eq!(shape(&p, "src.Cart.Price"), k(C, 9, 9));
    assert_eq!(shape(&p, "src.Cart.Prices"), k(T, 11, 11));
    assert_eq!(shape(&p, "src.Cart.Priced"), k(I, 14, 15));
    assert_eq!(shape(&p, "src.Cart.subtotal"), k(F, 19, 21));
    assert_eq!(sym(&p, "src.Cart.subtotal").doc, "Adds up the prices.");
    assert_eq!(shape(&p, "src.Cart.subtotal.go"), k(F, 21, 21));
    assert_eq!(shape(&p, "src.Cart.add"), k(F, 24, 24));
    assert_eq!(shape(&p, "src.Cart.lookupPrice"), k(F, 27, 27));
    assert_eq!(shape(&p, "src.Cart.checkout"), k(F, 30, 30));

    assert_call(&p, "src.Cart.subtotal", "go", "");
    assert_call(&p, "src.Cart.subtotal.go", "foldr", "");
    assert_call(&p, "src.Cart.lookupPrice", "lookup", "Map");
    assert_call(&p, "src.Cart.checkout", "formatMoney", "");
    assert_call(&p, "src.Cart.checkout", "subtotal", "");
    assert_eq!(
        rationale(&p),
        [("src.Cart.add".to_string(), "NOTE: no overflow check".to_string())]
    );

    let money = parse(
        "src/Money.hs",
        "module Money where\n\nformatMoney :: Int -> String\nformatMoney = show\n",
    );
    // The same name in another language is not a candidate.
    let elm = parse(
        "web/Money.elm",
        "module Money exposing (..)\n\nformatMoney : Int -> String\nformatMoney c =\n    String.fromInt c\n",
    );
    let w = resolve(&[&p, &money, &elm]);
    w.assert_provenance();
    w.expect_call("src.Cart.subtotal", "src.Cart.subtotal.go", method::STATIC, 0.95);
    w.expect_call("src.Cart.checkout", "src.Cart.subtotal", method::STATIC, 0.95);
    w.expect_call("src.Cart.checkout", "src.Money.formatMoney", method::INFERRED, 0.50);
    assert!(w.call("src.Cart.checkout", "web.Money.formatMoney").is_none());
}

#[test]
fn haskell_test_files_and_unicode_names() {
    let t = parse(
        "test/CartSpec.hs",
        "module CartSpec where\n\nspec = describe \"cart\" (it \"adds\" (subtotal [] `shouldBe` 0))\n",
    );
    assert!(t.symbols.iter().all(|s| s.is_test));
    let p = parse("src/Units.hs", "module Units where\n\ngröße x = maß x\nmaß x = x * 2\n");
    assert_eq!(shape(&p, "src.Units.größe"), k(F, 3, 3));
    let w = resolve(&[&p]);
    w.expect_call("src.Units.größe", "src.Units.maß", method::STATIC, 0.95);
}

/// Instances of one class each implement the method: they are not one
/// function, and a call of the class method cannot be pinned to one of them.
#[test]
fn haskell_instance_methods_are_not_one_static_function() {
    let src = r#"module Shapes where

class Shape a where
  area :: a -> Double

data Circle = Circle Double
data Square = Square Double

instance Shape Circle where
  area (Circle r) = pi * r * r

instance Shape Square where
  area (Square s) = s * s

describe :: Shape a => a -> String
describe s = show (area s)
"#;
    let p = parse("src/Shapes.hs", src);
    let areas: Vec<i64> = p
        .symbols
        .iter()
        .filter(|s| s.name == "area")
        .map(|s| s.start_line)
        .collect();
    let w = resolve(&[&p]);
    let static_area: Vec<(String, String, f64)> = w
        .calls_from("src.Shapes.describe")
        .into_iter()
        .filter(|(_, m, _)| m == method::STATIC)
        .collect();
    assert!(
        static_area.is_empty(),
        "a class-method call was pinned to one instance: {static_area:?} (area symbols at lines {areas:?})"
    );
    assert_eq!(areas, [10, 13], "both instance implementations are symbols");
}

/// `f $ x` applies `f` exactly like `f (x)`.
#[test]
fn haskell_dollar_application_is_a_call() {
    let src = "module Report where\n\nreport :: Int -> IO ()\nreport n = putStrLn $ render $ double n\n\nrender :: Int -> String\nrender = show\n\ndouble :: Int -> Int\ndouble x = x * 2\n";
    let p = parse("src/Report.hs", src);
    assert_call(&p, "src.Report.report", "double", "");
    assert_call(&p, "src.Report.report", "render", "");
    assert_call(&p, "src.Report.report", "putStrLn", "");
    let w = resolve(&[&p]);
    w.expect_call("src.Report.report", "src.Report.render", method::STATIC, 0.95);
}

/// A function written as several equations is one function.
#[test]
fn haskell_multi_equation_function_covers_every_equation() {
    let src = "module Math where\n\nfact :: Int -> Int\nfact 0 = 1\nfact n = n * fact (n - 1)\n";
    let p = parse("src/Math.hs", src);
    assert_eq!(shape(&p, "src.Math.fact"), k(F, 4, 5));
}

// ---------------------------------------------------------------------------
// OCaml
// ---------------------------------------------------------------------------

const OCAML_CART: &str = r#"(* Prices a cart. *)

type cart = { items : int list }

let add a b = a + b

module Pricing = struct
  let subtotal xs = List.fold_left add 0 xs
  let total xs = xs |> subtotal |> Tax.apply
end

class register name = object (self)
  method name = name
  method greet = print_endline name
end

let rec even n = n = 0 || odd (n - 1)
and odd n = n <> 0 && even (n - 1)

let checkout c =
  (* NOTE: totals are in cents *)
  Money.format_money (Pricing.total c.items)
"#;

#[test]
fn ocaml_modules_classes_recursion_and_pipes() {
    let p = parse("src/cart.ml", OCAML_CART);
    assert_well_formed(&p);
    assert_eq!(shape(&p, "src.cart.cart"), k(T, 3, 3));
    assert_eq!(shape(&p, "src.cart.add"), k(F, 5, 5));
    assert_eq!(shape(&p, "src.cart.Pricing"), k(MOD, 7, 10));
    assert_eq!(shape(&p, "src.cart.Pricing.subtotal"), k(F, 8, 8));
    assert_eq!(shape(&p, "src.cart.Pricing.total"), k(F, 9, 9));
    assert_eq!(shape(&p, "src.cart.register"), k(C, 12, 15));
    assert_eq!(shape(&p, "src.cart.register.name"), k(M, 13, 13));
    assert_eq!(shape(&p, "src.cart.register.greet"), k(M, 14, 14));
    assert_eq!(shape(&p, "src.cart.even"), k(F, 17, 17));
    assert_eq!(shape(&p, "src.cart.odd"), k(F, 18, 18));
    assert_eq!(shape(&p, "src.cart.checkout"), k(F, 20, 22));

    assert_call(&p, "src.cart.Pricing.subtotal", "fold_left", "List");
    assert_call(&p, "src.cart.Pricing.total", "subtotal", "");
    assert_call(&p, "src.cart.Pricing.total", "apply", "Tax");
    assert_call(&p, "src.cart.even", "odd", "");
    assert_call(&p, "src.cart.odd", "even", "");
    assert_call(&p, "src.cart.checkout", "format_money", "Money");
    assert_call(&p, "src.cart.checkout", "total", "Pricing");
    assert_eq!(
        rationale(&p),
        [("src.cart.checkout".to_string(), "NOTE: totals are in cents".to_string())]
    );

    let money = parse("src/money.ml", "let format_money c = string_of_int c\n");
    let w = resolve(&[&p, &money]);
    w.assert_provenance();
    w.expect_call(
        "src.cart.Pricing.total",
        "src.cart.Pricing.subtotal",
        method::STATIC,
        0.95,
    );
    w.expect_call("src.cart.even", "src.cart.odd", method::STATIC, 0.95);
    w.expect_call("src.cart.odd", "src.cart.even", method::STATIC, 0.95);
    w.expect_call("src.cart.checkout", "src.money.format_money", method::INFERRED, 0.40);
    let e = w
        .call("src.cart.checkout", "src.cart.Pricing.total")
        .expect("Pricing.total is reached");
    assert_ne!(e.method, method::UNRESOLVED);
}

/// `obj#m` is a method call on `obj`; `self#m` is a call on self. Neither is a
/// bare call of a top-level function that happens to be called `m`.
#[test]
fn ocaml_method_invocation_has_a_receiver() {
    let src = r#"let area r = r *. r

class circle r = object (self)
  method area = 3.14 *. r *. r
  method describe = string_of_float self#area
end

let show (c : circle) = c#area
"#;
    let p = parse("src/shapes.ml", src);
    let w = resolve(&[&p]);
    assert!(
        w.call("src.shapes.circle.describe", "src.shapes.area")
            .is_none_or(|e| e.method != method::STATIC),
        "self#area was bound statically to the top-level function area"
    );
    assert!(
        w.call("src.shapes.show", "src.shapes.area")
            .is_none_or(|e| e.method != method::STATIC),
        "c#area was bound statically to the top-level function area"
    );
    assert_call(&p, "src.shapes.circle.describe", "area", "self");
    assert_call(&p, "src.shapes.show", "area", "c");
    w.expect_call(
        "src.shapes.circle.describe",
        "src.shapes.circle.area",
        method::STATIC,
        0.95,
    );
}

/// Infix keywords (`mod`, `land`, ...) and binding operators (`let*`) are
/// operators, which lge does not keep as calls (like `+`).
#[test]
fn ocaml_word_operators_are_not_calls() {
    let src = "let cents x = x mod 100\n\nlet both a b =\n  let* x = a in\n  let* y = b in\n  Some (x land y)\n";
    let p = parse("src/ops.ml", src);
    let names: Vec<String> = p.calls.iter().map(|c| c.name.clone()).collect();
    for op in ["mod", "land", "let*"] {
        assert!(!names.contains(&op.to_string()), "{op} became a call: {names:?}");
    }
}

/// Inside `show`, `render` is not `Json.render`: a member of a sibling module is
/// only in scope qualified or after `open Json`. Here it comes from `open Util`.
#[test]
fn ocaml_bare_call_is_not_bound_to_a_sibling_module_member() {
    let src = "open Util\n\nmodule Json = struct\n  let render x = \"json:\" ^ x\nend\n\nlet show x = render x\n";
    let p = parse("src/view.ml", src);
    let w = resolve(&[&p]);
    assert!(
        w.call("src.view.show", "src.view.Json.render")
            .is_none_or(|e| e.method != method::STATIC),
        "{:?}",
        w.calls_from("src.view.show")
    );
}

// ---------------------------------------------------------------------------
// Elm
// ---------------------------------------------------------------------------

const ELM_CART: &str = r#"module Cart exposing (..)

import Money


{-| A cart. -}
type alias Cart =
    { items : List Int }


type Msg
    = Add Int
    | Clear


{-| Adds up the prices. -}
subtotal : Cart -> Int
subtotal cart =
    List.foldl add 0 cart.items


add : Int -> Int -> Int
add a b =
    a + b


update : Msg -> Cart -> Cart
update msg cart =
    let
        append x =
            { cart | items = x :: cart.items }
    in
    case msg of
        Add x ->
            append x

        Clear ->
            { cart | items = [] }


view : Cart -> String
view cart =
    -- TODO: currency symbol per locale
    Money.formatMoney (subtotal cart)
"#;

#[test]
fn elm_types_let_functions_and_qualified_calls() {
    let p = parse("src/Cart.elm", ELM_CART);
    assert_well_formed(&p);
    assert_eq!(shape(&p, "src.Cart.Cart"), k(C, 7, 8));
    assert_eq!(sym(&p, "src.Cart.Cart").doc, "A cart.");
    assert_eq!(shape(&p, "src.Cart.Msg"), k(T, 11, 13));
    assert_eq!(shape(&p, "src.Cart.subtotal"), k(F, 18, 19));
    assert_eq!(sym(&p, "src.Cart.subtotal").doc, "Adds up the prices.");
    assert_eq!(shape(&p, "src.Cart.add"), k(F, 23, 24));
    assert_eq!(shape(&p, "src.Cart.update"), k(F, 28, 38));
    assert_eq!(shape(&p, "src.Cart.update.append"), k(F, 30, 31));
    assert_eq!(shape(&p, "src.Cart.view"), k(F, 42, 44));

    assert_call(&p, "src.Cart.subtotal", "foldl", "List");
    assert_call(&p, "src.Cart.update", "append", "");
    assert_call(&p, "src.Cart.view", "formatMoney", "Money");
    assert_call(&p, "src.Cart.view", "subtotal", "");
    assert_eq!(
        rationale(&p),
        [(
            "src.Cart.view".to_string(),
            "TODO: currency symbol per locale".to_string()
        )]
    );

    let money = parse(
        "src/Money.elm",
        "module Money exposing (..)\n\nformatMoney : Int -> String\nformatMoney c =\n    String.fromInt c\n",
    );
    let w = resolve(&[&p, &money]);
    w.assert_provenance();
    w.expect_call("src.Cart.update", "src.Cart.update.append", method::STATIC, 0.95);
    w.expect_call("src.Cart.view", "src.Cart.subtotal", method::STATIC, 0.95);
    w.expect_call("src.Cart.view", "src.Money.formatMoney", method::INFERRED, 0.40);
}

/// `x |> f` and `f <| x` apply `f`: the usual Elm pipeline style.
#[test]
fn elm_pipeline_calls_are_calls() {
    let src = r#"module Cart exposing (..)


total : List Int -> String
total items =
    items
        |> List.sum
        |> applyTax
        |> String.fromInt


applyTax : Int -> Int
applyTax a =
    a * 108 // 100


view : List Int -> String
view items =
    String.fromInt <| applyTax (List.length items)
"#;
    let p = parse("src/Cart.elm", src);
    assert_call(&p, "src.Cart.total", "sum", "List");
    assert_call(&p, "src.Cart.total", "applyTax", "");
    assert_call(&p, "src.Cart.total", "fromInt", "String");
    assert_call(&p, "src.Cart.view", "fromInt", "String");
    let w = resolve(&[&p]);
    w.expect_call("src.Cart.total", "src.Cart.applyTax", method::STATIC, 0.95);
}

/// The doc comment of a definition usually spans several lines.
#[test]
fn elm_multi_line_doc_comment() {
    let src = "module Cart exposing (..)\n\n\n{-| Adds up the prices.\n\n    total [ 1, 2 ] == 3\n\n-}\ntotal : List Int -> Int\ntotal items =\n    List.sum items\n";
    let p = parse("src/Cart.elm", src);
    assert_eq!(sym(&p, "src.Cart.total").doc, "Adds up the prices.");
}

// ---------------------------------------------------------------------------
// Nix
// ---------------------------------------------------------------------------

const NIX_CART: &str = r#"# Cart helpers.
{ lib, money }:

let
  # Adds up the prices.
  subtotal = items: lib.foldl' add 0 items;
  add = a: b: a + b;
  applyTax = { amount, rate ? 108 }: amount * rate / 100;
in
rec {
  total = items: applyTax { amount = subtotal items; };
  checkout = items: money.formatMoney (total items);
  # FIXME: describe is slow
  report = items: map (i: describe i) items;
}
"#;

#[test]
fn nix_lambdas_attrsets_and_selected_calls() {
    let p = parse("pkgs/cart.nix", NIX_CART);
    assert_well_formed(&p);
    assert_eq!(sym(&p, "pkgs.cart").doc, "Cart helpers.");
    assert_eq!(shape(&p, "pkgs.cart.subtotal"), k(F, 6, 6));
    assert_eq!(sym(&p, "pkgs.cart.subtotal").doc, "Adds up the prices.");
    assert_eq!(shape(&p, "pkgs.cart.add"), k(F, 7, 7));
    assert_eq!(shape(&p, "pkgs.cart.applyTax"), k(F, 8, 8));
    assert_eq!(shape(&p, "pkgs.cart.total"), k(F, 11, 11));
    assert_eq!(shape(&p, "pkgs.cart.checkout"), k(F, 12, 12));
    assert_eq!(shape(&p, "pkgs.cart.report"), k(F, 14, 14));

    assert_call(&p, "pkgs.cart.subtotal", "foldl'", "lib");
    assert_call(&p, "pkgs.cart.total", "applyTax", "");
    assert_call(&p, "pkgs.cart.total", "subtotal", "");
    assert_call(&p, "pkgs.cart.checkout", "formatMoney", "money");
    assert_call(&p, "pkgs.cart.checkout", "total", "");
    // Inside a lambda: still the binding's call.
    assert_call(&p, "pkgs.cart.report", "describe", "");

    let money = parse(
        "pkgs/money.nix",
        "{ lib }:\n{\n  formatMoney = cents: toString cents;\n  describe = x: x;\n}\n",
    );
    let w = resolve(&[&p, &money]);
    w.assert_provenance();
    w.expect_call("pkgs.cart.total", "pkgs.cart.applyTax", method::STATIC, 0.95);
    w.expect_call("pkgs.cart.total", "pkgs.cart.subtotal", method::STATIC, 0.95);
    w.expect_call("pkgs.cart.checkout", "pkgs.cart.total", method::STATIC, 0.95);
    w.expect_call("pkgs.cart.checkout", "pkgs.money.formatMoney", method::INFERRED, 0.40);
    w.expect_call("pkgs.cart.report", "pkgs.money.describe", method::INFERRED, 0.50);
}

/// RFC 145 doc comments: `/** ... */` above the binding.
#[test]
fn nix_rfc145_doc_comment() {
    let src = "{\n  /**\n    Applies the sales tax.\n  */\n  applyTax = amount: amount * 108 / 100;\n}\n";
    let p = parse("lib/tax.nix", src);
    assert_eq!(sym(&p, "lib.tax.applyTax").doc, "Applies the sales tax.");
}

// ---------------------------------------------------------------------------
// Solidity
// ---------------------------------------------------------------------------

const SOLIDITY_TOKEN: &str = r#"// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/// A token.
contract Token {
    mapping(address => uint256) balances;
    address owner;

    event Moved(address to, uint256 amount);
    struct Account { uint256 balance; }
    enum State { Open, Closed }

    modifier onlyOwner() {
        require(msg.sender == owner, "owner");
        _;
    }

    /// Moves tokens.
    function move(address to, uint256 amount) external {
        // NOTE: checks-effects-interactions
        _debit(amount);
        this.credit(to, amount);
        emit Moved(to, amount);
        uint256 fee = Fees(owner).compute(amount);
        helper(fee);
    }

    function _debit(uint256 amount) internal {
        balances[msg.sender] -= amount;
    }

    function credit(address to, uint256 amount) public {
        balances[to] += amount;
    }
}

function helper(uint256 x) pure returns (uint256) {
    return x;
}
"#;

#[test]
fn solidity_contracts_members_and_calls() {
    let p = parse("contracts/Token.sol", SOLIDITY_TOKEN);
    assert_well_formed(&p);
    let t = "contracts.Token.Token";
    assert_eq!(shape(&p, t), k(C, 5, 35));
    assert_eq!(sym(&p, t).doc, "A token.");
    assert_eq!(shape(&p, &format!("{t}.Account")), k(C, 10, 10));
    assert_eq!(shape(&p, &format!("{t}.State")), k(E, 11, 11));
    assert_eq!(shape(&p, &format!("{t}.onlyOwner")), k(M, 13, 16));
    assert_eq!(shape(&p, &format!("{t}.move")), k(M, 19, 26));
    assert_eq!(sym(&p, &format!("{t}.move")).doc, "Moves tokens.");
    assert_eq!(shape(&p, &format!("{t}._debit")), k(M, 28, 30));
    assert_eq!(shape(&p, &format!("{t}.credit")), k(M, 32, 34));
    assert_eq!(shape(&p, "contracts.Token.helper"), k(F, 37, 39));
    assert!(!p.symbols.iter().any(|s| s.name == "Moved"), "an event is not a symbol");

    let mv = format!("{t}.move");
    assert_call(&p, &mv, "_debit", "");
    assert_call(&p, &mv, "credit", "self");
    assert_call(&p, &mv, "compute", "Fees(owner)");
    assert_call(&p, &mv, "helper", "");
    assert!(!p.calls.iter().any(|c| c.name == "Moved"), "emit is not a call");
    assert_eq!(
        rationale(&p),
        [(mv.clone(), "NOTE: checks-effects-interactions".to_string())]
    );

    let fees = parse(
        "contracts/Fees.sol",
        "pragma solidity ^0.8.20;\ncontract Fees {\n    function compute(uint256 a) public pure returns (uint256) { return a / 100; }\n}\n",
    );
    let w = resolve(&[&p, &fees]);
    w.assert_provenance();
    // Implicit `this`, then `this.`, then a free function of the same file.
    w.expect_call(&mv, &format!("{t}._debit"), method::STATIC, 0.95);
    w.expect_call(&mv, &format!("{t}.credit"), method::STATIC, 0.95);
    w.expect_call(&mv, "contracts.Token.helper", method::STATIC, 0.95);
    w.expect_call(&mv, "contracts.Fees.Fees.compute", method::INFERRED, 0.40);
}

/// Library functions (`SafeMath.add`, `Strings.toString`, ...) are the
/// definitions most Solidity code calls.
#[test]
fn solidity_library_functions_are_symbols() {
    let src = r#"pragma solidity ^0.8.20;

/// Fixed-point helpers.
library PriceMath {
    /// Applies the tax rate.
    function withTax(uint256 a) internal pure returns (uint256) {
        return a * 108 / 100;
    }
}

contract Shop {
    function price(uint256 a) public pure returns (uint256) {
        return PriceMath.withTax(a);
    }
}
"#;
    let p = parse("contracts/Shop.sol", src);
    assert_eq!(shape(&p, "contracts.Shop.PriceMath"), k(C, 4, 9));
    assert_eq!(shape(&p, "contracts.Shop.PriceMath.withTax"), k(M, 6, 8));
    assert_eq!(sym(&p, "contracts.Shop.PriceMath.withTax").doc, "Applies the tax rate.");
    let w = resolve(&[&p]);
    let e = w
        .call("contracts.Shop.Shop.price", "contracts.Shop.PriceMath.withTax")
        .expect("price calls PriceMath.withTax");
    assert_eq!(e.method, method::STATIC);
}

/// A constructor is a definition: the calls in it are its calls.
#[test]
fn solidity_constructor_is_a_symbol() {
    let src = "pragma solidity ^0.8.20;\ncontract Vault {\n    address owner;\n    constructor(address o) {\n        owner = o;\n        _init();\n    }\n    function _init() internal {}\n}\n";
    let p = parse("contracts/Vault.sol", src);
    let ctor = p
        .symbols
        .iter()
        .find(|s| s.name == "constructor")
        .unwrap_or_else(|| panic!("no constructor symbol: {:?}", p.symbols));
    assert_eq!((ctor.kind.as_str(), ctor.start_line, ctor.end_line), (M, 4, 7));
    let init = p.calls.iter().find(|c| c.name == "_init").expect("_init call");
    assert_eq!(init.caller_id, ctor.id, "the constructor makes the call");
}

/// Interface functions are declarations worth finding (`IERC20.transfer`).
#[test]
fn solidity_interface_functions_are_symbols() {
    let src = "pragma solidity ^0.8.20;\ninterface IVault {\n    function deposit(uint256 amount) external;\n}\n";
    let p = parse("contracts/IVault.sol", src);
    assert_eq!(shape(&p, "contracts.IVault.IVault"), k(I, 2, 4));
    assert_eq!(shape(&p, "contracts.IVault.IVault.deposit"), k(M, 3, 3));
}

/// `function f() onlyOwner` runs the modifier: it is a call of it.
#[test]
fn solidity_modifier_invocation_is_a_call() {
    let src = "pragma solidity ^0.8.20;\ncontract Vault {\n    address owner;\n    modifier onlyOwner() { require(msg.sender == owner); _; }\n    function withdraw() external onlyOwner { }\n}\n";
    let p = parse("contracts/Vault.sol", src);
    assert_call(&p, "contracts.Vault.Vault.withdraw", "onlyOwner", "");
    let w = resolve(&[&p]);
    w.expect_call(
        "contracts.Vault.Vault.withdraw",
        "contracts.Vault.Vault.onlyOwner",
        method::STATIC,
        0.95,
    );
}

/// `_mint()` in a contract that inherits it from a base contract of the same
/// file calls the base's `_mint` (the OpenZeppelin style).
#[test]
fn solidity_call_of_an_inherited_function_has_an_edge() {
    let src = "pragma solidity ^0.8.20;\ncontract Base {\n    function _mint(address to) internal {}\n}\ncontract Token is Base {\n    function mint() external { _mint(msg.sender); }\n}\n";
    let p = parse("contracts/Token.sol", src);
    assert_call(&p, "contracts.Token.Token.mint", "_mint", "");
    let w = resolve(&[&p]);
    assert!(
        w.call("contracts.Token.Token.mint", "contracts.Token.Base._mint")
            .is_some(),
        "no edge for an inherited call: {:?} (external: {})",
        w.calls_from("contracts.Token.Token.mint"),
        w.res.stats.calls_external
    );
}

// ---------------------------------------------------------------------------
// Gleam, F#
// ---------------------------------------------------------------------------

/// `x |> f` applies `f`: Gleam's usual style.
#[test]
fn gleam_pipeline_calls_are_calls() {
    let src = "import gleam/int\n\nfn apply_tax(a: Int) -> Int {\n  a * 108 / 100\n}\n\npub fn total(items: List(Int)) -> Int {\n  items\n  |> int.sum\n  |> apply_tax\n}\n";
    let p = parse("src/cart.gleam", src);
    assert_call(&p, "src.cart.total", "sum", "int");
    assert_call(&p, "src.cart.total", "apply_tax", "");
    let w = resolve(&[&p]);
    w.expect_call("src.cart.total", "src.cart.apply_tax", method::STATIC, 0.95);
}

#[test]
fn fsharp_static_members_are_methods() {
    let src = "module Shop.Cart\n\ntype Cart(items: int list) =\n    member this.Total() = List.sum items\n    static member Empty() = Cart([])\n";
    let p = parse("src/Cart.fs", src);
    assert_eq!(shape(&p, "src.Cart.Shop.Cart.Cart.Total"), k(M, 4, 4));
    assert_eq!(shape(&p, "src.Cart.Shop.Cart.Cart.Empty"), k(M, 5, 5));
}

/// `x |> f` applies `f`: F#'s usual style.
#[test]
fn fsharp_pipeline_calls_are_calls() {
    let src = "module Shop.Cart\n\nlet applyTax amount = amount * 108 / 100\n\nlet checkout (items: int list) =\n    items |> List.sum |> applyTax\n";
    let p = parse("src/Cart.fs", src);
    assert_call(&p, "src.Cart.Shop.Cart.checkout", "sum", "List");
    assert_call(&p, "src.Cart.Shop.Cart.checkout", "applyTax", "");
    let w = resolve(&[&p]);
    w.expect_call(
        "src.Cart.Shop.Cart.checkout",
        "src.Cart.Shop.Cart.applyTax",
        method::STATIC,
        0.95,
    );
}

// ---------------------------------------------------------------------------
// CUDA
// ---------------------------------------------------------------------------

const CUDA_GPU: &str = r#"// Scales a vector on the GPU.
#include <cuda_runtime.h>

namespace gpu {

// Squares one value.
__device__ float square(float x) { return x * x; }

template <typename T>
__global__ void scale(T *data, int n) {
    int i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i < n) data[i] = square(data[i]);
}

class Buffer {
  public:
    explicit Buffer(int n) : n_(n) { allocate(); }
    void allocate() { cudaMalloc(&ptr_, n_ * sizeof(float)); }
    int size() const { return n_; }
  private:
    float *ptr_;
    int n_;
};

__global__ void fill(float *d) { d[threadIdx.x] = square(1.0f); }

void launch(float *d, int n) {
    // NOTE: 256 threads per block
    scale<float><<<(n + 255) / 256, 256>>>(d, n);
    fill<<<1, 32>>>(d);
    cudaDeviceSynchronize();
}

}  // namespace gpu

int main() {
    gpu::Buffer buf(1024);
    gpu::launch(nullptr, buf.size());
    return 0;
}
"#;

#[test]
fn cuda_kernels_launches_namespaces_and_classes() {
    let p = parse("src/gpu.cu", CUDA_GPU);
    assert_well_formed(&p);
    assert_eq!(shape(&p, "src.gpu.gpu"), k(MOD, 4, 34));
    assert_eq!(shape(&p, "src.gpu.gpu.square"), k(F, 7, 7));
    assert_eq!(sym(&p, "src.gpu.gpu.square").doc, "Squares one value.");
    assert_eq!(shape(&p, "src.gpu.gpu.scale"), k(F, 10, 13));
    assert_eq!(shape(&p, "src.gpu.gpu.Buffer"), k(C, 15, 23));
    assert_eq!(shape(&p, "src.gpu.gpu.Buffer.Buffer"), k(M, 17, 17));
    assert_eq!(shape(&p, "src.gpu.gpu.Buffer.allocate"), k(M, 18, 18));
    assert_eq!(shape(&p, "src.gpu.gpu.Buffer.size"), k(M, 19, 19));
    assert_eq!(shape(&p, "src.gpu.gpu.fill"), k(F, 25, 25));
    assert_eq!(shape(&p, "src.gpu.gpu.launch"), k(F, 27, 32));
    assert_eq!(shape(&p, "src.gpu.main"), k(F, 36, 40));

    assert_call(&p, "src.gpu.gpu.scale", "square", "");
    // `kernel<<<grid, block>>>(args)` and `kernel<T><<<...>>>(args)` are calls.
    assert_call(&p, "src.gpu.gpu.launch", "scale", "");
    assert_call(&p, "src.gpu.gpu.launch", "fill", "");
    assert_call(&p, "src.gpu.gpu.Buffer.Buffer", "allocate", "");
    assert_call(&p, "src.gpu.main", "launch", "gpu");
    assert_call(&p, "src.gpu.main", "size", "buf");
    assert_eq!(
        rationale(&p),
        [(
            "src.gpu.gpu.launch".to_string(),
            "NOTE: 256 threads per block".to_string()
        )]
    );

    let host = parse("src/host.cpp", "void report(int n) {}\n");
    let w = resolve(&[&p, &host]);
    w.assert_provenance();
    w.expect_call("src.gpu.gpu.scale", "src.gpu.gpu.square", method::STATIC, 0.95);
    w.expect_call("src.gpu.gpu.launch", "src.gpu.gpu.scale", method::STATIC, 0.95);
    w.expect_call("src.gpu.gpu.launch", "src.gpu.gpu.fill", method::STATIC, 0.95);
    // Implicit `this` in a constructor.
    w.expect_call(
        "src.gpu.gpu.Buffer.Buffer",
        "src.gpu.gpu.Buffer.allocate",
        method::STATIC,
        0.95,
    );
    let e = w
        .call("src.gpu.main", "src.gpu.gpu.launch")
        .expect("main calls gpu::launch");
    assert_ne!(e.method, method::UNRESOLVED);
}

// ---------------------------------------------------------------------------
// Luau
// ---------------------------------------------------------------------------

const LUAU_CART: &str = r#"--!strict
-- Prices a cart.
local Cart = {}
Cart.__index = Cart

export type Cart = { items: { number } }
type Price = number

-- Makes a cart.
function Cart.new(items: { number }): Cart
	return setmetatable({ items = items }, Cart) :: any
end

-- Adds up the prices.
function Cart:subtotal(): number
	local sum = 0
	for _, p in self.items do
		sum += p
	end
	return sum
end

function Cart:total(): number
	return applyTax(self:subtotal())
end

local function map<T, U>(xs: { T }, f: (T) -> U): { U }
	local out = {}
	for i, x in xs do
		out[i] = f(x)
	end
	return out
end

local double = function(x: number): number
	return x * 2
end

function applyTax(a: number): number
	return a * 108 // 100
end

local function checkout(items: { number }): string
	local cart = Cart.new(map(items, double))
	return tostring(cart:total())
end

return { Cart = Cart, checkout = checkout }
"#;

#[test]
fn luau_methods_generics_and_colon_calls() {
    let p = parse("src/cart.luau", LUAU_CART);
    assert_well_formed(&p);
    assert_eq!(sym(&p, "src.cart").doc, "Prices a cart.");
    assert_eq!(shape(&p, "src.cart.Cart.new"), k(M, 10, 12));
    assert_eq!(sym(&p, "src.cart.Cart.new").doc, "Makes a cart.");
    assert_eq!(shape(&p, "src.cart.Cart.subtotal"), k(M, 15, 21));
    assert_eq!(sym(&p, "src.cart.Cart.subtotal").doc, "Adds up the prices.");
    assert_eq!(shape(&p, "src.cart.Cart.total"), k(M, 23, 25));
    assert_eq!(shape(&p, "src.cart.map"), k(F, 27, 33));
    assert_eq!(shape(&p, "src.cart.applyTax"), k(F, 39, 41));
    assert_eq!(shape(&p, "src.cart.checkout"), k(F, 43, 46));

    assert_call(&p, "src.cart.Cart.total", "applyTax", "");
    assert_call(&p, "src.cart.Cart.total", "subtotal", "self");
    assert_call(&p, "src.cart.checkout", "new", "Cart");
    assert_call(&p, "src.cart.checkout", "map", "");
    assert_call(&p, "src.cart.checkout", "total", "cart");

    let money = parse(
        "src/money.luau",
        "local M = {}\nfunction M.formatMoney(c: number): string\n\treturn tostring(c)\nend\nlocal function show(c)\n\treturn M.formatMoney(c)\nend\nreturn M\n",
    );
    let other = parse(
        "src/report.luau",
        "local function report()\n\treturn applyTax(1)\nend\n",
    );
    let w = resolve(&[&p, &money, &other]);
    w.assert_provenance();
    w.expect_call("src.cart.Cart.total", "src.cart.applyTax", method::STATIC, 0.95);
    w.expect_call("src.cart.checkout", "src.cart.map", method::STATIC, 0.95);
    w.expect_call("src.cart.checkout", "src.cart.Cart.total", method::INFERRED, 0.40);
    w.expect_call("src.report.report", "src.cart.applyTax", method::INFERRED, 0.50);
}

/// `local Cart = {}; Cart.__index = Cart` is the Luau class: `self:subtotal()`
/// in `Cart:total()` names `Cart:subtotal` as surely as `self.subtotal()` in a
/// class of another language (acknowledged in the implementer's open issues).
#[test]
fn luau_self_call_reaches_the_own_method() {
    let p = parse("src/cart.luau", LUAU_CART);
    let w = resolve(&[&p]);
    w.expect_call("src.cart.Cart.total", "src.cart.Cart.subtotal", method::STATIC, 0.95);
}

/// `local double = function(x) ... end` defines a function named `double`.
#[test]
fn luau_function_assigned_to_a_local_is_a_symbol() {
    let p = parse("src/cart.luau", LUAU_CART);
    assert_eq!(shape(&p, "src.cart.double"), k(F, 35, 37));
}

/// `type Price = number` is a type definition. (Only `Price` is checked: a
/// `Cart` type would share its qualified name with the `Cart.*` methods.)
#[test]
fn luau_type_aliases_are_symbols() {
    let p = parse("src/cart.luau", LUAU_CART);
    assert_eq!(shape(&p, "src.cart.Price"), k(T, 7, 7));
}

/// TestEZ and Jest-Lua test files are `*.spec.luau`.
#[test]
fn luau_spec_files_are_tests() {
    let p = parse(
        "src/Cart.spec.luau",
        "return function()\n\tdescribe(\"cart\", function() end)\nend\n",
    );
    assert!(p.symbols[0].is_test);
}

// ---------------------------------------------------------------------------
// PowerShell
// ---------------------------------------------------------------------------

const POWERSHELL_CART: &str = r#"# Prices a cart.

class Cart {
    [int[]] $Items

    Cart([int[]] $items) {
        $this.Items = $items
    }

    # Adds up the prices.
    [int] Subtotal() {
        return ($this.Items | Measure-Object -Sum).Sum
    }

    [int] Total() {
        $sub = $this.Subtotal()
        return [Cart]::Round($sub * 1.08)
    }

    static [int] Round([double] $x) {
        return [math]::Floor($x)
    }
}

<#
.SYNOPSIS
Adds the sales tax.
#>
function Get-Tax {
    param([int] $Amount)
    return [math]::Floor($Amount * 108 / 100)
}

function script:Write-Log([string] $Message) {
    Write-Host $Message
}

function Invoke-Checkout([int[]] $Items) {
    # WHY: one cart per call
    $cart = [Cart]::new($Items)
    $Items | ForEach-Object { Write-Log "item $_" }
    $total = get-tax $cart.Total()
    Format-Money (Get-Tax $total)
}
"#;

#[test]
fn powershell_classes_functions_and_commands() {
    let p = parse("src/Cart.ps1", POWERSHELL_CART);
    assert_well_formed(&p);
    assert_eq!(shape(&p, "src.Cart.Cart"), k(C, 3, 23));
    assert_eq!(shape(&p, "src.Cart.Cart.Cart"), k(M, 6, 8));
    assert_eq!(shape(&p, "src.Cart.Cart.Subtotal"), k(M, 11, 13));
    assert_eq!(sym(&p, "src.Cart.Cart.Subtotal").doc, "Adds up the prices.");
    assert_eq!(shape(&p, "src.Cart.Cart.Total"), k(M, 15, 18));
    assert_eq!(shape(&p, "src.Cart.Cart.Round"), k(M, 20, 22));
    assert_eq!(shape(&p, "src.Cart.Get-Tax"), k(F, 29, 32));
    assert_eq!(shape(&p, "src.Cart.Invoke-Checkout"), k(F, 38, 44));

    assert_call(&p, "src.Cart.Cart.Total", "Subtotal", "self");
    assert_call(&p, "src.Cart.Cart.Total", "Round", "[Cart]");
    assert_call(&p, "src.Cart.Get-Tax", "Floor", "[math]");
    // A command inside a script block belongs to the function.
    assert_call(&p, "src.Cart.Invoke-Checkout", "Write-Log", "");
    assert_call(&p, "src.Cart.Invoke-Checkout", "Get-Tax", "");
    assert_call(&p, "src.Cart.Invoke-Checkout", "Format-Money", "");
    assert_eq!(
        rationale(&p),
        [(
            "src.Cart.Invoke-Checkout".to_string(),
            "WHY: one cart per call".to_string()
        )]
    );

    let money = parse(
        "src/Money.ps1",
        "function Format-Money([int] $Cents) {\n    return \"$Cents\"\n}\n",
    );
    let w = resolve(&[&p, &money]);
    w.assert_provenance();
    w.expect_call("src.Cart.Cart.Total", "src.Cart.Cart.Subtotal", method::STATIC, 0.95);
    w.expect_call("src.Cart.Invoke-Checkout", "src.Cart.Get-Tax", method::STATIC, 0.95);
    w.expect_call(
        "src.Cart.Invoke-Checkout",
        "src.Money.Format-Money",
        method::INFERRED,
        0.50,
    );
}

/// Comment-based help (`<# .SYNOPSIS ... #>`) is PowerShell's doc comment.
#[test]
fn powershell_comment_based_help_is_the_doc() {
    let p = parse("src/Cart.ps1", POWERSHELL_CART);
    assert_eq!(sym(&p, "src.Cart.Get-Tax").doc, "Adds the sales tax.");
}

/// `function script:Write-Log` defines `Write-Log` (in script scope); callers
/// write `Write-Log`.
#[test]
fn powershell_scope_modifier_is_not_part_of_the_name() {
    let p = parse("src/Cart.ps1", POWERSHELL_CART);
    assert_eq!(shape(&p, "src.Cart.Write-Log"), k(F, 34, 36));
    let w = resolve(&[&p]);
    w.expect_call("src.Cart.Invoke-Checkout", "src.Cart.Write-Log", method::STATIC, 0.95);
}

/// PowerShell command names are case-insensitive: `get-tax` runs `Get-Tax`.
#[test]
fn powershell_command_names_are_case_insensitive() {
    let p = parse("src/Cart.ps1", POWERSHELL_CART);
    let w = resolve(&[&p]);
    let line_42: Vec<&EdgeRec> = w
        .res
        .edges
        .iter()
        .filter(|e| e.edge_type == EdgeType::Calls && e.start_line == 42)
        .collect();
    assert!(
        line_42.iter().any(|e| e.to == w.sym("src.Cart.Get-Tax").id),
        "get-tax on line 42 has no edge to Get-Tax: {line_42:?}"
    );
}

/// `[Cart]::Round($x)` names the class `Cart` of the same file.
#[test]
fn powershell_static_call_on_a_same_file_class_is_static() {
    let p = parse("src/Cart.ps1", POWERSHELL_CART);
    let w = resolve(&[&p]);
    let e = w
        .call("src.Cart.Cart.Total", "src.Cart.Cart.Round")
        .expect("Total calls Round");
    assert_eq!(e.method, method::STATIC, "{e:?}");
}

/// Pester test files are `*.Tests.ps1`, usually next to the code they test.
#[test]
fn powershell_pester_files_are_tests() {
    let p = parse(
        "src/Cart.Tests.ps1",
        "Describe 'Cart' {\n    It 'adds' {\n        Get-Tax 100 | Should -Be 108\n    }\n}\n",
    );
    assert!(p.symbols[0].is_test);
}

// ---------------------------------------------------------------------------
// Clojure (dropped by the implementer)
// ---------------------------------------------------------------------------

/// Clojure was dropped because tree-sitter-clojure 0.1.0 needs tree-sitter
/// 0.25. tree-sitter-clojure-orchard 0.2.9 depends only on
/// tree-sitter-language 0.1, is ABI 14, loads with tree-sitter 0.27.2 and
/// takes a tags query (checked in a scratch crate), so Clojure can be read.
#[test]
fn clojure_files_are_indexed() {
    assert_eq!(lge_index::lang_of("src/shop/cart.clj"), Some("clojure"));
    let src = "(ns shop.cart)\n\n(defn apply-tax [amount] (quot (* amount 108) 100))\n\n(defn checkout [cart]\n  (apply-tax (reduce + 0 (:items cart))))\n";
    let p = parse("src/shop/cart.clj", src);
    assert!(p.symbols.iter().any(|s| s.name == "checkout" && s.kind == F));
    let w = resolve(&[&p]);
    let checkout = p.symbols.iter().find(|s| s.name == "checkout").expect("checkout");
    let apply = p.symbols.iter().find(|s| s.name == "apply-tax").expect("apply-tax");
    w.expect_call(&checkout.qualified_name, &apply.qualified_name, method::STATIC, 0.95);
}

// ---------------------------------------------------------------------------
// Every language of the group: broken and empty files
// ---------------------------------------------------------------------------

#[test]
fn syntax_errors_and_empty_files_never_panic() {
    let broken = [
        (
            "lib/broken.ex",
            "defmodule Broken do\n  def ok(x), do: helper(x)\n  def bad(x do\n    helper(\n  end\n  def helper(x), do: x\n",
        ),
        ("src/broken.erl", "-module(broken).\nok(X) -> helper(X).\nbad(X -> helper(.\nhelper(X) -> X.\n"),
        ("src/Broken.hs", "module Broken where\nok x = helper x\nbad x = = (\nhelper x = x\n"),
        ("src/Broken.elm", "module Broken exposing (..)\nok x = helper x\nbad x = (\nhelper x = x\n"),
        ("src/broken.ml", "let ok x = helper x\nlet bad x = (\nlet helper x = x\n"),
        ("pkgs/broken.nix", "{ ok = x: helper x; bad = x: (; helper = x: x; }\n"),
        (
            "contracts/Broken.sol",
            "contract Broken { function ok() public { helper(); } function bad( { } function helper() internal {} }\n",
        ),
        ("src/broken.gleam", "fn ok(x) { helper(x) }\nfn bad(x { (\nfn helper(x) { x }\n"),
        ("src/Broken.fs", "module Broken\nlet ok x = helper x\nlet bad x = (\nlet helper x = x\n"),
        // Not UTF-8, NUL bytes, a lone BOM.
        ("src/bytes.ex", "defmodule B do\n  def f(\u{0}x), do: g(x)\nend\n"),
        ("src/bom.hs", "\u{feff}module Bom where\nf x = g x\n"),
    ];
    for (path, src) in broken {
        let p = parse(path, src);
        assert_well_formed(&p);
        // A call that was read is owned by a symbol named in the file.
        let w = resolve(&[&p]);
        w.assert_provenance();
    }
    let raw = lge_index::parse_file("src/raw.erl", "svc", &[0xff, 0xfe, b'f', b'(', b')', b'.'], "h")
        .expect("invalid UTF-8 is not an error")
        .expect("supported");
    assert_well_formed(&raw);

    for path in [
        "a/e.ex",
        "a/e.exs",
        "a/e.erl",
        "a/e.hrl",
        "a/E.hs",
        "a/E.elm",
        "a/e.ml",
        "a/e.nix",
        "a/E.sol",
        "a/e.gleam",
        "a/E.fs",
    ] {
        for src in ["", "\n\n", "   ", "% only a comment\n", "-- only a comment\n"] {
            let p = parse(path, src);
            assert_well_formed(&p);
            assert!(p.calls.is_empty() || src.contains("comment"), "{path}: {:?}", p.calls);
        }
        let p = parse(path, "");
        assert_eq!(p.symbols.len(), 1, "{path}: only the module symbol");
        assert_eq!((p.symbols[0].start_line, p.symbols[0].end_line), (1, 1));
    }
}

/// The comment token of each language finds its rationale comments.
#[test]
fn rationale_comments_in_each_language() {
    let cases = [
        (
            "lib/r.ex",
            "defmodule R do\n  def f(x) do\n    # WHY: keep order\n    x\n  end\nend\n",
            "R.f",
            "WHY: keep order",
        ),
        (
            "src/r.erl",
            "-module(r).\nf(X) ->\n    %% HACK: until v2\n    X.\n",
            "f",
            "HACK: until v2",
        ),
        (
            "src/R.hs",
            "module R where\nf x =\n  -- TODO: strict\n  x\n",
            "f",
            "TODO: strict",
        ),
        (
            "src/r.ml",
            "let f x =\n  (* IMPORTANT: pure *)\n  x\n\nlet g y = y\n",
            "f",
            "IMPORTANT: pure",
        ),
        (
            "src/R.elm",
            "module R exposing (..)\nf x =\n    -- FIXME: slow\n    x\n",
            "f",
            "FIXME: slow",
        ),
        (
            "pkgs/r.nix",
            "{\n  f = x:\n    # NOTE: lazy\n    x;\n}\n",
            "f",
            "NOTE: lazy",
        ),
        (
            "contracts/R.sol",
            "contract R {\n    function f() public {\n        // RATIONALE: gas\n    }\n}\n",
            "R.f",
            "RATIONALE: gas",
        ),
    ];
    for (path, src, owner, note) in cases {
        let p = parse(path, src);
        let module = &p.symbols[0].qualified_name;
        assert_eq!(
            rationale(&p),
            [(format!("{module}.{owner}"), note.to_string())],
            "{path}"
        );
    }
}

/// A rationale comment belongs to the innermost symbol around it. When a
/// function spans the whole file, that is the function, not the module.
#[test]
fn rationale_in_a_function_spanning_the_whole_file_belongs_to_the_function() {
    let p = parse("src/r.ml", "let f x =\n  (* IMPORTANT: pure *)\n  x\n");
    assert_eq!(rationale(&p), [("src.r.f".to_string(), "IMPORTANT: pure".to_string())]);
}
