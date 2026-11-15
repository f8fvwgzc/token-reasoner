//! Every language besides Python, TypeScript and JavaScript: a Tree-sitter
//! grammar and a tags query per language (the format GitHub code navigation
//! uses), run through `tree_sitter_tags`.
//!
//! A tags query names definitions (`@definition.function`, `@definition.class`,
//! ...) and call sites (`@reference.call`), each with its `@name`. That is
//! enough for symbols with line ranges, qualified names from how definitions
//! nest, and call sites with their receiver. No import sites are extracted: lge
//! does not resolve these languages' module systems, so a call into another
//! file is matched by name and is at most `inferred`.

mod grammars;
mod text;

use crate::facts::fact_site;
use crate::{cap_body, unique_ids};
use anyhow::{anyhow, Context, Result};
use grammars::{by_extension, position, Grammar, Qualifier, Query, GRAMMARS};
use lge_core::{fact, file_id, kind, lang, stable_id, symbol_id, CallSite, FactSite, FileRec, ParsedFile, SymbolRec};
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::OnceLock;
use text::Lines;
use tree_sitter_tags::{Tag, TagsConfiguration, TagsContext};

/// The language of a file extension (without the dot), when a grammar reads it.
pub(crate) fn lang_of_extension(ext: &str) -> Option<&'static str> {
    by_extension(ext).map(|g| g.lang)
}

/// The line comment token of a language read here.
pub(crate) fn comment_token(lang: &str) -> Option<&'static str> {
    position(lang).map(|i| GRAMMARS[i].comment)
}

/// A second line comment token of a language read here (PHP's `#`).
pub(crate) fn alt_comment_token(lang: &str) -> Option<&'static str> {
    position(lang).and_then(|i| GRAMMARS[i].alt_comment)
}

/// Compiled tags configurations, built the first time a language is parsed.
/// A query that does not compile is an error for every file of its language.
static CONFIGS: [OnceLock<Result<TagsConfiguration, String>>; GRAMMARS.len()] =
    [const { OnceLock::new() }; GRAMMARS.len()];

fn config(i: usize) -> Result<&'static TagsConfiguration> {
    let g = &GRAMMARS[i];
    CONFIGS[i]
        .get_or_init(|| {
            let query = match g.query {
                Query::Crate { ours, theirs } => format!("{ours}\n{theirs}"),
                Query::Own(q) => q.to_string(),
            };
            TagsConfiguration::new(g.language.into(), &query, g.locals).map_err(|e| e.to_string())
        })
        .as_ref()
        .map_err(|e| anyhow!("{} tags query: {e}", g.lang))
}

/// What a definition tag declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Def {
    /// A function, method, macro or constructor: a method when a class-like
    /// definition directly encloses it or an owner is written before its name.
    Callable,
    Class,
    /// A block that adds members to a type named elsewhere (Rust `impl`, Swift
    /// `extension`, Objective-C `@implementation`). It is a class symbol only when
    /// the file has no other definition with its name.
    Impl,
    Interface,
    Module,
    Type,
    Enum,
    /// The body of an anonymous class (`new Runnable() { ... }`, Kotlin
    /// `object : Runnable { }`): its functions are methods, but it is no symbol
    /// and adds no name.
    Anonymous,
    /// A lambda (Kotlin): no symbol and no name. A bare call in it may be made
    /// on the lambda's receiver (`sb.apply { append(x) }`).
    Lambda,
    /// Code that runs nothing (a typespec: Elixir `@spec`, Erlang `-spec`):
    /// what reads as a call in it names a type.
    Skip,
}

impl Def {
    /// The definition kind a tags query names, when lge keeps it. `test` is a
    /// test case (`TEST(Suite, Name)`, Zig `test "name"`), `declaration` a
    /// member declared without a body (a C++ method defined out of line), and
    /// `file_namespace` a namespace that runs to the end of the file (C#
    /// `namespace Shop;`).
    fn of(syntax_type: &str) -> Option<Def> {
        Some(match syntax_type {
            "function" | "method" | "macro" | "constructor" | "test" | "declaration" => Def::Callable,
            "class" | "struct" | "object" | "record" | "union" => Def::Class,
            "impl" => Def::Impl,
            "interface" | "trait" | "protocol" => Def::Interface,
            "module" | "namespace" | "package" | "file_namespace" => Def::Module,
            "type" => Def::Type,
            "enum" => Def::Enum,
            "anonymous" => Def::Anonymous,
            "lambda" => Def::Lambda,
            "skip" => Def::Skip,
            _ => return None,
        })
    }

