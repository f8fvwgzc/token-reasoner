//! Parses every file of `tests/fixtures/languages` (one directory per language
//! read through a grammar and a tags query) and resolves each language on its
//! own: symbols with their kinds and line ranges, a bare call and a member call
//! with its receiver, a call to a function of the same file that is `static`,
//! and a call into another file of the same language that is `inferred`.

use lge_core::{fact, kind, method, EdgeRec, EdgeType, ParsedFile, ResolveInput, Resolved, SymbolRec};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// What one language directory must give.
struct Case {
    lang: &'static str,
    /// (qualified name, kind, first line, last line).
    symbols: &'static [(&'static str, &'static str, i64, i64)],
    /// (caller, called name): a call with no receiver.
    bare: (&'static str, &'static str),
    /// (caller, called name, receiver). `None` for a language with no member calls.
    member: Option<(&'static str, &'static str, &'static str)>,
    /// (caller, callee): a call to a function defined in the same file, static 0.95.
    same_file: (&'static str, &'static str),
    /// (caller, callee): a call to a function of another file, inferred.
    other_file: (&'static str, &'static str),
    /// (caller, callee): a method calling a method of its own class (through
    /// `self`, `this` or an implicit receiver), static 0.95.
    own_member: Option<(&'static str, &'static str)>,
    /// Symbols that are tests.
    tests: &'static [&'static str],
}

const C: &str = kind::CLASS;
const F: &str = kind::FUNCTION;
const M: &str = kind::METHOD;
const I: &str = kind::INTERFACE;
const MOD: &str = kind::MODULE;
const T: &str = kind::TYPE;

