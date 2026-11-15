//! Reading source text around a tag: receivers, qualifiers, signatures, doc
//! lines and line numbers. Everything here works on the bytes of the file, so it
//! is the same for every language; what differs is passed in.

use super::grammars::CallSyntax;

/// Receivers that stand for the object a method runs on: PHP's `static::`
/// and Perl's `$self` among them, and the module being defined for Elixir's
/// `__MODULE__.f()` and Erlang's `?MODULE:f()`.
const SELF_WORDS: [&str; 11] = [
    "self",
    "this",
    "cls",
    "Self",
    "@",
    "$this",
    ".self",
    "static",
    "$self",
    "__MODULE__",
    "?MODULE",
];

/// Longest receiver text kept, in chars.
const RECEIVER_CAP: usize = 80;

/// Line numbers of byte offsets.
pub(crate) struct Lines {
    starts: Vec<usize>,
}

impl Lines {
    pub fn new(src: &[u8]) -> Lines {
        let mut starts = vec![0];
        starts.extend(src.iter().enumerate().filter(|(_, b)| **b == b'\n').map(|(i, _)| i + 1));
        Lines { starts }
    }

    /// 1-based line of a byte offset.
    pub fn line(&self, byte: usize) -> i64 {
        self.starts.partition_point(|&s| s <= byte) as i64
    }

    /// Number of lines, counting a last line without a newline.
    pub fn count(&self, src: &[u8]) -> i64 {
        let n = self.starts.len() as i64;
        if src.last() == Some(&b'\n') {
            n - 1
        } else {
            n
        }
    }

    /// Byte offset where the line holding `byte` starts.
    pub fn start_of(&self, byte: usize) -> usize {
        self.starts[(self.line(byte) - 1) as usize]
    }
}

/// The text of a name, trimmed.
pub(crate) fn name_text(src: &[u8], range: &std::ops::Range<usize>) -> String {
    String::from_utf8_lossy(&src[range.clone()]).trim().to_string()
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'$' | b'@' | b'\'') || b >= 0x80
}

/// True when a name has at least one letter, digit or underscore: operators
/// tagged as calls (`+` in OCaml) are not names lge keeps.
pub(crate) fn is_name(name: &str) -> bool {
    name.chars().any(|c| c.is_alphanumeric() || c == '_')
}

/// Start of the expression that ends at `end`: identifiers chained with `.`,
/// `::` or `->`, and balanced `(...)` / `[...]` groups (`Repo().save`,
/// `items[0].save`, `[math]::Floor`).
fn expression_start(src: &[u8], end: usize) -> usize {
    // A receiver longer than this is cut anyway; the bound keeps a minified
    // line from being walked once per call on it.
    let floor = end.saturating_sub(4 * RECEIVER_CAP);
    let mut i = end;
    while i > floor {
        let b = src[i - 1];
        if is_ident(b) || b == b'.' {
            i -= 1;
        } else if i >= 2 && matches!(&src[i - 2..i], b"::" | b"->") {
            i -= 2;
        } else if (b == b'?' || b == b'!') && src.get(i) == Some(&b'.') {
            i -= 1;
        } else if b == b')' || b == b']' {
            match group_start(src, i) {
                Some(open) => i = open,
                None => break,
            }
        } else if b == b'>' {
            // Type arguments of a call: `listOf<Int>(1)`, `Pick<string>(x)`.
            match angle_start(src, i) {
                Some(open) if open > 0 && (is_ident(src[open - 1]) || src[..open].ends_with(b"::")) => i = open,
                _ => break,
            }
        } else {
            break;
        }
    }
    i
}

/// Start of the type argument list that closes at `end - 1`: a balanced
/// `<...>` of names, commas and spaces on one line.
fn angle_start(src: &[u8], end: usize) -> Option<usize> {
    let floor = end.saturating_sub(200);
    let mut depth = 0usize;
    let mut i = end;
    while i > floor {
        i -= 1;
        match src[i] {
            b'>' => depth += 1,
            b'<' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(i);
                }
            }
            b if is_ident(b) || matches!(b, b' ' | b',' | b'.' | b':' | b'[' | b']' | b'?' | b'*' | b'&') => {}
            _ => return None,
        }
    }
    None
}

