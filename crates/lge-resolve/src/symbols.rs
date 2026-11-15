//! Lookup tables over the files and symbols of one resolver run.
//!
//! Every table is built from sorted, de-duplicated input, so a lookup gives the
//! same answer whatever order the store returned the records in.

use lge_core::{kind, lang, FileRec, ResolveInput, SymbolRec};
use std::collections::{BTreeMap, HashMap, HashSet};

/// Extensions a TypeScript or JavaScript import may leave out, in the order they are tried.
pub(crate) const SCRIPT_EXTENSIONS: [&str; 6] = [".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs"];

/// The import systems the resolver knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lang {
    Python,
    /// TypeScript and JavaScript share one module system.
    Script,
    Other,
}

impl Lang {
    /// The language of the file at `path` among `files`, from its extension
    /// when it has no record.
    fn of_path(files: &BTreeMap<&str, &FileRec>, path: &str) -> Lang {
        match files.get(path) {
            Some(f) => Lang::of(f),
            None => Lang::of(&FileRec {
                path: path.to_string(),
                ..FileRec::default()
            }),
        }
    }

    pub fn of(file: &FileRec) -> Lang {
        match file.lang.as_str() {
            lang::PYTHON => Lang::Python,
            lang::TYPESCRIPT | lang::JAVASCRIPT => Lang::Script,
            // A record without a language: the extension says the same thing.
            "" if file.path.ends_with(".py") => Lang::Python,
            "" if SCRIPT_EXTENSIONS.iter().any(|ext| file.path.ends_with(ext)) => Lang::Script,
            _ => Lang::Other,
        }
    }
}

/// Which symbol kinds a lookup accepts. Module symbols are in no lookup table, so
/// no lookup by name can make a module a call, inheritance or reference target.
/// The one way to a module symbol is a default export the indexer marked on it
/// (`export default { ... }`): a reference to that import mentions the module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Want {
    /// What a bare call `f()` can reach: a function or a class (its constructor).
    Callable,
    /// What `x.f()` can reach by name alone: a method or a function.
    Member,
    /// What `self.f()` or `Class.f()` can reach inside a known class.
    CallTarget,
    Class,
    /// A type whose members `self` and an implicit `this` name: a class, an
    /// enum or an interface (a Rust trait, a Java interface with default methods).
    Scope,
    /// What a class can extend or implement.
    Base,
    /// What a bare identifier can name: anything but a method.
    Nameable,
    Any,
}

impl Want {
    pub fn admits(self, k: &str) -> bool {
        match self {
            Want::Callable => k == kind::FUNCTION || k == kind::CLASS,
            Want::Member => k == kind::METHOD || k == kind::FUNCTION,
            Want::CallTarget => k == kind::FUNCTION || k == kind::METHOD || k == kind::CLASS,
            Want::Class => k == kind::CLASS,
            Want::Scope => k == kind::CLASS || k == kind::ENUM || k == kind::INTERFACE,
            Want::Base => k == kind::CLASS || k == kind::INTERFACE,
            Want::Nameable => k != kind::METHOD,
            Want::Any => true,
        }
    }

    /// True for the lookups that look for something to call.
    fn is_call(self) -> bool {
        matches!(self, Want::Callable | Want::Member | Want::CallTarget)
    }
}

pub(crate) struct Index<'a> {
    files: BTreeMap<&'a str, &'a FileRec>,
    files_by_id: HashMap<&'a str, &'a FileRec>,
    /// The [`family`] of each file, by path: looked up once per candidate of
    /// every match by name.
    families: HashMap<&'a str, &'a str>,
    by_id: HashMap<&'a str, &'a SymbolRec>,
    modules: HashMap<&'a str, &'a SymbolRec>,
    /// Every module symbol by file and qualified name, inner modules included.
    module_qn: HashMap<(&'a str, &'a str), &'a SymbolRec>,
    by_file_name: HashMap<(&'a str, &'a str), Vec<&'a SymbolRec>>,
    by_file_qn: HashMap<(&'a str, &'a str), Vec<&'a SymbolRec>>,
    by_name: HashMap<&'a str, Vec<&'a SymbolRec>>,
    /// Members declared inside an interface or a type: signatures with no body,
    /// so nothing to call. In the languages read through a tags query an
    /// interface member can have a body (a Java default method, a Rust trait
    /// method), and only a member without one is a signature.
    signatures: HashSet<&'a str>,
    /// Members written without a body in the languages read through a tags
    /// query (a C++ method declared in its class and defined in the `.cpp`, an
    /// abstract method): what a call in the same file can name, but no
    /// candidate of a match by name, where the definition is the one meant.
    declarations: HashSet<&'a str>,
}

