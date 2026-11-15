//! What a shell command was for, read from its text. The proxy only shortens
//! the output of commands it understands; everything else passes untouched.

/// The purpose of a command, as far as the proxy can tell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Purpose {
    /// The whole of one file: `cat path`.
    ReadWhole { path: String },
    /// A line range of one file: `sed -n '10,40p' path`, `head -n 50 path`.
    /// `from_top` is true when the range starts at line 1: reading a file
    /// from the top is looking around, not fetching known lines.
    ReadRange {
        path: String,
        start: usize,
        end: usize,
        from_top: bool,
    },
    /// `rg`, `grep`, `git grep`, `ag`.
    Search,
    /// A lint, type-check, build or test run; the tool as `lge_filter` names it.
    Check { tool: String },
    /// Anything else, including every command the proxy cannot read with certainty.
    Other,
}

/// Characters that make a command more than one simple command. With any of
/// them present the output is not simply the output of the first word.
const SHELL_OPERATORS: [&str; 9] = ["|", "&&", "||", ";", ">", "<", "$(", "`", "\n"];

/// Split a simple command into words, honouring single and double quotes.
/// `None` when a quote is left open.
fn words(command: &str) -> Option<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for c in command.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                started = true;
            }
            None if c.is_whitespace() => {
                if started || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            None => cur.push(c),
        }
    }
    if quote.is_some() {
        return None;
    }
    if started || !cur.is_empty() {
        out.push(cur);
    }
    Some(out)
}

/// `10,40p` -> (10, 40); `5p` -> (5, 5).
fn sed_range(script: &str) -> Option<(usize, usize)> {
    let body = script.strip_suffix('p')?;
    match body.split_once(',') {
        Some((a, b)) => Some((a.parse().ok()?, b.parse().ok()?)),
        None => body.parse().ok().map(|n| (n, n)),
    }
}

/// Programs that may follow a search in a pipeline: each passes some of the
/// lines on as they are, so what is printed is still a list of matches.
const AFTER_SEARCH: [&str; 8] = ["head", "tail", "sort", "uniq", "grep", "rg", "egrep", "fgrep"];

/// True when a search command line prints only what the search found: every
/// stage of a pipe after it is one of [`AFTER_SEARCH`], and it is not chained
/// with another command. `2>&1`, `2>/dev/null` and a closing `|| true` are
/// allowed. Quotes are honoured, so a pattern may hold `|` or `;`.
fn plain_search(command: &str) -> bool {
    let command = command.trim_end();
    let command = command
        .strip_suffix("|| true")
        .or_else(|| command.strip_suffix("|| :"))
        .unwrap_or(command);
    let s: Vec<char> = command.chars().collect();
    let mut stages: Vec<String> = vec![String::new()];
    let mut quote: Option<char> = None;
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        match quote {
            Some(q) if c == q => quote = None,
            Some('"') if c == '\\' => {
                i += 1;
            }
            Some(_) => {}
            None => match c {
                '\'' | '"' => quote = Some(c),
                '\\' => {
                    i += 1;
                }
                '|' if s.get(i + 1) == Some(&'|') => return false,
                '|' => {
                    stages.push(String::new());
                    i += 1;
                    continue;
                }
                '>' => {
                    let rest: String = s[i + 1..].iter().collect();
                    let spaces = rest.chars().take_while(|c| c.is_whitespace()).count();
                    let stderr = i > 0 && s[i - 1] == '2' && (i < 2 || s[i - 2].is_whitespace());
                    let to = ["&1", "/dev/null"]
                        .into_iter()
                        .find(|t| rest.trim_start().starts_with(t));
                    match to {
                        Some(t) if stderr => {
                            i += 1 + spaces + t.chars().count();
                            continue;
                        }
                        _ => return false,
                    }
                }
                ';' | '&' | '`' | '\n' | '<' => return false,
                '$' if s.get(i + 1) == Some(&'(') => return false,
                _ => {}
            },
        }
        if let Some(stage) = stages.last_mut() {
            stage.push(c);
        }
        i += 1;
    }
    quote.is_none()
        && stages.iter().skip(1).all(|stage| {
            let first = stage.split_whitespace().next().unwrap_or("");
            AFTER_SEARCH.contains(&first.rsplit('/').next().unwrap_or(first))
        })
}

