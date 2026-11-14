//! Helpers shared by the language extractors: table names in SQL text, string
//! templates, URL shapes, route joining and fact records.

use lge_core::{stable_id, FactSite};
use std::collections::HashSet;

// ---------------------------------------------------------------------------
// SQL
// ---------------------------------------------------------------------------

/// Table names in `sql` and whether the statement writes them.
///
/// Understands `SELECT ... FROM t`, `JOIN t`, `INSERT INTO t`, `UPDATE t SET` and
/// `DELETE FROM t`, in any letter case. A schema-qualified name keeps only the
/// table, a quoted name loses its quotes, and names of common table expressions
/// are left out. Text counts as SQL only when a statement keyword starts it (or
/// follows a semicolon), so a sentence that merely contains "from" gives nothing.
///
/// The result is in order of first appearance, without duplicates.
pub fn sql_tables(sql: &str) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = Vec::new();
    if !may_be_sql(sql) {
        return out;
    }
    let toks = tokenize(sql);
    for statement in toks.split(|t| *t == Tok::Semi) {
        for found in scan_statement(statement) {
            if !out.contains(&found) {
                out.push(found);
            }
        }
    }
    out
}

/// Cheap test that keeps ordinary strings away from the tokenizer.
fn may_be_sql(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    ["from", "into", "update"].iter().any(|k| lower.contains(k))
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Word(String),
    /// An identifier written in double quotes, backticks or square brackets.
    Quoted(String),
    Dot,
    Comma,
    Open,
    Close,
    Semi,
    /// Operators, numbers, string literals and placeholders: never a name.
    Other,
}

fn tokenize(sql: &str) -> Vec<Tok> {
    let chars: Vec<char> = sql.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if c.is_whitespace() {
            i += 1;
        } else if c == '-' && next == Some('-') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && next == Some('*') {
            i += 2;
            while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                i += 1;
            }
            i += 2;
        } else if c == '\'' {
            i = skip_string_literal(&chars, i);
            toks.push(Tok::Other);
        } else if c == '"' || c == '`' {
            let (name, end) = read_until(&chars, i + 1, c);
            // `"{0}"` and `"%s"` are placeholders in quotes, not names.
            let placeholder = name.contains(['{', '}', '%', '?', '$']);
            toks.push(if placeholder { Tok::Other } else { Tok::Quoted(name) });
            i = end;
        } else if c == '[' {
            // `[dbo].[Orders]` is a quoted name; `arr[1]` is not.
            let (name, end) = read_until(&chars, i + 1, ']');
            if !name.is_empty()
                && name.chars().all(|ch| ch.is_alphanumeric() || ch == '_' || ch == ' ')
                && !starts_with_digit(&name)
            {
                toks.push(Tok::Quoted(name));
                i = end;
            } else {
                toks.push(Tok::Other);
                i += 1;
            }
        } else if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$') {
                i += 1;
            }
            toks.push(Tok::Word(chars[start..i].iter().collect()));
        } else if c.is_ascii_digit() {
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '.' || chars[i] == '_') {
                i += 1;
            }
            toks.push(Tok::Other);
        } else {
            toks.push(match c {
                '.' => Tok::Dot,
                ',' => Tok::Comma,
                '(' => Tok::Open,
                ')' => Tok::Close,
                ';' => Tok::Semi,
                _ => Tok::Other,
            });
            i += 1;
        }
    }
    toks
}

fn starts_with_digit(s: &str) -> bool {
    s.chars().next().is_some_and(|c| c.is_ascii_digit())
}

/// Index after the single-quoted literal that starts at `start`.
fn skip_string_literal(chars: &[char], start: usize) -> usize {
    let mut i = start + 1;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 2,
            '\'' if chars.get(i + 1) == Some(&'\'') => i += 2,
            '\'' => return i + 1,
            _ => i += 1,
        }
    }
    chars.len()
}

/// Text up to `close` and the index after it.
fn read_until(chars: &[char], start: usize, close: char) -> (String, usize) {
    let mut i = start;
    while i < chars.len() && chars[i] != close {
        i += 1;
    }
    (chars[start..i].iter().collect(), (i + 1).min(chars.len()))
}

