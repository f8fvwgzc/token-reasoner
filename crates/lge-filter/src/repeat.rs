//! Runs of identical lines (a retry loop, a log line printed once per item) say
//! nothing after the first copy except how many there were.

use std::borrow::Cow;

/// Shortest line, once trimmed, that a run is collapsed for: shorter lines (`}`,
/// `---`) are structure, and the note would be longer than what it replaces.
const MIN_CHARS: usize = 8;
/// How many identical lines in a row make a run.
const MIN_RUN: usize = 3;
/// Lines lge writes itself start with this, and are never collapsed.
const NOTE: &str = "lge:";

/// Replaces each run of 3 or more identical lines with its first line and the note
/// `lge: the line above repeats N more times`. Blank lines, lines shorter than 8
/// characters once trimmed and lines starting with `lge:` are never collapsed, so a
/// second pass changes nothing. Returns the input itself when nothing was collapsed.
pub fn collapse_repeats(text: &str) -> Cow<'_, str> {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out: Vec<Cow<str>> = Vec::with_capacity(lines.len());
    let mut changed = false;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let run = lines[i..].iter().take_while(|l| **l == line).count();
        if run >= MIN_RUN && collapsible(line) {
            out.push(Cow::Borrowed(line));
            out.push(Cow::Owned(format!(
                "{NOTE} the line above repeats {} more times",
                run - 1
            )));
            changed = true;
        } else {
            out.extend(lines[i..i + run].iter().map(|l| Cow::Borrowed(*l)));
        }
        i += run;
    }
    if changed {
        Cow::Owned(out.join("\n"))
    } else {
        Cow::Borrowed(text)
    }
}

fn collapsible(line: &str) -> bool {
    line.trim().chars().count() >= MIN_CHARS && !line.starts_with(NOTE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_of_three_or_more_becomes_the_line_and_a_count() {
        let text = "start\nretrying connection...\nretrying connection...\nretrying connection...\nretrying connection...\nend\n";
        assert_eq!(
            collapse_repeats(text),
            "start\nretrying connection...\nlge: the line above repeats 3 more times\nend\n"
        );
        let at_end = "x\nwarning: deprecated\nwarning: deprecated\nwarning: deprecated";
        assert_eq!(
            collapse_repeats(at_end),
            "x\nwarning: deprecated\nlge: the line above repeats 2 more times"
        );
    }

    #[test]
    fn short_blank_and_lge_lines_and_pairs_stay() {
        for text in [
            "}\n}\n}\n}\n",
            "\n\n\n\n\n",
            "       \n       \n       \n",
            "lge: a note here\nlge: a note here\nlge: a note here\n",
            "same line twice\nsame line twice\nother\n",
            "1234567\n1234567\n1234567\n",
            "",
        ] {
            assert!(matches!(collapse_repeats(text), Cow::Borrowed(_)), "{text:?}");
        }
        // Eight characters is enough; indentation does not count towards it.
        assert_eq!(
            collapse_repeats("  12345678\n  12345678\n  12345678"),
            "  12345678\nlge: the line above repeats 2 more times"
        );
    }

    #[test]
    fn a_second_pass_changes_nothing() {
        let text = "a line that repeats\n".repeat(50)
            + "\n\n\n"
            + &"another repeated one\n".repeat(3)
            + "a line that repeats\n"
            + &"lge: the line above repeats 2 more times\n".repeat(3);
        let once = collapse_repeats(&text).into_owned();
        assert_eq!(once.matches("lge: the line above repeats 49 more times").count(), 1);
        assert_eq!(once.matches("lge: the line above repeats 2 more times").count(), 4);
        assert!(matches!(collapse_repeats(&once), Cow::Borrowed(_)));
    }

    #[test]
    fn unicode_lines_are_counted_in_characters() {
        let text = "✓ готово ✓\n".repeat(3);
        assert_eq!(
            collapse_repeats(&text),
            "✓ готово ✓\nlge: the line above repeats 2 more times\n"
        );
    }
}
