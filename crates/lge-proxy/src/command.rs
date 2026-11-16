//! What a shell command was for, read from its text. The proxy only shortens
//! the output of commands it understands; everything else passes untouched.
//!
//! A command line is first normalised ([`normalise`]): leading statements that
//! print nothing (`cd dir`, `export A=b`, `set -euo pipefail`) are dropped, and
//! wrappers that do not change what is printed (`A=b`, `env`, `rtk proxy`,
//! `git -C dir`) are removed. Only a command that is then one producer, piped
//! at most into a few display filters, gets a purpose other than
//! [`Purpose::Other`]: the output of anything else is not the output of one
//! known program.

// ---------------------------------------------------------------------------
// Shell words
// ---------------------------------------------------------------------------

/// One word of a shell command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Word {
    /// What the program receives: quotes removed, escapes decoded.
    value: String,
    /// The word as written.
    raw: String,
    /// An unquoted `*`, `?`, `[` or `{`: the shell may turn it into other words.
    glob: bool,
    /// A `$` outside single quotes: the value is only known when the command runs.
    expands: bool,
}

/// An operator between words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    /// `&&`
    And,
    /// `||`
    Or,
    /// `;` or a line break.
    Semi,
    /// `|`
    Pipe,
    /// A redirection that leaves stdout as it is, or only drops it:
    /// `2>&1`, `2>/dev/null` or `>/dev/null`.
    Redirect(&'static str),
}

impl Op {
    fn text(self) -> &'static str {
        match self {
            Op::And => "&&",
            Op::Or => "||",
            Op::Semi => ";",
            Op::Pipe => "|",
            Op::Redirect(r) => r,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Sh {
    Word(Word),
    Op(Op),
}

/// Splits a command line into words and operators, honouring quotes and
/// backslashes the way a POSIX shell does.
struct Lexer<'a> {
    s: &'a [char],
    i: usize,
    toks: Vec<Sh>,
    /// The word being read and where it started.
    word: Option<(usize, Word)>,
    /// Something was seen that makes the line more than plain commands joined
    /// by `&&`, `;`, `||` and `|`: a substitution, a lone `&`, `|&`, a
    /// parenthesis, a here-document, or a redirection other than [`Op::Redirect`].
    complex: bool,
}

impl Lexer<'_> {
    fn word(&mut self, start: usize) -> &mut Word {
        &mut self.word.get_or_insert_with(|| (start, Word::default())).1
    }

    fn end_word(&mut self, end: usize) {
        if let Some((start, mut w)) = self.word.take() {
            w.raw = self.s[start..end].iter().collect();
            self.toks.push(Sh::Word(w));
        }
    }

    fn op(&mut self, at: usize, op: Op) {
        self.end_word(at);
        self.toks.push(Sh::Op(op));
    }

    /// A `;` or a line break. A line break after `&&`, `||` or `|` continues
    /// the command; an empty statement is nothing.
    fn separator(&mut self, at: usize, newline: bool) {
        self.end_word(at);
        match self.toks.last() {
            None | Some(Sh::Op(Op::Semi)) => {}
            Some(Sh::Op(Op::And | Op::Or | Op::Pipe)) => {
                if !newline {
                    self.complex = true;
                }
            }
            _ => self.toks.push(Sh::Op(Op::Semi)),
        }
    }

    /// The rest of a `"..."` string whose quote is at `at` or earlier in the word.
    fn double_quoted(&mut self, at: usize) -> Option<()> {
        self.word(at);
        loop {
            let c = *self.s.get(self.i)?;
            self.i += 1;
            match c {
                '"' => return Some(()),
                '\\' => {
                    let n = *self.s.get(self.i)?;
                    self.i += 1;
                    let w = self.word(at);
                    match n {
                        '$' | '`' | '"' | '\\' => w.value.push(n),
                        '\n' => {}
                        _ => {
                            w.value.push('\\');
                            w.value.push(n);
                        }
                    }
                }
                '$' => {
                    if self.s.get(self.i) == Some(&'(') {
                        self.complex = true;
                    }
                    let w = self.word(at);
                    w.expands = true;
                    w.value.push('$');
                }
                '`' => {
                    self.complex = true;
                    self.word(at).value.push('`');
                }
                c => self.word(at).value.push(c),
            }
        }
    }

    /// The rest of a `$'...'` string. Its escapes are skipped, not decoded:
    /// such a word already makes the command complex.
    fn ansi_quoted(&mut self, at: usize) -> Option<()> {
        self.word(at);
        loop {
            let c = *self.s.get(self.i)?;
            self.i += 1;
            match c {
                '\'' => return Some(()),
                '\\' => {
                    self.s.get(self.i)?;
                    self.i += 1;
                }
                c => self.word(at).value.push(c),
            }
        }
    }

    /// A `>` at `at`. Only `2>&1`, `2>/dev/null` and `>/dev/null` keep the
    /// command simple.
    fn redirect(&mut self, at: usize) {
        // `2>`: digits written right before the `>` name the stream.
        let fd = match &self.word {
            Some((start, w))
                if !w.value.is_empty()
                    && w.value.chars().all(|c| c.is_ascii_digit())
                    && at - start == w.value.len() =>
            {
                let fd = w.value.clone();
                self.word = None;
                Some(fd)
            }
            _ => {
                self.end_word(at);
                None
            }
        };
        let s = self.s;
        let allowed = match s.get(self.i) {
            Some('&') => {
                self.i += 1;
                let start = self.i;
                while s.get(self.i).is_some_and(|c| c.is_ascii_alphanumeric() || *c == '-') {
                    self.i += 1;
                }
                (fd.as_deref() == Some("2") && s[start..self.i] == ['1']).then_some("2>&1")
            }
            Some('>' | '|' | '(') => {
                self.i += 1;
                None
            }
            _ => {
                while matches!(s.get(self.i), Some(' ' | '\t')) {
                    self.i += 1;
                }
                let start = self.i;
                while s
                    .get(self.i)
                    .is_some_and(|c| !c.is_whitespace() && !"|&;<>()'\"`$\\".contains(*c))
                {
                    self.i += 1;
                }
                let target: String = s[start..self.i].iter().collect();
                match (fd.as_deref(), target.as_str()) {
                    (Some("2"), "/dev/null") => Some("2>/dev/null"),
                    (None, "/dev/null") => Some(">/dev/null"),
                    _ => None,
                }
            }
        };
        match allowed {
            Some(r) => self.toks.push(Sh::Op(Op::Redirect(r))),
            None => self.complex = true,
        }
    }
}

/// The words and operators of a command line, and whether it is complex (see
/// [`Lexer::complex`]). `None` when a quote is left open.
fn lex(command: &str) -> Option<(Vec<Sh>, bool)> {
    let s: Vec<char> = command.chars().collect();
    let mut lx = Lexer {
        s: &s,
        i: 0,
        toks: Vec::new(),
        word: None,
        complex: false,
    };
    while lx.i < s.len() {
        let at = lx.i;
        let c = s[at];
        lx.i += 1;
        match c {
            ' ' | '\t' | '\r' => lx.end_word(at),
            '\n' => lx.separator(at, true),
            ';' => {
                if s.get(lx.i) == Some(&';') {
                    lx.complex = true;
                    lx.i += 1;
                }
                lx.separator(at, false);
            }
            '&' => match s.get(lx.i) {
                Some('&') => {
                    lx.i += 1;
                    lx.op(at, Op::And);
                }
                Some('>') => {
                    lx.i += 1;
                    lx.complex = true;
                    lx.end_word(at);
                }
                // A command sent to the background.
                _ => {
                    lx.complex = true;
                    lx.separator(at, false);
                }
            },
            '|' => match s.get(lx.i) {
                Some('|') => {
                    lx.i += 1;
                    lx.op(at, Op::Or);
                }
                Some('&') => {
                    lx.i += 1;
                    lx.complex = true;
                    lx.op(at, Op::Pipe);
                }
                _ => lx.op(at, Op::Pipe),
            },
            // Subshells, groups by parenthesis, input redirections, here-documents.
            '(' | ')' | '<' => {
                lx.complex = true;
                lx.end_word(at);
            }
            '>' => lx.redirect(at),
            '#' if lx.word.is_none() => {
                while lx.i < s.len() && s[lx.i] != '\n' {
                    lx.i += 1;
                }
            }
            '\\' => match s.get(lx.i) {
                Some('\n') => lx.i += 1,
                Some(&n) => {
                    lx.i += 1;
                    lx.word(at).value.push(n);
                }
                None => lx.word(at).value.push('\\'),
            },
            '\'' => {
                let start = lx.i;
                let len = s[start..].iter().position(|c| *c == '\'')?;
                lx.word(at).value.extend(&s[start..start + len]);
                lx.i = start + len + 1;
            }
            '"' => lx.double_quoted(at)?,
            '$' => match s.get(lx.i) {
                Some('\'') => {
                    lx.complex = true;
                    lx.i += 1;
                    lx.ansi_quoted(at)?;
                }
                next => {
                    if matches!(next, Some('(' | '"')) {
                        lx.complex = true;
                    }
                    let w = lx.word(at);
                    w.expands = true;
                    w.value.push('$');
                }
            },
            '`' => {
                lx.complex = true;
                lx.word(at).value.push('`');
            }
            '*' | '?' | '[' | '{' => {
                let w = lx.word(at);
                w.glob = true;
                w.value.push(c);
            }
            c => lx.word(at).value.push(c),
        }
    }
    lx.end_word(s.len());
    Some((lx.toks, lx.complex))
}

fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// `NAME=value`, written without quotes around the name.
fn is_assignment(w: &Word) -> bool {
    let Some((name, _)) = w.raw.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// A word whose value is fixed when written: no `$`, no backquote.
fn is_literal(w: &Word) -> bool {
    !w.raw.contains('$') && !w.raw.contains('`')
}

/// An argument of `set` that only changes how errors are handled:
/// `-e`, `-u`, `-o`, `+e`, a cluster of these, or the option names.
fn is_quiet_set_flag(arg: &str) -> bool {
    matches!(arg, "pipefail" | "errexit" | "nounset")
        || (arg.len() >= 2
            && (arg.starts_with('-') || arg.starts_with('+'))
            && arg[1..].chars().all(|c| matches!(c, 'e' | 'u' | 'o')))
}

/// What a statement that prints nothing says about the command after it.
#[derive(Debug, Clone, Copy, Default)]
struct Quiet {
    /// It exports `LGE_RAW=1`.
    raw: bool,
    /// It turns on `pipefail`: a pipe's exit status is the producer's.
    pipefail: bool,
}

/// `Some` when the statement prints nothing and changes nothing the command
/// after it prints: `cd DIR` (not `cd -`, which prints the directory),
/// `export NAME=literal ...`, `NAME=literal ...`, and `set` with only `-e`,
/// `-u`, `-o pipefail` and the like (not `-x` or `-v`, which print a trace).
fn quiet(statement: &[Sh]) -> Option<Quiet> {
    let words: Vec<&Word> = statement
        .iter()
        .map(|t| match t {
            Sh::Word(w) => Some(w),
            Sh::Op(_) => None,
        })
        .collect::<Option<_>>()?;
    let (first, rest) = words.split_first()?;
    let sets_raw = |ws: &[&Word]| ws.iter().any(|w| w.raw == "LGE_RAW=1");
    match first.raw.as_str() {
        "cd" => (rest.len() == 1 && rest[0].value != "-").then(Quiet::default),
        "export" => (!rest.is_empty() && rest.iter().all(|w| is_assignment(w) && is_literal(w))).then(|| Quiet {
            raw: sets_raw(rest),
            pipefail: false,
        }),
        "set" => {
            // `set -o` with no option name after it prints every option.
            let ends_open = rest
                .last()
                .is_some_and(|w| w.raw.starts_with(['-', '+']) && w.raw.contains('o'));
            (!rest.is_empty() && !ends_open && rest.iter().all(|w| is_quiet_set_flag(&w.raw))).then(|| Quiet {
                raw: false,
                pipefail: rest.iter().any(|w| w.raw == "pipefail"),
            })
        }
        _ => words.iter().all(|w| is_assignment(w) && is_literal(w)).then(|| Quiet {
            raw: sets_raw(&words),
            pipefail: false,
        }),
    }
}

// ---------------------------------------------------------------------------
// Normalisation
// ---------------------------------------------------------------------------

/// How a command was run through RTK, another output filter an agent may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rtk {
    /// Not through RTK.
    No,
    /// `rtk CMD`: RTK has condensed the output already.
    Condensed,
    /// `rtk proxy CMD` or `rtk bypass CMD`: RTK passed the output on as it was.
    Bypass,
}

