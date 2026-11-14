//! The promises of the filter, checked for every parser at once:
//!
//! 1. every failure the tool reports is named in the filtered text,
//! 2. a passing run is one short line,
//! 3. a failed run nothing was parsed from still shows its output,
//! 4. the cap shortens excerpts and messages, never the list of failures.

use lge_filter::{filter_output, filter_with, FilterOptions, MAX_FILTERED_BYTES};

macro_rules! sample {
    ($name:literal) => {
        include_str!(concat!("../testdata/", $name, ".txt"))
    };
}

const FAILED: FilterOptions = FilterOptions {
    passed: Some(false),
    max_bytes: MAX_FILTERED_BYTES,
};
const PASSED: FilterOptions = FilterOptions {
    passed: Some(true),
    max_bytes: MAX_FILTERED_BYTES,
};

/// (tool, output of a failing run, the name of every failure the tool reports in it)
fn failing_runs() -> Vec<(&'static str, &'static str, Vec<&'static str>)> {
    let pytest = vec![
        "tests/test_cart.py::test_total_rounds",
        "tests/test_orders.py::TestCheckout::test_rejects_empty_cart",
        "tests/test_orders.py::test_discount[10-90.0]",
        "tests/test_orders.py::test_discount[50-50.0]",
    ];
    let jest = vec![
        "Cart › totals items",
        "Cart › applyDiscount › handles ten percent",
        "render › shows the order id",
    ];
    let vitest = vec![
        "Cart > totals items",
        "Cart > applyDiscount > handles ten percent",
        "render > shows the order id",
    ];
    let tsc = vec!["TS2307", "TS2345", "TS2322", "TS2304"];
    vec![
        (
            "pytest",
            sample!("pytest_fail_long"),
            [pytest.clone(), vec!["tests/test_orders.py::test_list_orders"]].concat(),
        ),
        ("pytest", sample!("pytest_fail_short"), pytest),
        (
            "pytest",
            sample!("pytest_collect_error"),
            vec!["tests/test_orders.py", "tests/test_cart.py"],
        ),
        (
            "ruff",
            sample!("ruff_fail_concise"),
            vec!["F401", "E722", "E501", "B008", "I001"],
        ),
        ("ruff", sample!("ruff_fail_full"), vec!["F401", "E722", "SyntaxError"]),
        ("ruff", sample!("ruff_fail_new"), vec!["F401", "E722", "invalid-syntax"]),
        ("flake8", sample!("flake8_fail"), vec!["F401", "E722", "E501", "E999"]),
        (
            "mypy",
            sample!("mypy_fail"),
            vec!["return-value", "arg-type", "operator", "no-untyped-def", "attr-defined"],
        ),
        ("mypy", sample!("mypy_fail_pretty"), vec!["return-value", "operator"]),
        ("mypy", sample!("mypy_syntax"), vec!["syntax"]),
        (
            "pyright",
            sample!("pyright_fail"),
            vec![
                "reportReturnType",
                "reportArgumentType",
                "reportOptionalOperand",
                "reportAttributeAccessIssue",
            ],
        ),
        ("tsc", sample!("tsc_fail"), tsc.clone()),
        ("tsc", sample!("tsc_fail_pretty"), tsc),
        ("tsc", sample!("tsc_crash"), vec!["TS5058"]),
        (
            "eslint",
            sample!("eslint_fail_stylish"),
            vec![
                "@typescript-eslint/no-unused-vars",
                "no-console",
                "Parsing error: Declaration or statement expected",
                "react/react-in-jsx-scope",
            ],
        ),
        (
            "eslint",
            sample!("eslint_fail_unix"),
            vec![
                "@typescript-eslint/no-unused-vars",
                "no-console",
                "Parsing error: Declaration or statement expected",
            ],
        ),
        ("jest", sample!("jest_fail"), jest),
        ("jest", sample!("jest_suite_error"), vec!["Test suite failed to run"]),
        ("vitest", sample!("vitest_fail"), vitest),
        ("vitest", sample!("vitest_suite_error"), vec!["src/orders.test.ts"]),
        (
            "cargo",
            sample!("cargo_test_fail"),
            vec![
                "cart::tests::totals_items",
                "pricing::tests::must_panic",
                "pricing::tests::parses",
                "pricing::tests::ten_percent",
                "order_total_matches_cart",
                "src/lib.rs - add_one (line 6)",
            ],
        ),
        ("cargo", sample!("cargo_build_err"), vec!["E0308", "E0425"]),
        (
            "cargo",
            sample!("clippy_fail"),
            vec![
                "unused_variables",
                "clippy::needless_return",
                "clippy::needless_return",
                "clippy::needless_return",
                "clippy::needless_bool",
                "clippy::ptr_arg",
            ],
        ),
        ("cargo", sample!("cargo_test_abort"), vec!["pricing::tests::zz_aborts"]),
    ]
}