/// Start of the bracket group that closes at `end - 1`, when it is balanced
/// within a short look-back.
fn group_start(src: &[u8], end: usize) -> Option<usize> {
    let floor = end.saturating_sub(400);
    let mut depth = 0usize;
    let mut i = end;
    while i > floor {
        i -= 1;
        match src[i] {
            b')' | b']' => depth += 1,
            b'(' | b'[' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(i);
                }
            }
            b'\n' if depth == 0 => return None,
            _ => {}
        }
    }
    None
}

fn trim_ascii_end(src: &[u8], mut end: usize) -> usize {
    while end > 0 && src[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    end
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// "self" for a receiver that stands for the instance, the text otherwise,
/// cut at [`RECEIVER_CAP`] chars.
fn receiver_text(text: &str, self_var: Option<&str>) -> String {
    if SELF_WORDS.contains(&text) || self_var == Some(text) {
        "self".to_string()
    } else {
        text.chars().take(RECEIVER_CAP).collect()
    }
}

/// The receiver of a call whose name starts at byte `at`.
///
/// When the name follows a member operator (`.`, `->`, `::`, and `:` or `$`
/// where the language calls through them) the call is made on the expression
/// before the operator; otherwise it is a bare call and the receiver is "".
/// `self_var` is a variable that stands for the instance in the enclosing
/// method (a Go receiver), reported as "self" like `self` and `this`.
pub(crate) fn receiver_before(src: &[u8], at: usize, syntax: CallSyntax, self_var: Option<&str>) -> String {
    let before = &src[..at.min(src.len())];
    let op = if before.ends_with(b"::") || before.ends_with(b"->") {
        2
    } else if before.ends_with(b"..") {
        // A range (`0..count()`) or a concatenation (`a..f()`), not a member.
        0
    } else if before.ends_with(b".")
        || (syntax.colon && before.ends_with(b":"))
        || (syntax.dollar && before.ends_with(b"$"))
        || (syntax.hash && before.ends_with(b"#"))
        || (syntax.slash && before.ends_with(b"/"))
    {
        1
    } else {
        0
    };
    if op == 0 {
        return if syntax.message {
            message_receiver(before, self_var)
        } else {
            String::new()
        };
    }
    let mut end = before.len() - op;
    // `x?.f()`, `x!.f()` and Ruby's `x&.f()` call `f` on `x`, as PHP's `x?->f()` does.
    if op == 1 && end > 0 && before.ends_with(b".") && matches!(before[end - 1], b'?' | b'!' | b'&') {
        end -= 1;
    }
    if op == 2 && end > 0 && before.ends_with(b"->") && before[end - 1] == b'?' {
        end -= 1;
    }
    // A chained call on the next line: `items\n    .map(...)`.
    let end = trim_ascii_end(before, end);
    let mut start = expression_start(before, end);
    // Erlang's `?MODULE:f()`: the macro sign is part of the receiver.
    if syntax.colon && start > 0 && before[start - 1] == b'?' {
        start -= 1;
    }
    let text = lossy(&before[start..end]);
    let text = text.trim_start_matches(['.', ':']);
    if !text.is_empty() {
        return receiver_text(text, self_var);
    }
    // Not an expression this reads (a string literal, a block): keep what is
    // written, which never names a class, so it is never taken for one.
    let run_start = before[..end]
        .iter()
        .rposition(|b| b.is_ascii_whitespace())
        .map_or(0, |i| i + 1)
        .max(end.saturating_sub(RECEIVER_CAP));
    let run = lossy(&before[run_start..end]);
    if run.is_empty() {
        lossy(&before[end..before.len()])
    } else {
        receiver_text(&run, None)
    }
}

/// What a cascade starts with: `sb` in `sb..write(a)..write(b)`, for a call
/// whose name starts at `name_start` in the cascade that starts at `start`.
pub(crate) fn cascade_target(src: &[u8], start: usize, name_start: usize, self_var: Option<&str>) -> String {
    let span = &src[start..name_start.max(start)];
    let end = span.windows(2).position(|w| w == b"..").unwrap_or(span.len());
    receiver_text(lossy(&span[..end]).trim(), self_var)
}

/// Objective-C's `[receiver message]`: the name follows the receiver and a space.
fn message_receiver(before: &[u8], self_var: Option<&str>) -> String {
    let end = trim_ascii_end(before, before.len());
    if end == before.len() {
        return String::new();
    }
    let start = expression_start(before, end);
    let open = trim_ascii_end(before, start);
    if start == end || open == 0 || before[open - 1] != b'[' {
        return String::new();
    }
    receiver_text(&lossy(&before[start..end]), self_var)
}

/// The names written right before a definition's name and joined to it by
/// `.`, `:` or `::`, outermost first: `Cart` in `function Cart:total()` and
/// `int Cart::total()`, `M` in `function M.helper()`. Only text from
/// `floor` on is read (the start of the definition or of its line).
pub(crate) fn qualifier_before(src: &[u8], floor: usize, name_start: usize) -> Vec<String> {
    let mut parts = Vec::new();
    let mut i = name_start;
    while parts.len() < 8 {
        let op = if src[floor..i].ends_with(b"::") {
            2
        } else if src[floor..i].ends_with(b".") || src[floor..i].ends_with(b":") {
            1
        } else {
            0
        };
        if op == 0 {
            break;
        }
        let mut end = i - op;
        // A template's arguments: `Stack<T>::push`.
        if op == 2 && end > floor && src[end - 1] == b'>' {
            match angle_start(src, end) {
                Some(open) if open > floor => end = open,
                _ => break,
            }
        }
        let mut start = end;
        while start > floor
            && (src[start - 1].is_ascii_alphanumeric() || src[start - 1] == b'_' || src[start - 1] >= 0x80)
        {
            start -= 1;
        }
        if start == end {
            break;
        }
        parts.push(lossy(&src[start..end]));
        i = start;
    }
    parts.reverse();
    parts
}

/// True for `var.name` with no `(` after the name: a field read on a
/// variable, which starts lower case (a module alias starts upper case).
pub(crate) fn reads_field(src: &[u8], receiver: &str, name_end: usize) -> bool {
    let on_variable = receiver.chars().next().is_some_and(|c| c.is_lowercase() || c == '_');
    let next = src[name_end.min(src.len())..]
        .iter()
        .find(|b| !matches!(b, b' ' | b'\t'));
    on_variable && next != Some(&b'(')
}

/// The string a definition is named by, written between `from` and the `(` of
/// its parameters (a Spock feature `def "totals the items"()`), without quotes,
/// with the offset it starts at. `None` when there is none.
pub(crate) fn string_name(src: &[u8], from: usize, end: usize) -> Option<(usize, String)> {
    let rest = &src[from..end.max(from)];
    let written = &rest[..rest.iter().position(|&b| b == b'(')?];
    let start = from + written.iter().position(|b| !b.is_ascii_whitespace())?;
    let name = lossy(written).trim().trim_matches(['"', '\'']).to_string();
    (!name.is_empty()).then_some((start, name))
}

/// The name of the call a lambda is passed to (`apply` in `sb.apply { }`,
/// `buildList` in `buildList(n) { }`); `lambda` when it is passed to none.
pub(crate) fn lambda_label(src: &[u8], lambda_start: usize) -> String {
    let mut end = trim_ascii_end(src, lambda_start);
    if end > 0 && src[end - 1] == b')' {
        end = group_start(src, end).map_or(end, |open| trim_ascii_end(src, open));
    }
    let mut start = end;
    while start > 0 && (src[start - 1].is_ascii_alphanumeric() || src[start - 1] == b'_' || src[start - 1] >= 0x80) {
        start -= 1;
    }
    if start == end {
        "lambda".to_string()
    } else {
        lossy(&src[start..end])
    }
}

/// True for a header that is C++ rather than C: a line that starts a class,
/// a namespace, a template or an access section.
pub(crate) fn looks_like_cpp(src: &[u8]) -> bool {
    String::from_utf8_lossy(src).lines().any(|line| {
        let t = line.trim_start();
        [
            "class ",
            "namespace ",
            "template",
            "public:",
            "private:",
            "protected:",
            "using namespace ",
        ]
        .iter()
        .any(|p| t.starts_with(p))
    })
}

/// True for a Rust item marked as a test by an attribute above it: `#[test]`,
/// `#[tokio::test]`, `#[rstest]`, `#[test_case(..)]`, or a `#[cfg(test)]` module.
/// `index` is the 0-based line the item starts on.
pub(crate) fn has_test_attribute(lines: &[&str], index: usize) -> bool {
    for line in lines[..index.min(lines.len())].iter().rev() {
        let t = line.trim();
        if t.starts_with("///") || t.starts_with("//!") {
            continue;
        }
        let Some(attribute) = t.strip_prefix("#[").and_then(|a| a.strip_suffix(']')) else {
            return false;
        };
        let path = attribute.split(['(', ' ']).next().unwrap_or(attribute);
        if attribute == "cfg(test)"
            || path == "test"
            || path.ends_with("::test")
            || path == "rstest"
            || path == "test_case"
        {
            return true;
        }
    }
    false
}

/// True for a name that marks a test in a language whose tests are named:
/// `test_total` and `testTotal` for a function, `TestCart` for a type, and
/// EUnit's `total_test`. A word that only starts with "test" (`testimonials`,
/// `Testimonial`) is none. Rust, Go, C, C++, Zig and Objective-C mark tests
/// otherwise (an attribute, a `_test` file, a macro), so a name says nothing there.
pub(crate) fn is_test_name(lang: &str, name: &str, is_type: bool) -> bool {
    use lge_core::lang;
    let upper_after = |prefix: &str| {
        name.strip_prefix(prefix)
            .and_then(|rest| rest.chars().next())
            .is_some_and(|c| c.is_uppercase() || c.is_ascii_digit() || c == '_')
    };
    match lang {
        lang::RUST | lang::GO | lang::C | lang::CPP | lang::CUDA | lang::ZIG | lang::OBJC => false,
        lang::ERLANG => name.ends_with("_test") || name.ends_with("_test_"),
        _ if is_type => name == "Test" || upper_after("Test"),
        _ => name == "test" || upper_after("test"),
    }
}

/// The source with a `;` written at the end of each line that ends a
/// statement, for a grammar that needs one where the language lets a newline
/// end it (Groovy: `def name = 'x'` above a method). A line ends a statement
/// when it ends in a name, a literal or a closing bracket, is no annotation or
/// control header, and the next line does not go on with the expression (`.`,
/// an operator, `{`). Returns the new text and the offsets (in `src`) where a
/// `;` was written, sorted.
pub(crate) fn with_semicolons(src: &[u8]) -> (Vec<u8>, Vec<usize>) {
    let mut out = Vec::with_capacity(src.len() + src.len() / 16);
    let mut inserted = Vec::new();
    let mut start = 0;
    while start < src.len() {
        let end = src[start..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(src.len(), |i| start + i);
        out.extend_from_slice(&src[start..end]);
        if ends_statement(&src[start..end], next_line(src, end)) {
            out.push(b';');
            inserted.push(end);
        }
        if end < src.len() {
            out.push(b'\n');
        }
        start = end + 1;
    }
    (out, inserted)
}

/// The first line with text after the line that ends at `end`.
fn next_line(src: &[u8], end: usize) -> &[u8] {
    src.get(end + 1..)
        .unwrap_or_default()
        .split(|&b| b == b'\n')
        .map(|l| l.trim_ascii())
        .find(|l| !l.is_empty())
        .unwrap_or_default()
}

fn ends_statement(line: &[u8], next: &[u8]) -> bool {
    let t = line.trim_ascii();
    let Some(&last) = t.last() else {
        return false;
    };
    let ends_value = last.is_ascii_alphanumeric() || matches!(last, b'_' | b'"' | b'\'' | b')' | b']');
    let header = [
        &b"@"[..],
        b"//",
        b"/*",
        b"*",
        b"if",
        b"for",
        b"while",
        b"switch",
        b"catch",
        b"else",
        b"synchronized",
    ]
    .iter()
    .any(|p| t.starts_with(p));
    let keyword_end = [&b"else"[..], b"try", b"finally", b"do"].iter().any(|k| t.ends_with(k));
    let goes_on = next.first().is_some_and(|b| b"{.?:+-*/&|=)]".contains(b));
    ends_value && !header && !keyword_end && !goes_on
}

/// The offset in the original source of `at`, an offset in the text that
/// [`with_semicolons`] made, given the offsets where it wrote a `;`.
pub(crate) fn unshift(inserted: &[usize], at: usize) -> usize {
    // The k-th `;` sits at `inserted[k] + k` in the new text.
    let before = inserted.iter().enumerate().take_while(|(k, p)| *p + k < at).count();
    at - before
}

/// True for a qualifier that stands for the instance (`self.create` in Ruby).
pub(crate) fn is_self_word(text: &str) -> bool {
    SELF_WORDS.contains(&text)
}

/// The receiver variable and type of a Go method: `("c", "Cart")` for
/// `func (c *Cart) Total() int`. The variable is `None` when it is not named.
pub(crate) fn go_receiver(definition: &str) -> Option<(Option<String>, String)> {
    let rest = definition.trim_start().strip_prefix("func")?.trim_start();
    let inner = rest.strip_prefix('(')?;
    let inner = &inner[..inner.find(')')?];
    // Type parameters (`Cache[K, V]`) hold spaces of their own.
    let inner = inner.split('[').next().unwrap_or(inner);
    let mut words: Vec<&str> = inner.split_whitespace().collect();
    let ty = words.pop()?;
    let ty = ty.trim_start_matches('*');
    let ty = ty.split('[').next().unwrap_or(ty).trim();
    if ty.is_empty() {
        return None;
    }
    Some((words.first().map(|v| v.to_string()), ty.to_string()))
}

/// Openers that end a definition's header, stripped from its signature.
const HEADER_ENDS: [&str; 8] = [" do", " where", " then", " is", "=", "->", ":", "{"];

/// Lines a header may run on to while its brackets are open.
const HEADER_LINES: usize = 6;

/// The header of a definition: from the line its name is on (or the start of
/// the definition, when later) up to the body opener: a `{`, `;`, ` = ` or
/// ` => ` outside brackets, or the end of the line once brackets are balanced
/// (at most [`HEADER_LINES`] lines). A function assigned to a name keeps its
/// parameters (`fit = function(df)`), and Julia's `where {T}` is a type
/// parameter list, not a body. Whitespace is collapsed and the result is at
/// most 240 chars.
pub(crate) fn signature(src: &[u8], lines: &Lines, definition_start: usize, name_start: usize, end: usize) -> String {
    let start = lines.start_of(name_start).max(definition_start);
    let window = &src[start..end.min(start + 2000).max(start)];
    let text = String::from_utf8_lossy(window);
    let mut depth = 0i32;
    let mut where_braces = 0i32;
    let mut newlines = 0;
    let mut cut = text.len();
    for (i, c) in text.char_indices() {
        let where_brace = c == '{' && text[..i].trim_end().ends_with(" where");
        let opens_body = depth <= 0
            && match c {
                '{' => !where_brace,
                ';' | '\n' => true,
                ' ' => {
                    let assigned_function =
                        text[i..].starts_with(" = ") && text[i + 3..].trim_start().starts_with("function");
                    (text[i..].starts_with(" = ") && !assigned_function) || text[i..].starts_with(" => ")
                }
                _ => false,
            };
        if c == '\n' {
            newlines += 1;
        }
        if opens_body || newlines == HEADER_LINES {
            cut = i;
            break;
        }
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            '{' if where_brace => {
                depth += 1;
                where_braces += 1;
            }
            '}' if where_braces > 0 => {
                depth -= 1;
                where_braces -= 1;
            }
            _ => {}
        }
    }
    let mut sig = text[..cut].split_whitespace().collect::<Vec<_>>().join(" ");
    for end in HEADER_ENDS {
        if let Some(rest) = sig.strip_suffix(end) {
            sig = rest.trim_end().to_string();
        }
    }
    sig.chars().take(240).collect()
}

/// Comment openers, longest first, stripped from a doc line.
const OPENERS: [&str; 23] = [
    "{-|", "{-", "(**", "(*", "/**", "/*!", "/*", "////", "///", "//!", "//", "--|", "---", "--", "#'", "##", "#",
    "%%%", "%%", "%", ";;", ";", "*",
];
const CLOSERS: [&str; 3] = ["-}", "*)", "*/"];

/// The text of one comment line without its comment markers, and without XML
/// tags when it starts with one (`/// <summary>Adds up.</summary>`).
pub(crate) fn comment_text(line: &str) -> String {
    let mut t = line.trim();
    if let Some(o) = OPENERS.iter().find(|o| t.starts_with(*o)) {
        t = &t[o.len()..];
    }
    let mut t = t.trim_start().trim_start_matches(['|', '!']).trim();
    for c in CLOSERS {
        t = t.strip_suffix(c).unwrap_or(t).trim_end();
    }
    if t.starts_with('<') {
        strip_tags(t)
    } else {
        t.to_string()
    }
}

fn strip_tags(text: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in text.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.trim().to_string()
}

/// True for a line that is part of a comment in a language whose line comment
/// is `comment`.
fn is_comment_line(trimmed: &str, comment: &str) -> bool {
    trimmed.starts_with(comment) || ["/*", "*", "(*", "{-", "-}"].iter().any(|o| trimmed.starts_with(o))
}

/// A string on a line of its own: a docstring (Julia).
fn is_docstring_line(trimmed: &str) -> bool {
    trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"')
}

/// First line of a doc text, at most 200 chars.
pub(crate) fn first_doc_line<'a>(lines: impl IntoIterator<Item = &'a str>) -> String {
    lines
        .into_iter()
        .map(comment_text)
        .find(|t| !t.is_empty())
        .map(|t| t.trim_matches('"').trim().chars().take(200).collect())
        .unwrap_or_default()
}

/// Block comments and docstrings that span lines: (opener, closer).
const BLOCKS: [(&str, &str); 5] = [
    ("<#", "#>"),
    ("{-", "-}"),
    ("/*", "*/"),
    ("(*", "*)"),
    ("\"\"\"", "\"\"\""),
];

/// The doc line of a block comment (or docstring) that ends on the last of
/// `lines`: its first line with text, without markers and without help
/// keywords (PowerShell's `.SYNOPSIS`). `None` when the last line ends no block.
fn block_doc(lines: &[&str]) -> Option<String> {
    let last = lines.last()?.trim();
    let (opener, closer) = BLOCKS
        .iter()
        .find(|(o, c)| last.ends_with(c) && !last[..last.len() - c.len()].contains(o))?;
    let before = &lines[..lines.len() - 1];
    let open = before.iter().rposition(|l| l.contains(opener))?;
    let doc = lines[open..]
        .iter()
        .map(|l| {
            let t = l.trim();
            let t = t.split_once(opener).map_or(t, |(_, rest)| rest);
            let t = t.strip_suffix(closer).unwrap_or(t);
            comment_text(t.trim_start_matches(['|', '*', '!']))
        })
        .find(|t| !t.is_empty() && !t.starts_with('.'))?;
    Some(doc.trim_matches('"').trim().chars().take(200).collect())
}

/// The doc line of a definition that starts on line `index` (0-based) of
/// `lines`: the first line of the comment block right above it. Attributes and
/// annotations (`#[test]`, `@Override`, `@spec`, Erlang's `-spec`) and a type
/// signature of the same name (`total : Cart -> Int`) may sit between them.
/// Elixir's `@doc "..."` counts, and so does a block comment or docstring
/// over several lines (`<# .SYNOPSIS ... #>`, `{-| ... -}`, `""" ... """`).
pub(crate) fn leading_doc(lines: &[&str], index: usize, comment: &str, name: &str) -> String {
    let mut block: Vec<&str> = Vec::new();
    let above = &lines[..index.min(lines.len())];
    for (k, line) in above.iter().enumerate().rev() {
        let t = line.trim();
        if let Some(doc) = t.strip_prefix("@doc ") {
            if block.is_empty() {
                return first_doc_line([doc.trim()]);
            }
            break;
        }
        let signature_of_name = t
            .strip_prefix(name)
            .is_some_and(|rest| rest.trim_start().starts_with(':'));
        let attribute = t.starts_with("#[") || t.starts_with('@') || t.starts_with("-spec");
        if block.is_empty() && (attribute || signature_of_name) {
            continue;
        }
        if block.is_empty() {
            if let Some(doc) = block_doc(&above[..=k]) {
                return doc;
            }
        }
        if t.is_empty() || !(is_comment_line(t, comment) || is_docstring_line(t)) {
            break;
        }
        block.push(t);
    }
    block.reverse();
    first_doc_line(block)
}

/// The doc line of a file: its first comment, skipping a shebang, licence lines
/// and one-word directives (`--!strict`). A comment right above a definition
/// is that definition's doc, not the file's: `starts` holds the 1-based first
/// lines of the file's definitions.
pub(crate) fn module_doc(lines: &[&str], comment: &str, starts: &[i64]) -> String {
    let mut block: Vec<&str> = Vec::new();
    let mut after = 0;
    for (i, line) in lines.iter().enumerate().take(40) {
        let t = line.trim();
        let skip = t.starts_with("#!")
            || t.starts_with("<?php")
            || t.contains("SPDX-License-Identifier")
            || t.contains("Copyright")
            || (is_comment_line(t, comment) && !comment_text(t).contains(' ') && block.is_empty());
        if t.is_empty() && block.is_empty() || skip {
            continue;
        }
        if t.is_empty() || !is_comment_line(t, comment) {
            break;
        }
        block.push(t);
        after = i + 1;
    }
    if starts.contains(&(after as i64 + 1)) {
        return String::new();
    }
    first_doc_line(block)
}

/// End of a definition without the blank and comment lines it ends with. Some
/// grammars (F#) let a definition run on over the doc comment of the next one.
pub(crate) fn trim_trailing_comments(src: &[u8], lines: &Lines, name_start: usize, end: usize, comment: &str) -> usize {
    let floor = lines.start_of(name_start);
    let mut end = end.min(src.len());
    loop {
        let line_start = lines.start_of(end.saturating_sub(1).max(floor));
        if line_start <= floor {
            return end;
        }
        let text = String::from_utf8_lossy(&src[line_start..end]);
        let t = text.trim();
        if !t.is_empty() && !t.starts_with(comment) {
            return end;
        }
        end = trim_ascii_end(src, line_start);
    }
}

/// True for a test file: under a `tests/`, `test/`, `spec/`, `__tests__/` or
/// SwiftPM `Tests/` directory, or named like `*_test.*`, `*_tests.*`, `*Test.*`,
/// `*Tests.*`, `*_spec.*`, `test_*.*`, `*.spec.*` or `*.Tests.*`.
pub(crate) fn is_test_path(path: &str) -> bool {
    let rooted = format!("/{path}");
    if ["/tests/", "/test/", "/spec/", "/__tests__/", "/Tests/"]
        .iter()
        .any(|d| rooted.contains(d))
    {
        return true;
    }
    let base = path.rsplit('/').next().unwrap_or(path);
    let mut parts: Vec<&str> = base.split('.').collect();
    if parts.len() > 1 {
        parts.pop();
    }
    let stem = parts[0];
    // `Cart.spec.luau` (TestEZ, Jest-Lua) and `Cart.Tests.ps1` (Pester).
    let marked = parts[1..]
        .iter()
        .any(|p| matches!(*p, "spec" | "test" | "tests" | "Tests"));
    marked
        || stem.starts_with("test_")
        || stem.ends_with("_test")
        || stem.ends_with("_tests")
        || stem.ends_with("Test")
        || stem.ends_with("Tests")
        || stem.ends_with("_spec")
}