const CASES: &[Case] = &[
    Case {
        lang: "bash",
        symbols: &[
            ("bash.checkout.subtotal", F, 6, 12),
            ("bash.checkout.apply_tax", F, 15, 17),
            ("bash.checkout.checkout", F, 20, 24),
            ("bash.money.format_money", F, 4, 6),
        ],
        bare: ("bash.checkout.checkout", "apply_tax"),
        member: None,
        same_file: ("bash.checkout.checkout", "bash.checkout.apply_tax"),
        other_file: ("bash.checkout.checkout", "bash.money.format_money"),
        own_member: None,
        tests: &[],
    },
    Case {
        lang: "c",
        symbols: &[
            ("c.cart.Cart", C, 7, 11),
            ("c.cart.subtotal", F, 14, 20),
            ("c.cart.apply_tax", F, 23, 25),
            ("c.cart.checkout", F, 28, 31),
            ("c.money.format_money", F, 5, 7),
        ],
        bare: ("c.cart.checkout", "apply_tax"),
        member: Some(("c.cart.checkout", "total", "cart")),
        same_file: ("c.cart.checkout", "c.cart.apply_tax"),
        other_file: ("c.cart.checkout", "c.money.format_money"),
        own_member: None,
        tests: &[],
    },
    Case {
        lang: "cpp",
        symbols: &[
            ("cpp.cart.shop", MOD, 8, 40),
            ("cpp.cart.shop.Cart", C, 11, 23),
            ("cpp.cart.shop.Cart.subtotal", M, 14, 16),
            ("cpp.cart.shop.apply_tax", F, 26, 28),
            ("cpp.cart.shop.Cart.total", M, 30, 32),
            ("cpp.cart.shop.checkout", F, 35, 38),
            ("cpp.money.format_money", F, 6, 10),
        ],
        bare: ("cpp.cart.shop.checkout", "apply_tax"),
        member: Some(("cpp.cart.shop.checkout", "total", "cart")),
        same_file: ("cpp.cart.shop.checkout", "cpp.cart.shop.apply_tax"),
        other_file: ("cpp.cart.shop.checkout", "cpp.money.format_money"),
        own_member: Some(("cpp.cart.shop.Cart.total", "cpp.cart.shop.Cart.subtotal")),
        tests: &[],
    },
    Case {
        lang: "csharp",
        symbols: &[
            ("csharp.Cart.Shop", MOD, 4, 44),
            ("csharp.Cart.Shop.Cart", C, 7, 22),
            ("csharp.Cart.Shop.Cart.Subtotal", M, 12, 15),
            ("csharp.Cart.Shop.Cart.Total", M, 18, 21),
            ("csharp.Cart.Shop.IPricer", I, 24, 27),
            ("csharp.Cart.Shop.Checkout", C, 29, 43),
            ("csharp.Cart.Shop.Checkout.ApplyTax", M, 32, 35),
            ("csharp.Cart.Shop.Checkout.Run", M, 38, 42),
            ("csharp.Money.Shop.Money.FormatMoney", M, 6, 9),
        ],
        bare: ("csharp.Cart.Shop.Checkout.Run", "ApplyTax"),
        member: Some(("csharp.Cart.Shop.Checkout.Run", "Total", "cart")),
        same_file: ("csharp.Cart.Shop.Checkout.Run", "csharp.Cart.Shop.Checkout.ApplyTax"),
        other_file: ("csharp.Cart.Shop.Checkout.Run", "csharp.Money.Shop.Money.FormatMoney"),
        own_member: Some(("csharp.Cart.Shop.Cart.Total", "csharp.Cart.Shop.Cart.Subtotal")),
        tests: &[],
    },
    Case {
        lang: "cuda",
        symbols: &[
            ("cuda.cart.Cart", C, 7, 22),
            ("cuda.cart.Cart.subtotal", M, 12, 18),
            ("cuda.cart.apply_tax_kernel", F, 25, 30),
            ("cuda.cart.apply_tax", F, 33, 35),
            ("cuda.cart.Cart.total", M, 37, 39),
            ("cuda.cart.checkout", F, 42, 45),
            ("cuda.money.format_money", F, 4, 6),
        ],
        bare: ("cuda.cart.checkout", "apply_tax"),
        member: Some(("cuda.cart.checkout", "total", "cart")),
        same_file: ("cuda.cart.apply_tax_kernel", "cuda.cart.apply_tax"),
        other_file: ("cuda.cart.checkout", "cuda.money.format_money"),
        own_member: Some(("cuda.cart.Cart.total", "cuda.cart.Cart.subtotal")),
        tests: &[],
    },
    Case {
        lang: "dart",
        symbols: &[
            ("dart.cart.Cart", C, 4, 18),
            ("dart.cart.Cart.subtotal", M, 10, 12),
            ("dart.cart.Cart.total", M, 15, 17),
            ("dart.cart.applyTax", F, 21, 23),
            ("dart.cart.checkout", F, 26, 29),
            ("dart.money.formatMoney", F, 2, 5),
        ],
        bare: ("dart.cart.checkout", "applyTax"),
        member: Some(("dart.cart.checkout", "total", "cart")),
        same_file: ("dart.cart.checkout", "dart.cart.applyTax"),
        other_file: ("dart.cart.checkout", "dart.money.formatMoney"),
        own_member: Some(("dart.cart.Cart.total", "dart.cart.Cart.subtotal")),
        tests: &[],
    },
    Case {
        lang: "elixir",
        symbols: &[
            ("elixir.cart.Shop.Cart", MOD, 1, 26),
            ("elixir.cart.Shop.Cart.subtotal", F, 7, 9),
            ("elixir.cart.Shop.Cart.total", F, 12, 14),
            ("elixir.cart.Shop.Cart.apply_tax", F, 17, 19),
            ("elixir.cart.Shop.Cart.checkout", F, 22, 25),
            ("elixir.money.Shop.Money.format_money", F, 5, 7),
        ],
        bare: ("elixir.cart.Shop.Cart.checkout", "apply_tax"),
        member: Some(("elixir.cart.Shop.Cart.checkout", "format_money", "Shop.Money")),
        same_file: ("elixir.cart.Shop.Cart.checkout", "elixir.cart.Shop.Cart.apply_tax"),
        other_file: ("elixir.cart.Shop.Cart.checkout", "elixir.money.Shop.Money.format_money"),
        own_member: None,
        tests: &["elixir.cart_test", "elixir.cart_test.Shop.CartTest"],
    },
    Case {
        lang: "elm",
        symbols: &[
            ("elm.Cart.Cart", C, 8, 9),
            ("elm.Cart.subtotal", F, 15, 16),
            ("elm.Cart.total", F, 22, 23),
            ("elm.Cart.applyTax", F, 29, 30),
            ("elm.Cart.checkout", F, 36, 37),
            ("elm.Money.formatMoney", F, 7, 8),
        ],
        bare: ("elm.Cart.checkout", "applyTax"),
        member: Some(("elm.Cart.subtotal", "sum", "List")),
        same_file: ("elm.Cart.checkout", "elm.Cart.applyTax"),
        other_file: ("elm.Cart.checkout", "elm.Money.formatMoney"),
        own_member: None,
        tests: &[],
    },
    Case {
        lang: "erlang",
        symbols: &[
            ("erlang.cart.cart", C, 6, 6),
            ("erlang.cart.subtotal", F, 9, 10),
            ("erlang.cart.total", F, 13, 14),
            ("erlang.cart.apply_tax", F, 17, 18),
            ("erlang.cart.checkout", F, 21, 23),
            ("erlang.money.format_money", F, 6, 7),
        ],
        bare: ("erlang.cart.checkout", "apply_tax"),
        member: Some(("erlang.cart.checkout", "format_money", "money")),
        same_file: ("erlang.cart.checkout", "erlang.cart.apply_tax"),
        other_file: ("erlang.cart.checkout", "erlang.money.format_money"),
        own_member: None,
        tests: &[],
    },
    Case {
        lang: "fsharp",
        symbols: &[
            ("fsharp.Cart.Shop.Cart", MOD, 2, 20),
            ("fsharp.Cart.Shop.Cart.applyTax", F, 7, 7),
            ("fsharp.Cart.Shop.Cart.Cart", C, 10, 15),
            ("fsharp.Cart.Shop.Cart.Cart.Subtotal", M, 12, 12),
            ("fsharp.Cart.Shop.Cart.Cart.Total", M, 15, 15),
            ("fsharp.Cart.Shop.Cart.checkout", F, 18, 20),
            ("fsharp.Money.Shop.Money.formatMoney", F, 5, 5),
        ],
        bare: ("fsharp.Cart.Shop.Cart.checkout", "applyTax"),
        member: Some(("fsharp.Cart.Shop.Cart.checkout", "Total", "cart")),
        same_file: ("fsharp.Cart.Shop.Cart.checkout", "fsharp.Cart.Shop.Cart.applyTax"),
        other_file: ("fsharp.Cart.Shop.Cart.checkout", "fsharp.Money.Shop.Money.formatMoney"),
        own_member: Some((
            "fsharp.Cart.Shop.Cart.Cart.Total",
            "fsharp.Cart.Shop.Cart.Cart.Subtotal",
        )),
        tests: &[],
    },
    Case {
        lang: "gleam",
        symbols: &[
            ("gleam.cart.Cart", C, 8, 10),
            ("gleam.cart.subtotal", F, 13, 15),
            ("gleam.cart.apply_tax", F, 18, 20),
            ("gleam.cart.total", F, 23, 25),
            ("gleam.cart.checkout", F, 28, 31),
            ("gleam.money.format_money", F, 5, 7),
        ],
        bare: ("gleam.cart.checkout", "apply_tax"),
        member: Some(("gleam.cart.checkout", "format_money", "money")),
        same_file: ("gleam.cart.checkout", "gleam.cart.apply_tax"),
        other_file: ("gleam.cart.checkout", "gleam.money.format_money"),
        own_member: None,
        tests: &[],
    },
    Case {
        lang: "go",
        symbols: &[
            ("go.cart.Cart", C, 5, 7),
            ("go.cart.Pricer", I, 10, 12),
            ("go.cart.Cart.Subtotal", M, 15, 21),
            ("go.cart.Cart.Total", M, 24, 26),
            ("go.cart.ApplyTax", F, 29, 31),
            ("go.cart.Checkout", F, 34, 37),
            ("go.money.FormatMoney", F, 6, 8),
        ],
        bare: ("go.cart.Checkout", "ApplyTax"),
        member: Some(("go.cart.Checkout", "Total", "cart")),
        same_file: ("go.cart.Checkout", "go.cart.ApplyTax"),
        other_file: ("go.cart.Checkout", "go.money.FormatMoney"),
        own_member: Some(("go.cart.Cart.Total", "go.cart.Cart.Subtotal")),
        tests: &["go.cart_test", "go.cart_test.TestCheckout"],
    },
    Case {
        lang: "groovy",
        symbols: &[
            ("groovy.Cart.Cart", C, 4, 16),
            ("groovy.Cart.Cart.subtotal", M, 8, 10),
            ("groovy.Cart.Cart.total", M, 13, 15),
            ("groovy.Cart.Checkout", C, 19, 30),
            ("groovy.Cart.Checkout.applyTax", M, 21, 23),
            ("groovy.Cart.Checkout.checkout", M, 26, 29),
            ("groovy.Money.Money.formatMoney", M, 5, 7),
        ],
        bare: ("groovy.Cart.Checkout.checkout", "applyTax"),
        member: Some(("groovy.Cart.Checkout.checkout", "total", "cart")),
        same_file: ("groovy.Cart.Checkout.checkout", "groovy.Cart.Checkout.applyTax"),
        other_file: ("groovy.Cart.Checkout.checkout", "groovy.Money.Money.formatMoney"),
        own_member: Some(("groovy.Cart.Cart.total", "groovy.Cart.Cart.subtotal")),
        tests: &[],
    },
    Case {
        lang: "haskell",
        symbols: &[
            ("haskell.Cart.Cart", C, 8, 8),
            ("haskell.Cart.Priced", I, 11, 12),
            // An instance method belongs to the type of its instance.
            ("haskell.Cart.Cart.total", M, 15, 15),
            ("haskell.Cart.subtotal", F, 19, 19),
            ("haskell.Cart.applyTax", F, 23, 23),
            ("haskell.Cart.checkout", F, 27, 27),
            ("haskell.Money.formatMoney", F, 8, 8),
        ],
        bare: ("haskell.Cart.checkout", "applyTax"),
        member: Some(("haskell.Cart.subtotal", "foldl'", "List")),
        same_file: ("haskell.Cart.checkout", "haskell.Cart.applyTax"),
        other_file: ("haskell.Cart.checkout", "haskell.Money.formatMoney"),
        own_member: None,
        tests: &[],
    },
    Case {
        lang: "java",
        symbols: &[
            ("java.Checkout.Cart", C, 7, 23),
            ("java.Checkout.Cart.subtotal", M, 11, 17),
            ("java.Checkout.Cart.total", M, 20, 22),
            ("java.Checkout.Checkout", C, 26, 37),
            ("java.Checkout.Checkout.applyTax", M, 28, 30),
            ("java.Checkout.Checkout.checkout", M, 33, 36),
            ("java.Money.Money.formatMoney", M, 6, 8),
        ],
        bare: ("java.Checkout.Checkout.checkout", "applyTax"),
        member: Some(("java.Checkout.Checkout.checkout", "total", "cart")),
        same_file: ("java.Checkout.Checkout.checkout", "java.Checkout.Checkout.applyTax"),
        other_file: ("java.Checkout.Checkout.checkout", "java.Money.Money.formatMoney"),
        own_member: Some(("java.Checkout.Cart.total", "java.Checkout.Cart.subtotal")),
        tests: &[
            "java.CheckoutTest.CheckoutTest",
            "java.CheckoutTest.CheckoutTest.pricesAnEmptyCart",
        ],
    },
    Case {
        lang: "julia",
        symbols: &[
            ("julia.cart.Shop", MOD, 2, 30),
            ("julia.cart.Shop.Cart", C, 7, 9),
            ("julia.cart.Shop.subtotal", F, 12, 14),
            ("julia.cart.Shop.apply_tax", F, 17, 19),
            ("julia.cart.Shop.total", F, 22, 22),
            ("julia.cart.Shop.checkout", F, 25, 28),
            ("julia.money.format_money", F, 4, 6),
        ],
        bare: ("julia.cart.Shop.checkout", "apply_tax"),
        member: Some(("julia.cart.Shop.subtotal", "sum", "Base")),
        same_file: ("julia.cart.Shop.checkout", "julia.cart.Shop.apply_tax"),
        other_file: ("julia.cart.Shop.checkout", "julia.money.format_money"),
        own_member: None,
        tests: &[],
    },
    Case {
        lang: "kotlin",
        symbols: &[
            ("kotlin.Cart.Cart", C, 4, 12),
            ("kotlin.Cart.Cart.subtotal", M, 6, 6),
            ("kotlin.Cart.Cart.total", M, 9, 11),
            ("kotlin.Cart.applyTax", F, 15, 15),
            ("kotlin.Cart.checkout", F, 18, 21),
            ("kotlin.Money.formatMoney", F, 4, 4),
        ],
        bare: ("kotlin.Cart.checkout", "applyTax"),
        member: Some(("kotlin.Cart.checkout", "total", "cart")),
        same_file: ("kotlin.Cart.checkout", "kotlin.Cart.applyTax"),
        other_file: ("kotlin.Cart.checkout", "kotlin.Money.formatMoney"),
        own_member: Some(("kotlin.Cart.Cart.total", "kotlin.Cart.Cart.subtotal")),
        tests: &["kotlin.CartTest.CartTest", "kotlin.CartTest.CartTest.pricesTheItems"],
    },
    Case {
        lang: "lua",
        symbols: &[
            ("lua.cart.Cart.subtotal", M, 9, 15),
            ("lua.cart.apply_tax", F, 18, 20),
            ("lua.cart.Cart.total", M, 23, 25),
            ("lua.cart.checkout", F, 28, 31),
            ("lua.money.M.format_money", M, 5, 7),
        ],
        bare: ("lua.cart.checkout", "apply_tax"),
        member: Some(("lua.cart.checkout", "total", "cart")),
        same_file: ("lua.cart.checkout", "lua.cart.apply_tax"),
        other_file: ("lua.cart.checkout", "lua.money.M.format_money"),
        own_member: None,
        tests: &[],
    },
    Case {
        lang: "luau",
        symbols: &[
            ("luau.cart.Cart.subtotal", M, 12, 18),
            ("luau.cart.applyTax", F, 21, 23),
            ("luau.cart.Cart.total", M, 26, 28),
            ("luau.cart.checkout", F, 31, 34),
            ("luau.money.M.formatMoney", M, 5, 7),
        ],
        bare: ("luau.cart.checkout", "applyTax"),
        member: Some(("luau.cart.checkout", "total", "cart")),
        same_file: ("luau.cart.checkout", "luau.cart.applyTax"),
        other_file: ("luau.cart.checkout", "luau.money.M.formatMoney"),
        own_member: None,
        tests: &[],
    },
    Case {
        lang: "nix",
        symbols: &[
            ("nix.cart.subtotal", F, 6, 6),
            ("nix.cart.applyTax", F, 9, 9),
            ("nix.cart.total", F, 12, 12),
            ("nix.cart.checkout", F, 18, 18),
            ("nix.money.formatMoney", F, 6, 6),
        ],
        bare: ("nix.cart.checkout", "applyTax"),
        member: Some(("nix.cart.checkout", "formatMoney", "money")),
        same_file: ("nix.cart.checkout", "nix.cart.applyTax"),
        other_file: ("nix.cart.checkout", "nix.money.formatMoney"),
        own_member: None,
        tests: &[],
    },
    Case {
        lang: "objc",
        symbols: &[
            ("objc.Cart.Cart", C, 7, 11),
            ("objc.Cart.ApplyTax", F, 14, 16),
            ("objc.Cart.Cart.subtotal", M, 21, 27),
            ("objc.Cart.Cart.total", M, 30, 32),
            ("objc.Cart.Checkout", F, 37, 40),
            ("objc.Money.FormatMoney", F, 4, 6),
        ],
        bare: ("objc.Cart.Checkout", "ApplyTax"),
        member: Some(("objc.Cart.Checkout", "total", "cart")),
        same_file: ("objc.Cart.Checkout", "objc.Cart.ApplyTax"),
        other_file: ("objc.Cart.Checkout", "objc.Money.FormatMoney"),
        own_member: Some(("objc.Cart.Cart.total", "objc.Cart.Cart.subtotal")),
        tests: &[],
    },
    Case {
        lang: "ocaml",
        symbols: &[
            ("ocaml.cart.cart", T, 4, 4),
            ("ocaml.cart.apply_tax", F, 7, 7),
            ("ocaml.cart.Cart", MOD, 9, 15),
            ("ocaml.cart.Cart.subtotal", F, 11, 11),
            ("ocaml.cart.Cart.total", F, 14, 14),
            ("ocaml.cart.checkout", F, 18, 20),
            ("ocaml.money.format_money", F, 4, 4),
        ],
        bare: ("ocaml.cart.checkout", "apply_tax"),
        member: Some(("ocaml.cart.checkout", "total", "Cart")),
        same_file: ("ocaml.cart.checkout", "ocaml.cart.apply_tax"),
        other_file: ("ocaml.cart.checkout", "ocaml.money.format_money"),
        own_member: None,
        tests: &[],
    },
    Case {
        lang: "php",
        symbols: &[
            ("php.Cart.Cart", C, 6, 21),
            ("php.Cart.Cart.subtotal", M, 11, 14),
            ("php.Cart.Cart.total", M, 17, 20),
            ("php.Cart.apply_tax", F, 24, 27),
            ("php.Cart.checkout", F, 30, 34),
            ("php.money.format_money", F, 4, 7),
        ],
        bare: ("php.Cart.checkout", "apply_tax"),
        member: Some(("php.Cart.checkout", "total", "$cart")),
        same_file: ("php.Cart.checkout", "php.Cart.apply_tax"),
        other_file: ("php.Cart.checkout", "php.money.format_money"),
        own_member: Some(("php.Cart.Cart.total", "php.Cart.Cart.subtotal")),
        tests: &[],
    },
    Case {
        lang: "powershell",
        symbols: &[
            ("powershell.Cart.Cart", C, 5, 17),
            ("powershell.Cart.Cart.Subtotal", M, 9, 11),
            ("powershell.Cart.Cart.Total", M, 14, 16),
            ("powershell.Cart.Get-Tax", F, 20, 22),
            ("powershell.Cart.Invoke-Checkout", F, 25, 28),
            ("powershell.Money.Format-Money", F, 2, 4),
        ],
        bare: ("powershell.Cart.Invoke-Checkout", "Get-Tax"),
        member: Some(("powershell.Cart.Invoke-Checkout", "Total", "$Cart")),
        same_file: ("powershell.Cart.Invoke-Checkout", "powershell.Cart.Get-Tax"),
        other_file: ("powershell.Cart.Invoke-Checkout", "powershell.Money.Format-Money"),
        own_member: None,
        tests: &[],
    },
    Case {
        lang: "r",
        symbols: &[
            ("r.cart.Cart", C, 5, 15),
            ("r.cart.Cart.subtotal", M, 8, 10),
            ("r.cart.Cart.total", M, 11, 13),
            ("r.cart.apply_tax", F, 18, 20),
            ("r.cart.checkout", F, 23, 26),
            ("r.money.format_money", F, 4, 6),
        ],
        bare: ("r.cart.checkout", "apply_tax"),
        member: Some(("r.cart.checkout", "total", "cart")),
        same_file: ("r.cart.checkout", "r.cart.apply_tax"),
        other_file: ("r.cart.checkout", "r.money.format_money"),
        own_member: Some(("r.cart.Cart.total", "r.cart.Cart.subtotal")),
        tests: &[],
    },
    Case {
        lang: "ruby",
        symbols: &[
            ("ruby.cart.Cart", C, 4, 18),
            ("ruby.cart.Cart.subtotal", M, 10, 12),
            ("ruby.cart.Cart.total", M, 15, 17),
            ("ruby.cart.apply_tax", F, 21, 23),
            ("ruby.cart.checkout", F, 26, 29),
            ("ruby.money.format_money", F, 4, 6),
        ],
        bare: ("ruby.cart.checkout", "apply_tax"),
        member: Some(("ruby.cart.checkout", "total", "cart")),
        same_file: ("ruby.cart.checkout", "ruby.cart.apply_tax"),
        other_file: ("ruby.cart.checkout", "ruby.money.format_money"),
        own_member: Some(("ruby.cart.Cart.total", "ruby.cart.Cart.subtotal")),
        tests: &["ruby.cart_test.CartTest", "ruby.cart_test.CartTest.test_checkout"],
    },
    Case {
        lang: "rust",
        symbols: &[
            ("rust.cart.Cart", C, 6, 8),
            ("rust.cart.Cart.new", M, 12, 14),
            ("rust.cart.Cart.subtotal", M, 17, 19),
            ("rust.cart.Cart.total", M, 22, 24),
            ("rust.cart.apply_tax", F, 28, 30),
            ("rust.cart.checkout", F, 33, 36),
            ("rust.money.format_money", F, 4, 6),
        ],
        bare: ("rust.cart.checkout", "apply_tax"),
        member: Some(("rust.cart.checkout", "total", "cart")),
        same_file: ("rust.cart.checkout", "rust.cart.apply_tax"),
        other_file: ("rust.cart.checkout", "rust.money.format_money"),
        own_member: Some(("rust.cart.Cart.total", "rust.cart.Cart.subtotal")),
        // `mod tests` and `fn test_checkout` are tests by their names.
        tests: &["rust.cart.tests", "rust.cart.tests.test_checkout"],
    },
    Case {
        lang: "scala",
        symbols: &[
            ("scala.Cart.Cart", C, 4, 10),
            ("scala.Cart.Cart.subtotal", M, 6, 6),
            ("scala.Cart.Cart.total", M, 9, 9),
            ("scala.Cart.Checkout", C, 13, 22),
            ("scala.Cart.Checkout.applyTax", M, 15, 15),
            ("scala.Cart.Checkout.checkout", M, 18, 21),
            ("scala.Money.Money.formatMoney", M, 5, 5),
        ],
        bare: ("scala.Cart.Checkout.checkout", "applyTax"),
        member: Some(("scala.Cart.Checkout.checkout", "total", "cart")),
        same_file: ("scala.Cart.Checkout.checkout", "scala.Cart.Checkout.applyTax"),
        other_file: ("scala.Cart.Checkout.checkout", "scala.Money.Money.formatMoney"),
        own_member: Some(("scala.Cart.Cart.total", "scala.Cart.Cart.subtotal")),
        tests: &[],
    },
    Case {
        lang: "solidity",
        symbols: &[
            ("solidity.Cart.Cart", C, 7, 21),
            ("solidity.Cart.Cart.subtotal", M, 11, 15),
            ("solidity.Cart.Cart.total", M, 18, 20),
            ("solidity.Cart.applyTax", F, 24, 26),
            ("solidity.Cart.checkout", F, 29, 32),
            ("solidity.Money.formatMoney", F, 7, 9),
        ],
        bare: ("solidity.Cart.checkout", "applyTax"),
        member: Some(("solidity.Cart.checkout", "total", "cart")),
        same_file: ("solidity.Cart.checkout", "solidity.Cart.applyTax"),
        other_file: ("solidity.Cart.checkout", "solidity.Money.formatMoney"),
        own_member: Some(("solidity.Cart.Cart.total", "solidity.Cart.Cart.subtotal")),
        tests: &[],
    },
    Case {
        lang: "swift",
        symbols: &[
            ("swift.Cart.Cart", C, 4, 16),
            ("swift.Cart.Cart.subtotal", M, 8, 10),
            ("swift.Cart.Cart.total", M, 13, 15),
            ("swift.Cart.Priced", I, 19, 21),
            ("swift.Cart.applyTax", F, 24, 26),
            ("swift.Cart.checkout", F, 29, 32),
            ("swift.Money.formatMoney", F, 4, 6),
        ],
        bare: ("swift.Cart.checkout", "applyTax"),
        member: Some(("swift.Cart.checkout", "total", "cart")),
        same_file: ("swift.Cart.checkout", "swift.Cart.applyTax"),
        other_file: ("swift.Cart.checkout", "swift.Money.formatMoney"),
        own_member: Some(("swift.Cart.Cart.total", "swift.Cart.Cart.subtotal")),
        tests: &[],
    },
    Case {
        lang: "zig",
        symbols: &[
            ("zig.cart.Cart", C, 6, 22),
            ("zig.cart.Cart.subtotal", M, 10, 16),
            ("zig.cart.Cart.total", M, 19, 21),
            ("zig.cart.applyTax", F, 25, 27),
            ("zig.cart.checkout", F, 30, 33),
            ("zig.money.formatMoney", F, 4, 6),
        ],
        bare: ("zig.cart.checkout", "applyTax"),
        member: Some(("zig.cart.checkout", "total", "cart")),
        same_file: ("zig.cart.checkout", "zig.cart.applyTax"),
        other_file: ("zig.cart.checkout", "zig.money.formatMoney"),
        own_member: Some(("zig.cart.Cart.total", "zig.cart.Cart.subtotal")),
        tests: &[],
    },
];

