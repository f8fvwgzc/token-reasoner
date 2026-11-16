//! pyright: a heading per file, then `  path:line:col - error: message (rule)` with
//! indented continuation lines, and a closing `N errors, M warnings, K informations`.
//! pyright prints absolute paths; the runner makes them relative.

use super::{closing_state, number, pattern, Parsed};
use crate::format::{clip_lines, failure};
use crate::Failure;

pattern!(DIAG, r"^\s*(.+?):(\d+):(\d+) - (error|warning|information): (.*)$");
pattern!(RULE, r"^(.*?)\s*\((report[A-Za-z]+)\)\s*$");
pattern!(
    TOTALS,
    r"^(\d+) errors?, (\d+) warnings?, (\d+) (?:informations?|notes?)\s*$"
);

struct Open {
    file: String,
    line: Option<u32>,
    severity: String,
    message: String,
    detail: Vec<String>,
}

impl Open {
    /// The rule closes the last line of a diagnostic, which may be a continuation line.
    fn close(mut self) -> (String, Failure) {
        let last = self.detail.last_mut().unwrap_or(&mut self.message);
        let rule = RULE
            .captures(last.as_str())
            .map(|c| (c[1].to_string(), c[2].to_string()));
        let name = match rule {
            Some((rest, rule)) => {
                *last = rest;
                rule
            }
            None => self.severity.clone(),
        };
        let detail = clip_lines(self.detail, 4, 2);
        (
            self.severity,
            failure(&self.file, self.line, &name, &self.message, &detail),
        )
    }
}

pub(crate) fn parse(text: &str) -> Parsed {
    let mut parsed = Parsed::default();
    let mut open: Option<Open> = None;
    let mut done: Vec<(String, Failure)> = Vec::new();

    for line in text.lines() {
        if let Some(c) = DIAG.captures(line) {
            done.extend(open.take().map(Open::close));
            open = Some(Open {
                file: c[1].to_string(),
                line: number(&c[2]),
                severity: c[4].to_string(),
                message: c[5].to_string(),
                detail: Vec::new(),
            });
        } else if let Some(c) = TOTALS.captures(line) {
            done.extend(open.take().map(Open::close));
            parsed.recognised = true;
            parsed.summary = Some(line.trim().to_string());
            parsed.reported = c[1].parse().ok();
        } else if line.starts_with("    ") && open.is_some() {
            if let Some(o) = open.as_mut() {
                o.detail.push(line.trim().to_string());
            }
        } else {
            done.extend(open.take().map(Open::close));
        }
    }
    done.extend(open.take().map(Open::close));

    for (severity, f) in done {
        match severity.as_str() {
            "error" => parsed.failures.push(f),
            "warning" => parsed.warnings.push(f),
            _ => {}
        }
    }
    parsed.recognised |= !parsed.failures.is_empty() || !parsed.warnings.is_empty();
    (parsed.closed, parsed.trailing) = closing_state(text, &[&TOTALS], &[]);
    parsed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter_output;

    const PASS: &str = include_str!("../../testdata/pyright_pass.txt");
    const FAIL: &str = include_str!("../../testdata/pyright_fail.txt");
    const CRASH: &str = include_str!("../../testdata/pyright_crash.txt");

    #[test]
    fn passing_run_is_one_line() {
        assert_eq!(
            filter_output("pyright", PASS),
            (Vec::new(), "ok: 0 errors, 0 warnings, 0 informations".to_string())
        );
    }

    #[test]
    fn errors_with_rules_and_continuation_lines() {
        let parsed = parse(FAIL);
        assert_eq!(parsed.reported, Some(4));
        assert_eq!(parsed.summary.as_deref(), Some("4 errors, 1 warning, 0 informations"));
        let got: Vec<(&str, Option<u32>, &str)> = parsed
            .failures
            .iter()
            .map(|f| (f.file.as_str(), f.line, f.name.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("/home/dev/shop/shop/orders.py", Some(14), "reportReturnType"),
                ("/home/dev/shop/shop/orders.py", Some(31), "reportArgumentType"),
                ("/home/dev/shop/shop/pricing.py", Some(8), "reportOptionalOperand"),
                (
                    "/home/dev/shop/tests/test_cart.py",
                    Some(5),
                    "reportAttributeAccessIssue"
                ),
            ]
        );
        assert_eq!(
            parsed.failures[0].message,
            "Type \"None\" is not assignable to return type \"Order\""
        );
        assert_eq!(parsed.failures[0].excerpt, "\"None\" is not assignable to \"Order\"");
        assert_eq!(parsed.failures[2].message, "Operator \"/\" not supported for \"None\"");
        assert_eq!(parsed.failures[2].excerpt, "");
        // The warning is kept apart: it does not fail the run.
        assert_eq!(parsed.warnings.len(), 1);
        assert_eq!(parsed.warnings[0].name, "reportMissingModuleSource");
    }

    #[test]
    fn a_crash_is_shown_whole() {
        let (failures, text) = filter_output("pyright", CRASH);
        assert!(failures.is_empty());
        assert_eq!(text, CRASH);
    }
}
