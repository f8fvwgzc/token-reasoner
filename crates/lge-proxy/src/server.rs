//! The daemon: one local HTTP server that is the proxy in front of the model
//! provider, the owner of every graph database, the MCP endpoint, and the page
//! that shows what was spent and saved.
//!
//! Rules of the proxy path:
//! * Fail open. If anything about rewriting a request goes wrong, the original
//!   bytes are forwarded.
//! * The response is relayed as it arrives, byte for byte; the proxy only reads
//!   the token usage out of it.
//! * Nothing of a conversation is stored except token counts, the first typed
//!   message cut to 200 characters, and the decisions needed to repeat a rewrite.

use crate::responses::{inspect, transform, Usage, UsageScanner};
use crate::session::{Graph, Options, Session};
use anyhow::{anyhow, bail, Context, Result};
use lge_core::{outcome, stable_id, UsageRecord};
use lge_retrieve::{index_repo, unix_now, IndexOptions};
use lge_store::{GraphStore, Store};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tiny_http::{Header, Method, Request, Response};

const UI: &str = include_str!("ui.html");
/// Largest request body the proxy reads into memory.
const MAX_BODY: usize = 128 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    /// 0 lets the system choose (tests).
    pub port: u16,
    /// Where graph databases, the usage log and the daemon's settings live.
    pub data_dir: PathBuf,
    /// Folders the daemon may index. Empty = any folder.
    pub workspaces: Vec<PathBuf>,
    /// Provider base for API-key requests.
    pub upstream_api: String,
    /// Provider base for requests made with a ChatGPT login.
    pub upstream_chatgpt: String,
    /// The agent's home (`~/.codex`), for importing the usage of past sessions.
    pub codex_home: Option<PathBuf>,
}

impl Config {
    pub fn new(data_dir: PathBuf) -> Config {
        Config {
            host: "127.0.0.1".to_string(),
            port: 8787,
            data_dir,
            workspaces: Vec::new(),
            upstream_api: "https://api.openai.com/v1".to_string(),
            upstream_chatgpt: "https://chatgpt.com/backend-api/codex".to_string(),
            codex_home: None,
        }
    }
}

/// What the page can switch. Saved in `daemon.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub inject_context: bool,
    pub shorten_outputs: bool,
    pub context_budget: usize,
    /// Index a git repository the first time the agent works in it.
    pub auto_index: bool,
}

impl Default for Settings {
    fn default() -> Self {
        let o = Options::default();
        Settings {
            inject_context: o.inject_context,
            shorten_outputs: o.shorten_outputs,
            context_budget: o.context_budget,
            auto_index: true,
        }
    }
}

impl Settings {
    fn options(&self) -> Options {
        Options {
            inject_context: self.inject_context,
            shorten_outputs: self.shorten_outputs,
            context_budget: self.context_budget,
            ..Options::default()
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct SavedState {
    #[serde(default)]
    repos: Vec<String>,
    #[serde(default)]
    settings: Option<Settings>,
}

enum RepoState {
    Indexing,
    Ready(Box<lge_mcp::Server>),
    Failed(String),
}

struct Repo {
    root: PathBuf,
    state: Mutex<RepoState>,
}

/// One conversation as the proxy sees it.
struct Conv {
    session: Session,
    record: UsageRecord,
    repo: Option<PathBuf>,
    requests: u64,
    routed: bool,
}

pub struct Daemon {
    pub cfg: Config,
    agent: ureq::Agent,
    usage: Store,
    repos: Mutex<BTreeMap<PathBuf, Arc<Repo>>>,
    convs: Mutex<HashMap<String, Arc<Mutex<Conv>>>>,
    settings: Mutex<Settings>,
    last_repo: Mutex<Option<PathBuf>>,
    started: Instant,
}

/// A poisoned lock means another request panicked; its data is still the best there is.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn header(name: &str, value: &str) -> Option<Header> {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).ok()
}

fn respond_json(request: Request, status: u16, body: &Value) {
    let mut response = Response::from_string(body.to_string()).with_status_code(status);
    if let Some(h) = header("Content-Type", "application/json") {
        response = response.with_header(h);
    }
    let _ = request.respond(response);
}

/// Headers that belong to one hop and are never forwarded.
fn hop_by_hop(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "host"
            | "content-length"
            | "expect"
    )
}

/// `/v1/responses?x=1` -> (`/v1/responses`, `x=1`).
fn split_url(url: &str) -> (&str, &str) {
    url.split_once('?').unwrap_or((url, ""))
}

fn query_value(query: &str, key: &str) -> Option<String> {
    query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| percent_decode(v))
}