/// A command line reduced to the one program whose output it prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Normalised {
    /// The producer and its pipe stages as written, without the statements
    /// and wrappers that were dropped. Two calls with the same `command` ran
    /// the same thing; this is the string compared between calls.
    pub command: String,
    /// The basename of the producer's first word.
    pub program: String,
    /// The first word after the program that does not start with `-`.
    pub sub: Option<String>,
    /// The producer's words after the program, as the program receives them.
    pub args: Vec<String>,
    /// The basename of the first word of each later `|` stage.
    pub stages: Vec<String>,
    /// `LGE_RAW=1` was set for it: the agent asked for the output whole.
    pub raw: bool,
    pub rtk: Rtk,
    /// One producer is left, with nothing around it that could print or
    /// change what it prints. Everything above describes that producer only
    /// when this is true.
    pub single: bool,
    /// The exit status the shell reports may not be the run's verdict: a
    /// final `|| true` or `|| :`, a pipe without `set -o pipefail`, an `||`,
    /// or several statements joined by `;` or a line break (the status is the
    /// last one's).
    pub exit_masked: bool,
}

/// One `|` stage of the producer.
#[derive(Debug, Clone, Default)]
struct Stage {
    words: Vec<Word>,
    redirects: Vec<&'static str>,
}

impl Stage {
    fn program(&self) -> &str {
        self.words.first().map_or("", |w| basename(&w.value))
    }

    fn args(&self) -> &[Word] {
        self.words.get(1..).unwrap_or(&[])
    }

    fn values(&self) -> Vec<&str> {
        self.args().iter().map(|w| w.value.as_str()).collect()
    }

    fn text(&self) -> String {
        let mut parts: Vec<&str> = self.words.iter().map(|w| w.raw.as_str()).collect();
        // Codex captures both streams: `2>&1` changes nothing that is printed.
        parts.extend(self.redirects.iter().filter(|r| **r != "2>&1"));
        parts.join(" ")
    }
}

/// A normalised command and the words behind it.
#[derive(Debug, Clone)]
struct Parsed {
    norm: Normalised,
    stages: Vec<Stage>,
}

impl Parsed {
    fn producer(&self) -> &Stage {
        &self.stages[0]
    }

    fn program(&self) -> &str {
        &self.norm.program
    }

    fn sub(&self) -> Option<&str> {
        self.norm.sub.as_deref()
    }

    /// The later `|` stages.
    fn later(&self) -> &[Stage] {
        &self.stages[1..]
    }

    /// The producer's words after its subcommand (`git grep -n x` gives `-n x`).
    fn after_sub(&self) -> &[Word] {
        let args = self.producer().args();
        match args.iter().position(|w| !w.value.starts_with('-')) {
            Some(k) => &args[k + 1..],
            None => &[],
        }
    }
}

/// Removes, from the front of the producer, what does not change what it
/// prints: `NAME=value` words (`LGE_RAW=1` sets `raw`), `env`, `rtk`, and
/// git's global options.
fn strip_wrappers(words: &mut Vec<Word>, raw: &mut bool) -> Rtk {
    let mut rtk = Rtk::No;
    loop {
        let before = words.len();
        while words.first().is_some_and(is_assignment) {
            if words[0].raw == "LGE_RAW=1" {
                *raw = true;
            }
            words.remove(0);
        }
        if words.first().is_some_and(|w| w.raw == "env") && words.get(1).is_some_and(|w| !w.value.starts_with('-')) {
            words.remove(0);
        }
        if rtk == Rtk::No && words.first().is_some_and(|w| w.raw == "rtk") {
            match words.get(1).map(|w| w.value.as_str()) {
                Some("proxy" | "bypass") if words.len() > 2 => {
                    words.drain(..2);
                    rtk = Rtk::Bypass;
                }
                Some(v) if !v.starts_with('-') && !matches!(v, "proxy" | "bypass") => {
                    words.remove(0);
                    rtk = Rtk::Condensed;
                }
                _ => {}
            }
        }
        if words.len() == before {
            break;
        }
    }
    if words.first().is_some_and(|w| basename(&w.value) == "git") {
        loop {
            match words.get(1).map(|w| w.value.as_str()) {
                Some("-C" | "-c") if words.len() > 2 => {
                    words.drain(1..3);
                }
                Some("--no-pager" | "-P" | "--no-optional-locks") => {
                    words.remove(1);
                }
                Some(v) if v.starts_with("--git-dir=") || v.starts_with("--work-tree=") => {
                    words.remove(1);
                }
                _ => break,
            }
        }
    }
    rtk
}

fn parse(command: &str) -> Option<Parsed> {
    let (mut toks, mut complex) = lex(command)?;
    while matches!(toks.last(), Some(Sh::Op(Op::Semi))) {
        toks.pop();
    }
    if matches!(toks.last(), Some(Sh::Op(Op::And | Op::Or | Op::Pipe))) {
        complex = true;
    }
    // A final `|| true` or `|| :` changes the exit status, not the output.
    let mut or_true = false;
    if let [.., Sh::Op(Op::Or), Sh::Word(w)] = toks.as_slice() {
        if w.raw == "true" || w.raw == ":" {
            toks.truncate(toks.len() - 2);
            or_true = true;
        }
    }
    // Statements, each with the index of its first token, and the operators
    // between them (`joins[k]` comes after statement `k`).
    let mut statements: Vec<(usize, Vec<Sh>)> = Vec::new();
    let mut joins: Vec<Op> = Vec::new();
    let mut current: Option<(usize, Vec<Sh>)> = None;
    for (k, t) in toks.iter().enumerate() {
        match t {
            Sh::Op(op @ (Op::And | Op::Semi | Op::Or)) => {
                if *op == Op::Or {
                    complex = true;
                }
                if let Some(statement) = current.take() {
                    statements.push(statement);
                    joins.push(*op);
                }
            }
            t => current.get_or_insert_with(|| (k, Vec::new())).1.push(t.clone()),
        }
    }
    statements.extend(current);

    let mut raw = false;
    let mut pipefail = false;
    let mut first = 0;
    while let Some(q) = statements.get(first).and_then(|(_, s)| quiet(s)) {
        raw |= q.raw;
        pipefail |= q.pipefail;
        first += 1;
    }
    let rest = &statements[first.min(statements.len())..];
    let single_statement = rest.len() == 1;
    // `LGE_RAW=1` in front of any statement left asks for the output whole.
    for (_, statement) in rest {
        let mut words = statement.iter().map_while(|t| match t {
            Sh::Word(w) => Some(w),
            Sh::Op(_) => None,
        });
        let leading = words.by_ref().take_while(|w| is_assignment(w) || w.raw == "env");
        if leading.into_iter().any(|w| w.raw == "LGE_RAW=1") {
            raw = true;
        }
    }
    // The shell reports the last statement's status after `;` or a line break,
    // and the last stage's after a pipe without `pipefail`, and `||` turns a
    // failure into the next statement's status: then it is not the run's verdict.
    let joined_masked = joins
        .iter()
        .skip(first)
        .take(rest.len().saturating_sub(1))
        .any(|op| matches!(op, Op::Semi | Op::Or));
    let piped = rest.iter().any(|(_, s)| s.contains(&Sh::Op(Op::Pipe)));
    // With every statement dropped, the last one stands for the command.
    let (start, statement) = rest
        .first()
        .or(statements.last())
        .cloned()
        .unwrap_or((toks.len(), Vec::new()));

    let mut stages: Vec<Stage> = vec![Stage::default()];
    for t in statement {
        if t == Sh::Op(Op::Pipe) {
            stages.push(Stage::default());
            continue;
        }
        let Some(stage) = stages.last_mut() else { continue };
        match t {
            Sh::Op(Op::Redirect(r)) => stage.redirects.push(r),
            Sh::Word(w) => stage.words.push(w),
            Sh::Op(_) => {}
        }
    }
    let rtk = strip_wrappers(&mut stages[0].words, &mut raw);
    let opens_block = stages[0]
        .words
        .first()
        .is_some_and(|w| matches!(w.raw.as_str(), "{" | "}" | "!"));
    let single = !complex && single_statement && !opens_block && stages.iter().all(|s| !s.words.is_empty());

    let producer = &stages[0];
    let command = if single {
        stages.iter().map(Stage::text).collect::<Vec<_>>().join(" | ")
    } else {
        toks[start.min(toks.len())..]
            .iter()
            .map(|t| match t {
                Sh::Word(w) => w.raw.as_str(),
                Sh::Op(op) => op.text(),
            })
            .collect::<Vec<_>>()
            .join(" ")
    };
    let norm = Normalised {
        command,
        program: producer.program().to_string(),
        sub: producer
            .args()
            .iter()
            .find(|w| !w.value.starts_with('-'))
            .map(|w| w.value.clone()),
        args: producer.args().iter().map(|w| w.value.clone()).collect(),
        stages: stages[1..].iter().map(|s| s.program().to_string()).collect(),
        raw,
        rtk,
        single,
        exit_masked: or_true || joined_masked || (piped && !pipefail),
    };
    Some(Parsed { norm, stages })
}

/// Normalise a command line; see [`Normalised`]. `None` only when a quote is
/// left open.
///
/// Statements are split on unquoted `&&`, `;`, `||` and line breaks. Leading
/// statements that print nothing are dropped (`cd DIR` but not `cd -`,
/// `export NAME=literal`, `NAME=literal`, `set -euo pipefail` but not
/// `set -x`); exactly one statement must remain. A substitution (`$(`, a backquote), a background
/// `&`, `|&`, a here-document, a process substitution, a subshell or group,
/// an `||` other than a final `|| true`, or a redirection other than `2>&1`,
/// `2>/dev/null` and `>/dev/null` makes the command not `single`.
pub fn normalise(command: &str) -> Option<Normalised> {
    parse(command).map(|p| p.norm)
}

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

/// One argument as a getopt-style reader sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arg<'a> {
    /// One letter of a `-abc` cluster, and the value it takes, if it takes one.
    Short(char, Option<&'a str>),
    /// `--name`, `--name=value`, or `--name value` for an option known to take one.
    Long(&'a str, Option<&'a str>),
    Positional(&'a str),
}

/// Reads `args` as options. `short_values` are the letters that take a value
/// (the rest of the cluster, or the next word); `long_values` the long names
/// that take the next word when no `=` is written.
fn scan<'a>(args: &'a [Word], short_values: &str, long_values: &[&str]) -> Vec<Arg<'a>> {
    let mut out = Vec::new();
    let mut k = 0;
    let mut only_positionals = false;
    while k < args.len() {
        let a = args[k].value.as_str();
        k += 1;
        if only_positionals || a == "-" || !a.starts_with('-') {
            out.push(Arg::Positional(a));
            continue;
        }
        if a == "--" {
            only_positionals = true;
            continue;
        }
        if let Some(long) = a.strip_prefix("--") {
            match long.split_once('=') {
                Some((name, value)) => out.push(Arg::Long(name, Some(value))),
                None if long_values.contains(&long) => {
                    out.push(Arg::Long(long, args.get(k).map(|w| w.value.as_str())));
                    k += 1;
                }
                None => out.push(Arg::Long(long, None)),
            }
            continue;
        }
        let body = &a[1..];
        for (j, c) in body.char_indices() {
            if short_values.contains(c) {
                let rest = &body[j + c.len_utf8()..];
                let value = if rest.is_empty() {
                    k += 1;
                    args.get(k - 1).map(|w| w.value.as_str())
                } else {
                    Some(rest)
                };
                out.push(Arg::Short(c, value));
                break;
            }
            out.push(Arg::Short(c, None));
        }
    }
    out
}

