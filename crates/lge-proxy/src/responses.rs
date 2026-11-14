//! Rewriting one OpenAI Responses request body.
//!
//! The body's `input` is the whole conversation as a list of items. This
//! module finds the two kinds of item the proxy acts on, the output of a shell
//! command and a message the user typed, and leaves every other item, and
//! every other field of the body, exactly as it was. Items that carry ids or
//! encrypted reasoning are never opened.

use crate::session::{Graph, Options, Session};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;

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
    /// Tool outputs replaced in this request (old and new decisions alike).
    pub outputs_replaced: usize,
    /// Context messages present in this request.
    pub contexts_present: usize,
}

/// A shell command a tool call ran, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Ran {
    command: String,
    workdir: Option<String>,
}

/// An output smaller than this is cheaper to repeat than to point at.
const REPEAT_MIN_BYTES: usize = 2000;

const SHELLS: [&str; 6] = ["bash", "zsh", "sh", "/bin/bash", "/bin/zsh", "/bin/sh"];

/// The command line of a shell-type tool call, from its arguments object.
/// Understands `{"command": ["bash", "-lc", "..."]}`, `{"command": "..."}`
/// and `{"cmd": "..."}`.
fn command_of(args: &Value) -> Option<Ran> {
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

/// Wrappers the agent puts around context it adds as user messages.
const INJECTED_STARTS: [&str; 8] = [
    "# agents.md instructions",
    "<environment_context>",
    "<turn_aborted>",
    "<user_shell_command>",
    "<subagent_notification>",
    "<codex_internal_context",
    "<external_",
    "<lge_context>",
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

fn tag<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&format!("</{name}>"))? + start;
    Some(text[start..end].trim())
}

/// Splits a tool output string into what frames it and the command's own
/// text. Handles the plain layout (`Exit code: ..` lines, then `Output:`) and
/// the JSON layout (`{"output": "...", "metadata": {...}}`).
enum Framed {
    /// `prefix` + text.
    Plain { prefix: String, text: String },
    /// A JSON object whose `output` field is the text.
    Json { object: Value, text: String },
}

fn unframe(raw: &str) -> Framed {
    if raw.trim_start().starts_with('{') {
        if let Ok(object) = serde_json::from_str::<Value>(raw) {
            if let Some(text) = object.get("output").and_then(Value::as_str) {
                let text = text.to_string();
                return Framed::Json { object, text };
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
            };
        }
    }
    Framed::Plain {
        prefix: String::new(),
        text: raw.to_string(),
    }
}

fn reframe(framed: Framed, new_text: String) -> String {
    match framed {
        Framed::Plain { prefix, .. } => prefix + &new_text,
        Framed::Json { mut object, .. } => {
            object["output"] = Value::String(new_text);
            object.to_string()
        }
    }
}

