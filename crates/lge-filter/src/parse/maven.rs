//! Maven: `[INFO]`, `[WARNING]` and `[ERROR]` lines, closed by `BUILD SUCCESS` or
//! `BUILD FAILURE`.
//!
//! Only an output with that closing line is read: it is what says the lines above are
//! Maven's own, in the shape this parser knows. Failing tests come twice, once as a
//! Surefire block per test class (with the stack traces, every frame kept) and once in
//! the `Results:` list (one line per test); compile errors are javac's, in brackets.

use super::{closing_state, number, pattern, Parsed};
use crate::format::failure;
use crate::Failure;

pattern!(BUILD, r"BUILD (SUCCESS|FAILURE)\s*$");
pattern!(EPILOGUE, r"^\[(?:INFO|ERROR|WARNING)\]");
// `[ERROR] Tests run: 3, Failures: 1, ... <<< FAILURE! -- in com.example.CartTest`
pattern!(CLASS, r"^\[ERROR\] Tests run: .* <<< (?:FAILURE|ERROR)! .* in (\S+)");
pattern!(
    TALLY,
    r"Tests run: (\d+), Failures: (\d+), Errors: (\d+), Skipped: (\d+)"
);
pattern!(BLOCK_END, r"^\[INFO\]|^\[ERROR\] Tests run");
// `[ERROR]   CartTest.totalsItems:14 expected: <3.0> but was: <3.01>`
pattern!(ENTRY, r"^\[ERROR\]\s+(\S+)\.(\w+):(\d+) (.*)$");
pattern!(RESULTS, r"^\[ERROR\] (?:Failures|Errors):\s*$");
// Inside `Results:` a line need not have a line number: `CartTest.init » NoClassDefFound`.
pattern!(
    RESULTS_ENTRY,
    r"^\[ERROR\]\s{2,}([\w$.]+\.[\w$]+(?:\([^)]*\))?(?:\[\d+\])?)(?::(\d+))?(?:->\S*)?(?:\s+(.*))?$"
);
pattern!(JAVAC, r"^\[ERROR\] (\S+\.(?:java|kt|scala)):\[(\d+),(\d+)\] (.*)$");
pattern!(JAVAC_WARNING, r"^\[WARNING\] (\S+\.(?:java|kt)):\[(\d+),(\d+)\] (.*)$");
// javac's lint category in front of a warning: `[deprecation] Date(int,int,int) ...`
pattern!(CATEGORY, r"^\[([\w-]+)\] (.*)$");
pattern!(GOAL, r"^\[ERROR\] Failed to execute goal (.*)$");
pattern!(RESUME, r"^\[ERROR\]\s+(mvn\b.*-rf :\S+)\s*$");

/// Lines kept under a compile error (`symbol:`, `location:`).
const DETAIL_LINES: usize = 6;

