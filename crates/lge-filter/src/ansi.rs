//! Terminal escape codes carry no information for the model and break every line
//! pattern, so they are removed before anything else looks at the output.

use std::borrow::Cow;
use std::iter::Peekable;
use std::str::Chars;

/// Removes escape sequences and control characters, and applies carriage returns the
/// way a terminal would: text before a lone `\r` is overwritten (progress bars), while
/// `\r\n` is a plain line end.
///
/// Removed: control sequences (`ESC [` or CSI, parameters, one final byte in
/// 0x40-0x7E), strings (`ESC ]`, `ESC P`, `ESC X`, `ESC ^`, `ESC _`, up to BEL, `ESC \`
/// or the line end), character-set selections and every other two-byte escape. An
/// escape is never allowed to take a line break or a tab with it.
pub fn strip_ansi(raw: &str) -> String {
    strip(raw, true)
}

/// Cleans what a terminal would not show, without losing anything it would:
///
/// * escape sequences and control characters go (see [`strip_ansi`]);
/// * on each line, a `\r` before the line end is dropped, and a line redrawn with
///   `\r` (a progress bar) keeps its last non-blank state;
/// * trailing spaces and tabs go.
///
/// Returns the input itself when there was nothing to clean; a second pass never
/// changes anything.
pub fn normalize_terminal(text: &str) -> Cow<'_, str> {
    let stripped = strip(text, false);
    let mut out = String::with_capacity(stripped.len());
    for (i, line) in stripped.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let line = line.trim_end_matches('\r');
        let line = if line.contains('\r') {
            line.split('\r').rev().find(|s| !s.trim().is_empty()).unwrap_or("")
        } else {
            line
        };
        out.push_str(line.trim_end_matches([' ', '\t']));
    }
    if out == text {
        Cow::Borrowed(text)
    } else {
        Cow::Owned(out)
    }
}

/// The escape and control-character pass. With `apply_cr`, carriage returns overwrite
/// the line as on a terminal; without it they are kept for the caller.
fn strip(raw: &str, apply_cr: bool) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut line_start = 0;
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => skip_escape(&mut chars),
            '\u{9b}' => skip_csi(&mut chars),
            '\r' if apply_cr => {
                while chars.peek() == Some(&'\r') {
                    chars.next();
                }
                if !matches!(chars.peek(), Some('\n') | None) {
                    out.truncate(line_start);
                }
            }
            '\r' => out.push('\r'),
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

/// Consumes what follows an ESC. A control character right after it (a line break, a
/// tab, another ESC) is not part of the escape: the ESC was a stray byte.
fn skip_escape(chars: &mut Peekable<Chars>) {
    let Some(&next) = chars.peek() else {
        return;
    };
    if next.is_control() {
        return;
    }
    chars.next();
    match next {
        '[' => skip_csi(chars),
        // OSC, DCS, SOS, PM, APC: a string ended by BEL or by ESC \. A real one never
        // spans lines, so a line end closes it too: a stray ESC in binary output must
        // not swallow every line (and every failure) after it.
        ']' | 'P' | 'X' | '^' | '_' => {
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
        // Character-set selection takes one more byte, unless a line break or another
        // control character comes first.
        '(' | ')' | '*' | '+' | '#' | '%' if chars.peek().is_some_and(|c| !c.is_control()) => {
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

    #[test]
    fn a_stray_escape_keeps_the_line_break_after_it() {
        assert_eq!(strip_ansi("FAILED a\u{1b}\nFAILED b\n"), "FAILED a\nFAILED b\n");
        assert_eq!(strip_ansi("a\u{1b}(\nb"), "a\nb");
        assert_eq!(strip_ansi("a\u{1b}\tb"), "a\tb");
        assert_eq!(strip_ansi("a\u{1b}\u{1b}[31mb"), "ab");
        // ESC plus one byte: `ESC M` (reverse index), `ESC 7`, `ESC c`.
        assert_eq!(strip_ansi("x\u{1b}My\u{1b}7z\u{1b}c!"), "xyz!");
    }

    #[test]
    fn normalize_keeps_the_last_state_of_a_redrawn_line() {
        // pip
        let pip = "Collecting requests\n   ---------------------------------------- 0.0/64.9 kB ? eta -:--:--\r   ---------------------------------------- 64.9/64.9 kB 2.1 MB/s eta 0:00:00\nInstalling collected packages: requests\n";
        assert_eq!(
            normalize_terminal(pip),
            "Collecting requests\n   ---------------------------------------- 64.9/64.9 kB 2.1 MB/s eta 0:00:00\nInstalling collected packages: requests\n"
        );
        // npm: a spinner redrawn with CR and cleared with `ESC [K`.
        let npm = "\u{1b}[?25l\r\u{1b}[K\u{2838} reify:lodash\r\u{1b}[K\u{2834} reify:react\r\u{1b}[K\nadded 3 packages in 2s\n";
        assert_eq!(
            normalize_terminal(npm),
            "\u{2834} reify:react\nadded 3 packages in 2s\n"
        );
        // docker pull: layer lines redrawn in place.
        let docker = "a1b2c3: Pulling fs layer \r\u{1b}[1Ba1b2c3: Downloading  1.2MB/3.4MB\r\u{1b}[1Ba1b2c3: Pull complete \r\nDigest: sha256:abc\n";
        assert_eq!(
            normalize_terminal(docker),
            "a1b2c3: Pull complete\nDigest: sha256:abc\n"
        );
        // cargo: the download bar, then CRLF line ends.
        let cargo = "    Updating crates.io index\r\n Downloading crates ...\r  Downloaded serde v1.0.0\r\n   Compiling serde v1.0.0   \r\n";
        assert_eq!(
            normalize_terminal(cargo),
            "    Updating crates.io index\n  Downloaded serde v1.0.0\n   Compiling serde v1.0.0\n"
        );
    }

    #[test]
    fn normalize_borrows_when_clean_and_is_idempotent() {
        let clean = "src/a.rs:1: fine\n\tindented\nno newline at the end";
        assert!(matches!(normalize_terminal(clean), Cow::Borrowed(_)));
        assert!(matches!(normalize_terminal(""), Cow::Borrowed(_)));
        for dirty in [
            "\u{1b}[31mred\u{1b}[0m  \t\n",
            "a\rb\r\n\u{1b}]8;;https://x.test\u{7}link\u{1b}]8;;\u{1b}\\\n",
            "progress 10%\rprogress 100%\r\u{1b}[K",
            "ok\u{1b}",
            "  \r  \r\n\u{0}é\u{8}\n",
        ] {
            let once = normalize_terminal(dirty).into_owned();
            assert_ne!(once, dirty);
            assert!(
                matches!(normalize_terminal(&once), Cow::Borrowed(_)),
                "{dirty:?} -> {once:?}"
            );
        }
        assert_eq!(normalize_terminal("x \n y\t\n"), "x\n y\n");
        assert_eq!(normalize_terminal("a\r\r\nb"), "a\nb");
        assert_eq!(normalize_terminal("   \r"), "");
    }

    #[test]
    fn normalize_cuts_only_on_char_boundaries() {
        let text = "日本語\u{1b}[1m✕ fehler\u{1b}[0m ü\r→ done ✓\n";
        assert_eq!(normalize_terminal(text), "→ done ✓\n");
        assert_eq!(strip_ansi("é\u{1b}é"), "é");
    }
}
