//! The promises of `invariants.rs`, for Go, golangci-lint, Maven, Gradle and the C
//! compilers: every failure named, a pass in one line, a failure nothing was parsed
//! from still shown, the same result whether the tool is named or sniffed.

use lge_filter::{filter_output, filter_with, FilterOptions, MAX_FILTERED_BYTES};

macro_rules! sample {
    ($name:literal) => {
        include_str!(concat!("../testdata/", $name, ".txt"))
    };
}

const FAILED: FilterOptions = FilterOptions {
    passed: Some(false),
    max_bytes: MAX_FILTERED_BYTES,
    keep_warnings: false,
};
const PASSED: FilterOptions = FilterOptions {
    passed: Some(true),
    max_bytes: MAX_FILTERED_BYTES,
    keep_warnings: false,
};

/// (tool, output of a failing run, the name of every failure in it, recognised when sniffed)
fn failing_runs() -> Vec<(&'static str, &'static str, Vec<&'static str>, bool)> {
    let go = vec![
        "TestTotal",
        "TestEmpty",
        "TestTable",
        "TestTable/one",
        "TestPanics",
        "build",
        "build",
        "build",
    ];
    vec![
        (
            "go",
            sample!("go_fail"),
            [go.clone(), vec!["panic in TestPanics"]].concat(),
            true,
        ),
        ("go", sample!("go_fail_json"), go, true),
        (
            "go",
            sample!("go_fail_v"),
            vec![
                "TestTotal",
                "TestEmpty",
                "TestTable",
                "TestTable/one",
                "TestPanics",
                "panic in TestPanics",
            ],
            true,
        ),
        ("go", sample!("go_vet"), vec!["build", "build"], false),
        ("go", sample!("go_build"), vec!["build", "build", "build"], false),
        (
            "golangci-lint",
            sample!("golangci_v1"),
            vec!["errcheck", "ineffassign", "unused", "gomoddirectives"],
            true,
        ),
        (
            "golangci-lint",
            sample!("golangci_v2"),
            vec!["errcheck", "ineffassign", "unused", "gomoddirectives"],
            true,
        ),
        (
            "mvn",
            sample!("maven_test_fail"),
            vec!["com.example.CartTest", "CartTest.totalsItems", "CartTest.emptyCart"],
            true,
        ),
        ("mvn", sample!("maven_compile_fail"), vec!["javac", "javac"], true),
        ("mvn", sample!("maven_reactor_fail"), vec!["maven"], true),
        (
            "gradle",
            sample!("gradle_test_fail"),
            vec![
                "com.example.CartTest > totalsItems()",
                "com.example.CartTest > emptyCart()",
            ],
            true,
        ),
        ("gradle", sample!("gradle_compile_fail"), vec!["javac", "javac"], true),
        ("gradle", sample!("gradle_kotlin_fail"), vec!["kotlin", "kotlin"], true),
        ("clang", sample!("clang_fail"), vec!["error", "error"], true),
        ("gcc", sample!("gcc_fail"), vec!["error"], true),
        (
            "gcc",
            sample!("make_gcc_fail"),
            vec!["error", "-Werror=implicit-function-declaration"],
            true,
        ),
        ("clang", sample!("clang_link"), vec!["ld", "ld", "clang"], false),
        ("gcc", sample!("gcc_link"), vec!["ld", "ld", "collect2"], false),
    ]
}

/// (tool, output of a passing run)
fn passing_runs() -> Vec<(&'static str, &'static str)> {
    vec![
        ("go", sample!("go_pass")),
        ("go", sample!("go_pass_json")),
        ("golangci-lint", "0 issues.\n"),
        ("mvn", sample!("maven_pass")),
        ("gradle", sample!("gradle_pass")),
        ("clang", sample!("clang_warn")),
    ]
}

fn failure_lines(text: &str) -> usize {
    text.lines()
        .filter(|l| l.starts_with("  ") && !l.starts_with("   "))
        .count()
}

