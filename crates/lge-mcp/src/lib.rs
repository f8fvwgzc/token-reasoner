//! MCP stdio server: the code graph and the long-term memory as tools for a
//! coding agent.
//!
//! The transport is newline-delimited JSON-RPC 2.0 on stdin and stdout, written
//! by hand: a tool server needs four methods (`initialize`, `tools/list`,
//! `tools/call`, `ping`), and owning them keeps the binary free of an async
//! runtime and gives exact control over how many bytes each answer costs.
//! The tool logic is in [`Server::call`], apart from the transport.
//!
//! Every answer is compact text under a token budget and ends with the indexed
//! commit. The server process owns the database: before answering it re-indexes
//! a working tree that changed.

use anyhow::{anyhow, bail, Context, Result};
use lge_core::{content_hash, NodeLabel, NodeRec};
use lge_memory::{recall, refresh_pages, retain, search_pages, Outcome, RecallOptions, RetainInput};
use lge_retrieve::budget::{count, truncate_lines, DEFAULT_TOOL_BUDGET};
use lge_retrieve::{
    architecture, commit_line, dependencies, evidence_pack, impact, refresh_if_stale, render_deps, render_impact,
    render_trace, repo_map, resolve_targets, trace, unix_now, Freshness, IndexOptions, PackOptions, WalkOptions,
};
use lge_store::GraphStore;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

/// Told to the client at `initialize`. Short on purpose: it is part of every
/// session's prompt.
pub const INSTRUCTIONS: &str = "Code graph and project memory for this repository. \
Ask here before the first grep, directory listing or file read: `search_code_context` for a task or question, \
`analyze_change_impact` before editing a symbol or file, `trace_execution_flow` and `find_dependencies` for how code connects, \
`get_architecture_context` for services, APIs, queues and tables, `get_relevant_source` for exact lines. \
`recall_memory` returns decisions and conventions from earlier sessions; `retain_memory` stores one at the end of a task. \
Read raw files only for lines these tools located. Edges marked inferred or unresolved are not verified: check them.";

/// Protocol revisions this server speaks. The tool methods are the same in all.
const PROTOCOLS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const MAX_BUDGET: usize = 20_000;
/// Default for [`Server::fresh_for`].
const FRESH_FOR: Duration = Duration::from_millis(250);

pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
}

fn schema(properties: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": properties, "required": required, "additionalProperties": false })
}

/// The tools, in the order they are listed to the client. Descriptions are one
/// line each: they are sent with every session.
pub fn tools() -> Vec<ToolDef> {
    let budget = json!({ "type": "integer", "description": "Token budget for the answer" });
    vec![
        ToolDef {
            name: "search_code_context",
            description: "Ranked code context for a task or question: starting symbols with source, what they use, what uses them, tests.",
            input_schema: schema(
                json!({ "question": { "type": "string" }, "service": { "type": "string", "description": "Restrict to one service" }, "budget": budget }),
                &["question"],
            ),
        },
        ToolDef {
            name: "trace_execution_flow",
            description: "Call path from or to a symbol or endpoint, across services, with call sites.",
            input_schema: schema(
                json!({
                    "symbol": { "type": "string", "description": "Symbol name, qualified name, or endpoint like `POST /orders`" },
                    "direction": { "type": "string", "enum": ["downstream", "upstream"] },
                    "max_depth": { "type": "integer", "minimum": 1, "maximum": 6 },
                    "budget": budget
                }),
                &["symbol"],
            ),
        },
        ToolDef {
            name: "analyze_change_impact",
            description: "What is affected by changing a symbol or file: code, tests, APIs, services.",
            input_schema: schema(json!({ "target": { "type": "string", "description": "Symbol or file path" }, "budget": budget }), &["target"]),
        },
        ToolDef {
            name: "find_dependencies",
            description: "Direct upstream and downstream edges of a symbol, file, service, endpoint, table or queue, with provenance.",
            input_schema: schema(json!({ "target": { "type": "string" }, "budget": budget }), &["target"]),
        },
        ToolDef {
            name: "get_relevant_source",
            description: "Exact source of symbols, or of a file line range.",
            input_schema: schema(
                json!({
                    "symbols": { "type": "array", "items": { "type": "string" } },
                    "file": { "type": "string" },
                    "start_line": { "type": "integer", "minimum": 1 },
                    "end_line": { "type": "integer", "minimum": 1 },
                    "budget": budget
                }),
                &[],
            ),
        },
        ToolDef {
            name: "get_architecture_context",
            description: "Services and what connects them: dependencies, endpoints, queues, tables, key symbols.",
            input_schema: schema(json!({ "service": { "type": "string" }, "budget": budget }), &[]),
        },
        ToolDef {
            name: "recall_memory",
            description: "Decisions, conventions and facts from earlier sessions that match a query.",
            input_schema: schema(json!({ "query": { "type": "string" }, "budget": budget }), &["query"]),
        },
        ToolDef {
            name: "retain_memory",
            description: "Store one fact for later sessions (a decision, a convention, what a task found).",
            input_schema: schema(
                json!({
                    "text": { "type": "string", "description": "One statement" },
                    "kind": { "type": "string", "enum": ["world", "experience", "decision", "convention", "observation"] },
                    "key": { "type": "string", "description": "What it is about, e.g. `auth.token-expiry`; a newer fact with the same key replaces it" },
                    "tags": { "type": "array", "items": { "type": "string" } }
                }),
                &["text"],
            ),
        },
    ]
}

