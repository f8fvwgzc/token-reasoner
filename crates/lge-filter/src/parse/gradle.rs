//! Gradle: task lines, compiler diagnostics (javac's `file:line: error:`, Kotlin's
//! `e: file:line:col message`), failing tests (`Class > test FAILED`) and the
//! `* What went wrong:` block, closed by `BUILD SUCCESSFUL in` or `BUILD FAILED in`.
//!
//! Only an output with that closing line is read.

use super::{closing_state, number, pattern, Parsed};
use crate::format::failure;
use crate::Failure;

pattern!(BUILD, r"^BUILD (SUCCESSFUL|FAILED) in ");
pattern!(
    EPILOGUE,
    r"^(?:\d+ actionable tasks?:|Publishing build scan|https://gradle\.com/s/)"
);
pattern!(KOTLIN, r"^([ew]): (?:file://)?(\S+?):(\d+):(\d+):? (.*)$");
// Kotlin before 1.9: `e: /src/App.kt: (7, 16): Unresolved reference: c`.
pattern!(KOTLIN_OLD, r"^([ew]): (\S+?): \((\d+), (\d+)\): (.*)$");
pattern!(JAVAC, r"^(\S+\.java):(\d+): (error|warning): (.*)$");
// javac's lint category in front of a warning: `[deprecation] Date(int,int,int) ...`
pattern!(CATEGORY, r"^\[([\w-]+)\] (.*)$");
pattern!(TEST_FAILED, r"^(\S+) > (.+) FAILED$");
pattern!(TESTS, r"^(\d+) tests? completed, (\d+) failed");
pattern!(WRONG, r"^\* What went wrong:\s*$");

/// Lines kept under a compile error (source, caret, `symbol:`, `location:`).
const DETAIL_LINES: usize = 6;

/// Sources Gradle compiles: their diagnostics are Gradle's when it ran.
fn is_jvm_source(file: &str) -> bool {
    [".java", ".kt", ".kts", ".groovy", ".scala"]
        .iter()
        .any(|ext| file.ends_with(ext))
}