/// (tool, output of a passing run)
fn passing_runs() -> Vec<(&'static str, &'static str)> {
    vec![
        ("pytest", sample!("pytest_pass")),
        ("ruff", sample!("ruff_pass")),
        ("flake8", ""),
        ("mypy", sample!("mypy_pass")),
        ("pyright", sample!("pyright_pass")),
        ("tsc", ""),
        ("eslint", ""),
        ("jest", sample!("jest_pass")),
        ("vitest", sample!("vitest_pass")),
        ("cargo", sample!("cargo_test_pass")),
        ("cargo", sample!("clippy_pass")),
    ]
}

/// (tool, output of a run that died before reporting, a phrase the model must see)
fn crashed_runs() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        (
            "pytest",
            sample!("pytest_usage_error"),
            "unrecognized arguments: --cov=shop",
        ),
        (
            "pytest",
            sample!("pytest_internal_error"),
            "'Function' object has no attribute 'priority'",
        ),
        ("ruff", sample!("ruff_crash"), "TOML parse error at line 14"),
        ("flake8", sample!("flake8_crash"), "does not match"),
        ("mypy", sample!("mypy_crash"), "can't read file 'shop/missing.py'"),
        ("pyright", sample!("pyright_crash"), "could not be parsed"),
        ("tsc", sample!("tsc_crash"), "The specified path does not exist"),
        (
            "eslint",
            sample!("eslint_crash"),
            "couldn't find the config \"airbnb-base\"",
        ),
        (
            "jest",
            sample!("jest_config_error"),
            "Module ts-jest in the transform option was not found",
        ),
        (
            "vitest",
            sample!("vitest_suite_error"),
            "Failed to resolve import \"./orderz\"",
        ),
        ("cargo", sample!("cargo_test_abort"), "SIGABRT"),
        ("cargo", sample!("cargo_no_manifest"), "could not find `Cargo.toml`"),
    ]
}

/// Lines that open a failure in the filtered text: two spaces, then the line or name.
fn failure_lines(text: &str) -> usize {
    text.lines()
        .filter(|l| l.starts_with("  ") && !l.starts_with("   "))
        .count()
}

#[test]
fn every_reported_failure_is_named() {
    for (tool, raw, names) in failing_runs() {
        for opts in [FilterOptions::default(), FAILED] {
            let (failures, text) = filter_with(tool, raw, &opts);
            assert_eq!(failures.len(), names.len(), "{tool}: {text}");
            assert_eq!(failure_lines(&text), names.len(), "{tool}: {text}");
            for name in &names {
                assert!(text.contains(name), "{tool}: `{name}` is missing from:\n{text}");
            }
            for failure in &failures {
                assert!(text.contains(&failure.name), "{tool}: `{}` is missing", failure.name);
                assert!(!failure.name.is_empty() && !failure.message.contains('\n'));
            }
            assert!(text.len() <= MAX_FILTERED_BYTES, "{tool}: {} bytes", text.len());
            assert!(!text.contains('\u{1b}'), "{tool}: escape codes left in the text");
            assert!(
                !text.contains("unparsed output"),
                "{tool}: the parser missed a failure:\n{text}"
            );
        }
    }
}

#[test]
fn filtering_is_smaller_than_the_raw_output_on_real_sized_runs() {
    for (tool, raw, _) in failing_runs() {
        let (_, text) = filter_output(tool, raw);
        // Tiny samples (a handful of one-line errors) gain a file heading; anything
        // with tracebacks or snippets must shrink.
        if raw.len() > 1_500 {
            assert!(text.len() < raw.len(), "{tool}: {} -> {} bytes", raw.len(), text.len());
        }
    }
}