fn has_short(args: &[Arg], letters: &str) -> bool {
    args.iter()
        .any(|a| matches!(a, Arg::Short(c, _) if letters.contains(*c)))
}

fn has_long(args: &[Arg], names: &[&str]) -> bool {
    args.iter().any(|a| matches!(a, Arg::Long(n, _) if names.contains(n)))
}

fn positionals<'a>(args: &[Arg<'a>]) -> Vec<&'a str> {
    args.iter()
        .filter_map(|a| match a {
            Arg::Positional(p) => Some(*p),
            _ => None,
        })
        .collect()
}

const RG_SHORT_VALUES: &str = "ABCEMTdefgjmrt";
const RG_LONG_VALUES: &[&str] = &[
    "after-context",
    "before-context",
    "color",
    "colors",
    "context",
    "context-separator",
    "dfa-size-limit",
    "encoding",
    "engine",
    "field-context-separator",
    "field-match-separator",
    "file",
    "glob",
    "hostname-bin",
    "hyperlink-format",
    "iglob",
    "ignore-file",
    "max-columns",
    "max-count",
    "max-depth",
    "max-filesize",
    "path-separator",
    "pre",
    "pre-glob",
    "regex-size-limit",
    "regexp",
    "replace",
    "sort",
    "sortr",
    "threads",
    "type",
    "type-add",
    "type-clear",
    "type-not",
];
const GREP_SHORT_VALUES: &str = "ABCDdefm";
const GREP_LONG_VALUES: &[&str] = &[
    "after-context",
    "before-context",
    "binary-files",
    "context",
    "devices",
    "directories",
    "exclude",
    "exclude-dir",
    "exclude-from",
    "file",
    "group-separator",
    "include",
    "label",
    "max-count",
    "regexp",
];
const GIT_GREP_SHORT_VALUES: &str = "ABCefm";
const GIT_GREP_LONG_VALUES: &[&str] = &[
    "after-context",
    "before-context",
    "context",
    "max-count",
    "max-depth",
    "threads",
];
const AG_SHORT_VALUES: &str = "ABCGgmp";
const AG_LONG_VALUES: &[&str] = &[
    "after",
    "before",
    "context",
    "depth",
    "file-search-regex",
    "ignore",
    "ignore-dir",
    "max-count",
    "pager",
    "path-to-ignore",
    "workers",
];
const FD_SHORT_VALUES: &str = "EScdejotxX";
const FD_LONG_VALUES: &[&str] = &[
    "base-directory",
    "batch-size",
    "changed-before",
    "changed-within",
    "color",
    "exact-depth",
    "exclude",
    "exec",
    "exec-batch",
    "extension",
    "format",
    "ignore-file",
    "max-depth",
    "max-results",
    "min-depth",
    "owner",
    "path-separator",
    "search-path",
    "size",
    "threads",
    "type",
];

// ---------------------------------------------------------------------------
// Purpose
// ---------------------------------------------------------------------------

/// What a search command looks for, as far as the arguments say.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchArgs {
    /// The first `-e`/`--regexp` pattern, or the first positional argument.
    pub pattern: Option<String>,
    pub fixed: bool,
    pub ignore_case: bool,
    pub smart_case: bool,
    pub word: bool,
}

/// The purpose of a command, as far as the proxy can tell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Purpose {
    /// The whole of one file: `cat path`; `numbered` for `cat -n` and `nl -ba`,
    /// whose lines start with their number and a tab.
    ReadWhole { path: String, numbered: bool },
    /// A line range of one file: `sed -n '10,40p' path`, `head -n 50 path`.
    /// `from_top` is true when the range starts at line 1: reading a file
    /// from the top is looking around, not fetching known lines.
    ReadRange {
        path: String,
        start: usize,
        end: usize,
        from_top: bool,
        numbered: bool,
    },
    /// `rg`, `grep`, `git grep`, `ag` printing matching lines.
    Search(SearchArgs),
    /// A list of paths: `rg --files`, `rg -l`, `git ls-files`, `fd`, `find`.
    Paths,
    /// A lint, type-check, build or test run; the tool as `lge_filter` names
    /// it. `detail` when the agent asked for the program's own prints
    /// (`--nocapture`, `pytest -s`, `mvn -X`, ...).
    Check { tool: String, detail: bool },
    /// Output in a machine format (`--json`, `gh api`, `kubectl -o json`).
    Json,
    /// A patch: `git diff`, `git show`, `git log -p`, `diff`.
    Diff,
    /// Run through `rtk`, which has condensed the output already.
    Condensed,
    /// Run with `LGE_RAW=1`: the agent asked for the output whole.
    Raw,
    /// Anything else, including every command the proxy cannot read with certainty.
    Other,
}

/// Programs that may follow a search in a pipeline: each passes some of the
/// lines on as they are, so what is printed is still a list of matches.
const AFTER_SEARCH: [&str; 8] = ["head", "tail", "sort", "uniq", "grep", "rg", "egrep", "fgrep"];

/// Tools whose output is reduced to its result. `lge_filter` has a parser for each.
const CHECK_TOOLS: [&str; 24] = [
    "pytest",
    "py.test",
    "cargo",
    "jest",
    "vitest",
    "tsc",
    "eslint",
    "pyright",
    "basedpyright",
    "mypy",
    "ruff",
    "flake8",
    "go",
    "golangci-lint",
    "mvn",
    "mvnw",
    "gradle",
    "gradlew",
    "gcc",
    "g++",
    "cc",
    "c++",
    "clang",
    "clang++",
];

/// Programs and words that run the tool named after them.
const LAUNCHERS: [&str; 22] = [
    "npx", "pnpx", "bunx", "uvx", "npm", "pnpm", "yarn", "bun", "node", "python", "python3", "uv", "poetry", "pipenv",
    "pdm", "hatch", "run", "exec", "dlx", "time", "nice", "timeout",
];

/// Ruff's subcommands; a first word that is none of them is a path to check.
const RUFF_SUBCOMMANDS: [&str; 11] = [
    "check",
    "rule",
    "config",
    "linter",
    "clean",
    "format",
    "server",
    "analyze",
    "version",
    "help",
    "generate-shell-completion",
];

/// `tail` that waits for more: its output never ends.
fn tail_follows(stage: &Stage) -> bool {
    stage.values().iter().any(|a| {
        *a == "--follow"
            || a.starts_with("--follow=")
            || (a.starts_with('-') && !a.starts_with("--") && a.contains(['f', 'F']))
    })
}

/// `10,40p` -> (10, 40); `5p` -> (5, 5).
fn sed_range(script: &str) -> Option<(usize, usize)> {
    let body = script.strip_suffix('p')?;
    let (start, end) = match body.split_once(',') {
        Some((a, b)) => (a.parse().ok()?, b.parse().ok()?),
        None => {
            let n = body.parse().ok()?;
            (n, n)
        }
    };
    (start >= 1 && end >= start).then_some((start, end))
}

/// `sed -n 'A,Bp'` as a pipe stage: the range it keeps.
fn sed_stage(stage: &Stage) -> Option<(usize, usize)> {
    match (stage.program(), stage.values().as_slice()) {
        ("sed", ["-n", script]) => sed_range(script),
        _ => None,
    }
}

/// `head -n N`, `head -N` or `head -nN` as a pipe stage: N.
fn head_stage(stage: &Stage) -> Option<usize> {
    if stage.program() != "head" {
        return None;
    }
    let n = match stage.values().as_slice() {
        ["-n", n] => n.parse().ok()?,
        [flag] => flag
            .strip_prefix("-n")
            .filter(|n| !n.is_empty())
            .or_else(|| flag.strip_prefix('-'))?
            .parse()
            .ok()?,
        _ => return None,
    };
    (n >= 1).then_some(n)
}

/// A file path argument the shell passes on as written.
fn plain_path(w: &Word) -> Option<String> {
    (!w.value.is_empty() && !w.value.starts_with('-') && !w.glob && !w.expands).then(|| w.value.clone())
}

fn range(path: String, start: usize, end: usize, numbered: bool) -> Purpose {
    Purpose::ReadRange {
        path,
        start,
        end,
        from_top: start == 1,
        numbered,
    }
}

/// The read forms: `cat P`, `cat -n P`, `nl -ba P`, `nl -ba P | sed -n 'A,Bp'`,
/// `sed -n 'A,Bp' P`, `head -n N P` and `tail -n +A P | head -n K`.
fn read(p: &Parsed) -> Option<Purpose> {
    // A redirection of stdout leaves nothing to read.
    if p.stages.iter().any(|s| s.redirects.contains(&">/dev/null")) {
        return None;
    }
    let producer = p.producer();
    let args = producer.args();
    let values = producer.values();
    let later = p.later();
    let path_at = |k: usize| args.get(k).and_then(plain_path);
    match (p.program(), values.as_slice(), later) {
        ("cat", [_], []) => Some(Purpose::ReadWhole {
            path: path_at(0)?,
            numbered: false,
        }),
        ("cat", ["-n", _], []) => Some(Purpose::ReadWhole {
            path: path_at(1)?,
            numbered: true,
        }),
        ("nl", ["-ba", _], _) | ("nl", ["-b", "a", _], _) | ("cat", ["-n", _], [_]) => {
            let path = path_at(args.len() - 1)?;
            match later {
                [] => Some(Purpose::ReadWhole { path, numbered: true }),
                [stage] => {
                    let (start, end) = sed_stage(stage).or_else(|| head_stage(stage).map(|n| (1, n)))?;
                    Some(range(path, start, end, true))
                }
                _ => None,
            }
        }
        ("sed", ["-n", script, _], []) => {
            let (start, end) = sed_range(script)?;
            Some(range(path_at(2)?, start, end, false))
        }
        ("head", ["-n", n, _], []) | ("head", [_, "-n", n], []) => {
            let n: usize = n.parse().ok().filter(|n| *n >= 1)?;
            let path = if values[0] == "-n" { path_at(2)? } else { path_at(0)? };
            Some(range(path, 1, n, false))
        }
        ("head", [flag, _], []) => {
            let n: usize = flag
                .strip_prefix("-n")
                .filter(|n| !n.is_empty())
                .or_else(|| flag.strip_prefix('-'))?
                .parse()
                .ok()
                .filter(|n| *n >= 1)?;
            Some(range(path_at(1)?, 1, n, false))
        }
        ("tail", ["-n", from, _], [stage]) => {
            let start: usize = from.strip_prefix('+')?.parse().ok().filter(|n| *n >= 1)?;
            let k = head_stage(stage)?;
            Some(range(path_at(2)?, start, start + k - 1, false))
        }
        _ => None,
    }
}

fn is_diff(p: &Parsed) -> bool {
    match (p.program(), p.sub()) {
        ("diff", _) => true,
        ("git", Some("diff" | "show" | "range-diff")) => true,
        ("git", Some("log")) => p
            .after_sub()
            .iter()
            .any(|w| matches!(w.value.as_str(), "-p" | "-u") || w.value.starts_with("--patch")),
        ("git", Some("stash")) => {
            let rest = p.after_sub();
            rest.first().is_some_and(|w| w.value == "show")
                && rest.iter().any(|w| w.value == "-p" || w.value.starts_with("--patch"))
        }
        ("git", Some("format-patch")) => p.after_sub().iter().any(|w| w.value == "--stdout"),
        _ => false,
    }
}

/// Programs a list of paths may be piped through and stay a list of paths.
fn path_filter(stage: &Stage) -> bool {
    match stage.program() {
        "head" | "sort" | "uniq" | "grep" | "rg" => true,
        "tail" => !tail_follows(stage),
        _ => false,
    }
}

