//! The table of languages read through a grammar and a tags query.
//!
//! A query is the grammar crate's own `TAGS_QUERY` when it has one that works,
//! with patterns of ours in front when it misses something (a pattern earlier in
//! a query wins when two tag the same name). Otherwise it is a query of ours,
//! kept as a `.scm` file under `queries/`. Each grammar here builds with the
//! workspace's tree-sitter and is parsed by a test.

use lge_core::lang;
use tree_sitter_language::LanguageFn;

/// Where a language's tags query comes from.
pub(crate) enum Query {
    /// The grammar crate's own query, with `ours` in front of it.
    Crate { ours: &'static str, theirs: &'static str },
    /// The crate has no query, or one that cannot be used as it is.
    Own(&'static str),
}

/// How a call writes its receiver besides `recv.f()`, `recv->f()` and `Recv::f()`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CallSyntax {
    /// `obj:method()` (Lua) and `module:fun()` (Erlang).
    pub colon: bool,
    /// `obj$method()` (R).
    pub dollar: bool,
    /// `[receiver message]` (Objective-C).
    pub message: bool,
    /// `obj#method` (OCaml).
    pub hash: bool,
    /// `ns/f` (Clojure).
    pub slash: bool,
}

/// What a name written before a definition's own name is: `Cart:total` (Lua),
/// `Cart::total` (C++) and `M.helper` (Lua) name an owner; F#'s `this.Total`
/// names the variable that stands for the instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Qualifier {
    Owner,
    SelfVariable,
}

pub(crate) struct Grammar {
    /// One of `lge_core::lang`.
    pub lang: &'static str,
    /// File extensions, without the dot.
    pub extensions: &'static [&'static str],
    pub language: LanguageFn,
    pub query: Query,
    /// The locals query of a tags query that skips local variables (`#is-not? local`).
    pub locals: &'static str,
    /// The line comment token: for rationale comments and leading doc comments.
    pub comment: &'static str,
    /// A second line comment token (PHP's `#`), for rationale comments.
    pub alt_comment: Option<&'static str>,
    pub calls: CallSyntax,
    pub qualifier: Qualifier,
    /// A function may be written as several clauses (Elixir, Erlang, Haskell):
    /// clauses of one name that follow each other are one definition.
    pub clauses: bool,
    /// Names are compared without case (PowerShell commands).
    pub case_insensitive: bool,
    /// Names the query tags as calls that are operators (OCaml's `mod`).
    pub not_calls: &'static [&'static str],
    /// The grammar needs a `;` where the language lets a newline end a
    /// statement (Groovy): the file is parsed with them written out.
    pub semicolons: bool,
    /// `var.name` without parentheses on a variable reads a field (Elixir): a
    /// remote call names a module on the left or is written with parentheses.
    pub field_reads: bool,
}

const PLAIN: CallSyntax = CallSyntax {
    colon: false,
    dollar: false,
    message: false,
    hash: false,
    slash: false,
};
const COLON: CallSyntax = CallSyntax { colon: true, ..PLAIN };

/// A grammar with a query of ours, plain calls and owner qualifiers.
const fn own(
    lang: &'static str,
    extensions: &'static [&'static str],
    language: LanguageFn,
    query: &'static str,
    comment: &'static str,
) -> Grammar {
    Grammar {
        lang,
        extensions,
        language,
        query: Query::Own(query),
        locals: "",
        comment,
        alt_comment: None,
        calls: PLAIN,
        qualifier: Qualifier::Owner,
        clauses: false,
        case_insensitive: false,
        not_calls: &[],
        semicolons: false,
        field_reads: false,
    }
}

/// A grammar with the crate's query and ours in front of it.
const fn with_crate(
    lang: &'static str,
    extensions: &'static [&'static str],
    language: LanguageFn,
    ours: &'static str,
    theirs: &'static str,
    comment: &'static str,
) -> Grammar {
    Grammar {
        query: Query::Crate { ours, theirs },
        ..own(lang, extensions, language, "", comment)
    }
}

