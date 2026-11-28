//! `lge report`: the daemon's page as plain text (or JSON), to share in place
//! of copying the page by hand.
//!
//! Everything is read from the status JSON (`GET /lge/api/status`) as a
//! [`Value`], so the status of an older daemon, with fewer or differently named
//! fields, renders too: a missing field or section is left out (or shown as
//! "–" in a table cell), never as `null`, and unknown fields are ignored.
//! `--redact` rewrites the JSON before anything is rendered: task texts become
//! stable session labels, and paths become their last component.

use anyhow::{bail, Context, Result};
use lge_bench::ab::{DaemonApi, HttpDaemon};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::PathBuf;

/// The address `lge serve` listens on by default.
pub const DEFAULT_URL: &str = "http://127.0.0.1:8787";

/// The key `--json` puts the compaction what-if under, next to the status fields.
pub const WHATIF_KEY: &str = "compact_whatif";

/// Shown for a value that is unknown or missing, as the page does.
const DASH: &str = "–";

/// Characters of a task shown in the sessions table.
const TASK_CHARS: usize = 60;

#[derive(clap::Args)]
pub struct ReportArgs {
    /// The address of the running daemon (`lge serve`).
    #[arg(long, default_value = DEFAULT_URL)]
    pub url: String,
    /// Rows of the sessions table: the sessions with the most input tokens.
    #[arg(long, default_value_t = 25)]
    pub sessions: usize,
    /// Print the status JSON (pretty), with the compaction what-if under `compact_whatif`.
    #[arg(long)]
    pub json: bool,
    /// Make the report shareable: task texts become session labels and paths their last
    /// component. Numbers are kept.
    #[arg(long)]
    pub redact: bool,
    /// Write the report to this file instead of printing it.
    #[arg(long, value_name = "FILE")]
    pub out: Option<PathBuf>,
}

/// What the text report shows.
pub struct Options {
    /// Rows of the sessions table.
    pub sessions: usize,
    /// The JSON was redacted: the top says so.
    pub redacted: bool,
}

/// What the report says about itself at the top.
pub struct Header {
    pub cli_version: String,
    pub url: String,
    pub local_time: String,
}

/// `lge report`: read the daemon's status (and its compaction what-if when it
/// has one), render it, and print it or write it to `--out`.
pub fn run(args: &ReportArgs) -> Result<()> {
    let daemon = HttpDaemon::new(&args.url)?;
    let base = daemon.base().to_string();
    let mut status = daemon.status().with_context(|| {
        format!("cannot read the lge status at {base}/lge/api/status: `lge serve` must be running at {base}")
    })?;
    if !status.is_object() {
        bail!("{base}/lge/api/status did not answer with lge's status JSON: is `lge serve` running at {base}?");
    }
    // Only daemons of v0.0.6 and later have it; an error (404 on older ones) leaves it out.
    let mut whatif = daemon.get("/lge/api/compact-whatif").ok().filter(Value::is_object);
    if args.redact {
        let home = std::env::home_dir().map(|h| h.to_string_lossy().to_string());
        redact(&mut status, whatif.as_mut(), home.as_deref());
    }
    let text = if args.json {
        json_text(&status, whatif.as_ref(), args.redact)?
    } else {
        let header = Header {
            cli_version: env!("CARGO_PKG_VERSION").to_string(),
            url: if args.redact {
                shareable_url(&base)
            } else {
                base.clone()
            },
            local_time: local_time(now(), true),
        };
        let opts = Options {
            sessions: args.sessions,
            redacted: args.redact,
        };
        render(&status, whatif.as_ref(), &opts, &header)
    };
    match &args.out {
        Some(path) => {
            std::fs::write(path, text).with_context(|| format!("cannot write {}", path.display()))?;
            eprintln!("lge report: wrote {}", path.display());
        }
        None => print!("{text}"),
    }
    Ok(())
}

/// The status JSON, pretty, with the what-if under [`WHATIF_KEY`] when there
/// is one and `"redacted": true` after `--redact`.
pub fn json_text(status: &Value, whatif: Option<&Value>, redacted: bool) -> Result<String> {
    let mut v = status.clone();
    if let Value::Object(map) = &mut v {
        if let Some(w) = whatif {
            map.insert(WHATIF_KEY.to_string(), w.clone());
        }
        if redacted {
            map.insert("redacted".to_string(), Value::Bool(true));
        }
    }
    Ok(serde_json::to_string_pretty(&v)? + "\n")
}

// ---------------------------------------------------------------------------
// Reading values

/// The value at `ptr` (a JSON pointer such as `/saved/since`), unless it is missing or null.
fn at<'a>(v: &'a Value, ptr: &str) -> Option<&'a Value> {
    v.pointer(ptr).filter(|x| !x.is_null())
}

fn float(v: &Value, ptr: &str) -> Option<f64> {
    at(v, ptr)?.as_f64()
}

fn int(v: &Value, ptr: &str) -> Option<i64> {
    let x = at(v, ptr)?;
    x.as_i64().or_else(|| x.as_f64().map(|f| f.round() as i64))
}

fn text<'a>(v: &'a Value, ptr: &str) -> Option<&'a str> {
    at(v, ptr)?.as_str().filter(|s| !s.is_empty())
}

fn list<'a>(v: &'a Value, ptr: &str) -> Option<&'a [Value]> {
    at(v, ptr)?.as_array().map(Vec::as_slice)
}

fn flag(v: &Value, ptr: &str) -> bool {
    at(v, ptr).and_then(Value::as_bool).unwrap_or(false)
}

/// A number with thousands separators (a fraction is rounded), or `None`.
fn num(v: &Value, ptr: &str) -> Option<String> {
    let x = at(v, ptr)?;
    if let Some(i) = x.as_i64() {
        return Some(group(i));
    }
    x.as_f64().filter(|f| f.is_finite()).map(|f| group(f.round() as i64))
}

/// [`num`], or "–".
fn n(v: &Value, ptr: &str) -> String {
    num(v, ptr).unwrap_or_else(|| DASH.to_string())
}