/// `Some(Paths)` for the forms that print a list of paths, `Some(Other)` for
/// those programs with an option that prints more than paths, `None` for
/// anything else.
fn paths(p: &Parsed) -> Option<Purpose> {
    let args = p.producer().args();
    let listed = match (p.program(), p.sub()) {
        ("rg", _) => {
            let a = scan(args, RG_SHORT_VALUES, RG_LONG_VALUES);
            // rg's -L is --follow, not a list.
            if !(has_short(&a, "l") || has_long(&a, &["files", "files-with-matches", "files-without-match"])) {
                return None;
            }
            // NUL-separated: not one path per line.
            !(has_short(&a, "0") || has_long(&a, &["null"]))
        }
        ("grep" | "egrep" | "fgrep", _) => {
            let a = scan(args, GREP_SHORT_VALUES, GREP_LONG_VALUES);
            if !(has_short(&a, "lL") || has_long(&a, &["files-with-matches", "files-without-match"])) {
                return None;
            }
            !(has_short(&a, "Z") || has_long(&a, &["null"]))
        }
        ("git", Some("ls-files")) => {
            let a = scan(
                p.after_sub(),
                "xX",
                &["exclude", "exclude-from", "exclude-per-directory"],
            );
            // Stage numbers, NUL separators, status tags, unmerged entries.
            !(has_short(&a, "stvfuz") || has_long(&a, &["stage", "format", "debug", "eol", "unmerged", "resolve-undo"]))
        }
        ("git", Some("ls-tree")) => {
            let a = scan(p.after_sub(), "", &[]);
            let named = has_long(&a, &["name-only", "name-status"]);
            let others_allowed = a.iter().all(|x| match x {
                Arg::Short(c, _) => matches!(c, 'r' | 't' | 'd'),
                Arg::Long(n, _) => {
                    matches!(*n, "name-only" | "name-status" | "full-name" | "full-tree" | "abbrev")
                }
                Arg::Positional(_) => true,
            });
            if !(named && others_allowed) {
                return None;
            }
            true
        }
        ("fd" | "fdfind", _) => {
            let a = scan(args, FD_SHORT_VALUES, FD_LONG_VALUES);
            !(has_short(&a, "xXl0") || has_long(&a, &["exec", "exec-batch", "list-details", "print0"]))
        }
        ("find", _) => !args.iter().any(|w| {
            matches!(
                w.value.as_str(),
                "-exec"
                    | "-execdir"
                    | "-ok"
                    | "-okdir"
                    | "-printf"
                    | "-fprintf"
                    | "-print0"
                    | "-fprint"
                    | "-fprint0"
                    | "-ls"
                    | "-fls"
                    | "-delete"
            )
        }),
        _ => return None,
    };
    Some(if listed && p.later().iter().all(path_filter) {
        Purpose::Paths
    } else {
        Purpose::Other
    })
}

/// Which search tool a command runs, and the words after it.
fn search_tool(p: &Parsed) -> Option<(&'static str, &[Word])> {
    let args = p.producer().args();
    Some(match (p.program(), p.sub()) {
        ("rg", _) => ("rg", args),
        ("grep", _) => ("grep", args),
        ("egrep", _) => ("egrep", args),
        ("fgrep", _) => ("fgrep", args),
        ("ag", _) => ("ag", args),
        ("git", Some("grep")) => ("git-grep", p.after_sub()),
        _ => return None,
    })
}

fn search(p: &Parsed) -> Option<Purpose> {
    let (tool, words) = search_tool(p)?;
    if !p.later().iter().all(|s| AFTER_SEARCH.contains(&s.program())) {
        return Some(Purpose::Other);
    }
    let (short_values, long_values) = match tool {
        "rg" => (RG_SHORT_VALUES, RG_LONG_VALUES),
        "ag" => (AG_SHORT_VALUES, AG_LONG_VALUES),
        "git-grep" => (GIT_GREP_SHORT_VALUES, GIT_GREP_LONG_VALUES),
        _ => (GREP_SHORT_VALUES, GREP_LONG_VALUES),
    };
    let a = scan(words, short_values, long_values);
    if has_long(&a, &["json"]) {
        return Some(Purpose::Json);
    }
    let rg_like = matches!(tool, "rg" | "ag");
    // Counts, nothing at all, NUL-separated output.
    let null = if rg_like { "0" } else { "Zz" };
    if has_short(&a, "cq")
        || has_short(&a, null)
        || has_long(&a, &["count", "count-matches", "quiet", "null", "null-data"])
    {
        return Some(Purpose::Other);
    }
    let mut s = SearchArgs {
        fixed: tool == "fgrep",
        // ag is smart-case unless told otherwise.
        smart_case: tool == "ag",
        ..SearchArgs::default()
    };
    let mut from_e: Option<&str> = None;
    let mut from_file = false;
    for arg in &a {
        match *arg {
            Arg::Short('e', v) | Arg::Long("regexp", v) => {
                if from_e.is_none() {
                    from_e = v;
                }
            }
            Arg::Short('f', _) | Arg::Long("file", _) if tool != "ag" => from_file = true,
            Arg::Short('F', _) | Arg::Long("fixed-strings", _) => s.fixed = true,
            Arg::Short('Q', _) | Arg::Long("literal", _) if tool == "ag" => s.fixed = true,
            Arg::Short('i', _) | Arg::Long("ignore-case", _) => s.ignore_case = true,
            Arg::Short('S', _) | Arg::Long("smart-case", _) if rg_like => s.smart_case = true,
            Arg::Short('s', _) | Arg::Long("case-sensitive", _) if rg_like => {
                s.ignore_case = false;
                s.smart_case = false;
            }
            Arg::Short('w', _) | Arg::Long("word-regexp", _) => s.word = true,
            _ => {}
        }
    }
    s.pattern = match from_e {
        Some(e) => Some(e.to_string()),
        // With patterns from a file, the first positional is a path.
        None if from_file => None,
        None => positionals(&a).first().map(|p| p.to_string()),
    };
    Some(Purpose::Search(s))
}

/// A value of `name` among `args`: `name value` or `name=value`.
fn option_values<'a>(args: &[&'a str], names: &[&str]) -> Vec<&'a str> {
    let mut out = Vec::new();
    for (k, a) in args.iter().enumerate() {
        for name in names {
            if a == name {
                out.extend(args.get(k + 1).copied());
            } else if let Some(v) = a.strip_prefix(name).and_then(|r| r.strip_prefix('=')) {
                out.push(v);
            } else if name.len() == 2 && !name.starts_with("--") && a.len() > 2 && a.starts_with(name) {
                // A short option with its value attached: `-ojson`.
                out.push(&a[2..]);
            }
        }
    }
    out
}

/// A machine-readable output format was asked for.
fn machine_format(args: &[&str]) -> bool {
    args.iter()
        .any(|a| matches!(*a, "--json" | "--outputjson") || a.starts_with("--output.json"))
        || option_values(
            args,
            &[
                "--reporter",
                "--format",
                "--output-format",
                "--message-format",
                "--out-format",
                "--output",
                "-f",
                "-O",
            ],
        )
        .iter()
        .any(|v| v.starts_with("json"))
}

/// pytest's `-s`, alone or in a cluster such as `-xvs`. The letters that take
/// a value end a cluster: `-rs` is `-r s`.
fn pytest_no_capture(args: &[&str]) -> bool {
    args.iter().any(|a| {
        if *a == "--capture=no" {
            return true;
        }
        let Some(body) = a.strip_prefix('-').filter(|b| !b.starts_with('-')) else {
            return false;
        };
        for c in body.chars() {
            if c == 's' {
                return true;
            }
            if "kmcpornW".contains(c) {
                return false;
            }
        }
        false
    }) || args.windows(2).any(|w| w == ["--capture", "no"])
}

fn check(p: &Parsed) -> Option<Purpose> {
    let producer = p.producer();
    let words: Vec<&str> = producer.words.iter().map(|w| w.value.as_str()).collect();
    let tool = lge_filter::tool_of(&words.join(" "));
    if !CHECK_TOOLS.contains(&tool.as_str()) {
        return None;
    }
    let Some(at) = words.iter().position(|w| basename(w) == tool) else {
        return Some(Purpose::Other);
    };
    // The tool must be what runs: the program itself, or behind launchers
    // (`npx tsc`, `python -m pytest`, `timeout 600 cargo test`). In
    // `ls tests/pytest` it is only an argument.
    let launched = words[..at]
        .iter()
        .all(|w| LAUNCHERS.contains(w) || w.starts_with('-') || w.bytes().all(|b| b.is_ascii_digit()));
    if !launched {
        return None;
    }
    let after = &words[at + 1..];
    let sub = after
        .iter()
        .copied()
        .find(|w| !w.starts_with('-') && !w.starts_with('+'));
    let allowed = match tool.as_str() {
        "cargo" => matches!(
            sub,
            Some("test" | "t" | "build" | "b" | "check" | "c" | "clippy" | "nextest")
        ),
        "go" => matches!(sub, Some("test" | "build" | "vet")),
        "golangci-lint" => sub == Some("run"),
        "ruff" => sub.is_none_or(|s| s == "check" || !RUFF_SUBCOMMANDS.contains(&s)),
        _ => true,
    };
    // Only `| cat` passes the output on whole; `| tail` and the like are
    // judged by content, under the closing-line rule.
    if !allowed || !p.later().iter().all(|s| s.program() == "cat" && s.args().is_empty()) {
        return Some(Purpose::Other);
    }
    let fixes = after.iter().any(|a| {
        matches!(
            *a,
            "--fix" | "--fix-only" | "--write" | "--unsafe-fixes" | "fmt" | "format"
        ) || a.starts_with("--fix=")
    });
    if fixes {
        return Some(Purpose::Other);
    }
    if tool == "go" && after.contains(&"-json") {
        // `go test -json` is read by the go parser; `go vet -json` is not.
        return Some(if sub == Some("test") {
            Purpose::Check { tool, detail: false }
        } else {
            Purpose::Json
        });
    }
    if machine_format(after) {
        return Some(Purpose::Json);
    }
    let detail = after
        .iter()
        .any(|a| matches!(*a, "--nocapture" | "--no-capture" | "--show-output"))
        || match tool.as_str() {
            "pytest" | "py.test" => pytest_no_capture(after),
            "mvn" | "mvnw" => after.iter().any(|a| matches!(*a, "-X" | "-e" | "--debug" | "--errors")),
            "gradle" | "gradlew" => after.iter().any(|a| {
                matches!(
                    *a,
                    "--info" | "--debug" | "--stacktrace" | "--full-stacktrace" | "-i" | "-d" | "-s" | "-S"
                )
            }),
            _ => false,
        };
    Some(Purpose::Check { tool, detail })
}

/// `jq` that prints JSON (not raw strings).
fn jq_prints_json(args: &[&str]) -> bool {
    !args.iter().any(|a| {
        matches!(*a, "--raw-output" | "--join-output" | "--raw-output0")
            || (a.starts_with('-') && !a.starts_with("--") && a.contains(['r', 'j']))
    })
}

fn json(p: &Parsed) -> bool {
    // A final `| jq` without raw output still prints JSON.
    if !p
        .later()
        .iter()
        .all(|s| s.program() == "jq" && jq_prints_json(&s.values()))
    {
        return false;
    }
    let args = p.producer().values();
    let format_is = |names: &[&str], wanted: &[&str]| {
        option_values(&args, names)
            .iter()
            .any(|v| wanted.iter().any(|w| v.starts_with(w)))
    };
    let generic = args.contains(&"--json") || format_is(&["--format"], &["json"]);
    match p.program() {
        "gh" => p.sub() == Some("api") || generic || args.iter().any(|a| a.starts_with("--json=")),
        "jq" => jq_prints_json(&args),
        "curl" => true,
        "kubectl" | "oc" => format_is(&["-o", "--output"], &["json"]),
        "aws" => !format_is(&["--output"], &["text", "table"]),
        "gcloud" => format_is(&["--format"], &["json"]),
        "az" => !format_is(&["-o", "--output"], &["table", "tsv"]),
        _ => generic,
    }
}