/// Answer one JSON-RPC message of the protocol. `call` runs a tool by name
/// and returns its text. `None` for a notification. The transport (stdio
/// lines, HTTP posts) is the caller's.
pub fn reply(message: &Value, call: &mut dyn FnMut(&str, &Value) -> Result<String>) -> Option<Value> {
    let id = message.get("id").cloned();
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let reply = |result: Value| {
        id.clone()
            .map(|id| json!({ "jsonrpc": "2.0", "id": id, "result": result }))
    };
    let fail = |code: i64, text: String| {
        Some(
            json!({ "jsonrpc": "2.0", "id": id.clone().unwrap_or(Value::Null), "error": { "code": code, "message": text } }),
        )
    };
    match method {
        "initialize" => {
            let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
            let version = if PROTOCOLS.contains(&asked) {
                asked
            } else {
                PROTOCOLS[1]
            };
            reply(json!({
                "protocolVersion": version,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "lge", "version": env!("CARGO_PKG_VERSION") },
                "instructions": INSTRUCTIONS,
            }))
        }
        "ping" => reply(json!({})),
        "tools/list" => {
            let list: Vec<Value> = tools()
                .into_iter()
                .map(|t| json!({ "name": t.name, "description": t.description, "inputSchema": t.input_schema }))
                .collect();
            reply(json!({ "tools": list }))
        }
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("").to_string();
            let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            let (text, is_error) = match call(&name, &args) {
                Ok(text) => (text, false),
                Err(e) => (format!("error: {e}\n"), true),
            };
            reply(json!({ "content": [{ "type": "text", "text": text }], "isError": is_error }))
        }
        "resources/list" => reply(json!({ "resources": [] })),
        "prompts/list" => reply(json!({ "prompts": [] })),
        // A notification gets no answer, whatever it is.
        _ if id.is_none() => None,
        "" => fail(-32600, "not a JSON-RPC request".to_string()),
        other => fail(-32601, format!("method `{other}` is not supported")),
    }
}

pub struct Server {
    store: Box<dyn GraphStore>,
    root: PathBuf,
    index: IndexOptions,
    fresh: Freshness,
    /// Symbol id -> hash of the source already returned in this session.
    sent: HashMap<String, String>,
    /// Where pages are mirrored as markdown; `None` keeps them in the database only.
    pub pages_dir: Option<PathBuf>,
    /// Re-index a changed tree before answering. Off for a read-only snapshot.
    pub keep_fresh: bool,
    /// A tree that was checked this recently is not checked again.
    pub fresh_for: Duration,
}

fn text_arg<'a>(args: &'a Value, name: &str) -> Result<&'a str> {
    args.get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("`{name}` is required"))
}

fn opt_text<'a>(args: &'a Value, name: &str) -> Option<&'a str> {
    args.get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn budget_arg(args: &Value, default: usize) -> usize {
    args.get("budget")
        .and_then(Value::as_u64)
        .map(|b| b as usize)
        .unwrap_or(default)
        .clamp(100, MAX_BUDGET)
}

fn label_of(n: &NodeRec) -> &str {
    if n.qualified_name.is_empty() {
        &n.name
    } else {
        &n.qualified_name
    }
}

