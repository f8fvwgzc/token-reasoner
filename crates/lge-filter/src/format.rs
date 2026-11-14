//! The text form of a list of failures, and the size cap.
//!
//! Failures are grouped by file and sorted, so the same failures always give the same
//! bytes whatever order the tool printed them in (test runners with workers do not
//! print in a stable order).

use crate::Failure;
use std::cmp::Ordering;

/// Longest message kept from a tool, in characters.
pub(crate) const MAX_MESSAGE_CHARS: usize = 300;
/// Longest excerpt line kept from a tool, in characters.
pub(crate) const MAX_LINE_CHARS: usize = 200;

const NO_FILE: &str = "(no file)";
const CUT: &str = "...";

/// Builds a failure from what a parser found: the message becomes one clipped line,
/// excerpt lines lose trailing blanks and their common indentation, and an excerpt
/// that opens by repeating the message does not say it twice.
pub(crate) fn failure(file: &str, line: Option<u32>, name: &str, message: &str, excerpt: &[String]) -> Failure {
    let message = message.lines().next().unwrap_or("").trim();
    let mut excerpt = dedent(excerpt);
    if excerpt.first().is_some_and(|first| first.trim() == message) {
        excerpt.remove(0);
        excerpt = dedent(&excerpt);
    }
    Failure {
        file: file.trim().to_string(),
        line,
        name: name.trim().to_string(),
        message: clip_chars(message, MAX_MESSAGE_CHARS),
        excerpt: excerpt.join("\n"),
    }
}

/// Cuts `s` to at most `max` characters, marking the cut.
pub(crate) fn clip_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        None => s.to_string(),
        Some((end, _)) => format!("{}{CUT}", &s[..end]),
    }
}

/// Cuts `s` to at most `max` bytes on a character boundary, marking the cut. Too small
/// a budget gives an empty string instead of a lone marker.
pub(crate) fn clip_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    if max <= CUT.len() {
        return String::new();
    }
    let mut end = max - CUT.len();
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{CUT}", s[..end].trim_end())
}

/// Keeps the first `head` and last `tail` lines: a traceback starts in the test and
/// ends at the error, and both ends matter more than the middle.
pub(crate) fn clip_lines(lines: Vec<String>, head: usize, tail: usize) -> Vec<String> {
    if lines.len() <= head + tail + 1 {
        return lines;
    }
    let mut out: Vec<String> = lines[..head].to_vec();
    out.push(CUT.to_string());
    out.extend_from_slice(&lines[lines.len() - tail..]);
    out
}