fn percent_decode(s: &str) -> String {
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match (
            bytes[i],
            bytes.get(i + 1).copied().and_then(hex),
            bytes.get(i + 2).copied().and_then(hex),
        ) {
            (b'%', Some(high), Some(low)) => {
                out.push(high * 16 + low);
                i += 3;
            }
            (b'+', _, _) => {
                out.push(b' ');
                i += 1;
            }
            (b, _, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// 0 = the repository is one service; 1 = each top-level folder that has its
/// own manifest is a service (a monorepo).
fn guess_service_depth(root: &Path) -> usize {
    const MANIFESTS: [&str; 6] = [
        "package.json",
        "pyproject.toml",
        "requirements.txt",
        "setup.py",
        "Cargo.toml",
        "go.mod",
    ];
    let with_manifest = std::fs::read_dir(root)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().is_dir() && MANIFESTS.iter().any(|m| e.path().join(m).exists()))
                .count()
        })
        .unwrap_or(0);
    usize::from(with_manifest >= 2)
}

impl Daemon {
    /// Open the data directory, the usage log and the saved repositories.
    pub fn open(cfg: Config) -> Result<Arc<Daemon>> {
        std::fs::create_dir_all(&cfg.data_dir).with_context(|| format!("cannot create {}", cfg.data_dir.display()))?;
        let usage = Store::open(&cfg.data_dir.join("usage.lbug"))
            .context("cannot open the usage log (is another lge daemon running?)")?;
        let saved: SavedState = std::fs::read_to_string(cfg.data_dir.join("daemon.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .http_status_as_error(false)
                .max_redirects(0)
                .timeout_connect(Some(Duration::from_secs(20)))
                .build(),
        );
        let daemon = Arc::new(Daemon {
            cfg,
            agent,
            usage,
            repos: Mutex::new(BTreeMap::new()),
            convs: Mutex::new(HashMap::new()),
            settings: Mutex::new(saved.settings.unwrap_or_default()),
            last_repo: Mutex::new(None),
            started: Instant::now(),
        });
        for path in saved.repos {
            // A folder that went away is dropped from the list, not an error.
            let _ = daemon.add_repo(Path::new(&path));
        }
        Ok(daemon)
    }

    fn save(&self) {
        let state = SavedState {
            repos: lock(&self.repos)
                .keys()
                .map(|p| p.to_string_lossy().to_string())
                .collect(),
            settings: Some(lock(&self.settings).clone()),
        };
        if let Ok(text) = serde_json::to_string_pretty(&state) {
            let _ = std::fs::write(self.cfg.data_dir.join("daemon.json"), text);
        }
    }

    fn allowed(&self, path: &Path) -> bool {
        self.cfg.workspaces.is_empty() || self.cfg.workspaces.iter().any(|w| path.starts_with(w))
    }

    /// Register a folder and index it in the background.
    pub fn add_repo(self: &Arc<Self>, path: &Path) -> Result<PathBuf> {
        let root = path
            .canonicalize()
            .with_context(|| format!("{} does not exist", path.display()))?;
        if !root.is_dir() {
            bail!("{} is not a folder", root.display());
        }
        if !self.allowed(&root) {
            bail!("{} is outside the folders this daemon may index", root.display());
        }
        let repo = Arc::new(Repo {
            root: root.clone(),
            state: Mutex::new(RepoState::Indexing),
        });
        if lock(&self.repos).insert(root.clone(), repo.clone()).is_some() {
            // Already known: index again with the existing entry replaced.
        }
        self.save();
        let daemon = self.clone();
        std::thread::spawn(move || {
            let state = match daemon.open_repo(&repo.root) {
                Ok(server) => RepoState::Ready(Box::new(server)),
                Err(e) => RepoState::Failed(format!("{e:#}")),
            };
            *lock(&repo.state) = state;
        });
        Ok(root)
    }

    pub fn remove_repo(&self, path: &Path) -> bool {
        let removed = lock(&self.repos).remove(path).is_some();
        self.save();
        removed
    }

    fn graph_dir(&self, root: &Path) -> PathBuf {
        let name = root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "root".to_string());
        let safe: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        self.cfg
            .data_dir
            .join("graphs")
            .join(format!("{safe}-{}", &stable_id(&[&root.to_string_lossy()])[..8]))
    }