/// Every fixture file, parsed, by language directory (the service).
fn parsed() -> BTreeMap<String, Vec<ParsedFile>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/languages");
    let files = lge_index::scan(&root, 1).unwrap();
    for f in &files {
        assert_eq!(f.lang, f.service, "{}: the directory names the language", f.rel_path);
    }
    let mut out: BTreeMap<String, Vec<ParsedFile>> = BTreeMap::new();
    for (path, result) in lge_index::parse_many(&files) {
        let p = result.unwrap_or_else(|e| panic!("{path}: {e:#}"));
        let service = p.file.as_ref().map(|f| f.service.clone()).unwrap_or_default();
        out.entry(service).or_default().push(p);
    }
    out
}

fn resolve(files: &[ParsedFile]) -> (ResolveInput, Resolved) {
    let mut input = ResolveInput::default();
    for p in files {
        input.files.extend(p.file.clone());
        input.symbols.extend(p.symbols.iter().cloned());
        input.calls.extend(p.calls.iter().cloned());
        input.imports.extend(p.imports.iter().cloned());
        input.facts.extend(p.facts.iter().cloned());
    }
    let resolved = lge_resolve::resolve(&input);
    (input, resolved)
}

fn symbol<'a>(input: &'a ResolveInput, qualified_name: &str) -> &'a SymbolRec {
    let found: Vec<&SymbolRec> = input
        .symbols
        .iter()
        .filter(|s| s.qualified_name == qualified_name)
        .collect();
    match found.as_slice() {
        [one] => one,
        _ => panic!("{} symbols named {qualified_name}", found.len()),
    }
}

