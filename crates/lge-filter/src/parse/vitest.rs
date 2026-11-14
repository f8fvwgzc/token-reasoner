//! Vitest: after the file tree, a `FAIL  file > suite > test` block per failing test,
//! separated by `⎯⎯⎯` rules, then `Test Files` and `Tests` totals.

use super::jest::is_library_frame;
use super::{count_of, number, pattern, squeeze, Parsed};
use crate::format::{clip_lines, failure};
use crate::Failure;

pattern!(FAIL, r"^\s*FAIL\s+(.+)$");
// ` ❯ src/cart.test.ts:12:26`, ` ❯ applyDiscount src/pricing.ts:5:28`, `  File: /abs/a.test.ts:2:28`
pattern!(FRAME, r"^\s*(?:❯|File:) (?:.*\s)?(\S+?):(\d+):(\d+)$");
pattern!(TESTS, r"^\s*Tests\s{2,}(.*)$");
pattern!(FILES, r"^\s*Test Files\s{2,}(.*)$");
// A file that could not be loaded: `src/orders.test.ts [ src/orders.test.ts ]`.
pattern!(WHOLE_FILE, r"^(\S+) \[ .* \]$");
// A workspace project in front of the file: `|unit| src/a.test.ts > adds`.
pattern!(PROJECT, r"^\|[^|]*\|\s+");

const SUITE_FAILED: &str = "suite failed to run";

struct Block {
    file: String,
    name: String,
    lines: Vec<String>,
}

impl Block {
    fn open(header: &str) -> Block {
        let header = PROJECT.replace(header.trim(), "");
        let (file, name) = match (WHOLE_FILE.captures(&header), header.split_once(" > ")) {
            (Some(c), _) => (c[1].to_string(), SUITE_FAILED.to_string()),
            (None, Some((file, name))) => (file.to_string(), name.to_string()),
            (None, None) => (
                header.split_whitespace().next().unwrap_or("").to_string(),
                SUITE_FAILED.to_string(),
            ),
        };
        Block {
            file,
            name,
            lines: Vec::new(),
        }
    }

    fn close(self) -> Failure {
        let message = self
            .lines
            .iter()
            .map(|l| l.trim())
            .find(|l| !l.is_empty())
            .unwrap_or("");
        let line = self
            .lines
            .iter()
            .filter_map(|l| FRAME.captures(l))
            .find(|c| c[1].ends_with(&self.file))
            .and_then(|c| number(&c[2]));
        let excerpt: Vec<String> = self
            .lines
            .iter()
            .filter(|l| !l.trim().is_empty() && !is_library_frame(l))
            .cloned()
            .collect();
        failure(&self.file, line, &self.name, message, &clip_lines(excerpt, 12, 4))
    }
}

pub(crate) fn parse(text: &str) -> Parsed {
    let mut parsed = Parsed::default();
    let mut open: Option<Block> = None;
    let (mut files, mut tests) = (None, None);
    let (mut saw_rule, mut saw_banner) = (false, false);

    for line in text.lines() {
        let rule = line.trim_start().starts_with('⎯');
        let header = FAIL.captures(line);
        let totals = FILES.is_match(line) || TESTS.is_match(line);
        if rule || header.is_some() || totals {
            parsed.failures.extend(open.take().map(Block::close));
        }
        saw_rule |= rule;
        saw_banner |= line.trim_start().starts_with("RUN  v");
        if let Some(c) = header {
            open = Some(Block::open(&c[1]));
        } else if let Some(c) = FILES.captures(line) {
            files = Some(squeeze(&c[1]));
        } else if let Some(c) = TESTS.captures(line) {
            tests = Some(squeeze(&c[1]));
        } else if let Some(block) = open.as_mut() {
            block.lines.push(line.to_string());
        }
    }
    parsed.failures.extend(open.take().map(Block::close));

    let failed_tests = tests.as_deref().map_or(0, |t| count_of(t, "failed"));
    let failed_files = files.as_deref().map_or(0, |t| count_of(t, "failed"));
    parsed.recognised = tests.is_some() || files.is_some() || saw_banner || (saw_rule && !parsed.failures.is_empty());
    if tests.is_some() || files.is_some() {
        parsed.reported = Some(if failed_tests > 0 { failed_tests } else { failed_files });
    }
    parsed.summary = tests
        .map(|t| format!("Tests {t}"))
        .or(files.map(|f| format!("Test Files {f}")));
    parsed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter_output;

    const PASS: &str = include_str!("../../testdata/vitest_pass.txt");
    const FAIL_RUN: &str = include_str!("../../testdata/vitest_fail.txt");
    const SUITE_ERROR: &str = include_str!("../../testdata/vitest_suite_error.txt");

    #[test]
    fn passing_run_is_one_line() {
        assert_eq!(
            filter_output("vitest", PASS),
            (Vec::new(), "ok: Tests 7 passed (7)".to_string())
        );
    }

    #[test]
    fn failing_tests_with_their_file_line_and_frame() {
        let parsed = parse(FAIL_RUN);
        assert_eq!(parsed.reported, Some(3));
        assert_eq!(parsed.summary.as_deref(), Some("Tests 3 failed | 5 passed (8)"));
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
                    "Cart > totals items",
                    "AssertionError: expected 1 to be 1.01 // Object.is equality"
                ),
                (
                    "src/cart.test.ts",
                    Some(22),
                    "Cart > applyDiscount > handles ten percent",
                    "TypeError: Cannot read properties of undefined (reading 'percent')"
                ),
                (
                    "src/views.test.tsx",
                    Some(16),
                    "render > shows the order id",
                    "AssertionError: expected { id: undefined, total: 12 } to deeply equal { id: 'o-1', total: 12 }"
                ),
            ]
        );
        let first = &parsed.failures[0];
        assert!(first.excerpt.contains("- 1.01\n+ 1\n"));
        assert!(first.excerpt.contains("12|     expect(cart.total()).toBe(1.01);"));
        assert!(parsed.failures[1]
            .excerpt
            .contains("❯ applyDiscount src/pricing.ts:5:28"));
    }

    #[test]
    fn a_file_that_cannot_load_is_a_failure() {
        let (failures, text) = filter_output("vitest", SUITE_ERROR);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].file, "src/orders.test.ts");
        assert_eq!(failures[0].name, "suite failed to run");
        assert_eq!(failures[0].line, Some(2));
        assert!(failures[0]
            .message
            .starts_with("Error: Failed to resolve import \"./orderz\""));
        assert!(!failures[0].excerpt.contains("node_modules"));
        assert!(text.ends_with("summary: Tests no tests\n"));
    }

    #[test]
    fn workspace_project_prefix_is_not_the_file() {
        let block = Block::open("|unit| src/a.test.ts > math > adds");
        assert_eq!(
            (block.file.as_str(), block.name.as_str()),
            ("src/a.test.ts", "math > adds")
        );
        let block = Block::open("src/a.test.ts");
        assert_eq!(
            (block.file.as_str(), block.name.as_str()),
            ("src/a.test.ts", SUITE_FAILED)
        );
    }
}