    fn open_repo(&self, root: &Path) -> Result<lge_mcp::Server> {
        let dir = self.graph_dir(root);
        std::fs::create_dir_all(&dir)?;
        let store = Store::open(&dir.join("graph.lbug"))?;
        let index = IndexOptions {
            service_depth: guess_service_depth(root),
            ..IndexOptions::default()
        };
        index_repo(&store, root, &index)?;
        let _ = lge_memory::export_pages(&store, &dir.join("pages"));
        let mut server = lge_mcp::Server::new(Box::new(store), root, index);
        server.pages_dir = Some(dir.join("pages"));
        Ok(server)
    }

    /// The registered repository that holds `cwd`. With auto-indexing on, a
    /// git repository seen for the first time is registered and `None` is
    /// returned until its index is ready.
    fn repo_for(self: &Arc<Self>, cwd: &Path) -> Option<Arc<Repo>> {
        let found = lock(&self.repos)
            .iter()
            .filter(|(root, _)| cwd.starts_with(root))
            .max_by_key(|(root, _)| root.as_os_str().len())
            .map(|(_, r)| r.clone());
        if found.is_some() {
            return found;
        }
        if lock(&self.settings).auto_index {
            let mut dir = Some(cwd);
            while let Some(d) = dir {
                if d.join(".git").exists() {
                    if self.allowed(d) {
                        let _ = self.add_repo(d);
                    }
                    break;
                }
                dir = d.parent();
            }
        }
        None
    }

    // ---- the proxy path ---------------------------------------------------

    /// Rewrite a Responses request body. Returns the new bytes and the
    /// conversation they belong to, or `None` to forward the original.
    fn rewrite(self: &Arc<Self>, body: &[u8]) -> Option<(Vec<u8>, Arc<Mutex<Conv>>)> {
        let mut value: Value = serde_json::from_slice(body).ok()?;
        let seen = inspect(&value);
        let key = seen.conversation.clone().unwrap_or_else(|| {
            stable_id(&[
                "conversation",
                &seen.first_user_message,
                seen.cwd.as_deref().unwrap_or(""),
            ])
        });
        let conv = lock(&self.convs)
            .entry(key.clone())
            .or_insert_with(|| {
                let now = unix_now();
                let record = UsageRecord {
                    id: stable_id(&["session", &key]),
                    task_id: key.clone(),
                    task: seen.first_user_message.clone(),
                    attempt: 1,
                    level: "top".to_string(),
                    runner: "codex".to_string(),
                    session_id: key.clone(),
                    started_at: now,
                    finished_at: now,
                    outcome: outcome::UNKNOWN.to_string(),
                    mode: "proxy".to_string(),
                    ..UsageRecord::default()
                };
                Arc::new(Mutex::new(Conv {
                    session: Session::default(),
                    record,
                    repo: None,
                    requests: 0,
                    routed: false,
                }))
            })
            .clone();
        let options = lock(&self.settings).options();
        let repo = seen.cwd.as_deref().and_then(|cwd| self.repo_for(Path::new(cwd)));
        {
            let mut c = lock(&conv);
            c.requests += 1;
            if c.record.task.is_empty() {
                c.record.task = seen.first_user_message.clone();
            }
            if let Some(model) = &seen.model {
                c.record.model = model.clone();
            }
            match &repo {
                Some(repo) => {
                    *lock(&self.last_repo) = Some(repo.root.clone());
                    c.repo = Some(repo.root.clone());
                    let mut state = lock(&repo.state);
                    match &mut *state {
                        RepoState::Ready(server) => {
                            let _ = server.freshen();
                            let graph = Graph {
                                store: server.store(),
                                root: server.root(),
                            };
                            if !c.routed && !seen.first_user_message.is_empty() {
                                c.routed = true;
                                if let Ok(route) = lge_router::route(
                                    graph.store,
                                    &seen.first_user_message,
                                    &lge_router::RouterConfig::default(),
                                ) {
                                    c.record.recommended_level = route.level.as_str().to_string();
                                    c.record.reason = route.reason;
                                    c.record.files = route.files;
                                    c.record.services = route.services;
                                }
                            }
                            transform(&mut value, &mut c.session, Some(&graph), &options);
                        }
                        _ => {
                            transform(&mut value, &mut c.session, None, &options);
                        }
                    }
                }
                None => {
                    transform(&mut value, &mut c.session, None, &options);
                }
            }
            c.record.pack_tokens = c.session.saved.context_tokens as i64;
        }
        Some((serde_json::to_vec(&value).ok()?, conv))
    }