fn call_edge<'r>(res: &'r Resolved, input: &ResolveInput, from: &str, to: &str) -> &'r EdgeRec {
    let (from_id, to_id) = (&symbol(input, from).id, &symbol(input, to).id);
    res.edges
        .iter()
        .find(|e| e.edge_type == EdgeType::Calls && e.from == *from_id && e.to == *to_id)
        .unwrap_or_else(|| panic!("no call {from} -> {to}"))
}

#[test]
fn every_language_directory_has_a_case() {
    let dirs: BTreeSet<String> = parsed().into_keys().collect();
    let cases: BTreeSet<String> = CASES.iter().map(|c| c.lang.to_string()).collect();
    assert_eq!(dirs, cases);
    assert!(CASES.len() >= 25, "{} languages", CASES.len());
}

#[test]
fn symbols_have_their_kind_and_lines() {
    let parsed = parsed();
    for case in CASES {
        let (input, _) = resolve(&parsed[case.lang]);
        for (qn, kind, start, end) in case.symbols {
            let s = symbol(&input, qn);
            assert_eq!(
                (s.kind.as_str(), s.start_line, s.end_line),
                (*kind, *start, *end),
                "{}: {qn}",
                case.lang
            );
            assert!(!s.signature.is_empty(), "{qn}: signature");
            assert!(!s.body.is_empty(), "{qn}: body");
        }
        // Every file has a module symbol named after its path.
        for p in &parsed[case.lang] {
            let file = p.file.as_ref().unwrap();
            let module = &p.symbols[0];
            assert_eq!(module.kind, kind::MODULE, "{}", file.path);
            assert_eq!(
                module.qualified_name,
                file.path.rsplit_once('.').unwrap().0.replace('/', ".")
            );
            assert!(p.imports.is_empty(), "{}: no import sites", file.path);
        }
    }
}

