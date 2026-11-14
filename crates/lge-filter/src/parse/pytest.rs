//! pytest.
//!
//! The "short test summary info" block is the authoritative list of what failed: one
//! `FAILED <node id> - <message>` or `ERROR <node id>` line per test. The `FAILURES`
//! and `ERRORS` sections above it hold one traceback block per test, which gives the
//! line and the excerpt. Either can be missing (`-rN`, `--tb=no`, a run cut short), so
//! each is used alone when the other is absent.

use super::{count_of, number, pattern, Parsed};
use crate::format::{clip_lines, failure};
use crate::Failure;

pattern!(SECTION, r"^={2,} (.+?) ={2,}$");
pattern!(BLOCK, r"^_{2,} (.+?) _{2,}$");
pattern!(ENTRY, r"^(FAILED|ERROR) (.+)$");
pattern!(
    TALLY,
    r"^(?:\d+ [a-z]+(?:, )?)+ in [0-9.]+s(?: \([0-9:]+\))?$|^no tests ran in [0-9.]+s(?: \([0-9:]+\))?$"
);
pattern!(TIMING, r" in [0-9.]+s(?: \([0-9:]+\))?$");
// `tests/test_cart.py:14: AssertionError`, `tests/test_cart.py:14: in test_x`, `/abs/test_x.py:40`
pattern!(LOCATION, r"^([^\s:][^:]*):(\d+)(?::(?: (.*))?)?$");
pattern!(NATIVE_FRAME, r#"^\s*File "(.+)", line (\d+)"#);
pattern!(CAPTURED, r"^-{2,} (?:Captured|generated) .* -{2,}$");
pattern!(VERBOSE, r"^(\S+::\S.*?) (?:FAILED|ERROR)(?:\s+\[\s*\d+%\])?$");
pattern!(USAGE, r"^ERROR: (.+)$");
// `rootdir: /home/dev/shop` (older versions add `, configfile: ...` on the same line)
pattern!(ROOTDIR, r"^rootdir: (.+?)(?:, .*)?$");
// `<shop.cart.Cart object at 0x7f3c1a2b4d10>`: the address is different on every run.
pattern!(ADDRESS, r" at 0x[0-9a-fA-F]+>");
pattern!(
    EXCEPTION_LINE,
    r"^(?:assert\b|[\w.]*(?:Error|Exception|Warning|Exit|Interrupt|Failed)\b)"
);

const INTERNAL: &str = "INTERNALERROR>";

#[derive(PartialEq)]
enum Section {
    Other,
    Tracebacks,
    Short,
}

struct Block<'a> {
    title: &'a str,
    lines: Vec<&'a str>,
    used: bool,
}

/// What one traceback block says about its failure.
#[derive(Default)]
struct Digest {
    line: Option<u32>,
    first_path: Option<String>,
    error: Option<String>,
    excerpt: Vec<String>,
}

