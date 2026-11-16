//! The daemon between a client and a stand-in provider, over real sockets.
use lge_core::UsageRecord;
use lge_proxy::server::{bind, Config, Daemon, Settings};
use lge_store::{GraphStore, Store};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const STREAM: &str = "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n\
event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"done\"}\n\n\
event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":1200,\"input_tokens_details\":{\"cached_tokens\":1000},\"output_tokens\":300,\"output_tokens_details\":{\"reasoning_tokens\":250},\"total_tokens\":1500}}}\n\n";

/// What the stand-in provider received: (method, url, headers, body).
type Received = Arc<Mutex<Vec<(String, String, Vec<(String, String)>, Vec<u8>)>>>;

/// How the stand-in answers a `/responses` request: (url, body, index of the
/// request) -> (status, body).
type Answer = Box<dyn Fn(&str, &[u8], usize) -> (u16, String) + Send>;

/// A provider that records every request and answers `/responses` with a
/// fixed event stream.
fn provider() -> (u16, Received) {
    provider_with(Box::new(|_, _, _| (200, STREAM.to_string())))
}

/// A provider that records every request and answers `/responses` as `answer` says.
fn provider_with(answer: Answer) -> (u16, Received) {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    let received: Received = Arc::new(Mutex::new(Vec::new()));
    let log = received.clone();
    std::thread::spawn(move || {
        let mut n = 0;
        for mut request in server.incoming_requests() {
            let mut body = Vec::new();
            request.as_reader().read_to_end(&mut body).unwrap();
            let headers = request
                .headers()
                .iter()
                .map(|h| {
                    (
                        h.field.as_str().as_str().to_ascii_lowercase(),
                        h.value.as_str().to_string(),
                    )
                })
                .collect();
            let url = request.url().to_string();
            log.lock().unwrap().push((
                request.method().as_str().to_string(),
                url.clone(),
                headers,
                body.clone(),
            ));
            let header = |k: &str, v: &str| tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()).unwrap();
            let response = if url.ends_with("/responses") {
                let (status, text) = answer(&url, &body, n);
                n += 1;
                let kind = if status == 200 {
                    "text/event-stream"
                } else {
                    "application/json"
                };
                tiny_http::Response::from_string(text)
                    .with_status_code(status)
                    .with_header(header("Content-Type", kind))
                    .with_header(header("x-request-id", "req_42"))
            } else {
                tiny_http::Response::from_string("{\"data\":[{\"id\":\"gpt-test\"}]}")
                    .with_header(header("Content-Type", "application/json"))
            };
            request.respond(response).unwrap();
        }
    });
    (port, received)
}

/// The event stream with other usage numbers.
fn stream_with(input: i64, cached: i64) -> String {
    STREAM
        .replace("\"input_tokens\":1200", &format!("\"input_tokens\":{input}"))
        .replace("\"cached_tokens\":1000", &format!("\"cached_tokens\":{cached}"))
}

const REFUSAL: &str = r#"{"error":{"message":"Your input exceeds the context window of this model.","type":"invalid_request_error","code":"context_length_exceeded"}}"#;

/// Daemons open at once in this test binary. Each holds databases whose
/// buffer pools reserve a large share of the address space, so only a few
/// may be open together.
const OPEN_AT_ONCE: usize = 4;
static OPEN: (Mutex<usize>, Condvar) = (Mutex::new(0), Condvar::new());

/// A place among the [`OPEN_AT_ONCE`] daemons, held until dropped.
struct Slot;

impl Slot {
    fn take() -> Slot {
        let (count, freed) = &OPEN;
        let mut n = count.lock().unwrap_or_else(|e| e.into_inner());
        while *n >= OPEN_AT_ONCE {
            n = freed.wait(n).unwrap_or_else(|e| e.into_inner());
        }
        *n += 1;
        Slot
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        *OPEN.0.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
        OPEN.1.notify_one();
    }
}

/// A daemon serving on its own port until dropped. Dropped, it stops
/// listening and waits until nothing else holds the daemon, so its databases
/// are closed.
struct Serving {
    server: Arc<tiny_http::Server>,
    thread: Option<std::thread::JoinHandle<()>>,
    addr: std::net::SocketAddr,
    daemon: Arc<Daemon>,
}

impl Serving {
    fn start(daemon: Arc<Daemon>) -> Serving {
        let (server, addr) = bind(&daemon).unwrap();
        let server = Arc::new(server);
        let (listening, serving) = (server.clone(), daemon.clone());
        let thread = std::thread::spawn(move || {
            for request in listening.incoming_requests() {
                let daemon = serving.clone();
                std::thread::spawn(move || daemon.handle(request));
            }
        });
        Serving {
            server,
            thread: Some(thread),
            addr,
            daemon,
        }
    }
}

