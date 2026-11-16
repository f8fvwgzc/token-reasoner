//! Turns the raw output of a check into failures.
//!
//! Each tool has a parser: a pure function from text (escape codes already removed) to
//! [`Parsed`]. [`filter_with`] picks the parser, decides whether the run failed, and
//! formats what is left. When no parser recognises the output of a failed run, or a
//! parser finds fewer failures than the tool itself counted, the [`generic`] filter
//! takes over, so a failure is never lost to a format this crate does not know.

use crate::ansi::strip_ansi;
use crate::format::{cap_failures, clip_chars, format_failures, group_by_rule, sort_failures};
use crate::{Failure, MAX_FILTERED_BYTES};
use regex::Regex;

mod cc;
mod eslint;
pub(crate) mod generic;
mod go;
mod golangci;
mod gradle;
mod jest;
mod maven;
mod mypy;
mod pycodes;
mod pyright;
mod pytest;
mod rust;
mod tsc;
mod vitest;

/// Declares a lazily compiled regex. The patterns are constants of this crate, never
/// user input, so a pattern that does not compile is a bug the first test run shows.
macro_rules! pattern {
    ($name:ident, $pattern:expr) => {
        static $name: ::std::sync::LazyLock<::regex::Regex> =
            ::std::sync::LazyLock::new(|| ::regex::Regex::new($pattern).expect("pattern is a valid regex"));
    };
}
pub(crate) use pattern;

/// What a parser found in one output.
#[derive(Debug, Default)]
pub(crate) struct Parsed {
    /// Failing tests and errors.
    pub failures: Vec<Failure>,
    /// Lower-severity diagnostics. They are reported only when the run failed and
    /// there are no errors, because then they are why it failed (`--max-warnings`).
    pub warnings: Vec<Failure>,
    /// The tool's own closing line, without timings.
    pub summary: Option<String>,
    /// How many entries `failures` should have according to the tool's own count.
    pub reported: Option<usize>,
    /// The output has the shape this parser expects, whether it passed or failed.
    /// A closing line alone is not enough when another tool prints the same one
    /// (`Found 1 error.` is ruff's and tsc's): claiming a foreign output as a clean
    /// run would hide its failures.
    pub recognised: bool,
    /// The parser read a failure, but the lines that say what failed are in a form it
    /// does not read (another runner behind `cargo`). The unparsed output is shown too.
    pub partial: bool,
    /// The tool's own closing line is in the output (see [`closing_state`]).
    pub closed: bool,
    /// Lines that are neither blank nor the tool's epilogue come after its closing
    /// line: another program printed after the tool, so its closing line is not the
    /// end of the output.
    pub trailing: bool,
    /// How many test binaries closed with their own `test result:` line (cargo).
    pub suites: usize,
    /// Files whose diagnostics belong to this parser alone when it recognises the
    /// output. A parser that reads the same line shape for any tool (mypy reads every
    /// compiler's `file:line:col: error:`) leaves them out of a merge, so one
    /// diagnostic is not reported twice.
    pub owns_files: Option<fn(&str) -> bool>,
}

impl Parsed {
    /// How many failures the tool counted that the parser did not find.
    fn missing(&self) -> usize {
        self.reported.map_or(0, |n| n.saturating_sub(self.failures.len()))
    }
}

type Parser = fn(&str) -> Parsed;

/// The parser for a tool name as it appears in a command. `cargo` covers `cargo test`,
/// `cargo clippy` and `cargo build`: compiler diagnostics and test results share one output.
fn parser_for(tool: &str) -> Option<Parser> {
    Some(match tool {
        "pytest" | "py.test" => pytest::parse,
        "cargo" | "rustc" | "cargo-clippy" | "clippy-driver" => rust::parse,
        "jest" => jest::parse,
        "vitest" => vitest::parse,
        "tsc" => tsc::parse,
        "eslint" => eslint::parse,
        "pyright" | "basedpyright" => pyright::parse,
        "mypy" => mypy::parse,
        "ruff" | "flake8" => pycodes::parse,
        "go" => go::parse,
        "golangci-lint" => golangci::parse,
        "mvn" | "mvnw" => maven::parse,
        "gradle" | "gradlew" => gradle::parse,
        "gcc" | "g++" | "cc" | "c++" | "clang" | "clang++" => cc::parse,
        _ => return None,
    })
}