pub(crate) fn parse(text: &str) -> Parsed {
    let lines: Vec<&str> = text.lines().collect();
    let Some(build) = lines.iter().rev().find_map(|l| BUILD.captures(l)) else {
        return Parsed::default();
    };
    let build = format!("BUILD {}", &build[1]);
    let mut parsed = Parsed {
        recognised: true,
        owns_files: Some(is_jvm_source),
        ..Parsed::default()
    };
    let (mut tests, mut failed_tests, mut tallied) = (0usize, 0usize, false);
    let mut wrong: Vec<Failure> = Vec::new();

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        if let Some(c) = KOTLIN.captures(line).or_else(|| KOTLIN_OLD.captures(line)) {
            let f = failure(&c[2], number(&c[3]), "kotlin", &c[5], &[]);
            if &c[1] == "e" {
                parsed.failures.push(f);
            } else {
                parsed.warnings.push(f);
            }
        } else if let Some(c) = JAVAC.captures(line) {
            let mut detail = Vec::new();
            while i < lines.len() && lines[i].starts_with(char::is_whitespace) && !lines[i].trim().is_empty() {
                if detail.len() < DETAIL_LINES {
                    detail.push(lines[i].to_string());
                }
                i += 1;
            }
            if &c[3] == "error" {
                parsed
                    .failures
                    .push(failure(&c[1], number(&c[2]), "javac", &c[4], &detail));
            } else {
                let (name, message) = match CATEGORY.captures(&c[4]) {
                    Some(k) => (k[1].to_string(), k[2].to_string()),
                    None => ("javac".to_string(), c[4].to_string()),
                };
                parsed
                    .warnings
                    .push(failure(&c[1], number(&c[2]), &name, &message, &detail));
            }
        } else if let Some(c) = TEST_FAILED.captures(line) {
            let mut block = Vec::new();
            while i < lines.len() && lines[i].starts_with(char::is_whitespace) && !lines[i].trim().is_empty() {
                block.push(lines[i].to_string());
                i += 1;
            }
            let message = block.first().map_or("", |l| l.trim());
            let name = format!("{} > {}", &c[1], &c[2]);
            parsed.failures.push(failure("", None, &name, message, &block));
        } else if let Some(c) = TESTS.captures(line) {
            tallied = true;
            tests += c[1].parse::<usize>().unwrap_or(0);
            failed_tests += c[2].parse::<usize>().unwrap_or(0);
        } else if WRONG.is_match(line) {
            let block: Vec<String> = lines[i..]
                .iter()
                .take_while(|l| !l.trim().is_empty())
                .map(|l| l.to_string())
                .collect();
            i += block.len();
            let message = block.first().map_or("", |l| l.as_str());
            wrong.push(failure("", None, "gradle", message, &block));
        }
    }
    // Each `What went wrong` block is kept unless the failures read above already
    // explain it: a test task's `There were failing tests` once a test failure was
    // read, a compile task's `Compilation failed` once a compile error was. Another
    // task that failed (`--continue`: ktlint, checkstyle, lint) is never dropped.
    let tests_read = parsed.failures.iter().any(|f| f.file.is_empty());
    let compile_read = parsed.failures.iter().any(|f| !f.file.is_empty());
    parsed.failures.extend(wrong.into_iter().filter(|w| {
        let text = format!("{}\n{}", w.message, w.excerpt);
        let explained = (tests_read && text.contains("There were failing tests"))
            || (compile_read && (text.contains("Compilation failed") || text.contains("Compilation error")));
        !explained
    }));
    let failed = build == "BUILD FAILED";
    parsed.summary = Some(if tallied {
        format!("{tests} tests completed, {failed_tests} failed; {build}")
    } else {
        build
    });
    // `BUILD FAILED` is at least one failure, read or not: a build whose reason was
    // cut off (`| tail`) is not called passed on its closing line.
    match (tallied, failed) {
        (true, _) => parsed.reported = Some(failed_tests.max(usize::from(failed))),
        (false, true) => parsed.reported = Some(1),
        (false, false) => {}
    }
    (parsed.closed, parsed.trailing) = closing_state(text, &[&BUILD], &[&EPILOGUE]);
    parsed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter_output;

    const PASS: &str = include_str!("../../testdata/gradle_pass.txt");
    const TEST_FAIL: &str = include_str!("../../testdata/gradle_test_fail.txt");
    const COMPILE_FAIL: &str = include_str!("../../testdata/gradle_compile_fail.txt");
    const KOTLIN_FAIL: &str = include_str!("../../testdata/gradle_kotlin_fail.txt");

    #[test]
    fn a_passing_build_is_one_line() {
        assert_eq!(
            filter_output("./gradlew build", PASS),
            (Vec::new(), "ok: BUILD SUCCESSFUL".to_string())
        );
        let parsed = parse(PASS);
        assert!(parsed.recognised && parsed.closed && !parsed.trailing);
    }

    #[test]
    fn failing_tests_with_their_exception() {
        let parsed = parse(TEST_FAIL);
        assert_eq!(parsed.reported, Some(2));
        assert_eq!(
            parsed.summary.as_deref(),
            Some("3 tests completed, 2 failed; BUILD FAILED")
        );
        let got: Vec<(&str, &str)> = parsed
            .failures
            .iter()
            .map(|f| (f.name.as_str(), f.message.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                (
                    "com.example.CartTest > totalsItems()",
                    "org.opentest4j.AssertionFailedError at CartTest.java:14"
                ),
                (
                    "com.example.CartTest > emptyCart()",
                    "java.lang.NullPointerException at Cart.java:9"
                ),
            ]
        );
        assert_eq!(
            parsed.failures[1].excerpt,
            "Caused by: java.lang.IllegalStateException at Cart.java:4"
        );
        assert!(parsed.closed && !parsed.trailing);
    }

    #[test]
    fn javac_errors_and_warnings() {
        let parsed = parse(COMPILE_FAIL);
        let got: Vec<(Option<u32>, &str, &str)> = parsed
            .failures
            .iter()
            .map(|f| (f.line, f.name.as_str(), f.message.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                (Some(7), "javac", "cannot find symbol"),
                (
                    Some(11),
                    "javac",
                    "incompatible types: String cannot be converted to int"
                ),
            ]
        );
        assert_eq!(
            parsed.failures[0].excerpt,
            "      return a + c;\n                 ^\nsymbol:   variable c\nlocation: class App"
        );
        assert_eq!(parsed.warnings.len(), 1);
        assert_eq!(parsed.warnings[0].name, "deprecation");
        // `What went wrong` only repeats that compilation failed.
        assert!(!parsed.failures.iter().any(|f| f.name == "gradle"));
    }

    #[test]
    fn kotlin_diagnostics_and_what_went_wrong() {
        let parsed = parse(KOTLIN_FAIL);
        let got: Vec<(&str, Option<u32>, &str)> = parsed
            .failures
            .iter()
            .map(|f| (f.file.as_str(), f.line, f.message.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                (
                    "/home/dev/shop/app/src/main/kotlin/App.kt",
                    Some(7),
                    "Unresolved reference 'c'."
                ),
                (
                    "/home/dev/shop/app/src/main/kotlin/Cart.kt",
                    Some(3),
                    "Unresolved reference: items"
                ),
            ]
        );
        assert_eq!(parsed.warnings.len(), 1);
        assert_eq!(parsed.warnings[0].line, Some(5));

        let raw = "> Task :app:dependencies FAILED\n\nFAILURE: Build failed with an exception.\n\n* What went wrong:\nExecution failed for task ':app:dependencies'.\n> Could not resolve com.example:missing:1.0.\n\n* Try:\n> Run with --stacktrace option to get the stack trace.\n\nBUILD FAILED in 1s\n1 actionable task: 1 executed\n";
        let parsed = parse(raw);
        assert_eq!(parsed.failures.len(), 1);
        assert_eq!(parsed.failures[0].name, "gradle");
        assert_eq!(
            parsed.failures[0].message,
            "Execution failed for task ':app:dependencies'."
        );
        assert_eq!(
            parsed.failures[0].excerpt,
            "> Could not resolve com.example:missing:1.0."
        );
        assert!(parsed.closed && !parsed.trailing);
    }

    #[test]
    fn without_the_closing_line_nothing_is_claimed() {
        let raw = "> Task :app:compileJava\n/home/dev/App.java:7: error: cannot find symbol\n";
        let parsed = parse(raw);
        assert!(!parsed.recognised && parsed.failures.is_empty());
    }
}