/// Words that cannot be an unquoted table name or a table alias.
const KEYWORDS: &[&str] = &[
    "ALL",
    "AND",
    "AS",
    "BY",
    "CASE",
    "CROSS",
    "DEFAULT",
    "DELETE",
    "DISTINCT",
    "DUAL",
    "ELSE",
    "END",
    "EXCEPT",
    "EXISTS",
    "FETCH",
    "FOR",
    "FROM",
    "FULL",
    "GROUP",
    "HAVING",
    "IN",
    "INNER",
    "INSERT",
    "INTERSECT",
    "INTO",
    "IS",
    "JOIN",
    "LATERAL",
    "LEFT",
    "LIKE",
    "LIMIT",
    "NATURAL",
    "NOT",
    "NULL",
    "OF",
    "OFFSET",
    "ON",
    "ONLY",
    "OR",
    "ORDER",
    "OUTER",
    "PARTITION",
    "RETURNING",
    "RIGHT",
    "SELECT",
    "SET",
    "TABLESAMPLE",
    "THEN",
    "UNION",
    "UPDATE",
    "USING",
    "VALUES",
    "WHEN",
    "WHERE",
    "WINDOW",
    "WITH",
];

/// A "table" with one of these names means the text is a sentence, not SQL
/// ("Select a plan from the list").
const PROSE: &[&str] = &[
    "the", "a", "an", "this", "that", "these", "those", "your", "my", "our", "their", "its", "it", "here", "there",
    "each", "which",
];

const STATEMENT_START: &[&str] = &["SELECT", "INSERT", "UPDATE", "DELETE", "WITH", "REPLACE"];

fn is_word(tok: Option<&Tok>, word: &str) -> bool {
    matches!(tok, Some(Tok::Word(w)) if w.eq_ignore_ascii_case(word))
}

fn is_any_word(tok: Option<&Tok>, words: &[&str]) -> bool {
    matches!(tok, Some(Tok::Word(w)) if words.iter().any(|k| w.eq_ignore_ascii_case(k)))
}

fn is_keyword(word: &str) -> bool {
    KEYWORDS.iter().any(|k| word.eq_ignore_ascii_case(k))
}

/// True when the token at `i`, the first one inside a parenthesis, begins a
/// statement: `(SELECT`, `(WITH`, the body of a data-modifying CTE, or a nested `((`.
fn opens_query(toks: &[Tok], i: usize) -> bool {
    matches!(toks.get(i), Some(Tok::Open))
        || is_any_word(toks.get(i), &["SELECT", "WITH", "INSERT", "UPDATE", "DELETE"])
}

/// Index of the `)` that closes the `(` at `open`, or the last index when it is missing.
fn matching_close(toks: &[Tok], open: usize) -> usize {
    let mut depth = 0usize;
    for (i, t) in toks.iter().enumerate().skip(open) {
        match t {
            Tok::Open => depth += 1,
            Tok::Close => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return i;
                }
            }
            _ => {}
        }
    }
    toks.len().saturating_sub(1)
}

/// Lower-cased names defined by `WITH name AS (...)`, with or without a column list.
fn cte_names(toks: &[Tok]) -> HashSet<String> {
    let mut names = HashSet::new();
    if !toks.iter().any(|t| is_word(Some(t), "WITH")) {
        return names;
    }
    for (i, tok) in toks.iter().enumerate() {
        let name = match tok {
            Tok::Word(w) if !is_keyword(w) => w,
            Tok::Quoted(q) => q,
            _ => continue,
        };
        let mut j = i + 1;
        if matches!(toks.get(j), Some(Tok::Open)) {
            j = matching_close(toks, j) + 1;
        }
        if !is_word(toks.get(j), "AS") {
            continue;
        }
        j += 1;
        if is_word(toks.get(j), "NOT") {
            j += 1;
        }
        if is_word(toks.get(j), "MATERIALIZED") {
            j += 1;
        }
        let body = matches!(toks.get(j), Some(Tok::Open))
            && is_any_word(
                toks.get(j + 1),
                &["SELECT", "WITH", "INSERT", "UPDATE", "DELETE", "VALUES"],
            );
        if body {
            names.insert(name.to_lowercase());
        }
    }
    names
}