/// Every field of a symbol that can change the output, so that two records with
/// the same id are told apart the same way on every run.
fn symbol_order(s: &SymbolRec) -> (&str, &str, &str, &str, &str, &str, bool) {
    (
        &s.id,
        &s.file_path,
        &s.qualified_name,
        &s.kind,
        &s.name,
        &s.service,
        s.is_test,
    )
}

impl<'a> Index<'a> {
    pub fn new(input: &'a ResolveInput) -> Index<'a> {
        let mut files: Vec<&FileRec> = input.files.iter().collect();
        files.sort_by(|a, b| (&a.path, &a.id, &a.service, &a.lang).cmp(&(&b.path, &b.id, &b.service, &b.lang)));
        let mut symbols: Vec<&SymbolRec> = input.symbols.iter().collect();
        symbols.sort_by(|a, b| symbol_order(a).cmp(&symbol_order(b)));

        let mut ix = Index {
            files: BTreeMap::new(),
            files_by_id: HashMap::new(),
            families: HashMap::new(),
            by_id: HashMap::new(),
            modules: HashMap::new(),
            module_qn: HashMap::new(),
            by_file_name: HashMap::new(),
            by_file_qn: HashMap::new(),
            by_name: HashMap::new(),
            signatures: HashSet::new(),
            declarations: HashSet::new(),
        };
        for f in files {
            if !ix.files.contains_key(f.path.as_str()) {
                ix.files.insert(&f.path, f);
                ix.files_by_id.entry(&f.id).or_insert(f);
                ix.families.insert(&f.path, family(f));
            }
        }
        for s in symbols {
            // Placeholders of an earlier run are derived data, not input.
            if s.kind == kind::UNRESOLVED || ix.by_id.contains_key(s.id.as_str()) {
                continue;
            }
            ix.by_id.insert(&s.id, s);
            // Outlines of HTML, CSS, Markdown, SQL and configuration files: in
            // the graph for search and line ranges, never a call or reference target.
            if s.kind == kind::DOCUMENT || s.kind == kind::SECTION {
                continue;
            }
            if s.kind == kind::MODULE {
                // A file's own module symbol is the outermost one: a module
                // defined inside the file (an Elixir `defmodule`, a C++
                // namespace) has a longer qualified name under it.
                let file_module = ix.modules.entry(&s.file_path).or_insert(s);
                if s.qualified_name.len() < file_module.qualified_name.len() {
                    *file_module = s;
                }
                ix.module_qn.entry((&s.file_path, &s.qualified_name)).or_insert(s);
                continue;
            }
            ix.by_file_name.entry((&s.file_path, &s.name)).or_default().push(s);
            ix.by_file_qn
                .entry((&s.file_path, &s.qualified_name))
                .or_default()
                .push(s);
            ix.by_name.entry(&s.name).or_default().push(s);
            let member = s.kind == kind::METHOD || s.kind == kind::FUNCTION;
            if member && Lang::of_path(&ix.files, &s.file_path) == Lang::Other && is_declaration(s) {
                ix.declarations.insert(&s.id);
            }
        }
        // A member's scope is its qualified name without the last segment. A
        // class merged with an interface of the same name still defines its
        // members, so every declaration of the scope must be signature-only.
        for ((file, qualified_name), members) in &ix.by_file_qn {
            let scope = qualified_name
                .rsplit_once('.')
                .and_then(|(scope, _)| ix.by_file_qn.get(&(*file, scope)));
            if scope.is_some_and(|outer| outer.iter().all(|s| declares_only(s))) {
                let other = Lang::of_path(&ix.files, file) == Lang::Other;
                ix.signatures.extend(
                    members
                        .iter()
                        .filter(|s| !other || has_no_body(s))
                        .map(|s| s.id.as_str()),
                );
            }
        }
        ix
    }

    pub fn file(&self, path: &str) -> Option<&'a FileRec> {
        self.files.get(path).copied()
    }

