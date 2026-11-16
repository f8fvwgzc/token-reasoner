//! Repository scanner, content hashing, git change detection and Tree-sitter
//! extraction of symbols, call sites, import sites and fact sites.
//!
//! Python, TypeScript and JavaScript have extractors of their own. Every other
//! language goes through [`parse_generic`]: a grammar and a tags query per language.
//!
//! This crate only reads source files. It never resolves anything: what it
//! returns is what is written in one file, and `lge-resolve` turns that into edges.

mod bulky;
mod document;
pub mod facts;
mod generic;
pub mod git;
mod python;
mod typescript;

pub use bulky::{bulky_dirs, bulky_dirs_with, ignore_file, BulkyDir, BulkyLimits, BULKY_NAMES};
pub use generic::parse_generic;
pub use python::parse_python;
pub use typescript::parse_typescript;

use anyhow::{Context, Result};
use lge_core::{content_hash, lang, ParsedFile};
use rayon::prelude::*;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// A source file found by [`scan`].
#[derive(Debug, Clone, PartialEq)]
pub struct ScannedFile {
    /// Path relative to the scanned root, with forward slashes.
    pub rel_path: String,
    pub abs_path: PathBuf,
    pub service: String,
    /// One of `lge_core::lang`.
    pub lang: &'static str,
}

/// Language of a file, decided by its extension. `None` means the file is not indexed.
pub fn lang_of(path: &str) -> Option<&'static str> {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let (stem, ext) = name.rsplit_once('.')?;
    if stem.is_empty() {
        return None;
    }
    match ext {
        "py" => Some(lang::PYTHON),
        "ts" | "tsx" => Some(lang::TYPESCRIPT),
        "js" | "jsx" | "mjs" | "cjs" => Some(lang::JAVASCRIPT),
        other => generic::lang_of_extension(other).or_else(|| document::lang_of_extension(other)),
    }
}

/// Directories that hold dependencies or build output, never source to index.
const SKIP_DIRS: &[&str] = &["node_modules", "dist", "build", ".venv", "venv", "__pycache__"];

/// Generated files that share an extension with real source.
fn is_generated(file_name: &str) -> bool {
    file_name.ends_with(".d.ts")
        || file_name.ends_with(".min.js")
        || document::is_generated(file_name.rsplit('/').next().unwrap_or(file_name))
}

/// Directories where the other languages keep dependencies and build output
/// (Go and PHP `vendor`, Rust `target`, Elixir `deps` and `_build`, ...). They
/// are skipped only for files of those languages, so what Python, TypeScript
/// and JavaScript index stays the same.
const DEPENDENCY_DIRS: &[&str] = &[
    "vendor",
    "target",
    "_build",
    "deps",
    "Pods",
    ".build",
    ".gradle",
    ".dart_tool",
    ".stack-work",
    "dist-newstyle",
    "elm-stuff",
    "zig-cache",
    ".zig-cache",
    "zig-out",
    "_opam",
];

/// Generated source of the other languages: protobuf, Dart builders, designers.
const GENERATED_SUFFIXES: &[&str] = &[".pb.go", ".pb.cc", ".pb.h", ".g.dart", ".freezed.dart", ".Designer.cs"];

/// Largest file of the other languages that is read at all. Files over 2 MiB
/// are read for their definitions only (see `generic::CALLS_UP_TO_BYTES`):
/// they are almost always generated, and their calls would flood the graph.
const MAX_GENERIC_BYTES: u64 = 16 * 1024 * 1024;

/// True for a file of a language read through a tags query that is a
/// dependency, generated, or too big to be written by hand.
fn is_dependency_or_generated(rel: &str, lang: &str, bytes: u64) -> bool {
    if matches!(lang, lang::PYTHON | lang::TYPESCRIPT | lang::JAVASCRIPT) {
        return false;
    }
    let mut dirs = rel.split('/');
    dirs.next_back();
    bytes > MAX_GENERIC_BYTES
        || dirs.any(|d| DEPENDENCY_DIRS.contains(&d))
        || GENERATED_SUFFIXES.iter().any(|s| rel.ends_with(s))
}

