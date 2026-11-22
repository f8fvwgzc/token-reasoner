//! Rewriting one OpenAI Responses request body.
//!
//! The body's `input` is the whole conversation as a list of items. This
//! module finds the two kinds of item the proxy acts on, the output of a tool
//! call and a message the user typed, and leaves every other item, and every
//! other field of the body, exactly as it was. Items that carry ids or
//! encrypted reasoning are never opened; tool calls, scripts and patches are
//! read, never changed.
//!
//! Each output is decided once per conversation and replayed byte for byte on
//! every later request ([`Session`]). What a decision may depend on beyond the
//! item itself (a pointer to an identical earlier copy, a re-run sent whole
//! after a shortened one) is read from the earlier items of the request, which
//! never change, so the same request always gives the same bytes.

use crate::command::{is_read_only, normalise, patch_paths, purpose, script_edits, script_plan, Purpose, ScriptPlan};
use crate::rules::{Mode, RuleId};
use crate::session::{output_key, producer_of, same_path, Before, CallView, Draft, Graph, Options, Session};
use crate::shorten::RunStatus;
use regex::Regex;
use serde_json::{json, Value};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::LazyLock;

/// What a request said about itself, and what was changed in it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Seen {
    /// Directory the agent works in, from the environment context.
    pub cwd: Option<String>,
    /// Identifies the conversation across requests.
    pub conversation: Option<String>,
    pub model: Option<String>,
    /// The first message the user typed, cut to 200 chars.
    pub first_user_message: String,
    /// The first typed message as it was, trimmed and cut to 200 chars, even
    /// when context comes before the request. Names a conversation that has
    /// no cache key and no thread header, as it always did.
    pub first_user_raw: String,
    /// The same message cut to [`ROUTED_CHARS`], for the router: a task whose
    /// scope is named after its first 200 characters is routed by all of it.
    pub first_user_text: String,
    /// The request carries a conversation already under way: an item other
    /// than a user, developer or system message (reasoning, a tool call or
    /// its output, a compaction item, an item reference), an assistant
    /// message, more than one typed message, Codex's compaction summary or a
    /// turn-aborted notice, or a `previous_response_id`. False only for a
    /// conversation's genuine first request.
    pub has_history: bool,
    /// Tool outputs replaced in this request (old and new decisions alike).
    pub outputs_replaced: usize,
    /// Context messages present in this request.
    pub contexts_present: usize,
    /// Tokens not sent in this request: over the items replaced in it, the
    /// tokens before less the tokens after.
    pub removed_present: i64,
    /// Tokens of the context items present in this request.
    pub added_present: i64,
    /// One hash per input item as the client sent it.
    pub client_hashes: Vec<u64>,
    /// One hash per client input item as forwarded, with the context item
    /// inserted right after it, if any.
    pub forwarded_hashes: Vec<u64>,
    /// Names of the tools called in the request, each once, in order.
    pub called_tools: Vec<String>,
    /// Names the `exec` scripts of the request call from code mode
    /// (`tools.<name>(...)`), each once, in order.
    pub code_mode_calls: Vec<String>,
    /// Items first decided in this request (outputs, pointers, contexts).
    pub new_decisions: usize,
    /// The input item at which this request's forwarded prefix differs from
    /// the previous request's while the client's does not: a cache break lge
    /// caused.
    pub lge_break_item: Option<usize>,
    /// The first input item the client changed since the previous request.
    pub client_break_item: Option<usize>,
    /// The largest tool outputs new in this request (not in the previous
    /// request of the conversation), largest first, at most
    /// [`LARGEST_NEW_OUTPUTS`].
    pub largest_new_outputs: Vec<OutputFact>,
}

/// Outputs kept in [`Seen::largest_new_outputs`].
pub const LARGEST_NEW_OUTPUTS: usize = 3;

/// Characters of the first typed message the router reads: enough for a task
/// with its scope, while a pasted log does not make the search slow.
pub const ROUTED_CHARS: usize = 4000;

/// One tool output as a request trace shows it. No output text.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OutputFact {
    /// What produced it, cut to 120 characters: the shell command, the
    /// commands of a script, or the tool's name.
    pub command: String,
    /// Tokens of the output as the client sent it.
    pub tokens: usize,
    /// lge sent something else in its place.
    pub shortened: bool,
}

/// A shell command a tool call ran, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ran {
    pub command: String,
    pub workdir: Option<String>,
}

/// An output smaller than this is cheaper to repeat than to point at.
const REPEAT_MIN_BYTES: usize = 2000;

const SHELLS: [&str; 6] = ["bash", "zsh", "sh", "/bin/bash", "/bin/zsh", "/bin/sh"];

/// Names of the function tools that run a shell command.
pub(crate) const SHELL_TOOLS: [&str; 6] = [
    "shell",
    "container.exec",
    "exec_command",
    "local_shell",
    "shell_command",
    "bash",
];

/// Codex tools that change no file (their handlers in codex-rs/core/src/tools/handlers).
const READ_ONLY_TOOLS: [&str; 6] = [
    "update_plan",
    "view_image",
    "list_mcp_resources",
    "list_mcp_resource_templates",
    "read_mcp_resource",
    "tool_search",
];

/// The command line of a shell-type tool call, from its arguments object.
/// Understands `{"command": ["bash", "-lc", "..."]}`, `{"command": "..."}`
/// and `{"cmd": "..."}`.
pub(crate) fn command_of(args: &Value) -> Option<Ran> {
    let workdir = ["workdir", "cwd", "working_directory"]
        .iter()
        .find_map(|k| args.get(*k).and_then(Value::as_str))
        .map(str::to_string);
    let command = match args.get("command").or_else(|| args.get("cmd"))? {
        Value::String(s) => s.clone(),
        Value::Array(parts) => {
            let words: Vec<&str> = parts.iter().filter_map(Value::as_str).collect();
            match words.as_slice() {
                [shell, flag, script] if SHELLS.contains(shell) && matches!(*flag, "-lc" | "-c") => script.to_string(),
                _ => words.join(" "),
            }
        }
        _ => return None,
    };
    Some(Ran { command, workdir })
}

/// The text of a message item: its `input_text` parts joined.
fn message_text(item: &Value) -> Option<String> {
    match item.get("content")? {
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => {
            let texts: Vec<&str> = parts
                .iter()
                .filter(|p| p.get("type").and_then(Value::as_str) == Some("input_text"))
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect();
            (!texts.is_empty()).then(|| texts.join("\n"))
        }
        _ => None,
    }
}

/// Wrappers the agent puts around context it adds as user messages, lower-cased.
/// The last is how Codex opens the summary it writes when it compacts a
/// conversation (codex-rs/prompts/templates/compact/summary_prefix.md).
const INJECTED_STARTS: [&str; 9] = [
    "# agents.md instructions",
    "<environment_context>",
    "<turn_aborted>",
    "<user_shell_command>",
    "<subagent_notification>",
    "<codex_internal_context",
    "<external_",
    "<lge_context>",
    "another language model started to solve this problem and produced a summary",
];

/// True when a user-role message was written by the agent, not typed by the
/// person: environment context, instructions, notices. The agent says so in
/// the item's metadata when it sends one; otherwise the known wrappers decide,
/// and any other message that opens with a tag is treated as not typed.
fn is_injected(item: &Value, text: &str) -> bool {
    let kinds = item
        .get("internal_chat_message_metadata_passthrough")
        .and_then(|m| m.get("content_item_kinds"))
        .and_then(Value::as_array);
    if let Some(kinds) = kinds.filter(|k| !k.is_empty()) {
        return !kinds.iter().filter_map(Value::as_str).any(|k| k.starts_with("user."));
    }
    let t = text.trim_start().to_ascii_lowercase();
    INJECTED_STARTS.iter().any(|m| t.starts_with(m)) || t.starts_with('<')
}

/// The heading Codex puts between the context it adds in front of a message
/// (the IDE's open files and selection, referenced chats) and what the user
/// typed.
const REQUEST_HEADING: &str = "## My request for Codex:";

/// What the user asked, in a typed message that starts with context: the text
/// after the first `## My request for Codex:` line, trimmed. A message that
/// does not start with a `#` heading, or has no such line, is returned as it
/// is; one with nothing after the heading (a message of attachments only)
/// gives an empty string.
pub fn request_text(text: &str) -> Cow<'_, str> {
    // The harness's labels around an attached image (or audio clip) are lines
    // of their own in the joined text: they name a path, and say nothing about the task.
    let labelled = text.contains("<image") || text.contains("</image>") || text.contains("<audio");
    let stripped: Cow<'_, str> = if labelled {
        let is_label = |line: &str| {
            let l = line.trim();
            l == "</image>"
                || l == "<image>"
                || l.starts_with("<image name=")
                || l.starts_with("<audio")
                || l == "</audio>"
        };
        if text.lines().any(is_label) {
            Cow::Owned(text.lines().filter(|l| !is_label(l)).collect::<Vec<_>>().join("\n"))
        } else {
            Cow::Borrowed(text)
        }
    } else {
        Cow::Borrowed(text)
    };
    let t = stripped.trim_start();
    if !t.starts_with('#') {
        return stripped;
    }
    let mut at = 0;
    for line in t.split_inclusive('\n') {
        if line.starts_with(REQUEST_HEADING) {
            return Cow::Owned(t[at + REQUEST_HEADING.len()..].trim().to_string());
        }
        at += line.len();
    }
    stripped
}

/// The text of a message the user typed, or `None` for any other item.
fn typed_text(item: &Value) -> Option<String> {
    if item.get("type").and_then(Value::as_str) != Some("message")
        || item.get("role").and_then(Value::as_str) != Some("user")
    {
        return None;
    }
    message_text(item).filter(|t| !is_injected(item, t))
}

/// True when `path` is named in one of `items` before a message: in a tool
/// call's arguments, in a tool's output, or in a pack lge added.
fn mentioned_in(items: &[Value], path: &str) -> bool {
    let named = |text: &str| crate::session::mentions(text, path);
    items
        .iter()
        .any(|item| match item.get("type").and_then(Value::as_str).unwrap_or("") {
            "function_call" => item.get("arguments").and_then(Value::as_str).is_some_and(named),
            "custom_tool_call" => item.get("input").and_then(Value::as_str).is_some_and(named),
            "local_shell_call" => item.get("action").is_some_and(|a| named(&a.to_string())),
            "function_call_output" | "custom_tool_call_output" | "local_shell_call_output" => {
                match item.get("output") {
                    Some(Value::String(text)) => named(text),
                    Some(Value::Array(parts)) => parts
                        .iter()
                        .filter_map(|p| p.get("text").and_then(Value::as_str))
                        .any(named),
                    _ => false,
                }
            }
            "message" if item.get("role").and_then(Value::as_str) == Some("user") => {
                message_text(item).is_some_and(|t| t.starts_with(CONTEXT_OPEN_TAG) && named(&t))
            }
            _ => false,
        })
}

/// How every pack lge adds starts.
const CONTEXT_OPEN_TAG: &str = "<lge_context>";

/// How Codex's environment context opens, lower-cased.
const ENVIRONMENT_CONTEXT: &str = "<environment_context>";

/// How the summary Codex writes when it compacts a conversation opens, and its
/// notice of an interrupted turn, lower-cased: an injected message that only
/// a conversation already under way carries.
const HISTORY_STARTS: [&str; 2] = [
    "another language model started to solve this problem and produced a summary",
    "<turn_aborted>",
];

/// The texts of a message item one by one: its `input_text` parts, or its
/// string content.
fn message_parts(item: &Value) -> Vec<&str> {
    match item.get("content") {
        Some(Value::String(s)) => vec![s.as_str()],
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|p| p.get("type").and_then(Value::as_str) == Some("input_text"))
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect(),
        _ => Vec::new(),
    }
}

/// The working folder of Codex's environment context, read only from a part
/// that is one (Codex sends it as its own part, after AGENTS.md): a `<cwd>`
/// quoted in a typed message or a pasted log never counts.
fn environment_cwd(item: &Value) -> Option<String> {
    message_parts(item).into_iter().find_map(|part| {
        let trimmed = part.trim_start();
        let opens = trimmed
            .get(..ENVIRONMENT_CONTEXT.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(ENVIRONMENT_CONTEXT));
        opens.then(|| tag(trimmed, "cwd").map(str::to_string)).flatten()
    })
}

fn tag<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&format!("</{name}>"))? + start;
    Some(text[start..end].trim())
}

// ---------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------

/// One exec result Codex printed as a JSON line in a code-mode part
/// (`{"chunk_id":..,"wall_time_seconds":..,"exit_code":..,"output":".."}`),
/// split around the bytes of its `output` string token.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ExecLine {
    /// The line's bytes before the `output` string token, and after it.
    prefix: String,
    suffix: String,
    /// The decoded `output`.
    pub output: String,
    pub status: RunStatus,
}

/// A tool output string split into what frames it and the command's own text.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Framed {
    /// `prefix` + text. The prefix is the agent's status lines ending in
    /// `Output:` (exit code, wall time, chunk id), or empty.
    Plain {
        prefix: String,
        text: String,
        status: RunStatus,
    },
    /// A JSON object whose `output` field is the text.
    Json {
        object: Value,
        text: String,
        status: RunStatus,
    },
    /// Code mode: a script printed whole exec results, one JSON object per
    /// line, after `head` (Codex's truncation note, other printed lines).
    /// Each line is kept as it was, with its exec result when the line can be
    /// rewritten exactly.
    JsonLines {
        head: String,
        lines: Vec<(String, Option<ExecLine>)>,
    },
}

/// The status the prefix lines of a plain frame give.
fn plain_status(prefix: &str) -> RunStatus {
    static EXIT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^(?:Exit code: |Process exited with code )(-?\d+)$").expect("valid regex"));
    static RUNNING: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^(?:Process running with session ID \S+|Script running with cell ID \S+)$").expect("valid regex")
    });
    let mut status = RunStatus::default();
    for line in prefix.lines() {
        if let Some(c) = EXIT.captures(line) {
            status.exit = c[1].parse().ok();
        } else if RUNNING.is_match(line) {
            status.running = true;
        }
    }
    status
}

/// Keys of the object `tools.exec_command` returns in code mode
/// (codex-rs/core/src/tools/context.rs, `UnifiedExecCodeModeResult`).
const EXEC_KEYS: [&str; 6] = [
    "chunk_id",
    "wall_time_seconds",
    "exit_code",
    "session_id",
    "original_token_count",
    "output",
];