/// One open parenthesis.
struct Frame {
    /// `FROM` and `JOIN` inside name tables: a subquery or a parenthesized join,
    /// not the argument list of `EXTRACT(year FROM d)`.
    query: bool,
    /// The parenthesis stands where a table is expected, so a comma after it
    /// continues the table list. Holds whether that list is written.
    list: Option<bool>,
}

struct Scan<'t> {
    toks: &'t [Tok],
    ctes: HashSet<String>,
    stack: Vec<Frame>,
    found: Vec<(String, bool)>,
    prose: bool,
}

fn scan_statement(toks: &[Tok]) -> Vec<(String, bool)> {
    let first = toks.iter().position(|t| *t != Tok::Open).unwrap_or(toks.len());
    if !is_any_word(toks.get(first), STATEMENT_START) || is_sentence(toks) {
        return Vec::new();
    }
    let mut scan = Scan {
        toks,
        ctes: cte_names(toks),
        stack: Vec::new(),
        found: Vec::new(),
        prose: false,
    };
    scan.run();
    if scan.prose {
        return Vec::new();
    }
    scan.found
}

/// "Delete from cart" on a button reads like SQL. A sentence is told apart by
/// its shape: nothing but words, the first one capitalized and the keywords
/// after it in lower case. SQL in that style almost always has a `*`, `=`,
/// comma, parenthesis or placeholder somewhere.
fn is_sentence(toks: &[Tok]) -> bool {
    let mut words = toks.iter().map(|t| match t {
        Tok::Word(w) => Some(w.as_str()),
        _ => None,
    });
    let Some(Some(first)) = words.next() else { return false };
    let mut chars = first.chars();
    let capitalized = chars.next().is_some_and(char::is_uppercase) && chars.all(char::is_lowercase);
    capitalized && words.all(|w| w.is_some_and(|w| !is_keyword(w) || w.chars().all(char::is_lowercase)))
}