/// Walk `root`, honouring .gitignore, and return the source files of every
/// supported language, sorted by path.
///
/// `service_depth` 0 treats the whole root as one service; 1 uses the first
/// path component (a directory of sibling service repos).
pub fn scan(root: &Path, service_depth: usize) -> Result<Vec<ScannedFile>> {
    let root = root.canonicalize().context("root path")?;
    let root_name = root
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut out = Vec::new();
    let mut walk = ignore::WalkBuilder::new(&root);
    // .gitignore counts even when the root is not inside a git repository.
    walk.require_git(false);
    walk.filter_entry(|e| {
        // The root is scanned whatever it is called.
        let is_dir = e.depth() > 0 && e.file_type().is_some_and(|t| t.is_dir());
        !(is_dir && e.file_name().to_str().is_some_and(|n| SKIP_DIRS.contains(&n)))
    });
    for entry in walk.build() {
        let entry = entry?;
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        let rel = p.strip_prefix(&root)?.to_string_lossy().replace('\\', "/");
        let Some(lang) = lang_of(&rel) else { continue };
        let bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
        if is_generated(&rel) || is_dependency_or_generated(&rel, lang, bytes) {
            continue;
        }
        let service = match (service_depth, rel.split_once('/')) {
            (1, Some((first, _))) => first.to_string(),
            _ => root_name.clone(),
        };
        out.push(ScannedFile {
            rel_path: rel,
            abs_path: p.to_path_buf(),
            service,
            lang,
        });
    }
    out.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok(out)
}

pub fn hash_file(path: &Path) -> Result<(Vec<u8>, String)> {
    let bytes = std::fs::read(path)?;
    let h = content_hash(&bytes);
    Ok((bytes, h))
}

/// Parse one file with the extractor for its language. `Ok(None)` means the
/// language is not supported.
pub fn parse_file(rel_path: &str, service: &str, src: &[u8], hash: &str) -> Result<Option<ParsedFile>> {
    let (Some(lang), Some(comment)) = (lang_of(rel_path), comment_token(rel_path)) else {
        return Ok(None);
    };
    let mut parsed = match lang {
        lang::PYTHON => parse_python(rel_path, service, src, hash)?,
        lang::TYPESCRIPT | lang::JAVASCRIPT => parse_typescript(rel_path, service, src, hash)?,
        doc if document::is_document(doc) => document::parse_document(rel_path, service, src, hash, doc)?,
        // `.m` is Objective-C or MATLAB; MATLAB read as Objective-C is garbage.
        lang::OBJC if rel_path.ends_with(".m") && !generic::is_objc_source(src) => return Ok(None),
        other => parse_generic(rel_path, service, src, hash, other)?,
    };
    let tokens: Vec<&str> = [Some(comment), generic::alt_comment_token(lang)]
        .into_iter()
        .flatten()
        .collect();
    add_rationale(&mut parsed, rel_path, src, &tokens);
    Ok(Some(parsed))
}

/// The line comment token of a file's language (`#`, `//`, `--`, `%`, `(*`),
/// decided by its extension. `None` means the file is not indexed.
pub fn comment_token(path: &str) -> Option<&'static str> {
    match lang_of(path)? {
        lang::PYTHON => Some("#"),
        lang::TYPESCRIPT | lang::JAVASCRIPT => Some("//"),
        other => generic::comment_token(other).or_else(|| document::comment_token(other)),
    }
}

/// Most lines [`leading_start`] goes up.
pub const MAX_LEADING_LINES: usize = 40;