    /// A definition directly inside one of these makes a callable a method.
    fn holds_methods(self) -> bool {
        matches!(
            self,
            Def::Class | Def::Impl | Def::Interface | Def::Enum | Def::Anonymous
        )
    }

    /// A scope that is no symbol and adds no name to what it holds.
    fn is_marker(self) -> bool {
        matches!(self, Def::Anonymous | Def::Lambda | Def::Skip)
    }

    fn is_type(self) -> bool {
        matches!(self, Def::Class | Def::Impl | Def::Interface | Def::Enum)
    }
}

/// One definition tag, read.
struct Definition {
    def: Def,
    name: String,
    /// Names written before the name and joined to it (`Cart` in `Cart::total`).
    owner: Vec<String>,
    range: Range<usize>,
    name_start: usize,
    docs: Option<String>,
    name_end: usize,
    /// A variable that stands for the instance inside this method (a Go
    /// receiver, F#'s self identifier).
    self_var: Option<String>,
    /// Marked a test where it is written (`TEST(...)`, Zig `test`, Rust `#[test]`).
    test: bool,
    /// Declared without a body: no symbol when the file also defines it.
    declaration: bool,
    /// Filled once nesting is known.
    qualified_name: String,
    kind: &'static str,
    id: String,
    is_test: bool,
    /// The definition this one is written in.
    parent: Option<usize>,
}

/// One file being read: what its symbols and call sites share.
struct File<'a> {
    path: &'a str,
    service: &'a str,
    src: &'a [u8],
    lines: Lines,
    /// The text of each line, for doc comments.
    text: Vec<&'a str>,
    grammar: &'a Grammar,
    is_test: bool,
}

/// Parse one file of a language read through a grammar and a tags query.
pub fn parse_generic(rel_path: &str, service: &str, src: &[u8], hash: &str, lang: &str) -> Result<ParsedFile> {
    let lang = grammar_lang(rel_path, src, lang);
    let i = position(lang).with_context(|| format!("no grammar for language {lang:?}"))?;
    let config = config(i)?;
    let tags = tags_for(config, &GRAMMARS[i], src).with_context(|| format!("tags of {rel_path}"))?;
    let text = String::from_utf8_lossy(src);
    let file = File {
        path: rel_path,
        service,
        src,
        lines: Lines::new(src),
        text: text.lines().collect(),
        grammar: &GRAMMARS[i],
        is_test: text::is_test_path(rel_path),
    };
    let module = module_name(rel_path);
    let module_id = symbol_id(rel_path, &module, kind::MODULE);
    let syntax = |t: &Tag| config.syntax_type_name(t.syntax_type_id);

    let mut defs: Vec<Definition> = tags
        .iter()
        .filter(|t| t.is_definition)
        .filter_map(|t| definition(t, syntax(t), &file))
        .collect();
    one_per_name(&mut defs);
    defs.sort_by_key(|d| (d.range.start, Reverse(d.range.end)));
    if file.grammar.clauses {
        merge_clauses(&mut defs);
    }
    name_definitions(&mut defs, &file, &module, &module_id);
    let ids = symbol_ids(&defs);

    let mut out = ParsedFile {
        file: Some(FileRec {
            id: file_id(rel_path),
            path: rel_path.to_string(),
            service: service.to_string(),
            lang: file.grammar.lang.to_string(),
            content_hash: hash.to_string(),
        }),
        ..ParsedFile::default()
    };
    out.symbols.push(module_symbol(&file, &module, &module_id, &defs));
    out.symbols.extend(definition_symbols(&file, &defs, &ids));
    out.calls = call_sites(&file, &call_tags(&tags, config, src), config, &defs, &ids, &module_id);
    if file.grammar.case_insensitive {
        same_case_as_definitions(&mut out);
    }
    let bases = tags.iter().filter(|t| !t.is_definition && syntax(t) == "base");
    out.facts = base_facts(&file, bases, &defs, &ids);
    unique_ids(&mut out);
    Ok(out)
}