pub(crate) fn parse(text: &str) -> Parsed {
    let lines: Vec<&str> = text.lines().collect();
    let mut section = Section::Other;
    let mut session = false;
    let mut rootdir: Option<&str> = None;
    let mut tally: Option<&str> = None;
    let mut blocks: Vec<Block> = Vec::new();
    let mut entries: Vec<(&str, &str, Option<&str>)> = Vec::new();
    let mut verbose: Vec<&str> = Vec::new();
    let mut internal: Vec<&str> = Vec::new();
    let mut failures: Vec<Failure> = Vec::new();

    for (i, line) in lines.iter().copied().enumerate() {
        if let Some(c) = SECTION.captures(line) {
            let title = c.get(1).map_or("", |m| m.as_str());
            if TALLY.is_match(title) {
                tally = Some(title);
                section = Section::Other;
                continue;
            }
            session |= title.starts_with("test session starts");
            section = match title {
                "FAILURES" | "ERRORS" => Section::Tracebacks,
                t if t.starts_with("short test summary") => Section::Short,
                _ => Section::Other,
            };
            continue;
        }
        // With `-q` the closing tally has no `=` rule around it.
        if TALLY.is_match(line) {
            tally = Some(line);
            section = Section::Other;
            continue;
        }
        if let Some(rest) = line.strip_prefix(INTERNAL) {
            internal.push(rest.strip_prefix(' ').unwrap_or(rest));
            continue;
        }
        match section {
            Section::Tracebacks => {
                if let Some(c) = BLOCK.captures(line) {
                    blocks.push(Block {
                        title: c.get(1).map_or("", |m| m.as_str()),
                        lines: Vec::new(),
                        used: false,
                    });
                } else if let Some(block) = blocks.last_mut() {
                    block.lines.push(line);
                }
            }
            Section::Short => {
                if let Some(c) = ENTRY.captures(line) {
                    let titles: Vec<&str> = blocks.iter().map(|b| b.title).collect();
                    let (node_id, message) = split_entry(c.get(2).map_or("", |m| m.as_str()), &titles);
                    let entry = (c.get(1).map_or("", |m| m.as_str()), node_id, message);
                    if !entries.iter().any(|e| e.0 == entry.0 && e.1 == entry.1) {
                        entries.push(entry);
                    }
                }
            }
            Section::Other => {
                if let Some(c) = VERBOSE.captures(line) {
                    verbose.push(c.get(1).map_or("", |m| m.as_str()));
                } else if let Some(c) = ROOTDIR.captures(line) {
                    rootdir = c.get(1).map(|m| m.as_str());
                } else if let Some(c) = USAGE.captures(line) {
                    // A bad command line or a missing path: the lines below say which.
                    let detail: Vec<String> = lines[i + 1..]
                        .iter()
                        .take_while(|l| !l.trim().is_empty())
                        .take(5)
                        .map(|l| l.to_string())
                        .collect();
                    failures.push(failure("", None, "pytest", &c[1], &detail));
                }
            }
        }
    }

    // 1. The summary lines, each completed from its traceback block.
    for (kind, node_id, message) in &entries {
        let (file, rest) = match node_id.split_once("::") {
            Some((file, rest)) => (file, Some(rest)),
            None => (*node_id, None),
        };
        let digest = pick(&mut blocks, kind, file, rest)
            .map(|block| digest(&block.lines, file))
            .unwrap_or_default();
        // The summary cuts messages at the terminal width; the traceback has them whole.
        let message = match (message, &digest.error) {
            (Some(m), _) if !m.is_empty() && !m.ends_with("...") => (*m).to_string(),
            (_, Some(e)) => e.clone(),
            (m, None) => m.unwrap_or("").to_string(),
        };
        let message = ADDRESS.replace_all(&message, ">");
        failures.push(failure(file, digest.line, node_id, &message, &digest.excerpt));
    }

    // 2. Blocks no summary line claimed.
    for block in blocks.iter().filter(|b| !b.used) {
        let collecting = block.title.strip_prefix("ERROR collecting ");
        let first = digest(&block.lines, "");
        // Some locations are absolute; node ids are relative to the root directory.
        let located = first.first_path.as_deref().map(|p| strip_root(p, rootdir).to_string());
        let file = collecting.map(str::to_string).or(located).unwrap_or_default();
        let name = match collecting {
            Some(path) => path.to_string(),
            None if file.is_empty() => block.title.to_string(),
            None => format!("{file}::{}", node_tail(block.title)),
        };
        let digest = digest(&block.lines, &file);
        failures.push(failure(
            &file,
            digest.line,
            &name,
            digest.error.as_deref().unwrap_or(""),
            &digest.excerpt,
        ));
    }

    // 3. No summary and no tracebacks (`--tb=no -rN -v`): the progress lines.
    if failures.is_empty() {
        for node_id in &verbose {
            let file = node_id.split_once("::").map_or("", |(file, _)| file);
            failures.push(failure(file, None, node_id, "", &[]));
        }
    }

    // A crash inside pytest or a plugin: there is no test to blame.
    if let Some(last) = internal.iter().rev().find(|l| !l.trim().is_empty()) {
        let trace: Vec<String> = internal.iter().map(|l| l.to_string()).collect();
        failures.push(failure("", None, "INTERNALERROR", last, &clip_lines(trace, 0, 8)));
    }

    let reported = tally.map(|t| count_of(t, "failed") + count_of(t, "error") + count_of(t, "errors"));
    Parsed {
        recognised: session || tally.is_some() || !blocks.is_empty() || !entries.is_empty() || !failures.is_empty(),
        partial: false,
        summary: tally.map(|t| TIMING.replace(t, "").into_owned()),
        reported,
        failures,
        warnings: Vec::new(),
    }
}