/// Read the purpose of a command line. A leading `cd dir &&` is allowed and
/// ignored; any other shell operator makes the command `Other`.
pub fn purpose(command: &str) -> Purpose {
    let mut command = command.trim();
    if let Some(rest) = command.strip_prefix("cd ") {
        match rest.split_once("&&") {
            Some((dir, tail)) if !SHELL_OPERATORS.iter().any(|op| dir.contains(op)) => command = tail.trim(),
            _ => return Purpose::Other,
        }
    }
    // A search pattern may contain `|`; look at the first word before judging operators.
    let first = command.split_whitespace().next().unwrap_or("");
    let program = first.rsplit('/').next().unwrap_or(first);
    if matches!(program, "rg" | "grep" | "ag" | "egrep" | "fgrep") || command.starts_with("git grep ") {
        // `rg -l x | xargs cat` prints files, not matches.
        return if plain_search(command) {
            Purpose::Search
        } else {
            Purpose::Other
        };
    }
    let tool = lge_filter::tool_of(command);
    if matches!(
        tool.as_str(),
        "pytest"
            | "py.test"
            | "cargo"
            | "jest"
            | "vitest"
            | "tsc"
            | "eslint"
            | "pyright"
            | "basedpyright"
            | "mypy"
            | "ruff"
            | "flake8"
    ) {
        return Purpose::Check { tool };
    }
    if SHELL_OPERATORS.iter().any(|op| command.contains(op)) {
        return Purpose::Other;
    }
    let Some(w) = words(command) else { return Purpose::Other };
    let args: Vec<&str> = w.iter().skip(1).map(String::as_str).collect();
    match (program, args.as_slice()) {
        ("cat", [path]) if !path.starts_with('-') => Purpose::ReadWhole { path: path.to_string() },
        ("sed", ["-n", script, path]) => match sed_range(script) {
            Some((start, end)) if start >= 1 && end >= start => Purpose::ReadRange {
                path: path.to_string(),
                start,
                end,
                from_top: start == 1,
            },
            _ => Purpose::Other,
        },
        ("head", ["-n", n, path]) | ("head", [path, "-n", n]) => match n.parse::<usize>() {
            Ok(n) if n >= 1 => Purpose::ReadRange {
                path: path.to_string(),
                start: 1,
                end: n,
                from_top: true,
            },
            _ => Purpose::Other,
        },
        _ => Purpose::Other,
    }
}

// ---------------------------------------------------------------------------
// Code mode: the command inside a script
// ---------------------------------------------------------------------------

/// The shell command a code-mode script runs, read from its one
/// `tools.exec_command({cmd: "...", workdir: "..."})` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptCommand {
    pub cmd: String,
    pub workdir: Option<String>,
}

/// One token of a script. Only what the reader below needs is kept.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    /// An identifier or a keyword.
    Ident(String),
    /// Punctuation; `=>`, `...` and `?.` are one token each.
    Punct(String),
    /// A string literal and its value; `None` for a template with `${}`,
    /// whose value is only known when the script runs.
    Str(Option<String>),
    /// A number or a regular expression literal.
    Other,
}

/// Words after which a `/` starts a regular expression, not a division.
const BEFORE_REGEX: [&str; 14] = [
    "return",
    "typeof",
    "instanceof",
    "in",
    "of",
    "new",
    "delete",
    "void",
    "throw",
    "case",
    "do",
    "else",
    "yield",
    "await",
];

fn is_ident_start(c: char) -> bool {
    c == '_' || c == '$' || c.is_alphabetic()
}

fn is_ident_char(c: char) -> bool {
    c == '_' || c == '$' || c.is_alphanumeric()
}

/// `count` hex digits at `s[*i..]` as a number.
fn hex(s: &[char], i: &mut usize, count: usize) -> Option<u32> {
    let digits: String = s.get(*i..*i + count)?.iter().collect();
    *i += count;
    if digits.chars().all(|c| c.is_ascii_hexdigit()) {
        u32::from_str_radix(&digits, 16).ok()
    } else {
        None
    }
}

/// The code point of a `\u` escape whose `u` is at `s[*i - 1]`.
fn unicode_escape(s: &[char], i: &mut usize) -> Option<u32> {
    if s.get(*i) == Some(&'{') {
        let close = s[*i..].iter().position(|c| *c == '}')? + *i;
        let digits: String = s[*i + 1..close].iter().collect();
        *i = close + 1;
        if digits.is_empty() || digits.len() > 6 || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        return u32::from_str_radix(&digits, 16).ok();
    }
    hex(s, i, 4)
}

