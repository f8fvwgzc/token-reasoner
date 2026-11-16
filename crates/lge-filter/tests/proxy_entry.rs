//! The proxy's entry point: the same text as `lge check` unless the proxy asks for
//! more, warnings listed by rule when asked, and where the tool's closing line is.

use lge_filter::{filter_for_proxy, filter_with, FilterOptions, MAX_FILTERED_BYTES};
use std::path::Path;

fn opts(passed: Option<bool>, keep_warnings: bool) -> FilterOptions {
    FilterOptions {
        passed,
        max_bytes: MAX_FILTERED_BYTES,
        keep_warnings,
    }
}

/// Every sample in `testdata`, with the tool its name starts with.
fn samples() -> Vec<(String, String, String)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata");
    let mut out: Vec<(String, String, String)> = std::fs::read_dir(dir)
        .expect("testdata is there")
        .map(|e| e.expect("readable entry").path())
        .map(|path| {
            let name = path.file_stem().unwrap().to_string_lossy().to_string();
            let tool = match name.split('_').next().unwrap() {
                "clippy" => "cargo",
                "golangci" => "golangci-lint",
                "maven" => "mvn",
                tool => tool,
            }
            .to_string();
            (name, tool, std::fs::read_to_string(&path).expect("utf-8 sample"))
        })
        .collect();
    out.sort();
    out
}

#[test]
fn without_keep_warnings_the_text_is_lge_checks_to_the_byte() {
    for (name, tool, raw) in samples() {
        for passed in [None, Some(false), Some(true)] {
            let (failures, text) = filter_with(&tool, &raw, &opts(passed, false));
            let proxy = filter_for_proxy(&tool, &raw, &opts(passed, false));
            assert_eq!(proxy.failures, failures, "{name} {passed:?}");
            // The one thing the proxy adds: how many cargo test binaries a total covers.
            let suites = ["2", "3"]
                .iter()
                .fold(proxy.text.clone(), |t, n| t.replace(&format!(" ({n} suites)"), ""));
            assert_eq!(suites, text, "{name} {passed:?}");
            // Under `lge check`, warnings never show while there are errors or a pass.
            let (_, kept) = filter_with(&tool, &raw, &opts(passed, true));
            if !kept.contains("warnings: ") {
                assert_eq!(kept, text, "{name} {passed:?}");
            }
        }
    }
}

#[test]
fn a_summary_over_several_test_binaries_names_them_for_the_proxy_only() {
    let raw = include_str!("../testdata/cargo_test_pass.txt");
    let (_, text) = filter_with("cargo", raw, &opts(Some(true), false));
    assert_eq!(text, "ok: 3 passed; 0 failed; 1 ignored");
    let proxy = filter_for_proxy("cargo", raw, &opts(Some(true), false));
    assert_eq!(proxy.text, "ok: 3 passed; 0 failed; 1 ignored (2 suites)");
    assert!(proxy.recognised && proxy.closed && !proxy.trailing);

    let raw = include_str!("../testdata/cargo_test_fail.txt");
    let proxy = filter_for_proxy("cargo", raw, &opts(Some(false), false));
    assert!(proxy
        .text
        .ends_with("summary: 2 passed; 6 failed; 0 ignored (3 suites)\n"));
    // One binary: no count.
    let one = "running 1 test\ntest a ... ok\n\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n";
    assert_eq!(
        filter_for_proxy("cargo", one, &opts(None, false)).text,
        "ok: 1 passed; 0 failed; 0 ignored"
    );
}