#[test]
fn a_passing_run_is_one_short_line() {
    for (tool, raw) in passing_runs() {
        for opts in [FilterOptions::default(), PASSED] {
            let (failures, text) = filter_with(tool, raw, &opts);
            assert!(failures.is_empty(), "{tool}: {failures:?}");
            assert!(text == "ok" || text.starts_with("ok: "), "{tool}: {text}");
            assert!(!text.contains('\n') && text.len() <= 200, "{tool}: {text}");
        }
    }
}

#[test]
fn exit_code_zero_wins_over_anything_in_the_output() {
    for (tool, raw, _) in failing_runs() {
        let (failures, text) = filter_with(tool, raw, &PASSED);
        assert!(failures.is_empty(), "{tool}");
        assert!(!text.contains('\n') && text.len() <= 200, "{tool}: {text}");
    }
}

#[test]
fn a_failed_run_always_says_why() {
    for (tool, raw, phrase) in crashed_runs() {
        for opts in [FilterOptions::default(), FAILED] {
            let (_, text) = filter_with(tool, raw, &opts);
            assert!(text.contains(phrase), "{tool}: `{phrase}` is missing from:\n{text}");
        }
    }
    // A passing-looking output with a non-zero exit code is shown, not summarised.
    for (tool, raw) in passing_runs() {
        let (failures, text) = filter_with(tool, raw, &FAILED);
        assert!(failures.is_empty(), "{tool}");
        if raw.is_empty() {
            assert_eq!(text, "(no output)\n", "{tool}");
        } else {
            let last = raw.lines().rfind(|l| !l.trim().is_empty()).unwrap().trim_end();
            assert!(text.contains(last), "{tool}: {text}");
        }
    }
}

#[test]
fn unknown_tools_get_the_same_parsers_by_sniffing() {
    // `npm test`, `make check`: the command does not say which tool printed the output.
    for (tool, raw, names) in failing_runs() {
        let (failures, text) = filter_output("npm", raw);
        assert_eq!(failures.len(), names.len(), "{tool} sniffed: {text}");
        // No other parser claims any part of it: the result is the named tool's, to the byte.
        assert_eq!((failures, text), filter_output(tool, raw), "{tool} sniffed");
    }
    for (tool, raw) in passing_runs() {
        assert_eq!(filter_output("npm", raw), filter_output(tool, raw), "{tool} sniffed");
    }
    // A crash may be parsed only when the tool is named (a bare `error:` line could be
    // anyone's), but what it said is shown either way.
    for (tool, raw, phrase) in crashed_runs() {
        let (_, text) = filter_with("npm", raw, &FAILED);
        assert!(
            text.contains(phrase),
            "{tool} sniffed: `{phrase}` is missing from:\n{text}"
        );
    }
}

// ---------------------------------------------------------------------------
// Large runs
// ---------------------------------------------------------------------------