#[test]
fn definitions_carry_their_doc_line() {
    let parsed = parsed();
    for case in CASES {
        let (input, _) = resolve(&parsed[case.lang]);
        let (_, callee) = case.same_file;
        // Go's convention starts the doc with the name: "ApplyTax adds ...".
        let doc = &symbol(&input, callee).doc;
        assert!(
            doc.ends_with("dds the sales tax to an amount."),
            "{}: {callee}: {doc:?}",
            case.lang
        );
    }
}

#[test]
fn calls_have_their_receiver() {
    let parsed = parsed();
    for case in CASES {
        let (input, _) = resolve(&parsed[case.lang]);
        let calls_of = |caller: &str| -> Vec<(String, String)> {
            let id = &symbol(&input, caller).id;
            input
                .calls
                .iter()
                .filter(|c| c.caller_id == *id)
                .map(|c| (c.name.clone(), c.receiver.clone()))
                .collect()
        };
        let (caller, name) = case.bare;
        assert!(
            calls_of(caller).contains(&(name.to_string(), String::new())),
            "{}: bare {name} in {caller}: {:?}",
            case.lang,
            calls_of(caller)
        );
        if let Some((caller, name, receiver)) = case.member {
            assert!(
                calls_of(caller).contains(&(name.to_string(), receiver.to_string())),
                "{}: {receiver}.{name} in {caller}: {:?}",
                case.lang,
                calls_of(caller)
            );
        }
        for c in &input.calls {
            assert!(input.symbols.iter().any(|s| s.id == c.caller_id), "{c:?}");
        }
    }
}

