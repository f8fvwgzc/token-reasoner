//! Languages the indexer reads through a grammar and a tags query (Go, Rust,
//! Ruby, Java, ...). They have no import sites, so the call ladder is: the
//! enclosing class for `self`/`this` (and for a bare call where the language
//! has an implicit `this`), the same file, then the repository by name.

use crate::support::{edge, edges_from, expect, run, Repo};
use lge_core::{fact, kind, symbol_id, EdgeType, SymbolRec};

#[test]
fn a_bare_call_to_the_one_callable_of_the_same_file_is_static() {
    let mut r = Repo::new();
    r.file("shop/cart.go", "shop");
    r.file("shop/money.go", "shop");
    let checkout = r.function("shop/cart.go", "Checkout");
    let apply = r.function("shop/cart.go", "ApplyTax");
    // The same name in another file does not make the same-file call ambiguous.
    r.function("shop/money.go", "ApplyTax");
    r.call(&checkout, "", "ApplyTax", 3);
    let res = run(&r);
    let e = expect(&res, EdgeType::Calls, &checkout, &apply, "static", 0.95);
    assert_eq!((e.evidence_file.as_str(), e.start_line), ("shop/cart.go", 3));
}

#[test]
fn two_callables_of_the_same_name_in_the_file_are_not_narrowed_down() {
    let mut r = Repo::new();
    r.file("shop/cart.rs", "shop");
    let checkout = r.function("shop/cart.rs", "checkout");
    r.function("shop/cart.rs", "a.helper");
    r.function("shop/cart.rs", "b.helper");
    r.call(&checkout, "", "helper", 4);
    let res = run(&r);
    let calls = edges_from(&res, EdgeType::Calls, &checkout);
    assert_eq!(calls.len(), 1);
    assert_eq!(
        (calls[0].to.as_str(), calls[0].method.as_str(), calls[0].confidence),
        ("unresolved:helper", "unresolved", 0.0)
    );
}

#[test]
fn self_calls_reach_the_enclosing_class() {
    let mut r = Repo::new();
    r.file("shop/cart.rs", "shop");
    r.file("shop/other.rs", "shop");
    r.class("shop/cart.rs", "Cart");
    let total = r.method("shop/cart.rs", "Cart", "total");
    let subtotal = r.method("shop/cart.rs", "Cart", "subtotal");
    r.class("shop/other.rs", "Other");
    r.method("shop/other.rs", "Other", "subtotal");
    r.call(&total, "self", "subtotal", 7);
    let res = run(&r);
    expect(&res, EdgeType::Calls, &total, &subtotal, "static", 0.95);
    assert_eq!(edges_from(&res, EdgeType::Calls, &total).len(), 1);
}

#[test]
fn self_outside_a_class_is_matched_by_name() {
    let mut r = Repo::new();
    r.file("shop/cart.lua", "shop");
    r.file("shop/money.lua", "shop");
    let total = r.method("shop/cart.lua", "Cart", "total");
    let format = r.function("shop/money.lua", "format_money");
    r.call(&total, "self", "format_money", 2);
    let res = run(&r);
    expect(&res, EdgeType::Calls, &total, &format, "inferred", 0.40);
}

#[test]
fn a_bare_call_reaches_a_sibling_method_only_with_an_implicit_this() {
    // Java: `applyTax(x)` in a method of Checkout calls Checkout.applyTax.
    let mut r = Repo::new();
    r.file("shop/Checkout.java", "shop");
    r.class("shop/Checkout.java", "Checkout");
    let checkout = r.method("shop/Checkout.java", "Checkout", "checkout");
    let apply = r.method("shop/Checkout.java", "Checkout", "applyTax");
    r.call(&checkout, "", "applyTax", 9);
    let res = run(&r);
    expect(&res, EdgeType::Calls, &checkout, &apply, "static", 0.95);

    // Go: a bare `Subtotal()` is a package function, never a method.
    let mut r = Repo::new();
    r.file("shop/cart.go", "shop");
    r.class("shop/cart.go", "Cart");
    let total = r.method("shop/cart.go", "Cart", "Total");
    r.method("shop/cart.go", "Cart", "Subtotal");
    r.call(&total, "", "Subtotal", 5);
    let res = run(&r);
    assert!(edges_from(&res, EdgeType::Calls, &total).is_empty());
    assert_eq!(res.stats.calls_external, 1);
}