impl Scan<'_> {
    fn run(&mut self) {
        let toks = self.toks;
        let mut i = 0;
        while i < toks.len() {
            i = match &toks[i] {
                Tok::Open => {
                    self.stack.push(Frame {
                        query: opens_query(toks, i + 1),
                        list: None,
                    });
                    i + 1
                }
                Tok::Close => self.close(i + 1),
                Tok::Word(w) if self.in_query() => self.keyword(&w.to_ascii_uppercase(), i),
                _ => i + 1,
            };
        }
    }

    fn in_query(&self) -> bool {
        self.stack.last().is_none_or(|f| f.query)
    }

    /// Handles the keyword at `i` and returns the index to continue from.
    fn keyword(&mut self, kw: &str, i: usize) -> usize {
        let prev = i.checked_sub(1).and_then(|p| self.toks.get(p));
        match kw {
            "FROM" if is_word(prev, "DELETE") => self.table_ref(i + 1, true),
            // `a IS DISTINCT FROM b` compares two values.
            "FROM" if !is_word(prev, "DISTINCT") => self.table_list(i + 1, false),
            "JOIN" => self.table_ref(i + 1, false),
            "INTO" if self.is_insert(i) => self.table_ref(i + 1, true),
            "UPDATE" => self.update(i + 1),
            _ => i + 1,
        }
    }

    /// `INSERT INTO`, `REPLACE INTO`, `INSERT IGNORE INTO`, `INSERT OR REPLACE INTO`;
    /// not `SELECT ... INTO`.
    fn is_insert(&self, into: usize) -> bool {
        let at = |back: usize| into.checked_sub(back).and_then(|p| self.toks.get(p));
        is_any_word(at(1), &["INSERT", "REPLACE", "IGNORE"]) || (is_word(at(2), "OR") && is_word(at(3), "INSERT"))
    }

    /// `UPDATE t SET ...`. The `SET` is required: `ON CONFLICT DO UPDATE SET`,
    /// `SELECT ... FOR UPDATE` and "Update the docs" name no table.
    fn update(&mut self, start: usize) -> usize {
        let mut j = start;
        if is_word(self.toks.get(j), "ONLY") {
            j += 1;
        }
        let Some((name, quoted, end)) = self.qualified_name(j) else {
            return start;
        };
        let after = self.skip_alias(end);
        if !is_any_word(self.toks.get(after), &["SET", "JOIN", "INNER", "LEFT"]) {
            return start;
        }
        self.add(name, quoted, true);
        after
    }

    /// After a `)`: a parenthesis in a table list may be followed by more tables.
    fn close(&mut self, after: usize) -> usize {
        match self.stack.pop() {
            Some(Frame { list: Some(write), .. }) => {
                let j = self.skip_alias(after);
                if matches!(self.toks.get(j), Some(Tok::Comma)) {
                    self.table_list(j + 1, write)
                } else {
                    j
                }
            }
            _ => after,
        }
    }

    /// Comma-separated table references starting at `j`.
    fn table_list(&mut self, mut j: usize, write: bool) -> usize {
        loop {
            j = self.table_ref(j, write);
            if !matches!(self.toks.get(j), Some(Tok::Comma)) {
                return j;
            }
            j += 1;
        }
    }

    /// One table reference at `j`: a name with an optional alias, a subquery, a
    /// parenthesized join or a table function. Returns the index to continue from.
    fn table_ref(&mut self, mut j: usize, write: bool) -> usize {
        while is_any_word(self.toks.get(j), &["ONLY", "LATERAL"]) {
            j += 1;
        }
        if matches!(self.toks.get(j), Some(Tok::Open)) {
            self.stack.push(Frame {
                query: true,
                list: Some(write),
            });
            if opens_query(self.toks, j + 1) {
                return j + 1;
            }
            return self.table_list(j + 1, write);
        }
        let Some((name, quoted, end)) = self.qualified_name(j) else {
            return j;
        };
        // Read from, a name with arguments is a table function such as
        // `generate_series(1, 10)`. Written to, it is `INSERT INTO t (columns)`.
        if !write && matches!(self.toks.get(end), Some(Tok::Open)) {
            self.stack.push(Frame {
                query: false,
                list: Some(write),
            });
            return end + 1;
        }
        self.add(name, quoted, write);
        self.skip_alias(end)
    }

    /// `schema.table` at `j`: the last part, whether it was quoted, and the index after it.
    fn qualified_name(&self, j: usize) -> Option<(String, bool, usize)> {
        let (mut name, mut quoted) = match self.toks.get(j)? {
            Tok::Word(w) if !is_keyword(w) => (w.clone(), false),
            Tok::Quoted(q) if !q.is_empty() => (q.clone(), true),
            _ => return None,
        };
        let mut end = j + 1;
        while matches!(self.toks.get(end), Some(Tok::Dot)) {
            match self.toks.get(end + 1) {
                Some(Tok::Word(w)) => (name, quoted) = (w.clone(), false),
                Some(Tok::Quoted(q)) if !q.is_empty() => (name, quoted) = (q.clone(), true),
                _ => break,
            }
            end += 2;
        }
        Some((name, quoted, end))
    }

    /// Skips `AS alias` or a bare alias.
    fn skip_alias(&self, mut j: usize) -> usize {
        if is_word(self.toks.get(j), "AS") {
            j += 1;
        }
        match self.toks.get(j) {
            Some(Tok::Word(w)) if !is_keyword(w) => j + 1,
            Some(Tok::Quoted(_)) => j + 1,
            _ => j,
        }
    }

    fn add(&mut self, name: String, quoted: bool, write: bool) {
        let lower = name.to_lowercase();
        if self.ctes.contains(&lower) {
            return;
        }
        if !quoted && PROSE.contains(&lower.as_str()) {
            self.prose = true;
            return;
        }
        if !self.found.iter().any(|(n, w)| *n == name && *w == write) {
            self.found.push((name, write));
        }
    }
}

// ---------------------------------------------------------------------------
// String templates
// ---------------------------------------------------------------------------

/// One piece of a string as written in the source: literal text, or an
/// interpolated expression (`{x}` in an f-string, `${x}` in a template string,
/// a non-string operand of `+`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Piece {
    Text(String),
    /// Source text of the expression.
    Hole(String),
}

