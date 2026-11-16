//! tsc: `file(line,col): error TS2322: message` when piped, and
//! `file:line:col - error TS2322: message` followed by the source line with `--pretty`.

use super::{closing_state, number, pattern, Parsed};
use crate::format::{clip_lines, failure};
use crate::Failure;

pattern!(PLAIN, r"^(\S.*?)\((\d+),(\d+)\): (error|warning) (TS\d+): (.*)$");
pattern!(PRETTY, r"^(\S.*?):(\d+):(\d+) - (error|warning) (TS\d+): (.*)$");
// Errors about the project itself have no location: `error TS5058: The specified path does not exist`.
pattern!(GLOBAL, r"^error (TS\d+): (.*)$");
pattern!(FOUND, r"^Found (\d+) errors?\b");
// Under a diagnostic: indented elaboration and related locations, or the numbered source line.
pattern!(DETAIL, r"^(?:\s+\S|\d+\s)");
// The table of files tsc prints after `Found N errors in M files.`
pattern!(TABLE_HEAD, r"^\s*Errors\s+Files\s*$");
pattern!(TABLE_ROW, r"^\s+\d+\s+\S+:\d+\s*$");

struct Open {
    file: String,
    line: Option<u32>,
    code: String,
    message: String,
    detail: Vec<String>,
}

impl Open {
    fn close(self) -> Failure {
        failure(
            &self.file,
            self.line,
            &self.code,
            &self.message,
            &clip_lines(self.detail, 6, 2),
        )
    }
}

pub(crate) fn parse(text: &str) -> Parsed {
    let mut parsed = Parsed::default();
    let mut open: Option<Open> = None;
    let mut found: Option<(String, Option<usize>)> = None;
    for line in text.lines() {
        let located = PLAIN.captures(line).or_else(|| PRETTY.captures(line));
        if let Some(c) = located {
            parsed.failures.extend(open.take().map(Open::close));
            open = Some(Open {
                file: c[1].to_string(),
                line: number(&c[2]),
                code: c[5].to_string(),
                message: c[6].to_string(),
                detail: Vec::new(),
            });
        } else if let Some(c) = GLOBAL.captures(line) {
            parsed.failures.extend(open.take().map(Open::close));
            open = Some(Open {
                file: String::new(),
                line: None,
                code: c[1].to_string(),
                message: c[2].to_string(),
                detail: Vec::new(),
            });
        } else if let Some(c) = FOUND.captures(line) {
            // The table of files after this line repeats what the diagnostics said.
            parsed.failures.extend(open.take().map(Open::close));
            found = Some((line.trim().to_string(), c[1].parse().ok()));
        } else if DETAIL.is_match(line) {
            if let Some(o) = open.as_mut() {
                o.detail.push(line.to_string());
            }
        } else if !line.trim().is_empty() {
            parsed.failures.extend(open.take().map(Open::close));
        }
    }
    parsed.failures.extend(open.take().map(Open::close));
    // `Found N errors` is also how ruff and mypy close: it is tsc's only next to tsc's errors.
    parsed.recognised = !parsed.failures.is_empty();
    if let Some((summary, reported)) = found.filter(|_| parsed.recognised) {
        parsed.summary = Some(summary);
        parsed.reported = reported;
    }
    (parsed.closed, parsed.trailing) = closing_state(text, &[&FOUND], &[&TABLE_HEAD, &TABLE_ROW]);
    parsed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter_output;

    // Captured from tsc 5.7.2; the pretty sample still has its colour codes.
    const FAIL: &str = include_str!("../../testdata/tsc_fail.txt");
    const PRETTY_RUN: &str = include_str!("../../testdata/tsc_fail_pretty.txt");
    const CRASH: &str = include_str!("../../testdata/tsc_crash.txt");

    fn located(failures: &[Failure]) -> Vec<(&str, Option<u32>, &str)> {
        failures
            .iter()
            .map(|f| (f.file.as_str(), f.line, f.name.as_str()))
            .collect()
    }

    const EXPECTED: [(&str, Option<u32>, &str); 4] = [
        ("src/ordersClient.ts", Some(2), "TS2307"),
        ("src/ordersClient.ts", Some(9), "TS2345"),
        ("src/routes.ts", Some(4), "TS2322"),
        ("src/routes.ts", Some(9), "TS2304"),
    ];

    #[test]
    fn passing_run_prints_nothing_and_is_one_line() {
        assert_eq!(filter_output("tsc", ""), (Vec::new(), "ok".to_string()));
    }

    #[test]
    fn plain_format() {
        let (failures, text) = filter_output("tsc", FAIL);
        assert_eq!(located(&failures), EXPECTED);
        assert_eq!(
            text,
            "src/ordersClient.ts\n\
             \x20 2: TS2307: Cannot find module './nope' or its corresponding type declarations.\n\
             \x20 9: TS2345: Argument of type '{ id: string; }' is not assignable to parameter of type 'Order'.\n\
             \x20     Property 'total' is missing in type '{ id: string; }' but required in type 'Order'.\n\
             src/routes.ts\n\
             \x20 4: TS2322: Type 'string' is not assignable to type 'number'.\n\
             \x20 9: TS2304: Cannot find name 'orders'.\n"
        );
    }

    #[test]
    fn pretty_format_with_colour_codes() {
        assert!(PRETTY_RUN.contains('\u{1b}'), "the sample keeps tsc's escape codes");
        let (failures, text) = filter_output("tsc", PRETTY_RUN);
        assert_eq!(located(&failures), EXPECTED);
        assert_eq!(
            failures[2].excerpt,
            "4   const total: number = params.id;\n          ~~~~~"
        );
        // The elaboration, the source line and the related location all stay with the error.
        assert!(failures[1].excerpt.starts_with("  Property 'total' is missing in type"));
        assert!(failures[1].excerpt.contains("'total' is declared here."));
        assert!(text.ends_with("summary: Found 4 errors in 2 files.\n"));
        assert!(!text.contains('\u{1b}'));
        assert!(!text.contains("Errors  Files"));
    }

    #[test]
    fn project_errors_have_no_file() {
        let (failures, text) = filter_output("tsc", CRASH);
        assert_eq!(located(&failures), [("", None, "TS5058")]);
        assert_eq!(
            text,
            "(no file)\n  TS5058: The specified path does not exist: 'nope.json'.\n"
        );
    }

    #[test]
    fn another_tools_closing_line_is_not_claimed() {
        for foreign in [
            "Found 1 error.\n",
            "Found 2 errors in 1 file (checked 3 source files)\n",
        ] {
            let parsed = parse(foreign);
            assert!(!parsed.recognised && parsed.reported.is_none() && parsed.summary.is_none());
        }
    }

    #[test]
    fn a_path_with_parentheses() {
        let parsed = parse("app/(shop)/page.tsx(3,5): error TS2304: Cannot find name 'x'.\n");
        assert_eq!(located(&parsed.failures), [("app/(shop)/page.tsx", Some(3), "TS2304")]);
    }

    #[test]
    fn script_runner_noise_does_not_join_the_last_error() {
        let raw =
            "src/a.ts(1,1): error TS2304: Cannot find name 'x'.\nnpm error code 2\nnpm error path /home/dev/web\n";
        let parsed = parse(raw);
        assert_eq!(parsed.failures.len(), 1);
        assert_eq!(parsed.failures[0].excerpt, "");
    }
}
