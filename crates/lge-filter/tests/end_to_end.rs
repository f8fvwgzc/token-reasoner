//! From a repository to the text for the model: configuration, running, filtering,
//! diff hunks and rendering together. The checks are shell commands that replay the
//! output of real tools, so nothing but `sh` and `git` has to be installed.

#![cfg(unix)]

use lge_filter::{detect_checks, load_config, render, run_all, CheckKind, CheckSpec, Report};
use std::fs;
use std::path::Path;
use std::process::Command;

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn git(root: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

const PRICING_BEFORE: &str = "def apply_discount(price, percent):\n    return price * (1 - percent / 100)\n";
const PRICING_AFTER: &str = "def apply_discount(price, percent):\n    rate = percent / 100\n    return price\n";
const TEST_PRICING: &str = "from shop.pricing import apply_discount\n\n\ndef test_discount():\n    assert apply_discount(100.0, 10) == 90.0\n\n\ndef test_zero():\n    assert apply_discount(100.0, 0) == 100.0\n";

const RUFF_OUTPUT: &str = "shop/pricing.py:2:5: F841 Local variable `rate` is assigned to but never used\nFound 1 error.\nNo fixes available (1 hidden fix can be enabled with the `--unsafe-fixes` option).\n";

const PYTEST_OUTPUT: &str = "\
============================= test session starts ==============================
platform linux -- Python 3.11.9, pytest-8.2.2, pluggy-1.5.0
rootdir: /home/dev/shop
collected 2 items

tests/test_pricing.py F.                                                 [100%]

=================================== FAILURES ===================================
________________________________ test_discount _________________________________

    def test_discount():
>       assert apply_discount(100.0, 10) == 90.0
E       assert 100.0 == 90.0
E        +  where 100.0 = apply_discount(100.0, 10)

tests/test_pricing.py:5: AssertionError
=========================== short test summary info ============================
FAILED tests/test_pricing.py::test_discount - assert 100.0 == 90.0
========================= 1 failed, 1 passed in 0.03s ==========================
";

/// A committed project with one uncommitted change that breaks a lint rule and a test.
fn project(root: &Path) {
    git(root, &["init", "--quiet"]);
    write(root, "shop/pricing.py", PRICING_BEFORE);
    write(root, "shop/cart.py", "ITEMS = []\n");
    write(root, "tests/test_pricing.py", TEST_PRICING);
    write(root, ".gitignore", "replay/\nlge.toml\n");
    git(root, &["add", "--all"]);
    git(root, &["commit", "--quiet", "--message", "first"]);

    write(root, "shop/pricing.py", PRICING_AFTER);
    // Changed too, but no failure names it: it must stay out of the report.
    write(root, "shop/cart.py", "ITEMS = [1]\n");
    write(root, "replay/ruff.txt", RUFF_OUTPUT);
    write(root, "replay/pytest.txt", PYTEST_OUTPUT);
}

const CONFIG: &str = r#"
[prices]
input = 1.25

[[check]]
name = "ruff"
kind = "lint"
command = "cat replay/ruff.txt; exit 1"

[[check]]
name = "mypy"
kind = "types"
command = "echo 'Success: no issues found in 3 source files'"

[[check]]
name = "pytest"
kind = "tests"
command = "cat replay/pytest.txt >&2; exit 1"
timeout_secs = 30
"#;

fn expected_text(raw: usize, filtered: usize) -> String {
    format!(
        "\
[lint] ruff: FAIL 1 failure (exit 1)
shop/pricing.py
  2: F841: Local variable `rate` is assigned to but never used
summary: Found 1 error.
[types] mypy: PASS (no issues found in 3 source files)
[tests] pytest: FAIL 1 failure (exit 1)
tests/test_pricing.py
  5: tests/test_pricing.py::test_discount: assert 100.0 == 90.0
      >       assert apply_discount(100.0, 10) == 90.0
      E       assert 100.0 == 90.0
      E        +  where 100.0 = apply_discount(100.0, 10)
      tests/test_pricing.py:5: AssertionError
summary: 1 failed, 1 passed
diff of the files named above:
--- a/shop/pricing.py
+++ b/shop/pricing.py
@@ -1,2 +1,3 @@
 def apply_discount(price, percent):
-    return price * (1 - percent / 100)
+    rate = percent / 100
+    return price
raw {raw} bytes, filtered {filtered} bytes
"
    )
}

#[test]
fn configured_checks_to_rendered_report() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    project(root);
    write(root, "lge.toml", CONFIG);

    let specs = load_config(root).unwrap().expect("lge.toml has checks");
    assert_eq!(specs.len(), 3);
    let report = run_all(root, &specs).unwrap();

    assert!(!report.passed);
    let outcome: Vec<(&str, CheckKind, bool, Option<i32>, usize)> = report
        .results
        .iter()
        .map(|r| (r.name.as_str(), r.kind, r.passed, r.exit_code, r.failures.len()))
        .collect();
    assert_eq!(
        outcome,
        [
            ("ruff", CheckKind::Lint, false, Some(1), 1),
            ("mypy", CheckKind::Types, true, Some(0), 0),
            ("pytest", CheckKind::Tests, false, Some(1), 1),
        ]
    );
    let raw = RUFF_OUTPUT.len() + "Success: no issues found in 3 source files\n".len() + PYTEST_OUTPUT.len();
    assert_eq!(report.raw_bytes, raw);
    assert_eq!(report.results[2].raw_lines, PYTEST_OUTPUT.lines().count());

    let text = render(&report);
    assert_eq!(text, expected_text(raw, text.len()));
    assert_eq!(report.filtered_bytes, text.len());
    // The file changed without a failure naming it is not in the report.
    assert!(!text.contains("cart.py"));

    // The same repository state gives the same bytes again.
    let again = run_all(root, &specs).unwrap();
    assert_eq!(render(&again), text);

    // The report is plain data for the caller to store.
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["results"][0]["kind"], "lint");
    assert_eq!(
        json["results"][2]["failures"][0]["name"],
        "tests/test_pricing.py::test_discount"
    );
    assert_eq!(json["results"][2]["failures"][0]["line"], 5);
    let back: Report = serde_json::from_value(json).unwrap();
    assert_eq!(back, report);
}