fn purpose_of(p: &Parsed) -> Purpose {
    let n = &p.norm;
    // Asked for whole, in a chain too.
    if n.raw {
        return Purpose::Raw;
    }
    if !n.single {
        return Purpose::Other;
    }
    if n.rtk == Rtk::Condensed {
        return Purpose::Condensed;
    }
    if is_diff(p) {
        return Purpose::Diff;
    }
    if let Some(found) = paths(p).or_else(|| search(p)).or_else(|| check(p)) {
        return found;
    }
    if json(p) {
        return Purpose::Json;
    }
    read(p).unwrap_or(Purpose::Other)
}

/// Read the purpose of a command line; see [`Purpose`] and [`normalise`].
pub fn purpose(command: &str) -> Purpose {
    parse(command).map_or(Purpose::Other, |p| purpose_of(&p))
}

// ---------------------------------------------------------------------------
// Read-only commands and patches
// ---------------------------------------------------------------------------

/// A pipe stage that only filters or counts what it is given.
fn read_only_filter(stage: &Stage) -> bool {
    match stage.program() {
        "head" | "grep" | "egrep" | "fgrep" | "rg" | "wc" | "cat" | "nl" | "cut" | "tr" | "column" | "jq" => true,
        "tail" => !tail_follows(stage),
        "sort" => !sort_writes(stage),
        "uniq" => !uniq_writes(stage),
        "sed" => sed_stage(stage).is_some(),
        _ => false,
    }
}

fn sort_writes(stage: &Stage) -> bool {
    stage
        .values()
        .iter()
        .any(|a| a.starts_with("--output") || (a.starts_with('-') && !a.starts_with("--") && a.contains('o')))
}

/// `uniq IN OUT` writes OUT.
fn uniq_writes(stage: &Stage) -> bool {
    stage.values().iter().filter(|a| !a.starts_with('-')).count() > 1
}

/// True when the command certainly changes no file: it is `single`, and it is
/// a read, a search, a list of paths, a patch view, a check run without fix
/// flags, or one of a few programs that only print (`ls`, `wc`, `git status`,
/// `git log`, ...), piped only into filters. Anything unknown is false.
pub fn is_read_only(command: &str) -> bool {
    let Some(p) = parse(command) else {
        return false;
    };
    if !p.norm.single {
        return false;
    }
    let producer = p.producer();
    let values = producer.values();
    // `git diff --output=f`, `git log --output f` write a file; `| xargs rm`
    // does what it likes.
    if (p.program() == "git" && values.iter().any(|a| a.starts_with("--output")))
        || !p.later().iter().all(read_only_filter)
    {
        return false;
    }
    match purpose_of(&p) {
        Purpose::ReadWhole { .. }
        | Purpose::ReadRange { .. }
        | Purpose::Search(_)
        | Purpose::Paths
        | Purpose::Diff
        | Purpose::Check { .. } => return true,
        _ => {}
    }
    let after_sub = p.after_sub();
    let after: Vec<&str> = after_sub.iter().map(|w| w.value.as_str()).collect();
    match p.program() {
        "ls" | "pwd" | "wc" | "echo" | "printf" | "which" | "type" | "file" | "stat" | "du" | "df" | "head" | "cat"
        | "nl" | "printenv" => true,
        "tree" => !values.contains(&"-o"),
        "date" => !values.iter().any(|a| *a == "-s" || a.starts_with("--set")),
        "tail" => !tail_follows(producer),
        "sort" => !sort_writes(producer),
        "uniq" => !uniq_writes(producer),
        "git" => match p.sub() {
            Some("status" | "log" | "show" | "diff" | "ls-files" | "ls-tree" | "rev-parse" | "blame" | "describe") => {
                true
            }
            Some("branch") => {
                let destructive = after.iter().any(|a| {
                    matches!(
                        *a,
                        "-d" | "-D"
                            | "-m"
                            | "-M"
                            | "-c"
                            | "-C"
                            | "-f"
                            | "-u"
                            | "--delete"
                            | "--move"
                            | "--copy"
                            | "--force"
                            | "--edit-description"
                            | "--unset-upstream"
                    ) || a.starts_with("--set-upstream")
                });
                // `git branch NAME` creates a branch unless a listing option is given.
                let listing = after.iter().any(|a| {
                    matches!(
                        *a,
                        "-l" | "--list" | "--contains" | "--no-contains" | "--merged" | "--no-merged" | "--points-at"
                    ) || a.starts_with("--contains=")
                        || a.starts_with("--merged=")
                        || a.starts_with("--points-at=")
                });
                !destructive && (listing || after.iter().all(|a| a.starts_with('-')))
            }
            Some("remote") => after.iter().all(|a| matches!(*a, "-v" | "--verbose")),
            Some("tag") => {
                after.is_empty()
                    || (after.iter().any(|a| matches!(*a, "-l" | "--list"))
                        && !after
                            .iter()
                            .any(|a| matches!(*a, "-d" | "--delete" | "-a" | "-s" | "-f" | "-m" | "-F" | "--force")))
            }
            _ => false,
        },
        _ => false,
    }
}

/// The paths an `apply_patch` patch names: on its `*** Update File:`,
/// `*** Add File:`, `*** Delete File:` and `*** Move to:` lines, in order,
/// each once.
pub fn patch_paths(patch: &str) -> Vec<String> {
    const MARKERS: [&str; 4] = [
        "*** Update File: ",
        "*** Add File: ",
        "*** Delete File: ",
        "*** Move to: ",
    ];
    let mut out: Vec<String> = Vec::new();
    for line in patch.lines() {
        let line = line.trim();
        let Some(path) = MARKERS.iter().find_map(|m| line.strip_prefix(m)) else {
            continue;
        };
        let path = path.trim();
        if !path.is_empty() && !out.iter().any(|p| p == path) {
            out.push(path.to_string());
        }
    }
    out
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
    tokens_lines(script, code).map(|(toks, _)| toks)
}

