//! Terminal escape codes carry no information for the model and break every line
//! pattern, so they are removed before anything else looks at the output.

use std::iter::Peekable;
use std::str::Chars;

/// Removes escape sequences and control characters, and applies carriage returns the
/// way a terminal would: text before a lone `\r` is overwritten (progress bars), while
/// `\r\n` is a plain line end.
pub(crate) fn strip_ansi(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut line_start = 0;
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => skip_escape(&mut chars),
            '\u{9b}' => skip_csi(&mut chars),
            '\r' => {
                while chars.peek() == Some(&'\r') {
                    chars.next();
                }
                if !matches!(chars.peek(), Some('\n') | None) {
                    out.truncate(line_start);
                }
            }
            '\n' => {
                out.push('\n');
                line_start = out.len();
            }
            '\t' => out.push('\t'),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Consumes what follows an ESC.
fn skip_escape(chars: &mut Peekable<Chars>) {
    match chars.next() {
        Some('[') => skip_csi(chars),
        // OSC, DCS, SOS, PM, APC: a string ended by BEL or by ESC \. A real one never
        // spans lines, so a line end closes it too: a stray ESC in binary output must
        // not swallow every line (and every failure) after it.
        Some(']' | 'P' | 'X' | '^' | '_') => {
            while let Some(&c) = chars.peek() {
                if c == '\n' {
                    break;
                }
                chars.next();
                if c == '\u{7}' {
                    break;
                }
                if c == '\u{1b}' {
                    if chars.peek() == Some(&'\\') {
                        chars.next();
                    }
                    break;
                }
            }
        }
        // Character-set selection takes one more byte.
        Some('(' | ')' | '*' | '+' | '#' | '%') => {
            chars.next();
        }
        // Any other escape is two bytes long, and the second one is already consumed.
        _ => {}
    }
}

/// Consumes a control sequence: parameter and intermediate bytes, then one final byte.
/// Anything else ends it and stays in the text: an unfinished sequence must not take
/// the line break after it, or the start of the next line, with it.
fn skip_csi(chars: &mut Peekable<Chars>) {
    while let Some(&c) = chars.peek() {
        if ('\u{20}'..='\u{3f}').contains(&c) {
            chars.next();
            continue;
        }
        if ('\u{40}'..='\u{7e}').contains(&c) {
            chars.next();
        }
        break;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_colour_codes() {
        assert_eq!(
            strip_ansi("\u{1b}[1m\u{1b}[31mFAIL\u{1b}[0m src/a.test.ts"),
            "FAIL src/a.test.ts"
        );
        assert_eq!(strip_ansi("\u{1b}[38;5;196mred\u{1b}[39m\n"), "red\n");
    }

    #[test]
    fn removes_osc_links_and_charset_escapes() {
        assert_eq!(
            strip_ansi("\u{1b}]8;;https://x.test\u{7}link\u{1b}]8;;\u{1b}\\ ok"),
            "link ok"
        );
        assert_eq!(strip_ansi("\u{1b}(Bplain\u{1b}=\u{1b}[?25h"), "plain");
    }

    #[test]
    fn carriage_return_overwrites_the_line_but_crlf_is_a_line_end() {
        assert_eq!(strip_ansi("building 1/3\rbuilding 3/3\rdone\nnext"), "done\nnext");
        assert_eq!(strip_ansi("a\r\nb\r\r\nc\r"), "a\nb\nc");
    }

    #[test]
    fn drops_other_control_characters_and_keeps_tabs_and_unicode() {
        assert_eq!(strip_ansi("a\u{8}\u{7}\tb ✕ ●"), "a\tb ✕ ●");
    }

    #[test]
    fn an_unfinished_escape_ends_at_the_line_end() {
        // Bytes of a binary dump that happen to open a string or a control sequence.
        for opener in ["\u{1b}P", "\u{1b}]", "\u{1b}X", "\u{1b}^", "\u{1b}_"] {
            assert_eq!(
                strip_ansi(&format!("junk {opener}more junk\nFAILED tests/a.py::t\n")),
                "junk \nFAILED tests/a.py::t\n",
                "{opener:?}"
            );
        }
        assert_eq!(strip_ansi("junk \u{1b}[\nFAILED a\n"), "junk \nFAILED a\n");
        assert_eq!(strip_ansi("junk \u{1b}[12;\r\nFAILED a\n"), "junk \nFAILED a\n");
        assert_eq!(strip_ansi("a\u{9b}31mb\u{9b}\nc"), "ab\nc");
    }

    #[test]
    fn an_unfinished_escape_does_not_panic() {
        assert_eq!(strip_ansi("ok\u{1b}"), "ok");
        assert_eq!(strip_ansi("ok\u{1b}[31"), "ok");
        assert_eq!(strip_ansi("ok\u{1b}]0;title"), "ok");
    }
}