#[test]
fn everything_passing_is_a_few_lines() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    project(root);
    let spec = |name: &str, kind, command: &str| CheckSpec {
        name: name.into(),
        kind,
        command: command.into(),
        cwd: root.to_path_buf(),
        timeout_secs: 30,
    };
    let specs = [
        spec("lint", CheckKind::Lint, "echo 'All checks passed!'"),
        spec("types", CheckKind::Types, "true"),
        spec("tests", CheckKind::Tests, "i=0; while [ $i -lt 500 ]; do echo \"test case_$i ... ok\"; i=$((i+1)); done; echo; echo 'test result: ok. 500 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.50s'"),
    ];
    let report = run_all(root, &specs).unwrap();
    assert!(report.passed);
    assert_eq!(report.diff_hunks, "", "no failure, no diff");
    let text = render(&report);
    assert_eq!(
        text,
        format!(
            "[lint] lint: PASS (All checks passed!)\n[types] types: PASS\n[tests] tests: PASS (500 passed; 0 failed; 0 ignored)\nraw {} bytes, filtered {} bytes\n",
            report.raw_bytes,
            text.len()
        )
    );
    assert!(report.raw_bytes > 10_000 && report.filtered_bytes < 200);
}

#[test]
fn service_directories_report_paths_from_the_repository_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "--quiet"]);
    write(root, "orders_api/shop/pricing.py", PRICING_BEFORE);
    write(root, "orders_api/tests/test_pricing.py", TEST_PRICING);
    write(root, "web_gateway/src/routes.ts", "export const total: number = 1;\n");
    git(root, &["add", "--all"]);
    git(root, &["commit", "--quiet", "--message", "first"]);
    write(root, "orders_api/shop/pricing.py", PRICING_AFTER);
    write(
        root,
        "web_gateway/src/routes.ts",
        "export const total: number = \"1\";\n",
    );
    // Not committed and named by a failure: shown as a new file.
    write(
        root,
        "web_gateway/src/views.ts",
        "export function render(): number {\n  return \"x\";\n}\n",
    );

    let traceback = PYTEST_OUTPUT.replace(
        "\ntests/test_pricing.py:5: AssertionError",
        "\ntests/test_pricing.py:5: \n_ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ \n\n>       return price\nE       AssertionError\n\nshop/pricing.py:3: AssertionError",
    );
    write(root, ".replay/pytest.txt", &traceback);
    write(
        root,
        ".replay/tsc.txt",
        "src/routes.ts(1,14): error TS2322: Type 'string' is not assignable to type 'number'.\nsrc/views.ts(2,3): error TS2322: Type 'string' is not assignable to type 'number'.\n",
    );
    let specs = [
        CheckSpec {
            name: "orders_api/pytest".into(),
            kind: CheckKind::Tests,
            command: "cat ../.replay/pytest.txt; exit 1".into(),
            cwd: root.join("orders_api"),
            timeout_secs: 30,
        },
        CheckSpec {
            name: "web_gateway/tsc".into(),
            kind: CheckKind::Types,
            command: "cat ../.replay/tsc.txt; exit 2".into(),
            cwd: root.join("web_gateway"),
            timeout_secs: 30,
        },
    ];
    let report = run_all(root, &specs).unwrap();
    let files: Vec<&str> = report
        .results
        .iter()
        .flat_map(|r| r.failures.iter().map(|f| f.file.as_str()))
        .collect();
    assert_eq!(
        files,
        [
            "orders_api/tests/test_pricing.py",
            "web_gateway/src/routes.ts",
            "web_gateway/src/views.ts"
        ]
    );

    let text = render(&report);
    assert!(text.contains("[tests] orders_api/pytest: FAIL 1 failure (exit 1)\norders_api/tests/test_pricing.py\n  5: tests/test_pricing.py::test_discount: assert 100.0 == 90.0\n"));
    assert!(
        text.contains("[types] web_gateway/tsc: FAIL 2 failures (exit 2)\nweb_gateway/src/routes.ts\n  1: TS2322: ")
    );
    // The traceback names shop/pricing.py relative to the service: its hunk is found
    // from the root, next to the hunks of the files with errors, and the new file.
    let diff_files: Vec<&str> = report.diff_hunks.lines().filter(|l| l.starts_with("+++ ")).collect();
    assert_eq!(
        diff_files,
        [
            "+++ b/web_gateway/src/routes.ts",
            "+++ b/web_gateway/src/views.ts",
            "+++ b/orders_api/shop/pricing.py"
        ]
    );
    assert!(report.diff_hunks.contains(
        "--- /dev/null\n+++ b/web_gateway/src/views.ts\n@@ -0,0 +1,3 @@\n+export function render(): number {\n"
    ));
    assert_eq!(report.filtered_bytes, text.len());
}

