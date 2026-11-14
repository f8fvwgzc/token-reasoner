//! The fallback for output no parser understands: keep what looks like an error, with
//! a little context, and the end of the log.

use super::pattern;
use crate::format::clip_chars;
use std::collections::BTreeSet;

/// Lines kept from the end: where a tool prints its verdict.
const TAIL_LINES: usize = 40;
/// Lines kept on each side of a line that looks like an error.
const CONTEXT_LINES: usize = 2;
/// Longest line kept, in characters: 40 tail lines of this size stay under the default cap.
const MAX_LINE_CHARS: usize = 240;

pattern!(
    ERROR_LIKE,
    r"(?i)\b(errors?|fail(ed|ure|ures|ing)?|fatal|exception|traceback|panic(ked)?|assert(ion)?(error)?|cannot|can't|could not|couldn't|unable to|not found|no such file|undefined|denied|refused|timed out|timeout|segmentation fault|killed|aborted|unexpected|invalid|truncated)\b|\berr!|[✖✕✗✘×]"
);
// "0 errors", "no failures": a count of nothing is not an error.
pattern!(NOTHING_WRONG, r"(?i)\b(0|no) (errors?|failures?|failed|problems?)\b");

fn looks_like_error(line: &str) -> bool {
    ERROR_LIKE.is_match(line) && !NOTHING_WRONG.is_match(line)
}

/// Keeps the last 40 lines and every error-looking line with two lines of context, in
/// their original order, with a marker where lines were left out. At most `max_bytes`:
/// the tail is taken first, then error lines from the top of the log, because the
/// first error is usually the cause of the ones after it.
pub(crate) fn filter(text: &str, max_bytes: usize) -> String {
    let lines: Vec<String> = text.lines().map(|l| clip_chars(l.trim_end(), MAX_LINE_CHARS)).collect();
    if lines.iter().all(|l| l.is_empty()) {
        return "(no output)\n".to_string();
    }
    let n = lines.len();
    let cost = |i: usize| lines[i].len() + 1;
    // Every range taken below can add one marker; reserving its room keeps the result under the cap.
    let marker_room = marker(n).len();

    let mut keep: BTreeSet<usize> = BTreeSet::new();
    let mut used = 0;
    let mut take = |range: std::ops::Range<usize>, keep: &mut BTreeSet<usize>| -> bool {
        let new: Vec<usize> = range.filter(|i| !keep.contains(i)).collect();
        let bytes: usize = new.iter().map(|&i| cost(i)).sum::<usize>() + marker_room;
        if used + bytes > max_bytes {
            return false;
        }
        used += bytes;
        keep.extend(new);
        true
    };

    // The tail, shrunk from the top until it fits.
    let mut start = n.saturating_sub(TAIL_LINES);
    while start < n && !take(start..n, &mut keep) {
        start += 1;
    }
    for (i, line) in lines.iter().enumerate() {
        if looks_like_error(line) && !keep.contains(&i) {
            let range = i.saturating_sub(CONTEXT_LINES)..(i + CONTEXT_LINES + 1).min(n);
            if !take(range, &mut keep) {
                break;
            }
        }
    }

    let mut out = String::new();
    let mut next = 0;
    for &i in &keep {
        if i > next {
            out.push_str(&marker(i - next));
        }
        out.push_str(&lines[i]);
        out.push('\n');
        next = i + 1;
    }
    if out.is_empty() {
        // Not even the last line fits: say so rather than return nothing.
        return clip_chars(marker(n).trim_end(), max_bytes.saturating_sub(4)) + "\n";
    }
    out
}

fn marker(omitted: usize) -> String {
    format!("[... {omitted} lines omitted ...]\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_output_is_kept_whole() {
        let text = "building\nlinking\nld: symbol not found\n";
        assert_eq!(filter(text, 12_000), text);
    }

    #[test]
    fn blank_output_says_so() {
        assert_eq!(filter("", 12_000), "(no output)\n");
        assert_eq!(filter("\n  \n", 12_000), "(no output)\n");
    }

    #[test]
    fn keeps_error_lines_with_context_and_the_last_forty_lines() {
        let mut lines: Vec<String> = (1..=200).map(|i| format!("step {i} ok")).collect();
        lines[9] = "Traceback (most recent call last):".into();
        lines[99] = "make: *** [build] Error 2".into();
        let out = filter(&lines.join("\n"), 12_000);
        let kept: Vec<&str> = out.lines().collect();

        assert_eq!(kept[0], "[... 7 lines omitted ...]");
        assert_eq!(
            &kept[1..6],
            [
                "step 8 ok",
                "step 9 ok",
                "Traceback (most recent call last):",
                "step 11 ok",
                "step 12 ok"
            ]
        );
        assert_eq!(kept[6], "[... 85 lines omitted ...]");
        assert_eq!(
            &kept[7..12],
            [
                "step 98 ok",
                "step 99 ok",
                "make: *** [build] Error 2",
                "step 101 ok",
                "step 102 ok"
            ]
        );
        assert_eq!(kept[12], "[... 58 lines omitted ...]");
        assert_eq!(kept[13], "step 161 ok");
        assert_eq!(kept.len(), 13 + 40);
        assert_eq!(kept[kept.len() - 1], "step 200 ok");
    }

    #[test]
    fn a_count_of_zero_is_not_an_error() {
        assert!(!looks_like_error("0 errors, 0 warnings"));
        assert!(!looks_like_error("Finished with no failures"));
        assert!(looks_like_error("3 errors, 0 warnings"));
        assert!(looks_like_error("FAILED tests/test_a.py"));
        assert!(looks_like_error("✖ 2 problems"));
        assert!(looks_like_error("sh: ruff: command not found"));
        assert!(looks_like_error("npm ERR! code ELIFECYCLE"));
        assert!(!looks_like_error("compiling terror.c"));
    }

    #[test]
    fn stays_under_the_cap_and_prefers_the_tail_then_the_first_errors() {
        let lines: Vec<String> = (1..=5_000)
            .map(|i| format!("error: something went wrong in step {i} of the build, see the log"))
            .collect();
        let text = lines.join("\n");
        for max in [200, 1_000, 4_000, 12_000] {
            let out = filter(&text, max);
            assert!(out.len() <= max, "max {max}: {} bytes", out.len());
            assert!(out.trim_end().ends_with("step 5000 of the build, see the log"));
        }
        let out = filter(&text, 12_000);
        assert!(out.starts_with("error: something went wrong in step 1 of"));
        assert!(out.contains("lines omitted"));
    }

    #[test]
    fn long_lines_are_clipped_and_a_tiny_cap_still_answers() {
        let text = format!("{}\nerror: {}\n", "x".repeat(10), "y".repeat(5_000));
        let out = filter(&text, 12_000);
        assert!(out.len() < 300);
        assert!(out.ends_with("...\n"));
        let tiny = filter(&text, 10);
        assert!(tiny.len() <= 10 && tiny.ends_with('\n'));
    }
}