pub(crate) static GRAMMARS: [Grammar; 33] = [
    with_crate(
        lang::RUST,
        &["rs"],
        tree_sitter_rust::LANGUAGE,
        include_str!("../../queries/rust.scm"),
        tree_sitter_rust::TAGS_QUERY,
        "//",
    ),
    with_crate(
        lang::GO,
        &["go"],
        tree_sitter_go::LANGUAGE,
        include_str!("../../queries/go.scm"),
        tree_sitter_go::TAGS_QUERY,
        "//",
    ),
    with_crate(
        lang::JAVA,
        &["java"],
        tree_sitter_java::LANGUAGE,
        include_str!("../../queries/java.scm"),
        tree_sitter_java::TAGS_QUERY,
        "//",
    ),
    own(
        lang::C,
        &["c", "h"],
        tree_sitter_c::LANGUAGE,
        include_str!("../../queries/c.scm"),
        "//",
    ),
    own(
        lang::CPP,
        &["cpp", "cc", "cxx", "hpp", "hh", "hxx"],
        tree_sitter_cpp::LANGUAGE,
        include_str!("../../queries/cpp.scm"),
        "//",
    ),
    own(
        lang::CUDA,
        &["cu", "cuh"],
        tree_sitter_cuda::LANGUAGE,
        include_str!("../../queries/cpp.scm"),
        "//",
    ),
    own(
        lang::CSHARP,
        &["cs"],
        tree_sitter_c_sharp::LANGUAGE,
        include_str!("../../queries/csharp.scm"),
        "//",
    ),
    Grammar {
        locals: include_str!("../../queries/ruby_locals.scm"),
        ..own(
            lang::RUBY,
            &["rb", "rake"],
            tree_sitter_ruby::LANGUAGE,
            include_str!("../../queries/ruby.scm"),
            "#",
        )
    },
    Grammar {
        alt_comment: Some("#"),
        ..with_crate(
            lang::PHP,
            &["php"],
            tree_sitter_php::LANGUAGE_PHP,
            include_str!("../../queries/php.scm"),
            tree_sitter_php::TAGS_QUERY,
            "//",
        )
    },
    own(
        lang::KOTLIN,
        &["kt", "kts"],
        tree_sitter_kotlin_ng::LANGUAGE,
        include_str!("../../queries/kotlin.scm"),
        "//",
    ),
    own(
        lang::SWIFT,
        &["swift"],
        tree_sitter_swift::LANGUAGE,
        include_str!("../../queries/swift.scm"),
        "//",
    ),
    own(
        lang::SCALA,
        &["scala", "sc"],
        tree_sitter_scala::LANGUAGE,
        include_str!("../../queries/scala.scm"),
        "//",
    ),
    Grammar {
        semicolons: true,
        ..own(
            lang::GROOVY,
            &["groovy", "gvy"],
            tree_sitter_groovy::LANGUAGE,
            include_str!("../../queries/groovy.scm"),
            "//",
        )
    },
    own(
        lang::DART,
        &["dart"],
        tree_sitter_dart::LANGUAGE,
        include_str!("../../queries/dart.scm"),
        "//",
    ),
    Grammar {
        calls: COLON,
        ..with_crate(
            lang::LUA,
            &["lua"],
            tree_sitter_lua::LANGUAGE,
            include_str!("../../queries/lua.scm"),
            tree_sitter_lua::TAGS_QUERY,
            "--",
        )
    },
    Grammar {
        calls: COLON,
        ..own(
            lang::LUAU,
            &["luau"],
            tree_sitter_luau::LANGUAGE,
            include_str!("../../queries/luau.scm"),
            "--",
        )
    },
    own(
        lang::BASH,
        &["sh", "bash"],
        tree_sitter_bash::LANGUAGE,
        include_str!("../../queries/bash.scm"),
        "#",
    ),
    Grammar {
        case_insensitive: true,
        ..own(
            lang::POWERSHELL,
            &["ps1", "psm1"],
            tree_sitter_powershell::LANGUAGE,
            include_str!("../../queries/powershell.scm"),
            "#",
        )
    },
    Grammar {
        clauses: true,
        field_reads: true,
        ..with_crate(
            lang::ELIXIR,
            &["ex", "exs"],
            tree_sitter_elixir::LANGUAGE,
            include_str!("../../queries/elixir.scm"),
            tree_sitter_elixir::TAGS_QUERY,
            "#",
        )
    },
    Grammar {
        calls: COLON,
        clauses: true,
        ..own(
            lang::ERLANG,
            &["erl", "hrl"],
            tree_sitter_erlang::LANGUAGE,
            include_str!("../../queries/erlang.scm"),
            "%",
        )
    },
    own(
        lang::GLEAM,
        &["gleam"],
        tree_sitter_gleam::LANGUAGE,
        include_str!("../../queries/gleam.scm"),
        "//",
    ),
    Grammar {
        clauses: true,
        ..own(
            lang::HASKELL,
            &["hs"],
            tree_sitter_haskell::LANGUAGE,
            include_str!("../../queries/haskell.scm"),
            "--",
        )
    },
    own(
        lang::ELM,
        &["elm"],
        tree_sitter_elm::LANGUAGE,
        include_str!("../../queries/elm.scm"),
        "--",
    ),
    Grammar {
        calls: CallSyntax { hash: true, ..PLAIN },
        // Infix keywords and binding operators are operators, like `+`.
        not_calls: &[
            "mod", "land", "lor", "lxor", "lsl", "lsr", "asr", "or", "let*", "let+", "and*", "and+",
        ],
        ..with_crate(
            lang::OCAML,
            &["ml"],
            tree_sitter_ocaml::LANGUAGE_OCAML,
            "",
            tree_sitter_ocaml::TAGS_QUERY,
            "(*",
        )
    },
    Grammar {
        qualifier: Qualifier::SelfVariable,
        ..own(
            lang::FSHARP,
            &["fs", "fsx"],
            tree_sitter_fsharp::LANGUAGE_FSHARP,
            include_str!("../../queries/fsharp.scm"),
            "//",
        )
    },
    own(
        lang::ZIG,
        &["zig"],
        tree_sitter_zig::LANGUAGE,
        include_str!("../../queries/zig.scm"),
        "//",
    ),
    Grammar {
        calls: CallSyntax { dollar: true, ..PLAIN },
        ..with_crate(
            lang::R,
            &["r", "R"],
            tree_sitter_r::LANGUAGE,
            include_str!("../../queries/r.scm"),
            tree_sitter_r::TAGS_QUERY,
            "#",
        )
    },
    own(
        lang::JULIA,
        &["jl"],
        tree_sitter_julia::LANGUAGE,
        include_str!("../../queries/julia.scm"),
        "#",
    ),
    with_crate(
        lang::SOLIDITY,
        &["sol"],
        tree_sitter_solidity::LANGUAGE,
        include_str!("../../queries/solidity.scm"),
        tree_sitter_solidity::TAGS_QUERY,
        "//",
    ),
    Grammar {
        calls: CallSyntax { message: true, ..PLAIN },
        ..own(
            lang::OBJC,
            &["m"],
            tree_sitter_objc::LANGUAGE,
            include_str!("../../queries/objc.scm"),
            "//",
        )
    },
    own(
        lang::NIX,
        &["nix"],
        tree_sitter_nix::LANGUAGE,
        include_str!("../../queries/nix.scm"),
        "#",
    ),
    own(
        lang::PERL,
        &["pl", "pm"],
        ts_parser_perl::LANGUAGE,
        include_str!("../../queries/perl.scm"),
        "#",
    ),
    Grammar {
        calls: CallSyntax { slash: true, ..PLAIN },
        ..own(
            lang::CLOJURE,
            &["clj", "cljs", "cljc"],
            tree_sitter_clojure_orchard::LANGUAGE,
            include_str!("../../queries/clojure.scm"),
            ";",
        )
    },
];

/// The grammar for a file extension (without the dot).
pub(crate) fn by_extension(ext: &str) -> Option<&'static Grammar> {
    GRAMMARS.iter().find(|g| g.extensions.contains(&ext))
}

/// Position of a language in [`GRAMMARS`].
pub(crate) fn position(lang: &str) -> Option<usize> {
    GRAMMARS.iter().position(|g| g.lang == lang)
}