#[test]
fn an_implicit_this_prefers_the_member_over_a_function_of_the_file() {
    let mut r = Repo::new();
    r.file("shop/cart.rb", "shop");
    r.class("shop/cart.rb", "Cart");
    let total = r.method("shop/cart.rb", "Cart", "total");
    let member = r.method("shop/cart.rb", "Cart", "subtotal");
    r.function("shop/cart.rb", "subtotal");
    r.call(&total, "", "subtotal", 3);
    let res = run(&r);
    expect(&res, EdgeType::Calls, &total, &member, "static", 0.95);
}

#[test]
fn other_files_are_reached_by_name_only() {
    let mut r = Repo::new();
    r.file("shop/cart.go", "shop");
    r.file("shop/money.go", "shop");
    r.file("shop/tax.go", "shop");
    r.file("shop/a.go", "shop");
    r.file("shop/b.go", "shop");
    let checkout = r.function("shop/cart.go", "Checkout");
    let format = r.function("shop/money.go", "FormatMoney");
    r.class("shop/tax.go", "Table");
    let rate = r.method("shop/tax.go", "Table", "Rate");
    r.function("shop/a.go", "Round");
    r.function("shop/b.go", "Round");
    r.call(&checkout, "", "FormatMoney", 3);
    r.call(&checkout, "table", "Rate", 4);
    r.call(&checkout, "", "Round", 5);
    r.call(&checkout, "", "Missing", 6);
    r.call(&checkout, "m", "get", 7);
    let res = run(&r);
    expect(&res, EdgeType::Calls, &checkout, &format, "inferred", 0.50);
    expect(&res, EdgeType::Calls, &checkout, &rate, "inferred", 0.40);
    expect(&res, EdgeType::Calls, &checkout, "unresolved:Round", "unresolved", 0.0);
    assert_eq!(edges_from(&res, EdgeType::Calls, &checkout).len(), 3);
    // `Missing` is nowhere; `get` on an unknown receiver is too common to match.
    assert_eq!(res.stats.calls_external, 2);
}

#[test]
fn a_name_match_stays_inside_the_languages_that_call_each_other() {
    let mut r = Repo::new();
    r.file("svc/cart.rb", "svc");
    r.file("svc/money.go", "svc");
    r.file("svc/app.py", "svc");
    r.file("svc/util.ts", "svc");
    r.file("native/cart.c", "native");
    r.file("native/money.cpp", "native");
    let checkout = r.function("svc/cart.rb", "checkout");
    r.function("svc/money.go", "format_money");
    let handler = r.function("svc/app.py", "handler");
    let slug = r.function("svc/util.ts", "slugify");
    r.function("svc/money.go", "slugify");
    let c_checkout = r.function("native/cart.c", "checkout_c");
    let round = r.function("native/money.cpp", "round_cents");
    // Ruby cannot call a Go function by name.
    r.call(&checkout, "", "format_money", 2);
    // Python and TypeScript still see each other, and not Go.
    r.call(&handler, "", "slugify", 3);
    // C calls into C++.
    r.call(&c_checkout, "", "round_cents", 4);
    let res = run(&r);
    assert!(edges_from(&res, EdgeType::Calls, &checkout).is_empty());
    expect(&res, EdgeType::Calls, &handler, &slug, "inferred", 0.50);
    expect(&res, EdgeType::Calls, &c_checkout, &round, "inferred", 0.50);
    assert_eq!(res.stats.calls_external, 1);
}

#[test]
fn a_module_defined_inside_a_file_does_not_own_the_files_facts() {
    let mut r = Repo::new();
    let file_module = r.file("svc/cart.ex", "svc");
    // An Elixir `defmodule` inside the file, with an id that sorts first.
    r.input.symbols.push(SymbolRec {
        id: "0".to_string(),
        name: "Shop.Cart".to_string(),
        qualified_name: "svc.cart.Shop.Cart".to_string(),
        kind: kind::MODULE.to_string(),
        file_path: "svc/cart.ex".to_string(),
        service: "svc".to_string(),
        start_line: 1,
        end_line: 20,
        ..SymbolRec::default()
    });
    assert!("0" < file_module.as_str());
    r.fact_in(
        "svc/cart.ex",
        "not-a-symbol",
        fact::API_EXPOSE,
        "/carts",
        "GET",
        3,
        true,
    );
    let res = run(&r);
    let exposes: Vec<&str> = res
        .edges
        .iter()
        .filter(|e| e.edge_type == EdgeType::ExposesApi)
        .map(|e| e.from.as_str())
        .collect();
    assert_eq!(exposes, [file_module.as_str()]);
    assert!(edge(&res, EdgeType::ExposesApi, "0", &lge_core::endpoint_id("GET", "/carts")).is_none());
    assert_eq!(file_module, symbol_id("svc/cart.ex", "svc.cart", kind::MODULE));
}