/// Decode the escape whose backslash is at `s[*i - 1]` into `out`. `None` for
/// an escape whose meaning is not certain (a legacy octal, a lone surrogate).
fn read_escape(s: &[char], i: &mut usize, out: &mut String) -> Option<()> {
    let c = *s.get(*i)?;
    *i += 1;
    match c {
        'n' => out.push('\n'),
        't' => out.push('\t'),
        'r' => out.push('\r'),
        'b' => out.push('\u{8}'),
        'f' => out.push('\u{c}'),
        'v' => out.push('\u{b}'),
        '0' if !s.get(*i).is_some_and(char::is_ascii_digit) => out.push('\0'),
        '0'..='9' => return None,
        'x' => out.push(char::from_u32(hex(s, i, 2)?)?),
        'u' => {
            let first = unicode_escape(s, i)?;
            let code = if (0xD800..0xDC00).contains(&first) {
                // A surrogate pair written as two escapes.
                if s.get(*i) != Some(&'\\') || s.get(*i + 1) != Some(&'u') {
                    return None;
                }
                *i += 2;
                let low = unicode_escape(s, i)?;
                if !(0xDC00..0xE000).contains(&low) {
                    return None;
                }
                0x10000 + ((first - 0xD800) << 10) + (low - 0xDC00)
            } else {
                first
            };
            out.push(char::from_u32(code)?);
        }
        // A line continuation adds nothing.
        '\r' => {
            if s.get(*i) == Some(&'\n') {
                *i += 1;
            }
        }
        '\n' | '\u{2028}' | '\u{2029}' => {}
        other => out.push(other),
    }
    Some(())
}

/// A `'...'` or `"..."` literal whose opening quote is at `s[*i - 1]`.
fn quoted(s: &[char], i: &mut usize, quote: char) -> Option<String> {
    let mut out = String::new();
    loop {
        let c = *s.get(*i)?;
        *i += 1;
        match c {
            '\\' => read_escape(s, i, &mut out)?,
            '\n' | '\r' => return None,
            c if c == quote => return Some(out),
            c => out.push(c),
        }
    }
}

/// Templates nested in `${}` deeper than this are not read: the reader
/// recurses once per level and must not run out of stack on any input.
const MAX_TEMPLATE_DEPTH: usize = 16;

/// A template literal whose backtick is at `s[*i - 1]`, `depth` templates
/// deep. Its value, or `None` inside when it has a `${}`. The script text of
/// each `${}` goes to `code`.
fn template(s: &[char], i: &mut usize, code: &mut String, depth: usize) -> Option<Option<String>> {
    if depth > MAX_TEMPLATE_DEPTH {
        return None;
    }
    let mut out = String::new();
    let mut interpolated = false;
    loop {
        let c = *s.get(*i)?;
        *i += 1;
        match c {
            '`' => return Some((!interpolated).then_some(out)),
            '\\' => read_escape(s, i, &mut out)?,
            '$' if s.get(*i) == Some(&'{') => {
                interpolated = true;
                *i += 1;
                skip_expression(s, i, code, depth)?;
            }
            '\r' => {
                if s.get(*i) == Some(&'\n') {
                    *i += 1;
                }
                out.push('\n');
            }
            c => out.push(c),
        }
    }
}

/// Skip the expression of a `${` up to its closing brace, copying its text
/// (strings included) to `code`. `depth` is that of the template it is in.
fn skip_expression(s: &[char], i: &mut usize, code: &mut String, depth: usize) -> Option<()> {
    let mut braces = 0usize;
    loop {
        let start = *i;
        let c = *s.get(*i)?;
        *i += 1;
        match c {
            '}' if braces == 0 => return Some(()),
            '{' => braces += 1,
            '}' => braces -= 1,
            '\'' | '"' => {
                quoted(s, i, c)?;
            }
            '`' => {
                template(s, i, code, depth + 1)?;
            }
            _ => {}
        }
        code.extend(&s[start..*i]);
    }
}

/// A regular expression literal whose `/` is at `s[*i - 1]`.
fn regex(s: &[char], i: &mut usize) -> Option<()> {
    let mut class = false;
    loop {
        let c = *s.get(*i)?;
        *i += 1;
        match c {
            '\\' => *i += 1,
            '\n' | '\r' => return None,
            '[' => class = true,
            ']' => class = false,
            '/' if !class => break,
            _ => {}
        }
    }
    while s.get(*i).is_some_and(|c| is_ident_char(*c)) {
        *i += 1;
    }
    Some(())
}

