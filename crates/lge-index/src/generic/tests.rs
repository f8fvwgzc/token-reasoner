use super::grammars::{CallSyntax, GRAMMARS};
use super::text::{
    comment_text, go_receiver, is_test_path, leading_doc, module_doc, qualifier_before, receiver_before, signature,
    Lines,
};
use super::*;
use lge_core::lang;

const PLAIN: CallSyntax = CallSyntax {
    colon: false,
    dollar: false,
    message: false,
    hash: false,
    slash: false,
};

/// The receiver of the call whose name is the last occurrence of `name` in `src`.
fn receiver(src: &str, name: &str, syntax: CallSyntax) -> String {
    let at = src.rfind(name).expect("name in source");
    receiver_before(src.as_bytes(), at, syntax, None)
}

#[test]
fn every_grammar_loads_and_its_query_compiles() {
    for (i, g) in GRAMMARS.iter().enumerate() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&g.language.into())
            .unwrap_or_else(|e| panic!("{}: {e}", g.lang));
        assert!(parser.parse("x", None).is_some(), "{}", g.lang);
        config(i).unwrap_or_else(|e| panic!("{e:#}"));
    }
    let mut langs: Vec<&str> = GRAMMARS.iter().map(|g| g.lang).collect();
    langs.sort_unstable();
    langs.dedup();
    assert_eq!(langs.len(), GRAMMARS.len(), "one entry per language");
    let mut extensions: Vec<&str> = GRAMMARS.iter().flat_map(|g| g.extensions.iter().copied()).collect();
    let n = extensions.len();
    extensions.sort_unstable();
    extensions.dedup();
    assert_eq!(extensions.len(), n, "an extension belongs to one language");
}

#[test]
fn comment_tokens_follow_the_language() {
    let expected = [
        (
            "//",
            &[
                lang::RUST,
                lang::GO,
                lang::JAVA,
                lang::C,
                lang::CPP,
                lang::CUDA,
                lang::CSHARP,
            ][..],
        ),
        (
            "//",
            &[
                lang::PHP,
                lang::KOTLIN,
                lang::SWIFT,
                lang::SCALA,
                lang::GROOVY,
                lang::DART,
            ][..],
        ),
        (
            "//",
            &[lang::GLEAM, lang::FSHARP, lang::ZIG, lang::SOLIDITY, lang::OBJC][..],
        ),
        (
            "#",
            &[
                lang::RUBY,
                lang::BASH,
                lang::POWERSHELL,
                lang::ELIXIR,
                lang::R,
                lang::JULIA,
                lang::NIX,
                lang::PERL,
            ][..],
        ),
        (";", &[lang::CLOJURE][..]),
        ("--", &[lang::LUA, lang::LUAU, lang::HASKELL, lang::ELM][..]),
        ("%", &[lang::ERLANG][..]),
        ("(*", &[lang::OCAML][..]),
    ];
    let mut seen = 0;
    for (token, langs) in expected {
        for l in langs {
            assert_eq!(comment_token(l), Some(token), "{l}");
            seen += 1;
        }
    }
    assert_eq!(seen, GRAMMARS.len(), "every language is listed");
    assert_eq!(comment_token(lang::PYTHON), None);
    assert_eq!(comment_token("cobol"), None);
}

#[test]
fn a_member_operator_before_the_name_makes_a_receiver() {
    for (src, name, want) in [
        ("cart.total()", "total", "cart"),
        ("cart->total(cart)", "total", "cart"),
        ("std::accumulate(a, b, 0)", "accumulate", "std"),
        ("Repo::new()", "new", "Repo"),
        ("self.repo.save(x)", "save", "self.repo"),
        ("Repo().save()", "save", "Repo()"),
        ("items[0].save()", "save", "items[0]"),
        ("user?.save()", "save", "user"),
        ("user&.save", "save", "user"),
        ("items\n    .map(f)", "map", "items"),
        ("$cart->total()", "total", "$cart"),
        ("[math]::Floor($x)", "Floor", "[math]"),
        ("@items.sum", "sum", "@items"),
        ("Shop.Money.format_money(x)", "format_money", "Shop.Money"),
    ] {
        assert_eq!(receiver(src, name, PLAIN), want, "{src}");
    }
}

#[test]
fn self_and_this_receivers_are_self() {
    for src in [
        "self.subtotal()",
        "this.subtotal()",
        "this->subtotal()",
        "Self::subtotal()",
        "$this->subtotal()",
        "cls.subtotal()",
    ] {
        assert_eq!(receiver(src, "subtotal", PLAIN), "self", "{src}");
    }
    let r = CallSyntax { dollar: true, ..PLAIN };
    assert_eq!(receiver(".self$subtotal()", "subtotal", r), "self");
    // A Go method's receiver variable stands for the instance.
    let src = b"return c.Subtotal()";
    assert_eq!(receiver_before(src, 9, PLAIN, Some("c")), "self");
    assert_eq!(receiver_before(src, 9, PLAIN, Some("x")), "c");
}

