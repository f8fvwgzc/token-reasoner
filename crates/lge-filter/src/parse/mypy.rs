//! mypy: `path:line: error: message  [code]`, optional `note:` lines for the same
//! place, and a closing `Found N errors in M files`.
//!
//! The line shape is shared by many compilers (`file:line:col: error: message`), so
//! this parser also serves them when the tool is unknown.

use super::{closing_state, number, pattern, Parsed};
use crate::format::{clip_lines, failure};
use crate::Failure;

pattern!(
    DIAG,
    r"^([^\s:][^:]*):(\d+)(?::\d+)?(?::\d+:\d+)?: (error|warning|note): (.*)$"
);
pattern!(CODE, r"^(.*?)\s+\[([a-z][a-z0-9-]*)\]$");
pattern!(FOUND, r"^Found (\d+) errors? in \d+ files? \(.*\)$");
pattern!(SUCCESS, r"^Success: (no issues found in \d+ source files?)$");
pattern!(
    CLOSING,
    r"^(?:Found \d+ errors? in \d+ files?|Success: no issues found)"
);

/// An error being read: its message can wrap over several lines with `--pretty`.
struct Open {
    file: String,
    line: Option<u32>,
    warning: bool,
    message: String,
    /// Lines joined to the message so far; a wrapped message is two or three lines long.
    wrapped: u8,
    excerpt: Vec<String>,
}

impl Open {
    fn close(self) -> (bool, Failure) {
        let (message, code) = match CODE.captures(&self.message) {
            Some(c) => (c[1].to_string(), c[2].to_string()),
            None => (
                self.message.clone(),
                if self.warning { "warning" } else { "error" }.to_string(),
            ),
        };
        let excerpt = clip_lines(self.excerpt, 4, 2);
        (self.warning, failure(&self.file, self.line, &code, &message, &excerpt))
    }
}

pub(crate) fn parse(text: &str) -> Parsed {
    let mut parsed = Parsed::default();
    let mut open: Option<Open> = None;
    let mut done: Vec<(bool, Failure)> = Vec::new();

    for line in text.lines() {
        if let Some(c) = DIAG.captures(line) {
            if &c[3] == "note" {
                // A note explains the error just above it; alone it is not a failure.
                if let Some(o) = open.as_mut().filter(|o| o.file == c[1]) {
                    o.excerpt.push(format!("note: {}", &c[4]));
                }
                continue;
            }
            done.extend(open.take().map(Open::close));
            open = Some(Open {
                file: c[1].to_string(),
                line: number(&c[2]),
                warning: &c[3] == "warning",
                message: c[4].to_string(),
                wrapped: 0,
                excerpt: Vec::new(),
            });
        } else if let Some(c) = FOUND.captures(line) {
            done.extend(open.take().map(Open::close));
            parsed.recognised = true;
            parsed.summary = Some(line.to_string());
            parsed.reported = c[1].parse().ok();
        } else if let Some(c) = SUCCESS.captures(line) {
            done.extend(open.take().map(Open::close));
            parsed.recognised = true;
            parsed.summary = Some(c[1].to_string());
        } else if let Some(o) = open.as_mut() {
            if line.starts_with(' ') {
                // `--pretty`: the source line and the marker under it.
                o.excerpt.push(line.to_string());
            } else if !line.is_empty() && o.excerpt.is_empty() && o.wrapped < 2 && !CODE.is_match(&o.message) {
                // `--pretty` wraps a long message; the code closes it.
                o.message = format!("{} {}", o.message, line.trim());
                o.wrapped += 1;
            } else {
                done.extend(open.take().map(Open::close));
            }
        }
    }
    done.extend(open.take().map(Open::close));

    for (warning, f) in done {
        if warning {
            parsed.warnings.push(f);
        } else {
            parsed.failures.push(f);
        }
    }
    parsed.recognised |= !parsed.failures.is_empty() || !parsed.warnings.is_empty();
    (parsed.closed, parsed.trailing) = closing_state(text, &[&CLOSING], &[]);
    parsed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter_output;

    const PASS: &str = include_str!("../../testdata/mypy_pass.txt");
    const FAIL: &str = include_str!("../../testdata/mypy_fail.txt");
    const PRETTY: &str = include_str!("../../testdata/mypy_fail_pretty.txt");
    const SYNTAX: &str = include_str!("../../testdata/mypy_syntax.txt");
    const CRASH: &str = include_str!("../../testdata/mypy_crash.txt");

    #[test]
    fn passing_run_is_one_line() {
        assert_eq!(
            filter_output("mypy", PASS),
            (Vec::new(), "ok: no issues found in 14 source files".to_string())
        );
    }

    #[test]
    fn errors_with_codes_and_notes() {
        let parsed = parse(FAIL);
        assert_eq!(parsed.reported, Some(5));
        let got: Vec<(&str, Option<u32>, &str)> = parsed
            .failures
            .iter()
            .map(|f| (f.file.as_str(), f.line, f.name.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("shop/orders.py", Some(14), "return-value"),
                ("shop/orders.py", Some(31), "arg-type"),
                ("shop/pricing.py", Some(8), "operator"),
                ("shop/pricing.py", Some(19), "no-untyped-def"),
                ("tests/test_cart.py", Some(5), "attr-defined"),
            ]
        );
        assert_eq!(
            parsed.failures[0].message,
            "Incompatible return value type (got \"None\", expected \"Order\")"
        );
        assert_eq!(
            parsed.failures[2].excerpt,
            "note: Right operand is of type \"float | None\""
        );
        assert_eq!(parsed.failures[0].excerpt, "");
    }

    #[test]
    fn pretty_output_joins_wrapped_messages_and_keeps_the_source_line() {
        let parsed = parse(PRETTY);
        assert_eq!(parsed.failures.len(), 2);
        let first = &parsed.failures[0];
        assert_eq!(first.name, "return-value");
        assert_eq!(
            first.message,
            "Incompatible return value type (got \"None\", expected \"Order\")"
        );
        assert_eq!(first.excerpt, "return None\n       ^~~~");
        let second = &parsed.failures[1];
        assert_eq!(second.name, "operator");
        assert!(second
            .excerpt
            .ends_with("note: Right operand is of type \"float | None\""));
    }

    #[test]
    fn syntax_error_that_stops_the_check() {
        let (failures, text) = filter_output("mypy", SYNTAX);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].name, "syntax");
        assert!(text.ends_with("summary: Found 1 error in 1 file (errors prevented further checking)\n"));
    }

    #[test]
    fn a_crash_is_shown_whole() {
        let (failures, text) = filter_output("mypy", CRASH);
        assert!(failures.is_empty());
        assert_eq!(text, CRASH);
    }

    #[test]
    fn compiler_style_lines_from_other_tools() {
        let parsed =
            parse("src/main.c:12:5: error: use of undeclared identifier 'x'\nsrc/main.c:3:1: warning: unused\n");
        assert_eq!(parsed.failures.len(), 1);
        assert_eq!(parsed.failures[0].name, "error");
        assert_eq!(parsed.failures[0].line, Some(12));
        assert_eq!(parsed.warnings.len(), 1);
    }
}