/// Every tag of a file. A grammar that needs `;` where a newline ends a
/// statement reads the file with them written out; the tags are then moved
/// back to the offsets of `src`.
fn tags_for(config: &TagsConfiguration, grammar: &Grammar, src: &[u8]) -> Result<Vec<Tag>> {
    if !grammar.semicolons {
        return tags_of(config, src);
    }
    let (parsed, inserted) = text::with_semicolons(src);
    let mut tags = tags_of(config, &parsed)?;
    let back = |r: &Range<usize>| text::unshift(&inserted, r.start)..text::unshift(&inserted, r.end);
    for t in &mut tags {
        t.range = back(&t.range);
        t.name_range = back(&t.name_range);
    }
    Ok(tags)
}

/// The symbols of a file's definitions. An impl block of a type the file
/// defines adds no symbol: its members are the type's. A marker is no symbol,
/// and a declaration the file also defines is that definition.
fn definition_symbols(file: &File, defs: &[Definition], ids: &[String]) -> Vec<SymbolRec> {
    let defined: HashSet<&str> = defs.iter().filter(|d| !d.declaration).map(|d| d.id.as_str()).collect();
    defs.iter()
        .enumerate()
        .filter(|(k, d)| d.id == ids[*k] && !d.def.is_marker())
        .filter(|(_, d)| !(d.declaration && defined.contains(d.id.as_str())))
        .map(|(k, d)| symbol(file, d, defs.get(k + 1)))
        .collect()
}

/// The reference tags that are calls. A macro lives in another namespace than
/// a function: `name!(...)` is a call only of a macro the file defines.
fn call_tags<'t>(tags: &'t [Tag], config: &TagsConfiguration, src: &[u8]) -> Vec<&'t Tag> {
    let syntax = |t: &Tag| config.syntax_type_name(t.syntax_type_id);
    let macros: HashSet<String> = tags
        .iter()
        .filter(|t| t.is_definition && syntax(t) == "macro")
        .map(|t| text::name_text(src, &t.name_range))
        .collect();
    tags.iter()
        .filter(|t| {
            !t.is_definition
                && match syntax(t) {
                    "call" | "new" | "cascade" => true,
                    "macro" => macros.contains(&text::name_text(src, &t.name_range)),
                    _ => false,
                }
        })
        .collect()
}

/// The language whose grammar reads a file: a `.h` header written in C++
/// (most C++ projects name their headers `.h`) is read as C++.
fn grammar_lang<'l>(path: &str, src: &[u8], lang: &'l str) -> &'l str {
    if lang == lang::C && path.ends_with(".h") && text::looks_like_cpp(src) {
        lang::CPP
    } else {
        lang
    }
}

fn module_symbol(file: &File, module: &str, id: &str, defs: &[Definition]) -> SymbolRec {
    let starts: Vec<i64> = defs.iter().map(|d| file.lines.line(d.range.start)).collect();
    SymbolRec {
        id: id.to_string(),
        name: module.rsplit('.').next().unwrap_or(module).to_string(),
        qualified_name: module.to_string(),
        kind: kind::MODULE.into(),
        signature: String::new(),
        doc: text::module_doc(&file.text, file.grammar.comment, &starts),
        body: String::new(),
        file_path: file.path.to_string(),
        service: file.service.to_string(),
        start_line: 1,
        end_line: file.lines.count(file.src).max(1),
        is_test: file.is_test,
    }
}

/// The symbol of a definition. `next` is the definition after it: when that is
/// written inside it, the header stops before the line it starts on.
fn symbol(file: &File, d: &Definition, next: Option<&Definition>) -> SymbolRec {
    let start_line = file.lines.line(d.range.start);
    let doc = match d.docs.as_deref() {
        Some(docs) => text::first_doc_line(docs.lines()),
        None => text::leading_doc(&file.text, (start_line - 1) as usize, file.grammar.comment, &d.name),
    };
    let header_end = next
        .filter(|n| !n.def.is_marker() && n.range.start < d.range.end)
        .filter(|n| file.lines.line(n.range.start) > file.lines.line(d.name_start))
        .map_or(d.range.end, |n| file.lines.start_of(n.range.start));
    SymbolRec {
        id: d.id.clone(),
        name: d.name.clone(),
        qualified_name: d.qualified_name.clone(),
        kind: d.kind.into(),
        signature: text::signature(file.src, &file.lines, d.range.start, d.name_start, header_end),
        doc,
        body: cap_body(&file.src[d.range.clone()]),
        file_path: file.path.to_string(),
        service: file.service.to_string(),
        start_line,
        end_line: file.lines.line(d.range.end.saturating_sub(1).max(d.range.start)),
        is_test: d.is_test,
    }
}