#[test]
fn outside_a_repository_there_is_no_diff_and_no_error() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "a.py", "import os\n");
    let spec = CheckSpec {
        name: "lint".into(),
        kind: CheckKind::Lint,
        command: "echo 'a.py:1:8: F401 unused import'; exit 1".into(),
        cwd: dir.path().to_path_buf(),
        timeout_secs: 30,
    };
    let report = run_all(dir.path(), &[spec]).unwrap();
    assert_eq!(report.diff_hunks, "");
    let text = render(&report);
    assert_eq!(
        text,
        format!(
            "[lint] lint: FAIL 1 failure (exit 1)\na.py\n  1: F401: unused import\nraw 29 bytes, filtered {} bytes\n",
            text.len()
        )
    );
}

#[test]
fn no_configuration_and_nothing_to_detect_is_an_empty_report() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "notes.txt", "nothing to check here\n");
    assert_eq!(load_config(dir.path()).unwrap(), None);
    let specs = detect_checks(dir.path());
    assert!(specs.is_empty());
    let report = run_all(dir.path(), &specs).unwrap();
    assert!(report.passed && report.results.is_empty());
    assert_eq!(render(&report), "no checks ran\nraw 0 bytes, filtered 45 bytes\n");
}

#[test]
fn a_check_that_cannot_start_is_an_error_not_a_failure() {
    let dir = tempfile::tempdir().unwrap();
    let spec = CheckSpec {
        name: "lint".into(),
        kind: CheckKind::Lint,
        command: "true".into(),
        cwd: dir.path().join("no-such-service"),
        timeout_secs: 30,
    };
    let error = run_all(dir.path(), &[spec]).unwrap_err();
    assert!(format!("{error:#}").contains("no-such-service"));
}