/// Every parser, in the order their failures are merged when the tool is not known.
/// The C compilers come last: their diagnostic line is the most generic shape.
const ALL_PARSERS: &[Parser] = &[
    pytest::parse,
    rust::parse,
    jest::parse,
    vitest::parse,
    tsc::parse,
    eslint::parse,
    pyright::parse,
    mypy::parse,
    pycodes::parse,
    go::parse,
    golangci::parse,
    maven::parse,
    gradle::parse,
    cc::parse,
];

/// The tool a command runs: the one known tool name among its words
/// (`npx tsc --noEmit` is `tsc`, `python -m pytest -q` is `pytest`,
/// `.venv/bin/ruff check .` is `ruff`), otherwise its first word (`npm run lint` is
/// `npm`). A command that names several known tools gives them joined with `+`
/// (`ruff+mypy`), which is no tool's name, so the output is matched against every parser.
pub fn tool_of(command: &str) -> String {
    let words: Vec<&str> = command
        .split(|c: char| c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')'))
        .filter(|w| !w.is_empty() && !w.starts_with('-') && !is_assignment(w))
        .map(|w| w.rsplit('/').next().unwrap_or(w))
        .collect();
    let mut known: Vec<&str> = Vec::new();
    for word in &words {
        if parser_for(word).is_some() && !known.contains(word) {
            known.push(word);
        }
    }
    match known.as_slice() {
        [] => words.first().copied().unwrap_or("").to_string(),
        several => several.join("+"),
    }
}

/// `CI=1 pytest`: an environment assignment in front of a command is not the command.
fn is_assignment(word: &str) -> bool {
    word.split_once('=')
        .is_some_and(|(key, _)| !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
}

/// Parses `text` with the parser of `tool`, or, when the tool is unknown (a script
/// runner such as `npm` or `make`) or its parser finds nothing, with every parser that
/// recognises the output. Several tools behind one command are merged.
///
/// A named tool is trusted with weaker evidence than a sniffed one: a bare
/// `error: ...` line is a cargo error when the command was `cargo`, and just a log
/// line when it was `make`.
///
/// `detail` adds what only the proxy shows to the summary: the number of test
/// binaries behind a cargo total.
fn parse(tool: &str, text: &str, detail: bool) -> Parsed {
    if let Some(parser) = parser_for(tool) {
        let mut parsed = parser(text);
        if parsed.recognised || !parsed.failures.is_empty() {
            if detail {
                name_suites(&mut parsed);
            }
            return parsed;
        }
    }
    let parses: Vec<Parsed> = ALL_PARSERS
        .iter()
        .map(|parser| parser(text))
        .filter(|parsed| parsed.recognised)
        .collect();
    let owners: Vec<fn(&str) -> bool> = parses.iter().filter_map(|p| p.owns_files).collect();
    let mut merged = Parsed::default();
    let mut missing = 0;
    for mut parsed in parses {
        if detail {
            name_suites(&mut parsed);
        }
        merged.recognised = true;
        merged.partial |= parsed.partial;
        merged.closed |= parsed.closed;
        merged.trailing |= parsed.trailing;
        merged.suites += parsed.suites;
        // Each tool counts only its own failures, so what is missing is added up per tool.
        missing += parsed.missing();
        if parsed.owns_files.is_none() && !owners.is_empty() {
            let owned = |f: &Failure| owners.iter().any(|owns| owns(&f.file));
            parsed.failures.retain(|f| !owned(f));
            parsed.warnings.retain(|f| !owned(f));
        }
        merged.failures.extend(parsed.failures);
        merged.warnings.extend(parsed.warnings);
        merged.summary = match (merged.summary.take(), parsed.summary) {
            (Some(a), Some(b)) => Some(format!("{a}; {b}")),
            (a, b) => a.or(b),
        };
    }
    merged.reported = Some(merged.failures.len() + missing);
    merged
}

/// Ends a summary added up over several test binaries with how many there were.
fn name_suites(parsed: &mut Parsed) {
    if parsed.suites >= 2 {
        if let Some(summary) = parsed.summary.as_mut() {
            summary.push_str(&format!(" ({} suites)", parsed.suites));
        }
    }
}

/// How [`filter_with`] should read an output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilterOptions {
    /// Whether the command succeeded, when that is known. `None` lets the output decide:
    /// it failed if a failure was parsed or if no parser recognised it.
    pub passed: Option<bool>,
    /// Cap on the filtered text, see [`MAX_FILTERED_BYTES`].
    pub max_bytes: usize,
    /// List the warnings too, grouped by rule with every location, after the `ok` line
    /// of a passing run or after the failures of a failing one. Off for `lge check`,
    /// where warnings matter only when they are why the run failed; on for an agent
    /// that ran a linter to see them.
    pub keep_warnings: bool,
}

