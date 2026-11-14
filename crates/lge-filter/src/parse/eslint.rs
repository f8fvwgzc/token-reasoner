//! ESLint, `stylish` (the default: a heading per file, then aligned
//! `line:col  severity  message  rule` rows) and `unix`
//! (`path:line:col: message [Error/rule]`).

use super::{number, pattern, Parsed};
use crate::format::failure;

pattern!(ROW, r"^\s+(\d+):(\d+)\s+(error|warning)\s+(.+?)(?:\s{2,}(\S+))?\s*$");
pattern!(TOTAL, r"^✖ (\d+ problems? \((\d+) errors?, (\d+) warnings?\))$");
pattern!(UNIX, r"^(.+?):(\d+):(\d+): (.*) \[(Error|Warning)(?:/([^\]]+))?\]$");
pattern!(UNIX_TOTAL, r"^(\d+) problems?$");

pub(crate) fn parse(text: &str) -> Parsed {
    let mut parsed = Parsed::default();
    // The last unindented line: the heading of the rows that follow.
    let mut heading = "";
    let mut unix_total: Option<usize> = None;

    for line in text.lines() {
        let found = if let Some(c) = ROW.captures(line) {
            let rule = c.get(5).map_or(&c[3], |m| m.as_str());
            Some((&c[3] == "warning", failure(heading, number(&c[1]), rule, &c[4], &[])))
        } else if let Some(c) = UNIX.captures(line) {
            let severity = c[5].to_lowercase();
            let rule = c.get(6).map_or(severity.as_str(), |m| m.as_str());
            Some((severity == "warning", failure(&c[1], number(&c[2]), rule, &c[4], &[])))
        } else {
            None
        };
        match found {
            Some((true, f)) => parsed.warnings.push(f),
            Some((false, f)) => parsed.failures.push(f),
            None => {
                if let Some(c) = TOTAL.captures(line) {
                    parsed.summary = Some(c[1].to_string());
                    parsed.reported = c[2].parse().ok();
                } else if let Some(c) = UNIX_TOTAL.captures(line) {
                    parsed.summary = Some(line.to_string());
                    unix_total = c[1].parse().ok();
                } else if !line.is_empty() && !line.starts_with(char::is_whitespace) {
                    heading = line;
                }
            }
        }
    }
    // The unix total counts warnings too.
    if let Some(total) = unix_total {
        parsed.reported = Some(total.saturating_sub(parsed.warnings.len()));
    }
    parsed.recognised = !parsed.failures.is_empty() || !parsed.warnings.is_empty() || parsed.summary.is_some();
    parsed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{filter_output, filter_with, FilterOptions};

    const STYLISH: &str = include_str!("../../testdata/eslint_fail_stylish.txt");
    const UNIX_RUN: &str = include_str!("../../testdata/eslint_fail_unix.txt");
    const WARNINGS: &str = include_str!("../../testdata/eslint_warnings_only.txt");
    const CRASH: &str = include_str!("../../testdata/eslint_crash.txt");

    #[test]
    fn passing_run_prints_nothing_and_is_one_line() {
        assert_eq!(filter_output("eslint", ""), (Vec::new(), "ok".to_string()));
    }

    #[test]
    fn stylish_format() {
        let parsed = parse(STYLISH);
        assert_eq!(parsed.reported, Some(4));
        assert_eq!(parsed.summary.as_deref(), Some("5 problems (4 errors, 1 warning)"));
        let got: Vec<(&str, Option<u32>, &str, &str)> = parsed
            .failures
            .iter()
            .map(|f| (f.file.as_str(), f.line, f.name.as_str(), f.message.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                (
                    "/home/dev/web/src/app.ts",
                    Some(12),
                    "@typescript-eslint/no-unused-vars",
                    "'unused' is assigned a value but never used"
                ),
                (
                    "/home/dev/web/src/app.ts",
                    Some(41),
                    "no-console",
                    "Unexpected console statement"
                ),
                (
                    "/home/dev/web/src/routes.ts",
                    Some(5),
                    "error",
                    "Parsing error: Declaration or statement expected"
                ),
                (
                    "/home/dev/web/src/views.tsx",
                    Some(8),
                    "react/react-in-jsx-scope",
                    "'React' must be in scope when using JSX"
                ),
            ]
        );
        assert_eq!(parsed.warnings.len(), 1);
        assert_eq!(parsed.warnings[0].name, "@typescript-eslint/no-explicit-any");
    }

    #[test]
    fn unix_format() {
        let parsed = parse(UNIX_RUN);
        assert_eq!(parsed.reported, Some(3));
        let names: Vec<&str> = parsed.failures.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["@typescript-eslint/no-unused-vars", "no-console", "error"]);
        assert_eq!(parsed.failures[0].file, "/home/dev/web/src/app.ts");
        assert_eq!(parsed.failures[2].line, Some(5));
        assert_eq!(parsed.warnings.len(), 1);
    }

    #[test]
    fn warnings_fail_the_run_only_when_the_exit_code_says_so() {
        // Without an exit code, warnings alone look like a pass.
        assert_eq!(
            filter_output("eslint", WARNINGS),
            (Vec::new(), "ok: 1 problem (0 errors, 1 warning)".to_string())
        );
        // `--max-warnings 0` made the same output a failure: the warning is the reason.
        let failed = FilterOptions {
            passed: Some(false),
            ..FilterOptions::default()
        };
        let (failures, text) = filter_with("eslint", WARNINGS, &failed);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].name, "@typescript-eslint/no-explicit-any");
        assert!(text.starts_with("/home/dev/web/src/app.ts\n  30: @typescript-eslint/no-explicit-any: Unexpected any."));
    }

    #[test]
    fn a_crash_is_shown_whole() {
        let (failures, text) = filter_output("eslint", CRASH);
        assert!(failures.is_empty());
        assert_eq!(text, CRASH);
    }
}