    pub fn file_by_id(&self, id: &str) -> Option<&'a FileRec> {
        self.files_by_id.get(id).copied()
    }

    /// All files, sorted by path.
    pub fn files(&self) -> impl Iterator<Item = &'a FileRec> + '_ {
        self.files.values().copied()
    }

    pub fn lang(&self, path: &str) -> Lang {
        self.file(path).map(Lang::of).unwrap_or(Lang::Other)
    }

    /// The language of a file as the indexer recorded it; "" when unknown.
    pub fn lang_name(&self, path: &str) -> &'a str {
        self.file(path).map(|f| f.lang.as_str()).unwrap_or("")
    }

    /// Which code a symbol in `path` can call by name alone. See [`family`].
    pub fn family(&self, path: &str) -> &'a str {
        self.families.get(path).copied().unwrap_or("")
    }

    /// Any symbol, module symbols included (they own top-level calls and facts).
    pub fn sym(&self, id: &str) -> Option<&'a SymbolRec> {
        self.by_id.get(id).copied()
    }

    /// The module symbol of `file`: the owner of its top-level code.
    pub fn module_of(&self, file: &str) -> Option<&'a SymbolRec> {
        self.modules.get(file).copied()
    }

    /// Symbols called `name` that are defined in `file`.
    pub fn in_file(&self, file: &str, name: &str, want: Want) -> Vec<&'a SymbolRec> {
        self.admitted(self.by_file_name.get(&(file, name)), want)
    }

    /// The symbol called `name` in `file`, when there is exactly one.
    pub fn one_in_file(&self, file: &str, name: &str, want: Want) -> Option<&'a SymbolRec> {
        one(self.in_file(file, name, want))
    }

    /// The symbol with this qualified name in `file`, when there is exactly one.
    ///
    /// The file is part of the key because qualified names are not unique across
    /// files (`a.py` and `a/__init__.py`, `util.ts` and `util/index.ts`).
    pub fn member(&self, file: &str, qualified_name: &str, want: Want) -> Option<&'a SymbolRec> {
        one(self.admitted(self.by_file_qn.get(&(file, qualified_name)), want))
    }

    /// Every symbol of `file` with this qualified name, a module included.
    pub fn in_scope(&self, file: &str, qualified_name: &str) -> Vec<&'a SymbolRec> {
        let mut found = self
            .by_file_qn
            .get(&(file, qualified_name))
            .cloned()
            .unwrap_or_default();
        found.extend(self.module_qn.get(&(file, qualified_name)).copied());
        found
    }

    /// Symbols called `name` anywhere in the repository, sorted by id. A
    /// declaration is no candidate to call.
    #[cfg(test)]
    pub fn named(&self, name: &str, want: Want) -> Vec<&'a SymbolRec> {
        self.named_iter(name, want).collect()
    }

    /// [`Index::named`], one at a time: a match by name stops at its second
    /// candidate, and a common name has thousands.
    pub fn named_iter<'s>(&'s self, name: &str, want: Want) -> impl Iterator<Item = &'a SymbolRec> + 's {
        self.by_name
            .get(name)
            .into_iter()
            .flatten()
            .copied()
            .filter(move |s| self.fits(s, want) && !(want.is_call() && self.declarations.contains(s.id.as_str())))
    }

    fn admitted(&self, found: Option<&Vec<&'a SymbolRec>>, want: Want) -> Vec<&'a SymbolRec> {
        found
            .map(|v| v.iter().copied().filter(|s| self.fits(s, want)).collect())
            .unwrap_or_default()
    }

    fn fits(&self, s: &SymbolRec, want: Want) -> bool {
        want.admits(&s.kind) && !(want.is_call() && self.signatures.contains(s.id.as_str()))
    }
}