/// One call site per call tag, made by the innermost definition around it
/// (the module for top-level code). A call in a typespec is none.
fn call_sites(
    file: &File,
    calls: &[&Tag],
    config: &TagsConfiguration,
    defs: &[Definition],
    ids: &[String],
    module_id: &str,
) -> Vec<CallSite> {
    let owners = innermost(defs, calls.iter().map(|t| t.name_range.start));
    calls
        .iter()
        .zip(owners)
        .filter(|(_, owner)| !owner.is_some_and(|o| defs[o].def == Def::Skip))
        .filter_map(|(t, owner)| {
            let caller_id = owner.map_or(module_id, |o| ids[o].as_str());
            call_site(
                file,
                t,
                config.syntax_type_name(t.syntax_type_id),
                owner.map(|o| &defs[o]),
                caller_id,
            )
        })
        .collect()
}

/// The call site of one call tag made by `owner`, when it is a call.
fn call_site(file: &File, t: &Tag, syntax: &str, owner: Option<&Definition>, caller_id: &str) -> Option<CallSite> {
    let mut name = text::name_text(file.src, &t.name_range);
    if !text::is_name(&name) || file.grammar.not_calls.contains(&name.as_str()) {
        return None;
    }
    let self_var = owner.and_then(|o| o.self_var.as_deref());
    let mut receiver = match syntax {
        // `new ns::Cart(3)` constructs a `Cart`, as `new Cart(3)` does.
        "new" => String::new(),
        // A cascade (`sb..write(a)`) calls on what it starts with.
        "cascade" => text::cascade_target(file.src, t.range.start, t.name_range.start, self_var),
        _ => text::receiver_before(file.src, t.name_range.start, file.grammar.calls, self_var),
    };
    // One token that holds its package (Perl's `Shop::Money::fmt`).
    if file.grammar.lang == lang::PERL {
        if let Some((package, last)) = name.clone().rsplit_once("::") {
            receiver = package.to_string();
            name = last.to_string();
        }
    }
    // Elixir's `user.email` on a variable, without parentheses, reads a field.
    if file.grammar.field_reads && text::reads_field(file.src, &receiver, t.name_range.end) {
        return None;
    }
    // A bare call in a lambda may be made on the lambda's receiver, whose type
    // is not known (`sb.apply { append(x) }` appends to `sb`). It is written as
    // Kotlin writes that receiver: `this@apply`.
    if let Some(lambda) = owner.filter(|o| o.def == Def::Lambda && receiver.is_empty()) {
        receiver = format!("this@{}", text::lambda_label(file.src, lambda.range.start));
    }
    let line = file.lines.line(t.name_range.start);
    let col = t.span.start.column.to_string();
    Some(CallSite {
        id: stable_id(&["call", file.path, caller_id, &name, &line.to_string(), &col]),
        caller_id: caller_id.to_string(),
        file_path: file.path.to_string(),
        name,
        receiver,
        line,
    })
}

/// PowerShell command names are case-insensitive: a call written `get-tax`
/// runs `Get-Tax`, so it takes the name the file defines it with.
fn same_case_as_definitions(out: &mut ParsedFile) {
    let names: HashMap<String, String> = out
        .symbols
        .iter()
        .map(|s| (s.name.to_lowercase(), s.name.clone()))
        .collect();
    for c in &mut out.calls {
        if let Some(name) = names.get(&c.name.to_lowercase()) {
            c.name = name.clone();
        }
    }
}

/// An `inherits` fact per base a type names (`class Child : public Base`),
/// owned by the innermost definition around the base.
fn base_facts<'t>(
    file: &File,
    bases: impl Iterator<Item = &'t Tag>,
    defs: &[Definition],
    ids: &[String],
) -> Vec<FactSite> {
    let bases: Vec<&Tag> = bases.collect();
    let owners = innermost(defs, bases.iter().map(|t| t.name_range.start));
    bases
        .iter()
        .zip(owners)
        .filter_map(|(t, owner)| {
            let owner = owner.filter(|&o| defs[o].def.is_type())?;
            let line = file.lines.line(t.name_range.start);
            let name = text::name_text(file.src, &t.name_range);
            Some(fact_site(
                file.path,
                &ids[owner],
                fact::INHERITS,
                &name,
                "",
                (line, line),
                true,
            ))
        })
        .collect()
}