/// Output of `tool` with `n` failures, each with a long excerpt where the format has one.
fn big_run(tool: &str, n: usize) -> String {
    let mut out = String::new();
    match tool {
        "pytest" => {
            out.push_str("=================================== FAILURES ===================================\n");
            for i in 0..n {
                out.push_str(&format!(
                    "_________________________ test_case_{i} _________________________\n\n"
                ));
                out.push_str(&"    value = compute(some, long, argument, list)\n".repeat(20));
                out.push_str(&format!(">       assert value == {i}\n"));
                out.push_str(&format!("E       assert {} == {i}\n", i + 1).repeat(12));
                out.push_str(&format!("\ntests/test_big.py:{}: AssertionError\n", i + 10));
            }
            out.push_str("=========================== short test summary info ============================\n");
            for i in 0..n {
                out.push_str(&format!(
                    "FAILED tests/test_big.py::test_case_{i} - assert {} == {i}\n",
                    i + 1
                ));
            }
            out.push_str(&format!(
                "============================== {n} failed in 1.00s ==============================\n"
            ));
        }
        "ruff" => {
            for i in 0..n {
                out.push_str(&format!(
                    "pkg/mod_{i}.py:{}:1: E{} Something is wrong with this particular line of code\n",
                    i + 1,
                    100 + i
                ));
                out.push_str("  |\n1 | import os\n  | ^^^^^^^^^ here\n  |\n  = help: Remove it\n\n");
            }
            out.push_str(&format!("Found {n} errors.\n"));
        }
        "mypy" => {
            for i in 0..n {
                out.push_str(&format!(
                    "pkg/mod_{i}.py:{}: error: Incompatible types in assignment  [assignment]\n",
                    i + 1
                ));
                out.push_str(&format!(
                    "pkg/mod_{i}.py:{}: note: Consider using a wider type here\n",
                    i + 1
                ));
            }
            out.push_str(&format!("Found {n} errors in {n} files (checked {n} source files)\n"));
        }
        "pyright" => {
            for i in 0..n {
                out.push_str(&format!("/repo/pkg/mod_{i}.py\n  /repo/pkg/mod_{i}.py:{}:5 - error: Type \"str\" is not assignable to \"int\"\n", i + 1));
                out.push_str("    \"str\" is not assignable to \"int\" (reportAssignmentType)\n");
            }
            out.push_str(&format!("{n} errors, 0 warnings, 0 informations \n"));
        }
        "tsc" => {
            for i in 0..n {
                out.push_str(&format!(
                    "src/mod_{i}.ts({},5): error TS2322: Type 'string' is not assignable to type 'number'.\n",
                    i + 1
                ));
                out.push_str(
                    "  The expected type comes from property 'total' which is declared here on type 'Order'\n",
                );
            }
        }
        "eslint" => {
            for i in 0..n {
                out.push_str(&format!("\n/repo/src/mod_{i}.ts\n  {}:7  error  'unused' is assigned a value but never used  no-unused-vars\n", i + 1));
            }
            out.push_str(&format!("\n✖ {n} problems ({n} errors, 0 warnings)\n"));
        }
        "jest" => {
            out.push_str("FAIL src/big.test.ts\n");
            for i in 0..n {
                out.push_str(&format!(
                    "  ● big › case {i}\n\n    expect(received).toBe(expected) // Object.is equality\n\n"
                ));
                out.push_str(&format!("    Expected: {i}\n    Received: {}\n\n", i + 1));
                out.push_str(&"      10 |     some.code(that, is, shown, in, the, frame);\n".repeat(8));
                out.push_str(&format!(
                    "\n      at Object.<anonymous> (src/big.test.ts:{}:9)\n\n",
                    i + 10
                ));
            }
            out.push_str(&format!(
                "Test Suites: 1 failed, 1 total\nTests:       {n} failed, {n} total\n"
            ));
        }
        "vitest" => {
            out.push_str(&format!("⎯⎯⎯⎯⎯⎯⎯ Failed Tests {n} ⎯⎯⎯⎯⎯⎯⎯\n\n"));
            for i in 0..n {
                out.push_str(&format!(
                    " FAIL  src/big.test.ts > big > case {i}\nAssertionError: expected {} to be {i}\n",
                    i + 1
                ));
                out.push_str(&format!(" ❯ src/big.test.ts:{}:9\n", i + 10));
                out.push_str(&"     10|     some.code(that, is, shown, in, the, frame);\n".repeat(8));
                out.push_str(&format!("\n⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯⎯[{}/{n}]⎯\n\n", i + 1));
            }
            out.push_str(&format!(" Test Files  1 failed (1)\n      Tests  {n} failed ({n})\n"));
        }
        "cargo" => {
            out.push_str(&format!("running {n} tests\n"));
            for i in 0..n {
                out.push_str(&format!("test big::case_{i} ... FAILED\n"));
            }
            out.push_str("\nfailures:\n\n");
            for i in 0..n {
                out.push_str(&format!("---- big::case_{i} stdout ----\n"));
                out.push_str(&"some captured output of the test\n".repeat(15));
                out.push_str(&format!("\nthread 'big::case_{i}' (1) panicked at src/big.rs:{}:9:\nassertion `left == right` failed\n  left: {}\n right: {i}\n\n", i + 10, i + 1));
            }
            out.push_str("\nfailures:\n");
            for i in 0..n {
                out.push_str(&format!("    big::case_{i}\n"));
            }
            out.push_str(&format!("\ntest result: FAILED. 0 passed; {n} failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.10s\n"));
        }
        "rustc" => {
            for i in 0..n {
                out.push_str(&format!(
                    "error[E0308]: mismatched types\n  --> src/mod_{i}.rs:{}:20\n   |\n",
                    i + 1
                ));
                out.push_str(&format!("{} |     let n: usize = \"three\";\n   |            -----   ^^^^^^^ expected `usize`, found `&str`\n   |            |\n   |            expected due to this\n\n", i + 1));
            }
            out.push_str(&format!(
                "error: could not compile `big` (lib) due to {n} previous errors\n"
            ));
        }
        other => panic!("no generator for {other}"),
    }
    out
}