/// The tokens of a script, without comments. `code` collects the text of
/// every `${}` expression, which is code the tokens do not show. `None` when
/// the script cannot be read with certainty (an open string or comment).
fn tokens(script: &str, code: &mut String) -> Option<Vec<Tok>> {
    let s: Vec<char> = script.chars().collect();
    let mut out: Vec<Tok> = Vec::new();
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        i += 1;
        if c.is_whitespace() {
            continue;
        }
        match c {
            '/' if s.get(i) == Some(&'/') => {
                while i < s.len() && s[i] != '\n' {
                    i += 1;
                }
            }
            '/' if s.get(i) == Some(&'*') => {
                let close = s[i + 1..].windows(2).position(|w| w == ['*', '/'])?;
                i += 1 + close + 2;
            }
            '/' => {
                let regex_here = match out.last() {
                    None => true,
                    Some(Tok::Punct(p)) => !matches!(p.as_str(), ")" | "]" | "}"),
                    Some(Tok::Ident(w)) => BEFORE_REGEX.contains(&w.as_str()),
                    Some(Tok::Str(_) | Tok::Other) => false,
                };
                if regex_here {
                    regex(&s, &mut i)?;
                    out.push(Tok::Other);
                } else {
                    out.push(Tok::Punct("/".into()));
                }
            }
            '\'' | '"' => out.push(Tok::Str(Some(quoted(&s, &mut i, c)?))),
            '`' => out.push(Tok::Str(template(&s, &mut i, code, 0)?)),
            c if c.is_ascii_digit() || (c == '.' && s.get(i).is_some_and(char::is_ascii_digit)) => {
                while i < s.len() && (is_ident_char(s[i]) || s[i] == '.') {
                    i += 1;
                }
                out.push(Tok::Other);
            }
            c if is_ident_start(c) => {
                let start = i - 1;
                while i < s.len() && is_ident_char(s[i]) {
                    i += 1;
                }
                out.push(Tok::Ident(s[start..i].iter().collect()));
            }
            '=' if s.get(i) == Some(&'>') => {
                i += 1;
                out.push(Tok::Punct("=>".into()));
            }
            '.' if s.get(i) == Some(&'.') && s.get(i + 1) == Some(&'.') => {
                i += 2;
                out.push(Tok::Punct("...".into()));
            }
            '?' if s.get(i) == Some(&'.') && !s.get(i + 1).is_some_and(char::is_ascii_digit) => {
                i += 1;
                out.push(Tok::Punct("?.".into()));
            }
            c => out.push(Tok::Punct(c.to_string())),
        }
    }
    Some(out)
}

fn punct(t: Option<&Tok>, p: &str) -> bool {
    matches!(t, Some(Tok::Punct(q)) if q == p)
}

fn ident(t: Option<&Tok>, w: &str) -> bool {
    matches!(t, Some(Tok::Ident(v)) if v == w)
}

/// Words that make what follows them in the same statement run more than once,
/// or never: loops, and function and class bodies.
const REPEATS: [&str; 5] = ["for", "while", "do", "function", "class"];

/// An open bracket around the call.
struct Frame {
    open: String,
    /// This bracket is inside a loop or a function.
    repeats: bool,
    /// The statement being read inside this bracket has started a loop or a function.
    statement_repeats: bool,
    /// A `{` that opens a block run at most once (`try`, `else`, `if (..)`, ...).
    once: bool,
    /// For a `(`: the word before it (`if`, `catch`, ...).
    after: Option<String>,
}