/// `1234567` -> `1,234,567`.
fn group(i: i64) -> String {
    let digits = i.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    if i < 0 {
        out.push('-');
    }
    for (k, c) in digits.chars().enumerate() {
        if k > 0 && (digits.len() - k).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `a` as a whole percentage of `b`, as the page's `pct`.
fn pct(a: Option<f64>, b: Option<f64>) -> String {
    match (a, b) {
        (Some(a), Some(b)) if b > 0.0 => format!("{}%", (100.0 * a / b).round() as i64),
        _ => DASH.to_string(),
    }
}

/// A share from 0 to 1 as a percentage with one decimal, as the page's `rate`.
fn rate(r: Option<f64>) -> String {
    r.map_or_else(|| DASH.to_string(), |r| format!("{:.1}%", 100.0 * r))
}

/// Bytes as the page's `size`: B, KB, MB, GB, TB.
fn size(bytes: f64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let (mut b, mut i) = (bytes.max(0.0), 0);
    while b >= 1024.0 && i < units.len() - 1 {
        b /= 1024.0;
        i += 1;
    }
    match i {
        0 => format!("{b} {}", units[0]),
        _ if b < 10.0 => format!("{b:.1} {}", units[i]),
        _ => format!("{b:.0} {}", units[i]),
    }
}

/// Bytes in MB as the page's graphs table shows them.
fn mb(bytes: f64) -> String {
    let m = bytes / 1_048_576.0;
    if bytes >= 10_485_760.0 {
        format!("{m:.0} MB")
    } else {
        format!("{m:.1} MB")
    }
}

/// A unix time as local `YYYY-MM-DD HH:MM`, or "–" for none.
fn when(v: &Value, ptr: &str) -> String {
    match int(v, ptr) {
        Some(t) if t > 0 => local_time(t, false),
        _ => DASH.to_string(),
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// `YYYY-MM-DD HH:MM` in local time, with the UTC offset when `offset`.
fn local_time(unix: i64, offset: bool) -> String {
    let (y, mo, d, h, mi, off) = local_parts(unix);
    let mut s = format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}");
    if offset {
        match off {
            Some(secs) => {
                let sign = if secs < 0 { '-' } else { '+' };
                let a = secs.unsigned_abs();
                let _ = write!(s, " {sign}{:02}:{:02}", a / 3600, a % 3600 / 60);
            }
            None => s.push_str(" UTC"),
        }
    }
    s
}

/// Year, month, day, hour, minute, and the offset from UTC in seconds.
#[cfg(unix)]
fn local_parts(unix: i64) -> (i64, i64, i64, i64, i64, Option<i64>) {
    let t = unix as libc::time_t;
    // SAFETY: `tm` is plain data that `localtime_r` fills in; both pointers
    // are valid for the call, and the result is checked before use.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let filled = unsafe { !libc::localtime_r(&t, &mut tm).is_null() };
    if !filled {
        return utc_parts(unix);
    }
    (
        i64::from(tm.tm_year) + 1900,
        i64::from(tm.tm_mon) + 1,
        i64::from(tm.tm_mday),
        i64::from(tm.tm_hour),
        i64::from(tm.tm_min),
        // `c_long`: 32 bits on some targets.
        Some(tm.tm_gmtoff as i64),
    )
}

#[cfg(not(unix))]
fn local_parts(unix: i64) -> (i64, i64, i64, i64, i64, Option<i64>) {
    utc_parts(unix)
}

/// The civil date and time in UTC; no offset known.
fn utc_parts(unix: i64) -> (i64, i64, i64, i64, i64, Option<i64>) {
    let z = unix.div_euclid(86_400) + 719_468;
    let secs = unix.rem_euclid(86_400);
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day, secs / 3600, secs % 3600 / 60, None)
}

/// A task as the page shows it: a mention Codex writes as `[$name](path)` or
/// `[@name](path)` shown by its label, and whitespace (newlines) collapsed.
fn clean_task(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('[') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        match mention(tail) {
            Some((label, len)) => {
                out.push_str(label);
                rest = &tail[len..];
            }
            None => {
                out.push('[');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `[$label](target)` at the start of `t`: the label and the length of the whole.
fn mention(t: &str) -> Option<(&str, usize)> {
    let close = t.find(']')?;
    let label = &t[1..close];
    let sigil = label.starts_with('$') || label.starts_with('@');
    if !sigil || label.len() < 2 || label.contains('\n') {
        return None;
    }
    let after = t[close + 1..].strip_prefix('(')?;
    let end = after.find(')')?;
    let target = &after[..end];
    if target.is_empty() || target.contains('\n') {
        return None;
    }
    Some((label, close + 2 + end + 1))
}

/// At most `max` characters, with "…" when cut.
fn cut(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The sessions in the order of the table: most input tokens first; ties keep
/// the status's order (newest first). Redaction numbers the labels in it.
fn table_order(sessions: &[Value]) -> Vec<&Value> {
    let mut rows: Vec<&Value> = sessions.iter().collect();
    rows.sort_by(|a, b| {
        let input = |v: &Value| float(v, "/input_tokens").unwrap_or(0.0);
        input(b).total_cmp(&input(a))
    });
    rows
}

// ---------------------------------------------------------------------------
// Tables

/// A plain-text table: columns whose heading starts with `#` are right-aligned
/// (as on the page); each row can carry indented notes below it.
struct Table {
    head: Vec<String>,
    rows: Vec<(Vec<String>, Vec<String>)>,
}

impl Table {
    fn new(head: &[&str]) -> Table {
        Table {
            head: head.iter().map(|h| h.to_string()).collect(),
            rows: Vec::new(),
        }
    }

    fn row(&mut self, cells: Vec<String>) -> &mut Vec<String> {
        self.rows.push((cells, Vec::new()));
        &mut self.rows.last_mut().expect("just pushed").1
    }

    fn write(&self, o: &mut String, empty: &str) {
        if self.rows.is_empty() {
            if !empty.is_empty() {
                let _ = writeln!(o, "{empty}");
            }
            return;
        }
        let right: Vec<bool> = self.head.iter().map(|h| h.starts_with('#')).collect();
        let names: Vec<&str> = self.head.iter().map(|h| h.trim_start_matches('#')).collect();
        let mut widths: Vec<usize> = names.iter().map(|h| h.chars().count()).collect();
        for (cells, _) in &self.rows {
            for (w, c) in widths.iter_mut().zip(cells) {
                *w = (*w).max(c.chars().count());
            }
        }
        let line = |cells: &[&str]| -> String {
            let padded: Vec<String> = cells
                .iter()
                .zip(widths.iter().zip(&right))
                .map(|(c, (&w, &r))| if r { format!("{c:>w$}") } else { format!("{c:<w$}") })
                .collect();
            padded.join("  ").trim_end().to_string()
        };
        if names.iter().any(|h| !h.is_empty()) {
            let _ = writeln!(o, "{}", line(&names));
        }
        for (cells, notes) in &self.rows {
            let cells: Vec<&str> = cells.iter().map(String::as_str).collect();
            let _ = writeln!(o, "{}", line(&cells));
            for note in notes {
                let _ = writeln!(o, "    {note}");
            }
        }
    }
}

fn heading(o: &mut String, title: &str) {
    let _ = writeln!(o, "\n## {title}\n");
}

/// The parts that are there, joined with `sep`.
fn join(parts: impl IntoIterator<Item = Option<String>>, sep: &str) -> String {
    parts.into_iter().flatten().collect::<Vec<_>>().join(sep)
}

/// `"{number} {what}"` when the field is there.
fn clause(v: &Value, ptr: &str, what: &str) -> Option<String> {
    num(v, ptr).map(|n| format!("{n} {what}"))
}

// ---------------------------------------------------------------------------
// The report

/// The whole text report, sections in the page's order.
pub fn render(s: &Value, whatif: Option<&Value>, opts: &Options, h: &Header) -> String {
    let mut o = String::new();
    write_header(&mut o, s, opts, h);
    write_cards(&mut o, s);
    write_folders(&mut o, s);
    write_sessions(&mut o, s, whatif, opts);
    write_rules(&mut o, s);
    write_holdout(&mut o, s);
    write_cache(&mut o, s);
    write_prefix(&mut o, s);
    write_modes(&mut o, s);
    write_settings(&mut o, s);
    o
}

fn write_header(o: &mut String, s: &Value, opts: &Options, h: &Header) {
    let _ = writeln!(o, "# lge report");
    if opts.redacted {
        let _ = writeln!(
            o,
            "Redacted: task texts are replaced by session labels and paths by their last component; numbers are kept."
        );
    }
    let daemon = text(s, "/version").map_or_else(|| "unknown".to_string(), |v| format!("v{v}"));
    let _ = writeln!(
        o,
        "lge CLI v{} · daemon {daemon} · {} · {}",
        h.cli_version, h.url, h.local_time
    );
    let up = int(s, "/uptime_seconds").map(|secs| format!("up {} min", group(secs / 60)));
    let line = join(
        [
            text(s, "/version").map(|v| format!("v{v}")),
            text(s, "/listen").map(|l| format!("listening on {l}")),
            up,
        ],
        " · ",
    );
    if !line.is_empty() {
        let _ = writeln!(o, "{line}");
    }
    if let Some(e) = text(s, "/settings_error") {
        let _ = writeln!(o, "\nSettings error: {e}");
    }
}

fn write_cards(o: &mut String, s: &Value) {
    let mut t = Table::new(&["", "#", ""]);
    if let Some(modes) = list(s, "/modes") {
        let none = Value::Object(Map::new());
        let p = modes
            .iter()
            .find(|m| text(m, "/mode") == Some("proxy"))
            .unwrap_or(&none);
        let zero = |ptr: &str| num(p, ptr).unwrap_or_else(|| "0".to_string());
        t.row(vec![
            "Sessions".into(),
            zero("/sessions"),
            "all stored; background and lge ab runs apart".into(),
        ]);
        t.row(vec![
            "Input tokens".into(),
            zero("/input_tokens"),
            format!(
                "cached {}",
                pct(float(p, "/cached_input_tokens"), float(p, "/input_tokens"))
            ),
        ]);
        t.row(vec![
            "Output tokens".into(),
            zero("/output_tokens"),
            format!(
                "reasoning {}",
                pct(float(p, "/reasoning_tokens"), float(p, "/output_tokens"))
            ),
        ]);
    }
    if let Some(sv) = at(s, "/saved") {
        let since = int(sv, "/since")
            .filter(|t| *t > 0)
            .map(|t| format!(" at {}", local_time(t, false)))
            .unwrap_or_default();
        let before = float(sv, "/tokens_before");
        let removed = before.zip(float(sv, "/tokens_after")).map(|(b, a)| b - a);
        t.row(vec![
            "Removed from tool output".into(),
            removed.map_or_else(|| DASH.to_string(), |r| group(r as i64)),
            format!(
                "{} outputs, {} smaller; since the daemon started{since}, each change counted once",
                n(sv, "/outputs_shortened"),
                pct(removed, before)
            ),
        ]);
        t.row(vec![
            "Graph context added".into(),
            n(sv, "/context_tokens"),
            format!(
                "{} packs; since the daemon started{since}, each pack counted once",
                n(sv, "/contexts_added")
            ),
        ]);
    }
    if let Some(an) = at(s, "/annuity") {
        t.row(vec![
            "Kept out of requests".into(),
            n(an, "/removed_tokens_sent"),
            "all stored sessions, on every request that carried the change".into(),
        ]);
        t.row(vec![
            "Packs sent".into(),
            n(an, "/added_tokens_sent"),
            "all stored sessions, on every request that carried a pack".into(),
        ]);
    }
    if !t.rows.is_empty() {
        heading(o, "Cards");
        t.write(o, "");
    }
}

fn write_folders(o: &mut String, s: &Value) {
    let repos = list(s, "/repos");
    let graphs = at(s, "/graphs");
    if repos.is_none() && graphs.is_none() {
        return;
    }
    heading(o, "Folders in the graph");
    if let Some(repos) = repos {
        let mut t = Table::new(&["Folder", "State", "#Files", "#Symbols", "#Facts"]);
        for r in repos {
            let name = text(r, "/name").or_else(|| text(r, "/root")).unwrap_or(DASH);
            let state = text(r, "/state").unwrap_or(DASH);
            let notes = t.row(vec![
                name.to_string(),
                state.to_string(),
                n(r, "/files"),
                n(r, "/symbols"),
                n(r, "/memories"),
            ]);
            if let Some(root) = text(r, "/root").filter(|root| *root != name) {
                notes.push(root.to_string());
            }
            if let Some(detail) = text(r, "/detail") {
                notes.push(format!("{state}: {}", one_line(detail)));
            }
            if let Some(secs) = float(r, "/index_seconds") {
                notes.push(format!("last index {secs:.1} s"));
            }
            notes.extend(bulky(r));
        }
        t.write(
            o,
            "No folder yet. Add one on the page, or start Codex in a git repository.",
        );
    }
    if let Some(gr) = graphs {
        let rows = list(gr, "/graphs").unwrap_or(&[]);
        let _ = writeln!(
            o,
            "\nGraphs on disk: {}, {}; {} for folders not in the list, {}.",
            group(rows.len() as i64),
            mb(float(gr, "/total_bytes").unwrap_or(0.0)),
            n(gr, "/unlisted"),
            mb(float(gr, "/unlisted_bytes").unwrap_or(0.0)),
        );
        let mut t = Table::new(&["Graph folder", "Built for", "#Size", "State"]);
        for g in rows {
            let state = if flag(g, "/listed") {
                "listed"
            } else if at(g, "/exists").and_then(Value::as_bool) == Some(false) {
                "not listed, folder no longer exists"
            } else {
                "not listed"
            };
            t.row(vec![
                text(g, "/dir").or_else(|| text(g, "/path")).unwrap_or(DASH).to_string(),
                text(g, "/root")
                    .unwrap_or("unknown (built before lge recorded it)")
                    .to_string(),
                mb(float(g, "/bytes").unwrap_or(0.0)),
                state.to_string(),
            ]);
        }
        if !rows.is_empty() {
            o.push('\n');
        }
        t.write(o, "");
    }
}

/// The page's warnings on folders searches would read, at most five and a count.
fn bulky(r: &Value) -> Vec<String> {
    let list = list(r, "/bulky").unwrap_or(&[]);
    let mut out: Vec<String> = list
        .iter()
        .take(5)
        .map(|d| {
            let file = text(d, "/ignore_file")
                .map(str::to_string)
                .or_else(|| text(r, "/root").map(|root| format!("{root}/.gitignore")))
                .unwrap_or_else(|| ".gitignore".to_string());
            let mut line = format!(
                "Searches would read {}/ ({}, {} files)",
                text(d, "/path").unwrap_or(DASH),
                size(float(d, "/bytes").unwrap_or(0.0)),
                n(d, "/files"),
            );
            if let Some(reason) = text(d, "/reason") {
                let _ = write!(line, ": {reason}");
            }
            if let Some(ignore) = text(d, "/ignore_line") {
                let _ = write!(line, ". Add {ignore} to {file}");
            }
            line
        })
        .collect();
    if list.len() > 5 {
        out.push(format!("and {} more such folders", list.len() - 5));
    }
    out
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A session's name as the page shows it: its task, else `fallback`.
fn task_of(v: &Value, fallback: &str) -> String {
    let t = text(v, "/task").map(clean_task).unwrap_or_default();
    if t.is_empty() {
        fallback.to_string()
    } else {
        t
    }
}

fn write_sessions(o: &mut String, s: &Value, whatif: Option<&Value>, opts: &Options) {
    let Some(sessions) = list(s, "/sessions") else {
        // An older daemon without the table may still have the rest.
        write_long(o, s);
        write_whatif(o, s, whatif);
        write_packs(o, s);
        write_background(o, s);
        return;
    };
    heading(o, "Sessions through the proxy");
    let rows = table_order(sessions);
    let shown = rows.len().min(opts.sessions);
    let mut t = Table::new(&[
        "Task",
        "#Requests",
        "#Input / request",
        "#Input",
        "#Cached",
        "#Output",
        "#Reasoning",
        "#Kept out (all requests)",
        "#Added",
        "#Net",
        "#Net share",
    ]);
    for x in rows.iter().take(shown) {
        let mut task = task_of(x, "(no typed message)");
        if let Some(bg) = text(x, "/background") {
            task = format!("[background: {bg}] {task}");
        }
        let notes = t.row(vec![
            cut(&task, TASK_CHARS),
            n(x, "/requests"),
            n(x, "/mean_input_tokens"),
            n(x, "/input_tokens"),
            pct(float(x, "/cached_input_tokens"), float(x, "/input_tokens")),
            n(x, "/output_tokens"),
            n(x, "/reasoning_tokens"),
            n(x, "/removed_tokens_sent"),
            n(x, "/added_tokens_sent"),
            n(x, "/net_tokens"),
            rate(float(x, "/net_share")),
        ]);
        let detail = session_detail(x);
        if !detail.is_empty() {
            notes.push(detail);
        }
    }
    t.write(o, "No session has gone through the proxy yet.");
    if !rows.is_empty() {
        let _ = writeln!(
            o,
            "{} of {} sessions left out (the {} with the most input are shown; --sessions N to change).",
            group((rows.len() - shown) as i64),
            group(rows.len() as i64),
            group(shown as i64),
        );
    }
    write_long(o, s);
    write_whatif(o, s, whatif);
    write_packs(o, s);
    write_background(o, s);
}

/// The line under a session's task: model, folder, router, arm, packs.
fn session_detail(x: &Value) -> String {
    let router = text(x, "/recommended_level").map(|l| {
        let applied = if text(x, "/applied_level").is_some() {
            " (applied)"
        } else {
            ""
        };
        format!("router: {l}{applied}")
    });
    let packs = [
        "/packs_added",
        "/packs_known",
        "/packs_over_budget",
        "/packs_too_large",
        "/packs_retired",
    ]
    .iter()
    .any(|p| int(x, p).unwrap_or(0) != 0)
    .then(|| {
        format!(
            "packs: {}",
            join(
                [
                    clause(x, "/packs_added", "added"),
                    clause(x, "/packs_known", "already known"),
                    clause(x, "/packs_over_budget", "no room"),
                    clause(x, "/packs_too_large", "too large"),
                    clause(x, "/packs_retired", "retired after a rewrite of the history"),
                ],
                ", "
            )
        )
    });
    join(
        [
            text(x, "/model").map(str::to_string),
            text(x, "/repo").map(|r| r.rsplit(['/', '\\']).next().unwrap_or(r).to_string()),
            router,
            (text(x, "/mode") == Some("proxy-holdout")).then(|| "held out".to_string()),
            text(x, "/ab_arm").map(|a| format!("lge ab arm {a}")),
            packs,
        ],
        " · ",
    )
}

fn write_long(o: &mut String, s: &Value) {
    let long = list(s, "/long_sessions/sessions").unwrap_or(&[]);
    if long.is_empty() {
        return;
    }
    let _ = writeln!(o, "\nLong sessions:");
    for l in long {
        let fallback = text(l, "/id").unwrap_or(DASH);
        let mut line = format!(
            "- {}: {} requests, {} input tokens each on average",
            task_of(l, fallback),
            n(l, "/requests"),
            n(l, "/mean_input_tokens"),
        );
        if at(l, "/finished_at").is_some() {
            let _ = write!(line, ", last request {}", when(l, "/finished_at"));
        }
        let _ = writeln!(o, "{line}");
    }
    if let Some(note) = text(s, "/long_sessions/note") {
        let _ = writeln!(o, "{note}");
    }
}

/// The summary of the compaction what-if, as the page's box under the long sessions.
fn write_whatif(o: &mut String, s: &Value, whatif: Option<&Value>) {
    let Some(w) = whatif else { return };
    let _ = writeln!(
        o,
        "\nWhat if Codex compacted sooner? (an estimate, read from Codex's own session files)"
    );
    let analyses = list(w, "/sessions").unwrap_or(&[]);
    if analyses.is_empty() {
        let read = num(w, "/rollouts_read")
            .map(|r| format!(" ({r} session files read)"))
            .unwrap_or_default();
        let _ = writeln!(
            o,
            "No long session in Codex's session files of the last {} days{read}.",
            n(w, "/days")
        );
        return;
    }
    let stored = list(s, "/sessions").unwrap_or(&[]);
    for a in analyses {
        let id = text(a, "/session_id").unwrap_or(DASH);
        let name = match stored.iter().find(|x| text(x, "/id") == Some(id)) {
            Some(known) => task_of(known, id),
            // A redacted id is a label already.
            None if id.contains(' ') => id.to_string(),
            None => format!(
                "session {} (not matched to a stored session)",
                id.chars().take(8).collect::<String>()
            ),
        };
        let about = join(
            [
                clause(a, "/work_requests", "requests"),
                num(a, "/limit").map(|l| format!("limit {l}")),
                text(a, "/limit_source").map(str::to_string),
                text(a, "/confidence").map(|c| format!("confidence {c}")),
            ],
            ", ",
        );
        let _ = writeln!(o, "- {name} ({about})");
        let model_compactions = int(a, "/model_run/compactions").unwrap_or(0);
        for c in list(a, "/candidates").unwrap_or(&[]) {
            let Some(limit) = num(c, "/limit") else { continue };
            let session_limit = list(c, "/flags")
                .unwrap_or(&[])
                .iter()
                .any(|f| f.as_str() == Some("session limit"));
            if session_limit {
                continue;
            }
            let line = if flag(c, "/thrash") {
                format!("At {limit}: Codex would compact every few requests; no saving is shown.")
            } else if let Some(d) = float(c, "/delta_pct") {
                let more = int(c, "/run/compactions").unwrap_or(0) - model_compactions;
                format!(
                    "At {limit}: about {:.0}% {} weighted input cost, {} more compactions, Codex works from a summary after each.",
                    d.abs(),
                    if d <= 0.0 { "less" } else { "more" },
                    group(more)
                )
            } else {
                format!("At {limit}: no estimate.")
            };
            let _ = writeln!(o, "    {line}");
        }
        if let Some(e) = float(a, "/error_pct/cost") {
            let _ = writeln!(
                o,
                "    The model at the current limit is off by {e:.1}% from your measured totals."
            );
        }
    }
    let caveats: Vec<&str> = list(w, "/caveats")
        .unwrap_or(&[])
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let _ = writeln!(
        o,
        "{}Set model_auto_compact_token_limit in ~/.codex/config.toml yourself if you decide to: lge never writes that file.",
        if caveats.is_empty() { String::new() } else { format!("{} ", caveats.join(" ")) }
    );
}

fn write_packs(o: &mut String, s: &Value) {
    if let Some(pk) = at(s, "/packs") {
        let mut parts = Vec::new();
        let decided = join(
            [
                clause(pk, "/added", "added"),
                clause(pk, "/known", "not added as already known"),
                clause(pk, "/over_budget", "not added: no room left in the conversation budget"),
                clause(
                    pk,
                    "/too_large",
                    "not added: larger than a follow-up pack may be (600 tokens)",
                ),
                clause(
                    pk,
                    "/retired",
                    "retired after the history was rewritten (compaction, edit, rollback)",
                ),
            ],
            ", ",
        );
        if !decided.is_empty() {
            parts.push(format!("Packs (sessions not held out): {decided}."));
        }
        if let Some(sent) = num(pk, "/tokens_sent") {
            let since = if at(pk, "/since").is_some() {
                format!(" since {}", when(pk, "/since"))
            } else {
                String::new()
            };
            let of_them = join(
                [
                    clause(
                        pk,
                        "/tokens_sent_uncounted",
                        "in sessions whose records count no pack (recorded before v0.0.2)",
                    ),
                    clause(pk, "/tokens_sent_background", "in Codex's background sessions"),
                ],
                ", ",
            );
            let of_them = if of_them.is_empty() {
                String::new()
            } else {
                format!("; of them {of_them}")
            };
            parts.push(format!(
                "{sent} tokens of packs sent over all requests{since}{of_them}."
            ));
        }
        if let Some(w) = num(pk, "/holdout_would_add") {
            parts.push(format!("Held-out sessions would have added {w}."));
        }
        if let Some(g) = num(pk, "/no_graph") {
            parts.push(format!(
                "{g} typed messages were first seen while their folder had no graph yet."
            ));
        }
        if let Some(l) = num(pk, "/at_limit") {
            let live = num(pk, "/at_limit_live_would_add")
                .map(|x| format!("; a limit on live packs only would have let {x} of them through"))
                .unwrap_or_default();
            parts.push(format!(
                "{l} typed messages with a strong starting point got no pack because their conversation was at its limit of 6 packs{live}."
            ));
        }
        if !parts.is_empty() {
            let _ = writeln!(o, "\n{}", parts.join(" "));
        }
    }
    if let Some(us) = at(s, "/unread_scripts") {
        let any = int(us, "/outputs").unwrap_or(0) != 0 || int(us, "/tool_output_tokens").unwrap_or(0) != 0;
        if any {
            let share = float(us, "/share").map_or_else(|| DASH.to_string(), |x| format!("{:.1}%", x * 100.0));
            let _ = writeln!(
                o,
                "\nScripts lge could not read: {} outputs, {} tokens ({share} of tool output; outputs of 400 tokens or more, counted once each). A count, nothing was changed.",
                n(us, "/outputs"),
                n(us, "/tokens"),
            );
        } else {
            let _ = writeln!(
                o,
                "\nScripts lge could not read: none counted yet (outputs of 400 tokens or more, counted once each)."
            );
        }
    }
}

fn write_background(o: &mut String, s: &Value) {
    let Some(rows) = list(s, "/background") else { return };
    let _ = writeln!(
        o,
        "\nCodex's background requests (approval reviews, reviews, titles, recaps, memory):"
    );
    let mut t = Table::new(&[
        "Label",
        "#Sessions",
        "#Requests",
        "#Input tokens of these requests",
        "#Cached",
        "#Uncached",
        "#Removed by lge",
        "#Input tokens of its sessions",
    ]);
    for b in rows {
        let counted = int(b, "/input_counted_requests").unwrap_or(0);
        let input = if counted == 0 {
            DASH.to_string()
        } else {
            let missing = int(b, "/requests").unwrap_or(0) - counted;
            let mut cell = n(b, "/request_input_tokens");
            if missing > 0 {
                let _ = write!(cell, " + {} requests not counted (before v0.0.5)", group(missing));
            }
            cell
        };
        let (cached, uncached) = if at(b, "/cached_tokens").is_none() {
            (DASH.to_string(), DASH.to_string())
        } else {
            let mut cell = n(b, "/cached_tokens");
            let basis = float(b, "/cached_basis_tokens").unwrap_or(0.0);
            let input = float(b, "/request_input_tokens").unwrap_or(0.0);
            if basis < input {
                let _ = write!(
                    cell,
                    " over {} of {} input tokens",
                    n(b, "/cached_basis_tokens"),
                    n(b, "/request_input_tokens")
                );
            }
            (cell, n(b, "/uncached_tokens"))
        };
        let sessions_input = if at(b, "/sessions").is_none() {
            DASH.to_string()
        } else {
            n(b, "/input_tokens")
        };
        t.row(vec![
            text(b, "/label").unwrap_or(DASH).to_string(),
            n(b, "/sessions"),
            n(b, "/requests"),
            input,
            cached,
            uncached,
            n(b, "/removed_tokens"),
            sessions_input,
        ]);
    }
    t.write(o, "No background request yet.");
    write_guardian(o, s);
}

fn write_guardian(o: &mut String, s: &Value) {
    let Some(gd) = at(s, "/guardian") else { return };
    if int(gd, "/threads").unwrap_or(0) == 0 {
        return;
    }
    let without = int(gd, "/threads_without_parent")
        .filter(|x| *x > 0)
        .map(|x| format!(" ({} threads recorded without a parent)", group(x)))
        .unwrap_or_default();
    let per_parent = float(gd, "/requests_per_parent")
        .map(|x| format!(" ({x:.1} per parent)"))
        .unwrap_or_default();
    let split = |k: &str| -> String {
        let b = gd.pointer(&format!("/splits/{k}")).filter(|b| !b.is_null());
        b.map_or_else(
            || "0".to_string(),
            |b| format!("{} requests, {} tokens", n(b, "/requests"), n(b, "/shareable_uncached")),
        )
    };
    let _ = writeln!(
        o,
        "\nGuardian: {} threads for {} parent threads{without}, {} requests{per_parent}; {} threads had a single request ({} of the threads' input). \
         Common prefix with the previous request of the same parent (a measurement): {} requests had a previous sibling, {} of them shared 1,024 tokens or more; \
         at most {} tokens (about {} weighted) were shared with the previous request and yet not cached (an upper bound). \
         By cause: overlapped {}; gap over 300 s {}; key the same {}, key different {}; lge changed bytes {}; same thread {}, new thread {}.",
        n(gd, "/threads"),
        n(gd, "/parents"),
        n(gd, "/requests"),
        n(gd, "/single_request_threads"),
        pct(float(gd, "/single_request_input_tokens"), float(gd, "/thread_input_tokens")),
        n(gd, "/with_previous"),
        n(gd, "/long_prefix"),
        n(gd, "/shareable_uncached"),
        n(gd, "/shareable_uncached_weighted"),
        split("overlapped"),
        split("gap_over_300s"),
        split("key_same"),
        split("key_different"),
        split("lge_changed_bytes"),
        split("same_thread"),
        split("new_thread"),
    );
}

fn write_rules(o: &mut String, s: &Value) {
    let rules = list(s, "/rules");
    let cohorts = list(s, "/cohorts");
    if rules.is_none() && cohorts.is_none() {
        return;
    }
    heading(o, "Rules");
    if let Some(th) = at(s, "/thresholds") {
        let _ = writeln!(
            o,
            "Each rule runs off, in shadow (what it would save is counted) or on. A lossy rule whose outputs are recovered more than {} of the time over at least {} judged outputs moves to shadow by itself; below {} it is healthy. Counts are over the stored sessions of the last {} days.\n",
            rate(float(th, "/demote_above")),
            n(th, "/min_judged"),
            rate(float(th, "/healthy_below")),
            n(th, "/window_days"),
        );
    }
    if let Some(rules) = rules {
        let mut t = Table::new(&[
            "Rule",
            "Mode",
            "#Outputs",
            "#Tokens before",
            "#After",
            "#Shadow would save",
            "#Recovered",
            "#Judged",
            "Status",
        ]);
        for r in rules {
            let kind = text(r, "/kind").unwrap_or("");
            let mut mode = text(r, "/mode").unwrap_or(DASH).to_string();
            if kind == "measure_only" && mode == "on" {
                mode = "shadow".to_string();
            }
            let status = if let Some(why) = text(r, "/demoted") {
                format!("in shadow: {why}")
            } else if flag(r, "/healthy") {
                "healthy".to_string()
            } else if kind == "lossy" && int(r, "/stats/judged").unwrap_or(0) > 0 {
                "judging".to_string()
            } else {
                String::new()
            };
            let shadow = float(r, "/stats/shadow_tokens_before")
                .map(|b| group((b - float(r, "/stats/shadow_tokens_after").unwrap_or(0.0)) as i64))
                .unwrap_or_else(|| DASH.to_string());
            let id = text(r, "/id").unwrap_or(DASH);
            let rule = if kind.is_empty() {
                id.to_string()
            } else {
                format!("{id} ({})", kind.replace('_', " "))
            };
            t.row(vec![
                rule,
                mode,
                n(r, "/stats/items"),
                n(r, "/stats/tokens_before"),
                n(r, "/stats/tokens_after"),
                shadow,
                rate(float(r, "/rate")),
                n(r, "/stats/judged"),
                status,
            ]);
        }
        t.write(o, "No rule data yet.");
    }
    if let Some(cohorts) = cohorts {
        let _ = writeln!(o, "\nSkeletons of first reads, by language and size (measured):");
        let mut t = Table::new(&["Language and size", "#Reads", "#Would save", "#Patched within 5 calls"]);
        for c in cohorts {
            t.row(vec![
                text(c, "/cohort").unwrap_or(DASH).to_string(),
                n(c, "/reads"),
                n(c, "/would_save_tokens"),
                n(c, "/patched_within_5"),
            ]);
        }
        t.write(o, "No first read of an indexed file of 16 KB or more yet.");
    }
}

fn write_holdout(o: &mut String, s: &Value) {
    let Some(h) = at(s, "/holdout").filter(|h| h.is_object()) else {
        return;
    };
    heading(o, "Holdout");
    if let Some(share) = float(s, "/settings/holdout_share").or_else(|| float(h, "/share")) {
        let _ = writeln!(
            o,
            "Share of new conversations held out (every rule in shadow, nothing added): {share}\n"
        );
    }
    const METRICS: [(&str, &str); 7] = [
        ("input_tokens", "Input / session"),
        ("cached_input_tokens", "Cached"),
        ("uncached_input_tokens", "Uncached"),
        ("output_tokens", "Output"),
        ("reasoning_tokens", "Reasoning"),
        ("requests", "Requests"),
        ("removed_tokens_sent", "Kept out by lge"),
    ];
    let diffs: HashMap<&str, &Value> = list(h, "/differences")
        .unwrap_or(&[])
        .iter()
        .filter_map(|d| Some((text(d, "/metric")?, d)))
        .collect();
    // The headings carry the session count of each arm.
    let proxy_head = format!("#Through lge ({})", n(h, "/proxy/sessions"));
    let held_head = format!("#Held out ({})", n(h, "/holdout/sessions"));
    let mut t = Table::new(&[
        "Median per session",
        &proxy_head,
        &held_head,
        "#Difference (95% interval)",
    ]);
    for (key, label) in METRICS {
        let diff = diffs.get(key).map_or_else(
            || DASH.to_string(),
            |d| format!("{} ({} to {})", n(d, "/difference"), n(d, "/low"), n(d, "/high")),
        );
        t.row(vec![
            label.to_string(),
            n(h, &format!("/proxy/medians/{key}")),
            n(h, &format!("/holdout/medians/{key}")),
            diff,
        ]);
    }
    t.write(o, "");
    let note = join(
        [
            text(h, "/note").map(|x| format!("{x}: each arm needs 30 sessions of the last 30 days.")),
            text(h, "/left_out_note").map(|x| {
                format!("{x}. The arms measure all of lge's changes together (rules, packs and, with router_apply on, the effort).")
            }),
        ],
        " ",
    );
    if !note.is_empty() {
        let _ = writeln!(o, "{note}");
    }
}

fn write_cache(o: &mut String, s: &Value) {
    let monitor = at(s, "/monitor");
    let conversations = list(s, "/prefix/conversations");
    if monitor.is_none() && conversations.is_none() {
        return;
    }
    heading(o, "Prompt cache");
    if let Some(m) = monitor {
        let mut t = Table::new(&["", "#"]);
        for (label, ptr) in [
            ("Breaks caused by lge", "/lge_breaks"),
            ("Client breaks", "/client_breaks"),
            ("Cold restarts", "/cold_restarts"),
            ("Tokens not cached after them", "/cold_tokens"),
            ("Requests sent again", "/raw_retries"),
        ] {
            if let Some(v) = num(m, ptr) {
                t.row(vec![label.to_string(), v]);
            }
        }
        t.write(o, "");
        o.push('\n');
    }
    if let Some(conversations) = conversations {
        let mut t = Table::new(&["Session", "#lge breaks", "#Client breaks", "#Cold restarts"]);
        let broke = |c: &Value| {
            ["/lge_breaks", "/client_breaks", "/cold_restarts"]
                .iter()
                .any(|p| int(c, p).unwrap_or(0) != 0)
        };
        for c in conversations.iter().filter(|c| broke(c)) {
            let fallback = text(c, "/id").unwrap_or(DASH);
            let notes = t.row(vec![
                cut(&task_of(c, fallback), TASK_CHARS),
                n(c, "/lge_breaks"),
                n(c, "/client_breaks"),
                n(c, "/cold_restarts"),
            ]);
            if int(c, "/lge_breaks").unwrap_or(0) > 0 {
                notes.push(format!(
                    "lge changed item {}: a bug, please report it",
                    n(c, "/first_lge_break_item")
                ));
            }
        }
        t.write(o, "No cache break and no cold restart.");
    }
}

fn write_prefix(o: &mut String, s: &Value) {
    let fixed = at(s, "/prefix/fixed");
    let ghosts = list(s, "/ghost_tools");
    if let Some(fixed) = fixed {
        heading(o, "Fixed prefix");
        let _ = writeln!(
            o,
            "What Codex sends at the start of every request, by tool group, since the daemon started (lge never changes it).\n"
        );
        let mut t = Table::new(&["Group", "#Tools", "#Tokens per request", "Configuration entry"]);
        t.row(vec![
            "Instructions".into(),
            String::new(),
            n(fixed, "/instructions_tokens"),
            String::new(),
        ]);
        for g in list(fixed, "/groups").unwrap_or(&[]) {
            let mut name = text(g, "/group").unwrap_or(DASH).to_string();
            if flag(g, "/mcp") {
                name.push_str(" [MCP]");
            }
            t.row(vec![
                name,
                n(g, "/tools"),
                n(g, "/tokens"),
                text(g, "/config").unwrap_or("").to_string(),
            ]);
        }
        t.write(o, "");
    }
    if let Some(ghosts) = ghosts {
        heading(o, "Unused tools");
        let _ = writeln!(
            o,
            "Tools never called since the daemon started (a call inside an exec script counts by its name there).\n"
        );
        let mut t = Table::new(&[
            "Tool",
            "Group",
            "#Tokens per request",
            "#Requests seen",
            "Configuration entry",
        ]);
        for g in ghosts.iter().take(50) {
            t.row(vec![
                text(g, "/name").unwrap_or(DASH).to_string(),
                text(g, "/group").unwrap_or(DASH).to_string(),
                n(g, "/tokens"),
                n(g, "/requests_seen"),
                text(g, "/config").unwrap_or("").to_string(),
            ]);
        }
        let groups = fixed.and_then(|f| list(f, "/groups")).unwrap_or(&[]);
        t.write(
            o,
            if groups.is_empty() {
                "No request seen since the daemon started."
            } else {
                "Every tool in the definitions was called."
            },
        );
        if ghosts.len() > 50 {
            let _ = writeln!(o, "and {} more", group((ghosts.len() - 50) as i64));
        }
    }
}

fn write_modes(o: &mut String, s: &Value) {
    let Some(modes) = list(s, "/modes") else { return };
    heading(o, "Before and after");
    let mut t = Table::new(&[
        "",
        "#Sessions",
        "#Input / session",
        "#Cached",
        "#Output / session",
        "#Reasoning / session",
    ]);
    for m in modes {
        let mode = text(m, "/mode").unwrap_or(DASH);
        let label = match mode {
            "history" => "Past sessions (before lge)",
            "proxy" => "Through the proxy",
            "proxy-holdout" => "Held out (rules in shadow)",
            "ab" => "lge ab runs (both arms)",
            other => other,
        };
        let sessions = float(m, "/sessions").unwrap_or(0.0).max(1.0);
        let per = |ptr: &str| float(m, ptr).map_or_else(|| DASH.to_string(), |x| group((x / sessions).round() as i64));
        t.row(vec![
            label.to_string(),
            n(m, "/sessions"),
            per("/input_tokens"),
            pct(float(m, "/cached_input_tokens"), float(m, "/input_tokens")),
            per("/output_tokens"),
            per("/reasoning_tokens"),
        ]);
    }
    t.write(o, "Nothing measured yet.");
    if !modes.is_empty() {
        let _ = writeln!(
            o,
            "Over every stored session. Sessions of an lge ab run are counted apart; runs recorded before lge tagged them count as through the proxy."
        );
    }
}

fn write_settings(o: &mut String, s: &Value) {
    let Some(st) = at(s, "/settings").filter(|v| v.is_object()) else {
        return;
    };
    let on_off = |ptr: &str| {
        at(st, ptr)
            .and_then(Value::as_bool)
            .map(|b| if b { "on" } else { "off" }.to_string())
    };
    let router = text(st, "/router_apply").map(|r| {
        match r {
            "off" => "off (log the recommendation only)",
            "low-only" => "low only (a change in one file)",
            "low-and-medium" => "low and medium (one service)",
            other => other,
        }
        .to_string()
    });
    let mut t = Table::new(&["", ""]);
    for (label, value) in [
        (
            "Add graph context after a message about the code",
            on_off("/inject_context"),
        ),
        ("Apply the rules to tool output", on_off("/shorten_outputs")),
        (
            "Index a git repository the first time the agent works in it",
            on_off("/auto_index"),
        ),
        (
            "Tokens of graph context in one request at most",
            num(st, "/pack_conversation_budget"),
        ),
        ("Lower the reasoning effort when the router recommends it", router),
    ] {
        if let Some(v) = value {
            t.row(vec![format!("{label}:"), v]);
        }
    }
    if !t.rows.is_empty() {
        heading(o, "Settings");
        t.write(o, "");
    }
}

// ---------------------------------------------------------------------------
// Redaction

/// Keys whose value is a path: an absolute one becomes its last component.
const PATH_KEYS: [&str; 7] = [
    "root",
    "path",
    "repo",
    "ignore_file",
    "codex_home",
    "workspaces",
    "data_dir",
];

/// Where a home folder starts, whoever's it is.
const HOME_PREFIXES: [&str; 3] = ["/Users/", "/home/", "\\Users\\"];

/// The provider addresses lge uses by default; any other upstream is redacted.
const DEFAULT_UPSTREAMS: [&str; 2] = ["https://api.openai.com/v1", "https://chatgpt.com/backend-api/codex"];

/// Rewrite the status (and the what-if) for sharing: every session's task
/// becomes `session N`, numbered in the sessions table's order and the same
/// in every section (its id too); a path under a home folder or a listed
/// folder, anywhere in any string, becomes its last component; an upstream
/// other than the default is hidden. Numbers are kept.
pub fn redact(status: &mut Value, whatif: Option<&mut Value>, home: Option<&str>) {
    let mut r = Redactor::new(status, whatif.as_deref(), home);
    r.value(status, None);
    if let Some(w) = whatif {
        r.value(w, None);
    }
}

struct Redactor {
    /// Session id (or `task:` and its text, without an id) -> label.
    labels: HashMap<String, String>,
    /// Absolute folders the status names (listed folders, graphs, workspaces).
    roots: Vec<String>,
    home: Option<String>,
}

impl Redactor {
    fn new(status: &Value, whatif: Option<&Value>, home: Option<&str>) -> Redactor {
        let home = home
            .map(|h| h.trim_end_matches(['/', '\\']).to_string())
            .filter(|h| !h.is_empty());
        let mut r = Redactor {
            labels: HashMap::new(),
            roots: Vec::new(),
            home,
        };
        // Labels in the table's order first, then the sessions only other sections name.
        if let Some(rows) = list(status, "/sessions") {
            for row in table_order(rows) {
                r.label(row.as_object());
            }
        }
        for ptr in ["/long_sessions/sessions", "/prefix/conversations"] {
            for row in list(status, ptr).unwrap_or(&[]) {
                r.label(row.as_object());
            }
        }
        if let Some(w) = whatif {
            for ptr in ["/sessions", "/explain"] {
                for row in list(w, ptr).unwrap_or(&[]) {
                    r.label(row.as_object());
                }
            }
        }
        let mut roots: Vec<&str> = Vec::new();
        for ptr in ["/repos", "/graphs/graphs"] {
            for x in list(status, ptr).unwrap_or(&[]) {
                roots.extend(text(x, "/root"));
                roots.extend(text(x, "/path"));
            }
        }
        for x in list(status, "/workspaces").unwrap_or(&[]) {
            roots.extend(x.as_str());
        }
        roots.extend(text(status, "/codex_home"));
        for x in list(status, "/sessions").unwrap_or(&[]) {
            roots.extend(text(x, "/repo"));
        }
        let mut roots: Vec<String> = roots
            .into_iter()
            .filter(|p| is_absolute(p))
            .map(|p| p.trim_end_matches(['/', '\\']).to_string())
            .filter(|p| p.len() > 1)
            .collect();
        // Longest first, so a folder inside another is replaced whole.
        roots.sort_by_key(|p| std::cmp::Reverse(p.len()));
        roots.dedup();
        r.roots = roots;
        r
    }

    /// The label of a session object, given on first sight.
    fn label(&mut self, obj: Option<&Map<String, Value>>) -> String {
        let key = obj.and_then(|o| {
            ["id", "session_id"]
                .iter()
                .find_map(|k| o.get(*k).and_then(Value::as_str).filter(|s| !s.is_empty()))
                .map(str::to_string)
                .or_else(|| o.get("task").and_then(Value::as_str).map(|t| format!("task:{t}")))
        });
        let next = self.labels.len() + 1;
        match key {
            Some(k) => self
                .labels
                .entry(k)
                .or_insert_with(|| format!("session {next}"))
                .clone(),
            None => {
                // Nothing to know it by: a label of its own.
                self.labels.insert(format!("unnamed:{next}"), format!("session {next}"));
                format!("session {next}")
            }
        }
    }

    fn value(&mut self, v: &mut Value, key: Option<&str>) {
        match v {
            Value::String(s) => *s = self.string(s, key),
            Value::Array(items) => {
                for item in items {
                    self.value(item, key);
                }
            }
            Value::Object(map) => {
                let has_task = map.contains_key("task");
                let has_sid = map.get("session_id").is_some_and(Value::is_string);
                if has_task || has_sid {
                    let label = self.label(Some(map));
                    if has_task {
                        map.insert("task".to_string(), Value::String(label.clone()));
                        if map.get("id").is_some_and(Value::is_string) {
                            map.insert("id".to_string(), Value::String(label.clone()));
                        }
                    }
                    if has_sid {
                        map.insert("session_id".to_string(), Value::String(label));
                    }
                }
                for (k, mut val) in std::mem::take(map) {
                    self.value(&mut val, Some(&k));
                    map.insert(self.text(&k), val);
                }
            }
            _ => {}
        }
    }

    fn string(&self, s: &str, key: Option<&str>) -> String {
        match key {
            Some("upstream_api" | "upstream_chatgpt") if !DEFAULT_UPSTREAMS.contains(&s) => {
                "(custom upstream, redacted)".to_string()
            }
            Some(k) if PATH_KEYS.contains(&k) && is_absolute(s) => self.short(s),
            _ => self.text(s),
        }
    }

    /// Free text with every private path in it shortened to its last component.
    fn text(&self, s: &str) -> String {
        let mut s = s.to_string();
        // A folder with a space in it cannot be found as one word.
        for root in self.roots.iter().filter(|r| r.contains(is_delimiter)) {
            if s.contains(root.as_str()) {
                s = s.replace(root.as_str(), &self.short(root));
            }
        }
        let mut out = String::with_capacity(s.len());
        let mut word = String::new();
        for c in s.chars() {
            if is_delimiter(c) {
                out.push_str(&self.word(&word));
                word.clear();
                out.push(c);
            } else {
                word.push(c);
            }
        }
        out.push_str(&self.word(&word));
        out
    }

    fn word(&self, w: &str) -> String {
        let core = w.trim_end_matches(['.', '!', '?']);
        if core.is_empty() || !self.private(core) {
            return w.to_string();
        }
        format!("{}{}", self.short(core), &w[core.len()..])
    }

    /// A path under a home folder or a folder the status names.
    fn private(&self, w: &str) -> bool {
        let bare = w.trim_end_matches(['/', '\\']);
        let under = |root: &str| bare == root || w.contains(&format!("{root}/")) || w.contains(&format!("{root}\\"));
        HOME_PREFIXES.iter().any(|p| w.contains(p))
            || self.home.as_deref().is_some_and(under)
            || self.roots.iter().any(|r| under(r))
    }

    /// The last component of a path; a home folder itself is `~`.
    fn short(&self, path: &str) -> String {
        let bare = path.trim_end_matches(['/', '\\']);
        if bare.is_empty() {
            return path.to_string();
        }
        if self.home.as_deref() == Some(bare) || is_home_folder(bare) {
            return "~".to_string();
        }
        bare.rsplit(['/', '\\'])
            .find(|c| !c.is_empty())
            .unwrap_or(bare)
            .to_string()
    }
}

/// Characters that end a path inside free text.
fn is_delimiter(c: char) -> bool {
    c.is_whitespace() || "\"'`,;:()[]{}<>|=".contains(c)
}

fn is_absolute(p: &str) -> bool {
    let b = p.as_bytes();
    p.starts_with('/')
        || p.starts_with('\\')
        || p.starts_with("~/")
        || (b.len() > 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'\\' | b'/'))
}

/// `/Users/<name>` or `/home/<name>` and nothing below it.
fn is_home_folder(p: &str) -> bool {
    HOME_PREFIXES.iter().any(|prefix| {
        p.find(prefix).is_some_and(|i| {
            let rest = &p[i + prefix.len()..];
            !rest.is_empty() && !rest.contains(['/', '\\'])
        })
    })
}

/// The daemon's address in a redacted report: kept when it is this machine.
fn shareable_url(url: &str) -> String {
    let host = url.split("://").nth(1).unwrap_or(url).split('/').next().unwrap_or("");
    let name = if host.starts_with('[') {
        host.split(']').next().map(|h| format!("{h}]")).unwrap_or_default()
    } else {
        host.split(':').next().unwrap_or("").to_string()
    };
    if ["127.0.0.1", "localhost", "[::1]", "0.0.0.0"].contains(&name.as_str()) {
        url.to_string()
    } else {
        "(daemon address redacted)".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const HOME: &str = "/Users/tester";

    fn header() -> Header {
        Header {
            cli_version: "9.9.9".into(),
            url: DEFAULT_URL.into(),
            local_time: "2026-01-02 03:04 +00:00".into(),
        }
    }

    fn opts(sessions: usize, redacted: bool) -> Options {
        Options { sessions, redacted }
    }

    /// A status shaped like the current daemon's `status()`: synthetic values only.
    fn current() -> Value {
        serde_json::from_str(
            r#"
            {
              "version": "0.0.7",
              "uptime_seconds": 7260,
              "listen": "127.0.0.1:8787",
              "upstream_api": "https://gateway.example.test/v1",
              "upstream_chatgpt": "https://chatgpt.com/backend-api/codex",
              "workspaces": ["/Users/tester/work"],
              "codex_home": "/Users/tester/.codex",
              "settings": {
                "inject_context": true,
                "shorten_outputs": true,
                "shorten_reads": false,
                "context_budget": 1500,
                "pack_conversation_budget": 3000,
                "auto_index": false,
                "rules": {"check": "on", "skeleton": "shadow"},
                "holdout_share": 0.25,
                "demoted": {"line_clip": "recovered 40% of 50 outputs"},
                "rule_since": {},
                "router_apply": "low-only",
                "ab_arm": ""
              },
              "settings_error": "cannot parse /Users/tester/.lge/daemon.json: expected value",
              "repos": [
                {"root": "/Users/tester/work/demo-app", "name": "demo-app", "state": "ready", "detail": "", "files": 1204, "symbols": 23456, "services": 2, "memories": 17, "index_seconds": 3.25, "graph_dir": "demo-app-0a1b2c3d", "bulky": [{"path": "vendor/blobs", "bytes": 52428800, "files": 3000, "reason": "large generated files", "ignore_line": "/vendor/blobs/", "ignore_file": "/Users/tester/work/demo-app/.gitignore"}]},
                {"root": "/Users/tester/work/broken-svc", "name": "broken-svc", "state": "failed", "detail": "cannot read /Users/tester/work/broken-svc/src/main.rs: permission denied", "files": 0, "symbols": 0, "services": 0, "memories": 0, "bulky": [], "index_seconds": null, "graph_dir": "broken-svc-11223344"}
              ],
              "graphs": {
                "graphs": [
                  {"dir": "demo-app-0a1b2c3d", "path": "/Users/tester/.lge/graphs/demo-app-0a1b2c3d", "root": "/Users/tester/work/demo-app", "bytes": 15728640, "listed": true, "exists": true},
                  {"dir": "old-tool-99887766", "path": "/Users/tester/.lge/graphs/old-tool-99887766", "root": "/srv/scratch/old-tool", "bytes": 1048576, "listed": false, "exists": false}
                ],
                "total_bytes": 16777216,
                "unlisted": 1,
                "unlisted_bytes": 1048576
              },
              "stored_records": 42,
              "modes": [
                {"mode": "history", "sessions": 10, "input_tokens": 5000000, "cached_input_tokens": 4000000, "output_tokens": 100000, "reasoning_tokens": 50000},
                {"mode": "proxy", "sessions": 3, "input_tokens": 1234567, "cached_input_tokens": 1049382, "output_tokens": 20000, "reasoning_tokens": 5000}
              ],
              "saved": {
                "outputs_shortened": 12,
                "tokens_before": 40000,
                "tokens_after": 30000,
                "contexts_added": 4,
                "context_tokens": 2400,
                "since": 1767000000,
                "by_rule": {}
              },
              "sessions": [
                {"id": "aaaa1111-0000", "task": "Rename the widget factory in [$lint](/Users/tester/.codex/skills/lint/SKILL.md) code", "model": "gpt-test", "repo": "/Users/tester/work/demo-app", "mode": "proxy", "background": null, "ab_arm": null, "requests": 12, "mean_input_tokens": 20000, "packs_added": 2, "packs_known": 1, "packs_over_budget": 0, "packs_too_large": 0, "packs_retired": 0, "packs_no_graph": 0, "started_at": 1767000000, "finished_at": 1767003600, "input_tokens": 240000, "cached_input_tokens": 200000, "output_tokens": 3000, "reasoning_tokens": 1000, "recommended_level": "low", "applied_level": "low", "removed_tokens_sent": 5000, "added_tokens_sent": 1000, "net_tokens": 4000, "net_share": 0.0164, "lge_breaks": 0, "client_breaks": 1, "first_lge_break_item": null, "cold_restarts": 0, "cold_tokens": 0, "raw_retries": 0},
                {"id": "bbbb2222-0000", "task": "Investigate the flaky checkout test", "model": "gpt-test", "repo": "/Users/tester/work/demo-app", "mode": "proxy", "background": null, "ab_arm": null, "requests": 60, "mean_input_tokens": 160000, "started_at": 1767100000, "finished_at": 1767200000, "input_tokens": 9600000, "cached_input_tokens": 9000000, "output_tokens": 15000, "reasoning_tokens": 4000, "removed_tokens_sent": 0, "added_tokens_sent": 0, "net_tokens": 0, "net_share": 0.0, "lge_breaks": 1, "client_breaks": 0, "first_lge_break_item": 7, "cold_restarts": 2},
                {"id": "cccc3333-0000", "task": "", "model": "gpt-test", "repo": null, "mode": "proxy-background", "background": "review", "requests": null, "mean_input_tokens": null, "input_tokens": 1000, "cached_input_tokens": 0, "output_tokens": 10, "reasoning_tokens": 0, "removed_tokens_sent": 0, "added_tokens_sent": 0, "net_tokens": 0, "net_share": 0.0}
              ],
              "rules": [
                {"id": "line_clip", "kind": "lossy", "mode": "on", "effective_mode": "shadow", "demoted": "recovered 40% of 50 outputs", "since": null, "stats": {"items": 50, "tokens_before": 10000, "tokens_after": 6000, "shadow_tokens_before": 0, "shadow_tokens_after": 0, "judged": 50, "recovered": 20}, "rate": 0.4, "healthy": false},
                {"id": "check", "kind": "lossless", "mode": "on", "effective_mode": "on", "demoted": null, "since": null, "stats": {"items": 3, "tokens_before": 900, "tokens_after": 300, "judged": 0}, "rate": null, "healthy": true}
              ],
              "thresholds": {"min_judged": 30, "demote_above": 0.2, "healthy_below": 0.05, "window_days": 30},
              "cohorts": [
                {"cohort": "rust:16-64KB", "reads": 5, "would_save_tokens": 8000, "patched_within_5": 1}
              ],
              "holdout": {
                "proxy": {"arm": "proxy", "sessions": 31, "medians": {"input_tokens": 100000.4, "requests": 12.0}},
                "holdout": {"arm": "proxy-holdout", "sessions": 30, "medians": {"input_tokens": 120000.0, "requests": 13.0}},
                "differences": [
                  {"metric": "input_tokens", "difference": -19999.6, "low": -30000.0, "high": -5000.0}
                ],
                "note": null,
                "left_out": 2,
                "left_out_note": "2 sessions recorded before v0.0.2 have no request count and are left out",
                "share": 0.25
              },
              "prefix": {
                "lge_breaks": 1,
                "client_breaks": 1,
                "conversations": [
                  {"id": "aaaa1111-0000", "task": "Rename the widget factory in [$lint](/Users/tester/.codex/skills/lint/SKILL.md) code", "lge_breaks": 0, "client_breaks": 1, "first_lge_break_item": null, "cold_restarts": 0},
                  {"id": "bbbb2222-0000", "task": "Investigate the flaky checkout test", "lge_breaks": 1, "client_breaks": 0, "first_lge_break_item": 7, "cold_restarts": 2}
                ],
                "fixed": {
                  "instructions_tokens": 4100,
                  "tool_tokens": 9000,
                  "groups": [
                    {"group": "built-in", "mcp": false, "tools": 5, "tokens": 7000, "requests_seen": 20, "config": ""},
                    {"group": "notes_server", "mcp": true, "tools": 2, "tokens": 2000, "requests_seen": 20, "config": "[mcp_servers.notes_server]"}
                  ]
                }
              },
              "ghost_tools": [
                {"name": "mcp__notes_server__search", "group": "notes_server", "mcp": true, "tokens": 1200, "requests_seen": 20, "config": "[mcp_servers.notes_server]"}
              ],
              "monitor": {"raw_retries": 1, "cold_restarts": 2, "cold_tokens": 150000, "lge_breaks": 1, "client_breaks": 1},
              "annuity": {"removed_tokens_sent": 5000, "added_tokens_sent": 1000, "net_tokens": 4000},
              "packs": {
                "no_graph": 1,
                "added": 2,
                "known": 1,
                "over_budget": 0,
                "retired": 0,
                "too_large": 0,
                "tokens_sent": 1000,
                "tokens_sent_uncounted": 0,
                "tokens_sent_background": 0,
                "since": 1767000000,
                "holdout_would_add": 3,
                "at_limit": 0,
                "at_limit_live_would_add": 0
              },
              "unread_scripts": {"outputs": 2, "tokens": 1500, "tool_output_tokens": 30000, "share": 0.05},
              "background": [
                {"label": "review", "sessions": 1, "requests": 4, "input_tokens": 1000, "request_input_tokens": 1000, "input_counted_requests": 3, "cached_tokens": 200, "uncached_tokens": 600, "cached_basis_tokens": 800, "removed_tokens": 0},
                {"label": "title", "sessions": null, "requests": 9, "input_tokens": 0, "request_input_tokens": 0, "input_counted_requests": 0, "cached_tokens": null, "uncached_tokens": null, "cached_basis_tokens": 0, "removed_tokens": 0}
              ],
              "guardian": {"threads": 0, "requests": 0, "parents": 0, "requests_per_parent": null, "splits": {}},
              "long_sessions": {
                "min_requests": 50,
                "min_mean_input_tokens": 150000,
                "sessions": [
                  {"id": "bbbb2222-0000", "task": "Investigate the flaky checkout test", "requests": 60, "mean_input_tokens": 160000, "started_at": 1767100000, "finished_at": 1767200000}
                ],
                "note": "Every request of these sessions re-sends that many input tokens."
              },
              "a_field_from_the_future": {"anything": [1, 2, 3]}
            }
            "#,
        )
        .expect("fixture")
    }

    /// A status shaped like v0.0.4's: fewer fields, other shapes.
    fn v004() -> Value {
        serde_json::from_str(
            r#"
            {
              "version": "0.0.4",
              "uptime_seconds": 60,
              "listen": "127.0.0.1:8787",
              "workspaces": [],
              "codex_home": null,
              "settings": {
                "inject_context": true,
                "shorten_outputs": false,
                "shorten_reads": false,
                "context_budget": 1500,
                "auto_index": true,
                "rules": {},
                "holdout_share": 0.0,
                "demoted": {},
                "rule_since": {}
              },
              "settings_error": null,
              "repos": [
                {"root": "/Users/tester/work/demo-app", "name": "demo-app", "state": "indexing", "detail": "", "files": 0, "symbols": 0, "memories": 0}
              ],
              "modes": [
                {"mode": "proxy", "sessions": 1, "input_tokens": 5000, "cached_input_tokens": 0, "output_tokens": 100, "reasoning_tokens": 0}
              ],
              "saved": {
                "outputs_shortened": 0,
                "tokens_before": 0,
                "tokens_after": 0,
                "contexts_added": 0,
                "context_tokens": 0
              },
              "sessions": [
                {"id": "dddd4444", "task": "Add a health endpoint", "model": "gpt-test", "repo": null, "mode": "proxy", "background": null, "requests": null, "mean_input_tokens": null, "packs_added": 0, "input_tokens": 5000, "cached_input_tokens": 0, "output_tokens": 100, "reasoning_tokens": 0, "removed_tokens_sent": 0, "added_tokens_sent": 0, "net_tokens": 0, "net_share": 0.0}
              ],
              "rules": [],
              "thresholds": {"min_judged": 30, "demote_above": 0.2, "healthy_below": 0.05, "window_days": 30},
              "cohorts": [],
              "holdout": {
                "proxy": {"sessions": 0, "medians": {}},
                "holdout": {"sessions": 0, "medians": {}},
                "differences": [],
                "note": "not enough sessions",
                "share": 0.0
              },
              "prefix": {
                "lge_breaks": 0,
                "client_breaks": 0,
                "conversations": [],
                "fixed": {"instructions_tokens": 0, "tool_tokens": 0, "groups": []}
              },
              "ghost_tools": [],
              "monitor": {"raw_retries": 0, "cold_restarts": 0, "cold_tokens": 0, "lge_breaks": 0, "client_breaks": 0},
              "annuity": {"removed_tokens_sent": 0, "added_tokens_sent": 0, "net_tokens": 0},
              "packs": {"added": 0, "known": 0, "over_budget": 0, "retired": 0, "tokens_sent": 0, "holdout_would_add": 0},
              "background": [
                {"label": "review", "sessions": 2, "requests": 3, "input_tokens": 900}
              ],
              "long_sessions": {"min_requests": 50, "min_mean_input_tokens": 150000, "sessions": [], "note": null}
            }
            "#,
        )
        .expect("fixture")
    }

    fn whatif() -> Value {
        serde_json::from_str(
            r#"
            {
              "generated_at": 1767300000,
              "days": 30,
              "weights": {"cached": 0.1},
              "rollouts_read": 5,
              "total": null,
              "explain": [],
              "sessions": [
                {"session_id": "bbbb2222-0000", "model": "gpt-test", "work_requests": 60, "limit": 200000, "limit_source": "observed", "confidence": "medium", "model_run": {"compactions": 2}, "error_pct": {"cost": 3.21}, "candidates": [{"limit": 150000, "run": {"compactions": 4}, "delta_pct": -12.4, "thrash": false, "flags": []}, {"limit": 100000, "run": {"compactions": 9}, "delta_pct": null, "thrash": true, "flags": []}, {"limit": 200000, "run": {"compactions": 2}, "delta_pct": 0.0, "thrash": false, "flags": ["session limit"]}]}
              ],
              "caveats": ["An estimate."]
            }
            "#,
        )
        .expect("fixture")
    }

    #[test]
    fn current_status_renders_every_section_in_order() {
        let out = render(&current(), Some(&whatif()), &opts(25, false), &header());
        let sections = [
            "# lge report",
            "## Cards",
            "## Folders in the graph",
            "## Sessions through the proxy",
            "Long sessions:",
            "What if Codex compacted sooner?",
            "Packs (sessions not held out)",
            "Codex's background requests",
            "## Rules",
            "## Holdout",
            "## Prompt cache",
            "## Fixed prefix",
            "## Unused tools",
            "## Before and after",
            "## Settings",
        ];
        let mut from = 0;
        for s in sections {
            let at = out[from..]
                .find(s)
                .unwrap_or_else(|| panic!("`{s}` missing or out of order:\n{out}"));
            from += at;
        }
        for expected in [
            "lge CLI v9.9.9 · daemon v0.0.7 · http://127.0.0.1:8787",
            "v0.0.7 · listening on 127.0.0.1:8787 · up 121 min",
            "1,234,567",
            "cached 85%",
            "10,000",
            "25% smaller",
            "Settings error: cannot parse",
            "failed: cannot read /Users/tester/work/broken-svc/src/main.rs: permission denied",
            "Searches would read vendor/blobs/ (50 MB, 3,000 files): large generated files",
            "Rename the widget factory in $lint code",
            "router: low (applied)",
            "[background: review] (no typed message)",
            "Investigate the flaky checkout test: 60 requests, 160,000 input tokens each on average",
            "At 150,000: about 12% less weighted input cost, 2 more compactions",
            "At 100,000: Codex would compact every few requests",
            "off by 3.2% from your measured totals",
            "line_clip (lossy)",
            "in shadow: recovered 40% of 50 outputs",
            "40.0%",
            "-20,000 (-30,000 to -5,000)",
            "lge changed item 7: a bug, please report it",
            "notes_server [MCP]",
            "mcp__notes_server__search",
            "Past sessions (before lge)",
            "low only (a change in one file)",
            "+ 1 requests not counted (before v0.0.5)",
        ] {
            assert!(out.contains(expected), "`{expected}` missing:\n{out}");
        }
        // The what-if's own session limit is not a candidate.
        assert!(!out.contains("At 200,000"), "{out}");
        assert!(!out.contains("null"), "{out}");
    }

    #[test]
    fn older_and_empty_statuses_render_without_null() {
        let old = render(&v004(), None, &opts(25, false), &header());
        assert!(!old.contains("null"), "{old}");
        assert!(old.contains("daemon v0.0.4"), "{old}");
        assert!(old.contains("Add a health endpoint"), "{old}");
        // A session of v0.0.4 had no request count: unknown, not 0.
        let row = old
            .lines()
            .find(|l| l.starts_with("Add a health endpoint"))
            .expect("session row");
        assert!(
            row["Add a health endpoint".len()..].trim_start().starts_with('–'),
            "{row}"
        );
        // Fields v0.0.4 did not have are left out, not shown as 0 or null.
        assert!(
            !old.contains("too_large") && !old.contains("larger than a follow-up pack"),
            "{old}"
        );
        assert!(!old.contains("Tokens of graph context in one request"), "{old}");
        assert!(old.contains("review"), "{old}");
        let empty = render(&json!({}), None, &opts(25, true), &header());
        assert!(!empty.contains("null"), "{empty}");
        assert!(empty.contains("daemon unknown"), "{empty}");
        assert!(!empty.contains("## "), "no section without data:\n{empty}");
        // Values of the wrong type are skipped as well.
        let odd = json!({ "sessions": "x", "repos": 3, "modes": [1, null], "holdout": [], "rules": [{}], "packs": {} });
        let out = render(&odd, Some(&json!({})), &opts(25, false), &header());
        assert!(!out.contains("null"), "{out}");
    }

    #[test]
    fn redact_removes_tasks_and_home_paths_everywhere() {
        let mut status = current();
        let mut w = whatif();
        redact(&mut status, Some(&mut w), Some(HOME));
        let text = render(&status, Some(&w), &opts(25, true), &header());
        let json = json_text(&status, Some(&w), true).unwrap();
        for out in [&text, &json] {
            for secret in [
                "tester",
                "widget factory",
                "flaky checkout",
                "/srv/scratch",
                "gateway.example.test",
                "aaaa1111",
                "bbbb2222",
            ] {
                assert!(!out.contains(secret), "`{secret}` left in:\n{out}");
            }
        }
        assert!(text.contains("Redacted: task texts"), "{text}");
        // Labels follow the table (most input first) and are the same in every section.
        assert!(text.contains("session 1: 60 requests"), "long note:\n{text}");
        assert!(text.contains("- session 1 (60 requests"), "what-if:\n{text}");
        assert!(text.contains("session 2  "), "table:\n{text}");
        let table = &text[text.find("## Sessions").unwrap()..text.find("Long sessions:").unwrap()];
        assert!(
            table.find("session 1").unwrap() < table.find("session 2").unwrap(),
            "{table}"
        );
        // Paths become their last component, in the failed folder's error and the bulky warning too.
        assert!(
            text.contains("failed: cannot read main.rs: permission denied"),
            "{text}"
        );
        assert!(text.contains("Add /vendor/blobs/ to .gitignore"), "{text}");
        assert!(text.contains("demo-app"), "{text}");
        assert!(text.contains("Settings error: cannot parse daemon.json"), "{text}");
        // Numbers are kept.
        assert!(text.contains("1,234,567") && text.contains("9,600,000"), "{text}");
        assert!(json.contains("\"redacted\": true"), "{json}");
    }

    #[test]
    fn sessions_option_limits_rows_and_says_how_many_are_left_out() {
        let out = render(&current(), None, &opts(1, false), &header());
        let table = &out[out.find("## Sessions").unwrap()..out.find("Long sessions:").unwrap()];
        assert!(table.contains("Investigate the flaky checkout test  "), "{table}");
        assert!(!table.contains("Rename the widget factory"), "{table}");
        assert!(table.contains("2 of 3 sessions left out"), "{table}");
        let all = render(&current(), None, &opts(25, false), &header());
        assert!(all.contains("0 of 3 sessions left out"), "{all}");
    }

    #[test]
    fn json_round_trips_with_the_whatif_under_its_key() {
        let status = current();
        let out = json_text(&status, Some(&whatif()), false).unwrap();
        let mut back: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(back[WHATIF_KEY], whatif());
        back.as_object_mut().unwrap().remove(WHATIF_KEY);
        assert_eq!(back, status);
        let plain: Value = serde_json::from_str(&json_text(&status, None, false).unwrap()).unwrap();
        assert_eq!(plain, status);
    }

    #[test]
    fn helpers_format_like_the_page() {
        assert_eq!(group(0), "0");
        assert_eq!(group(999), "999");
        assert_eq!(group(1000), "1,000");
        assert_eq!(group(-1234567), "-1,234,567");
        assert_eq!(pct(Some(1.0), Some(3.0)), "33%");
        assert_eq!(pct(Some(1.0), Some(0.0)), DASH);
        assert_eq!(rate(Some(0.1234)), "12.3%");
        assert_eq!(size(52_428_800.0), "50 MB");
        assert_eq!(size(512.0), "512 B");
        assert_eq!(
            clean_task("Use [$skill](/a/b/SKILL.md) and [@file](x.rs)\nnow [not](a)"),
            "Use $skill and @file now [not](a)"
        );
        assert_eq!(cut("abcdef", 4), "abc…");
        assert_eq!(utc_parts(0), (1970, 1, 1, 0, 0, None));
        assert_eq!(utc_parts(1_767_225_600), (2026, 1, 1, 0, 0, None));
    }

    #[test]
    fn redaction_of_free_text_paths() {
        let r = Redactor {
            labels: HashMap::new(),
            roots: vec!["/srv/My Work/app".into(), "/srv/code/app".into()],
            home: Some(HOME.into()),
        };
        assert_eq!(r.text("see /Users/tester/a/b.rs:12."), "see b.rs:12.");
        assert_eq!(r.text("home is /Users/tester, done"), "home is ~, done");
        assert_eq!(r.text("in /home/someone/x/y"), "in y");
        assert_eq!(r.text("at /srv/code/app/src/lib.rs"), "at lib.rs");
        assert_eq!(r.text("at /srv/My Work/app now"), "at app now");
        assert_eq!(r.text("/tmp/other stays"), "/tmp/other stays");
        assert_eq!(r.string("/srv/elsewhere/tool", Some("root")), "tool");
        assert_eq!(r.string("vendor/blobs", Some("path")), "vendor/blobs");
        assert_eq!(shareable_url("http://127.0.0.1:8787"), "http://127.0.0.1:8787");
        assert_eq!(
            shareable_url("http://build-box.internal:8787"),
            "(daemon address redacted)"
        );
    }
}