/// Appends literal text, merging it into a preceding text piece.
pub(crate) fn push_text(pieces: &mut Vec<Piece>, text: &str) {
    if text.is_empty() {
        return;
    }
    match pieces.last_mut() {
        Some(Piece::Text(t)) => t.push_str(text),
        _ => pieces.push(Piece::Text(text.to_string())),
    }
}

/// The string with every interpolation written as a `${expr}` hole, which is the
/// form `lge_core::normalize_route` understands.
pub(crate) fn template_key(pieces: &[Piece]) -> String {
    let mut out = String::new();
    for p in pieces {
        match p {
            Piece::Text(t) => out.push_str(t),
            Piece::Hole(e) => {
                out.push_str("${");
                out.push_str(e);
                out.push('}');
            }
        }
    }
    out
}

/// The text when the string has no interpolation.
pub(crate) fn plain_text(pieces: &[Piece]) -> Option<String> {
    let mut out = String::new();
    for p in pieces {
        match p {
            Piece::Text(t) => out.push_str(t),
            Piece::Hole(_) => return None,
        }
    }
    Some(out)
}

/// A table named by SQL text that may contain interpolations.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SqlRef {
    /// The table name; empty when the name is an interpolation (see `hole`).
    pub table: String,
    pub write: bool,
    /// Index into the pieces of the interpolation that stands for the table name.
    pub hole: Option<usize>,
}

const HOLE_PREFIX: &str = "__lge_hole_";

/// Runs [`sql_tables`] over a string with interpolations. An interpolated value
/// (`WHERE id = {order_id}`) does not matter; an interpolated table name is
/// reported through `hole` so the caller can decide whether it knows the value.
/// A name that is only partly interpolated (`orders_{year}`) is dropped.
pub(crate) fn sql_refs(pieces: &[Piece]) -> Vec<SqlRef> {
    let mut sql = String::new();
    for (i, p) in pieces.iter().enumerate() {
        match p {
            Piece::Text(t) => sql.push_str(t),
            Piece::Hole(_) => sql.push_str(&format!("{HOLE_PREFIX}{i}__")),
        }
    }
    let mut out = Vec::new();
    for (table, write) in sql_tables(&sql) {
        let hole = table
            .strip_prefix(HOLE_PREFIX)
            .and_then(|r| r.strip_suffix("__"))
            .and_then(|n| n.parse::<usize>().ok());
        if hole.is_some() {
            out.push(SqlRef {
                table: String::new(),
                write,
                hole,
            });
        } else if !table.contains(HOLE_PREFIX) {
            out.push(SqlRef {
                table,
                write,
                hole: None,
            });
        }
    }
    out
}

/// The table in a name held by a constant (`"orders"`, `"public.orders"`), or
/// `None` when the text is not a plain table name.
pub(crate) fn table_name(text: &str) -> Option<String> {
    let last = text.trim().rsplit('.').next()?.trim_matches(|c| c == '"' || c == '`');
    let ok = !last.is_empty()
        && !starts_with_digit(last)
        && last.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '$');
    ok.then(|| last.to_string())
}

// ---------------------------------------------------------------------------
// URLs and routes
// ---------------------------------------------------------------------------

/// How much a string looks like the target of an HTTP call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UrlShape {
    /// `http://host/path`: an HTTP call whatever the receiver is called.
    Absolute,
    /// `/path` or `${BASE}/path`: only convincing on an HTTP client object.
    Path,
}

/// Classifies a key built by [`template_key`]; `None` when it is neither a URL nor a path.
pub(crate) fn url_shape(key: &str) -> Option<UrlShape> {
    let k = key.trim();
    let lower = k.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return Some(UrlShape::Absolute);
    }
    if k.starts_with('/') && !k.starts_with("//") {
        return Some(UrlShape::Path);
    }
    // A leading hole is a base URL when a path follows it.
    if k.starts_with("${") {
        let end = k.find('}')?;
        if k[end + 1..].starts_with('/') {
            return Some(UrlShape::Path);
        }
    }
    None
}

/// Joins a router or controller prefix and a route.
pub(crate) fn join_route(prefix: &str, route: &str) -> String {
    let p = prefix.trim_end_matches('/');
    if route.is_empty() {
        return if p.is_empty() { "/".to_string() } else { p.to_string() };
    }
    if p.is_empty() || route.starts_with('/') {
        format!("{p}{route}")
    } else {
        format!("{p}/{route}")
    }
}