/// `(sample, closed, trailing)` for real captures.
#[test]
fn closing_lines_of_real_captures() {
    let expected = [
        ("pytest_pass", true, false),
        ("pytest_fail_long", true, false),
        ("pytest_usage_error", false, false),
        ("cargo_test_pass", true, false),
        ("cargo_test_fail", true, false),
        // The binary died: what cargo says after the last result is not its epilogue.
        ("cargo_test_abort", true, true),
        ("cargo_build_err", false, false),
        ("clippy_pass", true, false),
        ("jest_pass", true, false),
        ("jest_fail", true, false),
        ("vitest_pass", true, false),
        ("vitest_fail", true, false),
        ("tsc_fail", false, false),
        ("tsc_fail_pretty", true, false),
        ("eslint_fail_stylish", true, false),
        // `ESLint found too many warnings`: the run failed after its closing line.
        ("eslint_warnings_only", true, true),
        ("pyright_pass", true, false),
        ("mypy_pass", true, false),
        ("mypy_fail", true, false),
        ("ruff_pass", true, false),
        ("ruff_fail_concise", true, false),
        ("flake8_fail", false, false),
        ("go_pass", true, false),
        ("go_fail", true, false),
        ("go_fail_v", true, false),
        ("go_pass_json", true, false),
        ("go_fail_json", true, false),
        ("go_vet", false, false),
        ("golangci_v1", false, false),
        ("golangci_v2", true, false),
        ("maven_pass", true, false),
        ("maven_test_fail", true, false),
        ("maven_no_footer", false, false),
        ("gradle_pass", true, false),
        ("gradle_test_fail", true, false),
        ("clang_fail", true, false),
        ("clang_warn", true, false),
        ("gcc_fail", false, false),
    ];
    let all = samples();
    for (sample, closed, trailing) in expected {
        let (_, tool, raw) = all.iter().find(|(n, _, _)| n == sample).expect(sample);
        let got = filter_for_proxy(tool, raw, &FilterOptions::default());
        assert_eq!((got.closed, got.trailing), (closed, trailing), "{sample}");
    }
}

#[test]
fn another_program_after_the_closing_line_is_trailing() {
    // `cargo run`: a build warning, then the program's own output and its panic.
    let cargo_run = "warning: unused variable: `x`\n --> src/main.rs:3:9\n  |\n3 |     let x = 1;\n  |         ^\n  |\n  = note: `#[warn(unused_variables)]` on by default\n\nwarning: `report` (bin \"report\") generated 1 warning\n    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.31s\n     Running `target/debug/report`\nrow 1\nrow 2\nthread 'main' panicked at src/main.rs:9:5:\nindex out of bounds\n";
    let got = filter_for_proxy("cargo run --bin report", cargo_run, &opts(None, false));
    assert!(got.closed && got.trailing, "{got:?}");
    assert!(
        got.text.starts_with("ok: 1 warnings"),
        "the verdict flags, not the text, carry the doubt"
    );

    // `pytest -q && python scripts/report.py`
    let chained = "............                                                             [100%]\n12 passed in 0.31s\nTraceback (most recent call last):\n  File \"scripts/report.py\", line 3, in <module>\n    main()\nKeyError: 'total'\n";
    let got = filter_for_proxy("pytest -q && python scripts/report.py", chained, &opts(None, false));
    assert!(got.closed && got.trailing);

    // What script runners, make, Codex and lge print after a tool is not another program.
    let jest = include_str!("../testdata/jest_fail.txt").to_string()
        + "npm error Lifecycle script `test` failed with error:\nnpm error code 1\n ELIFECYCLE  Test failed.\nmake: *** [Makefile:4: test] Error 1\n…312 tokens truncated…\nlge: note\n";
    for tool in ["jest", "npm"] {
        let got = filter_for_proxy(tool, &jest, &opts(Some(false), false));
        assert!(got.closed && !got.trailing, "{tool}");
    }

    // A run cut before its closing line is never closed.
    let cut = "============================= test session starts ==============================\ncollected 3 items\n\ntests/test_a.py ..";
    let got = filter_for_proxy("pytest", cut, &opts(None, false));
    assert!(got.recognised && !got.closed);
}

/// `cargo clippy` with `n` warnings over three lints and exit code 0.
fn clippy_warnings(n: usize) -> String {
    let mut out = String::from("    Checking shop v0.1.0 (/home/dev/shop)\n");
    let lints = [
        ("needless_return", "unneeded `return` statement", "remove `return`"),
        ("redundant_clone", "redundant clone", "remove this"),
        (
            "len_zero",
            "length comparison to zero",
            "using `is_empty` is clearer and more explicit",
        ),
    ];
    for i in 0..n {
        let (lint, message, help) = lints[i % 3];
        out.push_str(&format!(
            "warning: {message}\n  --> src/m{i}.rs:{}:5\n   |\n{} |     return value_{i}.clone();\n   |     ^^^^^^^^^^^^^^^^^^^^^^^^\n   |\n   = help: for further information visit https://rust-lang.github.io/rust-clippy/master/index.html#{lint}\n   = note: `#[warn(clippy::{lint})]` on by default\nhelp: {help}\n   |\n{} -     return value_{i}.clone();\n{} +     value_{i}\n   |\n\n",
            i + 10,
            i + 10,
            i + 10,
            i + 10
        ));
    }
    out.push_str(&format!(
        "warning: `shop` (lib) generated {n} warnings (run `cargo clippy --fix --lib -p shop` to apply {n} suggestions)\n    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.52s\n"
    ));
    out
}

