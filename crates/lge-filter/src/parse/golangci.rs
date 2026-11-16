//! golangci-lint, text output: `path:line:col: message (linter)`, then the source line
//! and a caret under it; v2 closes with `N issues:` and a count per linter.

use super::{closing_state, number, pattern, Parsed};
use crate::format::failure;

// golangci-lint lints Go code: an issue is on a `.go` file or on `go.mod`. Other tools
// end lines with a word in brackets too.
pattern!(ISSUE, r"^(\S+?\.go|\S*go\.mod):(\d+)(?::(\d+))?: (.+) \(([\w-]+)\)$");
pattern!(CARET, r"^\s*\^");
pattern!(CLOSING, r"^(\d+) issues?\b");
// The count per linter after the closing line.
pattern!(EPILOGUE, r"^\* [\w-]+: \d+");

pub(crate) fn parse(text: &str) -> Parsed {
    let lines: Vec<&str> = text.lines().collect();
    let mut parsed = Parsed::default();
    for (i, line) in lines.iter().enumerate() {
        if let Some(c) = ISSUE.captures(line) {
            // The source line under the issue; the caret under it says nothing more.
            let source: Vec<String> = lines
                .get(i + 1)
                .filter(|l| !l.trim().is_empty() && !CARET.is_match(l) && !ISSUE.is_match(l) && !CLOSING.is_match(l))
                .map(|l| vec![l.trim().to_string()])
                .unwrap_or_default();
            parsed
                .failures
                .push(failure(&c[1], number(&c[2]), &c[5], &c[4], &source));
        } else if let Some(c) = CLOSING.captures(line) {
            parsed.summary = Some(line.trim().trim_end_matches([':', '.']).to_string());
            parsed.reported = c[1].parse().ok();
        }
    }
    parsed.recognised = !parsed.failures.is_empty() || parsed.summary.is_some();
    (parsed.closed, parsed.trailing) = closing_state(text, &[&CLOSING], &[&EPILOGUE]);
    parsed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter_output;

    // golangci-lint v1 (no closing line) and v2 (`N issues:` and a count per linter).
    const V1: &str = include_str!("../../testdata/golangci_v1.txt");
    const V2: &str = include_str!("../../testdata/golangci_v2.txt");

    #[test]
    fn issues_with_their_linter_and_source_line() {
        for raw in [V1, V2] {
            let parsed = parse(raw);
            let got: Vec<(&str, Option<u32>, &str)> = parsed
                .failures
                .iter()
                .map(|f| (f.file.as_str(), f.line, f.name.as_str()))
                .collect();
            assert_eq!(
                got,
                [
                    ("cart/cart.go", Some(12), "errcheck"),
                    ("cart/cart.go", Some(20), "ineffassign"),
                    ("pricing/pricing.go", Some(5), "unused"),
                    ("go.mod", Some(3), "gomoddirectives"),
                ]
            );
            assert_eq!(
                parsed.failures[0].message,
                "Error return value of `f.Close` is not checked"
            );
            assert_eq!(parsed.failures[0].excerpt, "defer f.Close()");
            assert!(parsed.failures.iter().all(|f| !f.excerpt.contains('^')));
        }
        let v2 = parse(V2);
        assert_eq!((v2.summary.as_deref(), v2.reported), (Some("4 issues"), Some(4)));
        assert!(v2.closed && !v2.trailing);
        let v1 = parse(V1);
        assert!(v1.recognised && !v1.closed);
    }

    #[test]
    fn a_clean_v2_run_is_closed() {
        assert_eq!(
            filter_output("golangci-lint", "0 issues.\n"),
            (Vec::new(), "ok: 0 issues".to_string())
        );
        let parsed = parse("0 issues.\n");
        assert!(parsed.closed && !parsed.trailing && parsed.recognised);
    }

    #[test]
    fn lines_of_other_tools_are_not_issues() {
        for line in [
            "shop/orders.py:1:1: E501 Line too long (120 > 88)",
            "src/a.ts:3:1: something (rule)",
            "src/main.c:3:1: error: implicit declaration (C99)",
        ] {
            assert!(!parse(line).recognised, "{line}");
        }
    }
}