/// True when the token at `at` runs at most once: no loop or function around
/// it, and only blocks such as `try {` or `if (..) {` that run at most once.
fn runs_once(toks: &[Tok], at: usize) -> bool {
    let mut stack: Vec<Frame> = vec![Frame {
        open: String::new(),
        repeats: false,
        statement_repeats: false,
        once: true,
        after: None,
    }];
    // The word before the last `(` that was closed: `if (..) {` opens a block run once.
    let mut closed_after: Option<String> = None;
    // A `;` or `}` ends the statement unless the statement goes on with an
    // `else`, `catch` or `finally`: `for (..) if (a) b(); else c();` repeats `c()`.
    let ends_statement = |k: usize| !["else", "catch", "finally"].iter().any(|w| ident(toks.get(k + 1), w));
    for (k, t) in toks[..at].iter().enumerate() {
        let Some(top) = stack.last_mut() else { return false };
        match t {
            Tok::Ident(w) if REPEATS.contains(&w.as_str()) => top.statement_repeats = true,
            Tok::Punct(p) if p == "=>" => top.statement_repeats = true,
            Tok::Punct(p) if p == ";" && ends_statement(k) => top.statement_repeats = false,
            Tok::Punct(p) if matches!(p.as_str(), "(" | "[" | "{") => {
                let repeats = top.repeats || top.statement_repeats;
                let prev = k.checked_sub(1).and_then(|j| toks.get(j));
                let once = p == "{"
                    && (["try", "else", "finally", "catch"].iter().any(|w| ident(prev, w))
                        || (punct(prev, ")") && matches!(closed_after.as_deref(), Some("if" | "catch" | "switch"))));
                let after = match prev {
                    Some(Tok::Ident(w)) if p == "(" => Some(w.clone()),
                    _ => None,
                };
                stack.push(Frame {
                    open: p.clone(),
                    repeats,
                    statement_repeats: false,
                    once,
                    after,
                });
            }
            Tok::Punct(p) if matches!(p.as_str(), ")" | "]" | "}") => {
                let want = match p.as_str() {
                    ")" => "(",
                    "]" => "[",
                    _ => "{",
                };
                if stack.len() < 2 || stack.last().is_none_or(|f| f.open != want) {
                    return false;
                }
                let frame = stack.pop();
                closed_after = frame.and_then(|f| f.after);
                if p == "}" && ends_statement(k) {
                    if let Some(top) = stack.last_mut() {
                        top.statement_repeats = false;
                    }
                }
            }
            _ => {}
        }
    }
    stack
        .iter()
        .all(|f| !f.repeats && !f.statement_repeats && (f.open != "{" || f.once))
}

/// Keys that name the directory a command runs in, as for a function call.
const WORKDIR_KEYS: [&str; 3] = ["workdir", "cwd", "working_directory"];

/// Read the object literal of the call, whose `{` is at `toks[start]`, and the
/// `)` that closes the call after it.
fn call_arguments(toks: &[Tok], start: usize) -> Option<ScriptCommand> {
    let mut cmd: Option<String> = None;
    let mut workdir: Option<String> = None;
    let mut i = start + 1;
    loop {
        // One `key: value` entry, or the closing brace.
        let key = match toks.get(i)? {
            Tok::Punct(p) if p == "}" => break,
            Tok::Ident(k) => k.clone(),
            Tok::Str(Some(k)) => k.clone(),
            // A computed key, a spread, a template key: not certain what is passed.
            _ => return None,
        };
        i += 1;
        if !punct(toks.get(i), ":") {
            // A shorthand `{cmd}` passes a variable; a method is not an argument here.
            let shorthand = punct(toks.get(i), ",") || punct(toks.get(i), "}");
            if !shorthand || key == "cmd" || key == "command" || WORKDIR_KEYS.contains(&key.as_str()) {
                return None;
            }
            if punct(toks.get(i), ",") {
                i += 1;
            }
            continue;
        }
        i += 1;
        let from = i;
        let mut depth = 0usize;
        while let Some(t) = toks.get(i) {
            match t {
                Tok::Punct(p) if matches!(p.as_str(), "(" | "[" | "{") => depth += 1,
                Tok::Punct(p) if depth == 0 && (p == "," || p == "}") => break,
                Tok::Punct(p) if matches!(p.as_str(), ")" | "]" | "}") => depth = depth.checked_sub(1)?,
                _ => {}
            }
            i += 1;
        }
        let value = toks.get(from..i)?;
        let literal = match value {
            [Tok::Str(Some(v))] => Some(v.clone()),
            _ => None,
        };
        if key == "cmd" {
            if cmd.is_some() {
                return None;
            }
            cmd = Some(literal?);
        } else if key == "command" {
            return None;
        } else if WORKDIR_KEYS.contains(&key.as_str()) {
            if workdir.is_some() {
                return None;
            }
            workdir = Some(literal?);
        }
        if punct(toks.get(i), ",") {
            i += 1;
        }
    }
    // One argument: the object, then the call closes (a trailing comma is allowed).
    i += 1;
    if punct(toks.get(i), ",") {
        i += 1;
    }
    if !punct(toks.get(i), ")") {
        return None;
    }
    let cmd = cmd.filter(|c| !c.trim().is_empty())?;
    Some(ScriptCommand { cmd, workdir })
}