impl Default for FilterOptions {
    fn default() -> Self {
        FilterOptions {
            passed: None,
            max_bytes: MAX_FILTERED_BYTES,
            keep_warnings: false,
        }
    }
}

/// What [`filter_for_proxy`] returns: the failures and text of [`filter_with`], plus
/// what the proxy needs to decide whether a run may be called passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyFiltered {
    /// The failures in `text`, shortened in the same way.
    pub failures: Vec<Failure>,
    /// How many distinct warnings the tool printed, listed in `text` or not.
    pub warnings: usize,
    /// The filtered text.
    pub text: String,
    /// A parser recognised the output as its tool's.
    pub recognised: bool,
    /// The tool's own closing line is in the output.
    pub closed: bool,
    /// Something other than the tool's epilogue was printed after its closing line.
    pub trailing: bool,
    /// Failures the tool counted (or said it had) that no parser read, and 1 for a
    /// run without parsed failures that printed a line saying it failed (cargo's
    /// `error: 1 target failed:`, go's bare `FAIL`, `make: *** ... Error 1`, a script
    /// runner's failure note, `BUILD FAILED`). While this is above 0, a run without
    /// parsed failures is not a certain pass.
    pub unparsed: usize,
}

/// Reduces the raw output of `tool` to its failures and their text form.
///
/// `tool` is the first word of the command, a known tool name, or the whole command
/// (see [`tool_of`]). A passing run gives no failures and one short line. Pure: the
/// same input always gives the same output.
pub fn filter_output(tool: &str, raw: &str) -> (Vec<Failure>, String) {
    filter_with(tool, raw, &FilterOptions::default())
}

/// [`filter_output`] for a run whose exit status is known.
///
/// * Passed: no failures, one short line (`ok` or `ok: <the tool's summary>`), then
///   the warnings grouped by rule when `opts.keep_warnings` is set.
/// * Failed with parsed failures: all of them, grouped by file, capped at
///   `opts.max_bytes` by shortening excerpts and then messages; then the warnings
///   grouped by rule when `opts.keep_warnings` is set (never shortened).
/// * Failed with nothing parsed: no failures, and the generic filter's text.
pub fn filter_with(tool: &str, raw: &str, opts: &FilterOptions) -> (Vec<Failure>, String) {
    filter_paths(tool, raw, opts, &|file| file.to_string())
}

/// [`filter_with`] for the proxy: the same failures and text, plus whether a parser
/// recognised the output and where the tool's closing line is. A summary added up
/// over several cargo test binaries also says how many there were.
pub fn filter_for_proxy(tool: &str, raw: &str, opts: &FilterOptions) -> ProxyFiltered {
    filter_full(tool, raw, opts, &|file| file.to_string(), true)
}

/// [`filter_with`], with every failure's file passed through `rewrite` before the text
/// is built. The runner uses it to make paths relative to the repository root.
pub(crate) fn filter_paths(
    tool: &str,
    raw: &str,
    opts: &FilterOptions,
    rewrite: &dyn Fn(&str) -> String,
) -> (Vec<Failure>, String) {
    let filtered = filter_full(tool, raw, opts, rewrite, false);
    (filtered.failures, filtered.text)
}