/// The first line (0-based) of the doc comments, attributes and annotations
/// written right above a definition that starts on line `start` (0-based) of
/// `lines`, the text of the file at `path`: `start` itself when there are
/// none. A blank line, a line of code or a line indented deeper than the
/// definition (the end of the block before it) ends them; an attribute over
/// several lines (`#[derive(` ... `)]`) counts whole. At most
/// [`MAX_LEADING_LINES`]. A document (Markdown, YAML, ...) has none: the
/// lines above a section are the end of the section before it.
pub fn leading_start(path: &str, lines: &[&str], start: usize) -> usize {
    let start = start.min(lines.len());
    let Some(lang) = lang_of(path) else {
        return start;
    };
    let Some(comment) = comment_token(path) else {
        return start;
    };
    if document::is_document(lang) {
        return start;
    }
    let alt = generic::alt_comment_token(lang);
    // Where `#` starts a comment, `#[` is a comment too, and block comments,
    // `*` lines and the like are code.
    let hash_comments = comment == "#";
    let openers: &[&str] = if hash_comments {
        &["@"]
    } else {
        &["/*", "(*", "{-", "-}", "#[", "@", "-spec"]
    };
    let indent = |line: &str| line.len() - line.trim_start().len();
    let def_indent = lines.get(start).map_or(usize::MAX, |l| indent(l));
    let leading = |line: &str| {
        let t = line.trim();
        // ` * text` and ` */` continue a block comment one column in.
        if !hash_comments && t.starts_with('*') {
            return true;
        }
        indent(line) <= def_indent
            && (t.starts_with(comment)
                || alt.is_some_and(|a| t.starts_with(a))
                || openers.iter().any(|o| t.starts_with(o)))
    };
    let floor = start.saturating_sub(MAX_LEADING_LINES);
    let mut first = start;
    while first > floor {
        let line = lines[first - 1];
        let t = line.trim();
        if t.is_empty() {
            break;
        }
        if leading(line) {
            first -= 1;
            continue;
        }
        // The last line of an attribute over several lines.
        let opener = (!hash_comments && t.ends_with(']') && indent(line) <= def_indent).then(|| {
            (floor..first - 1)
                .rev()
                .take_while(|&k| {
                    let t = lines[k].trim();
                    !t.is_empty() && !t.ends_with([';', '{', '}'])
                })
                .find(|&k| lines[k].trim_start().starts_with("#[") && indent(lines[k]) <= def_indent)
        });
        match opener.flatten() {
            Some(k) => first = k,
            None => break,
        }
    }
    first
}

/// Comment prefixes that say why code is the way it is. Such a comment is
/// worth more to a reader than the lines around it, so it is kept with the symbol.
const RATIONALE_MARKERS: [&str; 7] = ["NOTE:", "WHY:", "HACK:", "IMPORTANT:", "RATIONALE:", "TODO:", "FIXME:"];

/// Attach every rationale comment to the innermost symbol that contains its
/// line, as a `rationale` fact. Works on source lines, so it is the same for
/// every language: a comment token (one of `comments`), optional spaces, then a marker.
fn add_rationale(parsed: &mut ParsedFile, rel_path: &str, src: &[u8], comments: &[&str]) {
    let text = String::from_utf8_lossy(src);
    for (i, line) in text.lines().enumerate() {
        let Some(note) = comments.iter().find_map(|c| rationale_in(line, c)) else {
            continue;
        };
        let line_no = i as i64 + 1;
        // The innermost symbol is the one with the smallest range that holds the
        // line; a definition before the module when both span the whole file.
        let owner = parsed
            .symbols
            .iter()
            .filter(|s| s.start_line <= line_no && line_no <= s.end_line)
            .min_by_key(|s| {
                (
                    s.end_line - s.start_line,
                    s.kind == lge_core::kind::MODULE,
                    s.start_line,
                )
            });
        let Some(owner) = owner else { continue };
        parsed.facts.push(facts::fact_site(
            rel_path,
            &owner.id,
            lge_core::fact::RATIONALE,
            &note,
            "",
            (line_no, line_no),
            true,
        ));
    }
    unique_ids(parsed);
}

/// The rationale text of a line, when it has a comment that starts with a
/// marker: `# NOTE: ...`, `// WHY: ...`, ` * HACK: ...`, `-- TODO: ...`,
/// `%% FIXME: ...`, `(* NOTE: ... *)`. At most 200 chars.
fn rationale_in(line: &str, comment: &str) -> Option<String> {
    let trimmed = line.trim_start();
    // A block-comment line (`* NOTE:` or `/* NOTE:`), a whole-line comment, or
    // a trailing comment after code.
    let after = if let Some(rest) = trimmed.strip_prefix("/*").or_else(|| trimmed.strip_prefix('*')) {
        rest
    } else if let Some(rest) = trimmed.strip_prefix(comment) {
        rest
    } else {
        let at = line.find(&format!(" {comment}"))?;
        &line[at + 1 + comment.len()..]
    };
    // A repeated comment token (`%%`, `;;`, `---`) is still the comment token.
    let after = after
        .trim_start_matches(|c: char| matches!(c, '/' | '#' | '*' | '!') || comment.contains(c))
        .trim_start();
    let closer = if comment == "(*" { "*)" } else { "*/" };
    RATIONALE_MARKERS
        .iter()
        .any(|m| after.starts_with(m))
        .then(|| after.trim_end_matches(closer).trim().chars().take(200).collect())
}