/// The extra item that carries graph context, right after the user's message.
fn context_item(text: &str) -> Value {
    json!({ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": text }] })
}

/// Shorten the texts of an output whose command is not known, each judged by
/// its content. Returns how many were replaced. The JSON type of `output` and
/// the number of its parts never change.
fn rewrite_by_content(item: &mut Value, session: &mut Session, opts: &Options) -> usize {
    let mut replaced = 0;
    match item.get_mut("output") {
        Some(Value::String(raw)) => {
            let framed = unframe(raw);
            let text = match &framed {
                Framed::Plain { text, .. } | Framed::Json { text, .. } => text.clone(),
            };
            if let Some(shorter) = session.rewrite_text(&text, opts) {
                *raw = reframe(framed, shorter);
                replaced += 1;
            }
        }
        Some(Value::Array(parts)) => {
            for part in parts.iter_mut() {
                if part.get("type").and_then(Value::as_str) != Some("input_text") {
                    continue;
                }
                let Some(Value::String(text)) = part.get_mut("text") else {
                    continue;
                };
                if let Some(shorter) = session.rewrite_text(text, opts) {
                    *text = shorter;
                    replaced += 1;
                }
            }
        }
        _ => {}
    }
    replaced
}

/// Read what the request says about itself, without changing it.
pub fn inspect(body: &Value) -> Seen {
    let mut seen = Seen {
        conversation: body.get("prompt_cache_key").and_then(Value::as_str).map(str::to_string),
        model: body.get("model").and_then(Value::as_str).map(str::to_string),
        ..Seen::default()
    };
    for item in body.get("input").and_then(Value::as_array).into_iter().flatten() {
        if item.get("type").and_then(Value::as_str) != Some("message")
            || item.get("role").and_then(Value::as_str) != Some("user")
        {
            continue;
        }
        let Some(text) = message_text(item) else { continue };
        if seen.cwd.is_none() {
            seen.cwd = tag(&text, "cwd").map(str::to_string);
        }
        if seen.first_user_message.is_empty() && !is_injected(item, &text) {
            seen.first_user_message = text.trim().chars().take(200).collect();
        }
    }
    seen
}

/// Rewrite `body` in place. Only `input` is touched, and in it only shell
/// outputs (replaced) and typed user messages (a context item is added after).
/// A body that is not a Responses request is left alone.
pub fn transform(body: &mut Value, session: &mut Session, graph: Option<&Graph>, opts: &Options) -> Seen {
    let mut seen = inspect(body);
    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return seen;
    };
    let default_dir = seen.cwd.clone();

    // Which command produced which output.
    let mut ran: HashMap<String, Ran> = HashMap::new();
    for item in items.iter() {
        let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
        let Some(call_id) = item.get("call_id").and_then(Value::as_str) else {
            continue;
        };
        let found = match kind {
            "function_call" => item
                .get("arguments")
                .and_then(Value::as_str)
                .and_then(|a| serde_json::from_str::<Value>(a).ok())
                .and_then(|a| command_of(&a)),
            "local_shell_call" => item.get("action").and_then(command_of),
            _ => None,
        };
        if let Some(r) = found {
            ran.insert(call_id.to_string(), r);
        }
    }

    let mut out: Vec<Value> = Vec::with_capacity(items.len() + 2);
    // Text of large outputs sent in full so far in this request -> the command that produced it.
    let mut sent_in_full: HashMap<String, String> = HashMap::new();
    for mut item in std::mem::take(items) {
        let kind = item.get("type").and_then(Value::as_str).unwrap_or("").to_string();
        match kind.as_str() {
            "function_call_output" | "custom_tool_call_output" | "local_shell_call_output"
                if !item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| ran.contains_key(id))
                    || !item.get("output").is_some_and(Value::is_string) =>
            {
                // No command to go by (a script, another tool), or the output is a
                // list of parts: each text is judged by its content alone.
                seen.outputs_replaced += rewrite_by_content(&mut item, session, opts);
                out.push(item);
            }
            "function_call_output" | "local_shell_call_output" => {
                let call = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .and_then(|id| ran.get(id))
                    .cloned();
                if let (Some(call), Some(Value::String(raw))) = (call, item.get("output")) {
                    let framed = unframe(raw);
                    let text = match &framed {
                        Framed::Plain { text, .. } | Framed::Json { text, .. } => text.clone(),
                    };
                    let dir: Option<PathBuf> = call.workdir.clone().or_else(|| default_dir.clone()).map(PathBuf::from);
                    if let Some(shorter) = session.rewrite_output(graph, &call.command, dir.as_deref(), &text, opts) {
                        item["output"] = Value::String(reframe(framed, shorter));
                        seen.outputs_replaced += 1;
                    } else if opts.shorten_outputs && text.len() >= REPEAT_MIN_BYTES {
                        // The same large output again (a file read twice, a command
                        // rerun with nothing changed): point to the first copy, which
                        // is still in this request. Decided from the request alone, so
                        // it is the same on every turn and right after a compaction.
                        let hash = lge_core::content_hash(text.as_bytes());
                        match sent_in_full.get(&hash) {
                            Some(earlier) => {
                                let first: String = earlier.chars().take(120).collect();
                                let pointer = format!(
                                    "lge: identical to the output of `{first}` earlier in this conversation ({} lines, unchanged); it is not repeated here.
",
                                    text.lines().count()
                                );
                                let id = item.get("call_id").and_then(Value::as_str).unwrap_or(&hash).to_string();
                                session.count_pointer(&id, &text, &pointer);
                                item["output"] = Value::String(reframe(framed, pointer));
                                seen.outputs_replaced += 1;
                            }
                            None => {
                                sent_in_full.insert(hash, call.command.clone());
                            }
                        }
                    }
                }
                out.push(item);
            }
            "message" if item.get("role").and_then(Value::as_str) == Some("user") => {
                let text = message_text(&item);
                let typed = text.as_deref().is_some_and(|t| !is_injected(&item, t));
                out.push(item);
                let (Some(text), Some(graph), true) = (text, graph, typed) else {
                    continue;
                };
                if let Some(context) = session.context_for(graph, &text, opts) {
                    out.push(context_item(&context));
                    seen.contexts_present += 1;
                }
            }
            _ => out.push(item),
        }
    }
    *items = out;
    seen
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

    fn user(text: &str) -> Value {
        json!({ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": text }] })
    }

    fn call(id: &str, command: &str) -> Value {
        json!({ "type": "function_call", "name": "exec_command", "call_id": id, "arguments": json!({ "cmd": command, "workdir": "/repo", "yield_time_ms": 10000 }).to_string() })
    }

    fn output(id: &str, text: &str) -> Value {
        json!({ "type": "function_call_output", "call_id": id, "output": text })
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
                output("c2", &run),
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
        }
        for field in ["model", "prompt_cache_key", "instructions", "tools", "stream"] {
            assert_eq!(body[field], before[field], "{field}");
        }

        // The next turn resends the conversation: the same item gets the same bytes.
        let mut again = before.clone();
        again["input"].as_array_mut().unwrap().push(user("thanks"));
        transform(&mut again, &mut session, None, &Options::default());
        assert_eq!(again["input"][4], body["input"][4]);
        assert_eq!(session.saved.outputs_shortened, 1, "decided once");
    }

    #[test]
    fn a_repeated_output_points_to_its_first_copy_while_that_copy_is_there() {
        let file: String = (0..120)
            .map(|i| format!("line {i}: some source text that is long enough to matter\n"))
            .collect();
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
            "lge: identical to the output of `cat src/app.py` earlier in this conversation (120 lines, unchanged); it is not repeated here.\n"
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
    fn a_json_framed_output_keeps_its_frame() {
        let run = passing_run();
        let framed = json!({ "output": run, "metadata": { "exit_code": 0, "duration_seconds": 9.9 } }).to_string();
        let mut body = json!({ "input": [call("c1", "pytest -v"), output("c1", &framed)] });
        transform(&mut body, &mut Session::default(), None, &Options::default());
        let new: Value = serde_json::from_str(body["input"][1]["output"].as_str().unwrap()).unwrap();
        assert!(new["output"].as_str().unwrap().starts_with("ok"), "{new}");
        assert_eq!(new["metadata"]["exit_code"], 0);
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
    fn an_output_without_a_known_command_is_judged_by_its_content() {
        // A script run (code mode): the output is a list of parts, the command is inside the script.
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
            { "type": "custom_tool_call", "call_id": "s1", "name": "exec", "input": "const r = await tools.exec_command({cmd: 'pytest -v'}); text(r.output);" },
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
        assert_eq!(
            seen.outputs_replaced,
            3,
            "{}",
            serde_json::to_string(&json!([parts[1], parts[3], body["input"][2]]))
                .unwrap()
                .chars()
                .take(900)
                .collect::<String>()
        );
        assert_eq!(parts.len(), 4, "the number of parts never changes");
        assert_eq!(parts[0], before["input"][1]["output"][0]);
        assert!(parts[1]["text"].as_str().unwrap().starts_with("ok"), "{}", parts[1]);
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
        assert_eq!(
            session.saved.outputs_shortened, 2,
            "the same text twice is decided and counted once"
        );
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
}