const TOOLS: [&str; 10] = [
    "pytest", "ruff", "mypy", "pyright", "tsc", "eslint", "jest", "vitest", "cargo", "rustc",
];

#[test]
fn the_cap_shortens_excerpts_but_keeps_every_failure() {
    for tool in TOOLS {
        let n = 120;
        let raw = big_run(tool, n);
        let (failures, text) = filter_output(tool, &raw);
        assert_eq!(failures.len(), n, "{tool}");
        assert_eq!(failure_lines(&text), n, "{tool}");
        assert!(text.len() <= MAX_FILTERED_BYTES, "{tool}: {} bytes", text.len());
        assert!(text.len() < raw.len(), "{tool}: {} -> {} bytes", raw.len(), text.len());
        for failure in &failures {
            let line = failure.line.expect("every generated failure has a line");
            let opening = format!("  {line}: {}", failure.name);
            assert!(text.contains(&opening), "{tool}: `{opening}` is missing");
        }
        // The returned failures are the ones in the text: shortened in the same way.
        let excerpts: usize = failures.iter().map(|f| f.excerpt.len()).sum();
        assert!(excerpts < MAX_FILTERED_BYTES, "{tool}");
    }
}

#[test]
fn a_small_cap_is_respected_as_far_as_the_names_allow() {
    let raw = big_run("pytest", 20);
    let names: usize = (0..20).map(|i| format!("tests/test_big.py::test_case_{i}").len()).sum();
    for max_bytes in [400, 1_500, 3_000, 6_000] {
        let opts = FilterOptions {
            passed: Some(false),
            max_bytes,
        };
        let (failures, text) = filter_with("pytest", &raw, &opts);
        assert_eq!(failures.len(), 20);
        assert_eq!(failure_lines(&text), 20);
        if max_bytes >= 1_500 {
            assert!(text.len() <= max_bytes, "{max_bytes}: {} bytes", text.len());
        } else {
            // Names alone do not fit: everything else is gone, the names are not.
            assert!(text.len() < names + 20 * 12 + 60, "{max_bytes}: {} bytes", text.len());
            assert!(failures.iter().all(|f| f.message.is_empty() && f.excerpt.is_empty()));
        }
    }
}

#[test]
fn more_failures_than_the_cap_can_name_are_all_still_named() {
    for tool in ["ruff", "tsc", "cargo"] {
        let n = 3_000;
        let (failures, text) = filter_output(tool, &big_run(tool, n));
        assert_eq!(failures.len(), n, "{tool}");
        assert_eq!(failure_lines(&text), n, "{tool}");
        assert!(text.len() > MAX_FILTERED_BYTES, "{tool}");
        assert!(
            failures.iter().all(|f| f.excerpt.is_empty() && f.message.is_empty()),
            "{tool}"
        );
    }
}

#[test]
fn filtering_is_deterministic_and_order_independent() {
    let raw = sample!("ruff_fail_concise");
    let mut reversed: Vec<&str> = raw.lines().collect();
    // Keep the closing lines in place, shuffle the violations.
    reversed[..5].reverse();
    let reversed = reversed.join("\n") + "\n";
    assert_eq!(filter_output("ruff", raw), filter_output("ruff", raw));
    assert_eq!(filter_output("ruff", raw), filter_output("ruff", &reversed));
}

#[test]
fn garbage_does_not_panic() {
    let noise =
        "\u{1b}[31m●\u{0}\r\r\n ---- stdout ----\nFAILED\n==== ====\n____ ____\nfailures:\n    \n@@ -1 +1 @@\n\u{1b}";
    let samples: Vec<String> = failing_runs().iter().map(|(_, raw, _)| raw.to_string()).collect();
    for tool in TOOLS.iter().chain(&["npm", "", "a b c"]) {
        for opts in [FilterOptions::default(), FAILED, PASSED] {
            let _ = filter_with(tool, noise, &opts);
            // Every parser against every other tool's output, whole and cut at every 257th byte.
            for sample in &samples {
                let _ = filter_with(tool, sample, &opts);
                let mut cut = 0;
                while cut < sample.len() {
                    if sample.is_char_boundary(cut) {
                        let _ = filter_with(tool, &sample[..cut], &opts);
                    }
                    cut += 257;
                }
            }
        }
    }
}