impl Drop for Serving {
    fn drop(&mut self) {
        self.server.unblock();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let deadline = Instant::now() + Duration::from_secs(60);
        while Arc::strong_count(&self.daemon) > 1 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

struct Running {
    base: String,
    received: Received,
    agent: ureq::Agent,
    repo: PathBuf,
    // Dropped in this order: the daemon, its folder, then its slot.
    serving: Serving,
    dir: tempfile::TempDir,
    _slot: Slot,
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn start() -> Running {
    start_with(provider(), |_| {})
}

/// A daemon in front of `provider`, its data folder prepared by `prepare`
/// before the daemon opens it.
fn start_with((provider_port, received): (u16, Received), prepare: impl FnOnce(&Path)) -> Running {
    let slot = Slot::take();
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("work/shop");
    copy_dir(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/polyglot"),
        &repo,
    );
    let repo = repo.canonicalize().unwrap();
    std::fs::create_dir_all(dir.path().join("data")).unwrap();
    prepare(&dir.path().join("data"));
    let daemon = Daemon::open(config(dir.path(), &repo, provider_port)).unwrap();
    let serving = Serving::start(daemon);
    let agent = ureq::Agent::new_with_config(ureq::Agent::config_builder().http_status_as_error(false).build());
    Running {
        base: format!("http://{}", serving.addr),
        received,
        agent,
        repo,
        serving,
        dir,
        _slot: slot,
    }
}

fn config(dir: &Path, repo: &Path, provider_port: u16) -> Config {
    let mut cfg = Config::new(dir.join("data"));
    cfg.port = 0;
    cfg.workspaces = vec![repo.parent().unwrap().to_path_buf()];
    cfg.upstream_api = format!("http://127.0.0.1:{provider_port}/v1");
    cfg.upstream_chatgpt = format!("http://127.0.0.1:{provider_port}/chatgpt");
    cfg
}

impl Running {
    fn post(&self, path: &str, body: &[u8], headers: &[(&str, &str)]) -> (u16, Vec<(String, String)>, Vec<u8>) {
        let mut builder = ureq::http::Request::builder()
            .method("POST")
            .uri(format!("{}{path}", self.base));
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        let response = self.agent.run(builder.body(body.to_vec()).unwrap()).unwrap();
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        (status, headers, response.into_body().read_to_vec().unwrap())
    }

    fn get(&self, path: &str) -> (u16, Vec<u8>) {
        let response = self.agent.get(format!("{}{path}", self.base)).call().unwrap();
        (response.status().as_u16(), response.into_body().read_to_vec().unwrap())
    }

    fn json(&self, path: &str) -> Value {
        serde_json::from_slice(&self.get(path).1).unwrap()
    }

    /// A Responses request with an API key; the status and the body relayed.
    fn send(&self, body: &Value) -> (u16, Vec<u8>) {
        let (status, _, answer) = self.post(
            "/v1/responses",
            &serde_json::to_vec(body).unwrap(),
            &[
                ("authorization", "Bearer test-key"),
                ("content-type", "application/json"),
            ],
        );
        (status, answer)
    }

    /// POST a JSON body to the page's API.
    fn api(&self, path: &str, body: &Value) -> Value {
        let (status, _, answer) = self.post(
            path,
            body.to_string().as_bytes(),
            &[("content-type", "application/json")],
        );
        let answer: Value = serde_json::from_slice(&answer).unwrap();
        assert_eq!(status, 200, "{answer}");
        answer
    }

    /// What the provider received, as JSON, in order.
    fn sent(&self) -> Vec<Value> {
        self.received
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, url, _, _)| url.ends_with("/responses"))
            .map(|(_, _, _, body)| serde_json::from_slice(body).unwrap_or(Value::Null))
            .collect()
    }

    fn add_repo(&self) {
        let add = json!({ "path": self.repo.to_string_lossy() });
        self.api("/lge/api/repos", &add);
        self.wait_ready();
    }

    fn last(&self) -> (String, String, Vec<(String, String)>, Vec<u8>) {
        self.received
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("the provider received a request")
    }

    fn wait_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let status = self.json("/lge/api/status");
            let state = status["repos"][0]["state"].as_str().unwrap_or("").to_string();
            if state == "ready" {
                return;
            }
            assert!(
                state != "failed" && Instant::now() < deadline,
                "repository not ready: {status}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

fn user(text: &str) -> Value {
    json!({ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": text }] })
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

fn request_body(cwd: &Path, conversation: &str, typed: &str) -> Value {
    json!({
        "model": "gpt-test",
        "prompt_cache_key": conversation,
        "instructions": "You are a coding agent.",
        "input": [
            user(&format!("<environment_context>\n  <cwd>{}</cwd>\n</environment_context>", cwd.display())),
            user(typed),
            { "type": "function_call", "name": "shell", "call_id": "c1", "arguments": json!({ "command": ["bash", "-lc", "pytest -v"] }).to_string() },
            { "type": "function_call_output", "call_id": "c1", "output": format!("Exit code: 0\nWall time: 9.9 seconds\nOutput:\n{}", passing_run()) },
        ],
        "stream": true,
    })
}

#[test]
fn a_request_is_shortened_relayed_and_measured() {
    let r = start();
    let body = request_body(Path::new("/somewhere/else"), "conv-a", "run the tests");
    let raw = serde_json::to_vec(&body).unwrap();
    let (status, headers, answer) = r.post(
        "/v1/responses",
        &raw,
        &[
            ("authorization", "Bearer test-key"),
            ("content-type", "application/json"),
            ("originator", "codex_cli_rs"),
        ],
    );

    // The client gets the provider's answer, byte for byte.
    assert_eq!(status, 200);
    assert_eq!(String::from_utf8(answer).unwrap(), STREAM);
    assert!(
        headers
            .iter()
            .any(|(k, v)| k == "content-type" && v.contains("text/event-stream")),
        "{headers:?}"
    );
    assert!(
        headers.iter().any(|(k, v)| k == "x-request-id" && v == "req_42"),
        "provider headers are relayed: {headers:?}"
    );

    // The provider got the caller's credentials and a smaller body.
    let (method, url, sent_headers, sent) = r.last();
    assert_eq!((method.as_str(), url.as_str()), ("POST", "/v1/responses"));
    assert!(
        sent_headers
            .iter()
            .any(|(k, v)| k == "authorization" && v == "Bearer test-key"),
        "{sent_headers:?}"
    );
    assert!(sent_headers
        .iter()
        .any(|(k, v)| k == "originator" && v == "codex_cli_rs"));
    assert!(sent.len() * 5 < raw.len(), "{} of {} bytes", sent.len(), raw.len());
    let sent: Value = serde_json::from_slice(&sent).unwrap();
    assert!(
        sent["input"][3]["output"]
            .as_str()
            .unwrap()
            .starts_with("Exit code: 0\nWall time: 9.9 seconds\nOutput:\nok"),
        "{sent}"
    );
    for field in ["model", "prompt_cache_key", "instructions", "stream"] {
        assert_eq!(sent[field], body[field], "{field}");
    }
    assert_eq!(sent["input"][1], body["input"][1]);

    // The usage of the stream is on the page's API.
    let status = r.json("/lge/api/status");
    let session = &status["sessions"][0];
    assert_eq!(session["task"], "run the tests", "{status}");
    assert_eq!(
        (
            session["input_tokens"].as_i64(),
            session["cached_input_tokens"].as_i64()
        ),
        (Some(1200), Some(1000))
    );
    assert_eq!(
        (session["output_tokens"].as_i64(), session["reasoning_tokens"].as_i64()),
        (Some(300), Some(250))
    );
    assert_eq!(session["outputs_shortened"], 1);
    assert!(session["tokens_before"].as_i64() > session["tokens_after"].as_i64());

    // The second turn of the same conversation adds up and repeats the rewrite.
    let first_output = sent["input"][3].clone();
    let mut next = body.clone();
    next["input"].as_array_mut().unwrap().push(user("thanks"));
    r.post(
        "/v1/responses",
        &serde_json::to_vec(&next).unwrap(),
        &[("authorization", "Bearer test-key")],
    );
    let again: Value = serde_json::from_slice(&r.last().3).unwrap();
    assert_eq!(again["input"][3], first_output, "the cached prefix must not change");
    let status = r.json("/lge/api/status");
    assert_eq!(status["sessions"][0]["input_tokens"], 2400);
    assert_eq!(status["sessions"][0]["requests"], 2);
    assert_eq!(status["saved"]["outputs_shortened"], 1);
}

#[test]
fn what_the_proxy_does_not_understand_passes_untouched() {
    let r = start();
    // Not JSON: forwarded as it came.
    let (status, _, _) = r.post("/v1/responses", b"{this is not json", &[("authorization", "Bearer k")]);
    assert_eq!(status, 200);
    assert_eq!(r.last().3, b"{this is not json");
    // Another endpoint, another method, with its query string.
    let (status, body) = r.get("/v1/models?limit=5");
    assert_eq!(
        (status, String::from_utf8(body).unwrap().as_str()),
        (200, "{\"data\":[{\"id\":\"gpt-test\"}]}")
    );
    let (method, url, _, _) = r.last();
    assert_eq!((method.as_str(), url.as_str()), ("GET", "/v1/models?limit=5"));
    // A ChatGPT login is sent to the other upstream.
    r.post(
        "/v1/responses",
        b"{}",
        &[("chatgpt-account-id", "acct_1"), ("authorization", "Bearer token")],
    );
    assert_eq!(r.last().1, "/chatgpt/responses");
    // Nothing of these became a session.
    assert_eq!(
        r.json("/lge/api/status")["sessions"].as_array().unwrap().len(),
        1,
        "only the `{{}}` body parsed as a request"
    );
}

#[test]
fn a_websocket_upgrade_is_declined_so_the_agent_uses_http() {
    let r = start();
    let response = r
        .agent
        .run(
            ureq::http::Request::builder()
                .method("GET")
                .uri(format!("{}/v1/responses", r.base))
                .header("connection", "Upgrade")
                .header("upgrade", "websocket")
                .header("sec-websocket-version", "13")
                .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
                .body(())
                .unwrap(),
        )
        .unwrap();
    assert_eq!(response.status().as_u16(), 426);
    assert!(r.received.lock().unwrap().is_empty(), "nothing reached the provider");
}

#[test]
fn each_agent_thread_gets_its_own_record() {
    let r = start();
    let body = serde_json::to_vec(&request_body(
        Path::new("/somewhere/else"),
        "shared-key",
        "run the tests",
    ))
    .unwrap();
    r.post("/v1/responses", &body, &[("thread-id", "thread-parent")]);
    r.post("/v1/responses", &body, &[("thread-id", "thread-child")]);
    let status = r.json("/lge/api/status");
    assert_eq!(status["sessions"].as_array().unwrap().len(), 2, "{status}");
}

#[test]
fn a_compressed_request_body_is_read_and_forwarded_plain() {
    let r = start();
    let body = request_body(Path::new("/somewhere/else"), "conv-z", "run the tests");
    let raw = serde_json::to_vec(&body).unwrap();
    let packed = ruzstd::encoding::compress_to_vec(&raw[..], ruzstd::encoding::CompressionLevel::Fastest);
    let (status, _, _) = r.post(
        "/v1/responses",
        &packed,
        &[("content-encoding", "zstd"), ("authorization", "Bearer k")],
    );
    assert_eq!(status, 200);
    let (_, _, headers, sent) = r.last();
    assert!(!headers.iter().any(|(k, _)| k == "content-encoding"), "{headers:?}");
    let sent: Value = serde_json::from_slice(&sent).expect("the provider got plain JSON");
    assert!(sent["input"][3]["output"].as_str().unwrap().contains("Output:\nok"));
}

#[test]
fn an_unreachable_provider_is_an_error_the_client_can_read() {
    let _slot = Slot::take();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::new(dir.path().join("data"));
    cfg.port = 0;
    cfg.upstream_api = "http://127.0.0.1:9/v1".to_string();
    let serving = Serving::start(Daemon::open(cfg).unwrap());
    let addr = serving.addr;
    let agent = ureq::Agent::new_with_config(ureq::Agent::config_builder().http_status_as_error(false).build());
    let response = agent.post(format!("http://{addr}/v1/responses")).send("{}").unwrap();
    assert_eq!(response.status().as_u16(), 502);
    let body: Value = serde_json::from_slice(&response.into_body().read_to_vec().unwrap()).unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("lge proxy: the provider could not be reached"),
        "{body}"
    );
}

#[test]
fn a_folder_is_indexed_and_its_graph_reaches_the_request_and_the_tools() {
    let r = start();
    let page = String::from_utf8(r.get("/").1).unwrap();
    assert!(page.contains("<title>lge</title>") && page.contains("Folders in the graph"));

    // Folders outside the allowed workspace are refused; browsing lists what is inside.
    let (status, _, refused) = r.post(
        "/lge/api/repos",
        br#"{"path":"/etc"}"#,
        &[("content-type", "application/json")],
    );
    assert_eq!(status, 400, "{}", String::from_utf8_lossy(&refused));
    let workspace = r.repo.parent().unwrap().to_string_lossy().to_string();
    let listing = r.json(&format!("/lge/api/browse?path={}", workspace.replace('/', "%2F")));
    assert_eq!(listing["folders"][0]["name"], "shop", "{listing}");
    // A page on another site cannot drive the daemon.
    let (status, _, _) = r.post(
        "/lge/api/repos",
        br#"{"path":"/tmp"}"#,
        &[("origin", "https://evil.example")],
    );
    assert_eq!(status, 403);

    let add = json!({ "path": r.repo.to_string_lossy() }).to_string();
    let (status, _, _) = r.post(
        "/lge/api/repos",
        add.as_bytes(),
        &[("content-type", "application/json"), ("origin", r.base.as_str())],
    );
    assert_eq!(status, 200);
    r.wait_ready();
    let status = r.json("/lge/api/status");
    assert_eq!(
        (
            status["repos"][0]["name"].as_str(),
            status["repos"][0]["files"].as_i64()
        ),
        (Some("shop"), Some(17)),
        "{status}"
    );

    // A typed message about the code gets the graph's context right after it.
    let cwd = r.repo.join("orders_api");
    let body = request_body(&cwd, "conv-b", "create_order must validate the payload");
    r.post(
        "/v1/responses",
        &serde_json::to_vec(&body).unwrap(),
        &[("authorization", "Bearer k")],
    );
    let sent: Value = serde_json::from_slice(&r.last().3).unwrap();
    let items = sent["input"].as_array().unwrap();
    assert_eq!(items.len(), 5, "one context item was added");
    assert_eq!(items[1], body["input"][1], "the typed message itself is untouched");
    let context = items[2]["content"][0]["text"].as_str().unwrap();
    assert!(
        context.starts_with("<lge_context>") && context.ends_with("</lge_context>"),
        "{context}"
    );
    assert!(
        context.contains("orders_api/routes.py:10-15  function orders_api.routes.create_order"),
        "{context}"
    );
    assert_eq!(items[2]["role"], "user");

    // The next turn carries the same context bytes at the same place.
    let mut next = body.clone();
    next["input"].as_array_mut().unwrap().push(user("ok, do it"));
    r.post(
        "/v1/responses",
        &serde_json::to_vec(&next).unwrap(),
        &[("authorization", "Bearer k")],
    );
    let again: Value = serde_json::from_slice(&r.last().3).unwrap();
    assert_eq!(again["input"][2], items[2]);
    assert_eq!(again["input"].as_array().unwrap().len(), 6, "small talk adds nothing");
    let status = r.json("/lge/api/status");
    assert_eq!(status["saved"]["contexts_added"], 1);
    assert_eq!(
        status["sessions"][0]["recommended_level"], "top",
        "the router's advice is logged, not applied: {status}"
    );

    // The same graph over MCP on HTTP.
    let rpc = |message: Value| -> Value {
        let (status, _, body) = r.post(
            "/mcp",
            message.to_string().as_bytes(),
            &[
                ("content-type", "application/json"),
                ("accept", "application/json, text/event-stream"),
            ],
        );
        assert_eq!(status, 200);
        serde_json::from_slice(&body).unwrap()
    };
    let init = rpc(
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18" } }),
    );
    assert_eq!(init["result"]["serverInfo"]["name"], "lge");
    let (status, _, _) = r.post(
        "/mcp",
        br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        &[],
    );
    assert_eq!(status, 202);
    let found = rpc(
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "analyze_change_impact", "arguments": { "target": "create_order" } } }),
    );
    let text = found["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("web_gateway.src.ordersClient.createOrder") && found["result"]["isError"] == false,
        "{text}"
    );
}

#[test]
fn bulky_folders_are_reported_with_the_line_that_hides_them() {
    let r = start();
    std::fs::create_dir_all(r.repo.join("artifacts/2025")).unwrap();
    std::fs::write(r.repo.join("artifacts/2025/build.tar"), vec![0u8; 4096]).unwrap();
    std::fs::create_dir_all(r.repo.join("logs")).unwrap();
    std::fs::write(r.repo.join("logs/today.log"), "x").unwrap();
    std::fs::write(r.repo.join(".gitignore"), "/logs/\n").unwrap();
    // Before anything is indexed, no repository and nothing to report.
    assert!(r.json("/lge/api/status")["repos"].as_array().unwrap().is_empty());

    let add = json!({ "path": r.repo.to_string_lossy() }).to_string();
    let (status, _, _) = r.post(
        "/lge/api/repos",
        add.as_bytes(),
        &[("content-type", "application/json")],
    );
    assert_eq!(status, 200);
    r.wait_ready();
    let status = r.json("/lge/api/status");
    let bulky = status["repos"][0]["bulky"].as_array().unwrap();
    assert_eq!(bulky.len(), 1, "the ignored `logs/` is not reported: {status}");
    assert_eq!(bulky[0]["path"], "artifacts");
    assert_eq!(bulky[0]["ignore_line"], "/artifacts/");
    assert_eq!(
        (bulky[0]["files"].as_u64(), bulky[0]["bytes"].as_u64()),
        (Some(1), Some(4096))
    );
    assert!(bulky[0]["reason"].as_str().unwrap().contains("archives"), "{status}");
    // The page renders the warning from these fields.
    let page = String::from_utf8(r.get("/").1).unwrap();
    assert!(page.contains("d.ignore_line") && page.contains("Searches would read"));

    // Found when the folder is indexed, not on every status call: a folder
    // added since shows only after the next index.
    std::fs::create_dir_all(r.repo.join("dist")).unwrap();
    std::fs::write(r.repo.join("dist/bundle.js"), "var a;").unwrap();
    for _ in 0..3 {
        let again = r.json("/lge/api/status");
        assert_eq!(again["repos"][0]["bulky"].as_array().unwrap().len(), 1, "{again}");
    }
    let (status, _, _) = r.post(
        "/lge/api/repos",
        add.as_bytes(),
        &[("content-type", "application/json")],
    );
    assert_eq!(status, 200);
    r.wait_ready();
    let status = r.json("/lge/api/status");
    let lines: Vec<&str> = status["repos"][0]["bulky"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["ignore_line"].as_str().unwrap())
        .collect();
    assert_eq!(lines, ["/artifacts/", "/dist/"], "{status}");

    // A folder deleted after it was added shows as missing, not ready, and
    // can still be removed from the list.
    let parked = r.repo.with_file_name("shop-parked");
    std::fs::rename(&r.repo, &parked).unwrap();
    let gone = r.json("/lge/api/status");
    assert_eq!(gone["repos"][0]["state"], "missing", "{gone}");
    assert!(gone["repos"][0]["detail"]
        .as_str()
        .unwrap()
        .contains("no longer exists"));
    std::fs::rename(&parked, &r.repo).unwrap();
    assert_eq!(r.json("/lge/api/status")["repos"][0]["state"], "ready");

    // `DELETE /lge/api/repos` with the path in a JSON body (as `lge ab` sends it).
    let response = r
        .agent
        .run(
            ureq::http::Request::builder()
                .method("DELETE")
                .uri(format!("{}/lge/api/repos", r.base))
                .header("content-type", "application/json")
                .body(add.clone().into_bytes())
                .unwrap(),
        )
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let answer: Value = serde_json::from_slice(&response.into_body().read_to_vec().unwrap()).unwrap();
    assert_eq!(answer["removed"], true, "{answer}");
    assert!(r.json("/lge/api/status")["repos"].as_array().unwrap().is_empty());
}

/// A passing pytest run of `n` tests.
fn run_of(n: usize) -> String {
    let mut run = format!(
        "============================= test session starts ==============================\ncollected {n} items\n\n"
    );
    for i in 0..n {
        run.push_str(&format!(
            "tests/test_orders.py::test_case_number_{i} PASSED                              [{:3}%]\n",
            i * 100 / n
        ));
    }
    run + &format!("\n============================= {n} passed in 9.87s ==============================\n")
}

/// Append a shell call that ran `cmd` and printed `output`.
fn push_call(body: &mut Value, id: &str, cmd: &str, output: &str) {
    let items = body["input"].as_array_mut().unwrap();
    items.push(json!({ "type": "function_call", "name": "shell", "call_id": id, "arguments": json!({ "command": ["bash", "-lc", cmd] }).to_string() }));
    items.push(json!({ "type": "function_call_output", "call_id": id, "output": format!("Exit code: 0\nWall time: 9.9 seconds\nOutput:\n{output}") }));
}

fn has_context(body: &Value) -> bool {
    body.to_string().contains("<lge_context>")
}

#[test]
fn a_refused_rewrite_is_sent_again_without_its_new_rewrites() {
    let r = start_with(
        provider_with(Box::new(|_, body, _| {
            if String::from_utf8_lossy(body).contains("<lge_context>") {
                (400, REFUSAL.to_string())
            } else {
                (200, STREAM.to_string())
            }
        })),
        |_| {},
    );
    r.add_repo();
    let cwd = r.repo.join("orders_api");
    // Turn 1: the test run is shortened; nothing about the code yet.
    let mut body = request_body(&cwd, "conv-r", "please help me");
    let (status, _) = r.send(&body);
    assert_eq!(status, 200);
    let first = r.sent().pop().unwrap();
    assert!(!has_context(&first));
    let shortened = first["input"][3].clone();
    assert!(
        shortened["output"].as_str().unwrap().contains("Output:\nok"),
        "{shortened}"
    );

    // Turn 2: a message about the code gets a pack; the provider refuses it.
    body["input"]
        .as_array_mut()
        .unwrap()
        .push(user("create_order must validate the payload"));
    let (status, answer) = r.send(&body);
    assert_eq!(status, 200, "the client gets the retry's answer");
    assert_eq!(String::from_utf8(answer).unwrap(), STREAM);
    let sent = r.sent();
    assert_eq!(sent.len(), 3, "the refused request and its retry");
    assert!(has_context(&sent[1]), "the first try carried the pack");
    assert!(!has_context(&sent[2]), "the retry did not");
    assert_eq!(sent[2]["input"][3], shortened, "an earlier decision keeps its bytes");
    assert_eq!(sent[2]["input"].as_array().unwrap().len(), 5);

    // Turn 3: the reverted item stays as the client sent it.
    body["input"].as_array_mut().unwrap().push(user("ok, do it"));
    let (status, _) = r.send(&body);
    assert_eq!(status, 200);
    let sent = r.sent();
    assert_eq!(sent.len(), 4, "no retry needed");
    assert!(!has_context(&sent[3]));
    assert_eq!(sent[3]["input"][3], shortened);

    let status = r.json("/lge/api/status");
    let session = &status["sessions"][0];
    assert_eq!(session["raw_retries"], 1, "{session}");
    assert_eq!(session["requests"], 3, "a retry is not another request");
    assert_eq!(status["monitor"]["raw_retries"], 1);
    let traces = r.json("/lge/api/sessions/conv-r/requests");
    let requests = traces["requests"].as_array().unwrap();
    assert_eq!(requests.len(), 3, "{traces}");
    assert_eq!(
        (
            requests[1]["request"].as_u64(),
            requests[1]["retried"].as_bool(),
            requests[1]["status"].as_u64()
        ),
        (Some(2), Some(true), Some(200))
    );
    assert_eq!(requests[1]["lge_added_tokens"], 0, "the retry added nothing");
}

#[test]
fn a_rate_limit_is_relayed_without_a_retry() {
    let r = start_with(
        provider_with(Box::new(|_, _, _| {
            (
                429,
                r#"{"error":{"type":"rate_limit_exceeded","code":"rate_limit_exceeded"}}"#.to_string(),
            )
        })),
        |_| {},
    );
    let (status, answer) = r.send(&request_body(Path::new("/somewhere/else"), "conv-l", "run the tests"));
    assert_eq!(status, 429);
    assert!(String::from_utf8(answer).unwrap().contains("rate_limit_exceeded"));
    assert_eq!(r.sent().len(), 1, "never retried");
    assert!(r.sent()[0]["input"][3]["output"]
        .as_str()
        .unwrap()
        .contains("Output:\nok"));
}

#[test]
fn an_unchanged_request_or_one_with_only_old_rewrites_is_never_retried() {
    // The first request is accepted, every later one refused.
    let r = start_with(
        provider_with(Box::new(|_, _, n| {
            if n == 0 {
                (200, STREAM.to_string())
            } else {
                (400, REFUSAL.to_string())
            }
        })),
        |_| {},
    );
    let mut body = request_body(Path::new("/somewhere/else"), "conv-o", "run the tests");
    assert_eq!(r.send(&body).0, 200);
    // Only the earlier decision changes this body: a retry would send the same.
    body["input"].as_array_mut().unwrap().push(user("thanks"));
    let (status, answer) = r.send(&body);
    assert_eq!(status, 400);
    assert_eq!(
        String::from_utf8(answer).unwrap(),
        REFUSAL,
        "the refusal is relayed as it came"
    );
    assert_eq!(r.sent().len(), 2);
    // A body lge did not change at all.
    let plain = json!({ "model": "gpt-test", "prompt_cache_key": "conv-u", "input": [user("hello")] });
    let raw = serde_json::to_vec(&plain).unwrap();
    let (status, _, _) = r.post("/v1/responses", &raw, &[("authorization", "Bearer k")]);
    assert_eq!(status, 400);
    assert_eq!(r.sent().len(), 3);
    assert_eq!(r.last().3, raw, "forwarded byte for byte");
    assert_eq!(r.json("/lge/api/status")["monitor"]["raw_retries"], 0);
}

#[test]
fn old_settings_load_with_the_skeleton_mapped() {
    let r = start_with(provider(), |data| {
        std::fs::write(
            data.join("daemon.json"),
            r#"{"repos":[],"settings":{"inject_context":true,"shorten_outputs":true,"shorten_reads":true,"context_budget":1500,"auto_index":true}}"#,
        )
        .unwrap();
    });
    let status = r.json("/lge/api/status");
    let settings = &status["settings"];
    assert_eq!(settings["rules"]["skeleton"], "on", "{settings}");
    assert_eq!(settings["shorten_reads"], true);
    assert_eq!(settings["rules"]["check"], "on");
    assert_eq!(settings["rules"]["noise"], "shadow");
    assert_eq!(settings["holdout_share"], 0.0);
    assert_eq!(status["settings_error"], Value::Null);
    // An older client that changes only shorten_reads, sending the rest back.
    let mut back = settings.clone();
    back["shorten_reads"] = json!(false);
    let saved = r.api("/lge/api/settings", &back);
    assert_eq!(saved["rules"]["skeleton"], "shadow");
    let file: Value =
        serde_json::from_str(&std::fs::read_to_string(r.dir.path().join("data/daemon.json")).unwrap()).unwrap();
    assert_eq!(file["settings"]["rules"]["skeleton"], "shadow");
    assert_eq!(file["settings"]["shorten_reads"], false);
}

#[test]
fn an_unreadable_settings_file_starts_from_the_defaults_and_says_so() {
    let r = start_with(provider(), |data| {
        std::fs::write(
            data.join("daemon.json"),
            r#"{"repos":[],"settings":{"rules":{"check":"often"}}}"#,
        )
        .unwrap();
    });
    let status = r.json("/lge/api/status");
    assert_eq!(status["settings"], serde_json::to_value(Settings::default()).unwrap());
    let error = status["settings_error"].as_str().expect("an error is shown");
    assert!(error.contains("daemon.json"), "{error}");
    assert!(
        r.dir.path().join("data/daemon.json.unreadable").exists(),
        "the old file is kept"
    );
    // The defaults are in use: a check run is shortened.
    r.send(&request_body(Path::new("/somewhere/else"), "conv-c", "run the tests"));
    assert!(r.sent()[0]["input"][3]["output"]
        .as_str()
        .unwrap()
        .contains("Output:\nok"));
    // Saving from the page clears it.
    r.api("/lge/api/settings", &json!({ "inject_context": true }));
    assert_eq!(r.json("/lge/api/status")["settings_error"], Value::Null);
}

#[test]
fn the_holdout_arm_forwards_the_clients_bytes_and_is_fixed_per_conversation() {
    let r = start();
    assert_eq!(
        r.api("/lge/api/settings", &json!({ "holdout_share": 1.0 }))["holdout_share"],
        1.0
    );
    let mut held = request_body(Path::new("/somewhere/else"), "conv-h", "run the tests");
    let raw = serde_json::to_vec(&held).unwrap();
    let (status, _, _) = r.post("/v1/responses", &raw, &[("authorization", "Bearer k")]);
    assert_eq!(status, 200);
    assert_eq!(r.last().3, raw, "byte-identical");
    let status = r.json("/lge/api/status");
    assert_eq!(status["sessions"][0]["mode"], "proxy-holdout");
    let check = status["rules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == "check")
        .unwrap()
        .clone();
    assert_eq!(
        (
            check["stats"]["items"].as_u64(),
            check["stats"]["shadow_items"].as_u64()
        ),
        (Some(0), Some(1)),
        "what it would have saved is counted: {check}"
    );
    assert_eq!(status["holdout"]["holdout"]["sessions"], 1);
    assert_eq!(status["holdout"]["note"], "not enough sessions");

    // Share 0 holds nobody new out; the held-out conversation stays in its arm.
    r.api("/lge/api/settings", &json!({ "holdout_share": 0.0 }));
    held["input"].as_array_mut().unwrap().push(user("thanks"));
    let raw = serde_json::to_vec(&held).unwrap();
    r.post("/v1/responses", &raw, &[("authorization", "Bearer k")]);
    assert_eq!(r.last().3, raw, "still in the holdout arm");
    r.send(&request_body(Path::new("/somewhere/else"), "conv-p", "run the tests"));
    assert!(r.sent()[2]["input"][3]["output"]
        .as_str()
        .unwrap()
        .contains("Output:\nok"));
    let status = r.json("/lge/api/status");
    let modes: Vec<(&str, &str)> = status["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["id"].as_str().unwrap(), s["mode"].as_str().unwrap()))
        .collect();
    assert!(
        modes.contains(&("conv-h", "proxy-holdout")) && modes.contains(&("conv-p", "proxy")),
        "{modes:?}"
    );
}

#[test]
fn a_conversation_continues_its_counters_after_a_restart() {
    let (port, received) = provider();
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("work");
    std::fs::create_dir_all(&repo).unwrap();
    let repo = repo.canonicalize().unwrap().join("shop");
    let cfg = config(dir.path(), &repo, port);
    let agent = ureq::Agent::new_with_config(ureq::Agent::config_builder().http_status_as_error(false).build());
    let mut body = request_body(Path::new("/somewhere/else"), "conv-s", "run the tests");
    let send = |addr: std::net::SocketAddr, body: &Value| {
        let response = agent
            .post(format!("http://{addr}/v1/responses"))
            .header("authorization", "Bearer k")
            .send(serde_json::to_vec(body).unwrap())
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        response.into_body().read_to_vec().unwrap();
    };

    let _slot = Slot::take();
    // The first daemon serves one request and stops.
    {
        let first = Serving::start(Daemon::open(cfg.clone()).unwrap());
        send(first.addr, &body);
    }
    // A new daemon on the same data folder: the stored record goes on.
    let second = Serving::start(Daemon::open(cfg).unwrap());
    let addr = second.addr;
    body["input"].as_array_mut().unwrap().push(user("thanks"));
    send(addr, &body);
    let status: Value = serde_json::from_slice(
        &agent
            .get(format!("http://{addr}/lge/api/status"))
            .call()
            .unwrap()
            .into_body()
            .read_to_vec()
            .unwrap(),
    )
    .unwrap();
    let session = &status["sessions"][0];
    assert_eq!(session["input_tokens"], 2400, "{session}");
    assert_eq!(session["output_tokens"], 600);
    assert_eq!(session["requests"], 2);
    assert_eq!(status["sessions"].as_array().unwrap().len(), 1);
    assert!(session["removed_tokens_sent"].as_i64().unwrap() > 0);
    assert_eq!(received.lock().unwrap().len(), 2);
}

#[test]
fn a_rule_with_too_many_recoveries_is_demoted_for_new_items_only() {
    let r = start_with(provider(), |data| {
        // Stored sessions of the last days: 200 judged check items, 40 of them recovered.
        let store = Store::open(&data.join("usage.lbug")).unwrap();
        let now = lge_retrieve::unix_now();
        store
            .put_usage(&UsageRecord {
                id: "earlier".to_string(),
                task_id: "earlier".to_string(),
                mode: "proxy".to_string(),
                started_at: now - 3600,
                finished_at: now - 3600,
                rule_stats: json!({ "by_rule": { "check": { "items": 200, "judged": 200, "recovered": 40 } } })
                    .to_string(),
                ..UsageRecord::default()
            })
            .unwrap();
    });
    let mut body = request_body(Path::new("/somewhere/else"), "conv-d", "run the tests");
    r.send(&body);
    let before = r.sent()[0]["input"][3].clone();
    assert!(before["output"].as_str().unwrap().contains("Output:\nok"));
    // The check runs once per 50 recorded responses.
    let filler = json!({ "model": "gpt-test", "prompt_cache_key": "filler", "input": [user("hello")] });
    for _ in 0..49 {
        assert_eq!(r.send(&filler).0, 200);
    }
    let status = r.json("/lge/api/status");
    let reason = status["settings"]["demoted"]["check"]
        .as_str()
        .expect("check is demoted")
        .to_string();
    assert!(reason.starts_with("recovery 20.0% over 200 items since 20"), "{reason}");
    let check = status["rules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == "check")
        .unwrap()
        .clone();
    assert_eq!(
        (check["mode"].as_str(), check["effective_mode"].as_str()),
        (Some("on"), Some("shadow"))
    );
    assert_eq!(check["demoted"], reason.as_str());
    let file: Value =
        serde_json::from_str(&std::fs::read_to_string(r.dir.path().join("data/daemon.json")).unwrap()).unwrap();
    assert_eq!(file["settings"]["demoted"]["check"], reason.as_str(), "saved");

    // The earlier item keeps its bytes; a new one is sent whole.
    push_call(&mut body, "c2", "pytest -v tests/test_orders.py", &run_of(301));
    r.send(&body);
    let sent = r.sent().pop().unwrap();
    assert_eq!(sent["input"][3], before);
    assert_eq!(sent["input"][5], body["input"][5], "sent as the client sent it");

    // Re-enabled: counting starts again from now, and new items are shortened.
    let saved = r.api("/lge/api/settings", &json!({ "clear_demotion": "check" }));
    assert!(saved["demoted"].as_object().unwrap().is_empty());
    assert!(saved["rule_since"]["check"].as_i64().is_some());
    push_call(&mut body, "c3", "pytest -q tests", &run_of(302));
    r.send(&body);
    let sent = r.sent().pop().unwrap();
    assert_eq!(sent["input"][3], before);
    assert_eq!(sent["input"][5], body["input"][5], "decided once");
    assert!(
        sent["input"][7]["output"].as_str().unwrap().contains("Output:\nok"),
        "{}",
        sent["input"][7]
    );
}

#[test]
fn a_cold_restart_is_counted_after_an_idle_gap() {
    let r = start_with(
        provider_with(Box::new(|_, _, n| match n {
            0 => (200, stream_with(50_000, 0)),
            _ => (200, stream_with(52_000, 1_000)),
        })),
        |_| {},
    );
    let mut body = request_body(Path::new("/somewhere/else"), "conv-g", "run the tests");
    r.send(&body);
    r.serving.daemon.advance_clock(400);
    body["input"].as_array_mut().unwrap().push(user("still there?"));
    r.send(&body);
    let status = r.json("/lge/api/status");
    let session = &status["sessions"][0];
    assert_eq!(
        (session["cold_restarts"].as_u64(), session["cold_tokens"].as_i64()),
        (Some(1), Some(49_000)),
        "{session}"
    );
    assert_eq!(status["monitor"]["cold_restarts"], 1);
}

#[test]
fn the_status_reports_rules_holdout_prefix_ghost_tools_monitor_and_annuity() {
    let r = start();
    let mut body = request_body(Path::new("/somewhere/else"), "conv-t", "run the tests");
    body["tools"] = json!([
        { "type": "function", "name": "shell", "description": "Runs a shell command.", "parameters": { "type": "object" } },
        { "type": "function", "name": "view_image", "description": "Shows an image.", "parameters": { "type": "object" } },
        { "type": "namespace", "name": "mcp__docs__", "description": "Docs.", "tools": [
            { "type": "function", "name": "lookup", "description": "Looks a term up.", "parameters": { "type": "object" } },
            { "type": "function", "name": "fetch", "description": "Fetches a page.", "parameters": { "type": "object" } }
        ] }
    ]);
    let items = body["input"].as_array_mut().unwrap();
    items.push(json!({ "type": "function_call", "name": "lookup", "namespace": "mcp__docs__", "call_id": "m1", "arguments": "{}" }));
    items.push(json!({ "type": "function_call_output", "call_id": "m1", "output": "a short answer" }));
    r.send(&body);
    let status = r.json("/lge/api/status");

    let rules = status["rules"].as_array().unwrap();
    assert_eq!(rules.len(), 12);
    let check = rules.iter().find(|x| x["id"] == "check").unwrap();
    assert_eq!(
        (check["kind"].as_str(), check["mode"].as_str()),
        (Some("lossy"), Some("on"))
    );
    assert_eq!(check["stats"]["items"], 1);
    assert_eq!(check["healthy"], false, "too few judged items to say");
    assert!(rules
        .iter()
        .any(|x| x["id"] == "rerun_delta" && x["kind"] == "measure_only"));
    assert_eq!(status["thresholds"]["min_judged"], 200);
    assert!(status["cohorts"].is_array());
    assert_eq!(status["holdout"]["proxy"]["sessions"], 1);
    assert_eq!(status["holdout"]["note"], "not enough sessions");

    let prefix = &status["prefix"];
    assert_eq!(
        (prefix["lge_breaks"].as_u64(), prefix["client_breaks"].as_u64()),
        (Some(0), Some(0))
    );
    assert!(prefix["fixed"]["instructions_tokens"].as_u64().unwrap() > 0);
    let groups = prefix["fixed"]["groups"].as_array().unwrap();
    let docs = groups
        .iter()
        .find(|g| g["group"] == "docs")
        .expect("the MCP server is a group");
    assert_eq!((docs["mcp"].as_bool(), docs["tools"].as_u64()), (Some(true), Some(2)));
    assert_eq!(docs["config"], "[mcp_servers.docs]");
    assert!(groups.iter().any(|g| g["group"] == "built-in" && g["tools"] == 2));
    let ghosts: Vec<&str> = status["ghost_tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["name"].as_str().unwrap())
        .collect();
    assert!(
        ghosts.contains(&"mcp__docs__fetch") && ghosts.contains(&"view_image"),
        "{ghosts:?}"
    );
    assert!(
        !ghosts.contains(&"mcp__docs__lookup") && !ghosts.contains(&"shell"),
        "called: {ghosts:?}"
    );

    assert_eq!(status["monitor"]["raw_retries"], 0);
    assert_eq!(status["monitor"]["cold_restarts"], 0);
    let removed = status["annuity"]["removed_tokens_sent"].as_i64().unwrap();
    assert!(removed > 0, "{}", status["annuity"]);
    let session = &status["sessions"][0];
    assert_eq!(session["removed_tokens_sent"].as_i64(), Some(removed));
    assert_eq!(session["added_tokens_sent"], 0);
    assert_eq!(session["net_tokens"].as_i64(), Some(removed));
    let share = session["net_share"].as_f64().unwrap();
    assert!(
        (share - removed as f64 / (1200 + removed) as f64).abs() < 1e-9,
        "{share}"
    );
    assert_eq!(status["settings_error"], Value::Null);

    // The trace of the request, for `lge ab`.
    let traces = r.json("/lge/api/sessions/conv-t/requests");
    assert_eq!(traces["known"], true);
    let t = &traces["requests"][0];
    assert_eq!(
        (
            t["request"].as_u64(),
            t["input_tokens"].as_i64(),
            t["cached_input_tokens"].as_i64()
        ),
        (Some(1), Some(1200), Some(1000))
    );
    assert_eq!(
        (t["output_tokens"].as_i64(), t["reasoning_tokens"].as_i64()),
        (Some(300), Some(250))
    );
    assert_eq!(t["lge_removed_tokens"].as_i64(), Some(removed));
    let largest = t["largest_outputs"].as_array().unwrap();
    assert_eq!(largest.len(), 2, "{t}");
    assert_eq!(
        (largest[0]["command"].as_str(), largest[0]["shortened"].as_bool()),
        (Some("pytest -v"), Some(true))
    );
    assert_eq!(
        (largest[1]["command"].as_str(), largest[1]["shortened"].as_bool()),
        (Some("mcp__docs__lookup"), Some(false))
    );
    assert!(largest[0]["tokens"].as_u64().unwrap() > largest[1]["tokens"].as_u64().unwrap());
    let unknown = r.json("/lge/api/sessions/nobody/requests");
    assert_eq!(
        (unknown["known"].as_bool(), unknown["requests"].as_array().map(Vec::len)),
        (Some(false), Some(0))
    );
}

#[test]
fn each_trace_keeps_its_own_request_when_the_next_one_arrives_early() {
    // Codex sends the next request of a conversation as soon as the previous
    // answer is complete, while the proxy may still be relaying its end. Here
    // the first answer is slow, so the second request is rewritten before the
    // first one's trace is taken.
    let slow = provider_with(Box::new(|_, _, n| {
        if n == 0 {
            std::thread::sleep(Duration::from_millis(1500));
        }
        (200, stream_with(1000 + n as i64, 0))
    }));
    let r = start_with(slow, |_| {});
    let first = request_body(Path::new("/somewhere/else"), "conv-race", "run the tests");
    let mut second = first.clone();
    let input = second["input"].as_array_mut().unwrap();
    input.push(json!({ "type": "function_call", "name": "shell", "call_id": "c2", "arguments": json!({ "command": ["bash", "-lc", "ls"] }).to_string() }));
    input.push(json!({ "type": "function_call_output", "call_id": "c2", "output": "Exit code: 0\nWall time: 0.1 seconds\nOutput:\nREADME.md\n" }));
    std::thread::scope(|s| {
        let a = s.spawn(|| r.send(&first));
        std::thread::sleep(Duration::from_millis(400));
        let b = s.spawn(|| r.send(&second));
        assert_eq!(a.join().unwrap().0, 200);
        assert_eq!(b.join().unwrap().0, 200);
    });
    let traces = r.json("/lge/api/sessions/conv-race/requests");
    let mut got: Vec<(u64, i64)> = traces["requests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| (t["request"].as_u64().unwrap(), t["input_tokens"].as_i64().unwrap()))
        .collect();
    got.sort();
    assert_eq!(got, vec![(1, 1000), (2, 1001)], "{traces}");
}

// ------------------------------------------------- restarts, packs, settings, origin

fn send_to(agent: &ureq::Agent, addr: std::net::SocketAddr, body: &Value) {
    let response = agent
        .post(format!("http://{addr}/v1/responses"))
        .header("authorization", "Bearer k")
        .send(serde_json::to_vec(body).unwrap())
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    response.into_body().read_to_vec().unwrap();
}

fn status_of(agent: &ureq::Agent, addr: std::net::SocketAddr) -> Value {
    let answer = agent
        .get(format!("http://{addr}/lge/api/status"))
        .call()
        .unwrap()
        .into_body()
        .read_to_vec()
        .unwrap();
    serde_json::from_slice(&answer).unwrap()
}

/// The check rule's (items, judged) on the status page.
fn check_items(status: &Value) -> (u64, u64) {
    let rules = status["rules"].as_array().unwrap();
    let check = rules.iter().find(|x| x["id"] == "check").unwrap();
    (
        check["stats"]["items"].as_u64().unwrap(),
        check["stats"]["judged"].as_u64().unwrap(),
    )
}

fn plain_agent() -> ureq::Agent {
    ureq::Agent::new_with_config(ureq::Agent::config_builder().http_status_as_error(false).build())
}

#[test]
fn a_restart_does_not_count_old_items_again() {
    let (port, received) = provider();
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let cfg = config(dir.path(), &work.canonicalize().unwrap().join("shop"), port);
    let agent = plain_agent();
    let mut body = request_body(Path::new("/somewhere/else"), "conv-dc", "run the tests");
    // Three more calls, so the pytest item is judged.
    for i in 2..=4 {
        push_call(&mut body, &format!("c{i}"), &format!("ls dir{i}"), "a\n");
    }
    let _slot = Slot::take();
    let before = {
        let first = Serving::start(Daemon::open(cfg.clone()).unwrap());
        send_to(&agent, first.addr, &body);
        body["input"].as_array_mut().unwrap().push(user("thanks"));
        send_to(&agent, first.addr, &body);
        check_items(&status_of(&agent, first.addr))
    };
    assert_eq!(before, (1, 1));
    for again in ["thanks again", "and again"] {
        let next = Serving::start(Daemon::open(cfg.clone()).unwrap());
        body["input"].as_array_mut().unwrap().push(user(again));
        send_to(&agent, next.addr, &body);
        assert_eq!(
            check_items(&status_of(&agent, next.addr)),
            before,
            "the one item is counted once"
        );
    }
    assert_eq!(received.lock().unwrap().len(), 4);
}

#[test]
fn a_restart_keeps_old_items_bytes_whatever_the_settings_say_now() {
    let (port, received) = provider();
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let cfg = config(dir.path(), &work.canonicalize().unwrap().join("shop"), port);
    let agent = plain_agent();
    let mut body = request_body(Path::new("/somewhere/else"), "conv-rs", "run the tests");
    let _slot = Slot::take();
    {
        let first = Serving::start(Daemon::open(cfg.clone()).unwrap());
        send_to(&agent, first.addr, &body);
        // Only new items follow the new mode.
        let r = agent
            .post(format!("http://{}/lge/api/settings", first.addr))
            .header("content-type", "application/json")
            .send(json!({ "rules": { "check": "shadow" } }).to_string())
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);
        body["input"].as_array_mut().unwrap().push(user("thanks"));
        send_to(&agent, first.addr, &body);
    }
    let second = Serving::start(Daemon::open(cfg).unwrap());
    body["input"].as_array_mut().unwrap().push(user("thanks again"));
    send_to(&agent, second.addr, &body);
    let sent: Vec<Value> = received
        .lock()
        .unwrap()
        .iter()
        .map(|(_, _, _, b)| serde_json::from_slice(b).unwrap())
        .collect();
    assert!(sent[0]["input"][3]["output"]
        .as_str()
        .unwrap()
        .contains("ok: 300 passed"));
    assert_eq!(sent[1]["input"][3], sent[0]["input"][3], "same daemon: replayed");
    assert_eq!(
        sent[2]["input"][3], sent[0]["input"][3],
        "after a restart: the same bytes"
    );
}

#[test]
fn a_pack_keeps_its_place_when_the_graph_goes_away() {
    let r = start();
    r.add_repo();
    let cwd = r.repo.join("orders_api");
    let mut body = request_body(&cwd, "conv-g", "create_order must validate the payload");
    r.send(&body);
    let first = r.sent().pop().unwrap();
    assert!(has_context(&first), "a pack was added");
    // Added again from the page: indexed again, so no graph for a while.
    r.api("/lge/api/repos", &json!({ "path": r.repo.to_string_lossy() }));
    body["input"].as_array_mut().unwrap().push(user("thanks"));
    r.send(&body);
    let second = r.sent().pop().unwrap();
    assert_eq!(second["input"][2], first["input"][2]);
    r.wait_ready();
    let response = r
        .agent
        .run(
            ureq::http::Request::builder()
                .method("DELETE")
                .uri(format!("{}/lge/api/repos", r.base))
                .header("content-type", "application/json")
                .body(json!({ "path": r.repo.to_string_lossy() }).to_string().into_bytes())
                .unwrap(),
        )
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    body["input"].as_array_mut().unwrap().push(user("thanks again"));
    r.send(&body);
    let third = r.sent().pop().unwrap();
    assert_eq!(third["input"][2], first["input"][2], "the old message keeps its pack");
}

#[test]
fn a_message_seen_before_the_graph_never_gets_a_pack_later() {
    let r = start();
    let cwd = r.repo.join("orders_api");
    let mut body = request_body(&cwd, "conv-late", "create_order must validate the payload");
    r.send(&body);
    let first = r.sent().pop().unwrap();
    assert!(!has_context(&first), "no graph yet");
    r.add_repo();
    body["input"].as_array_mut().unwrap().push(user("thanks"));
    r.send(&body);
    let second = r.sent().pop().unwrap();
    assert!(!has_context(&second));
    assert_eq!(second["input"][2], first["input"][2], "the old prefix keeps its bytes");
}

#[test]
fn a_refused_pack_does_not_stop_a_later_one() {
    let r = start_with(
        provider_with(Box::new(|_, body, n| {
            if n == 0 && String::from_utf8_lossy(body).contains("<lge_context>") {
                (400, REFUSAL.to_string())
            } else {
                (200, STREAM.to_string())
            }
        })),
        |_| {},
    );
    r.add_repo();
    let cwd = r.repo.join("orders_api");
    let mut body = request_body(&cwd, "conv-x", "create_order must validate the payload");
    r.send(&body);
    let sent = r.sent();
    assert_eq!(sent.len(), 2, "refused, then retried");
    assert!(has_context(&sent[0]) && !has_context(&sent[1]));
    // No pack about this code reached the model: the next message about it gets one.
    body["input"]
        .as_array_mut()
        .unwrap()
        .push(user("create_order should validate the payload fields too"));
    r.send(&body);
    assert!(has_context(&r.sent().pop().unwrap()));
}

#[test]
fn unreadable_settings_stay_in_the_file_until_saved_from_the_page() {
    let (port, _received) = provider();
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("work/shop");
    std::fs::create_dir_all(&repo).unwrap();
    let repo = repo.canonicalize().unwrap();
    let cfg = config(dir.path(), &repo, port);
    std::fs::create_dir_all(&cfg.data_dir).unwrap();
    let file = cfg.data_dir.join("daemon.json");
    let settings = json!({ "shorten_outputs": false, "inject_context": false, "rules": { "check": "often" } });
    std::fs::write(
        &file,
        json!({ "repos": [repo.to_string_lossy()], "settings": settings }).to_string(),
    )
    .unwrap();
    let agent = plain_agent();
    let _slot = Slot::take();
    {
        let _first = Serving::start(Daemon::open(cfg.clone()).unwrap());
    }
    let kept: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(
        kept["settings"], settings,
        "the user's settings are not replaced by the defaults"
    );
    assert_eq!(kept["repos"], json!([repo.to_string_lossy()]));
    let second = Serving::start(Daemon::open(cfg).unwrap());
    let status = status_of(&agent, second.addr);
    assert!(!status["settings_error"].is_null(), "the user is still told");
    // Saving from the page ends it.
    let r = agent
        .post(format!("http://{}/lge/api/settings", second.addr))
        .header("content-type", "application/json")
        .send(json!({ "inject_context": true }).to_string())
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(saved["settings"]["inject_context"], true);
    assert!(status_of(&agent, second.addr)["settings_error"].is_null());
}

#[test]
fn only_this_daemons_own_pages_may_drive_it() {
    let r = start();
    let change = br#"{"holdout_share":1.0,"shorten_outputs":false}"#;
    for origin in [
        "http://localhost.attacker.example",
        "http://127.0.0.1.nip.io",
        "http://localhost:1@evil.example",
        "https://evil.example",
    ] {
        let (status, _, _) = r.post(
            "/lge/api/settings",
            change,
            &[("origin", origin), ("content-type", "application/json")],
        );
        assert_eq!(status, 403, "{origin}");
    }
    // A form or a text/plain POST needs no preflight in a browser: refused.
    let (status, _, _) = r.post("/lge/api/settings", change, &[("content-type", "text/plain")]);
    assert_eq!(status, 403);
    let s = r.json("/lge/api/status");
    assert_eq!(
        (
            s["settings"]["holdout_share"].as_f64(),
            s["settings"]["shorten_outputs"].as_bool()
        ),
        (Some(0.0), Some(true))
    );
    // DNS rebinding: a page under another name that resolves here.
    let response = r
        .agent
        .run(
            ureq::http::Request::builder()
                .method("GET")
                .uri(format!("{}/lge/api/status", r.base))
                .header("host", "rebind.attacker.example")
                .body(())
                .unwrap(),
        )
        .unwrap();
    assert_eq!(response.status().as_u16(), 403);
    // The daemon's own page, under any loopback name, still works.
    for origin in [r.base.clone(), r.base.replace("127.0.0.1", "localhost")] {
        let (status, _, _) = r.post(
            "/lge/api/settings",
            br#"{"inject_context":true}"#,
            &[
                ("origin", origin.as_str()),
                ("content-type", "application/json; charset=utf-8"),
            ],
        );
        assert_eq!(status, 200, "{origin}");
    }
}