#[test]
fn clippy_warnings_keep_every_location() {
    let raw = clippy_warnings(30);
    let got = filter_for_proxy("cargo clippy --all-targets", &raw, &opts(Some(true), true));
    assert!(got.failures.is_empty() && got.closed && !got.trailing);
    assert_eq!(got.warnings, 30);
    assert!(
        got.text.starts_with("ok: 30 warnings\nwarnings: 30 (3 rules)\n"),
        "{}",
        got.text
    );
    for i in 0..30 {
        assert!(
            got.text.contains(&format!("src/m{i}.rs:{}", i + 10)),
            "lost warning {i}:\n{}",
            got.text
        );
    }
    for (lint, help) in [
        ("needless_return", "help: remove `return`"),
        ("redundant_clone", "help: remove this"),
        ("len_zero", "help: using `is_empty` is clearer and more explicit"),
    ] {
        assert_eq!(got.text.matches(&format!("clippy::{lint} (10): ")).count(), 1, "{lint}");
        assert_eq!(got.text.matches(help).count(), 1, "{help}");
    }
    assert!(
        got.text.len() * 10 <= raw.len() * 3,
        "{} of {} bytes",
        got.text.len(),
        raw.len()
    );
    // Without the option, the same run is one line.
    let (_, text) = filter_with("cargo clippy --all-targets", &raw, &opts(Some(true), false));
    assert_eq!(text, "ok: 30 warnings");
}

#[test]
fn eslint_warnings_keep_every_location() {
    let mut raw = String::new();
    for i in 0..40 {
        raw.push_str(&format!(
            "\n/home/dev/web/src/view_{i}.tsx\n  {}:7  warning  Unexpected any. Specify a different type  @typescript-eslint/no-explicit-any\n  {}:3  warning  Unexpected console statement  no-console\n",
            i + 1,
            i + 20
        ));
    }
    raw.push_str("\n✖ 80 problems (0 errors, 80 warnings)\n");
    let got = filter_for_proxy("npx eslint src", &raw, &opts(Some(true), true));
    assert_eq!(got.warnings, 80);
    assert!(got
        .text
        .starts_with("ok: 80 problems (0 errors, 80 warnings)\nwarnings: 80 (2 rules)\n"));
    for i in 0..40 {
        assert!(
            got.text.contains(&format!("/home/dev/web/src/view_{i}.tsx:{}", i + 1)),
            "{i}"
        );
        assert!(
            got.text.contains(&format!("/home/dev/web/src/view_{i}.tsx:{}", i + 20)),
            "{i}"
        );
    }
    for line in got.text.lines().skip(1) {
        assert!(line.chars().count() <= 120, "{line}");
    }
    assert!(got.text.len() < raw.len() / 2);
}

#[test]
fn errors_stay_whole_and_warnings_are_grouped_after_them() {
    let raw = include_str!("../testdata/cargo_build_err.txt");
    let (_, check) = filter_with("cargo", raw, &opts(Some(false), false));
    let got = filter_for_proxy("cargo", raw, &opts(Some(false), true));
    assert_eq!(got.failures.len(), 2);
    assert_eq!(got.warnings, 1);
    // The errors are exactly `lge check`'s, then the warnings, then the summary.
    let errors = check.split("summary: ").next().unwrap_or(&check);
    assert!(got.text.starts_with(errors), "{}", got.text);
    assert!(got
        .text
        .contains("warnings: 1 (1 rules)\nunused_imports (1): unused import: `std::collections::HashMap`\n"));
    assert!(got.text.contains("  at src/pricing.rs:1\n"));

    // Warnings are never shortened to fit the cap; errors are, as before.
    let mut many = String::new();
    for i in 0..400 {
        many.push_str(&format!("warning: unused variable: `v{i}`\n --> src/w{i}.rs:{i}:9\n  |\n  = note: `#[warn(unused_variables)]` on by default\n\n"));
    }
    many.push_str(raw);
    let got = filter_for_proxy("cargo", &many, &opts(Some(false), true));
    for i in 0..400 {
        assert!(got.text.contains(&format!("src/w{i}.rs:{i}")), "{i}");
    }
    assert_eq!(got.failures.len(), 2);
}