#[test]
fn a_call_without_a_member_operator_is_bare() {
    for (src, name) in [
        ("apply_tax(total)", "apply_tax"),
        ("return apply_tax(x)", "apply_tax"),
        ("format_money (apply_tax total)", "apply_tax"),
        ("foo(bar: baz())", "baz"),
        ("x = a ? b : c()", "c"),
        ("f . g x", "g"),
        ("for i in 0..count() {}", "count"),
    ] {
        assert_eq!(receiver(src, name, PLAIN), "", "{src}");
    }
}

#[test]
fn colon_dollar_and_message_calls_are_per_language() {
    let colon = CallSyntax { colon: true, ..PLAIN };
    assert_eq!(receiver("cart:total()", "total", colon), "cart");
    assert_eq!(receiver("money:format_money(X)", "format_money", colon), "money");
    assert_eq!(receiver("self:subtotal()", "subtotal", colon), "self");
    assert_eq!(
        receiver("cart:total()", "total", PLAIN),
        "",
        "':' is no member operator elsewhere"
    );
    let dollar = CallSyntax { dollar: true, ..PLAIN };
    assert_eq!(receiver("cart$total()", "total", dollar), "cart");
    assert_eq!(receiver("cart$total()", "total", PLAIN), "");
    let message = CallSyntax { message: true, ..PLAIN };
    assert_eq!(receiver("x = [cart total];", "total", message), "cart");
    assert_eq!(receiver("ApplyTax([self subtotal])", "subtotal", message), "self");
    assert_eq!(receiver("return ApplyTax(x)", "ApplyTax", message), "");
}

#[test]
fn a_receiver_that_is_no_expression_is_kept_as_written() {
    assert_eq!(receiver("\"abc\".upcase", "upcase", PLAIN), "\"abc\"");
    let long = format!("{}.save()", "a".repeat(120));
    assert_eq!(receiver(&long, "save", PLAIN).chars().count(), 80);
}

#[test]
fn qualifiers_before_a_definition_name() {
    let q = |src: &str, name: &str| qualifier_before(src.as_bytes(), 0, src.rfind(name).unwrap());
    assert_eq!(q("function Cart:total()", "total"), ["Cart"]);
    assert_eq!(q("function M.helper()", "helper"), ["M"]);
    assert_eq!(q("int shop::Cart::total() const", "total"), ["shop", "Cart"]);
    assert_eq!(q("def self.create", "create"), ["self"]);
    assert!(q("pub fn total(&self)", "total").is_empty());
    assert!(q("func (c *Cart) Total() int", "Total").is_empty());
}

#[test]
fn go_methods_name_their_receiver() {
    assert_eq!(
        go_receiver("func (c *Cart) "),
        Some((Some("c".to_string()), "Cart".to_string()))
    );
    assert_eq!(
        go_receiver("func (s Stack[T]) "),
        Some((Some("s".to_string()), "Stack".to_string()))
    );
    assert_eq!(go_receiver("func (*Cart) "), Some((None, "Cart".to_string())));
    assert_eq!(go_receiver("func "), None);
}

#[test]
fn signatures_stop_at_the_body() {
    let sig = |src: &str, name: &str| {
        let lines = Lines::new(src.as_bytes());
        signature(src.as_bytes(), &lines, 0, src.find(name).unwrap(), src.len())
    };
    assert_eq!(
        sig("pub fn total(&self) -> u64 {\n    1\n}", "total"),
        "pub fn total(&self) -> u64"
    );
    assert_eq!(
        sig("@Override\npublic int total(\n    int a,\n    int b) {\n", "total"),
        "public int total( int a, int b)"
    );
    assert_eq!(sig("def total(cart) do\n  1\nend", "total"), "def total(cart)");
    assert_eq!(sig("total cart =\n    1", "total"), "total cart");
    assert_eq!(
        sig("fun subtotal(): Int = items.sum()", "subtotal"),
        "fun subtotal(): Int"
    );
    assert_eq!(sig("def f(x: Int = 1): Int = x", "f"), "def f(x: Int = 1): Int");
    assert_eq!(sig("public int X() => 5;", "X"), "public int X()");
    assert_eq!(
        sig(
            "Cart <- setRefClass(\"Cart\",\n  a = 1,\n  b = 2,\n  c = 3,\n  d = 4,\n  e = 5,\n  f = 6)",
            "setRefClass"
        ),
        "Cart <- setRefClass(\"Cart\", a = 1, b = 2, c = 3, d = 4, e = 5,"
    );
    assert_eq!(sig("total(Cart) ->\n    1.", "total"), "total(Cart)");
    assert_eq!(sig("void f(int x);", "f"), "void f(int x)");
    assert_eq!(sig(&format!("fn {}() {{}}", "a".repeat(300)), "a").chars().count(), 240);
}