/// Read, hash and parse `files` in parallel. The result keeps the order of
/// `files` and pairs each relative path with its outcome; a file that cannot be
/// read or parsed gives an `Err`, and a file in an unsupported language is left out.
pub fn parse_many(files: &[ScannedFile]) -> Vec<(String, Result<ParsedFile>)> {
    files
        .par_iter()
        .filter_map(|f| {
            let parsed = hash_file(&f.abs_path)
                .with_context(|| format!("read {}", f.rel_path))
                .and_then(|(bytes, hash)| parse_file(&f.rel_path, &f.service, &bytes, &hash));
            match parsed {
                Ok(Some(p)) => Some((f.rel_path.clone(), Ok(p))),
                Ok(None) => None,
                Err(e) => Some((f.rel_path.clone(), Err(e))),
            }
        })
        .collect()
}

/// Longest definition body kept on a symbol, in bytes.
pub const BODY_CAP: usize = 4000;

/// The source text of a definition, cut at [`BODY_CAP`] bytes on a char boundary.
pub(crate) fn cap_body(src: &[u8]) -> String {
    // A few bytes of slack so a char that straddles the cap is seen whole.
    let window = &src[..src.len().min(BODY_CAP + 4)];
    let text = String::from_utf8_lossy(window);
    let mut end = text.len().min(BODY_CAP);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// Keeps the first record for each id. Ids are primary keys in the store, and
/// one file can repeat an id: a property and its setter share a qualified name,
/// and the same import can be written twice.
pub(crate) fn unique_ids(parsed: &mut ParsedFile) {
    fn keep_first<T>(items: &mut Vec<T>, id: impl Fn(&T) -> &str) {
        let mut seen = HashSet::new();
        items.retain(|item| seen.insert(id(item).to_string()));
    }
    keep_first(&mut parsed.symbols, |s| &s.id);
    keep_first(&mut parsed.calls, |c| &c.id);
    keep_first(&mut parsed.imports, |i| &i.id);
    keep_first(&mut parsed.facts, |f| &f.id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn rationale_comments_belong_to_the_innermost_symbol() {
        let src = b"# NOTE: module-level reason\nclass Repo:\n    def save(self, row):\n        # WHY: the unique index makes a second insert fail loudly\n        x = 1  # HACK: until the driver is fixed\n        return \"# NOTE: not a comment\"\n\ndef other():\n    # just a comment\n    pass\n";
        let parsed = parse_file("svc/repo.py", "svc", src, "h").unwrap().unwrap();
        let notes: Vec<(String, &str, i64)> = parsed
            .facts
            .iter()
            .filter(|f| f.kind == lge_core::fact::RATIONALE)
            .map(|f| {
                (
                    parsed
                        .symbols
                        .iter()
                        .find(|s| s.id == f.symbol_id)
                        .unwrap()
                        .qualified_name
                        .clone(),
                    f.key.as_str(),
                    f.line,
                )
            })
            .collect();
        assert_eq!(
            notes,
            [
                ("svc.repo".to_string(), "NOTE: module-level reason", 1),
                (
                    "svc.repo.Repo.save".to_string(),
                    "WHY: the unique index makes a second insert fail loudly",
                    4
                ),
                ("svc.repo.Repo.save".to_string(), "HACK: until the driver is fixed", 5),
            ]
        );
        let ts = b"/**\n * IMPORTANT: callers rely on the order of the result.\n */\nexport function list() {\n  // TODO: paginate\n  return [];\n}\n";
        let parsed = parse_file("web/list.ts", "web", ts, "h").unwrap().unwrap();
        let keys: Vec<&str> = parsed
            .facts
            .iter()
            .filter(|f| f.kind == lge_core::fact::RATIONALE)
            .map(|f| f.key.as_str())
            .collect();
        assert_eq!(
            keys,
            ["IMPORTANT: callers rely on the order of the result.", "TODO: paginate"]
        );
        assert_eq!(rationale_in("x = 'NOTE: in a string'", "#"), None);
        assert_eq!(rationale_in("    #   note: lower case is prose", "#"), None);
    }

    #[test]
    fn languages_come_from_the_extension() {
        assert_eq!(lang_of("a/b.py"), Some(lang::PYTHON));
        assert_eq!(lang_of("a/b.ts"), Some(lang::TYPESCRIPT));
        assert_eq!(lang_of("a/b.tsx"), Some(lang::TYPESCRIPT));
        for js in ["x.js", "x.jsx", "x.mjs", "x.cjs"] {
            assert_eq!(lang_of(js), Some(lang::JAVASCRIPT), "{js}");
        }
        // Documents are indexed as outlines.
        assert_eq!(lang_of("README.md"), Some("markdown"));
        assert_eq!(lang_of("web/x.json"), Some("json"));
        assert_eq!(lang_of("index.html"), Some("html"));
        for other in ["notes.txt", "Makefile", "a.py/readme", "x.pyc", "x.lock", ".py"] {
            assert_eq!(lang_of(other), None, "{other}");
        }
        for (path, lang) in [
            ("a/b.go", lang::GO),
            ("a/b.rs", lang::RUST),
            ("A.java", lang::JAVA),
            ("x.h", lang::C),
            ("x.hpp", lang::CPP),
            ("x.R", lang::R),
            ("x.r", lang::R),
            ("x.m", lang::OBJC),
            ("x.exs", lang::ELIXIR),
            ("x.ml", lang::OCAML),
            ("x.ps1", lang::POWERSHELL),
        ] {
            assert_eq!(lang_of(path), Some(lang), "{path}");
        }
        for other in ["x.mli", "x.fsi", "x.ini", "x.cfg", ".rs"] {
            assert_eq!(lang_of(other), None, "{other}");
        }
        for (path, token) in [
            ("a.py", "#"),
            ("a.ts", "//"),
            ("a.rb", "#"),
            ("a.lua", "--"),
            ("a.erl", "%"),
        ] {
            assert_eq!(comment_token(path), Some(token), "{path}");
        }
        assert_eq!(comment_token("README.md"), Some("<!--"));
        assert_eq!(comment_token("notes.txt"), None);
    }

    fn write(root: &Path, rel: &str, text: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    #[test]
    fn scan_returns_supported_files_and_skips_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, ".gitignore", "generated/\n*.log.py\n");
        for kept in [
            "api/app.py",
            "api/pkg/__init__.py",
            "web/src/index.ts",
            "web/src/App.tsx",
            "web/src/legacy.js",
            "web/src/Button.jsx",
            "web/tools/run.mjs",
            "web/tools/conf.cjs",
            "top.py",
        ] {
            write(root, kept, "x = 1\n");
        }
        for skipped in [
            "api/generated/client.py",
            "api/debug.log.py",
            "api/README.txt",
            "api/__pycache__/app.py",
            "api/.venv/lib/site.py",
            "api/venv/lib/site.py",
            "api/build/lib/app.py",
            "web/node_modules/dep/index.js",
            "web/dist/bundle.js",
            "web/src/types.d.ts",
            "web/src/vendor.min.js",
            "web/package-lock.json",
        ] {
            write(root, skipped, "x = 1\n");
        }

        let files = scan(root, 1).unwrap();
        let got: Vec<(&str, &str, &str)> = files
            .iter()
            .map(|f| (f.rel_path.as_str(), f.service.as_str(), f.lang))
            .collect();
        let root_name = root
            .canonicalize()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert_eq!(
            got,
            vec![
                ("api/app.py", "api", lang::PYTHON),
                ("api/pkg/__init__.py", "api", lang::PYTHON),
                ("top.py", root_name.as_str(), lang::PYTHON),
                ("web/src/App.tsx", "web", lang::TYPESCRIPT),
                ("web/src/Button.jsx", "web", lang::JAVASCRIPT),
                ("web/src/index.ts", "web", lang::TYPESCRIPT),
                ("web/src/legacy.js", "web", lang::JAVASCRIPT),
                ("web/tools/conf.cjs", "web", lang::JAVASCRIPT),
                ("web/tools/run.mjs", "web", lang::JAVASCRIPT),
            ]
        );
        assert!(files.iter().all(|f| f.abs_path.is_file()));

        // Depth 0: one service named after the root.
        let flat = scan(root, 0).unwrap();
        assert_eq!(flat.len(), files.len());
        assert!(flat.iter().all(|f| f.service == root_name));
    }

    #[test]
    fn scan_reads_the_other_languages_and_skips_their_dependencies() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for kept in [
            "cmd/main.go",
            "src/lib.rs",
            "app/models/cart.rb",
            "include/cart.h",
            "web/vendor/legacy.js",
            "tools/target/run.py",
        ] {
            write(root, kept, "x\n");
        }
        for skipped in [
            "vendor/github.com/x/y.go",
            "target/debug/build/out.rs",
            "deps/plug/lib/plug.ex",
            "api/order.pb.go",
            "lib/cart.g.dart",
            "ios/Pods/Lib/Lib.m",
        ] {
            write(root, skipped, "x\n");
        }
        let table = format!("int t[] = {{{}}};\n", "1,".repeat(MAX_GENERIC_BYTES as usize / 2));
        write(root, "src/parser.c", &table);
        let got: Vec<(String, &str)> = scan(root, 0)
            .unwrap()
            .into_iter()
            .map(|f| (f.rel_path, f.lang))
            .collect();
        let want: Vec<(String, &str)> = [
            ("app/models/cart.rb", lang::RUBY),
            ("cmd/main.go", lang::GO),
            ("include/cart.h", lang::C),
            ("src/lib.rs", lang::RUST),
            // A dependency directory of another language does not hide these.
            ("tools/target/run.py", lang::PYTHON),
            ("web/vendor/legacy.js", lang::JAVASCRIPT),
        ]
        .into_iter()
        .map(|(p, l)| (p.to_string(), l))
        .collect();
        assert_eq!(got, want);
    }

    #[test]
    fn scan_fails_on_a_missing_root() {
        assert!(scan(Path::new("/definitely/not/a/real/root"), 0).is_err());
    }

    #[test]
    fn hash_file_returns_bytes_and_their_hash() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.py", "x = 1\n");
        let (bytes, hash) = hash_file(&dir.path().join("a.py")).unwrap();
        assert_eq!(bytes, b"x = 1\n");
        assert_eq!(hash, content_hash(b"x = 1\n"));
        assert!(hash_file(&dir.path().join("missing.py")).is_err());
    }

    #[test]
    fn parse_file_dispatches_on_language() {
        let py = parse_file("svc/a.py", "svc", b"def f():\n    pass\n", "h")
            .unwrap()
            .unwrap();
        assert_eq!(py.file.as_ref().map(|f| f.lang.as_str()), Some(lang::PYTHON));
        assert!(py.symbols.iter().any(|s| s.qualified_name == "svc.a.f"));
        for (path, src, lang) in [
            ("svc/a.ts", "export function f(a: number): void {}\n", lang::TYPESCRIPT),
            ("svc/a.tsx", "export const f = () => <div />;\n", lang::TYPESCRIPT),
            ("svc/a.js", "function f() {}\n", lang::JAVASCRIPT),
            ("svc/a.jsx", "const f = () => <div />;\n", lang::JAVASCRIPT),
            ("svc/a.mjs", "export function f() {}\n", lang::JAVASCRIPT),
            ("svc/a.cjs", "exports.f = () => {};\n", lang::JAVASCRIPT),
        ] {
            let parsed = parse_file(path, "svc", src.as_bytes(), "h").unwrap().unwrap();
            assert_eq!(parsed.file.as_ref().map(|f| f.lang.as_str()), Some(lang), "{path}");
            assert!(parsed.symbols.iter().any(|s| s.qualified_name == "svc.a.f"), "{path}");
        }
        assert!(parse_file("svc/notes.txt", "svc", b"hi", "h").unwrap().is_none());
        let doc = parse_file("svc/notes.md", "svc", b"# hi", "h").unwrap().unwrap();
        assert!(doc
            .symbols
            .iter()
            .any(|s| s.kind == lge_core::kind::SECTION && s.name == "hi"));
    }

    #[test]
    fn parse_many_keeps_order_and_reports_unreadable_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "svc/b.py", "def b():\n    pass\n");
        write(root, "svc/a.py", "def a():\n    pass\n");
        write(root, "svc/c.ts", "export const c = (): number => 1;\n");
        write(root, "svc/d.js", "function d() {}\n");
        write(root, "svc/notes.txt", "not source\n");
        let mut files = scan(root, 1).unwrap();
        let extra = |rel: &str| ScannedFile {
            rel_path: rel.into(),
            abs_path: root.join(rel),
            service: "svc".into(),
            lang: lang::PYTHON,
        };
        // A language no extractor knows is left out; a file that is gone is an error.
        files.push(extra("svc/notes.txt"));
        files.push(extra("svc/gone.py"));

        let parsed = parse_many(&files);
        let paths: Vec<&str> = parsed.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            paths,
            vec!["svc/a.py", "svc/b.py", "svc/c.ts", "svc/d.js", "svc/gone.py"]
        );
        let a = parsed[0].1.as_ref().unwrap();
        let file = a.file.as_ref().unwrap();
        assert_eq!(file.content_hash, content_hash(b"def a():\n    pass\n"));
        assert_eq!(file.service, "svc");
        assert!(a.symbols.iter().any(|s| s.qualified_name == "svc.a.a"));
        let c = parsed[2].1.as_ref().unwrap();
        let file = c.file.as_ref().unwrap();
        assert_eq!(file.lang, lang::TYPESCRIPT);
        assert_eq!(file.content_hash, content_hash(b"export const c = (): number => 1;\n"));
        assert!(c.symbols.iter().any(|s| s.qualified_name == "svc.c.c"));
        let d = parsed[3].1.as_ref().unwrap();
        assert_eq!(d.file.as_ref().map(|f| f.lang.as_str()), Some(lang::JAVASCRIPT));
        assert!(d.symbols.iter().any(|s| s.qualified_name == "svc.d.d"));
        assert!(parsed[4].1.is_err());
    }

    #[test]
    fn bodies_are_cut_on_a_char_boundary() {
        assert_eq!(cap_body(b"def f(): pass"), "def f(): pass");
        let long = "x".repeat(BODY_CAP + 100);
        assert_eq!(cap_body(long.as_bytes()).len(), BODY_CAP);
        // 'é' is two bytes and straddles the cap: it is dropped, not split.
        let straddle = format!("{}é tail", "x".repeat(BODY_CAP - 1));
        let cut = cap_body(straddle.as_bytes());
        assert_eq!(cut.len(), BODY_CAP - 1);
        assert!(cut.chars().all(|c| c == 'x'));
        // Bytes that are not UTF-8 are replaced, never a panic.
        assert!(cap_body(&[0xff, b'a']).ends_with('a'));
    }

    #[test]
    fn leading_lines_are_the_comments_and_attributes_right_above() {
        let start = |path: &str, text: &str, line: usize| {
            let lines: Vec<&str> = text.lines().collect();
            leading_start(path, &lines, line)
        };
        let rust = "use x;\n\n/// Doc.\n///\n#[inline]\n#[cfg_attr(\n    test,\n    derive(Debug)\n)]\npub fn f() {}\n";
        assert_eq!(start("a.rs", rust, 9), 2, "doc, attribute, attribute over lines");
        // A blank line or code ends them.
        assert_eq!(start("a.rs", "/// Old.\n\nfn f() {}\n", 2), 2);
        assert_eq!(start("a.rs", "let v = [1];\nfn f() {}\n", 1), 1);
        assert_eq!(start("a.rs", "}\n// Note.\nfn f() {}\n", 2), 1);
        // The language's own comment token: `#` in Python, never in C.
        assert_eq!(start("a.py", "x = 1\n# Why.\n@app.get(\"/\")\ndef f():\n", 3), 1);
        assert_eq!(start("a.c", "#include <x.h>\nint f() {}\n", 1), 1);
        assert_eq!(start("a.c", "/**\n * Doc.\n */\nint f() {}\n", 3), 0);
        // At the top of a file, past its end, and in a file that is not indexed.
        assert_eq!(start("a.rs", "fn f() {}\n", 0), 0);
        assert_eq!(start("a.rs", "// a\n", 9), 0);
        assert_eq!(start("a.unknown", "// a\nf\n", 1), 1);
        // A long comment: only the lines nearest the definition.
        let long = format!("{}fn f() {{}}\n", "// x\n".repeat(100));
        assert_eq!(start("a.rs", &long, 100), 100 - MAX_LEADING_LINES);
        // The end of the block above (indented deeper) is not this one's doc.
        assert_eq!(start("a.py", "def g():\n    y = 1\n    # end of g\ndef f():\n", 3), 3);
        assert_eq!(start("a.py", "def g():\n    return (a\n        * b)\ndef f():\n", 3), 3);
        assert_eq!(
            start(
                "a.rs",
                "impl A {\n    fn g() {\n        // x\n    }\n    /// F.\n    fn f() {}\n",
                5
            ),
            4
        );
        // Where `#` is a comment, `#[` opens no attribute: the list stays out.
        assert_eq!(start("a.py", "#[x]\nL = [\n    1,\n]\ndef f():\n", 4), 4);
        // A document has no leading lines: they end the section before.
        assert_eq!(start("a.md", "## A\n* item\n## B\n", 2), 2);
        assert_eq!(start("a.yaml", "a: 1\n# about b\nb: 2\n", 2), 2);
    }
}