#[test]
fn nextest_failures_are_named_by_binary_and_test() {
    let raw = "   Compiling shop v0.1.0 (/home/dev/shop)\n    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.42s\n------------\n Nextest run ID 5d2f with nextest profile: default\n    Starting 3 tests across 1 binary\n        PASS [   0.005s] shop pricing::tests::ok\n        FAIL [   0.005s] shop cart::tests::totals_items\n\n--- STDOUT:              shop cart::tests::totals_items ---\n\nrunning 1 test\ntest cart::tests::totals_items ... FAILED\n\nfailures:\n\nfailures:\n    cart::tests::totals_items\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 2 filtered out; finished in 0.00s\n\n--- STDERR:              shop cart::tests::totals_items ---\nthread 'cart::tests::totals_items' panicked at src/cart.rs:31:9:\nassertion `left == right` failed\n  left: 1.0\n right: 1.01\n\n        FAIL [   0.004s] (3/3) shop cart::tests::other\n--- STDERR:              shop cart::tests::other ---\nthread 'cart::tests::other' panicked at src/cart.rs:40:9:\nboom\n\n------------\n     Summary [   0.006s] 3 tests run: 1 passed, 2 failed, 0 skipped\n        FAIL [   0.005s] shop cart::tests::totals_items\n        FAIL [   0.004s] shop cart::tests::other\nerror: test run failed\n";
    let got = filter_for_proxy("cargo nextest run", raw, &opts(Some(false), false));
    let found: Vec<(&str, &str, Option<u32>, &str)> = got
        .failures
        .iter()
        .map(|f| (f.name.as_str(), f.file.as_str(), f.line, f.message.as_str()))
        .collect();
    assert_eq!(
        found,
        [
            (
                "shop cart::tests::totals_items",
                "src/cart.rs",
                Some(31),
                "assertion `left == right` failed"
            ),
            ("shop cart::tests::other", "src/cart.rs", Some(40), "boom"),
        ]
    );
    assert_eq!(got.failures[0].excerpt, " left: 1.0\nright: 1.01");
    assert!(!got.text.contains("unparsed output"), "{}", got.text);
    assert!(got
        .text
        .ends_with("summary: 3 tests run: 1 passed, 2 failed, 0 skipped\n"));
    // The summary is the closing line; nextest's repeat of the failures comes after it.
    assert!(got.closed && got.trailing);

    let pass = "    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.42s\n------------\n Nextest run ID 5d2f with nextest profile: default\n    Starting 2 tests across 1 binary\n        PASS [   0.005s] shop a::one\n        PASS [   0.004s] shop a::two\n------------\n     Summary [   0.006s] 2 tests run: 2 passed, 0 skipped\n";
    let got = filter_for_proxy("cargo nextest run", pass, &opts(None, false));
    assert_eq!(got.text, "ok: 2 tests run: 2 passed, 0 skipped");
    assert!(got.closed && !got.trailing && got.failures.is_empty());

    // Cut before the summary: the counts of what was seen, and never closed.
    let cut = "        PASS [   0.005s] shop a::one\n        FAIL [   0.004s] shop a::two\n--- STDERR:              shop a::two ---\nthread 'a::two' panicked at src/a.rs:3:5:\nboom\n";
    let got = filter_for_proxy("cargo nextest run", cut, &opts(None, false));
    assert_eq!(got.failures.len(), 1);
    assert!(got.text.ends_with("summary: 1 passed, 1 failed\n"));
    assert!(!got.closed);
}

fn sample(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join(name);
    std::fs::read_to_string(path).expect("utf-8 sample")
}