#[test]
fn every_reported_failure_is_named() {
    for (tool, raw, names, _) in failing_runs() {
        let (failures, text) = filter_with(tool, raw, &FAILED);
        assert_eq!(failures.len(), names.len(), "{tool}: {text}");
        assert_eq!(failure_lines(&text), names.len(), "{tool}: {text}");
        let mut got: Vec<&str> = failures.iter().map(|f| f.name.as_str()).collect();
        let mut want = names.clone();
        got.sort_unstable();
        want.sort_unstable();
        assert_eq!(got, want, "{tool}");
        for failure in &failures {
            assert!(text.contains(&failure.name), "{tool}: `{}` is missing", failure.name);
            assert!(!failure.message.contains('\n'));
        }
        assert!(text.len() <= MAX_FILTERED_BYTES, "{tool}");
        assert!(
            !text.contains("unparsed output"),
            "{tool}: the parser missed a failure:\n{text}"
        );
    }
}

#[test]
fn a_passing_run_is_one_short_line() {
    for (tool, raw) in passing_runs() {
        for opts in [FilterOptions::default(), PASSED] {
            let (failures, text) = filter_with(tool, raw, &opts);
            assert!(failures.is_empty(), "{tool}: {failures:?}");
            assert!(
                text.starts_with("ok: ") && !text.contains('\n') && text.len() <= 200,
                "{tool}: {text}"
            );
        }
    }
}

#[test]
fn sniffed_output_gives_the_named_tools_result() {
    for (tool, raw, _, recognised) in failing_runs() {
        if recognised {
            assert_eq!(filter_output("make", raw), filter_output(tool, raw), "{tool} sniffed");
        } else {
            // Not claimed when sniffed, but shown.
            let (_, text) = filter_with("make", raw, &FAILED);
            let last = raw.lines().rfind(|l| !l.trim().is_empty()).unwrap().trim_end();
            assert!(text.contains(last), "{tool}: {text}");
        }
    }
    for (tool, raw) in passing_runs() {
        assert_eq!(filter_output("make", raw), filter_output(tool, raw), "{tool} sniffed");
    }
}

#[test]
fn an_output_without_the_closing_line_is_shown_not_parsed() {
    for (tool, raw, phrase) in [
        ("mvn", sample!("maven_no_footer"), "Kompilierungsfehler"),
        (
            "gradle",
            "> Task :app:compileJava\nerror: invalid source release: 99\n",
            "invalid source release",
        ),
    ] {
        for opts in [FilterOptions::default(), FAILED] {
            let (failures, text) = filter_with(tool, raw, &opts);
            assert!(failures.is_empty(), "{tool}");
            assert!(text.contains(phrase), "{tool}: {text}");
        }
    }
}

#[test]
fn garbage_and_cut_outputs_do_not_panic() {
    let tools = ["go", "golangci-lint", "mvn", "gradle", "clang", "make"];
    let samples: Vec<&str> = failing_runs().iter().map(|(_, raw, _, _)| *raw).collect();
    for tool in tools {
        for opts in [FilterOptions::default(), FAILED, PASSED] {
            for sample in &samples {
                let mut cut = 0;
                while cut < sample.len() {
                    if sample.is_char_boundary(cut) {
                        let _ = filter_with(tool, &sample[..cut], &opts);
                    }
                    cut += 97;
                }
            }
            let _ = filter_with(
                tool,
                "{\"Action\":\"fail\"}\n{\"Action\":\n[ERROR] Tests run: <<< FAILURE! in\nBUILD FAILURE\n",
                &opts,
            );
            let _ = filter_with(
                tool,
                "--- FAIL: \npanic: \ngoroutine 1 [\n\ta.go:1:1: x\ne: file://:1:1\nBUILD FAILED in 1s\n",
                &opts,
            );
        }
    }
}