/// Splits `<node id> - <message>` of a summary line. A parameter id may hold the
/// separator itself (`test_x[a - b] - assert 1 == 2`), so with parameters the node id
/// runs to a `]`: the one that gives the title of a traceback block when there is such
/// a block, the first one otherwise.
fn split_entry<'a>(rest: &'a str, titles: &[&str]) -> (&'a str, Option<&'a str>) {
    const SEPARATOR: &str = " - ";
    let Some(first) = rest.find(SEPARATOR) else {
        return (rest, None);
    };
    let at = |i: usize| (&rest[..i], Some(&rest[i + SEPARATOR.len()..]));
    if !rest[..first].contains('[') {
        return at(first);
    }
    let after_bracket: Vec<usize> = rest
        .match_indices(SEPARATOR)
        .map(|(i, _)| i)
        .filter(|&i| rest[..i].ends_with(']'))
        .collect();
    let has_block = |node_id: &str| {
        node_id.split_once("::").is_some_and(|(_, tail)| {
            let title = block_title(tail);
            titles.iter().any(|t| {
                *t == title
                    || t.strip_suffix(title.as_str())
                        .is_some_and(|head| head.ends_with(" of "))
            })
        })
    };
    if let Some(&i) = after_bracket.iter().find(|&&i| has_block(&rest[..i])) {
        return at(i);
    }
    if rest.ends_with(']') && (after_bracket.is_empty() || has_block(rest)) {
        return (rest, None);
    }
    at(after_bracket.first().copied().unwrap_or(first))
}

/// Finds the traceback block of a summary line by the title pytest gives it: the node
/// id without its file, with `.` between class and function. Two files can have a test
/// of the same name, so a block that mentions the file wins.
fn pick<'b, 'a>(blocks: &'b mut [Block<'a>], kind: &str, file: &str, rest: Option<&str>) -> Option<&'b mut Block<'a>> {
    let titles: Vec<String> = match (kind, rest) {
        ("ERROR", None) => vec![format!("ERROR collecting {file}")],
        ("ERROR", Some(rest)) => {
            let title = block_title(rest);
            vec![
                format!("ERROR at setup of {title}"),
                format!("ERROR at teardown of {title}"),
                title,
            ]
        }
        (_, Some(rest)) => vec![block_title(rest)],
        (_, None) => vec![file.to_string()],
    };
    let matches = |b: &Block| !b.used && titles.iter().any(|t| t == b.title);
    let index = blocks
        .iter()
        .position(|b| matches(b) && b.lines.iter().any(|l| l.contains(file)))
        .or_else(|| blocks.iter().position(matches))?;
    blocks[index].used = true;
    Some(&mut blocks[index])
}

/// `TestCheckout::test_rejects[a::b]` -> `TestCheckout.test_rejects[a::b]`.
fn block_title(rest: &str) -> String {
    let (head, params) = split_params(rest);
    format!("{}{params}", head.replace("::", "."))
}

/// `TestCheckout.test_rejects[1.5]` -> `TestCheckout::test_rejects[1.5]`; the setup and
/// teardown prefixes are not part of the node id.
fn node_tail(title: &str) -> String {
    let title = title
        .strip_prefix("ERROR at setup of ")
        .or_else(|| title.strip_prefix("ERROR at teardown of "))
        .unwrap_or(title);
    let (head, params) = split_params(title);
    format!("{}{params}", head.replace('.', "::"))
}