    fn record_usage(&self, conv: &Arc<Mutex<Conv>>, usage: Usage) {
        let record = {
            let mut c = lock(conv);
            c.record.input_tokens += usage.input_tokens;
            c.record.cached_input_tokens += usage.cached_input_tokens;
            c.record.output_tokens += usage.output_tokens;
            c.record.reasoning_tokens += usage.reasoning_tokens;
            c.record.finished_at = unix_now();
            c.record.clone()
        };
        let _ = self.usage.put_usage(&record);
    }

    fn upstream_for(&self, headers: &[(String, String)]) -> &str {
        // A ChatGPT login names its account; an API key does not.
        if headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("chatgpt-account-id"))
        {
            &self.cfg.upstream_chatgpt
        } else {
            &self.cfg.upstream_api
        }
    }

    fn proxy(self: &Arc<Self>, mut request: Request) {
        let method = request.method().as_str().to_string();
        let url = request.url().to_string();
        let (path, query) = split_url(&url);
        let mut headers: Vec<(String, String)> = request
            .headers()
            .iter()
            .map(|h| (h.field.as_str().as_str().to_string(), h.value.as_str().to_string()))
            .collect();
        let mut body = Vec::new();
        if request
            .as_reader()
            .take(MAX_BODY as u64)
            .read_to_end(&mut body)
            .is_err()
        {
            return respond_json(
                request,
                400,
                &json!({ "error": { "message": "lge proxy: the request body could not be read" } }),
            );
        }

        // Rewrite a Responses request; anything else, and any failure, passes as it came.
        let mut conv = None;
        if method == "POST" && path.trim_end_matches('/').ends_with("/responses") {
            let encoding = headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("content-encoding"))
                .map(|(_, v)| v.to_ascii_lowercase());
            let plain: Option<Vec<u8>> = match encoding.as_deref() {
                None | Some("identity") => Some(body.clone()),
                Some("zstd") => ruzstd::decoding::StreamingDecoder::new(&body[..])
                    .ok()
                    .and_then(|mut d| {
                        let mut out = Vec::new();
                        d.read_to_end(&mut out).ok().map(|_| out)
                    }),
                Some(_) => None,
            };
            let daemon = self.clone();
            let rewritten = plain.and_then(|plain| {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| daemon.rewrite(&plain)))
                    .ok()
                    .flatten()
            });
            if let Some((new_body, c)) = rewritten {
                body = new_body;
                headers.retain(|(k, _)| !k.eq_ignore_ascii_case("content-encoding"));
                conv = Some(c);
            }
        }

        let base = self.upstream_for(&headers).trim_end_matches('/').to_string();
        let tail = path
            .strip_prefix("/v1")
            .filter(|rest| rest.is_empty() || rest.starts_with('/'))
            .unwrap_or(path);
        let target = if query.is_empty() {
            format!("{base}{tail}")
        } else {
            format!("{base}{tail}?{query}")
        };
        let mut builder = ureq::http::Request::builder().method(method.as_str()).uri(&target);
        for (k, v) in &headers {
            if hop_by_hop(k) || k.eq_ignore_ascii_case("accept-encoding") {
                continue;
            }
            builder = builder.header(k.as_str(), v.as_str());
        }
        // Plain bytes back, so the usage can be read from the stream.
        builder = builder.header("accept-encoding", "identity");
        let sent = if body.is_empty() && method != "POST" {
            builder
                .body(())
                .map_err(|e| anyhow!("{e}"))
                .and_then(|r| self.agent.run(r).map_err(|e| anyhow!("{e}")))
        } else {
            builder
                .body(body)
                .map_err(|e| anyhow!("{e}"))
                .and_then(|r| self.agent.run(r).map_err(|e| anyhow!("{e}")))
        };
        let response = match sent {
            Ok(r) => r,
            Err(e) => {
                return respond_json(
                    request,
                    502,
                    &json!({ "error": { "message": format!("lge proxy: the provider could not be reached: {e}"), "type": "proxy_error" } }),
                );
            }
        };

        // Relay the status, the headers and the body as they arrive.
        let status = response.status();
        let mut head = format!(
            "HTTP/1.1 {} {}\r\n",
            status.as_u16(),
            status.canonical_reason().unwrap_or("")
        );
        let mut event_stream = false;
        for (name, value) in response.headers() {
            if hop_by_hop(name.as_str()) {
                continue;
            }
            let Ok(value) = value.to_str() else { continue };
            if name.as_str().eq_ignore_ascii_case("content-type") && value.contains("text/event-stream") {
                event_stream = true;
            }
            head.push_str(&format!("{}: {value}\r\n", name.as_str()));
        }
        head.push_str("Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n");
        let mut reader = response.into_body().into_reader();
        let mut writer = request.into_writer();
        let mut scanner = UsageScanner::default();
        let mut whole = Vec::new();
        let relay = (|| -> std::io::Result<()> {
            writer.write_all(head.as_bytes())?;
            let mut buf = vec![0u8; 16 * 1024];
            loop {
                let n = reader.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                if event_stream {
                    scanner.feed(&buf[..n]);
                } else if whole.len() < 8 * 1024 * 1024 {
                    whole.extend_from_slice(&buf[..n]);
                }
                write!(writer, "{n:x}\r\n")?;
                writer.write_all(&buf[..n])?;
                writer.write_all(b"\r\n")?;
                writer.flush()?;
            }
            writer.write_all(b"0\r\n\r\n")?;
            writer.flush()
        })();
        drop(writer);
        // A stream the client cut still used tokens: record what was seen.
        let _ = relay;
        let usage = scanner.usage.or_else(|| {
            // A response that was not streamed carries its usage in the body.
            let v: Value = serde_json::from_slice(&whole).ok()?;
            crate::responses::usage_of_event(&json!({ "type": "response.completed", "response": v }).to_string())
        });
        if let (Some(conv), Some(usage)) = (conv, usage) {
            self.record_usage(&conv, usage);
        }
    }

    // ---- MCP over HTTP ----------------------------------------------------

    /// The repository an MCP call is about: named in the query, or the one the
    /// agent last worked in.
    fn mcp_repo(&self, query: &str) -> Option<Arc<Repo>> {
        let named = query_value(query, "repo").map(PathBuf::from);
        let root = named.or_else(|| lock(&self.last_repo).clone()).or_else(|| {
            let repos = lock(&self.repos);
            (repos.len() == 1).then(|| repos.keys().next().cloned()).flatten()
        })?;
        lock(&self.repos).get(&root).cloned()
    }

    fn mcp(&self, mut request: Request) {
        if *request.method() != Method::Post {
            // No server-initiated stream is offered.
            let _ = request.respond(Response::empty(405));
            return;
        }
        let (_, query) = split_url(request.url());
        let repo = self.mcp_repo(query);
        let mut text = String::new();
        if request
            .as_reader()
            .take(8 * 1024 * 1024)
            .read_to_string(&mut text)
            .is_err()
        {
            return respond_json(request, 400, &json!({ "error": "unreadable body" }));
        }
        let Ok(message) = serde_json::from_str::<Value>(&text) else {
            return respond_json(
                request,
                400,
                &json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": "invalid JSON" } }),
            );
        };
        let mut call = |name: &str, args: &Value| -> Result<String> {
            let Some(repo) = &repo else {
                bail!("no indexed repository yet; add a folder on the lge page first")
            };
            let mut state = lock(&repo.state);
            match &mut *state {
                RepoState::Ready(server) => server.call(name, args),
                RepoState::Indexing => bail!("{} is still being indexed; try again in a moment", repo.root.display()),
                RepoState::Failed(e) => bail!("{} could not be indexed: {e}", repo.root.display()),
            }
        };
        let answer = match &message {
            Value::Array(batch) => {
                let replies: Vec<Value> = batch.iter().filter_map(|m| lge_mcp::reply(m, &mut call)).collect();
                (!replies.is_empty()).then_some(Value::Array(replies))
            }
            single => lge_mcp::reply(single, &mut call),
        };
        match answer {
            Some(v) => respond_json(request, 200, &v),
            None => {
                let _ = request.respond(Response::empty(202));
            }
        }
    }

    // ---- the page and its API ----------------------------------------------

    fn repo_json(&self, repo: &Repo) -> Value {
        let state = lock(&repo.state);
        let (word, detail, stats) = match &*state {
            RepoState::Indexing => ("indexing", String::new(), Vec::new()),
            RepoState::Failed(e) => ("failed", e.clone(), Vec::new()),
            RepoState::Ready(server) => ("ready", String::new(), server.store().stats().unwrap_or_default()),
        };
        let stat = |k: &str| stats.iter().find(|(n, _)| n == k).map(|(_, v)| *v).unwrap_or(0);
        json!({
            "root": repo.root.to_string_lossy(),
            "name": repo.root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
            "state": word,
            "detail": detail,
            "files": stat("files"),
            "symbols": stat("symbols"),
            "services": stat("services"),
            "memories": stat("memories"),
        })
    }

    fn status(&self) -> Value {
        let repos: Vec<Value> = lock(&self.repos).values().map(|r| self.repo_json(r)).collect();
        let records = self.usage.list_usage(0).unwrap_or_default();
        let live: HashMap<String, (u64, crate::session::Saved, Option<String>)> = lock(&self.convs)
            .values()
            .map(|c| {
                let c = lock(c);
                (
                    c.record.id.clone(),
                    (
                        c.requests,
                        c.session.saved.clone(),
                        c.repo.as_ref().map(|p| p.to_string_lossy().to_string()),
                    ),
                )
            })
            .collect();
        // Totals per mode: what past sessions cost, and what sessions through the proxy cost.
        let mut by_mode: BTreeMap<String, [i64; 5]> = BTreeMap::new();
        for r in &records {
            let t = by_mode
                .entry(if r.mode.is_empty() {
                    "other".to_string()
                } else {
                    r.mode.clone()
                })
                .or_insert([0; 5]);
            t[0] += 1;
            t[1] += r.input_tokens;
            t[2] += r.cached_input_tokens;
            t[3] += r.output_tokens;
            t[4] += r.reasoning_tokens;
        }
        let modes: Vec<Value> = by_mode
            .iter()
            .map(|(mode, t)| json!({ "mode": mode, "sessions": t[0], "input_tokens": t[1], "cached_input_tokens": t[2], "output_tokens": t[3], "reasoning_tokens": t[4] }))
            .collect();
        let mut saved = crate::session::Saved::default();
        for (_, s, _) in live.values() {
            saved.outputs_shortened += s.outputs_shortened;
            saved.tokens_before += s.tokens_before;
            saved.tokens_after += s.tokens_after;
            saved.contexts_added += s.contexts_added;
            saved.context_tokens += s.context_tokens;
        }
        let mut recent: Vec<&UsageRecord> = records.iter().filter(|r| r.mode == "proxy").collect();
        recent.sort_by_key(|r| std::cmp::Reverse(r.finished_at));
        let sessions: Vec<Value> = recent
            .iter()
            .take(50)
            .map(|r| {
                let (requests, s, repo) = live.get(&r.id).cloned().unwrap_or_default();
                json!({
                    "id": r.session_id, "task": r.task, "model": r.model, "repo": repo, "requests": requests,
                    "started_at": r.started_at, "finished_at": r.finished_at,
                    "input_tokens": r.input_tokens, "cached_input_tokens": r.cached_input_tokens,
                    "output_tokens": r.output_tokens, "reasoning_tokens": r.reasoning_tokens,
                    "recommended_level": r.recommended_level,
                    "outputs_shortened": s.outputs_shortened, "tokens_before": s.tokens_before, "tokens_after": s.tokens_after,
                    "contexts_added": s.contexts_added, "context_tokens": s.context_tokens,
                })
            })
            .collect();
        json!({
            "version": env!("CARGO_PKG_VERSION"),
            "uptime_seconds": self.started.elapsed().as_secs(),
            "listen": format!("{}:{}", self.cfg.host, self.cfg.port),
            "upstream_api": self.cfg.upstream_api,
            "upstream_chatgpt": self.cfg.upstream_chatgpt,
            "workspaces": self.cfg.workspaces.iter().map(|w| w.to_string_lossy().to_string()).collect::<Vec<_>>(),
            "codex_home": self.cfg.codex_home.as_ref().map(|p| p.to_string_lossy().to_string()),
            "settings": *lock(&self.settings),
            "repos": repos,
            "modes": modes,
            "saved": saved,
            "sessions": sessions,
        })
    }

    /// Folders under `path`, for the folder picker.
    fn browse(&self, path: Option<String>) -> Value {
        let Some(path) = path.filter(|p| !p.is_empty()).map(PathBuf::from) else {
            let roots: Vec<Value> = self.cfg.workspaces.iter().map(|w| json!({ "name": w.to_string_lossy(), "path": w.to_string_lossy(), "git": w.join(".git").exists() })).collect();
            return json!({ "path": "", "folders": roots });
        };
        if !self.allowed(&path) {
            return json!({ "path": path.to_string_lossy(), "folders": [], "error": "outside the folders this daemon may index" });
        }
        let mut folders: Vec<(String, PathBuf)> = std::fs::read_dir(&path)
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|e| e.path().is_dir())
                    .map(|e| (e.file_name().to_string_lossy().to_string(), e.path()))
                    .filter(|(name, _)| !name.starts_with('.') && name != "node_modules" && name != "target")
                    .collect()
            })
            .unwrap_or_default();
        folders.sort();
        let folders: Vec<Value> = folders
            .into_iter()
            .take(500)
            .map(|(name, p)| json!({ "name": name, "path": p.to_string_lossy(), "git": p.join(".git").exists() }))
            .collect();
        json!({ "path": path.to_string_lossy(), "parent": path.parent().filter(|p| self.allowed(p)).map(|p| p.to_string_lossy().to_string()), "folders": folders })
    }

    /// Token counts of sessions the agent recorded itself, as the baseline.
    /// Sessions the proxy already measured are not counted twice. Only counts,
    /// ids and the model are read; no conversation text is stored.
    fn import_history(&self) -> Result<Value> {
        let home = self
            .cfg
            .codex_home
            .clone()
            .ok_or_else(|| anyhow!("no agent home folder is configured (mount ~/.codex and set LGE_CODEX_HOME)"))?;
        let known: std::collections::HashSet<String> = self.usage.list_usage(0)?.into_iter().map(|r| r.id).collect();
        let (mut imported, mut skipped, mut failed) = (0, 0, 0);
        let mut stack = vec![home.join("sessions")];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                if !(name.starts_with("rollout-") && name.ends_with(".jsonl")) {
                    continue;
                }
                let Ok(session) = lge_usage::parse_rollout_file(&path) else {
                    failed += 1;
                    continue;
                };
                let id = stable_id(&["session", &session.session_id]);
                if session.session_id.is_empty() || known.contains(&id) || session.usage.total() == 0 {
                    skipped += 1;
                    continue;
                }
                let when = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                let record = UsageRecord {
                    id,
                    task_id: session.session_id.clone(),
                    attempt: 1,
                    level: "top".to_string(),
                    runner: "codex".to_string(),
                    model: session.model.clone(),
                    session_id: session.session_id.clone(),
                    started_at: when,
                    finished_at: when,
                    input_tokens: session.usage.input_tokens,
                    cached_input_tokens: session.usage.cached_input_tokens,
                    reasoning_tokens: session.usage.reasoning_tokens,
                    output_tokens: session.usage.output_tokens,
                    outcome: outcome::UNKNOWN.to_string(),
                    mode: "history".to_string(),
                    ..UsageRecord::default()
                };
                self.usage.put_usage(&record)?;
                imported += 1;
            }
        }
        Ok(json!({ "imported": imported, "skipped": skipped, "unreadable": failed }))
    }

    fn api(self: &Arc<Self>, mut request: Request) {
        let url = request.url().to_string();
        let (path, query) = split_url(&url);
        let method = request.method().clone();
        // A page on another origin must not be able to drive the daemon.
        if method != Method::Get {
            let origin = request
                .headers()
                .iter()
                .find(|h| h.field.equiv("Origin"))
                .map(|h| h.value.as_str().to_string());
            let local = |o: &str| {
                o.starts_with("http://127.0.0.1") || o.starts_with("http://localhost") || o.starts_with("http://[::1]")
            };
            if origin.as_deref().is_some_and(|o| !local(o)) {
                return respond_json(
                    request,
                    403,
                    &json!({ "error": "requests from other sites are refused" }),
                );
            }
        }
        let mut text = String::new();
        let _ = request.as_reader().take(1024 * 1024).read_to_string(&mut text);
        let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let path_arg = || body.get("path").and_then(Value::as_str).map(PathBuf::from);
        let result: Result<Value> = match (method, path) {
            (Method::Get, "/lge/api/status") => Ok(self.status()),
            (Method::Get, "/lge/api/browse") => Ok(self.browse(query_value(query, "path"))),
            (Method::Post, "/lge/api/repos") => path_arg()
                .ok_or_else(|| anyhow!("`path` is required"))
                .and_then(|p| self.add_repo(&p))
                .map(|root| json!({ "root": root.to_string_lossy() })),
            (Method::Delete, "/lge/api/repos") => {
                let path = query_value(query, "path")
                    .map(PathBuf::from)
                    .or_else(path_arg)
                    .ok_or_else(|| anyhow!("`path` is required"));
                path.map(|p| json!({ "removed": self.remove_repo(&p) }))
            }
            (Method::Post, "/lge/api/settings") => serde_json::from_value::<Settings>(body.clone())
                .map_err(|e| anyhow!("{e}"))
                .map(|s| {
                    *lock(&self.settings) = s.clone();
                    self.save();
                    json!(s)
                }),
            (Method::Post, "/lge/api/import-history") => self.import_history(),
            _ => Err(anyhow!("no such endpoint")),
        };
        match result {
            Ok(v) => respond_json(request, 200, &v),
            Err(e) => respond_json(request, 400, &json!({ "error": format!("{e:#}") })),
        }
    }

    fn handle(self: &Arc<Self>, request: Request) {
        let url = request.url().to_string();
        let (path, _) = split_url(&url);
        if path.starts_with("/lge/api/") {
            self.api(request);
        } else if path == "/mcp" || path == "/mcp/" {
            self.mcp(request);
        } else if (path == "/" || path == "/index.html") && *request.method() == Method::Get {
            let mut response = Response::from_string(UI);
            if let Some(h) = header("Content-Type", "text/html; charset=utf-8") {
                response = response.with_header(h);
            }
            let _ = request.respond(response);
        } else if path == "/health" {
            respond_json(request, 200, &json!({ "ok": true }));
        } else if path == "/favicon.ico" {
            let _ = request.respond(Response::empty(204));
        } else {
            self.proxy(request);
        }
    }
}

/// Bind the listening socket. Returns the server and the address it got.
pub fn bind(daemon: &Arc<Daemon>) -> Result<(tiny_http::Server, std::net::SocketAddr)> {
    let addr = format!("{}:{}", daemon.cfg.host, daemon.cfg.port);
    let server = tiny_http::Server::http(&addr).map_err(|e| anyhow!("cannot listen on {addr}: {e}"))?;
    let bound = server
        .server_addr()
        .to_ip()
        .ok_or_else(|| anyhow!("not an IP listener"))?;
    Ok((server, bound))
}

/// Serve until the process ends: one thread per request.
pub fn run(daemon: Arc<Daemon>, server: tiny_http::Server) {
    for request in server.incoming_requests() {
        let daemon = daemon.clone();
        std::thread::spawn(move || daemon.handle(request));
    }
}
