//! Repository scanner, content hashing, git change detection and Tree-sitter
//! extraction of symbols, call sites, import sites and fact sites.
//!
//! This crate only reads source files. It never resolves anything: what it
//! returns is what is written in one file, and `lge-resolve` turns that into edges.

pub mod facts;
pub mod git;
mod python;
mod typescript;

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
        _ => None,
    }
}

/// Directories that hold dependencies or build output, never source to index.
const SKIP_DIRS: &[&str] = &["node_modules", "dist", "build", ".venv", "venv", "__pycache__"];

/// Generated files that share an extension with real source.
fn is_generated(file_name: &str) -> bool {
    file_name.ends_with(".d.ts") || file_name.ends_with(".min.js")
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
        if is_generated(&rel) {
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
    match lang_of(rel_path) {
        Some(lang::PYTHON) => parse_python(rel_path, service, src, hash).map(Some),
        Some(lang::TYPESCRIPT | lang::JAVASCRIPT) => parse_typescript(rel_path, service, src, hash).map(Some),
        _ => Ok(None),
    }
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
pub(crate) const BODY_CAP: usize = 4000;

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
    fn languages_come_from_the_extension() {
        assert_eq!(lang_of("a/b.py"), Some(lang::PYTHON));
        assert_eq!(lang_of("a/b.ts"), Some(lang::TYPESCRIPT));
        assert_eq!(lang_of("a/b.tsx"), Some(lang::TYPESCRIPT));
        for js in ["x.js", "x.jsx", "x.mjs", "x.cjs"] {
            assert_eq!(lang_of(js), Some(lang::JAVASCRIPT), "{js}");
        }
        for other in ["README.md", "Makefile", "a.py/readme", "x.pyc", "x.json", ".py"] {
            assert_eq!(lang_of(other), None, "{other}");
        }
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
            "api/README.md",
            "api/__pycache__/app.py",
            "api/.venv/lib/site.py",
            "api/venv/lib/site.py",
            "api/build/lib/app.py",
            "web/node_modules/dep/index.js",
            "web/dist/bundle.js",
            "web/src/types.d.ts",
            "web/src/vendor.min.js",
            "web/package.json",
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
        assert!(parse_file("svc/notes.md", "svc", b"# hi", "h").unwrap().is_none());
    }

    #[test]
    fn parse_many_keeps_order_and_reports_unreadable_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "svc/b.py", "def b():\n    pass\n");
        write(root, "svc/a.py", "def a():\n    pass\n");
        write(root, "svc/c.ts", "export const c = (): number => 1;\n");
        write(root, "svc/d.js", "function d() {}\n");
        write(root, "svc/notes.md", "# not source\n");
        let mut files = scan(root, 1).unwrap();
        let extra = |rel: &str| ScannedFile {
            rel_path: rel.into(),
            abs_path: root.join(rel),
            service: "svc".into(),
            lang: lang::PYTHON,
        };
        // A language no extractor knows is left out; a file that is gone is an error.
        files.push(extra("svc/notes.md"));
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
}