/// The end (exclusive) of the JSON string token that starts at `i`.
fn string_end(b: &[u8], i: usize) -> Option<usize> {
    if b.get(i) != Some(&b'"') {
        return None;
    }
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            b'"' => return Some(j + 1),
            _ => j += 1,
        }
    }
    None
}

/// The end (exclusive) of the JSON value that starts at `i`.
fn value_end(b: &[u8], i: usize) -> Option<usize> {
    match *b.get(i)? {
        b'"' => string_end(b, i),
        b'{' | b'[' => {
            let mut depth = 0usize;
            let mut j = i;
            while j < b.len() {
                match b[j] {
                    b'"' => {
                        j = string_end(b, j)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(j + 1);
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            None
        }
        _ => Some(
            (i..b.len())
                .find(|&j| matches!(b[j], b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r'))
                .unwrap_or(b.len()),
        ),
    }
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    i
}

/// The byte spans of the values of a JSON object's top-level keys, in order.
/// `None` when `s` is not exactly one JSON object.
fn top_level_spans(s: &str) -> Option<Vec<(String, std::ops::Range<usize>)>> {
    let b = s.as_bytes();
    let mut i = skip_ws(b, 0);
    if b.get(i) != Some(&b'{') {
        return None;
    }
    i = skip_ws(b, i + 1);
    let mut out = Vec::new();
    if b.get(i) == Some(&b'}') {
        return (skip_ws(b, i + 1) == b.len()).then_some(out);
    }
    loop {
        let key_end = string_end(b, i)?;
        let key: String = serde_json::from_str(&s[i..key_end]).ok()?;
        i = skip_ws(b, key_end);
        if b.get(i) != Some(&b':') {
            return None;
        }
        i = skip_ws(b, i + 1);
        let end = value_end(b, i)?;
        out.push((key, i..end));
        i = skip_ws(b, end);
        match b.get(i) {
            Some(b',') => i = skip_ws(b, i + 1),
            Some(b'}') => {
                i += 1;
                break;
            }
            _ => return None,
        }
    }
    (skip_ws(b, i) == b.len()).then_some(out)
}

/// A line that is an exec result printed as JSON: it starts with `{"`, ends
/// with `}`, and is one object with a string `output`, an `exit_code` or
/// `chunk_id`, and no key the exec result does not have. The inner result is
/// given when the `output` token re-encodes to exactly its bytes; otherwise
/// the line must be left alone.
fn exec_line(line: &str) -> Option<Option<ExecLine>> {
    if !line.starts_with("{\"") || !line.ends_with('}') {
        return None;
    }
    let object: serde_json::Map<String, Value> = serde_json::from_str(line).ok()?;
    let output = object.get("output")?.as_str()?;
    if !(object.contains_key("exit_code") || object.contains_key("chunk_id"))
        || object.keys().any(|k| !EXEC_KEYS.contains(&k.as_str()))
    {
        return None;
    }
    let exit = object.get("exit_code").and_then(Value::as_i64);
    let status = RunStatus {
        exit: exit.and_then(|e| i32::try_from(e).ok()),
        running: exit.is_none() && object.get("session_id").is_some_and(|s| !s.is_null()),
        masked: false,
    };
    let exact = || {
        let spans = top_level_spans(line)?;
        let mut outputs = spans.iter().filter(|(k, _)| k == "output");
        let (_, span) = outputs.next()?;
        if outputs.next().is_some() {
            return None;
        }
        let token = &line[span.clone()];
        let decoded: String = serde_json::from_str(token).ok()?;
        (decoded == output && serde_json::to_string(&decoded).ok()? == token).then(|| ExecLine {
            prefix: line[..span.start].to_string(),
            suffix: line[span.end..].to_string(),
            output: decoded,
            status,
        })
    };
    Some(exact())
}

/// Split a tool output into its frame and its text. `json_lines`: the text is
/// a code-mode part a script printed, which may hold whole exec results as
/// JSON lines.
pub(crate) fn unframe(raw: &str, json_lines: bool) -> Framed {
    if json_lines {
        let mut at = 0usize;
        let mut first = None;
        for line in raw.split('\n') {
            if exec_line(line).is_some() {
                first = Some(at);
                break;
            }
            at += line.len() + 1;
        }
        if let Some(start) = first {
            let lines = raw[start..]
                .split('\n')
                .map(|l| (l.to_string(), exec_line(l).flatten()))
                .collect();
            return Framed::JsonLines {
                head: raw[..start].to_string(),
                lines,
            };
        }
    }
    if raw.trim_start().starts_with('{') {
        if let Ok(object) = serde_json::from_str::<Value>(raw) {
            if let Some(text) = object.get("output").and_then(Value::as_str) {
                let text = text.to_string();
                let exit = object
                    .get("metadata")
                    .and_then(|m| m.get("exit_code"))
                    .and_then(Value::as_i64)
                    .and_then(|e| i32::try_from(e).ok());
                return Framed::Json {
                    object,
                    text,
                    status: RunStatus {
                        exit,
                        running: false,
                        masked: false,
                    },
                };
            }
        }
    }
    // The agent frames a command's text with a few status lines that end in
    // `Output:` (exit code, wall time, chunk id, a truncation warning).
    if let Some(at) = raw.find("\nOutput:\n") {
        if at < 600 && raw[..at].contains("Wall time") {
            let cut = at + "\nOutput:\n".len();
            return Framed::Plain {
                prefix: raw[..cut].to_string(),
                text: raw[cut..].to_string(),
                status: plain_status(&raw[..at]),
            };
        }
    }
    Framed::Plain {
        prefix: String::new(),
        text: raw.to_string(),
        status: RunStatus::default(),
    }
}

/// A plain or JSON frame with its text replaced.
fn reframe(framed: Framed, new_text: String) -> String {
    match framed {
        Framed::Plain { prefix, .. } => prefix + &new_text,
        Framed::Json { mut object, .. } => {
            object["output"] = Value::String(new_text);
            object.to_string()
        }
        Framed::JsonLines { .. } => new_text,
    }
}

/// JSON lines put back together, each rewritten line's new output spliced
/// between its original prefix and suffix bytes.
fn rejoin(head: &str, lines: &[(String, Option<ExecLine>)], new: &[Option<String>]) -> String {
    let mut out = String::from(head);
    for (k, (raw, exec)) in lines.iter().enumerate() {
        if k > 0 {
            out.push('\n');
        }
        match (exec, new.get(k).and_then(Option::as_ref)) {
            (Some(e), Some(text)) => {
                out.push_str(&e.prefix);
                out.push_str(&serde_json::to_string(text).unwrap_or_default());
                out.push_str(&e.suffix);
            }
            _ => out.push_str(raw),
        }
    }
    out
}

/// The extra item that carries graph context, right after the user's message.
fn context_item(text: &str) -> Value {
    json!({ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": text }] })
}

/// The status part a code-mode script's result starts with
/// (`Script completed\nWall time 0.1 seconds\nOutput:\n`; codex-rs/core/src/tools/code_mode/output.rs).
fn is_script_header(part: &Value) -> bool {
    part.get("type").and_then(Value::as_str) == Some("input_text")
        && part
            .get("text")
            .and_then(Value::as_str)
            .is_some_and(is_script_header_text)
}

/// The text of a code-mode status part.
pub(crate) fn is_script_header_text(t: &str) -> bool {
    t.len() < 600 && t.contains("Wall time") && t.trim_end().ends_with("Output:")
}

/// The status of what a script printed so far: still running when Codex
/// returned before the script finished.
fn script_status(header: &Value) -> RunStatus {
    let text = header.get("text").and_then(Value::as_str).unwrap_or("");
    RunStatus {
        exit: None,
        running: text.starts_with("Script running"),
        masked: false,
    }
}

/// A hash of an input item as JSON, as 16 hex characters of its content hash.
fn hash_of(parts: &[&Value]) -> u64 {
    let mut bytes = Vec::new();
    for v in parts {
        bytes.extend_from_slice(serde_json::to_string(v).unwrap_or_default().as_bytes());
        bytes.push(b'\n');
    }
    u64::from_str_radix(&lge_core::content_hash(&bytes)[..16], 16).unwrap_or(0)
}

/// For each input item, a hash of the client's items up to and including it:
/// the same value in two requests means the same items up to there.
fn prefix_hashes(client: &[u64]) -> Vec<u64> {
    let mut acc = 0u64;
    client
        .iter()
        .map(|h| {
            let mut bytes = acc.to_le_bytes().to_vec();
            bytes.extend_from_slice(&h.to_le_bytes());
            acc = u64::from_str_radix(&lge_core::content_hash(&bytes)[..16], 16).unwrap_or(0);
            acc
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The calls of a request
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum CallKind {
    /// A shell command.
    Shell(Ran),
    /// A code-mode script; its plan when what it runs is certain.
    Script { plan: Option<ScriptPlan>, edits: bool },
    /// `apply_patch`, with the paths it names.
    Patch(Vec<String>),
    /// Any other tool, by its name (`write_stdin` included).
    Tool(String),
}

#[derive(Debug, Clone)]
struct Call {
    call_id: String,
    name: String,
    kind: CallKind,
    /// It may have changed files.
    edit: bool,
}

/// The namespace Codex puts its own tools in when it groups them.
const DEFAULT_TOOL_NAMESPACE: &str = "functions";

/// True for the tools lge itself serves (`search_code_context`, ...), with
/// or without an MCP server prefix.
fn is_lge_tool(name: &str) -> bool {
    let last = name.rsplit("__").next().unwrap_or(name);
    lge_mcp::tools().iter().any(|t| t.name == last)
}

/// A tool whose output may be a file the model will patch against: its JSON
/// is never minified.
fn file_tool(name: &str) -> bool {
    static FILE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)(read|file|open|view|cat|source|content|document)").expect("valid regex"));
    FILE.is_match(name)
}

/// The tool calls of a request, in order.
fn history(items: &[Value]) -> Vec<Call> {
    let mut calls = Vec::new();
    for item in items {
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
        let Some(call_id) = item.get("call_id").and_then(Value::as_str) else {
            continue;
        };
        let name = item.get("name").and_then(Value::as_str).unwrap_or("");
        // Codex's own tools may come back in its default namespace; they are
        // named without it, as Codex names them (codex-rs/protocol/src/tool_name.rs).
        let namespace = item
            .get("namespace")
            .and_then(Value::as_str)
            .filter(|n| *n != DEFAULT_TOOL_NAMESPACE)
            .unwrap_or("");
        let kind = match item_type {
            "function_call" => {
                let raw = item.get("arguments").and_then(Value::as_str).unwrap_or("");
                let args = serde_json::from_str::<Value>(raw).ok();
                if !namespace.is_empty() {
                    CallKind::Tool(format!("{namespace}{name}"))
                } else if SHELL_TOOLS.contains(&name) {
                    match args.as_ref().and_then(command_of) {
                        Some(ran) => CallKind::Shell(ran),
                        None => CallKind::Tool(name.to_string()),
                    }
                } else if name == "apply_patch" {
                    let patch = args
                        .as_ref()
                        .and_then(|a| a.get("input").or_else(|| a.get("patch")))
                        .and_then(Value::as_str)
                        .unwrap_or(raw);
                    CallKind::Patch(patch_paths(patch))
                } else {
                    CallKind::Tool(name.to_string())
                }
            }
            "local_shell_call" => match item.get("action").and_then(command_of) {
                Some(ran) => CallKind::Shell(ran),
                None => CallKind::Tool("local_shell".to_string()),
            },
            "custom_tool_call" => {
                let input = item.get("input").and_then(Value::as_str).unwrap_or("");
                match name {
                    "exec" => {
                        let plan = script_plan(input);
                        let edits = plan.as_ref().map_or_else(|| script_edits(input), |p| p.edits);
                        CallKind::Script { plan, edits }
                    }
                    "apply_patch" => CallKind::Patch(patch_paths(input)),
                    _ => CallKind::Tool(name.to_string()),
                }
            }
            _ => continue,
        };
        let edit = match &kind {
            CallKind::Shell(ran) => !is_read_only(&ran.command),
            CallKind::Script { edits, .. } => *edits,
            CallKind::Patch(_) => true,
            CallKind::Tool(name) => !(is_lge_tool(name) || READ_ONLY_TOOLS.contains(&name.as_str())),
        };
        let name = if !namespace.is_empty() {
            format!("{namespace}{name}")
        } else if item_type == "local_shell_call" {
            // Named by its tool type, as the request's tool list names it.
            "local_shell".to_string()
        } else {
            name.to_string()
        };
        calls.push(Call {
            call_id: call_id.to_string(),
            name,
            kind,
            edit,
        });
    }
    calls
}

impl Call {
    /// What a trace names the call by: its command, its script's commands,
    /// or the tool's name, cut to 120 characters.
    fn label(&self) -> String {
        let full = match &self.kind {
            CallKind::Shell(ran) => ran.command.clone(),
            CallKind::Script { plan: Some(plan), .. } => {
                let cmds: Vec<&str> = plan.commands.iter().map(|c| c.cmd.as_str()).collect();
                format!("exec: {}", cmds.join("; "))
            }
            CallKind::Script { plan: None, .. } => "exec (script)".to_string(),
            CallKind::Patch(_) => "apply_patch".to_string(),
            CallKind::Tool(name) => name.clone(),
        };
        full.chars().take(120).collect()
    }

    /// The shell commands the call ran, as far as it is known.
    fn commands(&self) -> Vec<String> {
        match &self.kind {
            CallKind::Shell(ran) => vec![ran.command.clone()],
            CallKind::Script { plan: Some(plan), .. } => plan.commands.iter().map(|c| c.cmd.clone()).collect(),
            _ => Vec::new(),
        }
    }

    fn view(&self) -> CallView {
        CallView {
            call_id: self.call_id.clone(),
            edit: self.edit,
            commands: self.commands(),
            patched: match &self.kind {
                CallKind::Patch(paths) => paths.clone(),
                _ => Vec::new(),
            },
        }
    }
}

/// The string two calls share when they ran the same command.
fn normalised_command(command: &str) -> String {
    normalise(command).map_or_else(|| command.trim().to_string(), |n| n.command)
}

// ---------------------------------------------------------------------------
// Outputs whose command is known
// ---------------------------------------------------------------------------

/// A large output sent whole, which a later identical copy can point at.
struct Whole {
    key: String,
    /// Its normalised command, and its run among the calls of the request
    /// that ran it.
    command: String,
    run: usize,
}

/// A read sent whole, which a later read of the same lines is compared with.
struct ReadCopy {
    path: String,
    numbered: bool,
    start: usize,
    end: usize,
    text: String,
    command: String,
}

/// The tool outputs of one request whose command is known, rewritten in
/// conversation order.
struct Known<'s, 'g> {
    session: &'s mut Session,
    graph: Option<&'g Graph<'g>>,
    opts: &'s Options,
    default_dir: Option<String>,
    calls: &'s [Call],
    /// Edit calls before each call index: `edits_before[i]` counts calls `< i`.
    edits_before: Vec<usize>,
    /// (call index, command index) -> its run among the request's calls of
    /// the same normalised command, from 1.
    runs: HashMap<(usize, usize), usize>,
    /// Keys of the items seen so far in this request.
    present: HashSet<String>,
    /// Content hash -> the first large output sent whole with it.
    sent_in_full: HashMap<String, Whole>,
    /// Normalised command -> (call index, key) of each earlier output.
    by_command: HashMap<String, Vec<(usize, String)>>,
    /// Normalised command -> (run, text) of its latest output sent whole.
    last_whole: HashMap<String, (usize, String)>,
    reads: Vec<ReadCopy>,
}

impl<'s, 'g> Known<'s, 'g> {
    fn new(
        session: &'s mut Session,
        graph: Option<&'g Graph<'g>>,
        opts: &'s Options,
        default_dir: Option<String>,
        calls: &'s [Call],
    ) -> Self {
        let mut edits_before = Vec::with_capacity(calls.len() + 1);
        let mut edits = 0;
        for call in calls {
            edits_before.push(edits);
            edits += usize::from(call.edit);
        }
        edits_before.push(edits);
        let mut counter: HashMap<String, usize> = HashMap::new();
        let mut runs = HashMap::new();
        for (i, call) in calls.iter().enumerate() {
            for (j, command) in call.commands().iter().enumerate() {
                let n = counter.entry(normalised_command(command)).or_insert(0);
                *n += 1;
                runs.insert((i, j), *n);
            }
        }
        Known {
            session,
            graph,
            opts,
            default_dir,
            calls,
            edits_before,
            runs,
            present: HashSet::new(),
            sent_in_full: HashMap::new(),
            by_command: HashMap::new(),
            last_whole: HashMap::new(),
            reads: Vec::new(),
        }
    }

    /// Edit calls strictly between call indexes `a` and `b` (`a <= b`).
    fn edits_between(&self, a: usize, b: usize) -> usize {
        if b <= a + 1 {
            return 0;
        }
        self.edits_before[b] - self.edits_before[a + 1]
    }

    /// The re-run recovery rule: an earlier call ran the same normalised
    /// command, its output was shortened by a lossy rule, and no call that
    /// may have changed files lies between them.
    fn recovery(&self, at: usize, command: &str) -> bool {
        let Some(earlier) = self.by_command.get(command) else {
            return false;
        };
        for (e, key) in earlier.iter().rev() {
            if self.edits_between(*e, at) > 0 {
                return false;
            }
            if self.session.lossy(key) {
                return true;
            }
        }
        false
    }

    /// What to send instead of one text that command `j` of call `at`
    /// printed, or `None` to send it as it is. `id` names the text (its call
    /// id, its part and line): decided once for it, and replayed after.
    fn text(&mut self, at: usize, j: usize, ran: &Ran, id: &str, text: &str, status: RunStatus) -> Option<String> {
        let key = output_key(id, &ran.command, text);
        let command = normalised_command(&ran.command);
        let present = &self.present;
        let replayed = self.session.replay(&key, &|target| present.contains(target));
        let result = match replayed {
            Some(known) => known,
            None => self.decide(at, ran, &key, &command, text, status),
        };
        self.present.insert(key.clone());
        if result.is_none() {
            self.keep_whole(at, j, ran, &key, &command, text);
        }
        self.by_command.entry(command).or_default().push((at, key));
        result
    }

    /// Remember an output sent whole: later identical copies point at it, and
    /// later re-runs and re-reads are measured against it.
    fn keep_whole(&mut self, at: usize, j: usize, ran: &Ran, key: &str, command: &str, text: &str) {
        let run = self.runs.get(&(at, j)).copied().unwrap_or(1);
        if text.len() >= REPEAT_MIN_BYTES {
            self.sent_in_full
                .entry(lge_core::content_hash(text.as_bytes()))
                .or_insert_with(|| Whole {
                    key: key.to_string(),
                    command: command.to_string(),
                    run,
                });
        }
        if !self.opts.shorten_outputs {
            return;
        }
        if self.opts.mode(RuleId::RerunDelta) != Mode::Off {
            self.last_whole.insert(command.to_string(), (run, text.to_string()));
        }
        if self.opts.mode(RuleId::ReadDelta) != Mode::Off {
            if let Some((path, numbered, start)) = read_of(&purpose(&ran.command)) {
                let lines = text.lines().count();
                if lines > 0 {
                    self.reads.push(ReadCopy {
                        path,
                        numbered,
                        start,
                        end: start + lines - 1,
                        text: text.to_string(),
                        command: command.to_string(),
                    });
                }
            }
        }
    }

    /// The first decision for an output: the rules for its purpose, else a
    /// pointer to an identical earlier copy, else the measured deltas.
    #[allow(clippy::too_many_arguments)]
    fn decide(
        &mut self,
        at: usize,
        ran: &Ran,
        key: &str,
        command: &str,
        text: &str,
        status: RunStatus,
    ) -> Option<String> {
        let dir: Option<PathBuf> = ran
            .workdir
            .clone()
            .or_else(|| self.default_dir.clone())
            .map(PathBuf::from);
        let p = purpose(&ran.command);
        // Decided before the daemon restarted: the rules it got then, and only those.
        let pinned = self.session.pinned(key).map(|mask| self.opts.pinned(mask));
        let opts = pinned.as_ref().unwrap_or(self.opts);
        let active = opts.shorten_outputs && p != Purpose::Raw;
        let recovery = active && self.recovery(at, command);
        let mut draft: Draft =
            self.session
                .draft_known(self.graph, &ran.command, dir.as_deref(), text, status, opts, recovery);
        draft.pinned = pinned.is_some();
        let call_id = self.calls[at].call_id.clone();
        if draft.text.is_none() && active && text.len() >= REPEAT_MIN_BYTES {
            // The same large output again (a file read twice, a command rerun
            // with nothing changed): point to the first copy sent whole, which
            // is still in this request. Never to a shortened copy.
            if let Some(first) = self.sent_in_full.get(&lge_core::content_hash(text.as_bytes())) {
                let pointer = pointer_text(first, text);
                let target = first.key.clone();
                match opts.mode(RuleId::RepeatPointer) {
                    Mode::On => {
                        return Some(self.session.commit_pointer(
                            key.to_string(),
                            target,
                            pointer,
                            draft,
                            text,
                            &call_id,
                        ));
                    }
                    Mode::Shadow => draft.add_shadow(RuleId::RepeatPointer, text, &pointer),
                    Mode::Off => {}
                }
            }
        }
        if draft.text.is_none() && active && !draft.pinned {
            self.deltas(&mut draft, &p, command, text);
        }
        let producer = normalise(&ran.command).map(|n| producer_of(&n));
        self.session.commit(key.to_string(), draft, text, &call_id, producer)
    }

    /// What a diff against an earlier whole copy would save, counted in shadow.
    fn deltas(&self, draft: &mut Draft, p: &Purpose, command: &str, text: &str) {
        match read_of(p) {
            Some((path, numbered, start)) => {
                if self.opts.mode(RuleId::ReadDelta) == Mode::Off {
                    return;
                }
                let lines = text.lines().count();
                if lines == 0 {
                    return;
                }
                let end = start + lines - 1;
                let covering =
                    self.reads.iter().rev().find(|r| {
                        r.numbered == numbered && same_path(&r.path, &path) && r.start <= start && r.end >= end
                    });
                if let Some(e) = covering {
                    let cmd: String = e.command.chars().take(120).collect();
                    if let Some(out) = crate::delta::read_delta(&e.text, e.start, text, start, &cmd) {
                        draft.add_shadow(RuleId::ReadDelta, text, &out);
                    }
                }
            }
            None if *p == Purpose::Diff => {}
            None => {
                if self.opts.mode(RuleId::RerunDelta) == Mode::Off {
                    return;
                }
                if let Some((run, earlier)) = self.last_whole.get(command) {
                    let cmd: String = command.chars().take(120).collect();
                    if let Some(out) = crate::delta::rerun_delta(earlier, text, *run, &cmd) {
                        draft.add_shadow(RuleId::RerunDelta, text, &out);
                    }
                }
            }
        }
    }

    /// One text of an output, with its frame: replaced in place when the
    /// decision says so. `json_lines`: a code-mode part that may hold exec
    /// results; each line is then decided on its own, by `ran` when the
    /// script runs one command, else by its content.
    #[allow(clippy::too_many_arguments)]
    fn part(
        &mut self,
        raw: &mut String,
        at: usize,
        j: usize,
        ran: &Ran,
        id: &str,
        json_lines: bool,
        single: bool,
        outer: RunStatus,
    ) -> usize {
        match unframe(raw, json_lines) {
            Framed::JsonLines { head, lines } => {
                let skip = head.matches('\n').count();
                let mut new: Vec<Option<String>> = Vec::with_capacity(lines.len());
                let mut replaced = 0;
                for (k, (_, exec)) in lines.iter().enumerate() {
                    let decided = exec.as_ref().and_then(|e| {
                        if single {
                            let line_id = format!("{id}#{}", skip + k);
                            self.text(at, j, ran, &line_id, &e.output, e.status)
                        } else {
                            self.session.decide_content(
                                &self.calls[at].call_id,
                                &e.output,
                                e.status,
                                false,
                                false,
                                self.opts,
                            )
                        }
                    });
                    replaced += usize::from(decided.is_some());
                    new.push(decided);
                }
                if replaced > 0 {
                    *raw = rejoin(&head, &lines, &new);
                }
                replaced
            }
            framed => {
                let (text, status) = match &framed {
                    Framed::Plain { text, status, .. } | Framed::Json { text, status, .. } => (text.clone(), *status),
                    Framed::JsonLines { .. } => unreachable!("handled above"),
                };
                let status = RunStatus {
                    exit: status.exit.or(outer.exit),
                    running: status.running || outer.running,
                    masked: false,
                };
                match self.text(at, j, ran, id, &text, status) {
                    Some(new) => {
                        *raw = reframe(framed, new);
                        1
                    }
                    None => 0,
                }
            }
        }
    }

    /// Rewrite the output of `item`, the result of call `at`. Returns how many
    /// texts were replaced, or `None` when the output has a shape this does not
    /// read (it is then judged by its content). The JSON type of `output`, the
    /// number of its parts and every part that is not text never change.
    fn output(&mut self, item: &mut Value, kind: &str, at: usize) -> Option<usize> {
        let call_id = item.get("call_id").and_then(Value::as_str).unwrap_or("").to_string();
        let calls = self.calls;
        let call = &calls[at];
        let (commands, prints): (Vec<Ran>, Option<&Vec<usize>>) = match &call.kind {
            CallKind::Shell(ran) => (vec![ran.clone()], None),
            CallKind::Script { plan: Some(plan), .. } => (
                plan.commands
                    .iter()
                    .map(|c| Ran {
                        command: c.cmd.clone(),
                        workdir: c.workdir.clone(),
                    })
                    .collect(),
                plan.prints.as_ref(),
            ),
            _ => return None,
        };
        let script = matches!(call.kind, CallKind::Script { .. });
        match item.get_mut("output")? {
            Value::String(raw) => {
                // One text: a shell's output, or a script's whole result as one string.
                if prints.is_some() {
                    return None;
                }
                let id = if call_id.is_empty() {
                    lge_core::content_hash(raw.as_bytes())
                } else {
                    call_id.clone()
                };
                let id = if script { format!("{id}#0") } else { id };
                Some(self.part(raw, at, 0, &commands[0], &id, script, true, RunStatus::default()))
            }
            // A code-mode script's result: its status part, then what the
            // script printed. The status part stays as it is.
            Value::Array(parts)
                if kind == "custom_tool_call_output" && script && parts.first().is_some_and(is_script_header) =>
            {
                let outer = script_status(&parts[0]);
                let texts: Vec<usize> = (1..parts.len())
                    .filter(|i| parts[*i].get("type").and_then(Value::as_str) == Some("input_text"))
                    .collect();
                // Several commands: part k is the k-th text() call's, when the
                // counts agree; otherwise none can be told apart.
                let owner: Vec<usize> = match prints {
                    Some(p) if p.len() == texts.len() => p.clone(),
                    Some(_) => return None,
                    None => vec![0; texts.len()],
                };
                let mut replaced = 0;
                for (k, i) in texts.into_iter().enumerate() {
                    let Some(Value::String(raw)) = parts[i].get_mut("text") else {
                        continue;
                    };
                    let id = if call_id.is_empty() {
                        lge_core::content_hash(raw.as_bytes())
                    } else {
                        format!("{call_id}#{i}")
                    };
                    let j = owner[k];
                    let Some(ran) = commands.get(j) else { continue };
                    replaced += self.part(raw, at, j, ran, &id, prints.is_none(), commands.len() == 1, outer);
                }
                Some(replaced)
            }
            _ => None,
        }
    }
}

/// The path, numbered flag and first line of a read.
fn read_of(p: &Purpose) -> Option<(String, bool, usize)> {
    match p {
        Purpose::ReadWhole { path, numbered } => Some((path.clone(), *numbered, 1)),
        Purpose::ReadRange {
            path, start, numbered, ..
        } => Some((path.clone(), *numbered, *start)),
        _ => None,
    }
}

/// The text that replaces an output identical to an earlier one. It names the
/// run it repeats and quotes its first line, so the model can tell which of
/// several runs of a command it is.
fn pointer_text(first: &Whole, text: &str) -> String {
    let cmd: String = first.command.chars().take(120).collect();
    let line: String = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim_end()
        .chars()
        .take(80)
        .collect();
    format!(
        "lge: identical to the output of run {} of `{cmd}` earlier in this conversation ({} lines, first line: \"{line}\"); it is not repeated here.\n",
        first.run,
        text.lines().count()
    )
}

/// Shorten the texts of an output whose command is not known, each judged by
/// its content. Returns how many were replaced. The JSON type of `output` and
/// the number of its parts never change. `code_mode`: a script's result, whose
/// parts may hold exec results as JSON lines. `minify`: JSON may be minified
/// (a tool that is neither a shell nor a file reader).
fn rewrite_by_content(
    item: &mut Value,
    session: &mut Session,
    opts: &Options,
    call: &str,
    code_mode: bool,
    minify: bool,
    unread: bool,
) -> usize {
    let mut one = |raw: &mut String, outer: RunStatus| -> usize {
        match unframe(raw, code_mode) {
            Framed::JsonLines { head, lines } => {
                let new: Vec<Option<String>> = lines
                    .iter()
                    .map(|(_, exec)| {
                        exec.as_ref()
                            .and_then(|e| session.decide_content(call, &e.output, e.status, false, unread, opts))
                    })
                    .collect();
                let replaced = new.iter().filter(|n| n.is_some()).count();
                if replaced > 0 {
                    *raw = rejoin(&head, &lines, &new);
                }
                replaced
            }
            framed => {
                let (text, status) = match &framed {
                    Framed::Plain { text, status, .. } | Framed::Json { text, status, .. } => (text.clone(), *status),
                    Framed::JsonLines { .. } => unreachable!("handled above"),
                };
                let status = RunStatus {
                    exit: status.exit.or(outer.exit),
                    running: status.running || outer.running,
                    masked: false,
                };
                match session.decide_content(call, &text, status, minify, unread, opts) {
                    Some(new) => {
                        *raw = reframe(framed, new);
                        1
                    }
                    None => 0,
                }
            }
        }
    };
    match item.get_mut("output") {
        Some(Value::String(raw)) => one(raw, RunStatus::default()),
        Some(Value::Array(parts)) => {
            let outer = parts
                .first()
                .filter(|p| is_script_header(p))
                .map(script_status)
                .unwrap_or_default();
            let mut replaced = 0;
            for part in parts.iter_mut() {
                if part.get("type").and_then(Value::as_str) != Some("input_text") {
                    continue;
                }
                let Some(Value::String(text)) = part.get_mut("text") else {
                    continue;
                };
                replaced += one(text, outer);
            }
            replaced
        }
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// Background requests
// ---------------------------------------------------------------------------

/// The header (and `client_metadata` key) in which Codex names the sub-agent
/// a request comes from (codex-rs/codex-api/src/requests/headers.rs,
/// codex-rs/core/src/responses_metadata.rs).
pub const SUBAGENT_HEADER: &str = "x-openai-subagent";
/// The sub-agent label of a spawned agent doing real work in its own thread:
/// its requests are a conversation like any other.
const SPAWNED_AGENT: &str = "collab_spawn";
/// Characters of a sub-agent label kept.
const LABEL_CHARS: usize = 40;
/// The header (and `client_metadata` key) in which Codex sends a turn's
/// metadata as JSON, its thread's `thread_source` among it
/// (codex-rs/core/src/responses_metadata.rs).
pub const TURN_METADATA_HEADER: &str = "x-codex-turn-metadata";

/// A field of a request's turn metadata, from the body's `client_metadata`
/// or else from the header (`turn_metadata`).
pub(crate) fn turn_metadata_field(turn_metadata: Option<&str>, body: &Value, field: &str) -> Option<String> {
    let from_body = body
        .get("client_metadata")
        .and_then(|m| m.get(TURN_METADATA_HEADER))
        .and_then(Value::as_str);
    [from_body, turn_metadata].into_iter().flatten().find_map(|json| {
        let metadata: Value = serde_json::from_str(json).ok()?;
        metadata.get(field)?.as_str().map(str::to_string)
    })
}

/// True for the request in which Codex asks the model for the summary of a
/// compaction: its turn metadata's `request_kind` is `compaction`
/// (codex-rs/core/src/responses_metadata.rs). It is a request of the
/// conversation itself (no sub-agent header), sent to `/responses`.
pub fn compaction_request(turn_metadata: Option<&str>, body: &Value) -> bool {
    turn_metadata_field(turn_metadata, body, "request_kind").as_deref() == Some("compaction")
}

/// The label of a request Codex makes in the background, or `None` for a
/// request of a conversation the person works in. Background: the
/// `x-openai-subagent` header, or the same key of the body's
/// `client_metadata`, names a sub-agent other than a spawned agent
/// (`review`, `memory_consolidation`, `guardian`, ...; codex-rs maps a
/// `compact` sub-agent too, but no request of it sends one); the turn
/// metadata's `thread_source` is a task title, a recap, an approval review
/// or a memory consolidation (see [`lge_usage::background_thread_source`]);
/// the model is the approval reviewer's (label `guardian`); or the
/// instructions, or a message up to and including the first typed one, open
/// with one of Codex's own prompts ([`lge_usage::BACKGROUND_PROMPTS`]: the
/// desktop app's title prompt, the approval reviewer's, a memory
/// consolidation's). `subagent` and `turn_metadata` are the headers' values.
pub fn background_label(subagent: Option<&str>, turn_metadata: Option<&str>, body: &Value) -> Option<String> {
    let label = |value: &str| -> Option<String> {
        let value = value.trim();
        if value.eq_ignore_ascii_case(SPAWNED_AGENT) {
            return None;
        }
        Some(if value.is_empty() {
            "subagent".to_string()
        } else {
            value.chars().take(LABEL_CHARS).collect()
        })
    };
    let metadata = body
        .get("client_metadata")
        .and_then(|m| m.get(SUBAGENT_HEADER))
        .and_then(Value::as_str);
    if let Some(found) = subagent.and_then(label).or_else(|| metadata.and_then(label)) {
        return Some(found);
    }
    if let Some(found) = turn_metadata_field(turn_metadata, body, "thread_source")
        .as_deref()
        .and_then(lge_usage::background_thread_source)
    {
        return Some(found.to_string());
    }
    if body
        .get("model")
        .and_then(Value::as_str)
        .is_some_and(|m| m.trim().eq_ignore_ascii_case(lge_usage::REVIEWER_MODEL))
    {
        return Some("guardian".to_string());
    }
    let prompt = |text: &str| lge_usage::background_task(text).map(str::to_string);
    if let Some(found) = body.get("instructions").and_then(Value::as_str).and_then(prompt) {
        return Some(found);
    }
    match body.get("input") {
        Some(Value::String(text)) => return prompt(text),
        Some(Value::Array(items)) => {
            // The messages up to the first one the person typed.
            for item in items {
                if item.get("type").and_then(Value::as_str) != Some("message") {
                    continue;
                }
                let Some(text) = message_text(item) else { continue };
                if let Some(found) = prompt(&text) {
                    return Some(found);
                }
                if item.get("role").and_then(Value::as_str) == Some("user") && !is_injected(item, &text) {
                    break;
                }
            }
        }
        _ => {}
    }
    None
}

/// Read what the request says about itself, without changing it.
pub fn inspect(body: &Value) -> Seen {
    let mut seen = Seen {
        conversation: body.get("prompt_cache_key").and_then(Value::as_str).map(str::to_string),
        model: body.get("model").and_then(Value::as_str).map(str::to_string),
        has_history: body.get("previous_response_id").is_some_and(|v| !v.is_null()),
        ..Seen::default()
    };
    let mut typed = 0usize;
    for item in body.get("input").and_then(Value::as_array).into_iter().flatten() {
        let kind = item.get("type").and_then(Value::as_str).unwrap_or("message");
        let role = item.get("role").and_then(Value::as_str).unwrap_or("");
        match (kind, role) {
            ("message", "user") => {}
            ("message", "developer" | "system") | ("additional_tools", _) => continue,
            // Reasoning, a tool call or its output, an assistant message, a
            // compaction item, an item reference: an earlier turn.
            _ => {
                seen.has_history = true;
                continue;
            }
        }
        if item.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let Some(text) = message_text(item) else { continue };
        if seen.cwd.is_none() {
            seen.cwd = environment_cwd(item);
        }
        if is_injected(item, &text) {
            let lower = text.trim_start().to_ascii_lowercase();
            if HISTORY_STARTS.iter().any(|start| lower.starts_with(start)) {
                seen.has_history = true;
            }
            continue;
        }
        typed += 1;
        if typed > 1 {
            seen.has_history = true;
        }
        if seen.first_user_raw.is_empty() {
            seen.first_user_raw = text.trim().chars().take(200).collect();
        }
        // The task is what the user asked, not the context in front of it; a
        // message with nothing but context (attachments) is not the task.
        if seen.first_user_message.is_empty() {
            let request_owned = request_text(&text);
            let request = request_owned.trim();
            seen.first_user_message = request.chars().take(200).collect();
            seen.first_user_text = request.chars().take(ROUTED_CHARS).collect();
        }
    }
    seen
}

/// The tools a code-mode script calls: `tools.name(` and `tools["name"](`.
/// Names in a comment or a string count too: a tool then shows as called
/// rather than as one nobody uses.
pub fn nested_tool_names(script: &str) -> Vec<String> {
    static CALL: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let call = CALL.get_or_init(|| {
        regex::Regex::new(r#"\btools(?:\.([A-Za-z_][A-Za-z0-9_]*)|\[\s*["']([^"']+)["']\s*\])\s*\("#)
            .expect("a valid pattern")
    });
    call.captures_iter(script)
        .filter_map(|c| c.get(1).or_else(|| c.get(2)).map(|m| m.as_str().to_string()))
        .collect()
}

/// Rewrite `body` in place. Only `input` is touched, and in it only tool
/// outputs (replaced) and typed user messages (a context item is added after).
/// A body that is not a Responses request is left alone. Starts a request in
/// the session ([`Session::begin_request`]); afterwards the session has judged
/// the follow-ups of earlier items and compared the prefix with the previous
/// request's.
pub fn transform(body: &mut Value, session: &mut Session, graph: Option<&Graph>, opts: &Options) -> Seen {
    let mut seen = inspect(body);
    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return seen;
    };
    session.begin_request();
    let calls = history(items);
    let index: HashMap<&str, usize> = calls.iter().enumerate().map(|(i, c)| (c.call_id.as_str(), i)).collect();
    for call in &calls {
        if !seen.called_tools.contains(&call.name) {
            seen.called_tools.push(call.name.clone());
        }
    }
    // A tool called from inside an `exec` script is named only in its text.
    for item in items.iter() {
        let script = match item.get("type").and_then(Value::as_str) {
            Some("custom_tool_call") if item.get("name").and_then(Value::as_str) == Some("exec") => "input",
            Some("function_call") if item.get("name").and_then(Value::as_str) == Some("exec") => "arguments",
            _ => continue,
        };
        if let Some(text) = item.get(script).and_then(Value::as_str) {
            for name in nested_tool_names(text) {
                if !seen.code_mode_calls.contains(&name) {
                    seen.code_mode_calls.push(name);
                }
            }
        }
    }
    // Calls of the provider's own tools carry no call id: named by their tool type.
    for item in items.iter() {
        let hosted = match item.get("type").and_then(Value::as_str) {
            Some("web_search_call") => "web_search",
            Some("image_generation_call") => "image_generation",
            Some("tool_search_call") => "tool_search",
            _ => continue,
        };
        if !seen.called_tools.iter().any(|t| t == hosted) {
            seen.called_tools.push(hosted.to_string());
        }
    }
    let client: Vec<u64> = items.iter().map(|item| hash_of(&[item])).collect();
    let previous = session.previous_client_hashes();
    let mut new_outputs: Vec<OutputFact> = Vec::new();
    // Where the client rewrote its history since the previous request (a
    // compaction, an edit, a rollback): packs of messages at or after it are
    // retired, the newest typed message may keep its own, and every item
    // before it keeps its bytes.
    let client_break = session.client_break(&client);
    let prefixes = prefix_hashes(&client);
    let newest = items
        .iter()
        .rposition(|item| typed_text(item).is_some())
        .unwrap_or(usize::MAX);
    // Typed texts seen so far in this request: a copy of one gets no pack, so
    // the packs present never exceed the conversation budget (in a
    // conversation of an older version, a copy replays its text's pack).
    let mut typed_before: HashSet<String> = HashSet::new();
    let copies_replay = !session.no_pack_for_copies();

    let mut known = Known::new(session, graph, opts, seen.cwd.clone(), &calls);
    let mut out: Vec<Value> = Vec::with_capacity(items.len() + 2);
    let mut forwarded: Vec<u64> = Vec::with_capacity(items.len());
    for mut item in std::mem::take(items) {
        let kind = item.get("type").and_then(Value::as_str).unwrap_or("").to_string();
        match kind.as_str() {
            "function_call_output" | "custom_tool_call_output" | "local_shell_call_output" => {
                let call_id = item.get("call_id").and_then(Value::as_str).unwrap_or("").to_string();
                let at = index.get(call_id.as_str()).copied();
                // A new item's size, read before it is rewritten.
                let fresh = !previous.contains(&client[forwarded.len()]);
                let tokens = if fresh { output_tokens(&item) } else { 0 };
                let replaced = at.and_then(|at| known.output(&mut item, &kind, at));
                // No command to go by (another tool, a script this cannot read), or an
                // output of another shape: each text is judged by its content alone.
                let replaced = replaced.unwrap_or_else(|| {
                    let call = at.map(|at| &calls[at]);
                    let code_mode = matches!(
                        call.map(|c| &c.kind),
                        Some(CallKind::Script {
                            plan: None | Some(ScriptPlan { prints: None, .. }),
                            ..
                        })
                    );
                    let minify = matches!(call.map(|c| &c.kind), Some(CallKind::Tool(name)) if name != "write_stdin" && !file_tool(name));
                    // A script's output that is not matched to a command is only counted.
                    let unread = matches!(call.map(|c| &c.kind), Some(CallKind::Script { .. }));
                    rewrite_by_content(&mut item, known.session, opts, &call_id, code_mode, minify, unread)
                });
                seen.outputs_replaced += replaced;
                if fresh {
                    new_outputs.push(OutputFact {
                        command: at.map(|at| calls[at].label()).unwrap_or_default(),
                        tokens,
                        shortened: replaced > 0,
                    });
                }
                // An item left as it was has the client's hash.
                forwarded.push(if replaced > 0 {
                    hash_of(&[&item])
                } else {
                    client[forwarded.len()]
                });
                out.push(item);
            }
            "message" if item.get("role").and_then(Value::as_str) == Some("user") => {
                let at = forwarded.len();
                // Looked up with or without a graph: a message decided once
                // keeps its pack, or its lack of one, on every later turn.
                // A copy of a message typed earlier in the request gets none,
                // decided from the items before it like everything else.
                let typed = typed_text(&item).map(|text| {
                    let copy = !typed_before.insert(text.clone());
                    (text, copy)
                });
                let context = match typed {
                    Some((text, copy)) if !copy || copies_replay => {
                        let earlier: &[Value] = &out;
                        let mentioned = |path: &str| mentioned_in(earlier, path);
                        let before = Before {
                            pack_tokens: known.session.present_tokens().1.max(0) as usize,
                            mentions: &mentioned,
                            rewritten: client_break.is_some_and(|b| b <= at),
                            // The anchor is the first copy's.
                            prefix: (!copy).then(|| prefixes[at]),
                            newest: at == newest,
                        };
                        known
                            .session
                            .context_after_request(graph, &text, &request_text(&text), &before, opts)
                    }
                    _ => None,
                };
                match context {
                    Some(context) => {
                        let added = context_item(&context);
                        forwarded.push(hash_of(&[&item, &added]));
                        out.push(item);
                        out.push(added);
                        seen.contexts_present += 1;
                    }
                    None => {
                        forwarded.push(client[forwarded.len()]);
                        out.push(item);
                    }
                }
            }
            _ => {
                forwarded.push(client[forwarded.len()]);
                out.push(item);
            }
        }
    }
    *items = out;

    let views: Vec<CallView> = calls.iter().map(Call::view).collect();
    session.judge(&views);
    let pairs: Vec<(u64, u64)> = client.iter().copied().zip(forwarded.iter().copied()).collect();
    (seen.lge_break_item, seen.client_break_item) = session.compare_prefix(pairs);
    (seen.removed_present, seen.added_present) = session.present_tokens();
    seen.new_decisions = session.new_this_request().len();
    seen.client_hashes = client;
    seen.forwarded_hashes = forwarded;
    // Largest first; among equals, the earlier item first.
    new_outputs.sort_by_key(|o| std::cmp::Reverse(o.tokens));
    new_outputs.truncate(LARGEST_NEW_OUTPUTS);
    seen.largest_new_outputs = new_outputs;
    seen
}

/// Tokens of a tool output item's text: the string, or its text parts.
fn output_tokens(item: &Value) -> usize {
    match item.get("output") {
        Some(Value::String(s)) => lge_retrieve::budget::count(s),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|p| p.get("type").and_then(Value::as_str) == Some("input_text"))
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .map(lge_retrieve::budget::count)
            .sum(),
        _ => 0,
    }
}

/// Token usage reported at the end of a streamed response.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct Usage {
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_tokens: i64,
}

/// Usage from one server-sent event's data, when it is the closing event of a
/// response (`response.completed`, or a failed or incomplete one that still
/// reports what was used).
pub fn usage_of_event(data: &str) -> Option<Usage> {
    let event: Value = serde_json::from_str(data).ok()?;
    let kind = event.get("type").and_then(Value::as_str)?;
    if !matches!(kind, "response.completed" | "response.incomplete" | "response.failed") {
        return None;
    }
    let usage = event.get("response")?.get("usage")?;
    let n = |v: Option<&Value>| v.and_then(Value::as_i64).unwrap_or(0);
    Some(Usage {
        input_tokens: n(usage.get("input_tokens")),
        cached_input_tokens: n(usage.get("input_tokens_details").and_then(|d| d.get("cached_tokens"))),
        output_tokens: n(usage.get("output_tokens")),
        reasoning_tokens: n(usage
            .get("output_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))),
    })
}

/// Finds usage in a stream of server-sent events fed in arbitrary chunks.
#[derive(Default)]
pub struct UsageScanner {
    pending: Vec<u8>,
    pub usage: Option<Usage>,
}

impl UsageScanner {
    /// Feed the next bytes of the stream.
    pub fn feed(&mut self, chunk: &[u8]) {
        self.pending.extend_from_slice(chunk);
        while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line);
            if let Some(data) = line.trim_end().strip_prefix("data:") {
                // Only the closing event is large enough to matter and it names itself early.
                if data.contains("\"response.completed\"")
                    || data.contains("\"response.incomplete\"")
                    || data.contains("\"response.failed\"")
                {
                    if let Some(u) = usage_of_event(data.trim_start()) {
                        self.usage = Some(u);
                    }
                }
            }
        }
        // A line that never ends must not grow without bound.
        if self.pending.len() > 16 * 1024 * 1024 {
            self.pending.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::Mode;

    fn user(text: &str) -> Value {
        json!({ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": text }] })
    }

    fn call(id: &str, command: &str) -> Value {
        json!({ "type": "function_call", "name": "exec_command", "call_id": id, "arguments": json!({ "cmd": command, "workdir": "/repo", "yield_time_ms": 10000 }).to_string() })
    }

    fn output(id: &str, text: &str) -> Value {
        json!({ "type": "function_call_output", "call_id": id, "output": text })
    }

    fn patch(id: &str, path: &str) -> Value {
        json!({ "type": "custom_tool_call", "call_id": id, "name": "apply_patch", "input": format!("*** Begin Patch\n*** Update File: {path}\n@@\n-a\n+b\n*** End Patch\n") })
    }

    fn patched(id: &str) -> Value {
        json!({ "type": "custom_tool_call_output", "call_id": id, "output": "Success. Updated the following files:\nM src/a.rs\n" })
    }

    fn passing_run() -> String {
        let mut run = String::from(
            "============================= test session starts ==============================\ncollected 300 items\n\n",
        );
        for i in 0..300 {
            run.push_str(&format!(
                "tests/test_orders.py::test_case_number_{i} PASSED                              [{:3}%]\n",
                i / 3
            ));
        }
        run + "\n============================= 300 passed in 9.87s ==============================\n"
    }

    /// `cargo clippy` with `n` warnings, as Codex frames it.
    fn clippy(n: usize) -> String {
        let mut out = String::from("    Checking shop v0.1.0 (/repo)\n");
        let lints = [
            ("needless_return", "unneeded `return` statement"),
            ("redundant_clone", "redundant clone"),
        ];
        for i in 0..n {
            let (lint, message) = lints[i % 2];
            out.push_str(&format!(
                "warning: {message}\n  --> src/m{i}.rs:{l}:5\n   |\n{l} |     return value_{i}.clone();\n   |     ^^^^^^^^^^^^^^^^^^^^^^^^\n   |\n   = help: for further information visit https://rust-lang.github.io/rust-clippy/master/index.html#{lint}\n   = note: `#[warn(clippy::{lint})]` on by default\nhelp: remove this\n   |\n{l} -     return value_{i}.clone();\n{l} +     value_{i}\n   |\n\n",
                l = i + 10
            ));
        }
        out.push_str(&format!(
            "warning: `shop` (lib) generated {n} warnings (run `cargo clippy --fix --lib -p shop` to apply {n} suggestions)\n    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.52s\n"
        ));
        format!("Chunk ID: 1a2b\nWall time: 1.2000 seconds\nProcess exited with code 0\nOutput:\n{out}")
    }

    fn file_text() -> String {
        (0..120)
            .map(|i| format!("line {i}: some source text that is long enough to matter\n"))
            .collect()
    }

    fn texts(body: &Value) -> Vec<String> {
        body["input"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["output"].as_str().map(str::to_string).unwrap_or_default())
            .collect()
    }

    #[test]
    fn commands_are_read_from_every_argument_shape() {
        let c = |v: Value| command_of(&v).map(|r| (r.command, r.workdir));
        assert_eq!(
            c(json!({ "command": ["bash", "-lc", "cat a.py"], "workdir": "/r" })),
            Some(("cat a.py".into(), Some("/r".into())))
        );
        assert_eq!(
            c(json!({ "command": ["rg", "-n", "foo"] })),
            Some(("rg -n foo".into(), None))
        );
        assert_eq!(
            c(json!({ "command": "pytest -q", "cwd": "/r" })),
            Some(("pytest -q".into(), Some("/r".into())))
        );
        assert_eq!(c(json!({ "cmd": "git status" })), Some(("git status".into(), None)));
        assert_eq!(c(json!({ "path": "a.py" })), None);
    }

    #[test]
    fn only_shell_outputs_change_and_everything_else_is_untouched() {
        let diff: String = (0..120)
            .map(|i| format!("-    old_line_{i} = compute({i})\n+    new_line_{i} = compute({i}, True)\n"))
            .collect();
        let run = passing_run();
        let framed = format!("Chunk ID: a1b2c3\nWall time: 9.9123 seconds\nProcess exited with code 0\nOriginal token count: 5460\nOutput:\n{run}");
        let mut body = json!({
            "model": "gpt-test",
            "prompt_cache_key": "conv-1",
            "instructions": "system text",
            "input": [
                user("<environment_context>\n  <cwd>/repo</cwd>\n</environment_context>"),
                user("run the tests please"),
                { "type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "opaque==" },
                call("c1", "pytest -v"),
                output("c1", &framed),
                call("c2", "git diff"),
                output("c2", &diff),
                { "type": "function_call", "name": "update_plan", "call_id": "c3", "arguments": "{\"plan\":[]}" },
                output("c3", "Plan updated"),
            ],
            "tools": [{ "type": "function", "name": "shell" }],
            "stream": true,
        });
        let before = body.clone();
        let mut session = Session::default();
        let seen = transform(&mut body, &mut session, None, &Options::default());
        assert_eq!(seen.cwd.as_deref(), Some("/repo"));
        assert_eq!(seen.conversation.as_deref(), Some("conv-1"));
        assert_eq!(seen.model.as_deref(), Some("gpt-test"));
        assert_eq!(seen.first_user_message, "run the tests please");
        assert_eq!(seen.outputs_replaced, 1);
        assert_eq!(seen.called_tools, ["exec_command", "update_plan"]);
        assert_eq!(seen.client_hashes.len(), 9);
        assert_eq!(seen.forwarded_hashes.len(), 9);

        let items = body["input"].as_array().unwrap();
        assert_eq!(items.len(), 9, "no item is added without a graph");
        let short = items[4]["output"].as_str().unwrap();
        assert!(
            short.starts_with("Chunk ID: a1b2c3\nWall time: 9.9123 seconds\nProcess exited with code 0\nOriginal token count: 5460\nOutput:\nok"),
            "the frame stays: {short}"
        );
        assert!(short.len() * 10 < framed.len());
        // Every other item, and every other field, is byte-identical.
        for i in [0, 1, 2, 3, 5, 6, 7, 8] {
            assert_eq!(items[i], before["input"][i], "item {i}");
            assert_eq!(seen.client_hashes[i], seen.forwarded_hashes[i], "item {i}");
        }
        assert_ne!(seen.client_hashes[4], seen.forwarded_hashes[4]);
        for field in ["model", "prompt_cache_key", "instructions", "tools", "stream"] {
            assert_eq!(body[field], before[field], "{field}");
        }

        // The next turn resends the conversation: the same item gets the same bytes.
        let mut again = before.clone();
        again["input"].as_array_mut().unwrap().push(user("thanks"));
        let next = transform(&mut again, &mut session, None, &Options::default());
        assert_eq!(again["input"][4], body["input"][4]);
        assert_eq!(session.saved.outputs_shortened, 1, "decided once");
        // Only the new message is decided (no pack: there is no graph).
        assert_eq!(next.new_decisions, 1);
        assert_eq!(next.contexts_present, 0);
        assert_eq!((next.lge_break_item, next.client_break_item), (None, None));
    }

    #[test]
    fn a_repeated_output_points_to_its_first_copy_while_that_copy_is_there() {
        let file = file_text();
        let mut body = json!({ "input": [
            call("c1", "cat src/app.py"), output("c1", &file),
            call("c2", "sed -n '1,200p' src/app.py"), output("c2", &file),
            call("c3", "cat small.txt"), output("c3", "short"),
            call("c4", "cat small.txt"), output("c4", "short"),
        ] });
        let mut session = Session::default();
        let seen = transform(&mut body, &mut session, None, &Options::default());
        assert_eq!(seen.outputs_replaced, 1);
        assert_eq!(body["input"][1]["output"], file, "the first copy stays whole");
        assert_eq!(
            body["input"][3]["output"],
            "lge: identical to the output of run 1 of `cat src/app.py` earlier in this conversation (120 lines, first line: \"line 0: some source text that is long enough to matter\"); it is not repeated here.\n"
        );
        assert_eq!(
            body["input"][7]["output"], "short",
            "a small output is cheaper to repeat"
        );
        // Resent on the next turn: the same bytes, and counted once.
        let first = body.clone();
        let mut again = json!({ "input": [call("c1", "cat src/app.py"), output("c1", &file), call("c2", "sed -n '1,200p' src/app.py"), output("c2", &file)] });
        transform(&mut again, &mut session, None, &Options::default());
        assert_eq!(again["input"][3], first["input"][3]);
        assert_eq!(session.saved.outputs_shortened, 1);
        assert_eq!(session.saved.by_rule["repeat_pointer"].items, 1);
        // The agent compacted its history and the first copy is gone: the content is sent again.
        let mut compacted = json!({ "input": [call("c2", "sed -n '1,200p' src/app.py"), output("c2", &file)] });
        transform(&mut compacted, &mut session, None, &Options::default());
        assert_eq!(compacted["input"][1]["output"], file);
        // Switched off.
        let mut off =
            json!({ "input": [call("c1", "cat a"), output("c1", &file), call("c2", "cat a"), output("c2", &file)] });
        transform(
            &mut off,
            &mut Session::default(),
            None,
            &Options {
                shorten_outputs: false,
                ..Options::default()
            },
        );
        assert_eq!(off["input"][3]["output"], file);
    }

    #[test]
    fn a_pointer_names_the_run_it_repeats() {
        // Run A fails, run B passes, run A' fails exactly as A did.
        let a: String = (0..80)
            .map(|i| format!("step {i}: building the order fixtures for the failing case\n"))
            .collect();
        let a = format!("worker 7 did not start, retrying\n{a}");
        let b: String = (0..80)
            .map(|i| format!("step {i}: building the order fixtures for the passing case\n"))
            .collect();
        let items = vec![
            call("c1", "make check"),
            output("c1", &a),
            call("c2", "make check"),
            output("c2", &b),
            call("c3", "make check"),
            output("c3", &a),
        ];
        let mut body = json!({ "input": items.clone() });
        let mut session = Session::default();
        transform(&mut body, &mut session, None, &Options::default());
        let pointer = "lge: identical to the output of run 1 of `make check` earlier in this conversation (81 lines, first line: \"worker 7 did not start, retrying\"); it is not repeated here.\n";
        assert_eq!(body["input"][5]["output"], pointer);
        assert_eq!(body["input"][3]["output"], b.as_str());
        let mut next = json!({ "input": items });
        next["input"].as_array_mut().unwrap().push(user("and now?"));
        transform(&mut next, &mut session, None, &Options::default());
        assert_eq!(next["input"][5]["output"], pointer, "the same bytes next turn");
    }

    #[test]
    fn a_rerun_with_nothing_changed_between_is_sent_whole() {
        let run = clippy(30);
        let items = vec![
            call("c1", "cargo clippy --all-targets"),
            output("c1", &run),
            call("c2", "cargo clippy --all-targets"),
            output("c2", &run),
            call("c3", "cargo clippy --all-targets"),
            output("c3", &run),
        ];
        let mut body = json!({ "input": items.clone() });
        let mut session = Session::default();
        transform(&mut body, &mut session, None, &Options::default());
        let out = texts(&body);
        assert!(
            out[1].contains("Output:\nok: 30 warnings\n"),
            "the first run is shortened: {}",
            out[1]
        );
        assert!(
            out[1].contains("lge: output of `cargo` reduced to its result and its 30 warnings"),
            "{}",
            out[1]
        );
        assert_eq!(out[3], run, "the re-run with nothing changed is sent whole");
        assert!(
            out[5].contains("lge: identical to the output of run 2 of `cargo clippy --all-targets`"),
            "a third identical run points at the second, never at the shortened first: {}",
            out[5]
        );
        let key = output_key(
            "c2",
            "cargo clippy --all-targets",
            run.split("Output:\n").nth(1).unwrap(),
        );
        assert!(session.is_raw_call(&key));
        // Stable on the next turn, and after a settings change.
        let mut next = json!({ "input": items.clone() });
        transform(&mut next, &mut session, None, &Options::default());
        assert_eq!(texts(&next), out);
        let mut off = json!({ "input": items });
        transform(
            &mut off,
            &mut session,
            None,
            &Options {
                shorten_outputs: false,
                ..Options::default()
            },
        );
        assert_eq!(texts(&off), out);
    }

    #[test]
    fn a_rerun_after_apply_patch_is_shortened_again() {
        let run = clippy(30);
        let mut body = json!({ "input": [
            call("c1", "cargo clippy --all-targets"), output("c1", &run),
            patch("p1", "src/a.rs"), patched("p1"),
            call("c2", "cargo clippy --all-targets"), output("c2", &run),
        ] });
        transform(&mut body, &mut Session::default(), None, &Options::default());
        let out = texts(&body);
        assert!(out[1].contains("lge: output of `cargo` reduced"), "{}", out[1]);
        assert!(out[5].contains("lge: output of `cargo` reduced"), "{}", out[5]);
        // A command that may write files counts as an edit too; a read does not.
        let mut write = json!({ "input": [
            call("c1", "cargo clippy --all-targets"), output("c1", &run),
            call("c2", "cargo fmt"), output("c2", ""),
            call("c3", "cargo clippy --all-targets"), output("c3", &run),
        ] });
        transform(&mut write, &mut Session::default(), None, &Options::default());
        assert!(texts(&write)[5].contains("lge: output of `cargo` reduced"));
        let mut read = json!({ "input": [
            call("c1", "cargo clippy --all-targets"), output("c1", &run),
            call("c2", "cat src/a.rs"), output("c2", "fn main() {}\n"),
            call("c3", "cargo clippy --all-targets"), output("c3", &run),
        ] });
        transform(&mut read, &mut Session::default(), None, &Options::default());
        assert_eq!(texts(&read)[5], run);
    }

    #[test]
    fn an_lge_raw_prefix_is_never_shortened() {
        let run = clippy(30);
        let file = file_text();
        let mut body = json!({ "input": [
            call("c1", "LGE_RAW=1 cargo clippy --all-targets"), output("c1", &run),
            call("c2", "cat src/app.py"), output("c2", &file),
            call("c3", "LGE_RAW=1 cat src/app.py"), output("c3", &file),
        ] });
        let before = body.clone();
        let seen = transform(&mut body, &mut Session::default(), None, &Options::default());
        assert_eq!(seen.outputs_replaced, 0);
        assert_eq!(body, before, "not reduced and not pointed at");
    }

    #[test]
    fn turning_shortening_off_mid_conversation_keeps_past_items() {
        let run = clippy(30);
        let first = vec![call("c1", "cargo clippy --all-targets"), output("c1", &run)];
        let mut session = Session::default();
        let mut body = json!({ "input": first.clone() });
        transform(&mut body, &mut session, None, &Options::default());
        let shortened = body["input"][1].clone();
        assert_ne!(shortened, first[1]);
        let off = Options {
            shorten_outputs: false,
            ..Options::default()
        };
        let mut items = first.clone();
        items.extend([call("c2", "pytest -v"), output("c2", &passing_run())]);
        let mut next = json!({ "input": items.clone() });
        let seen = transform(&mut next, &mut session, None, &off);
        assert_eq!(next["input"][1], shortened, "the old item keeps its bytes");
        assert_eq!(next["input"][3], items[3], "a new item is untouched");
        assert_eq!(seen.lge_break_item, None);
        // Switched on again: the item decided while it was off stays as it was.
        let mut again = json!({ "input": items.clone() });
        transform(&mut again, &mut session, None, &Options::default());
        assert_eq!(again["input"][3], items[3]);
    }

    #[test]
    fn a_pointer_survives_a_settings_change() {
        let file = file_text();
        let items = vec![
            call("c1", "cat src/app.py"),
            output("c1", &file),
            call("c2", "cat src/app.py"),
            output("c2", &file),
        ];
        let mut session = Session::default();
        let mut body = json!({ "input": items.clone() });
        transform(&mut body, &mut session, None, &Options::default());
        let pointer = body["input"][3].clone();
        assert_ne!(pointer, items[3]);
        for opts in [
            Options {
                shorten_outputs: false,
                ..Options::default()
            },
            Options {
                rules: crate::rules::RuleModes::all(Mode::Off),
                ..Options::default()
            },
            Options {
                holdout: true,
                ..Options::default()
            },
        ] {
            let mut again = json!({ "input": items.clone() });
            transform(&mut again, &mut session, None, &opts);
            assert_eq!(again["input"][3], pointer, "{opts:?}");
        }
    }

    #[test]
    fn promotion_from_shadow_to_on_applies_to_new_items_only() {
        let lock: String = {
            let mut text = String::from("# This file is automatically @generated by Cargo.\nversion = 4\n\n");
            for i in 0..80 {
                text.push_str(&format!(
                    "[[package]]\nname = \"crate-number-{i}\"\nversion = \"1.{i}.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"{:064x}\"\n\n",
                    i * 7919
                ));
            }
            text
        };
        let mut session = Session::default();
        let first = vec![call("c1", "cat Cargo.lock"), output("c1", &lock)];
        let mut body = json!({ "input": first.clone() });
        transform(&mut body, &mut session, None, &Options::default());
        assert_eq!(body["input"][1], first[1], "lockfile is in shadow by default");
        assert_eq!(session.saved.by_rule["lockfile"].shadow_items, 1);
        let mut on = Options::default();
        on.rules.lockfile = Mode::On;
        let mut items = first.clone();
        items.extend([call("c2", "cat Cargo.lock"), output("c2", &lock)]);
        let mut next = json!({ "input": items });
        transform(&mut next, &mut session, None, &on);
        assert_eq!(next["input"][1], first[1], "decided in shadow: stays as it was");
        let new = next["input"][3]["output"].as_str().unwrap();
        assert!(
            new.starts_with("lge: Cargo.lock shown as its package list (80 packages"),
            "{new}"
        );
        assert_eq!(session.saved.by_rule["lockfile"].items, 1);
    }

    #[test]
    fn holdout_forwards_bodies_byte_identical_and_counts_what_would_be_saved() {
        let run = clippy(30);
        let file = file_text();
        let mut body = json!({ "input": [
            call("c1", "cargo clippy --all-targets"), output("c1", &run),
            call("c2", "cat src/app.py"), output("c2", &file),
            call("c3", "cat src/app.py"), output("c3", &file),
        ] });
        let before = body.clone();
        let mut session = Session::default();
        let opts = Options {
            holdout: true,
            ..Options::default()
        };
        let seen = transform(&mut body, &mut session, None, &opts);
        assert_eq!(body, before);
        assert_eq!(seen.outputs_replaced, 0);
        assert_eq!(seen.removed_present, 0);
        let check = session.saved.by_rule["check"];
        assert_eq!((check.items, check.shadow_items), (0, 1));
        assert!(check.shadow_tokens_after < check.shadow_tokens_before);
        assert_eq!(session.saved.by_rule["repeat_pointer"].shadow_items, 1);
        assert_eq!(session.saved.outputs_shortened, 0);
    }

    #[test]
    fn exit_codes_and_running_states_are_read_from_every_frame() {
        let status = |raw: &str, lines: bool| match unframe(raw, lines) {
            Framed::Plain { status, .. } | Framed::Json { status, .. } => status,
            Framed::JsonLines { lines, .. } => lines.iter().find_map(|(_, e)| e.as_ref()).unwrap().status,
        };
        let done = |code| RunStatus {
            exit: Some(code),
            running: false,
            masked: false,
        };
        let running = RunStatus {
            exit: None,
            running: true,
            masked: false,
        };
        assert_eq!(
            status("Exit code: 2\nWall time: 1.0 seconds\nOutput:\nx", false),
            done(2)
        );
        assert_eq!(
            status(
                "Chunk ID: ab\nWall time: 1.0 seconds\nProcess exited with code -1\nOutput:\nx",
                false
            ),
            done(-1)
        );
        assert_eq!(
            status("Chunk ID: ab\nWall time: 10.0 seconds\nProcess running with session ID 4711\nOriginal token count: 9\nOutput:\nx", false),
            running
        );
        assert_eq!(
            status(
                r#"{"output":"x","metadata":{"exit_code":1,"duration_seconds":0.5}}"#,
                false
            ),
            done(1)
        );
        assert_eq!(
            status(
                r#"{"chunk_id":"ab","wall_time_seconds":0.5,"exit_code":3,"output":"x"}"#,
                true
            ),
            done(3)
        );
        assert_eq!(
            status(
                r#"{"chunk_id":"ab","wall_time_seconds":10.0,"session_id":17,"output":"x"}"#,
                true
            ),
            running
        );
        assert_eq!(status("no frame at all", false), RunStatus::default());
        // JSON lines are read only from what a script printed.
        assert!(matches!(
            unframe(
                r#"{"chunk_id":"ab","wall_time_seconds":0.5,"exit_code":3,"output":"x"}"#,
                false
            ),
            Framed::Json { .. }
        ));
        // A key the exec result does not have: not an exec result.
        assert!(!matches!(
            unframe(r#"{"chunk_id":"ab","exit_code":3,"output":"x","path":"a"}"#, true),
            Framed::JsonLines { .. }
        ));
    }

    #[test]
    fn json_lines_splice_keeps_every_byte_outside_the_output() {
        let line = r#"{"chunk_id":"ab","wall_time_seconds":3.458e-6,"exit_code":0,"original_token_count":12,"output":"a\u001b[1m\"b\"\n"}"#;
        let raw = format!("Warning: truncated output (original token count: 9)\nTotal output lines: 3\n\n{line}\nnot json\n{{\"chunk_id\":\"cut");
        let Framed::JsonLines { head, lines } = unframe(&raw, true) else {
            panic!("not read as JSON lines");
        };
        assert_eq!(
            head,
            "Warning: truncated output (original token count: 9)\nTotal output lines: 3\n\n"
        );
        assert_eq!(lines.len(), 3);
        let exec = lines[0].1.as_ref().unwrap();
        assert_eq!(exec.output, "a\u{1b}[1m\"b\"\n");
        assert!(lines[1].1.is_none() && lines[2].1.is_none());
        assert_eq!(rejoin(&head, &lines, &[None, None, None]), raw);
        let spliced = rejoin(&head, &lines, &[Some("ok".to_string()), None, None]);
        assert_eq!(
            spliced,
            raw.replace(r#""a\u001b[1m\"b\"\n""#, r#""ok""#),
            "only the output token changes"
        );
        // An escape serde_json would write differently: the line is left alone.
        let other = r#"{"chunk_id":"ab","exit_code":0,"output":"a\/b"}"#;
        let Framed::JsonLines { lines, .. } = unframe(other, true) else {
            panic!()
        };
        assert!(lines[0].1.is_none());
    }

    #[test]
    fn an_output_without_a_known_command_is_judged_by_its_content() {
        // A script run (code mode) with two commands, each printed by its own
        // text() call: each part is matched to its command.
        let listing: String = (0..200)
            .map(|i| {
                format!(
                    "src/mod_{}/views.py:{}:    return render(request, template)\n",
                    i % 25,
                    i
                )
            })
            .collect();
        let mut body = json!({ "input": [
            { "type": "custom_tool_call", "call_id": "s1", "name": "exec", "input": "const r = await tools.exec_command({cmd: 'pytest -v'}); text(r.output);\nconst s = await tools.exec_command({cmd: 'rg -n render src'}); text(s.output);" },
            { "type": "custom_tool_call_output", "call_id": "s1", "output": [
                { "type": "input_text", "text": "Script completed\nWall time 9.9 seconds\nOutput:\n" },
                { "type": "input_text", "text": passing_run() },
                { "type": "input_image", "image_url": "data:image/png;base64,AAAA" },
                { "type": "input_text", "text": listing },
            ] },
            output("zz", &passing_run()),
            output("yy", "a short note"),
            { "type": "function_call_output", "call_id": "m1", "output": "Wall time: 0.1 seconds\nOutput:\n{\"rows\": [1, 2, 3]}" },
        ] });
        let before = body.clone();
        let mut session = Session::default();
        let seen = transform(&mut body, &mut session, None, &Options::default());
        let parts = body["input"][1]["output"].as_array().unwrap();
        assert_eq!(seen.outputs_replaced, 3);
        assert_eq!(parts.len(), 4, "the number of parts never changes");
        assert_eq!(parts[0], before["input"][1]["output"][0]);
        assert!(parts[1]["text"].as_str().unwrap().starts_with("ok"), "{}", parts[1]);
        assert!(
            parts[1]["text"]
                .as_str()
                .unwrap()
                .contains("lge: output of `pytest` reduced"),
            "matched to its command: {}",
            parts[1]
        );
        assert_eq!(parts[2], before["input"][1]["output"][2]);
        assert!(
            parts[3]["text"]
                .as_str()
                .unwrap()
                .contains("lge: 60 of 200 matching lines shown"),
            "{}",
            parts[3]
        );
        assert!(body["input"][2]["output"].as_str().unwrap().starts_with("ok"));
        assert_eq!(
            body["input"][0], before["input"][0],
            "the script itself is never touched"
        );
        assert_eq!(body["input"][3], before["input"][3]);
        assert_eq!(
            body["input"][4], before["input"][4],
            "an unrecognised result passes through"
        );
        // Decided once: the same request again gives the same bytes.
        let mut again = before.clone();
        transform(&mut again, &mut session, None, &Options::default());
        assert_eq!(again, body);
        assert_eq!(session.saved.outputs_shortened, 3, "each item counted once");
    }

    #[test]
    fn json_from_a_tool_that_is_not_a_file_reader_is_minified() {
        let rows: Vec<Value> = (0..200)
            .map(|i| json!({ "id": i, "name": format!("order {i}"), "status": "open" }))
            .collect();
        let pretty = serde_json::to_string_pretty(&json!({ "rows": rows })).unwrap();
        let framed = format!("Wall time: 0.1000 seconds\nOutput:\n{pretty}");
        let mut body = json!({ "input": [
            { "type": "function_call", "name": "query", "namespace": "mcp__db__", "call_id": "m1", "arguments": "{}" },
            output("m1", &framed),
            { "type": "function_call", "name": "read_file", "namespace": "mcp__fs__", "call_id": "m2", "arguments": "{}" },
            output("m2", &framed),
        ] });
        let seen = transform(&mut body, &mut Session::default(), None, &Options::default());
        assert_eq!(seen.outputs_replaced, 1);
        assert_eq!(seen.called_tools, ["mcp__db__query", "mcp__fs__read_file"]);
        let small = body["input"][1]["output"].as_str().unwrap();
        assert!(
            small.starts_with("Wall time: 0.1000 seconds\nOutput:\n{\"rows\":[{\"id\":0,"),
            "{small}"
        );
        assert_eq!(
            body["input"][3]["output"],
            framed.as_str(),
            "a file tool's output stays"
        );
    }

    #[test]
    fn paths_are_folded_and_progress_noise_is_measured_in_shadow() {
        let paths: String = (0..120)
            .map(|i| format!("crates/lge-proxy/src/module_number_{}/file_{i}.rs\n", i / 30))
            .collect();
        let mut install: String = (0..150)
            .map(|i| format!("Collecting package-number-{i}==1.{i}.0\n  Downloading package_number_{i}-1.{i}.0-py3-none-any.whl (12 kB)\n"))
            .collect();
        install.push_str("Installing collected packages: many\nSuccessfully installed 150 packages\n");
        let framed = format!("Chunk ID: 9f\nWall time: 3.1000 seconds\nProcess exited with code 0\nOutput:\n{install}");
        let mut body = json!({ "input": [
            call("c1", "git ls-files"), output("c1", &paths),
            call("c2", "pip install -r requirements.txt"), output("c2", &framed),
        ] });
        let mut session = Session::default();
        let seen = transform(&mut body, &mut session, None, &Options::default());
        let folded = body["input"][1]["output"].as_str().unwrap();
        assert!(folded.starts_with(crate::fold::HEADER), "{folded}");
        assert_eq!(crate::fold::unfold_paths(folded), paths, "lossless");
        assert_eq!(session.saved.by_rule["path_fold"].items, 1);
        let noise = session.saved.by_rule.get("noise").copied().unwrap_or_default();
        assert_eq!(noise.items, 0, "noise is in shadow by default");
        assert_eq!(noise.shadow_items, 1, "{:?}", session.saved.by_rule);
        let mut on = Options::default();
        on.rules.noise = Mode::On;
        let mut again = json!({ "input": [call("c9", "pip install -r requirements.txt"), output("c9", &framed)] });
        transform(&mut again, &mut Session::default(), None, &on);
        let short = again["input"][1]["output"].as_str().unwrap();
        assert!(
            short.ends_with("lge: 301 progress lines of `pip install` left out (exit code 0).\n"),
            "{short}"
        );
        // Without a known exit code 0, progress lines stay.
        let unknown = json!({ "input": [call("c8", "pip install -r requirements.txt"), output("c8", &install)] });
        let mut unknown_body = unknown.clone();
        transform(&mut unknown_body, &mut Session::default(), None, &on);
        assert_eq!(unknown_body, unknown);
        assert!(seen.outputs_replaced >= 1);
    }

    #[test]
    fn reruns_and_rereads_are_measured_as_deltas_in_shadow() {
        let log = |changed: &str| -> String {
            (0..150)
                .map(|i| {
                    if i == 75 {
                        format!("{changed}\n")
                    } else {
                        format!("job {i}: compiled the module for the order service in release mode\n")
                    }
                })
                .collect()
        };
        let file = file_text();
        let middle: String = file.lines().skip(9).take(51).map(|l| format!("{l}\n")).collect();
        let mut body = json!({ "input": [
            call("c1", "make all"), output("c1", &log("job 75: failed")),
            call("c2", "make all"), output("c2", &log("job 75: done")),
            call("c3", "cat src/app.py"), output("c3", &file),
            call("c4", "sed -n '10,60p' src/app.py"), output("c4", &middle),
        ] });
        let before = body.clone();
        let mut session = Session::default();
        transform(&mut body, &mut session, None, &Options::default());
        assert_eq!(body, before, "measured only");
        assert_eq!(session.saved.by_rule["rerun_delta"].shadow_items, 1);
        assert_eq!(session.saved.by_rule["read_delta"].shadow_items, 1);
        let r = session.saved.by_rule["read_delta"];
        assert!(r.shadow_tokens_after * 5 < r.shadow_tokens_before, "{r:?}");
        // Switched off, nothing is measured.
        let mut off = Options::default();
        off.rules.rerun_delta = Mode::Off;
        off.rules.read_delta = Mode::Off;
        let mut quiet = Session::default();
        transform(&mut before.clone(), &mut quiet, None, &off);
        assert!(!quiet.saved.by_rule.contains_key("rerun_delta") && !quiet.saved.by_rule.contains_key("read_delta"));
    }

    #[test]
    fn removed_tokens_are_summed_over_the_items_present() {
        let run = clippy(30);
        let mut session = Session::default();
        let opts = Options::default();
        let one = vec![call("c1", "cargo clippy --all-targets"), output("c1", &run)];
        let mut body = json!({ "input": one.clone() });
        let first = transform(&mut body, &mut session, None, &opts);
        assert!(first.removed_present > 1000, "{}", first.removed_present);
        let mut two = one.clone();
        two.extend([call("c2", "pytest -v"), output("c2", &passing_run())]);
        let mut body = json!({ "input": two.clone() });
        let second = transform(&mut body, &mut session, None, &opts);
        assert!(second.removed_present > first.removed_present);
        let mut body = json!({ "input": two });
        let third = transform(&mut body, &mut session, None, &opts);
        assert_eq!(
            third.removed_present, second.removed_present,
            "replayed items count every time"
        );
        // After a compaction the items are gone, and so is what they removed.
        let mut body = json!({ "input": [user("summary of the work so far")] });
        let after = transform(&mut body, &mut session, None, &opts);
        assert_eq!(after.removed_present, 0);
        assert_eq!(after.client_break_item, Some(0));
    }

    #[test]
    fn the_prefix_monitor_tells_lge_breaks_from_client_breaks() {
        let run = clippy(30);
        let items = vec![
            user("fix the warnings"),
            call("c1", "cargo clippy --all-targets"),
            output("c1", &run),
        ];
        let mut session = Session::default();
        for _ in 0..2 {
            let mut body = json!({ "input": items.clone() });
            let seen = transform(&mut body, &mut session, None, &Options::default());
            assert_eq!((seen.lge_break_item, seen.client_break_item), (None, None));
        }
        assert_eq!(session.prefix, crate::session::PrefixStats::default());
        // A forced re-decision: the item gets other bytes while the client's are the same.
        session.forget_outputs();
        let mut off = Options::default();
        off.rules.check = Mode::Off;
        let mut body = json!({ "input": items.clone() });
        let seen = transform(&mut body, &mut session, None, &off);
        assert_eq!(seen.lge_break_item, Some(2));
        assert_eq!(session.prefix.lge_breaks, 1);
        assert_eq!(session.prefix.first_lge_break_item, Some(2));
        assert_eq!(session.prefix.client_breaks, 0);
        // A compaction replaces the items: a client break, not an lge one.
        let mut body = json!({ "input": [user("compacted summary"), call("c9", "ls"), output("c9", "a\nb\n")] });
        let seen = transform(&mut body, &mut session, None, &Options::default());
        assert_eq!((seen.lge_break_item, seen.client_break_item), (None, Some(0)));
        assert_eq!((session.prefix.lge_breaks, session.prefix.client_breaks), (1, 1));
    }

    #[test]
    fn a_retry_makes_only_this_requests_new_decisions_raw() {
        let run = clippy(30);
        let mut session = Session::default();
        let opts = Options::default();
        let one = vec![call("c1", "cargo clippy --all-targets"), output("c1", &run)];
        let mut body = json!({ "input": one.clone() });
        transform(&mut body, &mut session, None, &opts);
        let old = body["input"][1].clone();
        let saved = session.saved.clone();
        let prefix = session.prefix.clone();

        let mut two = one.clone();
        two.extend([call("c2", "pytest -v"), output("c2", &passing_run())]);
        let mut body = json!({ "input": two.clone() });
        let seen = transform(&mut body, &mut session, None, &opts);
        assert_eq!(seen.new_decisions, 1);
        assert_ne!(body["input"][3], two[3]);
        session.revert_new();
        assert_eq!(session.saved, saved, "the counters go back");
        let mut retry = json!({ "input": two.clone() });
        let seen = transform(&mut retry, &mut session, None, &opts);
        assert_eq!(seen.new_decisions, 0);
        assert_eq!(retry["input"][1], old, "an old decision replays");
        assert_eq!(retry["input"][3], two[3], "the new one is sent as it is");
        assert_eq!(session.saved, saved);
        assert_eq!(session.prefix, prefix, "a retry is not a cache break");
    }

    #[test]
    fn bodies_of_another_shape_pass_through() {
        for mut body in [
            json!({}),
            json!({ "input": "a plain string" }),
            json!({ "messages": [{ "role": "user", "content": "hi" }] }),
            json!([1, 2, 3]),
        ] {
            let before = body.clone();
            transform(&mut body, &mut Session::default(), None, &Options::default());
            assert_eq!(body, before);
        }
    }

    #[test]
    fn a_json_framed_output_keeps_its_frame() {
        let run = passing_run();
        let framed = json!({ "output": run, "metadata": { "exit_code": 0, "duration_seconds": 9.9 } }).to_string();
        let mut body = json!({ "input": [call("c1", "pytest -v"), output("c1", &framed)] });
        transform(&mut body, &mut Session::default(), None, &Options::default());
        let new: Value = serde_json::from_str(body["input"][1]["output"].as_str().unwrap()).unwrap();
        assert!(new["output"].as_str().unwrap().starts_with("ok"), "{new}");
        assert_eq!(new["metadata"]["exit_code"], 0);
        // A non-zero exit with a passing tally is not called a pass.
        let failed = json!({ "output": run, "metadata": { "exit_code": 1, "duration_seconds": 9.9 } }).to_string();
        let mut body = json!({ "input": [call("c1", "pytest -v"), output("c1", &failed)] });
        transform(&mut body, &mut Session::default(), None, &Options::default());
        assert_eq!(body["input"][1]["output"], failed.as_str());
    }

    #[test]
    fn typed_messages_are_told_from_what_the_agent_adds() {
        let typed = user("please fix the login bug");
        let mut tagged = user("# AGENTS.md instructions for /repo\n\n<INSTRUCTIONS>\nbe nice\n</INSTRUCTIONS>");
        assert!(!is_injected(&typed, "please fix the login bug"));
        assert!(is_injected(&tagged, "# AGENTS.md instructions for /repo"));
        for text in [
            "<environment_context>\n<cwd>/r</cwd>",
            "  <turn_aborted>",
            "<lge_context>x",
            "<anything else>",
        ] {
            assert!(is_injected(&typed, text), "{text}");
        }
        // The summary Codex writes when it compacts is not typed either.
        assert!(is_injected(
            &typed,
            "Another language model started to solve this problem and produced a summary of its thinking process."
        ));
        // The agent's own labels win over the text when they are present.
        tagged["internal_chat_message_metadata_passthrough"] = json!({ "content_item_kinds": ["user.text"] });
        assert!(!is_injected(&tagged, "<looks like a tag>"));
        tagged["internal_chat_message_metadata_passthrough"] =
            json!({ "content_item_kinds": ["agents_md.instructions", "environments.environment_context"] });
        assert!(is_injected(&tagged, "please fix the login bug"));
        // Both parts of the agent's context message are read for the working directory.
        let body = json!({ "input": [{ "type": "message", "role": "user", "content": [
            { "type": "input_text", "text": "# AGENTS.md instructions for /repo\n\n<INSTRUCTIONS>\nx\n</INSTRUCTIONS>" },
            { "type": "input_text", "text": "<environment_context>\n  <cwd>/repo/app</cwd>\n  <shell>zsh</shell>\n</environment_context>" },
        ] }, typed] });
        let seen = inspect(&body);
        assert_eq!(
            (seen.cwd.as_deref(), seen.first_user_message.as_str()),
            (Some("/repo/app"), "please fix the login bug")
        );
    }

    #[test]
    fn background_requests_are_told_by_header_metadata_model_and_title_prompt() {
        let plain = json!({ "model": "gpt-test", "input": [user("<environment_context>x</environment_context>"), user("fix the login bug")] });
        assert_eq!(background_label(None, None, &plain), None);
        // Codex's sub-agent header, any label but a spawned agent's.
        for label in ["guardian", "review", "compact", "memory_consolidation", "my-task"] {
            assert_eq!(background_label(Some(label), None, &plain).as_deref(), Some(label));
        }
        assert_eq!(background_label(Some("collab_spawn"), None, &plain), None);
        assert_eq!(background_label(Some(" "), None, &plain).as_deref(), Some("subagent"));
        // The same key in the body's client metadata.
        let mut meta = plain.clone();
        meta["client_metadata"] = json!({ "x-openai-subagent": "guardian", "thread_id": "t" });
        assert_eq!(background_label(None, None, &meta).as_deref(), Some("guardian"));
        meta["client_metadata"]["x-openai-subagent"] = json!("collab_spawn");
        assert_eq!(background_label(None, None, &meta), None);
        // The approval reviewer's model, in any case.
        let mut model = plain.clone();
        model["model"] = json!("Codex-Auto-Review");
        assert_eq!(background_label(None, None, &model).as_deref(), Some("guardian"));
        // The desktop app's title prompt, in the instructions or the first typed message.
        let prompt = format!("{}.\n\nUser prompt: fix the login bug", lge_usage::TITLE_PROMPT);
        let mut instructions = plain.clone();
        instructions["instructions"] = json!(prompt);
        assert_eq!(background_label(None, None, &instructions).as_deref(), Some("title"));
        let first = json!({ "model": "gpt-test", "input": [user("<environment_context>x</environment_context>"), user(&prompt)] });
        assert_eq!(background_label(None, None, &first).as_deref(), Some("title"));
        // Quoted later, or after a typed message, it is not one.
        let later = json!({ "model": "gpt-test", "input": [user("fix the login bug"), user(&prompt)] });
        assert_eq!(background_label(None, None, &later), None);
        let quoted = json!({ "input": [user(&format!("why does this start with: {prompt}"))] });
        assert_eq!(background_label(None, None, &quoted), None);

        // Codex's own temporary threads (a task title, a recap) and its
        // reviewer and memory threads, by the turn metadata's thread source,
        // in the body's client metadata or in the header.
        let turn = |source: &str| json!({ "turn_id": "t1", "thread_source": source }).to_string();
        for (source, label) in [
            ("thread_title", "title"),
            ("system", "recap"),
            ("guardian_review", "guardian"),
            ("memory_consolidation", "memory_consolidation"),
        ] {
            let mut body = plain.clone();
            body["client_metadata"] = json!({ "x-codex-turn-metadata": turn(source) });
            assert_eq!(background_label(None, None, &body).as_deref(), Some(label), "{source}");
            assert_eq!(
                background_label(None, Some(&turn(source)), &plain).as_deref(),
                Some(label),
                "{source}"
            );
        }
        for source in ["user", "subagent", "automation"] {
            assert_eq!(background_label(None, Some(&turn(source)), &plain), None, "{source}");
        }
        assert_eq!(background_label(None, Some("not json"), &plain), None);

        // Codex's request for a compaction's summary is told by its request kind.
        let mut summary = plain.clone();
        summary["client_metadata"] = json!({ "x-codex-turn-metadata": r#"{"request_kind":"compaction"}"# });
        assert!(compaction_request(None, &summary));
        assert!(compaction_request(Some(r#"{"request_kind":"compaction"}"#), &plain));
        assert!(!compaction_request(Some(r#"{"request_kind":"turn"}"#), &plain));
        assert!(!compaction_request(None, &plain));
        assert_eq!(background_label(None, None, &summary), None, "not a background session");
    }

    #[test]
    fn the_reviewer_and_memory_prompts_are_background_only_up_to_the_first_typed_message() {
        let env = user("<environment_context>\n  <cwd>/repo</cwd>\n</environment_context>");
        for (prompt, label) in [
            (lge_usage::GUARDIAN_PROMPT, "guardian"),
            (lge_usage::MEMORY_PROMPT, "memory_consolidation"),
            (lge_usage::TITLE_PROMPT, "title"),
        ] {
            let text = format!("{prompt}\n\nThe transcript follows.");
            let first = json!({ "model": "gpt-test", "input": [env.clone(), user(&text)] });
            assert_eq!(background_label(None, None, &first).as_deref(), Some(label));
            let instructions = json!({ "model": "gpt-test", "instructions": text, "input": [env.clone(), user("go")] });
            assert_eq!(background_label(None, None, &instructions).as_deref(), Some(label));
            // After a typed message, or quoted inside one, it is not one.
            let later = json!({ "model": "gpt-test", "input": [env.clone(), user("fix the login bug"), user(&text)] });
            assert_eq!(background_label(None, None, &later), None, "{label}");
            let quoted = json!({ "model": "gpt-test", "input": [user(&format!("what does this mean: {text}"))] });
            assert_eq!(background_label(None, None, &quoted), None, "{label}");
        }
    }

    #[test]
    fn the_working_folder_comes_only_from_codexs_environment_context() {
        let context = json!({ "type": "message", "role": "user", "content": [
            { "type": "input_text", "text": "# AGENTS.md instructions for /repo\n\n<INSTRUCTIONS>\nbe brief\n</INSTRUCTIONS>" },
            { "type": "input_text", "text": "<environment_context>\n  <cwd>/repo</cwd>\n</environment_context>" },
        ] });
        let typed = user("look at <cwd>/elsewhere</cwd>");
        let both = json!({ "input": [typed.clone(), context] });
        assert_eq!(inspect(&both).cwd.as_deref(), Some("/repo"));
        let single = json!({ "input": [typed.clone(), user("<environment_context>\n  <cwd>/repo</cwd>\n</environment_context>")] });
        assert_eq!(inspect(&single).cwd.as_deref(), Some("/repo"));
        let pasted =
            json!({ "input": [typed, user("<user_shell_command>\n<cwd>/other</cwd>\n</user_shell_command>")] });
        assert_eq!(
            inspect(&pasted).cwd,
            None,
            "only the environment context names the folder"
        );
    }

    #[test]
    fn a_genuine_first_request_is_told_from_one_that_carries_history() {
        let env = user("<environment_context>\n  <cwd>/repo</cwd>\n</environment_context>");
        let developer =
            json!({ "type": "message", "role": "developer", "content": [{ "type": "input_text", "text": "rules" }] });
        let tools = json!({ "type": "additional_tools", "tools": [] });
        let long = format!(
            "rename shout_text {} then update every caller in the billing service",
            "x".repeat(300)
        );
        let first = json!({ "input": [tools, developer, env.clone(), user(&long)] });
        let seen = inspect(&first);
        assert!(!seen.has_history, "{seen:?}");
        assert_eq!(seen.first_user_message.chars().count(), 200);
        assert!(
            seen.first_user_text.ends_with("billing service"),
            "the router reads it all"
        );
        let with = |extra: Value| json!({ "input": [env.clone(), user("fix it"), extra] });
        for extra in [
            json!({ "type": "reasoning", "summary": [], "encrypted_content": "x" }),
            json!({ "type": "function_call", "name": "shell", "call_id": "c1", "arguments": "{}" }),
            json!({ "type": "function_call_output", "call_id": "c1", "output": "ok" }),
            json!({ "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": "done" }] }),
            json!({ "type": "compaction", "encrypted_content": "x" }),
            json!({ "type": "item_reference", "id": "msg_1" }),
            user("and the other one too"),
            user(
                "Another language model started to solve this problem and produced a summary of its thinking process.",
            ),
            user("<turn_aborted>\nThe user interrupted the previous turn.\n</turn_aborted>"),
        ] {
            assert!(inspect(&with(extra.clone())).has_history, "{extra}");
        }
        let mut chained = first.clone();
        chained["previous_response_id"] = json!("resp_1");
        assert!(inspect(&chained).has_history);
    }

    #[test]
    fn usage_is_found_in_a_stream_cut_anywhere() {
        let stream = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n\
            event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\",\"usage\":{\"input_tokens\":1200,\"input_tokens_details\":{\"cached_tokens\":1000},\"output_tokens\":300,\"output_tokens_details\":{\"reasoning_tokens\":250},\"total_tokens\":1500}}}\n\n";
        let want = Usage {
            input_tokens: 1200,
            cached_input_tokens: 1000,
            output_tokens: 300,
            reasoning_tokens: 250,
        };
        for size in [1, 7, 64, stream.len()] {
            let mut scanner = UsageScanner::default();
            for chunk in stream.as_bytes().chunks(size) {
                scanner.feed(chunk);
            }
            assert_eq!(scanner.usage, Some(want), "chunk size {size}");
        }
        assert_eq!(usage_of_event("{\"type\":\"response.created\",\"response\":{}}"), None);
        assert_eq!(usage_of_event("not json"), None);
        let mut none = UsageScanner::default();
        none.feed(b"data: [DONE]\n\n");
        assert_eq!(none.usage, None);
    }

    #[test]
    fn the_largest_new_outputs_and_the_tools_called_are_reported() {
        let mut body = json!({
            "prompt_cache_key": "conv-n",
            "input": [
                user("run the tests"),
                call("c1", "pytest -v"),
                output("c1", &passing_run()),
                call("c2", "ls"),
                output("c2", "a.py\nb.py\n"),
                { "type": "function_call", "name": "shell", "namespace": "functions", "call_id": "c3", "arguments": json!({ "command": ["bash", "-lc", "cat a.py"] }).to_string() },
                output("c3", "print('a')\n"),
                { "type": "local_shell_call", "call_id": "c4", "status": "completed", "action": { "type": "exec", "command": ["bash", "-lc", "pwd"] } },
                output("c4", "/repo\n"),
                { "type": "web_search_call", "id": "ws_1", "status": "completed" },
            ],
        });
        let mut session = Session::default();
        let seen = transform(&mut body.clone(), &mut session, None, &Options::default());
        assert_eq!(
            seen.called_tools,
            ["exec_command", "shell", "local_shell", "web_search"]
        );
        let largest: Vec<(&str, bool)> = seen
            .largest_new_outputs
            .iter()
            .map(|o| (o.command.as_str(), o.shortened))
            .collect();
        assert_eq!(largest.len(), LARGEST_NEW_OUTPUTS);
        assert_eq!(largest[0], ("pytest -v", true));
        assert!(seen.largest_new_outputs[0].tokens > seen.largest_new_outputs[1].tokens);
        // Codex's default namespace is no namespace: the call is a shell call.
        assert!(largest.iter().any(|(c, _)| *c == "cat a.py"), "{largest:?}");

        // Next turn: only the items added since count as new.
        body["input"].as_array_mut().unwrap().push(call("c5", "git status"));
        body["input"]
            .as_array_mut()
            .unwrap()
            .push(output("c5", "nothing to commit\n"));
        let seen = transform(&mut body, &mut session, None, &Options::default());
        assert_eq!(seen.largest_new_outputs.len(), 1);
        assert_eq!(
            (
                seen.largest_new_outputs[0].command.as_str(),
                seen.largest_new_outputs[0].shortened
            ),
            ("git status", false)
        );
    }

    #[test]
    fn the_task_is_the_request_after_the_context_codex_puts_in_front() {
        let task = |text: &str| {
            let seen = inspect(&json!({ "input": [{ "type": "message", "role": "user", "content": [
                { "type": "input_text", "text": text }] }] }));
            (seen.first_user_message, seen.first_user_text, seen.first_user_raw)
        };
        let ide = "# Context from my IDE setup:\n\n## Active file: src/lib.rs\n\n## Open tabs:\n- lib.rs: src/lib.rs\n\n## My request for Codex:\nfix the login bug";
        let (message, text, raw) = task(ide);
        assert_eq!(
            (message.as_str(), text.as_str()),
            ("fix the login bug", "fix the login bug")
        );
        assert!(raw.starts_with("# Context from my IDE setup:"), "{raw}");
        let files = "# Files mentioned by the user:\n\n## Screenshot.png: /var/folders/x/Screenshot.png\n\n## My request for Codex:\nwhy is the cart empty";
        assert_eq!(task(files).0, "why is the cart empty");
        let selection = "x".repeat(30_000);
        let big = format!("# Context from my IDE setup:\n\n## Active selection of the file:\n{selection}\n## My request for Codex:\nwhy does this fail");
        assert_eq!(task(&big).1, "why does this fail");
        // Not a context: the phrase inside a plain message changes nothing.
        let plain = "explain `## My request for Codex:` to me";
        assert_eq!(request_text(plain), plain);
        let inline = "# Notes\nsee ## My request for Codex: in the docs";
        assert_eq!(request_text(inline), inline);
        // No heading: the message as it is.
        let no_heading = "# Files mentioned by the user:\n\n## a.png: /tmp/a.png";
        assert_eq!(request_text(no_heading), no_heading);
    }

    #[test]
    fn the_labels_around_an_attached_image_are_not_part_of_the_task() {
        // Codex says the message is typed in its metadata; without it a message that opens
        // with a tag counts as not typed (unchanged: that would move old decisions).
        let user = |parts: Value| {
            json!({ "type": "message", "role": "user", "content": parts,
            "internal_chat_message_metadata_passthrough": { "content_item_kinds": ["user.text", "user.image", "user.text", "user.text"] } })
        };
        let seen = inspect(&json!({ "input": [user(json!([
            { "type": "input_text", "text": "<image name=[Image #1] path=\"/tmp/a.png\">" },
            { "type": "input_image", "image_url": "data:image/png;base64,AAAA" },
            { "type": "input_text", "text": "</image>" },
            { "type": "input_text", "text": "fix the cart" },
        ]))] }));
        assert_eq!(seen.first_user_message, "fix the cart");
        assert_eq!(seen.first_user_text, "fix the cart");
        // The ledger key's text keeps the labels: it is what the client sent.
        assert!(seen.first_user_raw.contains("<image name="), "{}", seen.first_user_raw);
        assert_eq!(request_text("<audio>\nlisten\n</audio>\nplay it"), "listen\nplay it");
        assert_eq!(request_text("use <image> tags in html"), "use <image> tags in html");
    }

    #[test]
    fn a_message_of_attachments_only_is_not_the_task() {
        let user = |text: &str| json!({ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": text }] });
        let seen = inspect(&json!({ "input": [
            user("# Files mentioned by the user:\n\n## a.png: /tmp/a.png\n\n## My request for Codex:\n"),
            user("why is the cart empty"),
        ] }));
        assert_eq!(seen.first_user_message, "why is the cart empty");
        assert!(seen.has_history, "two typed messages");
    }
    #[test]
    fn tools_called_from_an_exec_script_are_seen() {
        assert_eq!(
            nested_tool_names("await tools.mcp__cua_repl__js_reset({});\nconst t = await tools.clock__curr_time ();\nawait tools[\"my tool\"](1)"),
            ["mcp__cua_repl__js_reset", "clock__curr_time", "my tool"]
        );
        assert!(nested_tool_names("const mytools.x = 1; tools.exec_command").is_empty());
        let mut body = json!({ "input": [
            { "type": "custom_tool_call", "call_id": "s1", "name": "exec", "input": "await tools.mcp__cua_repl__js_reset({}); await tools.clock__curr_time();" },
            { "type": "custom_tool_call", "call_id": "s2", "name": "exec", "input": "await tools.clock__curr_time();" },
            { "type": "custom_tool_call", "call_id": "s3", "name": "other", "input": "tools.hidden()" },
        ] });
        let seen = transform(&mut body, &mut Session::default(), None, &Options::default());
        assert_eq!(seen.code_mode_calls, ["mcp__cua_repl__js_reset", "clock__curr_time"]);
    }
}
