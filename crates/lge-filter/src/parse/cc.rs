//! C, C++ and Objective-C compilers (gcc, clang) and the linker behind them:
//! `file.c:line:col: error: message [-Wflag]`, the source and caret lines under it, and
//! `ld:` or `undefined reference to` lines. clang closes with `N errors generated.`;
//! gcc has no closing line, so a gcc run is never called passed on its word.

use super::{closing_state, number, pattern, Parsed};
use crate::format::failure;

pattern!(
    DIAG,
    r"^(\S+?):(\d+):(\d+): (fatal error|error|warning|note): (.*?)(?: \[(-W[\w=+,-]+)\])?$"
);
// The source line with its gutter (`    7 |     x = 1;`), the caret line, or an indented
// source line without a gutter (older clang).
pattern!(SNIPPET, r"^\s*\d* \||^\s+[\^~]|^\s+\S");
pattern!(INCLUDED, r"^In file included from ");
pattern!(INCLUDED_MORE, r"^\s+from \S+:\d+");
pattern!(LD, r"^(?:/usr/bin/)?ld: (.*)$");
pattern!(UNDEFINED_SYMBOLS, r"^Undefined symbols for architecture ");
pattern!(
    DRIVER,
    r"^(clang\+\+|clang|gcc|g\+\+|cc|c\+\+|collect2): (?:fatal )?error: (.*)$"
);
pattern!(CLOSING, r"^\d+ (?:errors?|warnings?)(?: and \d+ errors?)? generated\.");
pattern!(ERRORS, r"(?:^|and )(\d+) errors? generated\.");

/// Lines kept under a diagnostic: its source, caret and fix-it lines, and its notes.
const EXCERPT_LINES: usize = 6;

/// Extensions of the files a C-family compiler reports on: sources, headers, template
/// and inline implementation files, CUDA and C++ modules.
const C_EXTENSIONS: [&str; 24] = [
    "c", "h", "cc", "cpp", "cxx", "c++", "hpp", "hh", "hxx", "h++", "m", "mm", "C", "H", "tpp", "ipp", "inl", "tcc",
    "txx", "cu", "cuh", "ixx", "cppm", "mpp",
];

/// Files a C-family compiler reports on: a known extension, or a header without one
/// in a folder (`/usr/include/c++/13/variant`, the standard library's headers).
pub(super) fn is_c_source(file: &str) -> bool {
    let base = file.rsplit('/').next().unwrap_or(file);
    match base.rsplit_once('.') {
        Some((stem, ext)) => !stem.is_empty() && C_EXTENSIONS.contains(&ext),
        None => file.contains('/') && !base.is_empty(),
    }
}

