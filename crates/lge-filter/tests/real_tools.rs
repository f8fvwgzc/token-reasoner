//! Detection, running and parsing against the real tools, for the ones a developer
//! machine usually has: tsc, npm and cargo. Ignored by default because they need those
//! tools and compile a small crate; run them with
//! `cargo test -p lge-filter --test real_tools -- --ignored`.
//!
//! pytest, ruff, flake8, mypy, pyright, eslint, jest and vitest are covered only by the
//! recorded samples in `testdata/`.

#![cfg(unix)]

use lge_filter::{detect_checks, render, run_all, CheckKind};
use std::fs;
use std::path::Path;

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn installed(tool: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(tool).is_file()))
}

#[test]
#[ignore = "needs tsc on PATH"]
fn tsc_through_detection() {
    if !installed("tsc") {
        eprintln!("tsc is not installed: skipped");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "package.json", r#"{"name": "web", "private": true}"#);
    write(
        root,
        "tsconfig.json",
        r#"{"compilerOptions": {"strict": true, "noEmit": true}, "include": ["src"]}"#,
    );
    write(
        root,
        "src/routes.ts",
        "export const total: number = \"1\";\nexport const ok = 1;\n",
    );
    write(
        root,
        "src/views.ts",
        "export function render(): number {\n  return missing;\n}\n",
    );

    let specs = detect_checks(root);
    let commands: Vec<&str> = specs.iter().map(|s| s.command.as_str()).collect();
    assert_eq!(commands, ["tsc --noEmit"]);

    let report = run_all(root, &specs).unwrap();
    let text = render(&report);
    let failures = &report.results[0].failures;
    let got: Vec<(&str, Option<u32>, &str)> = failures
        .iter()
        .map(|f| (f.file.as_str(), f.line, f.name.as_str()))
        .collect();
    assert_eq!(
        got,
        [
            ("src/routes.ts", Some(1), "TS2322"),
            ("src/views.ts", Some(2), "TS2304")
        ],
        "{text}"
    );
    assert!(text.starts_with("[types] tsc: FAIL 2 failures (exit 2)\nsrc/routes.ts\n  1: TS2322: Type 'string' is not assignable to type 'number'.\n"));

    // Fixed: the same check passes and is one line.
    write(root, "src/routes.ts", "export const total: number = 1;\n");
    write(
        root,
        "src/views.ts",
        "export function render(): number {\n  return 1;\n}\n",
    );
    let report = run_all(root, &specs).unwrap();
    assert!(report.passed);
    assert!(render(&report).starts_with("[types] tsc: PASS\nraw 0 bytes"));
}

#[test]
#[ignore = "needs npm and tsc on PATH"]
fn npm_scripts_are_sniffed() {
    if !installed("npm") || !installed("tsc") {
        eprintln!("npm or tsc is not installed: skipped");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "package.json",
        r#"{"name": "web", "private": true, "scripts": {"typecheck": "tsc --noEmit", "test": "node -e \"console.error('Error: boom from the test runner'); process.exit(3)\""}}"#,
    );
    write(
        root,
        "tsconfig.json",
        r#"{"compilerOptions": {"strict": true, "noEmit": true}, "include": ["src"]}"#,
    );
    write(root, "src/routes.ts", "export const total: number = \"1\";\n");

    let specs = detect_checks(root);
    let found: Vec<(&str, CheckKind, &str)> = specs
        .iter()
        .map(|s| (s.name.as_str(), s.kind, s.command.as_str()))
        .collect();
    assert_eq!(
        found,
        [
            ("npm:typecheck", CheckKind::Types, "npm run typecheck"),
            ("npm:test", CheckKind::Tests, "npm run test"),
        ]
    );
    let report = run_all(root, &specs).unwrap();
    let text = render(&report);
    // npm wraps tsc's output in its own lines; the tsc error is still found.
    let types = &report.results[0];
    assert_eq!(types.failures.len(), 1, "{text}");
    assert_eq!(
        (types.failures[0].file.as_str(), types.failures[0].name.as_str()),
        ("src/routes.ts", "TS2322")
    );
    // An unknown runner that fails: its output is kept by the generic filter.
    let tests = &report.results[1];
    assert!(!tests.passed && tests.failures.is_empty());
    assert!(tests.filtered.contains("Error: boom from the test runner"), "{text}");
}

#[test]
#[ignore = "needs cargo and clippy, and compiles a crate"]
fn cargo_through_detection() {
    if !installed("cargo") || !installed("cargo-clippy") {
        eprintln!("cargo or clippy is not installed: skipped");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "Cargo.toml",
        "[package]\nname = \"shop\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    );
    write(
        root,
        "src/lib.rs",
        "pub fn add(a: i32, b: i32) -> i32 {\n    return a - b;\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn adds() {\n        assert_eq!(add(2, 2), 4);\n    }\n\n    #[test]\n    fn adds_zero() {\n        assert_eq!(add(2, 0), 2);\n    }\n}\n",
    );

    let specs = detect_checks(root);
    let commands: Vec<&str> = specs.iter().map(|s| s.command.as_str()).collect();
    assert_eq!(commands, ["cargo clippy --all-targets -- -D warnings", "cargo test"]);

    let report = run_all(root, &specs).unwrap();
    let text = render(&report);
    let clippy = &report.results[0];
    assert_eq!(clippy.failures.len(), 1, "{text}");
    assert_eq!(
        (
            clippy.failures[0].file.as_str(),
            clippy.failures[0].line,
            clippy.failures[0].name.as_str()
        ),
        ("src/lib.rs", Some(2), "clippy::needless_return")
    );
    let tests = &report.results[1];
    assert_eq!(tests.failures.len(), 1, "{text}");
    assert_eq!(
        (
            tests.failures[0].file.as_str(),
            tests.failures[0].line,
            tests.failures[0].name.as_str()
        ),
        ("src/lib.rs", Some(11), "tests::adds")
    );
    assert!(text.contains("[lint] clippy: FAIL 1 failure (exit 101)\n"), "{text}");
    assert!(
        text.contains("[tests] cargo-test: FAIL 1 failure (exit 101)\n"),
        "{text}"
    );
    assert!(report.filtered_bytes < report.raw_bytes, "{text}");
    eprintln!("{text}");
}