fn filter_full(
    tool: &str,
    raw: &str,
    opts: &FilterOptions,
    rewrite: &dyn Fn(&str) -> String,
    detail: bool,
) -> ProxyFiltered {
    let text = strip_ansi(raw);
    let parsed = parse(&tool_of(tool), &text, detail);
    let mut out = ProxyFiltered {
        failures: Vec::new(),
        warnings: 0,
        text: String::new(),
        recognised: parsed.recognised,
        closed: parsed.closed,
        trailing: parsed.trailing,
        // A line that says the run failed counts as one failure not read, so a run
        // whose failure was cut off (`| tail`) is not taken for a pass.
        unparsed: parsed
            .missing()
            .max(usize::from(parsed.partial))
            .max(usize::from(parsed.failures.is_empty() && says_failed(&text))),
    };
    let mut warnings = parsed.warnings.clone();
    for w in &mut warnings {
        w.file = rewrite(&w.file);
    }
    sort_failures(&mut warnings);
    out.warnings = warnings.len();

    let looks_passed = parsed.failures.is_empty() && (parsed.recognised || text.trim().is_empty());
    if opts.passed.unwrap_or(looks_passed) {
        out.text = ok_line(&parsed);
        if opts.keep_warnings && !warnings.is_empty() {
            out.text.push('\n');
            out.text.push_str(&group_by_rule(&warnings));
        }
        return out;
    }

    // Counted before repeats are removed: a tool that prints a failure twice counts it twice.
    let (found, missing, partial) = (parsed.failures.len(), parsed.missing(), parsed.partial);
    let mut failures = parsed.failures;
    let mut grouped = String::new();
    if failures.is_empty() && opts.passed == Some(false) {
        // The warnings are why the run failed (`--max-warnings 0`): they are the failures.
        failures = parsed.warnings;
    } else if opts.keep_warnings {
        grouped = group_by_rule(&warnings);
    }
    if failures.is_empty() {
        out.text = generic::filter(&text, opts.max_bytes);
        return out;
    }
    for f in &mut failures {
        f.file = rewrite(&f.file);
    }
    sort_failures(&mut failures);

    let mut tail = parsed.summary.map(|s| format!("summary: {s}\n")).unwrap_or_default();
    // The tool counted more failures than were parsed: show the raw tail as well,
    // rather than let the missing ones disappear.
    if missing > 0 {
        tail.push_str(&format!(
            "parsed {found} of {} reported failures; unparsed output:\n",
            found + missing
        ));
        tail.push_str(&generic::filter(&text, opts.max_bytes / 3));
    } else if partial {
        tail.push_str("the tool reported a failure in a form that was not parsed; unparsed output:\n");
        tail.push_str(&generic::filter(&text, opts.max_bytes / 3));
    }
    // The warnings are not part of the budget: errors keep their excerpts, and the
    // grouped warnings are short already (one line per rule, then locations).
    cap_failures(&mut failures, tail.len(), opts.max_bytes);
    out.text = format_failures(&failures) + &grouped + &tail;
    out.failures = failures;
    out
}