#[test]
fn a_same_file_call_is_static_and_a_call_into_another_file_is_inferred() {
    let parsed = parsed();
    for case in CASES {
        let (input, res) = resolve(&parsed[case.lang]);
        let (from, to) = case.same_file;
        let e = call_edge(&res, &input, from, to);
        assert_eq!(
            (e.method.as_str(), e.confidence),
            (method::STATIC, 0.95),
            "{}",
            case.lang
        );
        let (from, to) = case.other_file;
        let e = call_edge(&res, &input, from, to);
        assert_eq!(e.method, method::INFERRED, "{}: {from} -> {to}", case.lang);
        if let Some((from, to)) = case.own_member {
            let e = call_edge(&res, &input, from, to);
            assert_eq!(
                (e.method.as_str(), e.confidence),
                (method::STATIC, 0.95),
                "{}",
                case.lang
            );
        }
        // Nothing crosses files as static: these languages have no import sites.
        for e in res.edges.iter().filter(|e| e.method == method::STATIC) {
            let file = |id: &str| input.symbols.iter().find(|s| s.id == id).map(|s| s.file_path.clone());
            assert_eq!(file(&e.from), file(&e.to), "{}: {e:?}", case.lang);
        }
    }
}

#[test]
fn test_files_and_test_functions_are_tests() {
    let parsed = parsed();
    let mut languages_with_tests = 0;
    for case in CASES {
        let (input, _) = resolve(&parsed[case.lang]);
        for qn in case.tests {
            assert!(symbol(&input, qn).is_test, "{qn}");
        }
        for (qn, ..) in case.symbols {
            assert!(!symbol(&input, qn).is_test, "{qn}");
        }
        languages_with_tests += usize::from(!case.tests.is_empty());
    }
    assert!(languages_with_tests >= 3);
}

#[test]
fn rationale_comments_use_the_language_comment_token() {
    let parsed = parsed();
    let expected = [
        (
            "go",
            "go.cart.ApplyTax",
            "HACK: the rate is fixed until tax tables exist",
        ),
        ("ruby", "ruby.cart.apply_tax", "TODO: read the rate from the store"),
        ("lua", "lua.cart.apply_tax", "NOTE: integer division keeps whole cents"),
        ("haskell", "haskell.Cart.applyTax", "WHY: whole cents only"),
        (
            "erlang",
            "erlang.cart.apply_tax",
            "NOTE: integer division keeps whole cents",
        ),
        (
            "ocaml",
            "ocaml.cart.apply_tax",
            "NOTE: integer division keeps whole cents",
        ),
    ];
    for (lang, qn, note) in expected {
        let (input, _) = resolve(&parsed[lang]);
        let owner = &symbol(&input, qn).id;
        let notes: Vec<&str> = input
            .facts
            .iter()
            .filter(|f| f.kind == fact::RATIONALE)
            .map(|f| {
                assert_eq!(f.symbol_id, *owner, "{lang}");
                f.key.as_str()
            })
            .collect();
        assert_eq!(notes, [note], "{lang}");
    }
}