pub(crate) fn parse(text: &str) -> Parsed {
    let lines: Vec<&str> = text.lines().collect();
    let mut parsed = Parsed {
        owns_files: Some(is_c_source),
        ..Parsed::default()
    };
    let mut diagnostics = 0;
    let mut reported: Option<usize> = None;
    let mut foreign = false;

    for (i, line) in lines.iter().enumerate() {
        if let Some(c) = DIAG.captures(line).filter(|c| !is_c_source(&c[1])) {
            // Another tool's file, or a C-family file in a form not known here: not
            // read, so it is not lost behind the ones that are.
            if &c[4] != "note" {
                foreign = true;
            }
        } else if let Some(c) = DIAG.captures(line) {
            let severity = &c[4];
            if severity == "note" {
                // Notes are read with the diagnostic they explain.
                continue;
            }
            diagnostics += 1;
            let mut excerpt: Vec<String> = Vec::new();
            if let Some(included) = lines[..i]
                .iter()
                .rev()
                .take_while(|l| INCLUDED.is_match(l) || INCLUDED_MORE.is_match(l))
                .find(|l| INCLUDED.is_match(l))
            {
                excerpt.push(included.to_string());
            }
            let detail = lines[i + 1..]
                .iter()
                .take_while(|l| {
                    !INCLUDED_MORE.is_match(l)
                        && (SNIPPET.is_match(l)
                            || DIAG.captures(l).is_some_and(|d| &d[4] == "note" && is_c_source(&d[1])))
                })
                .take(EXCERPT_LINES);
            excerpt.extend(detail.map(|l| l.to_string()));
            let flag = c.get(6).map(|m| m.as_str().rsplit(',').next().unwrap_or(""));
            let error = severity != "warning";
            let name = flag.unwrap_or(if error { "error" } else { "warning" });
            let f = failure(&c[1], number(&c[2]), name, &c[5], &excerpt);
            if error {
                parsed.failures.push(f);
            } else {
                parsed.warnings.push(f);
            }
        } else if let Some(c) = LD.captures(line) {
            parsed.failures.push(failure("", None, "ld", &c[1], &[]));
        } else if line.contains("undefined reference to") {
            parsed.failures.push(failure("", None, "ld", line.trim(), &[]));
        } else if UNDEFINED_SYMBOLS.is_match(line) {
            let symbols: Vec<String> = lines[i + 1..]
                .iter()
                .take_while(|l| l.starts_with(char::is_whitespace) && !l.trim().is_empty())
                .take(10)
                .map(|l| l.to_string())
                .collect();
            parsed.failures.push(failure("", None, "ld", line.trim(), &symbols));
        } else if let Some(c) = DRIVER.captures(line) {
            parsed.failures.push(failure("", None, &c[1], &c[2], &[]));
        } else if CLOSING.is_match(line) {
            parsed.summary = Some(line.trim().trim_end_matches('.').to_string());
            let errors: usize = ERRORS.captures(line).and_then(|c| c[1].parse().ok()).unwrap_or(0);
            reported = Some(reported.unwrap_or(0) + errors);
        }
    }
    parsed.recognised = diagnostics > 0;
    // A diagnostic line this parser did not read: shown with the unparsed output.
    parsed.partial = parsed.recognised && foreign;
    parsed.reported = reported;
    (parsed.closed, parsed.trailing) = closing_state(text, &[&CLOSING], &[]);
    parsed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{filter_output, filter_with, FilterOptions};

    // clang 21 captures, and gcc output in the shape gcc 13 prints it.
    const CLANG_FAIL: &str = include_str!("../../testdata/clang_fail.txt");
    const CLANG_WARN: &str = include_str!("../../testdata/clang_warn.txt");
    const CLANG_LINK: &str = include_str!("../../testdata/clang_link.txt");
    const GCC_FAIL: &str = include_str!("../../testdata/gcc_fail.txt");
    const GCC_LINK: &str = include_str!("../../testdata/gcc_link.txt");
    const MAKE_GCC: &str = include_str!("../../testdata/make_gcc_fail.txt");

    #[test]
    fn clang_errors_with_their_include_line_and_notes() {
        let parsed = parse(CLANG_FAIL);
        assert!(parsed.recognised && parsed.closed && !parsed.trailing);
        assert_eq!(
            (parsed.summary.as_deref(), parsed.reported),
            (Some("2 errors generated"), Some(2))
        );
        let got: Vec<(&str, Option<u32>, &str, &str)> = parsed
            .failures
            .iter()
            .map(|f| (f.file.as_str(), f.line, f.name.as_str(), f.message.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("src/util.h", Some(4), "error", "unknown type name 'unknown_t'"),
                (
                    "src/main.c",
                    Some(7),
                    "error",
                    "use of undeclared identifier 'totl'; did you mean 'total'?"
                ),
            ]
        );
        assert_eq!(
            parsed.failures[0].excerpt,
            "In file included from src/main.c:2:\n    4 | unknown_t helper(void);\n      | ^"
        );
        assert!(parsed.failures[1]
            .excerpt
            .contains("src/main.c:6:9: note: 'total' declared here"));
        assert_eq!(parsed.failures[1].excerpt.lines().count(), EXCERPT_LINES);
    }

    #[test]
    fn warnings_are_named_by_their_flag() {
        let parsed = parse(CLANG_WARN);
        let got: Vec<(Option<u32>, &str)> = parsed.warnings.iter().map(|f| (f.line, f.name.as_str())).collect();
        assert_eq!(
            got,
            [
                (Some(5), "-Wparentheses"),
                (Some(4), "-Wunused-variable"),
                (Some(12), "-Wformat")
            ]
        );
        assert!(parsed.failures.is_empty() && parsed.closed);
        assert_eq!(
            filter_output("clang", CLANG_WARN),
            (Vec::new(), "ok: 3 warnings generated".to_string())
        );
        let werror = parse("a.c:3:9: error: unused variable 'x' [-Werror,-Wunused-variable]\n");
        assert_eq!(werror.failures[0].name, "-Wunused-variable");
        assert_eq!(werror.failures[0].message, "unused variable 'x'");
    }

    #[test]
    fn linker_errors() {
        let failed = FilterOptions {
            passed: Some(false),
            ..FilterOptions::default()
        };
        let (failures, _) = filter_with("clang", CLANG_LINK, &failed);
        let names: Vec<&str> = failures.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["clang", "ld", "ld"]);
        assert!(failures
            .iter()
            .any(|f| f.excerpt.contains("\"_missing_function\", referenced from:")));
        // No diagnostic on a source file: not claimed when sniffed.
        assert!(!parse(CLANG_LINK).recognised);
    }

    #[test]
    fn gcc_has_no_closing_line() {
        let parsed = parse(GCC_FAIL);
        assert!(parsed.recognised && !parsed.closed);
        let names: Vec<&str> = parsed.failures.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["error"]);
        assert_eq!(parsed.warnings.len(), 1);
        assert_eq!(parsed.warnings[0].name, "-Wunused-variable");
        assert!(parsed.failures[0].excerpt.contains("note: each undeclared identifier"));

        let link = parse(GCC_LINK);
        let names: Vec<&str> = link.failures.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["ld", "ld", "collect2"]);
        assert_eq!(link.failures[2].message, "ld returned 1 exit status");
        assert!(!link.recognised);
    }

    #[test]
    fn under_make_the_compiler_is_sniffed_once() {
        let (failures, text) = filter_output("make", MAKE_GCC);
        // mypy reads the same lines; the compiler's parser owns them.
        assert_eq!(failures.len(), 2, "{text}");
        assert_eq!(failures[0].name, "error");
        assert_eq!(failures[1].name, "-Werror=implicit-function-declaration");
        assert_eq!(filter_output("make", MAKE_GCC), filter_output("gcc", MAKE_GCC));
    }
}