pub(crate) fn parse(text: &str) -> Parsed {
    let lines: Vec<&str> = text.lines().collect();
    let Some(build) = lines.iter().rev().find_map(|l| BUILD.captures(l.trim())) else {
        return Parsed::default();
    };
    let build = format!("BUILD {}", &build[1]);
    let mut parsed = Parsed {
        recognised: true,
        ..Parsed::default()
    };
    let mut tally: Option<(String, usize)> = None;
    let mut goals: Vec<&str> = Vec::new();
    let mut resume: Option<&str> = None;
    let mut in_results = false;

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        if let Some(c) = TALLY.captures(line) {
            let failed = c[2].parse::<usize>().unwrap_or(0) + c[3].parse::<usize>().unwrap_or(0);
            tally = Some((c[0].to_string(), failed));
        }
        if RESULTS.is_match(line) {
            in_results = true;
            continue;
        }
        if in_results {
            if let Some(c) = RESULTS_ENTRY.captures(line) {
                let message = c.get(3).map_or("", |m| m.as_str());
                push_new(
                    &mut parsed.failures,
                    failure(
                        "",
                        c.get(2).and_then(|m| number(m.as_str())),
                        &c[1],
                        clean(message),
                        &[],
                    ),
                );
                continue;
            }
            if line.starts_with("[ERROR]  ") {
                // A rerun line under an entry (`Run 1: ...`).
                continue;
            }
            in_results = false;
        }
        if let Some(c) = CLASS.captures(line) {
            let end = (i..lines.len())
                .find(|&k| BLOCK_END.is_match(lines[k]))
                .unwrap_or(lines.len());
            let excerpt: Vec<String> = lines[i..end].iter().map(|l| l.to_string()).collect();
            let message = TALLY.find(line).map_or("", |m| m.as_str());
            parsed.failures.push(failure("", None, &c[1], message, &excerpt));
            i = end;
        } else if let Some(c) = JAVAC.captures(line) {
            let mut detail = Vec::new();
            while i < lines.len() && !lines[i].starts_with('[') && !lines[i].trim().is_empty() {
                if detail.len() < DETAIL_LINES {
                    detail.push(lines[i].to_string());
                }
                i += 1;
            }
            // Maven lists the compile errors again under `Failed to execute goal`.
            push_new(
                &mut parsed.failures,
                failure(&c[1], number(&c[2]), "javac", &c[4], &detail),
            );
        } else if let Some(c) = JAVAC_WARNING.captures(line) {
            let (name, message) = match CATEGORY.captures(&c[4]) {
                Some(k) => (k[1].to_string(), k[2].to_string()),
                None => ("javac".to_string(), c[4].to_string()),
            };
            push_new(
                &mut parsed.warnings,
                failure(&c[1], number(&c[2]), &name, &message, &[]),
            );
        } else if let Some(c) = GOAL.captures(line) {
            goals.push(c.get(1).map_or("", |m| m.as_str()));
        } else if let Some(c) = RESUME.captures(line) {
            resume = c.get(1).map(|m| m.as_str());
        } else if let Some(c) = ENTRY.captures(line) {
            let name = format!("{}.{}", &c[1], &c[2]);
            push_new(
                &mut parsed.failures,
                failure("", number(&c[3]), &name, clean(&c[4]), &[]),
            );
        }
    }
    // Each goal that failed is kept unless the failures read above already explain
    // it: Surefire's or Failsafe's `There are test failures` once a test failure was
    // read, the compiler's `Compilation failure` once a compile error was. Another
    // module's goal (`-fae`: checkstyle, enforcer, spotless) is never dropped.
    let tests_read = parsed.failures.iter().any(|f| f.file.is_empty());
    let compile_read = parsed.failures.iter().any(|f| !f.file.is_empty());
    let excerpt: Vec<String> = resume.map(|r| vec![r.to_string()]).unwrap_or_default();
    for goal in goals {
        let explained = (tests_read
            && (goal.contains("There are test failures") || goal.contains("There were test failures")))
            || (compile_read && goal.contains("Compilation failure"));
        if !explained {
            parsed.failures.push(failure("", None, "maven", goal, &excerpt));
        }
    }
    let failed = build == "BUILD FAILURE";
    parsed.summary = Some(match &tally {
        Some((t, _)) => format!("{t}; {build}"),
        None => build,
    });
    // `BUILD FAILURE` is at least one failure, read or not.
    parsed.reported = match tally {
        Some((_, n)) => Some(n.max(usize::from(failed))),
        None => failed.then_some(1),
    };
    (parsed.closed, parsed.trailing) = closing_state(text, &[&BUILD], &[&EPILOGUE]);
    parsed
}

/// `» NullPointer Cannot invoke ...`: the arrow only points at the message.
fn clean(message: &str) -> &str {
    message.trim().trim_start_matches('»').trim_start()
}