#[test]
fn doc_lines_come_from_comments_above() {
    let lines = [
        "/**",
        " * Adds up the item prices.",
        " */",
        "@Override",
        "int subtotal() {",
        "",
        "-- | Adds the tax.",
        "applyTax : Int -> Int",
        "applyTax amount =",
        "",
        "  @doc \"Prices the cart.\"",
        "  def checkout(cart) do",
        "",
        "\"The subtotal with tax.\"",
        "total(cart) = 1",
    ];
    assert_eq!(leading_doc(&lines, 4, "//", "subtotal"), "Adds up the item prices.");
    assert_eq!(leading_doc(&lines, 8, "--", "applyTax"), "Adds the tax.");
    assert_eq!(leading_doc(&lines, 11, "#", "checkout"), "Prices the cart.");
    assert_eq!(leading_doc(&lines, 14, "#", "total"), "The subtotal with tax.");
    assert_eq!(leading_doc(&lines, 6, "--", "x"), "", "a blank line ends the block");
    assert_eq!(comment_text("/// <summary>Adds up.</summary>"), "Adds up.");
    assert_eq!(comment_text("{-| The prices."), "The prices.");
    assert_eq!(comment_text("(* Adds the tax. *)"), "Adds the tax.");
    assert_eq!(comment_text("%% Renders cents."), "Renders cents.");
    assert_eq!(comment_text("//// Prices a cart."), "Prices a cart.");
    assert_eq!(
        module_doc(&["#!/usr/bin/env bash", "# Prices a cart.", "x=1"], "#", &[]),
        "Prices a cart."
    );
    assert_eq!(
        module_doc(&["--!strict", "-- Prices a cart.", "local x = 1"], "--", &[]),
        "Prices a cart."
    );
    assert_eq!(
        module_doc(&["// SPDX-License-Identifier: MIT", "pragma x;"], "//", &[]),
        ""
    );
    // The comment right above the first definition is that definition's.
    assert_eq!(module_doc(&["# Renders cents.", "def f", "end"], "#", &[2]), "");
}

#[test]
fn test_files_are_known_by_their_path() {
    for path in [
        "tests/cart.rs",
        "src/test/java/CartTest.java",
        "app/spec/cart_spec.rb",
        "web/__tests__/cart.dart",
        "shop/cart_test.go",
        "CartTests.swift",
        "test_cart.exs",
        "ShopTest.kt",
    ] {
        assert!(is_test_path(path), "{path}");
    }
    for path in ["src/cart.rs", "shop/contest.go", "latest.rb", "testing/cart.c"] {
        assert!(!is_test_path(path), "{path}");
    }
}

#[test]
fn module_names_drop_only_the_extension() {
    assert_eq!(module_name("go/cart_test.go"), "go.cart_test");
    assert_eq!(module_name("a/b/Cart.java"), "a.b.Cart");
    assert_eq!(module_name("Makefile"), "Makefile");
}

/// Qualified name, kind and line range of every symbol of a parsed snippet.
fn symbols(path: &str, lang: &str, src: &str) -> Vec<(String, String, i64, i64)> {
    parse_generic(path, "svc", src.as_bytes(), "h", lang)
        .unwrap()
        .symbols
        .into_iter()
        .map(|s| (s.qualified_name, s.kind, s.start_line, s.end_line))
        .collect()
}

fn sym(qn: &str, kind: &str, start: i64, end: i64) -> (String, String, i64, i64) {
    (qn.to_string(), kind.to_string(), start, end)
}

#[test]
fn an_impl_block_gives_its_functions_to_the_type() {
    let src = "struct Cart;\n\nimpl Cart {\n    fn total(&self) -> u64 {\n        self.subtotal()\n    }\n}\n\nimpl Other {\n    fn run() {}\n}\n";
    assert_eq!(
        symbols("a/cart.rs", lang::RUST, src),
        [
            sym("a.cart", kind::MODULE, 1, 11),
            sym("a.cart.Cart", kind::CLASS, 1, 1),
            sym("a.cart.Cart.total", kind::METHOD, 4, 6),
            sym("a.cart.Other", kind::CLASS, 9, 11),
            sym("a.cart.Other.run", kind::METHOD, 10, 10),
        ]
    );
    let parsed = parse_generic("a/cart.rs", "svc", src.as_bytes(), "h", lang::RUST).unwrap();
    let call = &parsed.calls[0];
    assert_eq!(
        (call.name.as_str(), call.receiver.as_str(), call.line),
        ("subtotal", "self", 5)
    );
    assert_eq!(call.caller_id, parsed.symbols[2].id);
}

#[test]
fn a_struct_inside_a_typedef_keeps_its_name() {
    let src = "typedef struct Cart {\n    int n;\n} Cart;\n";
    assert_eq!(
        symbols("c/cart.h", lang::C, src),
        [
            sym("c.cart", kind::MODULE, 1, 3),
            sym("c.cart.Cart", kind::TYPE, 1, 3),
            sym("c.cart.Cart", kind::CLASS, 1, 3),
        ]
    );
}

#[test]
fn an_unknown_language_is_an_error() {
    assert!(parse_generic("a.x", "svc", b"", "h", "cobol").is_err());
    assert!(parse_generic("a.go", "svc", b"", "h", lang::GO).is_ok());
}