/// Splits a test name from its parameter id, which may itself contain `.` or `::`.
fn split_params(name: &str) -> (&str, &str) {
    match name.find('[') {
        Some(i) => name.split_at(i),
        None => (name, ""),
    }
}

/// Frames in the standard library or in installed packages: on the way to the error,
/// not where the change is.
fn is_library(path: &str) -> bool {
    path.starts_with('<')
        || ["site-packages/", "dist-packages/", "/lib/python", "lib/python3"]
            .iter()
            .any(|part| path.contains(part))
}

fn strip_root<'a>(path: &'a str, rootdir: Option<&str>) -> &'a str {
    rootdir
        .and_then(|root| path.strip_prefix(root))
        .and_then(|rest| rest.strip_prefix('/'))
        .unwrap_or(path)
}

fn same_file(a: &str, b: &str) -> bool {
    !a.is_empty() && !b.is_empty() && (a == b || a.ends_with(&format!("/{b}")) || b.ends_with(&format!("/{a}")))
}

/// Keeps the lines of a traceback that carry the failure: the failing statements
/// (`>`), the error (`E`), and where each frame is. Source context, local variables
/// and captured output are dropped.
fn digest(lines: &[&str], file: &str) -> Digest {
    let mut kept: Vec<String> = Vec::new();
    let mut locations: Vec<(String, u32)> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    // `--tb=short` and `--tb=native` print the statement under its location; the value
    // says whether that statement is worth keeping.
    let mut statement: Option<bool> = None;
    // Where the lines of the frame being read start in `kept`.
    let mut frame_start = 0;
    let body: Vec<&str> = lines.iter().copied().take_while(|l| !CAPTURED.is_match(l)).collect();

    for line in &body {
        if let Some(keep) = statement.take() {
            if line.starts_with(' ') {
                if keep {
                    kept.push(line.to_string());
                }
                continue;
            }
        }
        let line = &ADDRESS.replace_all(line, ">");
        let frame = LOCATION
            .captures(line)
            .map(|c| (c, false))
            .or_else(|| NATIVE_FRAME.captures(line).map(|c| (c, true)));
        if *line == "E" || line.starts_with("E ") {
            let text = line[1..].trim();
            // A syntax error names its place inside the error text.
            if let Some(c) = NATIVE_FRAME.captures(text) {
                locations.extend(number(&c[2]).map(|n| (c[1].to_string(), n)));
            }
            errors.push(text.to_string());
            kept.push(line.to_string());
        } else if line.starts_with('>') {
            kept.push(line.to_string());
        } else if let Some((c, native)) = frame {
            let statement_follows = native || c.get(3).is_some_and(|m| m.as_str().starts_with("in "));
            if is_library(&c[1]) {
                // A frame inside a dependency: its statements (`>` above the location in
                // the long format) go, the error lines stay.
                let mut at = 0;
                kept.retain(|l| {
                    at += 1;
                    at <= frame_start || !l.starts_with('>')
                });
                statement = statement_follows.then_some(false);
            } else {
                locations.extend(number(&c[2]).map(|n| (c[1].to_string(), n)));
                kept.push(line.to_string());
                statement = statement_follows.then_some(true);
            }
            frame_start = kept.len();
        }
    }
    if kept.is_empty() {
        kept = body
            .iter()
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.to_string())
            .collect();
    }
    Digest {
        // The deepest frame inside the test's own file is where the test failed.
        line: locations
            .iter()
            .rev()
            .find(|(p, _)| same_file(p, file))
            .map(|(_, n)| *n),
        first_path: locations.first().map(|(p, _)| p.clone()),
        error: errors
            .iter()
            .find(|e| EXCEPTION_LINE.is_match(e))
            .or(errors.first())
            .cloned(),
        excerpt: clip_lines(kept, 3, 9),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter_output;

    const PASS: &str = include_str!("../../testdata/pytest_pass.txt");
    const FAIL_LONG: &str = include_str!("../../testdata/pytest_fail_long.txt");
    const FAIL_SHORT: &str = include_str!("../../testdata/pytest_fail_short.txt");
    const COLLECT: &str = include_str!("../../testdata/pytest_collect_error.txt");
    const USAGE_ERROR: &str = include_str!("../../testdata/pytest_usage_error.txt");
    const INTERNAL_ERROR: &str = include_str!("../../testdata/pytest_internal_error.txt");

    fn by_name<'a>(failures: &'a [Failure], name: &str) -> &'a Failure {
        failures
            .iter()
            .find(|f| f.name == name)
            .unwrap_or_else(|| panic!("no failure named {name}"))
    }

    #[test]
    fn passing_run_is_one_line() {
        let (failures, text) = filter_output("pytest", PASS);
        assert!(failures.is_empty());
        assert_eq!(text, "ok: 12 passed");
    }

    #[test]
    fn long_tracebacks_name_every_failure_and_error() {
        let parsed = parse(FAIL_LONG);
        assert_eq!(parsed.reported, Some(5));
        assert_eq!(parsed.summary.as_deref(), Some("4 failed, 3 passed, 1 error"));
        assert_eq!(parsed.failures.len(), 5);

        let plain = by_name(&parsed.failures, "tests/test_cart.py::test_total_rounds");
        assert_eq!(plain.file, "tests/test_cart.py");
        assert_eq!(plain.line, Some(14));
        assert_eq!(plain.message, "assert 1.0 == 1.01");
        assert!(plain
            .excerpt
            .starts_with(">       assert cart.total() == 1.01\nE       assert 1.0 == 1.01\n"));
        assert!(plain.excerpt.ends_with("tests/test_cart.py:14: AssertionError"));
        assert!(!plain.excerpt.contains("cart = Cart()"), "source context is dropped");
        assert!(plain
            .excerpt
            .contains("where 1.0 = <bound method Cart.total of <shop.cart.Cart object>>()"));
        assert!(
            !plain.excerpt.contains("0x7f3c"),
            "object addresses change on every run"
        );

        let method = by_name(
            &parsed.failures,
            "tests/test_orders.py::TestCheckout::test_rejects_empty_cart",
        );
        assert_eq!(method.line, Some(21));
        assert_eq!(method.message, "assert 201 == 400");
        assert!(
            !method.excerpt.contains("created order 17"),
            "captured output is dropped"
        );

        // Two parameter sets of one test: two failures, each with its own traceback.
        let ten = by_name(&parsed.failures, "tests/test_orders.py::test_discount[10-90.0]");
        let fifty = by_name(&parsed.failures, "tests/test_orders.py::test_discount[50-50.0]");
        assert_eq!((ten.line, fifty.line), (Some(35), Some(35)));
        assert_eq!(
            (ten.message.as_str(), fifty.message.as_str()),
            ("KeyError: 10", "KeyError: 50")
        );
        assert!(ten.excerpt.contains("E       KeyError: 10") && ten.excerpt.contains("shop/pricing.py:8: KeyError"));
        assert!(fifty.excerpt.contains("E       KeyError: 50"));

        let setup = by_name(&parsed.failures, "tests/test_orders.py::test_list_orders");
        assert_eq!(setup.line, Some(52));
        assert_eq!(setup.message, "fixture 'db' not found");
    }

    #[test]
    fn short_tracebacks_and_quiet_tally() {
        let parsed = parse(FAIL_SHORT);
        assert_eq!(parsed.reported, Some(4));
        assert_eq!(parsed.summary.as_deref(), Some("4 failed, 4 passed"));
        assert_eq!(parsed.failures.len(), 4);
        let ten = by_name(&parsed.failures, "tests/test_orders.py::test_discount[10-90.0]");
        assert_eq!(ten.line, Some(35));
        assert_eq!(
            ten.excerpt,
            "tests/test_orders.py:35: in test_discount\n    assert apply_discount(100.0, percent) == expected\n\
             shop/pricing.py:8: in apply_discount\n    return price * (1 - percent / RATES[percent])\nE   KeyError: 10"
        );
        let method = by_name(
            &parsed.failures,
            "tests/test_orders.py::TestCheckout::test_rejects_empty_cart",
        );
        assert_eq!(method.line, Some(21));
        assert!(!method.excerpt.contains("created order"));
    }

    #[test]
    fn collection_errors_are_failures() {
        let (failures, text) = filter_output("pytest", COLLECT);
        assert_eq!(failures.len(), 2);
        let import = by_name(&failures, "tests/test_orders.py");
        assert_eq!(import.file, "tests/test_orders.py");
        assert_eq!(import.line, Some(3));
        assert!(import
            .message
            .starts_with("ImportError: cannot import name 'create_order'"));
        // No message in the summary line: it comes from the traceback, and so does the line.
        let syntax = by_name(&failures, "tests/test_cart.py");
        assert_eq!(syntax.line, Some(9));
        assert_eq!(syntax.message, "SyntaxError: '(' was never closed");
        // Ten frames inside pytest and importlib are dropped; the error itself stays.
        assert_eq!(
            syntax.excerpt,
            "E     File \"/home/dev/shop/tests/test_cart.py\", line 9\nE       def test_total_rounds(\nE                            ^\nE   SyntaxError: '(' was never closed"
        );
        assert_eq!(
            import.excerpt,
            "tests/test_orders.py:3: in <module>\n    from shop.orders import create_order\nE   ImportError: cannot import name 'create_order' from 'shop.orders' (/home/dev/shop/shop/orders.py)"
        );
        assert!(text.ends_with("summary: 2 errors\n"));
    }

    #[test]
    fn tracebacks_without_a_summary_block_still_name_the_tests() {
        let cut = FAIL_LONG
            .split("=========================== short test summary info")
            .next()
            .unwrap();
        let parsed = parse(cut);
        let mut names: Vec<&str> = parsed.failures.iter().map(|f| f.name.as_str()).collect();
        names.sort();
        assert_eq!(
            names,
            [
                "tests/test_cart.py::test_total_rounds",
                "tests/test_orders.py::TestCheckout::test_rejects_empty_cart",
                "tests/test_orders.py::test_discount[10-90.0]",
                "tests/test_orders.py::test_discount[50-50.0]",
                "tests/test_orders.py::test_list_orders",
            ]
        );
        assert_eq!(
            by_name(&parsed.failures, "tests/test_cart.py::test_total_rounds").line,
            Some(14)
        );
    }

    #[test]
    fn verbose_progress_lines_are_the_last_resort() {
        let raw = "============================= test session starts ==============================\n\
                   collected 3 items\n\n\
                   tests/test_cart.py::test_starts_empty PASSED                             [ 33%]\n\
                   tests/test_cart.py::test_total_rounds FAILED                             [ 66%]\n\
                   tests/test_cart.py::TestCart::test_add[a b] FAILED                       [100%]\n\n\
                   ========================= 2 failed, 1 passed in 0.02s ==========================\n";
        let parsed = parse(raw);
        let names: Vec<&str> = parsed.failures.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "tests/test_cart.py::test_total_rounds",
                "tests/test_cart.py::TestCart::test_add[a b]"
            ]
        );
        assert_eq!(parsed.failures[0].file, "tests/test_cart.py");
    }

    #[test]
    fn same_test_name_in_two_files_keeps_its_own_traceback() {
        let raw = "=================================== FAILURES ===================================\n\
                   __________________________________ test_total __________________________________\n\
                   tests/test_a.py:5: in test_total\n    assert total() == 1\nE   assert 2 == 1\n\
                   __________________________________ test_total __________________________________\n\
                   tests/test_b.py:9: in test_total\n    assert total() == 3\nE   assert 2 == 3\n\
                   =========================== short test summary info ============================\n\
                   FAILED tests/test_b.py::test_total - assert 2 == 3\n\
                   FAILED tests/test_a.py::test_total - assert 2 == 1\n\
                   2 failed in 0.01s\n";
        let parsed = parse(raw);
        assert_eq!(by_name(&parsed.failures, "tests/test_a.py::test_total").line, Some(5));
        assert_eq!(by_name(&parsed.failures, "tests/test_b.py::test_total").line, Some(9));
    }

    #[test]
    fn frames_inside_dependencies_are_dropped() {
        let raw = "=================================== FAILURES ===================================\n\
                   ________________________________ test_get_order ________________________________\n\n\
                   client = <FlaskClient <Flask 'shop'>>\n\n    def test_get_order(client):\n\
                   >       resp = client.get(\"/orders/1\")\n\ntests/test_orders.py:12: \n\
                   _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ \n\
                   .venv/lib/python3.11/site-packages/werkzeug/test.py:1160: in get\n    return self.open(*args, **kw)\n\
                   .venv/lib/python3.11/site-packages/flask/app.py:882: in full_dispatch_request\n    rv = self.dispatch_request()\n\
                   shop/routes.py:21: in get_order\n    return jsonify(repo.load(order_id))\n\
                   _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ \n\n\
                   order_id = 1\n\n    def load(order_id):\n>       return ORDERS[order_id]\nE       KeyError: 1\n\n\
                   shop/repo.py:9: KeyError\n\
                   _______________________________ test_fetch_rates _______________________________\n\n\
                   >       rates = fetch()\n\ntests/test_rates.py:7: \n\
                   _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ _ \n\n\
                   >       raise ConnectionError(e, request=request)\n\
                   E       requests.exceptions.ConnectionError: connection refused\n\n\
                   /usr/lib/python3/dist-packages/requests/adapters.py:519: ConnectionError\n\
                   =========================== short test summary info ============================\n\
                   FAILED tests/test_orders.py::test_get_order - KeyError: 1\n\
                   FAILED tests/test_rates.py::test_fetch_rates - requests.exceptions.ConnectionError: connection refused\n\
                   ============================== 2 failed in 0.21s ===============================\n";
        let parsed = parse(raw);
        let order = by_name(&parsed.failures, "tests/test_orders.py::test_get_order");
        assert_eq!(order.line, Some(12));
        assert_eq!(
            order.excerpt,
            ">       resp = client.get(\"/orders/1\")\ntests/test_orders.py:12:\n\
             shop/routes.py:21: in get_order\n    return jsonify(repo.load(order_id))\n\
             >       return ORDERS[order_id]\nE       KeyError: 1\nshop/repo.py:9: KeyError"
        );
        // The error was raised inside a dependency: its statement goes, the error stays.
        let rates = by_name(&parsed.failures, "tests/test_rates.py::test_fetch_rates");
        assert_eq!(rates.line, Some(7));
        assert_eq!(
            rates.excerpt,
            ">       rates = fetch()\ntests/test_rates.py:7:\nE       requests.exceptions.ConnectionError: connection refused"
        );
    }

    #[test]
    fn usage_error_keeps_the_explanation() {
        let (failures, text) = filter_output("pytest", USAGE_ERROR);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].name, "pytest");
        assert!(text.contains("pytest: error: unrecognized arguments: --cov=shop"));
    }

    #[test]
    fn internal_error_is_reported_even_though_the_tally_looks_calm() {
        let (failures, text) = filter_output("pytest", INTERNAL_ERROR);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].name, "INTERNALERROR");
        assert_eq!(
            failures[0].message,
            "AttributeError: 'Function' object has no attribute 'priority'"
        );
        assert!(text.contains("File \"/home/dev/shop/conftest.py\", line 12, in pytest_collection_modifyitems"));
    }

    #[test]
    fn parameter_ids_with_the_separator_keep_their_node_id() {
        let raw = "=================================== FAILURES ===================================\r\n\
                   ______________________________ test_x[a - b] _______________________________\r\n\
                   tests/test_a.py:5: in test_x\r\n    assert 1 == 2\r\nE   assert 1 == 2\r\n\
                   ____________________ TestA.test_w[http://x/y?a=1 - 2] _____________________\r\n\
                   tests/my dir/test_b.py:12: in test_w\r\n    assert 0\r\nE   assert 0\r\n\
                   =========================== short test summary info ============================\r\n\
                   FAILED tests/test_a.py::test_x[a - b] - assert 1 == 2\r\n\
                   FAILED tests/my dir/test_b.py::TestA::test_w[http://x/y?a=1 - 2] - assert 0\r\n\
                   FAILED tests/test_a.py::test_y[x y] - AssertionError: boom - bang [1]\r\n\
                   FAILED tests/test_a.py::test_z[a - b]\r\n\
                   FAILED tests/test_a.py::test_v[[ - x] - boom\r\n\
                   FAILED tests/test_a.py::test_plain - assert [1] - [2] == 0\r\n\
                   ERROR tests/test_c.py - ImportError: no module [x] - y\r\n\
                   ======================== 6 failed, 1 error in 0.01s =========================\r\n";
        let (failures, text) = filter_output("pytest", raw);
        let mut got: Vec<(&str, Option<u32>, &str)> = failures
            .iter()
            .map(|f| (f.name.as_str(), f.line, f.message.as_str()))
            .collect();
        got.sort();
        assert_eq!(
            got,
            [
                (
                    "tests/my dir/test_b.py::TestA::test_w[http://x/y?a=1 - 2]",
                    Some(12),
                    "assert 0"
                ),
                ("tests/test_a.py::test_plain", None, "assert [1] - [2] == 0"),
                ("tests/test_a.py::test_v[[ - x]", None, "boom"),
                ("tests/test_a.py::test_x[a - b]", Some(5), "assert 1 == 2"),
                ("tests/test_a.py::test_y[x y]", None, "AssertionError: boom - bang [1]"),
                ("tests/test_a.py::test_z[a - b]", None, ""),
                ("tests/test_c.py", None, "ImportError: no module [x] - y"),
            ]
        );
        assert_eq!(
            by_name(&failures, "tests/test_a.py::test_x[a - b]").file,
            "tests/test_a.py"
        );
        assert!(!text.contains("unparsed output"), "{text}");
    }

    #[test]
    fn split_entry_prefers_the_id_a_traceback_block_has() {
        assert_eq!(
            split_entry("a.py::t[x] - y] - boom", &[]),
            ("a.py::t[x]", Some("y] - boom"))
        );
        assert_eq!(
            split_entry("a.py::t[x] - y] - boom", &["t[x] - y]"]),
            ("a.py::t[x] - y]", Some("boom"))
        );
        assert_eq!(
            split_entry("a.py::C::t[x] - y]", &["ERROR at setup of C.t[x] - y]"]),
            ("a.py::C::t[x] - y]", None)
        );
        assert_eq!(split_entry("a.py::t[x] - y]", &[]), ("a.py::t[x]", Some("y]")));
        assert_eq!(split_entry("a.py::t", &[]), ("a.py::t", None));
        assert_eq!(
            split_entry("dir[1]/a.py::t - boom", &[]),
            ("dir[1]/a.py::t", Some("boom"))
        );
    }

    #[test]
    fn titles_and_node_ids_convert_both_ways() {
        assert_eq!(
            block_title("TestA::TestB::test_x[a::b-1.5]"),
            "TestA.TestB.test_x[a::b-1.5]"
        );
        assert_eq!(node_tail("TestA.test_x[1.5]"), "TestA::test_x[1.5]");
        assert_eq!(node_tail("ERROR at setup of test_x"), "test_x");
        assert!(same_file("/home/dev/shop/tests/test_a.py", "tests/test_a.py"));
        assert!(!same_file("tests/other_test_a.py", "test_a.py"));
        assert_eq!(
            strip_root("/home/dev/shop/tests/a.py", Some("/home/dev/shop")),
            "tests/a.py"
        );
        assert_eq!(
            strip_root("/home/dev/shopping/a.py", Some("/home/dev/shop")),
            "/home/dev/shopping/a.py"
        );
        assert_eq!(strip_root("tests/a.py", None), "tests/a.py");
    }
}