/// Every tag of a file, in the order of their names.
fn tags_of(config: &TagsConfiguration, src: &[u8]) -> Result<Vec<Tag>> {
    let mut ctx = TagsContext::new();
    let (tags, _has_error) = ctx.generate_tags(config, src, None)?;
    let mut tags = tags.collect::<Result<Vec<Tag>, _>>()?;
    tags.sort_by_key(|t| (t.name_range.start, t.name_range.end));
    Ok(tags)
}

/// The path without its extension, `/` replaced by `.`.
fn module_name(path: &str) -> String {
    let stem = match path.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() && !stem.ends_with('/') => stem,
        _ => path,
    };
    stem.replace('/', ".")
}

fn definition(t: &Tag, syntax: &str, file: &File) -> Option<Definition> {
    let (src, lines, g) = (file.src, &file.lines, file.grammar);
    let def = Def::of(syntax)?;
    let mut name_start = t.name_range.start;
    // A name written as a string (a Spock feature, `def "adds the items"()`).
    let mut name = text::name_text(src, &t.name_range).trim_matches('"').to_string();
    if g.lang == lang::GROOVY && name == "def" {
        (name_start, name) = text::string_name(src, t.name_range.end, t.range.end)?;
    }
    if g.lang == lang::POWERSHELL {
        // `function script:Write-Log` defines `Write-Log` in the script scope.
        if let Some((scope, rest)) = name.split_once(':') {
            name_start += scope.len() + 1;
            name = rest.to_string();
        }
    }
    if !def.is_marker() && !text::is_name(&name) {
        return None;
    }
    let floor = lines.start_of(t.name_range.start).max(t.range.start);
    let mut owner = text::qualifier_before(src, floor, t.name_range.start);
    let mut self_var = None;
    if owner.len() == 1 && (g.qualifier == Qualifier::SelfVariable || text::is_self_word(&owner[0])) {
        self_var = owner.pop();
    }
    if def == Def::Callable && g.lang == lang::GO {
        let header = String::from_utf8_lossy(&src[t.range.start..name_start]);
        if let Some((var, ty)) = text::go_receiver(&header) {
            owner = vec![ty];
            self_var = var;
        }
    }
    let end = if syntax == "file_namespace" {
        src.len()
    } else {
        text::trim_trailing_comments(src, lines, name_start, t.range.end, g.comment)
    };
    let start_line = (lines.line(t.range.start) - 1) as usize;
    let test = syntax == "test" || (g.lang == lang::RUST && text::has_test_attribute(&file.text, start_line));
    Some(Definition {
        def,
        name,
        owner,
        range: t.range.start..end.max(t.name_range.end),
        name_start,
        docs: t.docs.clone(),
        name_end: t.name_range.end,
        self_var,
        test,
        declaration: syntax == "declaration",
        qualified_name: String::new(),
        kind: "",
        id: String::new(),
        is_test: false,
        parent: None,
    })
}

/// One definition per name. Two patterns that tag the same name are one tag
/// while both are queued, but a tag flushed in between (a type argument's
/// reference in `type Stack[T any] struct`, the functions in the body of a Zig
/// `fn Stack(T) type`) lets the second through. The more specific one is kept:
/// a type over a function, and a `type` alias only when it is all there is.
fn one_per_name(defs: &mut Vec<Definition>) {
    let weight = |d: &Definition| match d.def {
        Def::Type => 0,
        Def::Callable => 1,
        _ => 2,
    };
    defs.sort_by_key(|d| (d.name_start, d.name_end, Reverse(weight(d))));
    defs.dedup_by(|later, kept| later.name_start == kept.name_start && later.name_end == kept.name_end);
}

/// A function written as several clauses or equations (Elixir `def f(0)` and
/// `def f(n)`, Haskell `f 0 = 1` and `f n = ...`) is one function: clauses of
/// one name that follow each other become one definition over all of them.
fn merge_clauses(defs: &mut Vec<Definition>) {
    let mut merged: Vec<Definition> = Vec::with_capacity(defs.len());
    for d in defs.drain(..) {
        if let Some(last) = merged.last_mut() {
            let follows = last.def == Def::Callable && d.def == Def::Callable && d.range.start >= last.range.end;
            if follows && last.name == d.name && last.owner == d.owner {
                last.range.end = last.range.end.max(d.range.end);
                continue;
            }
        }
        merged.push(d);
    }
    *defs = merged;
}