/// The command a code-mode script runs, when it is certain: the script calls
/// `tools.exec_command` (or `exec_command`) exactly once, outside any loop or
/// function, with an object whose `cmd` (and `workdir`, if given) is a plain
/// string literal. Several calls, another tool, a computed command, or
/// anything this reader does not follow give `None`.
pub fn script_command(script: &str) -> Option<ScriptCommand> {
    let mut code = String::new();
    let toks = tokens(script, &mut code)?;
    // Code inside a `${}` is not followed: it must not call a tool.
    if code.contains("exec_command") || code.contains("tools") {
        return None;
    }
    // Ways to reach a tool that the counting below would not see: an escaped
    // identifier (`tools`), code in a string (`eval`, `Function`,
    // `import()`), or a tool named by a string (`globalThis["tools"]`).
    let hidden = toks.iter().any(|t| match t {
        Tok::Punct(p) => p == "\\",
        Tok::Ident(w) => matches!(w.as_str(), "eval" | "Function" | "import"),
        Tok::Str(Some(v)) => v == "tools" || v == "exec_command",
        _ => false,
    });
    if hidden {
        return None;
    }
    let calls: Vec<usize> = toks
        .iter()
        .enumerate()
        .filter(|(_, t)| ident(Some(*t), "exec_command"))
        .map(|(k, _)| k)
        .collect();
    let [at] = calls.as_slice() else { return None };
    let at = *at;
    let uses_of_tools = toks.iter().filter(|t| ident(Some(*t), "tools")).count();
    let before = at.checked_sub(1).and_then(|k| toks.get(k));
    let start = if punct(before, ".") || punct(before, "?.") {
        // `tools.exec_command`, and no other tool in the script.
        if !ident(at.checked_sub(2).and_then(|k| toks.get(k)), "tools") || uses_of_tools != 1 {
            return None;
        }
        at - 2
    } else {
        if uses_of_tools != 0 {
            return None;
        }
        at
    };
    if !punct(toks.get(at + 1), "(") || !punct(toks.get(at + 2), "{") || !runs_once(&toks, start) {
        return None;
    }
    call_arguments(&toks, at + 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(script: &str) -> Option<(String, Option<String>)> {
        script_command(script).map(|c| (c.cmd, c.workdir))
    }

    #[test]
    fn the_command_of_a_code_mode_script_is_read_when_it_is_certain() {
        let want = |c: &str, w: Option<&str>| Some((c.to_string(), w.map(str::to_string)));
        assert_eq!(
            cmd(
                r#"const r = await tools.exec_command({cmd:"rg -n foo src","workdir":"/repo","yield_time_ms":10000}); text(r.output);"#
            ),
            want("rg -n foo src", Some("/repo"))
        );
        assert_eq!(
            cmd("const r = await tools.exec_command({ cmd: 'pytest -q', yield_time_ms: 30000 });\ntext(r.output)\n"),
            want("pytest -q", None)
        );
        assert_eq!(
            cmd("// run the tests\nconst r = await exec_command({cmd: `cargo test -p lge-core`, workdir: `/r`,});\ntext(r.output);"),
            want("cargo test -p lge-core", Some("/r"))
        );
        // Escapes are decoded the way JavaScript decodes them.
        assert_eq!(
            cmd(r#"await tools.exec_command({cmd: "rg -n \"fn main\" src\\bin \x41B\u{43}", workdir: '/it\'s'})"#),
            want(r#"rg -n "fn main" src\bin ABC"#, Some("/it's"))
        );
        assert_eq!(
            cmd(r#"await tools.exec_command({cmd: 'grep -rn "it'"'"'s" .'})"#),
            None,
            "quotes JavaScript cannot read"
        );
        assert_eq!(
            cmd("await tools.exec_command({cmd: `printf 'a\\nb'`})"),
            want("printf 'a\nb'", None)
        );
        // A block that runs once, other text around the call, a template elsewhere.
        assert_eq!(
            cmd("try {\n  const r = await tools.exec_command({cmd: \"sed -n '1,80p' a.py\"});\n  text(`got ${r.output.length}`);\n} catch (e) { text(String(e)); }"),
            want("sed -n '1,80p' a.py", None)
        );
        assert_eq!(
            cmd("if (ok) { const r = await tools.exec_command({cmd: 'cat a.py'}); text(r.output) }"),
            want("cat a.py", None)
        );
        assert_eq!(
            cmd("// @exec: {\"yield_time_ms\": 20000}\nconst { output } = await tools.exec_command({cmd: 'ls'});\ntext(output ?? '');"),
            want("ls", None)
        );
        assert_eq!(
            cmd("try { text((await tools.exec_command({cmd: 'ls'})).output) } catch { text('failed') }"),
            want("ls", None)
        );
        assert_eq!(
            cmd("try { a() } catch { text((await tools.exec_command({cmd: 'ls'})).output) }"),
            want("ls", None)
        );
        assert_eq!(
            cmd("const xs = [1, 2].map((x) => x * 2); /* exec_command */ const r = await tools.exec_command({cmd: 'ls', max_output_tokens: 6000, env: {A: '1'}}); text(r.output.split(/\\n'/).join('\\n'));"),
            want("ls", None)
        );
    }

    #[test]
    fn a_script_that_is_not_certain_gives_no_command() {
        for script in [
            // Several calls, or another tool beside it.
            "const a = await tools.exec_command({cmd: 'ls'}); const b = await tools.exec_command({cmd: 'pwd'});",
            "const r = await tools.exec_command({cmd: 'ls'}); await tools.view_image({path: 'a.png'});",
            // A computed command, a variable, a template with ${}.
            "const c = 'ls'; await tools.exec_command({cmd: c});",
            "await tools.exec_command({cmd: 'rg ' + pattern});",
            "await tools.exec_command({cmd: `rg -n ${pattern} src`});",
            "await tools.exec_command({cmd});",
            "await tools.exec_command({...opts});",
            "await tools.exec_command({['cmd']: 'ls'});",
            "await tools.exec_command({cmd: 'ls', workdir: dir});",
            "await tools.exec_command({cmd: 'ls', cmd: 'pwd'});",
            "await tools.exec_command({cmd: 'ls'}, extra);",
            "await tools.exec_command(args);",
            "await tools.exec_command({command: ['bash', '-lc', 'ls']});",
            // In a loop or a function: it may run more than once, or never.
            "for (const f of files) { await tools.exec_command({cmd: 'ls'}); }",
            "for (let i = 0; i < 3; i++) await tools.exec_command({cmd: 'ls'});",
            "while (more) await tools.exec_command({cmd: 'ls'});",
            "const run = () => tools.exec_command({cmd: 'ls'}); await run(); await run();",
            "async function run() { return tools.exec_command({cmd: 'ls'}); }",
            "const o = { run() { return tools.exec_command({cmd: 'ls'}); } };",
            "await Promise.all(files.map((f) => tools.exec_command({cmd: 'ls'})));",
            "{ await tools.exec_command({cmd: 'ls'}); }",
            // Hidden in code this reader does not follow, or only in a string or comment.
            "text(`${(await tools.exec_command({cmd: 'ls'})).output}`);",
            "// tools.exec_command({cmd: 'ls'})\nawait tools.shell({command: 'ls'});",
            "const t = tools; await t.exec_command({cmd: 'ls'});",
            "await other.exec_command({cmd: 'ls'});",
            // The loop body goes on after a block or a statement: `else` still repeats.
            "for (const f of fs) if (f) { text(f) } else await tools.exec_command({cmd: 'ls'});",
            "for (const f of fs) if (f) text(f); else await tools.exec_command({cmd: 'ls'});",
            "while (more) try { step() } catch (e) { text(e) } finally { await tools.exec_command({cmd: 'ls'}) }",
            // A second tool call the counting would not see.
            "await tools.exec_command({cmd: 'rg x'}); await \\u0074ools.exec\\u005fcommand({cmd: 'cat y'});",
            "await tools.exec_command({cmd: 'rg x'}); eval(\"tools.exec_command({cmd: 'cat y'})\");",
            "await tools.exec_command({cmd: 'rg x'}); await new Function('return 1')();",
            "await tools.exec_command({cmd: 'rg x'}); await globalThis['tools']['exec_command']({cmd: 'cat y'});",
            "await tools.exec_command({cmd: 'rg x'}); text(`${globalThis['tools'].exec_command}`);",
            // Broken scripts.
            "await tools.exec_command({cmd: 'ls",
            "await tools.exec_command({cmd: 'ls'}",
            "",
        ] {
            assert_eq!(script_command(script), None, "{script}");
        }
        // A loop that ended before the call does not make it repeat.
        assert_eq!(
            cmd("for (const f of fs) try { a(f) } catch (e) { b(e) } finally { c() }\nconst r = await tools.exec_command({cmd: 'ls'});"),
            Some(("ls".to_string(), None))
        );
        assert_eq!(
            cmd("for (const f of fs) if (f) text(f);\nconst r = await tools.exec_command({cmd: 'ls'});"),
            Some(("ls".to_string(), None))
        );
    }

    #[test]
    fn deeply_nested_templates_give_no_command_instead_of_running_out_of_stack() {
        // On a small stack: the reader must stop by itself, whatever the input.
        let script = format!(
            "await tools.exec_command({{cmd: 'ls'}}); text({}1{});",
            "`${".repeat(50_000),
            "}`".repeat(50_000)
        );
        let reader = std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || script_command(&script))
            .unwrap();
        assert_eq!(reader.join().unwrap(), None);
        // A few levels are read as usual.
        assert_eq!(
            cmd("await tools.exec_command({cmd: 'ls'}); text(`a ${`b ${`c ${1}`}`}`);"),
            Some(("ls".to_string(), None))
        );
        // A long script with one call is read, and quickly.
        let long = format!(
            "{}const r = await tools.exec_command({{cmd: 'rg -n foo src'}});\ntext(r.output);",
            "const x = [1, 2, 3].length; // a comment with exec_command in it\n".repeat(20_000)
        );
        assert_eq!(script_command(&long).map(|c| c.cmd).as_deref(), Some("rg -n foo src"));
    }

    #[test]
    fn a_search_piped_into_something_else_is_not_a_search() {
        for search in [
            "rg -n foo src | head -50",
            "rg -n foo src 2>/dev/null | sort | uniq | head -n 80",
            "rg -n foo src 2> /dev/null",
            "grep -rn 'a|b;c' . 2>&1 | grep -v test",
            "rg -n \"x\\\"|y\" src || true",
        ] {
            assert_eq!(purpose(search), Purpose::Search, "{search}");
        }
        for other in [
            "rg -l foo | xargs cat",
            "rg -l foo src | head -1 | xargs sed -n '1,80p'",
            "rg foo src; cat a.yml",
            "rg foo src && cat a.yml",
            "rg foo src || cat a.yml",
            "rg foo $(cat list)",
            "rg foo src > out.txt",
            "rg foo src | awk -F: '{print $3}'",
            "rg 'open quote",
        ] {
            assert_eq!(purpose(other), Purpose::Other, "{other}");
        }
    }

    #[test]
    fn file_reads_are_recognised_only_in_their_plain_forms() {
        assert_eq!(
            purpose("cat src/app.py"),
            Purpose::ReadWhole {
                path: "src/app.py".into()
            }
        );
        assert_eq!(
            purpose("cd svc && cat 'my file.py'"),
            Purpose::ReadWhole {
                path: "my file.py".into()
            }
        );
        assert_eq!(
            purpose("sed -n '10,40p' a/b.ts"),
            Purpose::ReadRange {
                path: "a/b.ts".into(),
                start: 10,
                end: 40,
                from_top: false
            }
        );
        assert_eq!(
            purpose("sed -n 1,200p a.py"),
            Purpose::ReadRange {
                path: "a.py".into(),
                start: 1,
                end: 200,
                from_top: true
            }
        );
        assert_eq!(
            purpose("head -n 80 a.py"),
            Purpose::ReadRange {
                path: "a.py".into(),
                start: 1,
                end: 80,
                from_top: true
            }
        );
        for other in [
            "cat a.py b.py",
            "cat -n a.py",
            "cat a.py | head",
            "cat a.py > out",
            "sed -n '/def/p' a.py",
            "sed -i 's/a/b/' a.py",
            "nl -ba a.py",
            "cat 'unterminated",
            "cd a; cat b",
            "python -c 'print(1)'",
            "",
        ] {
            assert_eq!(purpose(other), Purpose::Other, "{other}");
        }
    }

    #[test]
    fn searches_and_checks_are_recognised_with_pipes_and_flags() {
        for search in [
            "rg -n \"create|update\" -S src",
            "grep -rn foo .",
            "git grep -n handler",
            "/opt/homebrew/bin/rg foo | head -50",
        ] {
            assert_eq!(purpose(search), Purpose::Search, "{search}");
        }
        assert_eq!(
            purpose("pytest -q tests/test_a.py"),
            Purpose::Check { tool: "pytest".into() }
        );
        assert_eq!(
            purpose("cd web && npx tsc --noEmit"),
            Purpose::Check { tool: "tsc".into() }
        );
        assert_eq!(
            purpose("cargo test -p lge-core 2>&1 | tail -20"),
            Purpose::Check { tool: "cargo".into() }
        );
        assert_eq!(
            purpose("npm run build"),
            Purpose::Other,
            "an unknown runner is left alone"
        );
        assert_eq!(purpose("ls -la"), Purpose::Other);
    }
}