/// A line terminator, as JavaScript counts them.
fn is_line_break(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

/// [`tokens`], and for each token whether a line break comes before it (in
/// whitespace or a comment): where a statement may end without a `;`.
fn tokens_lines(script: &str, code: &mut String) -> Option<(Vec<Tok>, Vec<bool>)> {
    let s: Vec<char> = script.chars().collect();
    let mut out: Vec<Tok> = Vec::new();
    let mut lines: Vec<bool> = Vec::new();
    let mut line_break = false;
    let mut i = 0;
    while i < s.len() {
        if out.len() > lines.len() {
            lines.push(line_break);
            line_break = false;
        }
        let c = s[i];
        i += 1;
        if c.is_whitespace() {
            line_break |= is_line_break(c);
            continue;
        }
        match c {
            '/' if s.get(i) == Some(&'/') => {
                while i < s.len() && !is_line_break(s[i]) {
                    i += 1;
                }
            }
            '/' if s.get(i) == Some(&'*') => {
                let close = s[i + 1..].windows(2).position(|w| w == ['*', '/'])?;
                line_break |= s[i + 1..i + 1 + close].iter().any(|c| is_line_break(*c));
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
    if out.len() > lines.len() {
        lines.push(line_break);
    }
    Some((out, lines))
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
/// `)` that closes the call after it, whose index comes back with the command.
fn call_arguments(toks: &[Tok], start: usize) -> Option<(ScriptCommand, usize)> {
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
    Some((ScriptCommand { cmd, workdir }, i))
}

/// Ways to reach a tool or the output that counting identifiers would not see:
/// an escaped identifier (`\u0074ools`), code in a string (`eval`,
/// `Function`, `import()`), or one of `names` written as a string
/// (`globalThis["tools"]`).
fn hidden(toks: &[Tok], names: &[&str]) -> bool {
    toks.iter().any(|t| match t {
        Tok::Punct(p) => p == "\\",
        Tok::Ident(w) => matches!(w.as_str(), "eval" | "Function" | "import"),
        Tok::Str(Some(v)) => names.contains(&v.as_str()),
        _ => false,
    })
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
    if hidden(&toks, &["tools", "exec_command"]) {
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
    call_arguments(&toks, at + 2).map(|(c, _)| c)
}

// ---------------------------------------------------------------------------
// Code mode: scripts that run several commands
// ---------------------------------------------------------------------------

/// What a code-mode script runs, and which command each printed part is from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptPlan {
    /// The commands, in source order.
    pub commands: Vec<ScriptCommand>,
    /// For a script with several commands: `prints[i]` is the index in
    /// `commands` of the command whose output the i-th `text()` call prints,
    /// in source order. Codex returns each `text()` call as its own
    /// `input_text` part after the status part. `None` for a script with one
    /// command, where every printed part is that command's.
    pub prints: Option<Vec<usize>>,
    /// See [`script_edits`].
    pub edits: bool,
}

/// The commands a code-mode script runs and which `text()` call prints each,
/// when that is certain.
///
/// * One `exec_command` call, read as [`script_command`] reads it: that call,
///   and `prints` is `None`.
/// * Several calls: only when every call has a literal `cmd` and runs once at
///   the top level, each as the initialiser of a top-level
///   `const|let|var NAME = await tools.exec_command({...})` or as an element
///   of the array of a top-level
///   `const|let|var [A, B] = await Promise.all([tools.exec_command({...}), ...])`;
///   every `text(` is a top-level statement whose one argument is `NAME.output`
///   or `(await tools.exec_command({...})).output`; each command is printed
///   exactly once; and the script has no other `tools.` call, no `console`
///   and no other use of `text` or of the names bound to the results.
pub fn script_plan(script: &str) -> Option<ScriptPlan> {
    let edits = script_edits(script);
    if let Some(one) = script_command(script) {
        return Some(ScriptPlan {
            commands: vec![one],
            prints: None,
            edits,
        });
    }
    let (commands, prints) = several_commands(script)?;
    Some(ScriptPlan {
        commands,
        prints: Some(prints),
        edits,
    })
}

/// The identifiers in a piece of script text.
fn identifiers(code: &str) -> Vec<&str> {
    code.split(|c: char| !is_ident_char(c))
        .filter(|w| !w.is_empty())
        .collect()
}

/// The bracket depth before each token and, for each closing bracket, the
/// index of the bracket it closes. `None` when the brackets do not balance.
type Brackets = (Vec<usize>, Vec<Option<usize>>);

fn brackets(toks: &[Tok]) -> Option<Brackets> {
    let mut stack: Vec<(usize, &str)> = Vec::new();
    let mut depth = Vec::with_capacity(toks.len());
    let mut opener = vec![None; toks.len()];
    for (k, t) in toks.iter().enumerate() {
        depth.push(stack.len());
        let Tok::Punct(p) = t else { continue };
        match p.as_str() {
            "(" | "[" | "{" => stack.push((k, p.as_str())),
            ")" | "]" | "}" => {
                let (o, open) = stack.pop()?;
                if !matches!((open, p.as_str()), ("(", ")") | ("[", "]") | ("{", "}")) {
                    return None;
                }
                opener[k] = Some(o);
            }
            _ => {}
        }
    }
    stack.is_empty().then_some((depth, opener))
}

/// One `tools.exec_command({...})` call: the index of `tools`, of the `)`
/// that closes the call, and the command.
struct Call {
    start: usize,
    close: usize,
    cmd: ScriptCommand,
}

fn is_declaration(t: Option<&Tok>) -> bool {
    matches!(t, Some(Tok::Ident(w)) if matches!(w.as_str(), "const" | "let" | "var"))
}

/// `const [A, B] = await Promise.all([` with the statement at `i`: the bound
/// names with their token indexes, and the index of the array's `[`.
fn promise_all_head(toks: &[Tok], i: usize) -> Option<(Vec<(String, usize)>, usize)> {
    if !is_declaration(toks.get(i)) || !punct(toks.get(i + 1), "[") {
        return None;
    }
    let mut j = i + 2;
    let mut names = Vec::new();
    loop {
        let Some(Tok::Ident(name)) = toks.get(j) else {
            return None;
        };
        names.push((name.clone(), j));
        j += 1;
        if punct(toks.get(j), ",") {
            j += 1;
            if punct(toks.get(j), "]") {
                break;
            }
            continue;
        }
        if punct(toks.get(j), "]") {
            break;
        }
        return None;
    }
    let rest = [
        Tok::Punct("]".into()),
        Tok::Punct("=".into()),
        Tok::Ident("await".into()),
        Tok::Ident("Promise".into()),
        Tok::Punct(".".into()),
        Tok::Ident("all".into()),
        Tok::Punct("(".into()),
        Tok::Punct("[".into()),
    ];
    for want in &rest {
        if toks.get(j) != Some(want) {
            return None;
        }
        j += 1;
    }
    Some((names, j - 1))
}

/// [`script_plan`] for a script with several commands: the commands and, for
/// each `text()` call in source order, the index of the command it prints.
fn several_commands(script: &str) -> Option<(Vec<ScriptCommand>, Vec<usize>)> {
    let mut code = String::new();
    let (toks, lines) = tokens_lines(script, &mut code)?;
    // Code inside a `${}` is not followed: it must not reach a tool or the output.
    let in_templates = identifiers(&code);
    if in_templates
        .iter()
        .any(|w| matches!(*w, "tools" | "exec_command" | "text" | "console"))
    {
        return None;
    }
    if hidden(&toks, &["tools", "exec_command", "text", "console"]) || toks.iter().any(|t| ident(Some(t), "console")) {
        return None;
    }
    let (depth, opener) = brackets(&toks)?;
    // A `)` that closes the head of `if (..)`, `while (..)`, `for (..)`: what
    // follows is its body, not a statement of its own.
    let closes_header = |j: usize| {
        opener[j]
            .and_then(|o| o.checked_sub(1))
            .is_some_and(|k| matches!(&toks[k], Tok::Ident(w) if matches!(w.as_str(), "if" | "while" | "for" | "with")))
    };
    // A statement starts at `i`, at the top level.
    let starts = |i: usize| -> bool {
        if depth[i] != 0 {
            return false;
        }
        let Some(k) = i.checked_sub(1) else {
            return true;
        };
        match &toks[k] {
            Tok::Punct(p) if p == ";" || p == "}" => true,
            Tok::Punct(p) if p == ")" => lines[i] && !closes_header(k),
            Tok::Punct(p) if p == "]" => lines[i],
            Tok::Ident(w) => lines[i] && !BEFORE_REGEX.contains(&w.as_str()),
            Tok::Str(_) | Tok::Other => lines[i],
            Tok::Punct(_) => false,
        }
    };
    // The statement that ends just before `j` is not continued at `j`.
    let ends = |j: usize| -> bool {
        match toks.get(j) {
            None => true,
            Some(Tok::Punct(p)) => p == ";",
            Some(Tok::Ident(w)) => lines[j] && !matches!(w.as_str(), "in" | "instanceof" | "of"),
            Some(_) => false,
        }
    };

    let mut calls: Vec<Call> = Vec::new();
    for (k, t) in toks.iter().enumerate() {
        if !ident(Some(t), "exec_command") {
            continue;
        }
        let start = k.checked_sub(2)?;
        if !punct(toks.get(k - 1), ".") || !ident(toks.get(start), "tools") || !punct(toks.get(k + 1), "(") {
            return None;
        }
        if !punct(toks.get(k + 2), "{") || !runs_once(&toks, start) {
            return None;
        }
        let (cmd, close) = call_arguments(&toks, k + 2)?;
        calls.push(Call { start, close, cmd });
    }
    if calls.len() < 2 || toks.iter().filter(|t| ident(Some(*t), "tools")).count() != calls.len() {
        return None;
    }
    let call_at = |start: usize| calls.iter().position(|c| c.start == start);

    let mut placed = vec![false; calls.len()];
    // name -> (call, index of the name's token)
    let mut bound: Vec<(String, usize, usize)> = Vec::new();
    // (index of `text`, call)
    let mut prints: Vec<(usize, usize)> = Vec::new();
    for (c, call) in calls.iter().enumerate() {
        let s = call.start;
        let back = |n: usize| s.checked_sub(n).and_then(|k| toks.get(k));
        let declared = s >= 4
            && is_declaration(back(4))
            && punct(back(2), "=")
            && ident(back(1), "await")
            && starts(s - 4)
            && ends(call.close + 1);
        if let (true, Some(Tok::Ident(name))) = (declared, back(3)) {
            bound.push((name.clone(), c, s - 3));
            placed[c] = true;
            continue;
        }
        // text((await tools.exec_command({...})).output)
        let after = |n: usize| toks.get(call.close + n);
        let inline = s >= 4
            && ident(back(4), "text")
            && punct(back(3), "(")
            && punct(back(2), "(")
            && ident(back(1), "await")
            && punct(after(1), ")")
            && punct(after(2), ".")
            && ident(after(3), "output")
            && punct(after(4), ")")
            && starts(s - 4)
            && ends(call.close + 5);
        if inline {
            prints.push((s - 4, c));
            placed[c] = true;
        }
    }
    for i in 0..toks.len() {
        let Some((names, array)) = promise_all_head(&toks, i).filter(|_| starts(i)) else {
            continue;
        };
        let mut elements = Vec::new();
        let mut pos = array + 1;
        while !punct(toks.get(pos), "]") {
            let c = call_at(pos)?;
            elements.push(c);
            pos = calls[c].close + 1;
            if punct(toks.get(pos), ",") {
                pos += 1;
            }
        }
        if !punct(toks.get(pos + 1), ")") || !ends(pos + 2) || names.len() != elements.len() {
            return None;
        }
        for ((name, at), c) in names.into_iter().zip(elements) {
            bound.push((name, c, at));
            placed[c] = true;
        }
    }
    if placed.iter().any(|p| !p) {
        return None;
    }
    for (k, (name, _, _)) in bound.iter().enumerate() {
        if bound[..k].iter().any(|(other, _, _)| other == name) || in_templates.contains(&name.as_str()) {
            return None;
        }
    }

    // text(NAME.output)
    let inline_heads: Vec<usize> = prints.iter().map(|(t, _)| *t).collect();
    for (t, tok) in toks.iter().enumerate() {
        if !ident(Some(tok), "text") || inline_heads.contains(&t) {
            continue;
        }
        let Some(Tok::Ident(name)) = toks.get(t + 2) else {
            return None;
        };
        let (_, c, declared_at) = bound.iter().find(|(n, _, _)| n == name)?;
        let ok = starts(t)
            && *declared_at < t
            && punct(toks.get(t + 1), "(")
            && punct(toks.get(t + 3), ".")
            && ident(toks.get(t + 4), "output")
            && punct(toks.get(t + 5), ")")
            && ends(t + 6)
            && runs_once(&toks, t);
        if !ok {
            return None;
        }
        prints.push((t, *c));
    }
    // A bound name is used only where it is declared and printed.
    for (name, _, _) in &bound {
        let uses = toks.iter().filter(|t| ident(Some(*t), name)).count();
        let printed = prints
            .iter()
            .filter(|(t, _)| !inline_heads.contains(t) && ident(toks.get(t + 2), name))
            .count();
        if uses != 1 + printed {
            return None;
        }
    }
    prints.sort_unstable();
    let order: Vec<usize> = prints.into_iter().map(|(_, c)| c).collect();
    let mut seen = order.clone();
    seen.sort_unstable();
    if seen != (0..calls.len()).collect::<Vec<_>>() {
        return None;
    }
    Some((calls.into_iter().map(|c| c.cmd).collect(), order))
}

/// True when a code-mode script may change files: it mentions `apply_patch`,
/// calls any tool other than `exec_command` (or reaches tools in a way this
/// reader does not follow), runs a command that is not a literal, or runs a
/// command that is not [`is_read_only`]. A script that cannot be read is
/// taken to change files.
pub fn script_edits(script: &str) -> bool {
    if script.contains("apply_patch") {
        return true;
    }
    let mut code = String::new();
    let Some(toks) = tokens(script, &mut code) else {
        return true;
    };
    if code.contains("tools") || code.contains("exec_command") || hidden(&toks, &["tools", "exec_command"]) {
        return true;
    }
    let read_only_call = |open: usize| {
        punct(toks.get(open - 1), "(")
            && punct(toks.get(open), "{")
            && call_arguments(&toks, open).is_some_and(|(c, _)| is_read_only(&c.cmd))
    };
    for (k, t) in toks.iter().enumerate() {
        if ident(Some(t), "tools") {
            let ok = punct(toks.get(k + 1), ".") && ident(toks.get(k + 2), "exec_command") && read_only_call(k + 4);
            if !ok {
                return true;
            }
        } else if ident(Some(t), "exec_command") {
            let qualified = k >= 2 && punct(toks.get(k - 1), ".") && ident(toks.get(k - 2), "tools");
            if !qualified && !read_only_call(k + 2) {
                return true;
            }
        }
    }
    false
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

    fn check(tool: &str, detail: bool) -> Purpose {
        Purpose::Check {
            tool: tool.into(),
            detail,
        }
    }

    fn is_search(command: &str) -> bool {
        matches!(purpose(command), Purpose::Search(_))
    }

    fn search_args(command: &str) -> SearchArgs {
        match purpose(command) {
            Purpose::Search(args) => args,
            other => panic!("{command}: {other:?}"),
        }
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
            assert!(is_search(search), "{search}");
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
        let whole = |path: &str, numbered: bool| Purpose::ReadWhole {
            path: path.into(),
            numbered,
        };
        let range = |path: &str, start: usize, end: usize, numbered: bool| Purpose::ReadRange {
            path: path.into(),
            start,
            end,
            from_top: start == 1,
            numbered,
        };
        assert_eq!(purpose("cat src/app.py"), whole("src/app.py", false));
        assert_eq!(purpose("cd svc && cat 'my file.py'"), whole("my file.py", false));
        assert_eq!(purpose("cd a; cat b"), whole("b", false));
        assert_eq!(purpose("cat -n a.py"), whole("a.py", true));
        assert_eq!(purpose("nl -ba a.py"), whole("a.py", true));
        assert_eq!(purpose("nl -b a a.py"), whole("a.py", true));
        assert_eq!(purpose("sed -n '10,40p' a/b.ts"), range("a/b.ts", 10, 40, false));
        assert_eq!(purpose("sed -n 1,200p a.py"), range("a.py", 1, 200, false));
        assert_eq!(purpose("sed -n '7p' a.py 2>/dev/null"), range("a.py", 7, 7, false));
        assert_eq!(purpose("head -n 80 a.py"), range("a.py", 1, 80, false));
        assert_eq!(purpose("head -80 a.py"), range("a.py", 1, 80, false));
        assert_eq!(purpose("head a.py -n 80"), range("a.py", 1, 80, false));
        assert_eq!(purpose("nl -ba f | sed -n '1,200p'"), range("f", 1, 200, true));
        assert_eq!(
            purpose("nl -ba src/x.rs | sed -n '120,180p'"),
            range("src/x.rs", 120, 180, true)
        );
        assert_eq!(purpose("cat -n f | sed -n '5,9p'"), range("f", 5, 9, true));
        assert_eq!(purpose("nl -ba f | head -n 40"), range("f", 1, 40, true));
        assert_eq!(purpose("tail -n +50 f | head -n 20"), range("f", 50, 69, false));
        assert_eq!(purpose("LGE_RAW=1 cat a.py"), Purpose::Raw);
        for other in [
            "cat a.py b.py",
            "cat a.py | head",
            "cat a.py > out",
            "cat a.py >/dev/null",
            "cat src/*.py",
            "cat {a,b}.py",
            "cat $FILE",
            "cat -",
            "sed -n '/def/p' a.py",
            "sed -n '40,10p' a.py",
            "sed -i 's/a/b/' a.py",
            "nl a.py",
            "nl -ba a.py | sed -n '1,9p' | head",
            "tail -n 50 f | head -n 20",
            "tail -n +50 f",
            "head -c 100 a.py",
            "cat 'unterminated",
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
            "git -C repo grep x",
            "git --no-pager -c color.ui=never grep -n x",
            "rtk proxy rg -n x src",
            "rtk bypass grep -rn x .",
            "ag create_order",
        ] {
            assert!(is_search(search), "{search}");
        }
        assert_eq!(purpose("pytest -q tests/test_a.py"), check("pytest", false));
        assert_eq!(purpose("cd web && npx tsc --noEmit"), check("tsc", false));
        assert_eq!(purpose("cargo test -p lge-core | cat"), check("cargo", false));
        assert_eq!(
            purpose("cargo test -p lge-core 2>&1 | tail -20"),
            Purpose::Other,
            "a cut run is judged by its content"
        );
        assert_eq!(
            purpose("npm run build"),
            Purpose::Other,
            "an unknown runner is left alone"
        );
        assert_eq!(purpose("ls -la"), Purpose::Other);
        assert_eq!(purpose("rtk cargo test"), Purpose::Condensed);
        assert_eq!(purpose("LGE_RAW=1 cargo test"), Purpose::Raw);
        assert_eq!(purpose("cd a && LGE_RAW=1 rg -n x src"), Purpose::Raw);
        assert_eq!(purpose("rg --json foo"), Purpose::Json);
        for other in [
            "rg -c foo",
            "grep -q foo a.py",
            "rg --count foo",
            "grep -rlZ foo .",
            "rg -0 foo",
        ] {
            assert_eq!(purpose(other), Purpose::Other, "{other}");
        }
    }

    #[test]
    fn search_arguments_are_read_from_the_flags() {
        let args = search_args("rg -n -g '*.rs' -A 3 create_order src");
        assert_eq!(args.pattern.as_deref(), Some("create_order"));
        assert!(!args.fixed && !args.ignore_case && !args.smart_case && !args.word);
        let args = search_args("rg -nFiw -e 'a.b(' src");
        assert_eq!(args.pattern.as_deref(), Some("a.b("));
        assert!(args.fixed && args.ignore_case && args.word);
        let args = search_args("rg --smart-case --regexp=Order src");
        assert_eq!(args.pattern.as_deref(), Some("Order"));
        assert!(args.smart_case);
        assert!(search_args("fgrep x .").fixed);
        assert!(search_args("grep --fixed-strings -rn 'x[' .").fixed);
        assert_eq!(search_args("grep -rn -f pats.txt src").pattern, None);
        assert_eq!(
            search_args("grep -rn --include='*.py' -m 5 'def run' .")
                .pattern
                .as_deref(),
            Some("def run")
        );
        assert_eq!(
            search_args("git grep -n -e handler -- src").pattern.as_deref(),
            Some("handler")
        );
        let args = search_args("rg -i -s foo");
        assert!(!args.ignore_case, "rg's -s turns case sensitivity back on");
    }

    #[test]
    fn leading_statements_that_print_nothing_are_dropped() {
        for command in [
            "cd a && export X=1 && pytest -q",
            "set -euo pipefail; cargo test",
            "set -e\ncd /repo\npytest",
            "export A=1 B='two words'; CI=1 pytest",
            "A=1 B=2; pytest",
            "set -o pipefail && pytest",
            "# run the tests\npytest",
            "env CI=1 pytest",
        ] {
            assert!(matches!(purpose(command), Purpose::Check { .. }), "{command}");
        }
        for command in [
            "cd - && pytest",
            "set -x; pytest",
            "set -v; pytest",
            "set -o; pytest",
            "export A=$(x) && pytest",
            "export A=$HOME && pytest",
            "export && pytest",
            "pytest; echo done",
            "source .venv/bin/activate && pytest",
            "pytest & sleep 1",
            "(cd a && pytest)",
            "{ pytest; }",
            "pytest |& cat",
            "pytest 2>&1 >log",
            "pytest < input",
            "cat <<EOF\nx\nEOF",
            "diff <(ls a) <(ls b)",
            "pytest `which x`",
            "cd a",
        ] {
            assert_eq!(purpose(command), Purpose::Other, "{command}");
        }
    }

    #[test]
    fn check_runs_need_a_known_subcommand_and_no_fix_flags() {
        for (command, tool) in [
            ("cargo test", "cargo"),
            ("cargo t -p x", "cargo"),
            ("cargo build --release", "cargo"),
            ("cargo +nightly clippy --all-targets", "cargo"),
            ("cargo nextest run", "cargo"),
            ("go test ./...", "go"),
            ("go test -json ./...", "go"),
            ("go vet ./...", "go"),
            ("golangci-lint run ./...", "golangci-lint"),
            ("ruff check .", "ruff"),
            ("ruff src/", "ruff"),
            ("python -m pytest -q", "pytest"),
            ("./gradlew test", "gradlew"),
            ("mvn -q test", "mvn"),
            ("clang -Wall -c a.c", "clang"),
            ("npx jest --ci", "jest"),
            ("pnpm exec vitest run", "vitest"),
            ("uv run pytest -q", "pytest"),
            ("poetry run mypy src", "mypy"),
            ("timeout 600 cargo test", "cargo"),
            (".venv/bin/pytest", "pytest"),
        ] {
            assert_eq!(purpose(command), check(tool, false), "{command}");
        }
        for command in [
            "cargo run --bin report",
            "cargo fmt --check",
            "cargo bench",
            "cargo tree",
            "go run .",
            "go fmt ./...",
            "golangci-lint fmt",
            "ruff format .",
            "ruff check --fix .",
            "eslint --fix src",
            "prettier --write src",
            "cargo test | head -50",
            "pytest | grep FAILED",
            "pytest -q && python scripts/report.py",
            "ls tests/pytest",
            "bash -c pytest",
        ] {
            assert_eq!(purpose(command), Purpose::Other, "{command}");
        }
        for command in [
            "eslint -f json src",
            "eslint --format=json src",
            "jest --json",
            "vitest run --reporter=json",
            "cargo build --message-format=json",
            "ruff check --output-format json .",
            "pyright --outputjson",
            "go vet -json ./...",
        ] {
            assert_eq!(purpose(command), Purpose::Json, "{command}");
        }
        for (command, tool) in [
            ("cargo test -- --nocapture", "cargo"),
            ("cargo test -- --show-output", "cargo"),
            ("pytest -s", "pytest"),
            ("pytest -xvs tests", "pytest"),
            ("pytest --capture=no", "pytest"),
            ("mvn -X test", "mvn"),
            ("./gradlew test --info", "gradlew"),
        ] {
            assert_eq!(purpose(command), check(tool, true), "{command}");
        }
        assert_eq!(purpose("pytest -rs"), check("pytest", false), "-r takes the s");
    }

    #[test]
    fn every_form_of_a_path_list_is_recognised() {
        for command in [
            "rg --files",
            "rg --files src | head -200",
            "rg -l create_order",
            "rg --files-with-matches x src",
            "rg --files-without-match x",
            "grep -rl foo .",
            "grep -lr foo .",
            "grep -Rl foo .",
            "grep -L foo *.py",
            "grep --files-with-matches foo -r .",
            "git ls-files",
            "git ls-files -o --exclude-standard | sort",
            "git ls-tree -r --name-only HEAD",
            "git ls-tree --name-only --full-tree -r HEAD src",
            "fd -e rs",
            "fdfind -t f config",
            "find . -name '*.rs'",
            "find src -type f | grep -v test | sort | uniq | tail -n 40",
        ] {
            assert_eq!(purpose(command), Purpose::Paths, "{command}");
        }
        for command in [
            "find . -exec rm {} ;",
            "find . -name x -delete",
            "find . -print0",
            "find . -ls",
            "fd -x wc -l",
            "fd -l",
            "git ls-files -s",
            "git ls-files --stage",
            "git ls-files -z",
            "rg --files | wc -l",
            "find . | xargs cat",
            "find . | tail -f",
        ] {
            assert_eq!(purpose(command), Purpose::Other, "{command}");
        }
        assert!(is_search("rg -L foo"), "rg's -L follows links");
        assert_eq!(purpose("git ls-tree -l HEAD"), Purpose::Other);
    }

    #[test]
    fn diffs_and_machine_formats_are_recognised() {
        for command in [
            "git diff",
            "git --no-pager diff HEAD~1",
            "git show HEAD",
            "git log -p -3",
            "git log --patch",
            "git stash show -p",
            "git format-patch --stdout HEAD~1",
            "git range-diff a...b",
            "diff -u a b",
            "git diff | head -100",
        ] {
            assert_eq!(purpose(command), Purpose::Diff, "{command}");
        }
        assert_eq!(purpose("git log --oneline -5"), Purpose::Other);
        assert_eq!(purpose("git format-patch HEAD~1"), Purpose::Other);
        for command in [
            "gh api repos/a/b/pulls",
            "gh pr list --json number,title",
            "jq . data.json",
            "curl -s https://example.com/api",
            "curl -s https://example.com/api | jq .",
            "kubectl get pods -o json",
            "kubectl get pods -ojson",
            "oc get pods --output=json",
            "aws ec2 describe-instances",
            "gcloud compute instances list --format=json",
            "az vm list",
            "npm ls --json",
            "pip list --format json",
        ] {
            assert_eq!(purpose(command), Purpose::Json, "{command}");
        }
        for command in [
            "jq -r .name data.json",
            "curl -s https://example.com | jq -r .x",
            "curl -s https://example.com | head",
            "kubectl get pods",
            "aws s3 ls --output text",
            "az vm list -o table",
            "gh pr list",
        ] {
            assert_eq!(purpose(command), Purpose::Other, "{command}");
        }
    }

    #[test]
    fn a_command_is_normalised_to_its_producer() {
        let n = normalise("cd /repo && export CI=1 && RUST_LOG=debug env A=b rtk proxy git -C sub --no-pager log -p 2>&1 | head -n 40 || true").unwrap();
        assert_eq!(n.command, "git log -p | head -n 40");
        assert_eq!(n.program, "git");
        assert_eq!(n.sub.as_deref(), Some("log"));
        assert_eq!(n.args, ["log", "-p"]);
        assert_eq!(n.stages, ["head"]);
        assert!(n.single && !n.raw && n.exit_masked);
        assert_eq!(n.rtk, Rtk::Bypass);

        let n = normalise("LGE_RAW=1 cargo   test  -p 'a b'").unwrap();
        assert_eq!(n.command, "cargo test -p 'a b'");
        assert_eq!(n.args, ["test", "-p", "a b"]);
        assert!(n.raw && n.single && !n.exit_masked);
        assert_eq!(n.rtk, Rtk::No);

        let n = normalise("rtk cargo test").unwrap();
        assert_eq!((n.rtk, n.command.as_str()), (Rtk::Condensed, "cargo test"));

        // A re-run with other harmless statements, spacing or wrappers is the same command.
        let a = normalise("cd a && pytest -q tests").unwrap();
        let b = normalise("set -e; LGE_RAW=1 pytest  -q tests").unwrap();
        assert_eq!(a.command, b.command);

        // set -o pipefail keeps the producer's exit status through a pipe.
        assert!(!normalise("set -o pipefail; cargo test | cat").unwrap().exit_masked);
        assert!(normalise("cargo test | cat").unwrap().exit_masked);
    }

    #[test]
    fn the_exit_status_of_a_chain_is_masked_unless_every_step_must_pass() {
        // `;` and line breaks report the last statement's status; `||` a later
        // statement's; a pipe in any statement its last stage's.
        for command in [
            "cargo fmt --check; cargo test",
            "cargo fmt --check\ncargo test",
            "black --check .; pytest -v",
            "cargo test || echo failed",
            "cargo build && cargo test | tail -n 20",
            "cd /repo && cargo fmt --check; cargo test",
        ] {
            assert!(normalise(command).unwrap().exit_masked, "{command}");
        }
        // `&&` stops at the first failure, so 0 means every step passed; dropped
        // leading statements change nothing.
        for command in [
            "cargo build && cargo test",
            "cd /repo && cargo test",
            "cd /repo; cargo test",
            "set -o pipefail; cargo test | tail -n 20",
            "export CI=1\ncargo test",
        ] {
            assert!(!normalise(command).unwrap().exit_masked, "{command}");
        }
    }

    #[test]
    fn lge_raw_in_a_chain_asks_for_the_output_whole() {
        for command in [
            "export LGE_RAW=1; cargo fmt --check; cargo test",
            "LGE_RAW=1 cargo test 2>&1 | tail -n 200; echo done",
            "cargo build; LGE_RAW=1 cargo test",
            "cargo build && env LGE_RAW=1 cargo test",
        ] {
            assert_eq!(purpose(command), Purpose::Raw, "{command}");
        }
        assert_eq!(purpose("cargo fmt --check; cargo test"), Purpose::Other);

        let n = normalise("pytest; echo done").unwrap();
        assert!(!n.single);
        assert_eq!(n.command, "pytest ; echo done");
        assert_eq!(normalise("echo 'open"), None);
        assert_eq!(normalise("echo \"open"), None);
        assert!(!normalise("").unwrap().single);
    }

    #[test]
    fn read_only_commands_are_told_apart_from_the_rest() {
        for command in [
            "cat a.py",
            "nl -ba a.py | sed -n '1,80p'",
            "rg -n foo src",
            "rg --files | head",
            "git diff",
            "pytest -q",
            "cargo build",
            "ls -la src",
            "pwd",
            "wc -l a.py b.py",
            "echo hi",
            "tree src | head -50",
            "git status",
            "git log --oneline | head",
            "git rev-parse HEAD",
            "git branch",
            "git branch -a",
            "git branch --list 'feat*'",
            "git remote -v",
            "git tag",
            "git -C sub status",
            "cd a && ls",
            "tail -n 20 log.txt",
            "sort a.txt | uniq -c",
        ] {
            assert!(is_read_only(command), "{command}");
        }
        for command in [
            "rm -rf build",
            "touch a",
            "echo x > a.py",
            "mkdir -p a && ls",
            "git commit -m x",
            "git checkout main",
            "git branch new-feature",
            "git branch -D old",
            "git remote add origin x",
            "git tag v1.0",
            "git diff --output=patch.txt",
            "sed -i 's/a/b/' a.py",
            "eslint --fix src",
            "cargo run",
            "npm install",
            "tail -f log.txt",
            "sort -o out.txt a.txt",
            "uniq a.txt b.txt",
            "find . -delete",
            "cat a | tee b",
            "ls | xargs rm",
            "git diff --name-only | xargs rm",
            "rg -l foo | sort -o out.txt",
            "python script.py",
            "cat 'open",
            "ls; rm a",
        ] {
            assert!(!is_read_only(command), "{command}");
        }
    }

    #[test]
    fn a_patch_names_its_paths() {
        let patch = "*** Begin Patch\n*** Update File: src/a.rs\n*** Move to: src/b.rs\n@@\n-x\n+y\n*** Add File: docs/new.md\n+*** Update File: not/a/header.rs\n*** Delete File: old.txt\n*** Update File: src/a.rs\n*** End Patch\n";
        assert_eq!(patch_paths(patch), ["src/a.rs", "src/b.rs", "docs/new.md", "old.txt"]);
        assert!(patch_paths("no patch here").is_empty());
    }

    fn plan(script: &str) -> Option<(Vec<String>, Option<Vec<usize>>)> {
        script_plan(script).map(|p| (p.commands.into_iter().map(|c| c.cmd).collect(), p.prints))
    }

    #[test]
    fn scripts_with_several_commands_say_which_part_each_prints() {
        let want = |cmds: &[&str], prints: &[usize]| {
            Some((
                cmds.iter().map(|c| c.to_string()).collect::<Vec<_>>(),
                Some(prints.to_vec()),
            ))
        };
        // Two sequential awaits, printed in the other order.
        assert_eq!(
            plan("const a = await tools.exec_command({cmd: \"rg -n foo src\"});\nconst b = await tools.exec_command({cmd: 'pytest -q', workdir: '/r'});\ntext(b.output);\ntext(a.output);\n"),
            want(&["rg -n foo src", "pytest -q"], &[1, 0])
        );
        // Without semicolons: the line breaks end the statements.
        assert_eq!(
            plan("const a = await tools.exec_command({cmd: 'ls'})\nlet b = await tools.exec_command({cmd: 'pwd'})\ntext(a.output)\ntext(b.output)"),
            want(&["ls", "pwd"], &[0, 1])
        );
        // Promise.all with array destructuring.
        assert_eq!(
            plan("const [x, y, z] = await Promise.all([\n  tools.exec_command({cmd: \"sed -n '1,80p' a.py\"}),\n  tools.exec_command({cmd: 'rg -n bar'}),\n  tools.exec_command({cmd: 'cargo test'}),\n]);\ntext(x.output);\ntext(y.output);\ntext(z.output);"),
            want(&["sed -n '1,80p' a.py", "rg -n bar", "cargo test"], &[0, 1, 2])
        );
        // An inline await inside text(), beside a declared one.
        assert_eq!(
            plan("text((await tools.exec_command({cmd: 'git status'})).output);\nconst d = await tools.exec_command({cmd: 'git diff'});\ntext(d.output);"),
            want(&["git status", "git diff"], &[0, 1])
        );
        // Other code that touches neither the tools nor the output is fine.
        assert_eq!(
            plan("// two reads\nconst n = 2;\nconst a = await tools.exec_command({cmd: 'cat a'});\nif (n > 1) { const m = n * 2; }\nconst b = await tools.exec_command({cmd: 'cat b'});\ntext(a.output);\ntext(b.output);"),
            want(&["cat a", "cat b"], &[0, 1])
        );
        // One command keeps the v0.0.1 reading: every part is its own.
        assert_eq!(
            plan("const r = await tools.exec_command({cmd: 'ls'}); text(r.output);"),
            Some((vec!["ls".to_string()], None))
        );
    }

    #[test]
    fn scripts_whose_parts_cannot_be_told_apart_give_no_plan() {
        for script in [
            // A call in a loop or a callback.
            "for (const f of fs) { const a = await tools.exec_command({cmd: 'ls'}); text(a.output); }\nconst b = await tools.exec_command({cmd: 'pwd'}); text(b.output);",
            "const rs = await Promise.all(['a', 'b'].map((f) => tools.exec_command({cmd: 'ls'})));",
            "const a = await tools.exec_command({cmd: 'ls'});\nconst b = await tools.exec_command({cmd: 'pwd'}).then((r) => r);\ntext(a.output); text(b.output);",
            // text() inside an if, or as the body of one.
            "const a = await tools.exec_command({cmd: 'ls'});\nconst b = await tools.exec_command({cmd: 'pwd'});\ntext(a.output);\nif (b.exit_code === 0) { text(b.output); }",
            "const a = await tools.exec_command({cmd: 'ls'});\nconst b = await tools.exec_command({cmd: 'pwd'});\ntext(a.output);\nif (ok)\ntext(b.output)",
            // A command printed twice, or not at all.
            "const a = await tools.exec_command({cmd: 'ls'});\nconst b = await tools.exec_command({cmd: 'pwd'});\ntext(a.output);\ntext(a.output);\ntext(b.output);",
            "const a = await tools.exec_command({cmd: 'ls'});\nconst b = await tools.exec_command({cmd: 'pwd'});\ntext(a.output);",
            // Something else printed, or the output used another way.
            "const a = await tools.exec_command({cmd: 'ls'});\nconst b = await tools.exec_command({cmd: 'pwd'});\ntext(a.output);\ntext(b.output);\ntext('done');",
            "const a = await tools.exec_command({cmd: 'ls'});\nconst b = await tools.exec_command({cmd: 'pwd'});\ntext(a.output);\ntext(b.output.slice(0, 100));",
            "const a = await tools.exec_command({cmd: 'ls'});\nconst b = await tools.exec_command({cmd: 'pwd'});\nconsole.log(a.output);\ntext(b.output);",
            "const a = await tools.exec_command({cmd: 'ls'});\nconst b = await tools.exec_command({cmd: 'pwd'});\ntext(a.output);\ntext(b.output);\nconst n = a.exit_code;",
            "let a = await tools.exec_command({cmd: 'ls'});\nconst b = await tools.exec_command({cmd: 'pwd'});\na = b;\ntext(a.output);\ntext(b.output);",
            "const a = await tools.exec_command({cmd: 'ls'});\nconst b = await tools.exec_command({cmd: 'pwd'});\ntext(a.output);\ntext(`${b.output}`);",
            // Printed before it is declared.
            "text(b.output);\nconst a = await tools.exec_command({cmd: 'ls'});\nconst b = await tools.exec_command({cmd: 'pwd'});\ntext(a.output);",
            // A computed command.
            "const a = await tools.exec_command({cmd: 'ls ' + dir});\nconst b = await tools.exec_command({cmd: 'pwd'});\ntext(a.output); text(b.output);",
            "const a = await tools.exec_command({cmd: `ls ${dir}`});\nconst b = await tools.exec_command({cmd: 'pwd'});\ntext(a.output); text(b.output);",
            // Another tool call.
            "const a = await tools.exec_command({cmd: 'ls'});\nconst b = await tools.exec_command({cmd: 'pwd'});\nawait tools.view_image({path: 'x.png'});\ntext(a.output); text(b.output);",
            // Destructuring that does not match the calls.
            "const [a] = await Promise.all([tools.exec_command({cmd: 'ls'}), tools.exec_command({cmd: 'pwd'})]);\ntext(a.output);",
            "const [a, , b] = await Promise.all([tools.exec_command({cmd: 'ls'}), tools.exec_command({cmd: 'pwd'})]);\ntext(a.output); text(b.output);",
            // A call whose result is not bound.
            "await tools.exec_command({cmd: 'mkdir x'});\nconst b = await tools.exec_command({cmd: 'pwd'});\ntext(b.output);",
            // Hidden calls.
            "const a = await tools.exec_command({cmd: 'ls'});\nconst b = await tools.exec_command({cmd: 'pwd'});\nglobalThis['text'](a.output);\ntext(b.output);",
        ] {
            assert_eq!(plan(script), None, "{script}");
        }
    }

    #[test]
    fn a_script_edits_when_it_may_change_a_file() {
        assert!(!script_edits(
            "const r = await tools.exec_command({cmd: 'rg -n foo src'}); text(r.output);"
        ));
        assert!(!script_edits("const [a, b] = await Promise.all([tools.exec_command({cmd: 'cat a'}), tools.exec_command({cmd: 'git status'})]);"));
        assert!(!script_edits("text('no tools at all');"));
        for script in [
            "await tools.apply_patch({input: '*** Begin Patch'});",
            "const p = 'apply_patch';",
            "await tools.exec_command({cmd: 'rm -rf build'});",
            "await tools.exec_command({cmd: 'cargo run'});",
            "await tools.exec_command({cmd: 'ls ' + dir});",
            "await tools.exec_command({cmd: `ls ${dir}`});",
            "await tools.view_image({path: 'a.png'});",
            "const t = tools; await t.exec_command({cmd: 'ls'});",
            "await globalThis['tools'].exec_command({cmd: 'ls'});",
            "await exec_command({cmd: 'touch a'});",
            "await tools.exec_command({cmd: 'ls",
        ] {
            assert!(script_edits(script), "{script}");
        }
        let plan = script_plan("const a = await tools.exec_command({cmd: 'ls'});\nconst b = await tools.exec_command({cmd: 'touch x'});\ntext(a.output);\ntext(b.output);").unwrap();
        assert!(plan.edits);
    }
}