/// Qualified names, kinds, ids and test marks from how definitions nest.
/// `defs` is sorted by start, outer before inner. A type alias is no scope: a
/// struct written inside a C `typedef` keeps its own name. A marker (a lambda,
/// an anonymous class) is a scope but adds no name, and stands for the
/// definition around it.
fn name_definitions(defs: &mut [Definition], file: &File, module: &str, module_id: &str) {
    let mut stack: Vec<usize> = Vec::new();
    for i in 0..defs.len() {
        while let Some(&top) = stack.last() {
            let (outer, inner) = (&defs[top].range, &defs[i].range);
            if outer.start <= inner.start && inner.end <= outer.end && outer != inner {
                break;
            }
            stack.pop();
        }
        let parent = stack.last().copied();
        let (mut qualified_name, parent_id, parent_test) = match parent {
            Some(p) => (defs[p].qualified_name.clone(), defs[p].id.clone(), defs[p].is_test),
            None => (module.to_string(), module_id.to_string(), file.is_test),
        };
        let in_class = parent.is_some_and(|p| defs[p].def.holds_methods());
        // `Cart.empty()` in class Cart (a Dart named constructor) names its own class.
        if parent.is_some_and(|p| defs[i].owner.len() == 1 && defs[i].owner[0] == defs[p].name) {
            defs[i].owner.clear();
        }
        let d = &mut defs[i];
        d.parent = parent;
        if !d.def.is_marker() {
            for part in d.owner.iter().chain([&d.name]) {
                qualified_name.push('.');
                qualified_name.push_str(part);
            }
        }
        d.kind = match d.def {
            Def::Callable if in_class || !d.owner.is_empty() => kind::METHOD,
            Def::Callable => kind::FUNCTION,
            Def::Class | Def::Impl => kind::CLASS,
            Def::Interface => kind::INTERFACE,
            Def::Module => kind::MODULE,
            Def::Type => kind::TYPE,
            Def::Enum => kind::ENUM,
            Def::Anonymous | Def::Lambda | Def::Skip => "",
        };
        d.is_test = parent_test
            || d.test
            || text::is_test_name(
                file.grammar.lang,
                &d.name,
                d.def != Def::Callable && d.def != Def::Module,
            );
        d.id = if d.def.is_marker() {
            parent_id
        } else {
            symbol_id(file.path, &qualified_name, d.kind)
        };
        d.qualified_name = qualified_name;
        if d.def != Def::Type {
            stack.push(i);
        }
    }
}

/// The symbol id each definition stands for. An impl block of a type the file
/// also defines stands for that definition, a marker for the definition it is
/// written in; every other definition is its own.
fn symbol_ids(defs: &[Definition]) -> Vec<String> {
    let mut named: HashMap<&str, &str> = HashMap::new();
    for d in defs.iter().filter(|d| d.def != Def::Impl && !d.def.is_marker()) {
        named.entry(d.qualified_name.as_str()).or_insert(d.id.as_str());
    }
    let mut ids: Vec<String> = Vec::with_capacity(defs.len());
    for d in defs {
        let id = match (d.def, d.parent) {
            (Def::Impl, _) => named
                .get(d.qualified_name.as_str())
                .map_or(d.id.clone(), |id| id.to_string()),
            (Def::Anonymous | Def::Lambda | Def::Skip, Some(p)) => ids[p].clone(),
            _ => d.id.clone(),
        };
        ids.push(id);
    }
    ids
}

/// For each byte position (in any order), the innermost definition whose range
/// holds it. `defs` is sorted by start, outer before inner.
fn innermost(defs: &[Definition], positions: impl Iterator<Item = usize>) -> Vec<Option<usize>> {
    let positions: Vec<usize> = positions.collect();
    let mut order: Vec<usize> = (0..positions.len()).collect();
    order.sort_by_key(|&k| positions[k]);
    let mut out = vec![None; positions.len()];
    let mut stack: Vec<usize> = Vec::new();
    let mut next = 0;
    for k in order {
        let at = positions[k];
        while next < defs.len() && defs[next].range.start <= at {
            while stack
                .last()
                .is_some_and(|&top| defs[top].range.end <= defs[next].range.start)
            {
                stack.pop();
            }
            stack.push(next);
            next += 1;
        }
        while stack.last().is_some_and(|&top| defs[top].range.end <= at) {
            stack.pop();
        }
        out[k] = stack.last().copied().filter(|&top| defs[top].range.contains(&at));
    }
    out
}

#[cfg(test)]
mod tests;