/// The languages that call each other by name: Python, TypeScript and
/// JavaScript (as before any other language was indexed), C with C++,
/// Objective-C, CUDA and Swift, the JVM languages, the BEAM languages, Lua with Luau;
/// every other language on its own. "" when the language is not known, which
/// matches every family.
pub(crate) fn family(file: &FileRec) -> &str {
    match file.lang.as_str() {
        lang::PYTHON | lang::TYPESCRIPT | lang::JAVASCRIPT => "script",
        "" if Lang::of(file) != Lang::Other => "script",
        // Swift calls Objective-C (and C) of its target by the same names.
        lang::C | lang::CPP | lang::OBJC | lang::CUDA | lang::SWIFT => "c",
        lang::JAVA | lang::KOTLIN | lang::SCALA | lang::GROOVY => "jvm",
        lang::ELIXIR | lang::ERLANG => "beam",
        lang::LUA | lang::LUAU => "lua",
        other => other,
    }
}

/// An interface or a type alias: it declares members, it does not define them.
fn declares_only(s: &SymbolRec) -> bool {
    s.kind == kind::INTERFACE || s.kind == kind::TYPE
}

/// A member written without a body: its source text is its signature and a
/// `;` at most (`fn area(&self) -> f64;`, `int total();`, `func total() -> Int`).
fn has_no_body(s: &SymbolRec) -> bool {
    let body = s.body.split_whitespace().collect::<Vec<_>>().join(" ");
    let body = body.trim_end_matches(';').trim_end();
    !s.signature.is_empty() && body.ends_with(s.signature.as_str())
}

/// A member declared with a `;` and no body (`void swap(Buffer&) noexcept;`,
/// `abstract int rows();`). The `;` tells it from a one-line definition whose
/// header runs to its end (Erlang's `report(C) -> C.`).
fn is_declaration(s: &SymbolRec) -> bool {
    s.body.trim_end().ends_with(';') && has_no_body(s)
}