/// Adds `f` unless a failure with the same file, line, name and message is there.
fn push_new(list: &mut Vec<Failure>, f: Failure) {
    let same = |g: &Failure| g.file == f.file && g.line == f.line && g.name == f.name && g.message == f.message;
    if !list.iter().any(same) {
        list.push(f);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{filter_output, filter_with, FilterOptions};

    const PASS: &str = include_str!("../../testdata/maven_pass.txt");
    const TEST_FAIL: &str = include_str!("../../testdata/maven_test_fail.txt");
    const COMPILE_FAIL: &str = include_str!("../../testdata/maven_compile_fail.txt");
    const REACTOR: &str = include_str!("../../testdata/maven_reactor_fail.txt");
    const QUIET: &str = include_str!("../../testdata/maven_no_footer.txt");

    #[test]
    fn a_passing_build_is_one_line() {
        assert_eq!(
            filter_output("mvn", PASS),
            (
                Vec::new(),
                "ok: Tests run: 5, Failures: 0, Errors: 0, Skipped: 0; BUILD SUCCESS".to_string()
            )
        );
        let parsed = parse(PASS);
        assert!(parsed.closed && !parsed.trailing);
    }

    #[test]
    fn failing_tests_keep_every_frame_and_every_test() {
        let parsed = parse(TEST_FAIL);
        assert!(parsed.recognised && parsed.closed && !parsed.trailing);
        assert_eq!(parsed.reported, Some(2));
        assert_eq!(
            parsed.summary.as_deref(),
            Some("Tests run: 5, Failures: 1, Errors: 1, Skipped: 0; BUILD FAILURE")
        );
        let got: Vec<(&str, Option<u32>, &str)> = parsed
            .failures
            .iter()
            .map(|f| (f.name.as_str(), f.line, f.message.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                (
                    "com.example.CartTest",
                    None,
                    "Tests run: 3, Failures: 1, Errors: 1, Skipped: 0"
                ),
                ("CartTest.totalsItems", Some(14), "expected: <3.0> but was: <3.01>"),
                (
                    "CartTest.emptyCart",
                    Some(20),
                    "NullPointer Cannot invoke \"java.util.List.size()\" because \"this.items\" is null"
                ),
            ]
        );
        let block = &parsed.failures[0].excerpt;
        // Library frames are kept: no deny-list decides which frame matters.
        assert!(block.contains("at org.junit.jupiter.api.AssertEquals.assertEquals(AssertEquals.java:86)"));
        assert!(block.contains("at com.example.CartTest.totalsItems(CartTest.java:14)"));
        assert!(block.contains("at com.example.CartTest.emptyCart(CartTest.java:20)"));
        // `Failed to execute goal` adds nothing when the tests say what failed.
        assert!(!parsed.failures.iter().any(|f| f.name == "maven"));
    }

    #[test]
    fn compile_errors_once_each_and_warnings_by_category() {
        let parsed = parse(COMPILE_FAIL);
        let got: Vec<(&str, Option<u32>, &str)> = parsed
            .failures
            .iter()
            .map(|f| (f.file.as_str(), f.line, f.message.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                (
                    "/home/dev/shop/src/main/java/com/example/App.java",
                    Some(7),
                    "cannot find symbol"
                ),
                (
                    "/home/dev/shop/src/main/java/com/example/App.java",
                    Some(11),
                    "incompatible types: java.lang.String cannot be converted to int"
                ),
            ]
        );
        assert_eq!(
            parsed.failures[0].excerpt,
            "symbol:   variable c\nlocation: class com.example.App"
        );
        assert_eq!(parsed.warnings.len(), 1);
        assert_eq!(parsed.warnings[0].name, "deprecation");
        assert_eq!(parsed.warnings[0].line, Some(12));
        assert_eq!(parsed.summary.as_deref(), Some("BUILD FAILURE"));
    }

    #[test]
    fn a_failed_goal_with_the_module_to_resume_from() {
        let (failures, text) = filter_output("./mvnw verify", REACTOR);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].name, "maven");
        assert!(failures[0]
            .message
            .contains("Could not resolve dependencies for project com.example:web"));
        assert_eq!(failures[0].excerpt, "mvn <args> -rf :web");
        assert!(text.contains("-rf :web"));
    }

    #[test]
    fn without_the_closing_line_nothing_is_claimed() {
        let parsed = parse(QUIET);
        assert!(!parsed.recognised && parsed.failures.is_empty() && !parsed.closed);
        let failed = FilterOptions {
            passed: Some(false),
            ..FilterOptions::default()
        };
        let (failures, text) = filter_with("mvn", QUIET, &failed);
        assert!(failures.is_empty());
        assert!(text.contains("Kompilierungsfehler"), "{text}");
    }

    #[test]
    fn output_after_the_build_line_is_trailing() {
        let raw = format!("{PASS}Started Application in 2.1 seconds\n");
        let parsed = parse(&raw);
        assert!(parsed.closed && parsed.trailing);
    }
}