// ---------------------------------------------------------------------------
// Fact records
// ---------------------------------------------------------------------------

/// Builds a fact with its stable id. `lines` is the first and last line of the evidence.
pub(crate) fn fact_site(
    path: &str,
    symbol_id: &str,
    kind: &str,
    key: &str,
    detail: &str,
    lines: (i64, i64),
    literal: bool,
) -> FactSite {
    FactSite {
        id: stable_id(&["fact", path, symbol_id, kind, key, detail, &lines.0.to_string()]),
        symbol_id: symbol_id.to_string(),
        file_path: path.to_string(),
        kind: kind.to_string(),
        key: key.to_string(),
        detail: detail.to_string(),
        line: lines.0,
        end_line: lines.1,
        literal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tables(sql: &str) -> Vec<(String, bool)> {
        sql_tables(sql)
    }

    fn t(name: &str, write: bool) -> (String, bool) {
        (name.to_string(), write)
    }

    #[test]
    fn select_from_reads_the_table() {
        assert_eq!(
            tables("SELECT id, total FROM orders WHERE id = %s"),
            vec![t("orders", false)]
        );
        assert_eq!(
            tables("  \n  select * from orders o order by o.id limit 5"),
            vec![t("orders", false)]
        );
    }

    #[test]
    fn select_from_a_comma_list_reads_every_table() {
        assert_eq!(
            tables("SELECT * FROM orders o, customers AS c WHERE o.customer_id = c.id"),
            vec![t("orders", false), t("customers", false)]
        );
    }

    #[test]
    fn join_reads_the_joined_table() {
        assert_eq!(
            tables("SELECT o.id FROM orders o LEFT OUTER JOIN customers c ON c.id = o.customer_id INNER JOIN items i USING (order_id)"),
            vec![t("orders", false), t("customers", false), t("items", false)]
        );
    }

    #[test]
    fn insert_into_writes_the_table() {
        assert_eq!(
            tables("INSERT INTO orders (customer_id, total) VALUES (%s, %s) RETURNING id"),
            vec![t("orders", true)]
        );
        assert_eq!(
            tables("insert or replace into cache values (?, ?)"),
            vec![t("cache", true)]
        );
    }

    #[test]
    fn insert_select_writes_one_table_and_reads_the_other() {
        assert_eq!(
            tables("INSERT INTO archive (id) SELECT id FROM orders WHERE created < now()"),
            vec![t("archive", true), t("orders", false)]
        );
    }

    #[test]
    fn update_writes_the_table() {
        assert_eq!(
            tables("UPDATE orders SET status = 'paid' WHERE id = :id"),
            vec![t("orders", true)]
        );
        assert_eq!(tables("update orders as o set total = 0"), vec![t("orders", true)]);
    }

    #[test]
    fn update_without_a_target_is_not_a_table() {
        // Upsert and row locking use the word UPDATE without naming a table.
        assert_eq!(
            tables("INSERT INTO orders (id) VALUES (1) ON CONFLICT (id) DO UPDATE SET total = excluded.total"),
            vec![t("orders", true)]
        );
        assert_eq!(
            tables("SELECT id FROM orders WHERE id = 1 FOR UPDATE"),
            vec![t("orders", false)]
        );
        assert_eq!(
            tables("SELECT id FROM orders FOR UPDATE OF orders SKIP LOCKED"),
            vec![t("orders", false)]
        );
    }

    #[test]
    fn delete_from_writes_the_table() {
        assert_eq!(tables("DELETE FROM orders WHERE id = ?"), vec![t("orders", true)]);
        assert_eq!(tables("Delete From sessions"), vec![t("sessions", true)]);
    }

    #[test]
    fn keywords_match_in_any_case() {
        assert_eq!(
            tables("sElEcT * fRoM Orders jOiN Customers on 1 = 1"),
            vec![t("Orders", false), t("Customers", false)]
        );
    }

    #[test]
    fn schema_qualified_names_keep_only_the_table() {
        assert_eq!(tables("SELECT * FROM public.orders"), vec![t("orders", false)]);
        assert_eq!(
            tables("INSERT INTO shop.dbo.orders VALUES (1)"),
            vec![t("orders", true)]
        );
    }

    #[test]
    fn quoted_identifiers_lose_their_quotes() {
        assert_eq!(tables("SELECT * FROM \"Orders\""), vec![t("Orders", false)]);
        assert_eq!(
            tables("SELECT * FROM `shop`.`order items`"),
            vec![t("order items", false)]
        );
        assert_eq!(tables("UPDATE [dbo].[Orders] SET total = 0"), vec![t("Orders", true)]);
        assert_eq!(tables("DELETE FROM \"public\".\"the\""), vec![t("the", true)]);
    }

    #[test]
    fn subqueries_are_searched_and_not_named() {
        assert_eq!(
            tables("SELECT * FROM (SELECT customer_id FROM orders) AS recent, customers c WHERE c.id IN (SELECT id FROM vips)"),
            vec![t("orders", false), t("customers", false), t("vips", false)]
        );
        assert_eq!(
            tables("DELETE FROM orders WHERE id IN (SELECT order_id FROM refunds)"),
            vec![t("orders", true), t("refunds", false)]
        );
    }

    #[test]
    fn cte_names_are_not_tables() {
        let sql = "WITH recent AS (SELECT * FROM orders), big (id) AS (SELECT id FROM recent) \
                   SELECT * FROM big JOIN customers ON customers.id = big.id";
        assert_eq!(tables(sql), vec![t("orders", false), t("customers", false)]);
        assert_eq!(
            tables("WITH gone AS (DELETE FROM orders RETURNING id) INSERT INTO audit SELECT id FROM gone"),
            vec![t("orders", true), t("audit", true)]
        );
    }

    #[test]
    fn from_inside_a_function_call_is_not_a_table() {
        assert_eq!(
            tables("SELECT EXTRACT(year FROM created_at), SUBSTRING(name FROM 1 FOR 3) FROM orders WHERE a IS DISTINCT FROM b"),
            vec![t("orders", false)]
        );
        assert_eq!(
            tables("SELECT n FROM generate_series(1, 3) AS n, orders"),
            vec![t("orders", false)]
        );
    }

    #[test]
    fn several_statements_are_split_at_semicolons() {
        assert_eq!(
            tables("SELECT 1 FROM t1; UPDATE t2 SET x = 1; nonsense from t3; DELETE FROM t4;"),
            vec![t("t1", false), t("t2", true), t("t4", true)]
        );
    }

    #[test]
    fn comments_and_string_literals_are_skipped() {
        let sql = "SELECT * -- not from here\n FROM orders /* nor from there */ WHERE note = 'copied from backup'";
        assert_eq!(tables(sql), vec![t("orders", false)]);
    }

    #[test]
    fn a_table_is_reported_once_per_access() {
        assert_eq!(
            tables("SELECT * FROM orders a JOIN orders b ON a.id = b.parent; UPDATE orders SET x = 1"),
            vec![t("orders", false), t("orders", true)]
        );
    }

    #[test]
    fn placeholders_are_not_tables() {
        assert_eq!(tables("SELECT * FROM %s WHERE id = %s"), vec![]);
        assert_eq!(tables("SELECT * FROM {table}"), vec![]);
        assert_eq!(tables("DELETE FROM ?"), vec![]);
        assert_eq!(tables("SELECT * FROM \"{0}\" WHERE id = \"%s\""), vec![]);
    }

    #[test]
    fn prose_is_not_sql() {
        for text in [
            "The value comes from the orders table",
            "Could not load data from orders",
            "Insert a task row.",
            "Update the cache and return",
            "Select a plan from the list",
            "Delete from the queue when done",
            "Delete from cart",
            "Select from options",
            "Insert into document",
            "imported from orders into archive",
            "",
        ] {
            assert_eq!(tables(text), vec![], "{text}");
        }
    }

    #[test]
    fn capitalized_sql_with_sql_punctuation_is_still_sql() {
        // Same first words as the sentences above, but no sentence has these shapes.
        assert_eq!(tables("Delete from cart where id = %s"), vec![t("cart", true)]);
        assert_eq!(tables("Select * from options"), vec![t("options", false)]);
        assert_eq!(tables("Insert into document values (1)"), vec![t("document", true)]);
        assert_eq!(tables("Select Name From Options"), vec![t("Options", false)]);
        assert_eq!(tables("delete from cart"), vec![t("cart", true)]);
    }

    #[test]
    fn interpolated_sql_separates_values_from_table_names() {
        let value = [
            Piece::Text("SELECT * FROM orders WHERE id = ".into()),
            Piece::Hole("order_id".into()),
        ];
        assert_eq!(
            sql_refs(&value),
            vec![SqlRef {
                table: "orders".into(),
                write: false,
                hole: None
            }]
        );

        let table = [
            Piece::Text("DELETE FROM ".into()),
            Piece::Hole("TABLE".into()),
            Piece::Text(" WHERE id = 1".into()),
        ];
        assert_eq!(
            sql_refs(&table),
            vec![SqlRef {
                table: String::new(),
                write: true,
                hole: Some(1)
            }]
        );

        let partial = [Piece::Text("SELECT * FROM orders_".into()), Piece::Hole("year".into())];
        assert_eq!(sql_refs(&partial), vec![]);

        let schema = [
            Piece::Text("SELECT * FROM ".into()),
            Piece::Hole("schema".into()),
            Piece::Text(".orders".into()),
        ];
        assert_eq!(
            sql_refs(&schema),
            vec![SqlRef {
                table: "orders".into(),
                write: false,
                hole: None
            }]
        );
    }

    #[test]
    fn template_keys_mark_holes() {
        let pieces = [
            Piece::Hole("BASE".into()),
            Piece::Text("/orders/".into()),
            Piece::Hole("order_id".into()),
        ];
        assert_eq!(template_key(&pieces), "${BASE}/orders/${order_id}");
        assert_eq!(plain_text(&pieces), None);
        assert_eq!(
            plain_text(&[Piece::Text("/orders".into())]),
            Some("/orders".to_string())
        );
        assert_eq!(lge_core::normalize_route(&template_key(&pieces)), "/orders/{}");
    }

    #[test]
    fn url_shapes() {
        assert_eq!(url_shape("http://orders-api/orders"), Some(UrlShape::Absolute));
        assert_eq!(url_shape("HTTPS://host"), Some(UrlShape::Absolute));
        assert_eq!(url_shape("/orders/${id}"), Some(UrlShape::Path));
        assert_eq!(url_shape("${BASE}/orders"), Some(UrlShape::Path));
        assert_eq!(url_shape("${url}"), None);
        assert_eq!(url_shape("user_id"), None);
        assert_eq!(url_shape("os.path.exists"), None);
    }

    #[test]
    fn routes_join_with_one_slash() {
        assert_eq!(join_route("/orders", ""), "/orders");
        assert_eq!(join_route("/orders", "/{order_id}"), "/orders/{order_id}");
        assert_eq!(join_route("/orders/", "/x"), "/orders/x");
        assert_eq!(join_route("", "/x"), "/x");
        assert_eq!(join_route("", ""), "/");
        assert_eq!(join_route("orders", ":id"), "orders/:id");
    }

    #[test]
    fn table_names_from_constants() {
        assert_eq!(table_name("orders"), Some("orders".to_string()));
        assert_eq!(table_name("public.orders"), Some("orders".to_string()));
        assert_eq!(table_name("orders WHERE 1=1"), None);
        assert_eq!(table_name(""), None);
    }

    #[test]
    fn fact_ids_are_stable() {
        let a = fact_site("a.py", "sym", "db_read", "orders", "", (3, 4), true);
        assert_eq!(a.id, stable_id(&["fact", "a.py", "sym", "db_read", "orders", "", "3"]));
        assert_eq!((a.line, a.end_line, a.literal), (3, 4, true));
        assert_eq!(
            (
                a.symbol_id.as_str(),
                a.file_path.as_str(),
                a.kind.as_str(),
                a.key.as_str()
            ),
            ("sym", "a.py", "db_read", "orders")
        );
        // The line is part of the id: the same fact further down is another fact.
        assert_ne!(a.id, fact_site("a.py", "sym", "db_read", "orders", "", (5, 5), true).id);
    }
}