/// Several candidates are never narrowed down to one.
pub(crate) fn one<T>(mut found: Vec<T>) -> Option<T> {
    if found.len() == 1 {
        found.pop()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym(file: &str, qn: &str, k: &str) -> SymbolRec {
        SymbolRec {
            id: lge_core::symbol_id(file, qn, k),
            name: qn.rsplit('.').next().unwrap_or(qn).to_string(),
            qualified_name: qn.to_string(),
            kind: k.to_string(),
            file_path: file.to_string(),
            ..SymbolRec::default()
        }
    }

    #[test]
    fn modules_and_placeholders_are_in_no_lookup_table() {
        let input = ResolveInput {
            symbols: vec![
                sym("a/tasks.py", "a.tasks", kind::MODULE),
                sym("", "tasks", kind::UNRESOLVED),
                sym("a/tasks.py", "a.tasks.tasks", kind::FUNCTION),
            ],
            ..ResolveInput::default()
        };
        let ix = Index::new(&input);
        assert_eq!(ix.named("tasks", Want::Any).len(), 1);
        assert_eq!(ix.named("tasks", Want::Any)[0].kind, kind::FUNCTION);
        assert!(ix.sym(&input.symbols[0].id).is_some(), "a module can still own calls");
        assert_eq!(
            ix.module_of("a/tasks.py").map(|s| s.id.as_str()),
            Some(input.symbols[0].id.as_str())
        );
        assert!(ix.module_of("a/other.py").is_none());
        assert!(ix.sym(&input.symbols[1].id).is_none());
        assert!(ix.member("a/tasks.py", "a.tasks", Want::Any).is_none());
    }

    #[test]
    fn several_matches_are_not_one() {
        let input = ResolveInput {
            symbols: vec![
                sym("m.py", "m.A.run", kind::METHOD),
                sym("m.py", "m.B.run", kind::METHOD),
            ],
            ..ResolveInput::default()
        };
        let ix = Index::new(&input);
        assert_eq!(ix.in_file("m.py", "run", Want::Member).len(), 2);
        assert!(ix.one_in_file("m.py", "run", Want::Member).is_none());
        assert!(ix.member("m.py", "m.A.run", Want::CallTarget).is_some());
        assert!(ix.member("other.py", "m.A.run", Want::CallTarget).is_none());
        assert!(
            ix.one_in_file("m.py", "run", Want::Callable).is_none(),
            "a method is not a bare callable"
        );
    }

    #[test]
    fn members_of_an_interface_or_type_are_not_call_targets() {
        let input = ResolveInput {
            symbols: vec![
                sym("m.ts", "m.Mailer", kind::INTERFACE),
                sym("m.ts", "m.Mailer.send", kind::METHOD),
                sym("m.ts", "m.Handlers", kind::TYPE),
                sym("m.ts", "m.Handlers.send", kind::METHOD),
                sym("m.ts", "m.Smtp", kind::CLASS),
                sym("m.ts", "m.Smtp.send", kind::METHOD),
            ],
            ..ResolveInput::default()
        };
        let ix = Index::new(&input);
        let called: Vec<&str> = ix
            .named("send", Want::Member)
            .iter()
            .map(|s| s.qualified_name.as_str())
            .collect();
        assert_eq!(called, ["m.Smtp.send"]);
        assert!(ix.member("m.ts", "m.Mailer.send", Want::CallTarget).is_none());
        assert_eq!(ix.named("send", Want::Any).len(), 3, "they can still be mentioned");
    }

    #[test]
    fn a_declaration_is_named_in_its_file_but_no_candidate_by_name() {
        let with_body = |file: &str, qn: &str, signature: &str, body: &str| SymbolRec {
            signature: signature.into(),
            body: body.into(),
            ..sym(file, qn, kind::METHOD)
        };
        let input = ResolveInput {
            symbols: vec![
                sym("order.hpp", "order.Order", kind::CLASS),
                with_body(
                    "order.hpp",
                    "order.Order.subtotal",
                    "int subtotal() const",
                    "int subtotal() const;",
                ),
                with_body(
                    "order.cpp",
                    "order.Order.subtotal",
                    "int Order::subtotal() const",
                    "int Order::subtotal() const { return 1; }",
                ),
                // A one-line Erlang clause: its header runs to its end, but it is defined.
                with_body("money.erl", "money.report", "report(C) -> C.", "report(C) -> C."),
            ],
            ..ResolveInput::default()
        };
        let ix = Index::new(&input);
        let called: Vec<&str> = ix
            .named("subtotal", Want::Member)
            .iter()
            .map(|s| s.file_path.as_str())
            .collect();
        assert_eq!(called, ["order.cpp"]);
        assert!(ix
            .member("order.hpp", "order.Order.subtotal", Want::CallTarget)
            .is_some());
        assert_eq!(ix.named("report", Want::Member).len(), 1);
    }

    #[test]
    fn language_comes_from_the_record_then_the_extension() {
        let file = |path: &str, lang: &str| FileRec {
            path: path.into(),
            lang: lang.into(),
            ..FileRec::default()
        };
        assert_eq!(Lang::of(&file("a.py", "python")), Lang::Python);
        assert_eq!(Lang::of(&file("a.ts", "typescript")), Lang::Script);
        assert_eq!(Lang::of(&file("a.mjs", "javascript")), Lang::Script);
        assert_eq!(Lang::of(&file("a.py", "")), Lang::Python);
        assert_eq!(Lang::of(&file("a.tsx", "")), Lang::Script);
        assert_eq!(Lang::of(&file("a.go", "go")), Lang::Other);
        assert_eq!(Lang::of(&file("a.txt", "")), Lang::Other);
    }

    #[test]
    fn languages_that_call_each_other_share_a_family() {
        let file = |path: &str, lang: &str| FileRec {
            path: path.into(),
            lang: lang.into(),
            ..FileRec::default()
        };
        for (path, lang) in [
            ("a.py", "python"),
            ("a.ts", "typescript"),
            ("a.js", "javascript"),
            ("a.tsx", ""),
        ] {
            assert_eq!(family(&file(path, lang)), "script", "{path}");
        }
        for (path, lang) in [("a.c", "c"), ("a.cpp", "cpp"), ("a.m", "objc"), ("a.cu", "cuda")] {
            assert_eq!(family(&file(path, lang)), "c", "{path}");
        }
        for (path, lang) in [("A.java", "java"), ("A.kt", "kotlin"), ("A.scala", "scala")] {
            assert_eq!(family(&file(path, lang)), "jvm", "{path}");
        }
        assert_eq!(family(&file("a.ex", "elixir")), family(&file("a.erl", "erlang")));
        assert_eq!(family(&file("a.lua", "lua")), family(&file("a.luau", "luau")));
        assert_eq!(family(&file("a.go", "go")), "go");
        assert_eq!(family(&file("a.rb", "ruby")), "ruby");
        assert_eq!(family(&file("notes.txt", "")), "", "unknown matches every family");
    }
}