/// Outputs cut by `| tail` whose failure is gone, with only the line saying the run
/// failed left: never a run without failures the proxy may call passed.
#[test]
fn a_line_saying_the_run_failed_counts_as_a_failure_not_read() {
    let cases: Vec<(&str, String)> = vec![
        // Real `cargo test --no-fail-fast 2>&1 | tail -n 100`: cargo exited 101.
        ("cargo", sample("cargo_tail_target_failed.txt")),
        // Real `go test ./... 2>&1 | tail -n 46`: the failing package is cut off.
        ("go", sample("go_tail_fail.txt")),
        (
            "",
            "src/a.c:3:9: warning: unused variable 'x' [-Wunused-variable]\n    3 |     int x;\n      |         ^\n\
             1 warning generated.\nmake: *** [Makefile:14: build/parser.o] Error 1\n"
                .to_string(),
        ),
        (
            "",
            format!(
                "{}\nFAIL Required test coverage of 90% not reached. Total coverage: 79.00%\n\
                 ============================== 30 passed in 2.31s ==============================\n",
                (0..30)
                    .map(|i| format!("tests/test_orders.py::test_case_{i} PASSED"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        ),
        (
            "",
            "> Task :app:test\n\n* Try:\n> Run with --scan to get full insights.\n\nBUILD FAILED in 4s\n4 actionable tasks: 2 executed, 2 up-to-date\n"
                .to_string(),
        ),
        (
            "mvn",
            "[INFO] BUILD FAILURE\n[INFO] ------------------------------------------------------------------------\n[INFO] Total time:  3.210 s\n"
                .to_string(),
        ),
        (
            "npm",
            "Tests:       3 passed, 3 total\nnpm ERR! code ELIFECYCLE\n".to_string(),
        ),
    ];
    for (tool, raw) in cases {
        let got = filter_for_proxy(tool, &raw, &opts(None, true));
        assert!(got.failures.is_empty(), "{raw}");
        assert!(got.unparsed > 0, "a failure note must not read as a pass: {raw}");
    }
    // A clean run is still a clean run.
    let clean = filter_for_proxy("go", &sample("go_pass.txt"), &opts(None, true));
    assert_eq!((clean.unparsed, clean.closed, clean.trailing), (0, true, false));
}

/// gcc prints no error count: a diagnostic in a file the C parser does not know
/// (template implementation files, extensionless standard headers) is read, and a
/// diagnostic line it cannot read is shown with the unparsed output.
#[test]
fn gcc_errors_in_template_files_and_standard_headers_are_kept() {
    let mut raw = String::new();
    for i in 0..25 {
        raw.push_str(&format!(
            "src/orders_{i}.cpp: In function 'int total_{i}()':\nsrc/orders_{i}.cpp:{}:12: error: 'subtotal_{i}' was not declared in this scope\n   {} |     return subtotal_{i} + tax;\n      |            ^~~~~~~~~~\n",
            10 + i,
            10 + i
        ));
    }
    raw.push_str("In file included from include/shop/vec.hpp:40,\n                 from src/main.cpp:3:\ninclude/shop/vec.tpp:22:9: error: 'class std::vector<int>' has no member named 'push'\n   22 |     v.push(1);\n      |       ^~~~\n");
    raw.push_str("In file included from src/main.cpp:1:\n/usr/include/c++/13/variant: In instantiation of 'class std::variant<>':\nsrc/main.cpp:5:22:   required from here\n/usr/include/c++/13/variant:1445:21: error: static assertion failed: variant must have at least one alternative\n 1445 |       static_assert(sizeof...(_Types) > 0,\n      |                     ^~~~~~~~~~~~~~~~~~~~~\n");
    let got = filter_for_proxy("g++", &raw, &opts(None, true));
    assert_eq!(got.failures.len(), 27);
    assert!(got.text.contains("include/shop/vec.tpp"), "{}", got.text);
    assert!(got.text.contains("/usr/include/c++/13/variant"), "{}", got.text);

    // A diagnostic in a file of no C-family kind is not read, and not lost either.
    let odd = "src/a.c:3:1: error: expected ';'\nsrc/gen.def:9:2: error: bad entry\n";
    let got = filter_for_proxy("gcc", odd, &opts(Some(false), true));
    assert_eq!(got.failures.len(), 1);
    assert!(got.unparsed > 0);
    assert!(got.text.contains("src/gen.def:9:2: error: bad entry"), "{}", got.text);
}

/// `--continue` and `-fae`: a second task or goal that failed is kept next to the
/// test failure, while the entry that only repeats the test failure is not.
#[test]
fn every_failed_gradle_task_and_maven_goal_is_kept() {
    let gradle = "> Task :app:test FAILED\n\nShopTest > refund() FAILED\n    org.opentest4j.AssertionFailedError at ShopTest.kt:42\n\n12 tests completed, 1 failed\n\n\
                  > Task :lib:ktlintMainSourceSetCheck FAILED\n\nFAILURE: Build completed with 2 failures.\n\n1: Task failed with an exception.\n-----------\n\
                  * What went wrong:\nExecution failed for task ':app:test'.\n> There were failing tests. See the report at: file:///repo/app/build/reports/tests/test/index.html\n\n\
                  * Try:\n> Run with --scan to get full insights.\n\n2: Task failed with an exception.\n-----------\n* What went wrong:\n\
                  Execution failed for task ':lib:ktlintMainSourceSetCheck'.\n> KtLint found code style violations.\n\n* Try:\n> Run with --scan to get full insights.\n\n\
                  BUILD FAILED in 9s\n44 actionable tasks: 44 executed\n";
    let got = filter_for_proxy("gradle", gradle, &opts(Some(false), true));
    let names: Vec<&str> = got.failures.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["ShopTest > refund()", "gradle"], "{}", got.text);
    assert!(got.text.contains("ktlintMainSourceSetCheck"), "{}", got.text);
    assert!(!got.text.contains("There were failing tests"), "{}", got.text);

    let maven = "[ERROR] Tests run: 2, Failures: 1, Errors: 0, Skipped: 0, Time elapsed: 0.05 s <<< FAILURE! -- in com.shop.core.CartTest\n\
                 [ERROR] com.shop.core.CartTest.totals -- Time elapsed: 0.01 s <<< FAILURE!\norg.opentest4j.AssertionFailedError: expected: <3> but was: <4>\n\
                 \tat com.shop.core.CartTest.totals(CartTest.java:21)\n[INFO] \n[INFO] Results:\n[INFO] \n[ERROR] Failures: \n\
                 [ERROR]   CartTest.totals:21 expected: <3> but was: <4>\n[INFO] \n[ERROR] Tests run: 92, Failures: 1, Errors: 0, Skipped: 0\n[INFO] \n\
                 [INFO] BUILD FAILURE\n[INFO] Total time:  4.890 s\n\
                 [ERROR] Failed to execute goal org.apache.maven.plugins:maven-surefire-plugin:3.2.5:test (default-test) on project shop-core: There are test failures.\n\
                 [ERROR] Failed to execute goal org.apache.maven.plugins:maven-checkstyle-plugin:3.3.1:check (validate) on project shop-api: You have 3 Checkstyle violations. -> [Help 1]\n";
    let got = filter_for_proxy("mvn", maven, &opts(Some(false), true));
    assert!(got.text.contains("You have 3 Checkstyle violations"), "{}", got.text);
    assert!(!got.text.contains("There are test failures"), "{}", got.text);
}

/// rustc names a lint only on its first occurrence, Kotlin and javac name none: such
/// warnings are grouped by message, never under the first one's message.
#[test]
fn warnings_without_a_rule_are_grouped_by_message() {
    let got = filter_for_proxy("cargo", &sample("cargo_warn_mixed.txt"), &opts(Some(true), true));
    // Line 6 is `unused_b` and line 11 `NeverBuilt`: neither is listed under `std::fs`.
    let fs_entry = got
        .text
        .lines()
        .skip_while(|l| !l.starts_with("unused import: `std::fs`"))
        .nth(1)
        .unwrap_or_default()
        .to_string();
    assert_eq!(fs_entry, "  at src/lib.rs:2", "{}", got.text);
    assert!(
        got.text
            .contains("unused variable: `unused_b` (1)\n  at src/lib.rs:6\n"),
        "{}",
        got.text
    );
    assert!(
        got.text
            .contains("struct `NeverBuilt` is never constructed (1)\n  at src/lib.rs:11\n"),
        "{}",
        got.text
    );

    let kotlin = "> Task :app:compileKotlin\n\
                  w: file:///repo/app/src/main/kotlin/com/shop/App.kt:10:9 Variable 'unused' is never used\n\
                  w: file:///repo/app/src/main/kotlin/com/shop/Repo.kt:22:16 'fun legacyFetch(): String' is deprecated. Use fetch() instead.\n\
                  w: file:///repo/app/src/main/kotlin/com/shop/Cast.kt:5:21 Unchecked cast: Any to List<String>\n\
                  \nBUILD SUCCESSFUL in 14s\n84 actionable tasks: 84 executed\n";
    let got = filter_for_proxy("gradle", kotlin, &opts(Some(true), true));
    assert!(
        got.text.starts_with("ok: BUILD SUCCESSFUL\nwarnings: 3 (3 rules)\n"),
        "{}",
        got.text
    );
    assert!(
        got.text
            .contains("Unchecked cast: Any to List<String> (1)\n  at /repo/app/src/main/kotlin/com/shop/Cast.kt:5\n"),
        "{}",
        got.text
    );
}
