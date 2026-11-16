//! Jest: a `FAIL <file>` line per failing suite, then one `● suite › test` block per
//! failing test with the assertion, a code frame and the stack.

use super::{closing_state, count_of, number, pattern, squeeze, Parsed};
use crate::format::{clip_lines, failure};
use crate::Failure;

// With colours on, the status is a padded badge: ` FAIL  src/a.test.ts`.
pattern!(SUITE, r"^\s?(?:FAIL|PASS)\s+(\S+)");
pattern!(BULLET, r"^\s*● (.+)$");
pattern!(FRAME, r"^\s*at (?:.*? \()?(.+?):(\d+):(\d+)\)?$");
pattern!(TESTS, r"^Tests:\s+(.*)$");
pattern!(SUITES, r"^Test Suites:\s+(.*)$");
// What jest prints after its `Tests:` line.
pattern!(
    EPILOGUE,
    r"^(?:(?:Snapshots|Time|Test Suites):|Ran all test suites|Watch Usage)"
);

/// Blocks that use the bullet but are not failures.
const NOT_FAILURES: &[&str] = &["Console", "Deprecation Warning", "Validation Warning"];

struct Block<'a> {
    file: &'a str,
    title: &'a str,
    lines: Vec<&'a str>,
}

impl Block<'_> {
    fn close(self) -> Failure {
        let message = self
            .lines
            .iter()
            .map(|l| l.trim())
            .find(|l| !l.is_empty())
            .unwrap_or("");
        // The frame inside the test file is where the test failed.
        let line = self
            .lines
            .iter()
            .filter_map(|l| FRAME.captures(l))
            .find(|c| !self.file.is_empty() && c[1].ends_with(self.file))
            .and_then(|c| number(&c[2]));
        let excerpt: Vec<String> = self
            .lines
            .iter()
            .filter(|l| !l.trim().is_empty() && !is_library_frame(l))
            .map(|l| l.to_string())
            .collect();
        failure(
            self.file,
            line,
            self.title.trim_end_matches(':'),
            message,
            &clip_lines(excerpt, 12, 4),
        )
    }
}

/// Stack frames inside dependencies or the runtime say nothing about the change.
pub(super) fn is_library_frame(line: &str) -> bool {
    let line = line.trim_start();
    (line.starts_with("at ") || line.starts_with('❯'))
        && (line.contains("node_modules/") || line.contains("node:internal") || line.contains("(<anonymous>)"))
}