impl Server {
    pub fn new(store: Box<dyn GraphStore>, root: &Path, index: IndexOptions) -> Server {
        Server {
            store,
            root: root.to_path_buf(),
            index,
            fresh: Freshness::default(),
            sent: HashMap::new(),
            pages_dir: None,
            keep_fresh: true,
            fresh_for: FRESH_FOR,
        }
    }

    /// The store this server answers from.
    pub fn store(&self) -> &dyn GraphStore {
        self.store.as_ref()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn commit(&self) -> String {
        self.store
            .repository()
            .ok()
            .flatten()
            .map(|(_, c)| c)
            .unwrap_or_default()
    }

    /// Bring the index up to date. A failure does not block the answer; it is
    /// reported in front of it, because the answer may then be stale.
    pub fn freshen(&mut self) -> Option<String> {
        if !self.keep_fresh {
            return None;
        }
        match refresh_if_stale(
            self.store.as_ref(),
            &self.root,
            &self.index,
            &mut self.fresh,
            self.fresh_for,
        ) {
            Ok(_) => None,
            Err(e) => Some(format!("warning: index not refreshed, answer may be stale: {e}\n")),
        }
    }

    /// Run one tool and return its text.
    pub fn call(&mut self, name: &str, args: &Value) -> Result<String> {
        let warning = if matches!(name, "recall_memory" | "retain_memory") {
            None
        } else {
            self.freshen()
        };
        let text = match name {
            "search_code_context" => self.search_code_context(args)?,
            "trace_execution_flow" => self.trace_execution_flow(args)?,
            "analyze_change_impact" => self.analyze_change_impact(args)?,
            "find_dependencies" => self.find_dependencies(args)?,
            "get_relevant_source" => self.get_relevant_source(args)?,
            "get_architecture_context" => self.get_architecture_context(args)?,
            "recall_memory" => self.recall_memory(args)?,
            "retain_memory" => self.retain_memory(args)?,
            other => bail!("unknown tool `{other}`"),
        };
        Ok(warning.unwrap_or_default() + &text)
    }

    fn search_code_context(&mut self, args: &Value) -> Result<String> {
        let question = text_arg(args, "question")?;
        let opts = PackOptions {
            budget: budget_arg(args, DEFAULT_TOOL_BUDGET),
            service: opt_text(args, "service").map(str::to_string),
            ..PackOptions::default()
        };
        let pack = evidence_pack(self.store.as_ref(), question, &opts)?;
        // The pack showed the seeds' source: remember it, so a later request
        // for the same symbol is answered with a pointer.
        for seed in &pack.seeds {
            if let Some(body) = self.store.symbol_body(&seed.id)? {
                if pack.text.contains(body.trim_end()) {
                    self.sent.insert(seed.id.clone(), content_hash(body.as_bytes()));
                }
            }
        }
        Ok(pack.text)
    }

    /// The nodes a tool argument names, or a short error that says how to be specific.
    fn targets(&self, text: &str) -> Result<Vec<NodeRec>> {
        let nodes = resolve_targets(self.store.as_ref(), text)?;
        if nodes.is_empty() {
            bail!("nothing in the graph is called `{text}`; use search_code_context to find the name");
        }
        Ok(nodes)
    }

    fn trace_execution_flow(&mut self, args: &Value) -> Result<String> {
        let symbol = text_arg(args, "symbol")?;
        let downstream = opt_text(args, "direction") != Some("upstream");
        let depth = args.get("max_depth").and_then(Value::as_u64).unwrap_or(4).clamp(1, 6) as u8;
        let budget = budget_arg(args, DEFAULT_TOOL_BUDGET);
        let nodes = self.targets(symbol)?;
        let starts: Vec<&NodeRec> = nodes
            .iter()
            .filter(|n| matches!(n.label, NodeLabel::Symbol | NodeLabel::ApiEndpoint | NodeLabel::Queue))
            .collect();
        if starts.len() > 1 {
            let names: Vec<String> = starts
                .iter()
                .take(12)
                .map(|n| format!("- {}  ({}:{})", label_of(n), n.file_path, n.start_line))
                .collect();
            return Ok(format!(
                "`{symbol}` names {} things. Ask again with one qualified name:\n{}\n",
                starts.len(),
                names.join("\n")
            ));
        }
        let Some(start) = starts.first() else {
            bail!("`{symbol}` is not a symbol, endpoint or queue")
        };
        let opts = WalkOptions {
            depth,
            ..WalkOptions::default()
        };
        let found = trace(self.store.as_ref(), &start.id, downstream, &opts)?
            .ok_or_else(|| anyhow!("`{symbol}` is no longer in the graph"))?;
        Ok(render_trace(&found, &self.commit(), budget))
    }

    fn analyze_change_impact(&mut self, args: &Value) -> Result<String> {
        let target = text_arg(args, "target")?;
        let ids: Vec<String> = self.targets(target)?.into_iter().map(|n| n.id).collect();
        let found = impact(self.store.as_ref(), &ids, &WalkOptions::default())?;
        Ok(render_impact(
            &found,
            &self.commit(),
            budget_arg(args, DEFAULT_TOOL_BUDGET),
        ))
    }

    fn find_dependencies(&mut self, args: &Value) -> Result<String> {
        let target = text_arg(args, "target")?;
        let nodes = self.targets(target)?;
        let budget = budget_arg(args, DEFAULT_TOOL_BUDGET);
        let commit = self.commit();
        let shown = nodes.len().min(3);
        let mut out = String::new();
        for node in nodes.iter().take(shown) {
            if let Some(deps) = dependencies(self.store.as_ref(), &node.id)? {
                out.push_str(&render_deps(&deps, &commit, budget / shown));
            }
        }
        if nodes.len() > shown {
            out.push_str(&format!(
                "`{target}` names {} things; the first {shown} are shown.\n",
                nodes.len()
            ));
        }
        Ok(out)
    }

    /// Lines `start..=end` of a file under the repository root.
    fn file_lines(&self, file: &str, start: usize, end: usize) -> Result<String> {
        let rel = Path::new(file.trim_start_matches("./"));
        if rel.is_absolute() || rel.components().any(|c| !matches!(c, Component::Normal(_))) {
            bail!("`{file}` must be a path inside the repository");
        }
        let text = std::fs::read_to_string(self.root.join(rel)).with_context(|| format!("cannot read {file}"))?;
        Ok(text
            .lines()
            .skip(start.saturating_sub(1))
            .take(end.saturating_sub(start) + 1)
            .collect::<Vec<_>>()
            .join("\n"))
    }

    fn get_relevant_source(&mut self, args: &Value) -> Result<String> {
        let budget = budget_arg(args, DEFAULT_TOOL_BUDGET);
        let commit = self.commit();
        let mut out = String::new();
        let mut left = budget.saturating_sub(count(&commit_line(&commit)));

        if let Some(file) = opt_text(args, "file") {
            let start = args.get("start_line").and_then(Value::as_u64).unwrap_or(1).max(1) as usize;
            let end = args
                .get("end_line")
                .and_then(Value::as_u64)
                .map(|e| e as usize)
                .unwrap_or(start + 80)
                .max(start);
            let (code, used) = truncate_lines(&self.file_lines(file, start, end)?, left.saturating_sub(12));
            out.push_str(&format!("{file}:{start}-{end}\n```\n{code}\n```\n"));
            left = left.saturating_sub(used + 12);
        }

        let names: Vec<&str> = args
            .get("symbols")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if names.is_empty() && out.is_empty() {
            bail!("give `symbols`, or `file` with `start_line` and `end_line`");
        }
        for name in names {
            let nodes: Vec<NodeRec> = self
                .targets(name)?
                .into_iter()
                .filter(|n| n.label == NodeLabel::Symbol)
                .collect();
            if nodes.is_empty() {
                out.push_str(&format!(
                    "`{name}` is not a symbol; give a file and line range instead.\n"
                ));
            }
            for n in nodes.iter().take(4) {
                let header = format!(
                    "{}:{}-{}  {} {}",
                    n.file_path, n.start_line, n.end_line, n.kind, n.qualified_name
                );
                // The stored body is capped; a longer symbol is read from the file.
                let stored = self.store.symbol_body(&n.id)?.unwrap_or_default();
                let lines = (n.end_line - n.start_line + 1).max(1) as usize;
                let body = if stored.lines().count() >= lines {
                    stored
                } else {
                    self.file_lines(&n.file_path, n.start_line as usize, n.end_line as usize)
                        .unwrap_or(stored)
                };
                let hash = content_hash(body.as_bytes());
                if self.sent.get(&n.id) == Some(&hash) {
                    out.push_str(&format!(
                        "{header}  (source already shown above in this session, unchanged)\n"
                    ));
                    continue;
                }
                let (code, used) = truncate_lines(body.trim_end(), left.saturating_sub(count(&header) + 12));
                if code.lines().count() <= 1 && body.lines().count() > 1 {
                    out.push_str(&format!("{header}  (does not fit the budget; ask with a larger one)\n"));
                    continue;
                }
                left = left.saturating_sub(used + count(&header) + 12);
                // Only a complete body counts as shown.
                if code == body.trim_end() {
                    self.sent.insert(n.id.clone(), hash);
                }
                out.push_str(&format!("{header}\n```\n{code}\n```\n"));
            }
            if nodes.len() > 4 {
                out.push_str(&format!(
                    "`{name}` names {} symbols; the first 4 are shown.\n",
                    nodes.len()
                ));
            }
        }
        out.push_str(&commit_line(&commit));
        Ok(out)
    }

    fn get_architecture_context(&mut self, args: &Value) -> Result<String> {
        let budget = budget_arg(args, DEFAULT_TOOL_BUDGET);
        let service = opt_text(args, "service");
        let (mut text, used) = truncate_lines(&architecture(self.store.as_ref(), service)?, budget);
        if !text.ends_with('\n') {
            text.push('\n');
        }
        // Room left: add the symbols most of the code depends on.
        if budget.saturating_sub(used) > 200 {
            if let Ok(map) = repo_map(self.store.as_ref(), service, budget - used) {
                text.push('\n');
                text.push_str(&map);
            }
        }
        Ok(text)
    }

    fn recall_memory(&mut self, args: &Value) -> Result<String> {
        let query = text_arg(args, "query")?;
        let budget = budget_arg(args, 800);
        let found = recall(
            self.store.as_ref(),
            query,
            &RecallOptions {
                budget,
                now: unix_now(),
                ..RecallOptions::default()
            },
            None,
        )?;
        let mut out = found.text;
        let mut left = budget.saturating_sub(found.tokens);
        for hit in search_pages(self.store.as_ref(), query, 2)? {
            let line = format!("page {}: {}\n", hit.slug, hit.snippet.replace('\n', " "));
            let cost = count(&line);
            if cost <= left {
                left -= cost;
                out.push_str(&line);
            }
        }
        if out.is_empty() {
            out.push_str("No stored fact or page matches.\n");
        } else if found.omitted > 0 {
            out.push_str(&format!(
                "{} more fact(s) not shown; ask with a budget of about {}.\n",
                found.omitted,
                budget * 2
            ));
        }
        Ok(out)
    }

    fn retain_memory(&mut self, args: &Value) -> Result<String> {
        let input = RetainInput {
            text: text_arg(args, "text")?.to_string(),
            kind: opt_text(args, "kind").unwrap_or_default().to_string(),
            key: opt_text(args, "key").unwrap_or_default().to_string(),
            tags: args
                .get("tags")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default(),
            source: "mcp".to_string(),
            valid_at: None,
        };
        let now = unix_now();
        let kept = retain(self.store.as_ref(), &input, now)?;
        refresh_pages(self.store.as_ref(), now)?;
        if let Some(dir) = &self.pages_dir {
            lge_memory::export_pages(self.store.as_ref(), dir)?;
        }
        let what = match &kept.outcome {
            Outcome::Stored => "stored".to_string(),
            Outcome::Repeated => "already known; confirmation counted".to_string(),
            Outcome::Superseded(old) => format!("stored; it replaces {} older fact(s) with the same key", old.len()),
        };
        Ok(format!(
            "{what}; linked to {} code entit{}.\n",
            kept.links.len(),
            if kept.links.len() == 1 { "y" } else { "ies" }
        ))
    }

    // ---- transport -------------------------------------------------------

    /// Answer one JSON-RPC message. `None` for a notification.
    pub fn handle(&mut self, message: &Value) -> Option<Value> {
        reply(message, &mut |name, args| self.call(name, args))
    }

    /// Serve until the input ends. One JSON message per line in, one per line out.
    pub fn serve(&mut self, input: impl BufRead, mut output: impl Write) -> Result<()> {
        for line in input.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let answer = match serde_json::from_str::<Value>(&line) {
                Ok(message) => self.handle(&message),
                Err(e) => Some(
                    json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": format!("invalid JSON: {e}") } }),
                ),
            };
            if let Some(answer) = answer {
                // serde_json never writes a raw newline inside a value.
                writeln!(output, "{answer}")?;
                output.flush()?;
            }
        }
        Ok(())
    }
}