/// Drops trailing blanks, blank lines at both ends and the indentation all lines share.
fn dedent(lines: &[String]) -> Vec<String> {
    let mut lines: Vec<String> = lines.iter().map(|l| clip_chars(l.trim_end(), MAX_LINE_CHARS)).collect();
    while lines.first().is_some_and(|l| l.is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    let indent = lines
        .iter()
        .filter(|l| !l.is_empty())
        .map(|l| l.len() - l.trim_start_matches(' ').len())
        .min()
        .unwrap_or(0);
    lines
        .into_iter()
        .map(|l| if l.len() >= indent { l[indent..].to_string() } else { l })
        .collect()
}

/// Sorts by file, line and name and removes exact repeats (a tool may report the same
/// diagnostic once per build target, or repeat failures in a final summary).
pub(crate) fn sort_failures(failures: &mut Vec<Failure>) {
    failures.sort_by(order);
    failures.dedup();
}

fn order(a: &Failure, b: &Failure) -> Ordering {
    // Failures without a file or a line go last within their group.
    (
        a.file.is_empty(),
        &a.file,
        a.line.is_none(),
        a.line,
        &a.name,
        &a.message,
        &a.excerpt,
    )
        .cmp(&(
            b.file.is_empty(),
            &b.file,
            b.line.is_none(),
            b.line,
            &b.name,
            &b.message,
            &b.excerpt,
        ))
}

/// One heading per file, then one line per failure, then its excerpt:
///
/// ```text
/// tests/test_cart.py
///   14: tests/test_cart.py::test_total_rounds: assert 1.0 == 1.01
///       >       assert cart.total() == 1.01
/// ```
///
/// `failures` must already be sorted with [`sort_failures`].
pub(crate) fn format_failures(failures: &[Failure]) -> String {
    let mut out = String::new();
    let mut current: Option<&str> = None;
    for f in failures {
        if current != Some(f.file.as_str()) {
            out.push_str(if f.file.is_empty() { NO_FILE } else { &f.file });
            out.push('\n');
            current = Some(&f.file);
        }
        out.push_str("  ");
        if let Some(line) = f.line {
            out.push_str(&line.to_string());
            out.push_str(": ");
        }
        out.push_str(&f.name);
        if !f.message.is_empty() {
            out.push_str(": ");
            out.push_str(&f.message);
        }
        out.push('\n');
        for line in f.excerpt.lines() {
            out.push_str("      ");
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Shrinks `failures` until their text plus `extra` bytes fits in `max_bytes`:
/// excerpts first, then messages. Names, files and lines are never touched, so the
/// result can still be over the cap when the names alone do not fit.
pub(crate) fn cap_failures(failures: &mut [Failure], extra: usize, max_bytes: usize) {
    let fits = |fs: &[Failure]| format_failures(fs).len() + extra <= max_bytes;
    if fits(failures) {
        return;
    }
    let longest = failures.iter().map(|f| f.excerpt.len()).max().unwrap_or(0);
    let limit = largest_fitting(longest, |l| fits(&shrunk(failures, l, usize::MAX)));
    shrink(failures, limit, usize::MAX);
    if fits(failures) {
        return;
    }
    let longest = failures.iter().map(|f| f.message.len()).max().unwrap_or(0);
    let limit = largest_fitting(longest, |l| fits(&shrunk(failures, 0, l)));
    shrink(failures, 0, limit);
}

/// Largest value in `0..=max` for which `fits` holds, assuming smaller values fit
/// whenever larger ones do. Zero when nothing fits.
fn largest_fitting(max: usize, fits: impl Fn(usize) -> bool) -> usize {
    let (mut low, mut high) = (0, max);
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        if fits(mid) {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    low
}

fn shrunk(failures: &[Failure], excerpt_bytes: usize, message_bytes: usize) -> Vec<Failure> {
    let mut copy = failures.to_vec();
    shrink(&mut copy, excerpt_bytes, message_bytes);
    copy
}

fn shrink(failures: &mut [Failure], excerpt_bytes: usize, message_bytes: usize) {
    for f in failures {
        f.excerpt = shorten_excerpt(&f.excerpt, excerpt_bytes);
        f.message = clip_bytes(&f.message, message_bytes);
    }
}

/// Cuts an excerpt to at most `max` bytes, keeping whole lines from both ends.
fn shorten_excerpt(excerpt: &str, max: usize) -> String {
    if excerpt.len() <= max {
        return excerpt.to_string();
    }
    let lines: Vec<&str> = excerpt.lines().collect();
    // Room for the marker line and the two line breaks around it.
    let Some(budget) = max.checked_sub(CUT.len() + 2) else {
        return String::new();
    };
    let (mut head, mut tail) = (0, lines.len());
    let mut used = 0;
    // Lines are taken from both ends in turn. The end of a traceback names the error,
    // so the tail gets the first pick. Two refusals in a row mean neither end fits.
    let (mut from_tail, mut refused) = (true, 0);
    while head < tail && refused < 2 {
        let next = if from_tail { tail - 1 } else { head };
        let cost = lines[next].len() + 1;
        if used + cost <= budget {
            used += cost;
            refused = 0;
            if from_tail {
                tail -= 1;
            } else {
                head += 1;
            }
        } else {
            refused += 1;
        }
        from_tail = !from_tail;
    }
    if head == 0 && tail == lines.len() {
        return String::new();
    }
    let mut out: Vec<&str> = lines[..head].to_vec();
    out.push(CUT);
    out.extend_from_slice(&lines[tail..]);
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(file: &str, line: Option<u32>, name: &str, message: &str, excerpt: &str) -> Failure {
        Failure {
            file: file.into(),
            line,
            name: name.into(),
            message: message.into(),
            excerpt: excerpt.into(),
        }
    }

    #[test]
    fn groups_by_file_and_sorts_by_line() {
        let mut fs = vec![
            f("b.py", Some(9), "E501", "line too long", ""),
            f("", None, "error", "no location", ""),
            f("a.py", None, "a.py::test_z", "boom", "E   boom\na.py:3: Error"),
            f("a.py", Some(3), "F401", "unused", ""),
            f("b.py", Some(9), "E501", "line too long", ""),
        ];
        sort_failures(&mut fs);
        assert_eq!(
            format_failures(&fs),
            "a.py\n  3: F401: unused\n  a.py::test_z: boom\n      E   boom\n      a.py:3: Error\n\
             b.py\n  9: E501: line too long\n(no file)\n  error: no location\n"
        );
    }

    #[test]
    fn failure_cleans_message_and_excerpt() {
        let got = failure(
            " a.py ",
            Some(1),
            " t ",
            "first\nsecond",
            &["".into(), "    x = 1   ".into(), "      y".into(), "".into()],
        );
        assert_eq!(got, f("a.py", Some(1), "t", "first", "x = 1\n  y"));
        let repeated = failure(
            "",
            None,
            "t",
            "boom",
            &["    boom".into(), "".into(), "      at f (a.js:1:2)".into()],
        );
        assert_eq!(repeated.excerpt, "at f (a.js:1:2)");
        let long = "é".repeat(MAX_MESSAGE_CHARS + 5);
        assert_eq!(
            failure("", None, "t", &long, &[]).message.chars().count(),
            MAX_MESSAGE_CHARS + 3
        );
    }

    #[test]
    fn clip_lines_keeps_both_ends() {
        let lines: Vec<String> = (0..10).map(|i| i.to_string()).collect();
        assert_eq!(clip_lines(lines.clone(), 2, 2), ["0", "1", "...", "8", "9"]);
        assert_eq!(clip_lines(lines.clone(), 5, 4), lines);
    }

    #[test]
    fn clip_bytes_respects_char_boundaries() {
        assert_eq!(clip_bytes("héllo wörld", 8), "héll...");
        assert_eq!(clip_bytes("héllo", 3), "");
        assert_eq!(clip_bytes("short", 99), "short");
    }

    #[test]
    fn shorten_excerpt_fits_and_keeps_the_end() {
        let excerpt = (0..40)
            .map(|i| format!("line {i} of the traceback"))
            .collect::<Vec<_>>()
            .join("\n");
        for max in [0, 4, 5, 30, 100, 400, excerpt.len() - 1] {
            let short = shorten_excerpt(&excerpt, max);
            assert!(short.len() <= max, "max {max}: got {} bytes", short.len());
            if max >= 100 {
                assert!(short.ends_with("line 39 of the traceback"));
                assert!(short.starts_with("line 0 of the traceback"));
                assert!(short.contains("\n...\n"));
            }
        }
        assert_eq!(shorten_excerpt(&excerpt, excerpt.len()), excerpt);
    }

    #[test]
    fn cap_shortens_excerpts_before_messages_and_never_names() {
        let make = |n: usize| -> Vec<Failure> {
            (0..n)
                .map(|i| {
                    f(
                        "tests/test_big.py",
                        Some(i as u32 + 1),
                        &format!("tests/test_big.py::test_case_{i}"),
                        &"assertion message ".repeat(8),
                        &"E   some traceback line\n".repeat(30),
                    )
                })
                .collect()
        };

        // Excerpts alone are enough here: messages stay whole.
        let mut some = make(20);
        cap_failures(&mut some, 0, 12_000);
        let text = format_failures(&some);
        assert!(text.len() <= 12_000);
        assert!(text.len() > 9_000, "the budget should be used, got {}", text.len());
        assert!(some.iter().all(|x| x.message.len() == 144 && !x.excerpt.is_empty()));

        // Too many for any excerpt: messages shrink, every name is still there.
        let mut many = make(150);
        cap_failures(&mut many, 0, 12_000);
        let text = format_failures(&many);
        assert!(text.len() <= 12_000);
        assert!(many.iter().all(|x| x.excerpt.is_empty()));
        for i in 0..150 {
            assert!(
                text.contains(&format!("tests/test_big.py::test_case_{i}:")),
                "lost test {i}"
            );
        }

        // Names alone are over the cap: nothing is dropped, the cap gives way.
        let mut huge = make(2_000);
        cap_failures(&mut huge, 0, 12_000);
        let text = format_failures(&huge);
        assert!(text.len() > 12_000);
        assert!(huge.iter().all(|x| x.excerpt.is_empty() && x.message.is_empty()));
        assert_eq!(text.lines().count(), 2_001);
    }

    #[test]
    fn cap_counts_the_extra_bytes() {
        let mut fs = vec![f("a.py", Some(1), "t", "m", &"x".repeat(50))];
        let full = format_failures(&fs).len();
        cap_failures(&mut fs, 10, full + 9);
        assert!(format_failures(&fs).len() + 10 <= full + 9);
    }
}