/// The one line a passing run is reduced to.
fn ok_line(parsed: &Parsed) -> String {
    match parsed.summary.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(summary) => format!("ok: {}", clip_chars(summary.lines().next().unwrap_or(""), 160)),
        None => "ok".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Helpers shared by the parsers
// ---------------------------------------------------------------------------

pub(crate) fn number(s: &str) -> Option<u32> {
    s.parse().ok()
}

/// The first number in front of `word` in a tally such as `2 failed, 5 passed`.
pub(crate) fn count_of(tally: &str, word: &str) -> usize {
    pattern!(NUMBER_WORD, r"(\d+) ([A-Za-z]+)");
    NUMBER_WORD
        .captures_iter(tally)
        .find(|c| &c[2] == word)
        .and_then(|c| c[1].parse().ok())
        .unwrap_or(0)
}

/// Collapses runs of whitespace, so padded summary columns become one stable line.
pub(crate) fn squeeze(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

// What script runners, build tools, Codex and lge print after any tool has finished:
// never a sign that another program ran.
pattern!(
    GLOBAL_EPILOGUE,
    r"^(?:npm (?:ERR!|(?:error|warn)\b)|\s*ELIFECYCLE\b|error Command failed with exit code \d+|info Visit https://yarnpkg|make(?:\[\d+\])?: \*\*\* |lge: )"
);

// Lines that say a run failed, whoever printed them: cargo's list of failed targets,
// go's bare `FAIL`, any line starting `FAIL ` (pytest-cov's threshold, jest's file),
// `make`'s error line, the script runners' failure notes and the build tools' closing
// lines. They are allowed after a closing line (see [`GLOBAL_EPILOGUE`]), so a run
// that printed one is never called passed on its closing line alone.
pattern!(
    FAILURE_NOTE,
    r"^(?:error: \d+ targets? failed|error: (?:test|doctest) failed|error: could not compile|FAIL\s*$|FAIL\s|--- FAIL:|make(?:\[\d+\])?: \*\*\* .*Error \d+|npm (?:ERR!|error\b)|\s*ELIFECYCLE\b|error Command failed with exit code|BUILD FAILED|(?:\[INFO\] )?BUILD FAILURE|FAILURE: Build failed)"
);

/// Whether `text` has a line that says the run failed (see [`FAILURE_NOTE`]).
fn says_failed(text: &str) -> bool {
    text.lines().any(|l| FAILURE_NOTE.is_match(l))
}

/// Whether `line` is one that may follow any tool's closing line: a script runner's
/// or `make`'s note that the command failed, an `lge:` note, or a truncation notice.
pub(crate) fn is_global_epilogue(line: &str) -> bool {
    GLOBAL_EPILOGUE.is_match(line)
        || line.contains("tokens truncated")
        || line.contains("chars truncated")
        || line.contains("lines omitted")
}

/// Where a tool's closing line is: `(closed, trailing)`.
///
/// The closing line is the last line matching one of `closing`. `closed` says there is
/// one; `trailing` says a non-blank line after it matches neither `epilogue` (what the
/// tool itself prints after its closing line) nor any tool's epilogue. Text after the
/// closing line is another program's output, so the run cannot be called passed on the
/// tool's word alone.
pub(crate) fn closing_state(text: &str, closing: &[&Regex], epilogue: &[&Regex]) -> (bool, bool) {
    let lines: Vec<&str> = text.lines().collect();
    let Some(at) = lines.iter().rposition(|l| closing.iter().any(|re| re.is_match(l))) else {
        return (false, false);
    };
    let trailing = lines[at + 1..]
        .iter()
        .any(|l| !l.trim().is_empty() && !epilogue.iter().any(|re| re.is_match(l)) && !is_global_epilogue(l));
    (true, trailing)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_of_finds_the_tool_behind_wrappers() {
        for (command, tool) in [
            ("pytest", "pytest"),
            ("pytest -q tests/test_ruff.py", "pytest"),
            ("python -m pytest -x", "pytest"),
            ("uv run ruff check .", "ruff"),
            (".venv/bin/mypy .", "mypy"),
            ("npx tsc --noEmit", "tsc"),
            ("CI=1 FORCE=x node_modules/.bin/eslint src", "eslint"),
            ("cargo clippy --all-targets -- -D warnings", "cargo"),
            ("cargo test", "cargo"),
            ("npm run lint", "npm"),
            ("make check", "make"),
            ("ruff check . && mypy .", "ruff+mypy"),
            ("pytest tests/a.py && pytest tests/b.py", "pytest"),
            ("(cd web && pnpm run test)", "cd"),
            ("", ""),
            ("   ", ""),
        ] {
            assert_eq!(tool_of(command), tool, "{command}");
        }
    }

    #[test]
    fn several_known_tools_in_one_command_are_all_parsed() {
        let raw = "shop/orders.py:1:8: F401 `os` imported but unused\nFound 1 error.\n\
                   shop/orders.py:14: error: Incompatible return value type  [return-value]\n\
                   Found 1 error in 1 file (checked 3 source files)\n";
        let (failures, text) = filter_output("ruff check . ; mypy .", raw);
        let names: Vec<&str> = failures.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["F401", "return-value"]);
        // Each tool's count matches its own failures: nothing is reported as missing.
        assert!(text.ends_with("summary: Found 1 error in 1 file (checked 3 source files); Found 1 error.\n"));
        assert!(text.contains("  1: F401: `os` imported but unused\n"));
        assert!(text.contains("  14: return-value: Incompatible return value type\n"));
    }

    #[test]
    fn a_script_runner_is_sniffed() {
        let raw = "\n> web@1.0.0 typecheck\n> tsc --noEmit\n\n\
                   src/routes.ts(4,9): error TS2322: Type 'string' is not assignable to type 'number'.\n\
                   npm error Lifecycle script `typecheck` failed with error:\n";
        let (failures, _) = filter_output("npm", raw);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].name, "TS2322");
        assert_eq!(failures[0].file, "src/routes.ts");
    }

    #[test]
    fn a_known_tool_with_foreign_output_falls_back_to_the_generic_filter() {
        let raw = "sh: pytest: command not found\n";
        let (failures, text) = filter_output("pytest", raw);
        assert!(failures.is_empty());
        assert_eq!(text, raw);
    }

    #[test]
    fn exit_status_overrides_what_the_output_looks_like() {
        let raw = "shop/orders.py:1:8: F401 `os` imported but unused\nFound 1 error.\n";
        let passed = FilterOptions {
            passed: Some(true),
            ..FilterOptions::default()
        };
        assert_eq!(
            filter_with("ruff", raw, &passed),
            (Vec::new(), "ok: Found 1 error.".to_string())
        );

        // Unknown tool, exit 0: one line, whatever it printed.
        assert_eq!(
            filter_with("make", "error: this is only a log line\n", &passed),
            (Vec::new(), "ok".to_string())
        );
        // Unknown tool, unknown status: the text is kept.
        let (_, text) = filter_output("make", "error: this is only a log line\n");
        assert_eq!(text, "error: this is only a log line\n");
        // Empty output: nothing to report.
        assert_eq!(filter_output("tsc", "  \n"), (Vec::new(), "ok".to_string()));
        // Empty output of a failed command still says so.
        let failed = FilterOptions {
            passed: Some(false),
            ..FilterOptions::default()
        };
        assert_eq!(
            filter_with("tsc", "", &failed),
            (Vec::new(), "(no output)\n".to_string())
        );
    }

    #[test]
    fn a_tool_count_above_the_parsed_count_adds_the_raw_tail() {
        // Two errors reported, one line in a shape the parser does not know.
        let raw = "shop/orders.py:1:8: F401 `os` imported but unused\n\
                   shop/orders.py (line 9): something this parser does not understand\n\
                   Found 2 errors.\n";
        let (failures, text) = filter_output("ruff", raw);
        assert_eq!(failures.len(), 1);
        assert!(text.contains("parsed 1 of 2 reported failures; unparsed output:\n"));
        assert!(text.contains("shop/orders.py (line 9): something this parser does not understand\n"));
    }

    #[test]
    fn rewrite_changes_the_file_in_failures_and_text() {
        let raw = "shop/orders.py:1:8: F401 `os` imported but unused\n";
        let (failures, text) = filter_paths("ruff", raw, &FilterOptions::default(), &|f| format!("api/{f}"));
        assert_eq!(failures[0].file, "api/shop/orders.py");
        assert!(text.starts_with("api/shop/orders.py\n"));
    }

    #[test]
    fn closing_state_reads_the_last_closing_line() {
        pattern!(CLOSE, r"^Done: \d+ checks$");
        pattern!(OWN, r"^see the report");
        let text = "Done: 1 checks\nmore work\nDone: 2 checks\n\nsee the report at r.html\nnpm error code 1\n\
                    make[2]: *** [all] Error 1\nlge: kept whole\n…12 tokens truncated…\n[... 40 lines omitted ...]\n";
        assert_eq!(closing_state(text, &[&CLOSE], &[&OWN]), (true, false));
        // Not the tool's epilogue nor anyone's: another program printed after it.
        assert_eq!(
            closing_state(&format!("{text}server listening\n"), &[&CLOSE], &[&OWN]),
            (true, true)
        );
        assert_eq!(
            closing_state("Done: 2 checks\nsee the report\n", &[&CLOSE], &[]),
            (true, true)
        );
        assert_eq!(closing_state("no closing line\n", &[&CLOSE], &[]), (false, false));
        assert_eq!(closing_state("", &[&CLOSE], &[]), (false, false));
        for line in [
            "npm ERR! code ELIFECYCLE",
            "npm warn deprecated x",
            " ELIFECYCLE  Test failed.",
            "error Command failed with exit code 1.",
            "info Visit https://yarnpkg.com/en/docs/cli/run for documentation about this command.",
            "make: *** [Makefile:3: test] Error 1",
            "lge: the line above repeats 4 more times",
            "abc…120 chars truncated…xyz",
        ] {
            assert!(is_global_epilogue(line), "{line}");
        }
        assert!(!is_global_epilogue("npm install done"));
        assert!(!is_global_epilogue("make check"));
    }

    #[test]
    fn a_merge_is_closed_or_trailing_when_any_tool_is() {
        // jest then ruff behind one script: jest's closing line has ruff's output after it.
        let raw = "Tests:       1 passed, 1 total\nshop/a.py:1:8: F401 `os` imported but unused\nFound 1 error.\n";
        let got = filter_for_proxy("npm test", raw, &FilterOptions::default());
        assert!(got.recognised && got.closed && got.trailing);
    }

    #[test]
    fn helpers() {
        assert_eq!(count_of("2 failed, 5 passed, 7 total", "failed"), 2);
        assert_eq!(count_of("1 failed | 12 passed (13)", "passed"), 12);
        assert_eq!(count_of("5 passed", "failed"), 0);
        assert_eq!(squeeze("Tests:       2 failed,  5 passed"), "Tests: 2 failed, 5 passed");
        assert_eq!(number("12"), Some(12));
        assert_eq!(number("x"), None);
    }
}