pub(crate) fn parse(text: &str) -> Parsed {
    let mut parsed = Parsed::default();
    let mut file = "";
    let mut open: Option<Block> = None;
    let mut skipping = false;
    let (mut suites, mut tests) = (None, None);
    let (mut saw_suite, mut saw_bullet) = (false, false);

    for line in text.lines() {
        if let Some(c) = BULLET.captures(line) {
            parsed.failures.extend(open.take().map(Block::close));
            let title = c.get(1).map_or("", |m| m.as_str()).trim();
            skipping = NOT_FAILURES.iter().any(|t| title.starts_with(t));
            saw_bullet = true;
            if !skipping {
                open = Some(Block {
                    file,
                    title,
                    lines: Vec::new(),
                });
            }
            continue;
        }
        let suite = SUITE.captures(line);
        let closes = suite.is_some()
            || TESTS.is_match(line)
            || SUITES.is_match(line)
            || line.starts_with("Summary of all failing tests")
            || line.starts_with("Ran all test suites");
        if closes {
            parsed.failures.extend(open.take().map(Block::close));
            skipping = false;
        }
        if let Some(c) = suite {
            file = c.get(1).map_or("", |m| m.as_str());
            saw_suite = true;
        } else if let Some(c) = TESTS.captures(line) {
            tests = Some(c[1].to_string());
        } else if let Some(c) = SUITES.captures(line) {
            suites = Some(c[1].to_string());
        } else if let Some(block) = open.as_mut().filter(|_| !skipping) {
            block.lines.push(line);
        }
    }
    parsed.failures.extend(open.take().map(Block::close));
    // The end of a long run repeats every failure under "Summary of all failing tests".
    let mut unique: Vec<Failure> = Vec::new();
    for f in parsed.failures.drain(..) {
        if !unique.contains(&f) {
            unique.push(f);
        }
    }
    parsed.failures = unique;

    let failed_tests = tests.as_deref().map_or(0, |t| count_of(t, "failed"));
    let failed_suites = suites.as_deref().map_or(0, |t| count_of(t, "failed"));
    parsed.recognised = tests.is_some() || suites.is_some() || (saw_suite && saw_bullet);
    if tests.is_some() || suites.is_some() {
        // A suite that could not run has no failing test, only itself.
        parsed.reported = Some(if failed_tests > 0 { failed_tests } else { failed_suites });
    }
    parsed.summary = tests
        .map(|t| format!("Tests: {}", squeeze(&t)))
        .or(suites.map(|s| format!("Test Suites: {}", squeeze(&s))));
    (parsed.closed, parsed.trailing) = closing_state(text, &[&TESTS], &[&EPILOGUE]);
    parsed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter_output;

    const PASS: &str = include_str!("../../testdata/jest_pass.txt");
    const FAIL: &str = include_str!("../../testdata/jest_fail.txt");
    const SUITE_ERROR: &str = include_str!("../../testdata/jest_suite_error.txt");
    const CONFIG_ERROR: &str = include_str!("../../testdata/jest_config_error.txt");

    #[test]
    fn passing_run_is_one_line() {
        assert_eq!(
            filter_output("jest", PASS),
            (Vec::new(), "ok: Tests: 7 passed, 7 total".to_string())
        );
    }

    #[test]
    fn failing_tests_with_their_file_line_and_frame() {
        let parsed = parse(FAIL);
        assert_eq!(parsed.reported, Some(3));
        assert_eq!(parsed.summary.as_deref(), Some("Tests: 3 failed, 5 passed, 8 total"));
        let got: Vec<(&str, Option<u32>, &str, &str)> = parsed
            .failures
            .iter()
            .map(|f| (f.file.as_str(), f.line, f.name.as_str(), f.message.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                (
                    "src/cart.test.ts",
                    Some(12),
                    "Cart › totals items",
                    "expect(received).toBe(expected) // Object.is equality"
                ),
                (
                    "src/cart.test.ts",
                    Some(22),
                    "Cart › applyDiscount › handles ten percent",
                    "TypeError: Cannot read properties of undefined (reading 'percent')"
                ),
                (
                    "src/views.test.tsx",
                    Some(16),
                    "render › shows the order id",
                    "expect(received).toEqual(expected) // deep equality"
                ),
            ]
        );
        let first = &parsed.failures[0];
        assert!(first.excerpt.contains("Expected: 1.01\nReceived: 1\n"));
        assert!(first.excerpt.contains("> 12 |     expect(cart.total()).toBe(1.01);"));
        assert!(first
            .excerpt
            .ends_with("at Object.<anonymous> (src/cart.test.ts:12:26)"));
        // The console block is not a failure and does not leak into one.
        assert!(parsed.failures.iter().all(|f| !f.excerpt.contains("cart has 1 items")));
        // Frames inside node_modules are dropped, the ones in the project stay.
        let second = &parsed.failures[1];
        assert!(second.excerpt.contains("at applyDiscount (src/pricing.ts:5:28)"));
        assert!(!second.excerpt.contains("node_modules") && !second.excerpt.contains("new Promise"));
    }

    #[test]
    fn a_suite_that_cannot_run_is_a_failure() {
        let (failures, text) = filter_output("jest", SUITE_ERROR);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].file, "src/orders.test.ts");
        assert_eq!(failures[0].name, "Test suite failed to run");
        assert_eq!(failures[0].line, Some(2));
        assert_eq!(
            failures[0].message,
            "Cannot find module './orderz' from 'src/orders.test.ts'"
        );
        assert!(text.ends_with("summary: Tests: 0 total\n"));
    }

    #[test]
    fn a_configuration_error_is_a_failure_without_a_file() {
        let (failures, text) = filter_output("jest", CONFIG_ERROR);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].name, "Validation Error");
        assert!(
            text.starts_with("(no file)\n  Validation Error: Module ts-jest in the transform option was not found.\n")
        );
    }

    #[test]
    fn coloured_badges_and_the_closing_summary_do_not_duplicate_failures() {
        let raw = "\u{1b}[0m\u{1b}[7m\u{1b}[1m\u{1b}[31m FAIL \u{1b}[39m\u{1b}[22m\u{1b}[27m\u{1b}[0m \u{1b}[2msrc/\u{1b}[22m\u{1b}[1ma.test.ts\u{1b}[22m\n\
                   \u{1b}[1m\u{1b}[31m  \u{1b}[1m● \u{1b}[22m\u{1b}[1madds\u{1b}[39m\u{1b}[22m\n\n    boom\n\n      at Object.<anonymous> (src/a.test.ts:3:9)\n\n\
                   Summary of all failing tests\n FAIL  src/a.test.ts\n  ● adds\n\n    boom\n\n      at Object.<anonymous> (src/a.test.ts:3:9)\n\n\
                   Test Suites: 1 failed, 1 total\nTests:       1 failed, 1 total\n";
        let (failures, _) = filter_output("jest", raw);
        assert_eq!(failures.len(), 1);
        assert_eq!(
            (failures[0].file.as_str(), failures[0].line, failures[0].name.as_str()),
            ("src/a.test.ts", Some(3), "adds")
        );
    }
}
