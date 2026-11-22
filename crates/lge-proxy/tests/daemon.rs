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
    // The fixture is not a git repository, where rg reads `.ignore` but not `.gitignore`.
    assert!(
        bulky[0]["ignore_file"].as_str().unwrap().ends_with("/.ignore"),
        "{status}"
    );
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
    assert_eq!(
        settings["pack_conversation_budget"], 3000,
        "a missing key takes its default"
    );
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
fn a_tool_called_from_inside_an_exec_script_is_not_listed_as_never_called() {
    let r = start();
    let mut body = request_body(Path::new("/somewhere/else"), "conv-nested", "reset the browser");
    body["tools"] = json!([
        { "type": "function", "name": "view_image", "description": "Shows an image.", "parameters": { "type": "object" } },
        { "type": "namespace", "name": "mcp__cua_repl__", "description": "Computer use.", "tools": [
            { "type": "function", "name": "js_reset", "description": "Resets.", "parameters": { "type": "object" } },
            { "type": "function", "name": "js", "description": "Runs.", "parameters": { "type": "object" } }
        ] },
        { "type": "namespace", "name": "clock", "description": "Time.", "tools": [
            { "type": "function", "name": "curr_time", "description": "The time.", "parameters": { "type": "object" } }
        ] }
    ]);
    let ghosts = |r: &Running| -> Vec<String> {
        r.json("/lge/api/status")["ghost_tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|g| g["name"].as_str().unwrap().to_string())
            .collect()
    };
    r.send(&body);
    let before = ghosts(&r);
    assert!(
        ["mcp__cua_repl__js_reset", "clockcurr_time", "view_image"]
            .iter()
            .all(|n| before.iter().any(|g| g == n)),
        "{before:?}"
    );
    let items = body["input"].as_array_mut().unwrap();
    items.push(json!({ "type": "custom_tool_call", "call_id": "s1", "name": "exec",
        "input": "await tools.mcp__cua_repl__js_reset({});\nconst t = await tools.clock__curr_time();\ntext(String(t));" }));
    items.push(json!({ "type": "custom_tool_call_output", "call_id": "s1", "output": "ok" }));
    r.send(&body);
    let after = ghosts(&r);
    assert!(
        !after
            .iter()
            .any(|g| g == "mcp__cua_repl__js_reset" || g == "clockcurr_time"),
        "{after:?}"
    );
    assert!(after.iter().any(|g| g == "mcp__cua_repl__js"), "{after:?}");
    assert!(after.iter().any(|g| g == "view_image"), "{after:?}");
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
    // Started while nobody was held out: not placed in an arm at random.
    assert_eq!(status["holdout"]["proxy"]["sessions"], 0);
    assert_eq!(status["holdout"]["left_out"], 1);
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

// ------------------------------------------------- background requests, pack cost, long sessions

const ABOUT_ROUTES: &str = "create_order must validate the payload";
const ABOUT_AUDIT: &str = "record the delivery of the email in the audit log";
const ABOUT_MAILER: &str = "sendEmail should retry on a timeout";

/// The desktop app's prompt for a task's title, with the task after it.
fn title_prompt(task: &str) -> String {
    format!("You are a helpful assistant. You will be presented with a user prompt, and your job is to provide a short title for a task that will be created from that prompt.\n\nThe user prompt:\n{task}")
}

impl Running {
    /// A Responses request with `headers` added; what the provider received.
    fn send_with(&self, body: &Value, headers: &[(&str, &str)]) -> Value {
        let mut all = vec![("authorization", "Bearer k"), ("content-type", "application/json")];
        all.extend_from_slice(headers);
        let (status, _, _) = self.post("/v1/responses", &serde_json::to_vec(body).unwrap(), &all);
        assert_eq!(status, 200);
        serde_json::from_slice(&self.last().3).unwrap()
    }
}

/// The status row of the session `id`.
fn session_row(status: &Value, id: &str) -> Value {
    status["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == id)
        .unwrap_or_else(|| panic!("no session {id}: {status}"))
        .clone()
}

#[test]
fn codex_background_requests_get_no_pack_and_are_counted_by_label() {
    let r = start();
    r.add_repo();
    let cwd = r.repo.join("orders_api");
    // Every new conversation would be held out; a background one never is.
    r.api("/lge/api/settings", &json!({ "holdout_share": 1.0 }));
    let shortened = |sent: &Value| sent["input"][3]["output"].as_str().unwrap().contains("Output:\nok");

    // Codex's sub-agent header.
    let guardian = r.send_with(
        &request_body(&cwd, "bg-header", ABOUT_ROUTES),
        &[("x-openai-subagent", "guardian")],
    );
    assert!(!has_context(&guardian), "no pack: {guardian}");
    assert!(shortened(&guardian), "the rules apply as usual: {guardian}");
    // The same key in the body's client metadata.
    let mut metadata = request_body(&cwd, "bg-meta", ABOUT_ROUTES);
    metadata["client_metadata"] = json!({ "x-openai-subagent": "review" });
    let review = r.send_with(&metadata, &[]);
    assert!(!has_context(&review) && shortened(&review), "{review}");
    // The approval reviewer's model.
    let mut model = request_body(&cwd, "bg-model", ABOUT_ROUTES);
    model["model"] = json!("codex-auto-review");
    let reviewer = r.send_with(&model, &[]);
    assert!(!has_context(&reviewer) && shortened(&reviewer), "{reviewer}");
    // The desktop app's title prompt.
    let titled = r.send_with(&request_body(&cwd, "bg-title", &title_prompt(ABOUT_ROUTES)), &[]);
    assert!(!has_context(&titled) && shortened(&titled), "{titled}");
    // A later request of a background session gets none either.
    let mut next = request_body(&cwd, "bg-header", ABOUT_ROUTES);
    next["input"].as_array_mut().unwrap().push(user(ABOUT_AUDIT));
    let later = r.send_with(&next, &[("x-openai-subagent", "guardian")]);
    assert!(!has_context(&later), "{later}");

    // A spawned agent does real work: a conversation like any other.
    r.api("/lge/api/settings", &json!({ "holdout_share": 0.0 }));
    let spawned = r.send_with(
        &request_body(&cwd, "spawned", ABOUT_ROUTES),
        &[("x-openai-subagent", "collab_spawn")],
    );
    assert!(has_context(&spawned), "{spawned}");

    let status = r.json("/lge/api/status");
    for (id, label) in [
        ("bg-header", "guardian"),
        ("bg-meta", "review"),
        ("bg-model", "guardian"),
        ("bg-title", "title"),
    ] {
        let s = session_row(&status, id);
        assert_eq!(
            (s["mode"].as_str(), s["background"].as_str()),
            (Some("proxy-background"), Some(label)),
            "{s}"
        );
        assert_eq!(s["outputs_shortened"], 1, "{s}");
        assert_eq!(s["packs_added"], 0, "{s}");
    }
    assert_eq!(
        session_row(&status, "bg-header")["input_tokens"],
        2400,
        "the usage is recorded"
    );
    let spawned = session_row(&status, "spawned");
    assert_eq!(
        (spawned["mode"].as_str(), spawned["background"].as_str()),
        (Some("proxy"), None)
    );
    let by_label: Vec<(&str, u64, u64)> = status["background"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| {
            (
                b["label"].as_str().unwrap(),
                b["sessions"].as_u64().unwrap(),
                b["requests"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        by_label,
        [("guardian", 2, 3), ("review", 1, 1), ("title", 1, 1)],
        "{}",
        status["background"]
    );
    // Left out of the holdout comparison and the before-and-after numbers.
    // The spawned agent's session started with no share held out: it is
    // left out of the arms too, and counted as such.
    assert_eq!(status["holdout"]["proxy"]["sessions"], 0);
    assert_eq!(status["holdout"]["holdout"]["sessions"], 0);
    assert_eq!(status["holdout"]["left_out"], 1, "{}", status["holdout"]);
    let modes: Vec<&str> = status["modes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["mode"].as_str().unwrap())
        .collect();
    assert_eq!(modes, ["proxy"]);
    assert_eq!(status["packs"]["added"], 1);
    let page = String::from_utf8(r.get("/").1).unwrap();
    assert!(page.contains("x.background") && page.contains("s.background"));
}

#[test]
fn the_pack_budget_is_a_setting_that_bounds_what_packs_add() {
    let r = start();
    assert_eq!(Settings::default().pack_conversation_budget, 3000);
    let status = r.json("/lge/api/status");
    assert_eq!(status["settings"]["pack_conversation_budget"], 3000);
    assert_eq!(status["long_sessions"]["note"], Value::Null, "no long session, no note");
    let saved = r.api("/lge/api/settings", &json!({ "pack_conversation_budget": 250 }));
    assert_eq!(saved["pack_conversation_budget"], 250);
    let file: Value =
        serde_json::from_str(&std::fs::read_to_string(r.dir.path().join("data/daemon.json")).unwrap()).unwrap();
    assert_eq!(file["settings"]["pack_conversation_budget"], 250);
    r.add_repo();
    let sent = r.send_with(
        &request_body(&r.repo.join("orders_api"), "conv-budget", ABOUT_ROUTES),
        &[],
    );
    assert!(!has_context(&sent), "no room for a pack: {sent}");
    let status = r.json("/lge/api/status");
    assert_eq!(
        (
            status["packs"]["added"].as_u64(),
            status["packs"]["over_budget"].as_u64()
        ),
        (Some(0), Some(1)),
        "{}",
        status["packs"]
    );
    assert_eq!(session_row(&status, "conv-budget")["packs_over_budget"], 1);
    let page = String::from_utf8(r.get("/").1).unwrap();
    assert!(page.contains("pack_conversation_budget"));
}

#[test]
fn a_compaction_retires_old_packs_and_the_monitor_calls_it_the_clients_break() {
    let r = start();
    r.add_repo();
    let cwd = r.repo.join("orders_api");
    let environment = user(&format!(
        "<environment_context>\n  <cwd>{}</cwd>\n</environment_context>",
        cwd.display()
    ));
    let body = |items: &[Value]| json!({ "model": "gpt-test", "prompt_cache_key": "conv-compact", "input": items, "stream": true });
    let call = |id: &str| {
        [
            json!({ "type": "function_call", "name": "shell", "call_id": id, "arguments": json!({ "command": ["bash", "-lc", "git status"] }).to_string() }),
            json!({ "type": "function_call_output", "call_id": id, "output": "Exit code: 0\nWall time: 0.1 seconds\nOutput:\nOn branch main\n" }),
        ]
    };
    let forwarded = |sent: &Value| sent["input"].as_array().unwrap().clone();

    // Two turns: a pack, a follow-up pack.
    let mut items = vec![environment.clone(), user(ABOUT_ROUTES)];
    let first = forwarded(&r.send_with(&body(&items), &[]));
    items.extend(call("c1"));
    items.push(user(ABOUT_AUDIT));
    let second = forwarded(&r.send_with(&body(&items), &[]));
    assert_eq!(second[..first.len()], first[..], "the cached prefix must not change");
    let audit_pack = second.last().unwrap().clone();
    assert!(has_context(&audit_pack));

    // Codex compacts in the middle of the turn (codex-rs/core/src/compact.rs):
    // the user messages it keeps, the initial context before the last of them,
    // and its summary. The first pack goes, the newest message keeps its own.
    let mut items = vec![
        user(ABOUT_ROUTES),
        environment,
        user(ABOUT_AUDIT),
        user("Another language model started to solve this problem and produced a summary of its thinking process. Here is the summary:\nhalf done."),
    ];
    let fourth = forwarded(&r.send_with(&body(&items), &[]));
    assert_eq!(fourth[..3], items[..3]);
    assert_eq!(fourth[3], audit_pack, "the newest message's pack, the same bytes");
    assert_eq!(fourth.len(), items.len() + 1);

    // Two more turns: every byte sent before is sent again.
    items.extend(call("c3"));
    items.push(user("ok, do it"));
    let fifth = forwarded(&r.send_with(&body(&items), &[]));
    assert_eq!(fifth[..fourth.len()], fourth[..], "the cached prefix must not change");
    items.push(user(ABOUT_MAILER));
    let sixth = forwarded(&r.send_with(&body(&items), &[]));
    assert_eq!(sixth[..fifth.len()], fifth[..], "the cached prefix must not change");
    assert!(
        has_context(sixth.last().unwrap()),
        "a pack about a file not seen before"
    );

    let status = r.json("/lge/api/status");
    assert_eq!(
        (
            status["prefix"]["lge_breaks"].as_u64(),
            status["prefix"]["client_breaks"].as_u64()
        ),
        (Some(0), Some(1)),
        "the compaction is the client's break: {}",
        status["prefix"]
    );
    let row = session_row(&status, "conv-compact");
    assert_eq!(
        (row["packs_added"].as_u64(), row["packs_retired"].as_u64()),
        (Some(3), Some(1)),
        "{row}"
    );
    assert_eq!(status["packs"]["retired"], 1);
}

#[test]
fn long_sessions_are_named_in_a_note_under_the_table() {
    let r = start_with(provider(), |data| {
        let store = Store::open(&data.join("usage.lbug")).unwrap();
        let now = lge_retrieve::unix_now();
        let record = |id: &str, mode: &str, requests: u64, mean: i64| UsageRecord {
            id: id.to_string(),
            task_id: id.to_string(),
            session_id: id.to_string(),
            task: format!("task {id}"),
            mode: mode.to_string(),
            started_at: now - 600,
            finished_at: now - 60,
            input_tokens: mean * requests as i64,
            monitor: json!({ "requests": requests }).to_string(),
            ..UsageRecord::default()
        };
        for rec in [
            record("long", "proxy", 50, 150_001),
            record("at-the-limit", "proxy", 50, 150_000),
            record("few-requests", "proxy", 49, 400_000),
            record("background", "proxy-background", 80, 300_000),
        ] {
            store.put_usage(&rec).unwrap();
        }
    });
    let status = r.json("/lge/api/status");
    let long = &status["long_sessions"];
    let named: Vec<&str> = long["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert_eq!(named, ["long"], "{long}");
    assert_eq!(long["sessions"][0]["mean_input_tokens"], 150_001);
    assert_eq!(
        (long["min_requests"].as_u64(), long["min_mean_input_tokens"].as_u64()),
        (Some(50), Some(150_000))
    );
    let note = long["note"].as_str().expect("a note");
    assert!(
        note.contains("model_auto_compact_token_limit") && note.contains("~/.codex/config.toml"),
        "{note}"
    );
    // Every row shows its requests and the mean input per request.
    let few = session_row(&status, "few-requests");
    assert_eq!(
        (few["requests"].as_u64(), few["mean_input_tokens"].as_i64()),
        (Some(49), Some(400_000))
    );
    let page = String::from_utf8(r.get("/").1).unwrap();
    assert!(page.contains("long_sessions") && page.contains("mean_input_tokens"));
}

#[test]
fn codex_temporary_threads_are_background_by_their_thread_source() {
    let r = start();
    r.add_repo();
    let cwd = r.repo.join("orders_api");
    // The TUI's task title (codex-rs/tui/src/app/thread_title.rs): its own
    // prompt, no sub-agent header, the thread source in the turn metadata header.
    let prompt =
        format!("Generate a concise, single-line task title of at most 36 characters.\n\nUser prompt:\n{ABOUT_ROUTES}");
    let title = r.send_with(
        &request_body(&cwd, "tui-title", &prompt),
        &[(
            "x-codex-turn-metadata",
            r#"{"turn_id":"t1","thread_source":"thread_title"}"#,
        )],
    );
    assert!(!has_context(&title), "no pack: {title}");
    // A recap (thread source `system`), named in the body's client metadata.
    let mut recap = request_body(&cwd, "tui-recap", ABOUT_ROUTES);
    recap["client_metadata"] = json!({ "x-codex-turn-metadata": r#"{"thread_source":"system"}"# });
    let recap = r.send_with(&recap, &[]);
    assert!(!has_context(&recap), "no pack: {recap}");
    // A thread someone works in.
    let mut work = request_body(&cwd, "tui-work", ABOUT_ROUTES);
    work["client_metadata"] = json!({ "x-codex-turn-metadata": r#"{"thread_source":"user"}"# });
    assert!(has_context(&r.send_with(&work, &[])));

    let status = r.json("/lge/api/status");
    for (id, mode, label) in [
        ("tui-title", "proxy-background", Some("title")),
        ("tui-recap", "proxy-background", Some("recap")),
        ("tui-work", "proxy", None),
    ] {
        let s = session_row(&status, id);
        assert_eq!(
            (s["mode"].as_str(), s["background"].as_str()),
            (Some(mode), label),
            "{s}"
        );
    }
}

#[test]
fn past_background_threads_are_left_out_of_before_and_after() {
    let (port, _received) = provider();
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let home = dir.path().join("codex");
    let day = home.join("sessions/2026/11/01");
    std::fs::create_dir_all(&day).unwrap();
    // Rollouts as Codex writes them: session_meta (codex-rs/protocol SessionMeta), then usage.
    let rollout = |id: &str, meta: Value| {
        let mut payload = json!({ "id": id });
        payload
            .as_object_mut()
            .unwrap()
            .extend(meta.as_object().unwrap().clone());
        format!(
            "{}\n{}\n",
            json!({ "type": "session_meta", "payload": payload }),
            json!({ "type": "event_msg", "payload": { "type": "token_count", "info": { "total_token_usage": { "input_tokens": 9000, "cached_input_tokens": 0, "output_tokens": 10, "reasoning_output_tokens": 1 } } } })
        )
    };
    for (n, (id, meta)) in [
        ("main-1", json!({ "source": "cli" })),
        ("spawned-1", json!({ "source": { "subagent": { "thread_spawn": { "parent_thread_id": "main-1", "depth": 1 } } }, "thread_source": "subagent" })),
        ("guard-1", json!({ "source": { "subagent": { "other": "guardian" } } })),
        ("review-1", json!({ "source": { "subagent": "review" } })),
        ("memory-1", json!({ "source": "cli", "thread_source": "memory_consolidation" })),
    ]
    .into_iter()
    .enumerate()
    {
        std::fs::write(
            day.join(format!("rollout-2026-11-01T00-00-0{n}-{id}.jsonl")),
            rollout(id, meta),
        )
        .unwrap();
    }
    let mut cfg = config(dir.path(), &work.canonicalize().unwrap().join("shop"), port);
    cfg.codex_home = Some(home);
    let _slot = Slot::take();
    let s = Serving::start(Daemon::open(cfg).unwrap());
    let agent = plain_agent();
    let imported: Value = serde_json::from_slice(
        &agent
            .post(format!("http://{}/lge/api/import-history", s.addr))
            .header("content-type", "application/json")
            .send("{}")
            .unwrap()
            .into_body()
            .read_to_vec()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        (imported["imported"].as_u64(), imported["background"].as_u64()),
        (Some(2), Some(3)),
        "{imported}"
    );
    // The approval reviewer through the proxy is left out the same way.
    let mut body = request_body(Path::new("/somewhere/else"), "guard-2", "review this command");
    body["model"] = json!("codex-auto-review");
    send_to(&agent, s.addr, &body);
    let status = status_of(&agent, s.addr);
    let modes: Vec<(&str, u64)> = status["modes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["mode"].as_str().unwrap(), m["sessions"].as_u64().unwrap()))
        .collect();
    assert_eq!(modes, [("history", 2)], "{}", status["modes"]);
}

#[test]
fn compressed_rollouts_are_imported_and_a_corrupt_one_is_counted_as_unreadable() {
    let (port, _received) = provider();
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let home = dir.path().join("codex");
    let day = home.join("sessions/2026/07/20");
    std::fs::create_dir_all(&day).unwrap();
    let rollout = |id: &str| {
        format!(
            "{}\n{}\n",
            json!({ "type": "session_meta", "payload": { "id": id, "source": "cli" } }),
            json!({ "type": "event_msg", "payload": { "type": "token_count", "info": { "total_token_usage": { "input_tokens": 9000, "cached_input_tokens": 0, "output_tokens": 10, "reasoning_output_tokens": 1 } } } })
        )
    };
    let packed =
        ruzstd::encoding::compress_to_vec(rollout("old-1").as_bytes(), ruzstd::encoding::CompressionLevel::Fastest);
    std::fs::write(day.join("rollout-2026-07-20T00-00-00-old-1.jsonl.zst"), packed).unwrap();
    std::fs::write(
        day.join("rollout-2026-07-20T00-00-01-old-2.jsonl.zst"),
        b"not zstd at all",
    )
    .unwrap();
    std::fs::write(day.join("rollout-2026-07-20T00-00-02-new-1.jsonl"), rollout("new-1")).unwrap();
    let mut cfg = config(dir.path(), &work.canonicalize().unwrap().join("shop"), port);
    cfg.codex_home = Some(home);
    let _slot = Slot::take();
    let s = Serving::start(Daemon::open(cfg).unwrap());
    let imported: Value = serde_json::from_slice(
        &plain_agent()
            .post(format!("http://{}/lge/api/import-history", s.addr))
            .header("content-type", "application/json")
            .send("{}")
            .unwrap()
            .into_body()
            .read_to_vec()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        (imported["imported"].as_u64(), imported["unreadable"].as_u64()),
        (Some(2), Some(1)),
        "{imported}"
    );
}

#[test]
fn held_out_sessions_are_not_counted_in_the_pack_totals() {
    let r = start();
    r.add_repo();
    let cwd = r.repo.join("orders_api");
    r.api("/lge/api/settings", &json!({ "holdout_share": 1.0 }));
    let mut body = request_body(&cwd, "held-out", ABOUT_ROUTES);
    assert!(!has_context(&r.send_with(&body, &[])));
    body["input"]
        .as_array_mut()
        .unwrap()
        .push(user("create_order must also validate the payload size"));
    assert!(!has_context(&r.send_with(&body, &[])));
    let status = r.json("/lge/api/status");
    let packs = &status["packs"];
    assert_eq!(
        (
            packs["added"].as_u64(),
            packs["known"].as_u64(),
            packs["over_budget"].as_u64(),
            packs["holdout_would_add"].as_u64()
        ),
        (Some(0), Some(0), Some(0), Some(1)),
        "{packs}"
    );
    assert_eq!(session_row(&status, "held-out")["mode"], "proxy-holdout");
}

#[test]
fn a_background_session_from_before_its_label_leaves_the_holdout_arm() {
    let (port, _received) = provider();
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let cfg = config(dir.path(), &work.canonicalize().unwrap().join("shop"), port);
    let agent = plain_agent();
    let mut body = request_body(Path::new("/somewhere/else"), "old-guard", "review this command");
    let _slot = Slot::take();
    // Served by a daemon that did not tell background requests apart: a
    // held-out session without a label and without a pack.
    {
        let first = Serving::start(Daemon::open(cfg.clone()).unwrap());
        let settings = json!({ "holdout_share": 1.0 }).to_string();
        let response = agent
            .post(format!("http://{}/lge/api/settings", first.addr))
            .header("content-type", "application/json")
            .send(settings)
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        send_to(&agent, first.addr, &body);
        let status = status_of(&agent, first.addr);
        assert_eq!(session_row(&status, "old-guard")["mode"], "proxy-holdout");
    }
    // After the restart its next request is the reviewer's.
    let second = Serving::start(Daemon::open(cfg).unwrap());
    body["input"].as_array_mut().unwrap().push(user("and this one"));
    body["model"] = json!("codex-auto-review");
    send_to(&agent, second.addr, &body);
    let status = status_of(&agent, second.addr);
    let row = session_row(&status, "old-guard");
    assert_eq!(
        (row["mode"].as_str(), row["background"].as_str()),
        (Some("proxy-background"), Some("guardian")),
        "{row}"
    );
    assert_eq!(status["holdout"]["holdout"]["sessions"], 0, "{}", status["holdout"]);
}

#[test]
fn the_long_session_note_counts_requests_as_the_table_does() {
    // The provider fails the request: no usage, so the stored record keeps 49 requests.
    let failing = provider_with(Box::new(|_, _, _| {
        (500, r#"{"error":{"type":"server_error","message":"boom"}}"#.to_string())
    }));
    let r = start_with(failing, |data| {
        let store = Store::open(&data.join("usage.lbug")).unwrap();
        let now = lge_retrieve::unix_now();
        store
            .put_usage(&UsageRecord {
                id: lge_core::stable_id(&["session", "long-thread"]),
                task_id: "long-thread".to_string(),
                session_id: "long-thread".to_string(),
                task: "a long task".to_string(),
                mode: "proxy".to_string(),
                started_at: now - 600,
                finished_at: now - 60,
                input_tokens: 50 * 200_000,
                monitor: json!({ "requests": 49 }).to_string(),
                ..UsageRecord::default()
            })
            .unwrap();
    });
    let body = request_body(Path::new("/somewhere/else"), "long-thread", "run the tests");
    let (status, _, _) = r.post(
        "/v1/responses",
        &serde_json::to_vec(&body).unwrap(),
        &[("authorization", "Bearer k"), ("thread-id", "long-thread")],
    );
    assert_eq!(status, 500);
    let status = r.json("/lge/api/status");
    let row = session_row(&status, "long-thread");
    assert_eq!(
        (row["requests"].as_u64(), row["mean_input_tokens"].as_i64()),
        (Some(50), Some(200_000))
    );
    let named: Vec<(&str, u64)> = status["long_sessions"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["id"].as_str().unwrap(), s["requests"].as_u64().unwrap()))
        .collect();
    assert_eq!(named, [("long-thread", 50)], "{}", status["long_sessions"]);
    let note = status["long_sessions"]["note"].as_str().unwrap();
    assert!(!note.contains('`'), "the page shows the note as text: {note}");
}

#[test]
fn the_long_session_note_names_an_old_long_session_below_sixty_newer_background_sessions() {
    let r = start_with(provider(), |data| {
        let store = Store::open(&data.join("usage.lbug")).unwrap();
        let now = lge_retrieve::unix_now();
        store
            .put_usage(&UsageRecord {
                id: lge_core::stable_id(&["session", "old-long"]),
                task_id: "old-long".to_string(),
                session_id: "old-long".to_string(),
                task: "a long task".to_string(),
                mode: "proxy".to_string(),
                started_at: now - 200_000,
                finished_at: now - 129_600,
                input_tokens: 60 * 200_000,
                monitor: json!({ "requests": 60 }).to_string(),
                ..UsageRecord::default()
            })
            .unwrap();
        for n in 0..60 {
            store
                .put_usage(&UsageRecord {
                    id: lge_core::stable_id(&["session", &format!("review-{n}")]),
                    task_id: format!("review-{n}"),
                    session_id: format!("review-{n}"),
                    task: "review an action".to_string(),
                    model: "codex-auto-review".to_string(),
                    mode: "proxy-background".to_string(),
                    started_at: now - 3_000 + n,
                    finished_at: now - 2_900 + n,
                    input_tokens: 30_000,
                    monitor: json!({ "requests": 1, "background": "guardian" }).to_string(),
                    ..UsageRecord::default()
                })
                .unwrap();
        }
    });
    let status = r.json("/lge/api/status");
    let named: Vec<&str> = status["long_sessions"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert_eq!(named, ["old-long"], "{}", status["long_sessions"]);
    let row = session_row(&status, "old-long");
    assert_eq!(row["requests"].as_u64(), Some(60));
    let rows = status["sessions"].as_array().unwrap();
    let background = rows.iter().filter(|s| s["mode"] == "proxy-background").count();
    assert_eq!(background, 10, "background sessions have a window of their own");
    assert!(status["prefix"]["conversations"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["id"] == "old-long"));
}

#[test]
fn the_cards_say_what_they_count_since_when_and_a_restart_keeps_the_stored_ones() {
    let before = lge_retrieve::unix_now();
    let r = start_with(provider(), |data| {
        let store = Store::open(&data.join("usage.lbug")).unwrap();
        let now = lge_retrieve::unix_now();
        store
            .put_usage(&UsageRecord {
                id: lge_core::stable_id(&["session", "earlier"]),
                task_id: "earlier".to_string(),
                session_id: "earlier".to_string(),
                task: "an earlier task".to_string(),
                mode: "proxy".to_string(),
                started_at: now - 600,
                finished_at: now - 60,
                input_tokens: 400_000,
                removed_tokens_sent: 120_000,
                added_tokens_sent: 3_000,
                monitor: json!({ "requests": 10 }).to_string(),
                ..UsageRecord::default()
            })
            .unwrap();
    });
    let status = r.json("/lge/api/status");
    // The live counts start at zero with the daemon; the stored ones stay.
    assert_eq!(status["saved"]["tokens_before"], 0);
    assert_eq!(status["annuity"]["removed_tokens_sent"], 120_000);
    assert_eq!(status["annuity"]["added_tokens_sent"], 3_000);
    let since = status["saved"]["since"]
        .as_i64()
        .expect("since when the live counts run");
    assert!(since >= before && since <= lge_retrieve::unix_now(), "{since}");
    // The page says so on both cards, and shows the stored totals.
    let page = String::from_utf8(r.get("/").1).unwrap();
    for card in ["'Removed from tool output'", "'Graph context added'"] {
        let line = page.lines().find(|l| l.contains(card)).expect(card);
        assert!(line.contains("since the daemon started"), "{line}");
    }
    assert!(page.contains("'Kept out of requests'") && page.contains("'Packs sent'"));
}

#[test]
fn the_request_for_a_compactions_summary_gets_no_new_pack_and_is_counted_apart() {
    let r = start();
    r.add_repo();
    let cwd = r.repo.join("orders_api");
    let mut body = request_body(&cwd, "conv-summary", ABOUT_ROUTES);
    let first = r.send_with(&body, &[]);
    assert!(has_context(&first));
    // Codex asks for the summary in the conversation itself
    // (codex-rs/core/src/compact.rs): the history and its prompt, with the
    // turn metadata's request kind, and no sub-agent header.
    body["input"].as_array_mut().unwrap().push(user(ABOUT_AUDIT));
    body["client_metadata"] = json!({ "x-codex-turn-metadata": r#"{"request_kind":"compaction"}"# });
    let summary = r.send_with(&body, &[]);
    let sent = summary["input"].as_array().unwrap();
    let before = first["input"].as_array().unwrap();
    assert_eq!(sent[..before.len()], before[..], "the cached prefix must not change");
    assert_eq!(sent.len(), before.len() + 1, "no new pack: {summary}");

    let status = r.json("/lge/api/status");
    let row = session_row(&status, "conv-summary");
    assert_eq!(
        (row["mode"].as_str(), row["background"].as_str()),
        (Some("proxy"), None)
    );
    let compaction: Vec<(Option<u64>, u64)> = status["background"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|b| b["label"] == "compaction")
        .map(|b| (b["sessions"].as_u64(), b["requests"].as_u64().unwrap()))
        .collect();
    assert_eq!(compaction, [(None, 1)], "{}", status["background"]);
}

// ---- router: lowering the reasoning effort --------------------------------------

/// A conversation's genuine first request: Codex's environment context and
/// one typed message, nothing of an earlier turn.
fn first_request(cwd: &Path, conversation: &str, typed: &str) -> Value {
    json!({
        "model": "gpt-test",
        "prompt_cache_key": conversation,
        "instructions": "You are a coding agent.",
        "input": [
            user(&format!("<environment_context>\n  <cwd>{}</cwd>\n</environment_context>", cwd.display())),
            user(typed),
        ],
        "stream": true,
    })
}

/// A function nothing calls: a change to it stays in one file.
fn add_one_file_function(repo: &Path) {
    std::fs::write(
        repo.join("orders_api/strings.py"),
        "def shout_text(text):\n    \"\"\"Upper-case a line of text.\"\"\"\n    return text.upper()\n",
    )
    .unwrap();
}

const ONE_FILE: &str = "shout_text should trim whitespace";

fn with_effort(mut body: Value, effort: &str) -> Value {
    body["reasoning"] = json!({ "effort": effort, "summary": "auto" });
    body
}

fn effort_sent(r: &Running) -> String {
    let sent: Value = serde_json::from_slice(&r.last().3).unwrap();
    sent["reasoning"]["effort"].as_str().unwrap_or("").to_string()
}

#[test]
fn the_router_lowers_the_effort_only_when_asked_and_never_raises_it() {
    let r = start();
    // A function nothing calls: a change to it stays in one file.
    std::fs::write(
        r.repo.join("orders_api/strings.py"),
        "def shout_text(text):\n    \"\"\"Upper-case a line of text.\"\"\"\n    return text.upper()\n",
    )
    .unwrap();
    r.add_repo();
    let cwd = r.repo.join("orders_api");
    let one_file = "shout_text should trim whitespace";
    let one_service = "audit log format";

    // Off by default: the recommendation is logged, the effort kept.
    assert_eq!(r.json("/lge/api/status")["settings"]["router_apply"], "off");
    r.send(&with_effort(first_request(&cwd, "conv-r0", one_file), "high"));
    assert_eq!(effort_sent(&r), "high");
    let row = session_row(&r.json("/lge/api/status"), "conv-r0");
    assert_eq!(
        (row["recommended_level"].as_str(), row["applied_level"].as_str()),
        (Some("low"), Some("")),
        "{row}"
    );

    // low-only: a one-file task is sent at low effort, on every request.
    assert_eq!(
        r.api("/lge/api/settings", &json!({ "router_apply": "low-only" }))["router_apply"],
        "low-only"
    );
    let mut first = with_effort(first_request(&cwd, "conv-r1", one_file), "high");
    r.send(&first);
    assert_eq!(effort_sent(&r), "low");
    let row = session_row(&r.json("/lge/api/status"), "conv-r1");
    assert_eq!(row["applied_level"], "low", "{row}");
    // A task in one service with several files keeps its effort.
    r.send(&with_effort(first_request(&cwd, "conv-r2", one_service), "high"));
    assert_eq!(effort_sent(&r), "high");

    // low-and-medium: the one-service task is sent at medium.
    r.api("/lge/api/settings", &json!({ "router_apply": "low-and-medium" }));
    r.send(&with_effort(first_request(&cwd, "conv-r3", one_service), "high"));
    assert_eq!(effort_sent(&r), "medium");
    // Never raised: a client already at low stays at low, and nothing is applied.
    r.send(&with_effort(first_request(&cwd, "conv-r4", one_service), "low"));
    assert_eq!(effort_sent(&r), "low");
    let row = session_row(&r.json("/lge/api/status"), "conv-r4");
    assert_eq!(row["applied_level"], "", "{row}");
    // A request that names no effort is left alone.
    r.send(&first_request(&cwd, "conv-r5", one_file));
    let sent: Value = serde_json::from_slice(&r.last().3).unwrap();
    assert!(sent.get("reasoning").is_none(), "{sent}");

    // Switched off later: a conversation keeps the effort it started with,
    // and a new one keeps the client's.
    r.api("/lge/api/settings", &json!({ "router_apply": "off" }));
    first["input"].as_array_mut().unwrap().push(user("ok, do it"));
    r.send(&first);
    assert_eq!(effort_sent(&r), "low", "the same effort for the whole conversation");
    r.send(&with_effort(first_request(&cwd, "conv-r6", one_file), "high"));
    assert_eq!(effort_sent(&r), "high");

    // Held out: the client's bytes, whatever the setting.
    r.api(
        "/lge/api/settings",
        &json!({ "router_apply": "low-only", "holdout_share": 1.0 }),
    );
    let held = with_effort(first_request(&cwd, "conv-r7", one_file), "high");
    let raw = serde_json::to_vec(&held).unwrap();
    r.post("/v1/responses", &raw, &[("authorization", "Bearer k")]);
    assert_eq!(r.last().3, raw, "byte-identical");

    // An unknown value is refused and changes nothing.
    let (status, _, _) = r.post(
        "/lge/api/settings",
        br#"{"router_apply":"always"}"#,
        &[("content-type", "application/json")],
    );
    assert_eq!(status, 400);
    assert_eq!(r.json("/lge/api/status")["settings"]["router_apply"], "low-only");
}

#[test]
fn an_applied_effort_holds_after_a_restart() {
    let (port, received) = provider();
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("work/shop");
    copy_dir(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/polyglot"),
        &repo,
    );
    std::fs::write(
        repo.join("orders_api/strings.py"),
        "def shout_text(text):\n    \"\"\"Upper-case a line of text.\"\"\"\n    return text.upper()\n",
    )
    .unwrap();
    let repo = repo.canonicalize().unwrap();
    std::fs::create_dir_all(dir.path().join("data")).unwrap();
    let cfg = config(dir.path(), &repo, port);
    let agent = plain_agent();
    let mut body = with_effort(
        first_request(&repo.join("orders_api"), "conv-ra", "shout_text should trim whitespace"),
        "high",
    );
    let effort_of_last = || -> String {
        let last = received.lock().unwrap().last().cloned().unwrap();
        let sent: Value = serde_json::from_slice(&last.3).unwrap();
        sent["reasoning"]["effort"].as_str().unwrap_or("").to_string()
    };

    let _slot = Slot::take();
    {
        let first = Serving::start(Daemon::open(cfg.clone()).unwrap());
        let post = |path: &str, v: &Value| {
            let response = agent
                .post(format!("http://{}{path}", first.addr))
                .header("content-type", "application/json")
                .send(v.to_string())
                .unwrap();
            assert_eq!(response.status().as_u16(), 200);
        };
        post("/lge/api/repos", &json!({ "path": repo.to_string_lossy() }));
        post("/lge/api/settings", &json!({ "router_apply": "low-only" }));
        let deadline = Instant::now() + Duration::from_secs(60);
        while status_of(&agent, first.addr)["repos"][0]["state"] != "ready" {
            assert!(Instant::now() < deadline, "repository not ready");
            std::thread::sleep(Duration::from_millis(100));
        }
        send_to(&agent, first.addr, &body);
        assert_eq!(effort_of_last(), "low");
    }
    // A new daemon: the conversation keeps its effort even with the setting
    // switched off in between, before its graph is ready again.
    let mut state: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("data/daemon.json")).unwrap()).unwrap();
    state["settings"]["router_apply"] = json!("off");
    std::fs::write(dir.path().join("data/daemon.json"), state.to_string()).unwrap();
    let second = Serving::start(Daemon::open(cfg).unwrap());
    body["input"].as_array_mut().unwrap().push(user("ok, do it"));
    send_to(&agent, second.addr, &body);
    assert_eq!(effort_of_last(), "low", "the same effort for the whole conversation");
}

#[test]
fn a_conversation_lge_first_sees_midway_keeps_its_effort() {
    let r = start();
    add_one_file_function(&r.repo);
    r.add_repo();
    r.api("/lge/api/settings", &json!({ "router_apply": "low-only" }));
    let cwd = r.repo.join("orders_api");

    // A thread lge has no record of, already under way (resumed, forked, or
    // started before lge was connected): its earlier requests went out at high.
    let mut midway = with_effort(first_request(&cwd, "conv-midway", ONE_FILE), "high");
    let items = midway["input"].as_array_mut().unwrap();
    items.push(
        json!({ "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": "Looking." }] }),
    );
    items.push(json!({ "type": "function_call", "name": "shell", "call_id": "c1", "arguments": json!({ "command": ["bash", "-lc", "git status"] }).to_string() }));
    items.push(json!({ "type": "function_call_output", "call_id": "c1", "output": "On branch main\n" }));
    r.send(&midway);
    assert_eq!(effort_sent(&r), "high");
    midway["input"].as_array_mut().unwrap().push(user("ok, do it"));
    r.send(&midway);
    assert_eq!(effort_sent(&r), "high", "the same effort for the whole conversation");
    let row = session_row(&r.json("/lge/api/status"), "conv-midway");
    assert_eq!(
        (row["recommended_level"].as_str(), row["applied_level"].as_str()),
        (Some("low"), Some("")),
        "{row}"
    );

    // The first request lge sees asks for a compaction's summary.
    let mut summary_first = with_effort(first_request(&cwd, "conv-summary-first", ONE_FILE), "high");
    summary_first["client_metadata"] = json!({ "x-codex-turn-metadata": r#"{"request_kind":"compaction"}"# });
    r.send(&summary_first);
    assert_eq!(effort_sent(&r), "high");
    summary_first.as_object_mut().unwrap().remove("client_metadata");
    summary_first["input"].as_array_mut().unwrap().push(user("go on"));
    r.send(&summary_first);
    assert_eq!(effort_sent(&r), "high");
    let row = session_row(&r.json("/lge/api/status"), "conv-summary-first");
    assert_eq!(row["applied_level"], "", "{row}");
}

#[test]
fn a_refused_lowered_effort_on_a_later_request_falls_back_to_the_clients_effort() {
    // Another model that takes no low effort.
    let refusing = provider_with(Box::new(|_, body, _| {
        let sent: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
        if sent["model"] == "gpt-other" && sent["reasoning"]["effort"] == "low" {
            let error = r#"{"error":{"message":"Unsupported value: 'low'.","type":"invalid_request_error","code":"unsupported_value"}}"#;
            (400, error.to_string())
        } else {
            (200, STREAM.to_string())
        }
    }));
    let r = start_with(refusing, |_| {});
    add_one_file_function(&r.repo);
    r.add_repo();
    r.api("/lge/api/settings", &json!({ "router_apply": "low-only" }));
    let mut body = with_effort(
        first_request(&r.repo.join("orders_api"), "conv-refused", ONE_FILE),
        "high",
    );
    r.send(&body);
    assert_eq!(effort_sent(&r), "low");

    // The thread moves to the other model.
    body["model"] = json!("gpt-other");
    body["input"].as_array_mut().unwrap().push(user("go on"));
    let (status, _) = r.send(&body);
    assert_eq!(status, 200, "sent again at the client's effort");
    assert_eq!(r.sent().len(), 3, "the first request, the refused one and its retry");
    assert_eq!(effort_sent(&r), "high");
    body["input"].as_array_mut().unwrap().push(user("and the rest"));
    let (status, _) = r.send(&body);
    assert_eq!(status, 200);
    assert_eq!(effort_sent(&r), "high", "the client's effort from now on");
    assert_eq!(r.sent().len(), 4);
    let row = session_row(&r.json("/lge/api/status"), "conv-refused");
    assert_eq!(
        (row["applied_level"].as_str(), row["raw_retries"].as_u64()),
        (Some(""), Some(1)),
        "{row}"
    );
}

#[test]
fn the_router_reads_the_whole_first_message() {
    let r = start();
    add_one_file_function(&r.repo);
    r.add_repo();
    r.api("/lge/api/settings", &json!({ "router_apply": "low-only" }));
    // The first 200 characters are about the one-file function; the rest
    // names code in another service.
    let filler = "Please keep the wording as it is and the tone as it was before. ".repeat(4);
    let task = format!("{ONE_FILE}. {filler}Then also change sendEmail in the notifier to match.");
    assert!(task.find("sendEmail").unwrap() > 200);
    r.send(&with_effort(
        first_request(&r.repo.join("orders_api"), "conv-whole", &task),
        "high",
    ));
    assert_eq!(effort_sent(&r), "high");
    let row = session_row(&r.json("/lge/api/status"), "conv-whole");
    assert_ne!(row["recommended_level"], "low", "{row}");
    assert_eq!(row["applied_level"], "", "{row}");
}

#[test]
fn the_compaction_request_of_an_applied_conversation_keeps_its_effort() {
    let r = start();
    add_one_file_function(&r.repo);
    r.add_repo();
    r.api("/lge/api/settings", &json!({ "router_apply": "low-only" }));
    let mut body = with_effort(
        first_request(&r.repo.join("orders_api"), "conv-applied", ONE_FILE),
        "high",
    );
    r.send(&body);
    assert_eq!(effort_sent(&r), "low");
    let before: Value = serde_json::from_slice(&r.last().3).unwrap();
    // Codex asks for the summary within the conversation, with the turn's settings.
    body["input"].as_array_mut().unwrap().push(user("ok, do it"));
    body["client_metadata"] = json!({ "x-codex-turn-metadata": r#"{"request_kind":"compaction"}"# });
    r.send(&body);
    assert_eq!(effort_sent(&r), "low", "the conversation's effort");
    let summary: Value = serde_json::from_slice(&r.last().3).unwrap();
    let (old, new) = (
        before["input"].as_array().unwrap(),
        summary["input"].as_array().unwrap(),
    );
    assert_eq!(new[..old.len()], old[..], "the cached prefix must not change");
}

#[test]
fn an_applied_effort_holds_after_a_restart_before_any_usage() {
    // The first answer is cut before its usage (the user interrupted it).
    let cut = "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n";
    let (port, received) = provider_with(Box::new(move |_, _, n| {
        (200, if n == 0 { cut.to_string() } else { STREAM.to_string() })
    }));
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("work/shop");
    copy_dir(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/polyglot"),
        &repo,
    );
    add_one_file_function(&repo);
    let repo = repo.canonicalize().unwrap();
    std::fs::create_dir_all(dir.path().join("data")).unwrap();
    let cfg = config(dir.path(), &repo, port);
    let agent = plain_agent();
    let mut body = with_effort(first_request(&repo.join("orders_api"), "conv-cut", ONE_FILE), "high");
    let effort_of_last = || -> String {
        let last = received.lock().unwrap().last().cloned().unwrap();
        let sent: Value = serde_json::from_slice(&last.3).unwrap();
        sent["reasoning"]["effort"].as_str().unwrap_or("").to_string()
    };
    let _slot = Slot::take();
    {
        let first = Serving::start(Daemon::open(cfg.clone()).unwrap());
        let post = |path: &str, v: &Value| {
            let response = agent
                .post(format!("http://{}{path}", first.addr))
                .header("content-type", "application/json")
                .send(v.to_string())
                .unwrap();
            assert_eq!(response.status().as_u16(), 200);
        };
        post("/lge/api/repos", &json!({ "path": repo.to_string_lossy() }));
        post("/lge/api/settings", &json!({ "router_apply": "low-only" }));
        let deadline = Instant::now() + Duration::from_secs(60);
        while status_of(&agent, first.addr)["repos"][0]["state"] != "ready" {
            assert!(Instant::now() < deadline, "repository not ready");
            std::thread::sleep(Duration::from_millis(100));
        }
        send_to(&agent, first.addr, &body);
        assert_eq!(effort_of_last(), "low");
    }
    let mut state: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("data/daemon.json")).unwrap()).unwrap();
    state["settings"]["router_apply"] = json!("off");
    std::fs::write(dir.path().join("data/daemon.json"), state.to_string()).unwrap();
    let second = Serving::start(Daemon::open(cfg).unwrap());
    body["input"].as_array_mut().unwrap().push(user("ok, do it"));
    send_to(&agent, second.addr, &body);
    assert_eq!(effort_of_last(), "low", "the same effort for the whole conversation");
}

// ---- stored records of older versions, and what the page counts ------------------

/// A stored proxy record of the conversation `key`.
fn stored(key: &str, mode: &str, task: &str, monitor: Value) -> UsageRecord {
    let now = lge_retrieve::unix_now();
    UsageRecord {
        id: lge_core::stable_id(&["session", key]),
        task_id: key.to_string(),
        session_id: key.to_string(),
        task: task.to_string(),
        model: "gpt-test".to_string(),
        mode: mode.to_string(),
        started_at: now - 3600,
        finished_at: now - 600,
        input_tokens: 60 * 200_000,
        monitor: if monitor.is_null() {
            String::new()
        } else {
            monitor.to_string()
        },
        ..UsageRecord::default()
    }
}

/// What a usage record keeps of a typed message: trimmed, 200 characters.
fn kept(text: &str) -> String {
    text.trim().chars().take(200).collect()
}

#[test]
fn stored_sessions_of_codexs_own_prompts_are_background_without_a_new_request() {
    let long = json!({ "requests": 60, "randomized": true });
    let r = start_with(provider(), |data| {
        let store = Store::open(&data.join("usage.lbug")).unwrap();
        let transcript = format!("{}\n\n{}", lge_usage::GUARDIAN_PROMPT, "the transcript ".repeat(30));
        let mut guardian = stored("old-guardian", "proxy", &kept(&transcript), long.clone());
        guardian.model = "codex-auto-review".to_string();
        guardian.added_tokens_sent = 400_000;
        let title = stored(
            "old-title",
            "proxy",
            &kept(&title_prompt("rename the settings page")),
            long.clone(),
        );
        let memory = stored(
            "old-memory",
            "proxy-holdout",
            &kept(&format!("{}\n\nSummaries follow.", lge_usage::MEMORY_PROMPT)),
            long.clone(),
        );
        let mut reviewer = stored("old-reviewer", "proxy", "is this command safe", long.clone());
        reviewer.model = "codex-auto-review".to_string();
        // A session someone worked in whose last request went to the reviewer
        // under a version that counted it.
        let mut worked = stored(
            "worked",
            "proxy",
            "fix the login form",
            json!({ "requests": 60, "randomized": true, "background_requests": { "guardian": 1 } }),
        );
        worked.model = "codex-auto-review".to_string();
        let work = stored("work", "proxy", "fix the order form", long.clone());
        for rec in [guardian, title, memory, reviewer, worked, work] {
            store.put_usage(&rec).unwrap();
        }
    });
    let status = r.json("/lge/api/status");
    for (id, label) in [
        ("old-guardian", "guardian"),
        ("old-title", "title"),
        ("old-memory", "memory_consolidation"),
        ("old-reviewer", "guardian"),
    ] {
        let row = session_row(&status, id);
        assert_eq!(
            (row["mode"].as_str(), row["background"].as_str()),
            (Some("proxy-background"), Some(label)),
            "{row}"
        );
    }
    for id in ["worked", "work"] {
        assert_eq!(session_row(&status, id)["mode"], "proxy", "{id}");
    }
    let proxy = status["modes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["mode"] == "proxy")
        .cloned();
    assert_eq!(
        proxy.map(|m| m["sessions"].clone()),
        Some(json!(2)),
        "{}",
        status["modes"]
    );
    assert_eq!(status["holdout"]["proxy"]["sessions"], 2);
    assert_eq!(status["holdout"]["holdout"]["sessions"], 0);
    let by_label: Vec<(&str, u64, u64)> = status["background"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| {
            (
                b["label"].as_str().unwrap(),
                b["sessions"].as_u64().unwrap(),
                b["requests"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        by_label,
        [("guardian", 2, 121), ("memory_consolidation", 1, 60), ("title", 1, 60)],
        "{}",
        status["background"]
    );
    let named: Vec<&str> = status["long_sessions"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert_eq!(named.len(), 2, "{named:?}");
    assert!(named.contains(&"worked") && named.contains(&"work"), "{named:?}");
    assert_eq!(status["packs"]["tokens_sent_background"], 400_000);

    // Nothing was written: the stored records keep their mode.
    let Running { serving, dir, .. } = r;
    drop(serving);
    let store = Store::open(&dir.path().join("data/usage.lbug")).unwrap();
    let old = store
        .get_usage(&lge_core::stable_id(&["session", "old-guardian"]))
        .unwrap()
        .unwrap();
    assert_eq!(old.mode, "proxy");
}

#[test]
fn a_restored_session_of_the_reviewers_prompt_with_packs_becomes_background_and_keeps_its_bytes() {
    let (port, _received) = provider();
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("work/shop");
    copy_dir(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/polyglot"),
        &repo,
    );
    let repo = repo.canonicalize().unwrap();
    std::fs::create_dir_all(dir.path().join("data")).unwrap();
    let cfg = config(dir.path(), &repo, port);
    let agent = plain_agent();
    let cwd = repo.join("orders_api");
    let mut body = request_body(&cwd, "old-review", ABOUT_ROUTES);
    let _slot = Slot::take();
    // A version that did not tell the reviewer's requests apart gave its
    // conversation a pack.
    let first = {
        let first = Serving::start(Daemon::open(cfg.clone()).unwrap());
        let response = agent
            .post(format!("http://{}/lge/api/repos", first.addr))
            .header("content-type", "application/json")
            .send(json!({ "path": repo.to_string_lossy() }).to_string())
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        let deadline = Instant::now() + Duration::from_secs(60);
        while status_of(&agent, first.addr)["repos"][0]["state"] != "ready" {
            assert!(Instant::now() < deadline, "repository not ready");
            std::thread::sleep(Duration::from_millis(100));
        }
        let (url, _) = (format!("http://{}/v1/responses", first.addr), ());
        let sent = agent
            .post(url)
            .header("authorization", "Bearer k")
            .send(serde_json::to_vec(&body).unwrap())
            .unwrap();
        assert_eq!(sent.status().as_u16(), 200);
        sent.into_body().read_to_vec().unwrap();
        let status = status_of(&agent, first.addr);
        assert_eq!(session_row(&status, "old-review")["packs_added"], 1);
        status
    };
    let forwarded: Value = {
        let r = _received.lock().unwrap();
        serde_json::from_slice(&r.last().unwrap().3).unwrap()
    };
    assert!(has_context(&forwarded["input"][2]), "{forwarded}");
    drop(first);
    // As that version stored it: the reviewer's prompt was its first message.
    {
        let store = Store::open(&dir.path().join("data/usage.lbug")).unwrap();
        let id = lge_core::stable_id(&["session", "old-review"]);
        let mut rec = store.get_usage(&id).unwrap().unwrap();
        rec.task = kept(&format!("{}\n\nThe transcript.", lge_usage::GUARDIAN_PROMPT));
        store.put_usage(&rec).unwrap();
    }
    let second = Serving::start(Daemon::open(cfg).unwrap());
    let deadline = Instant::now() + Duration::from_secs(60);
    while status_of(&agent, second.addr)["repos"][0]["state"] != "ready" {
        assert!(Instant::now() < deadline, "repository not ready");
        std::thread::sleep(Duration::from_millis(100));
    }
    let last_sent = || -> Value {
        let r = _received.lock().unwrap();
        serde_json::from_slice(&r.last().unwrap().3).unwrap()
    };
    // A message an ordinary conversation gets a follow-up pack for.
    let mut control = request_body(&cwd, "control", ABOUT_ROUTES);
    send_to(&agent, second.addr, &control);
    control["input"].as_array_mut().unwrap().push(user(ABOUT_MAILER));
    send_to(&agent, second.addr, &control);
    let control = last_sent();
    assert!(
        has_context(control["input"].as_array().unwrap().last().unwrap()),
        "an ordinary conversation gets a pack: {control}"
    );
    body["input"].as_array_mut().unwrap().push(user(ABOUT_MAILER));
    send_to(&agent, second.addr, &body);
    let sent = last_sent();
    let (old, new) = (
        forwarded["input"].as_array().unwrap(),
        sent["input"].as_array().unwrap(),
    );
    assert_eq!(new[..old.len()], old[..], "the cached prefix must not change");
    assert_eq!(new.len(), old.len() + 1, "no new pack: {sent}");
    let row = session_row(&status_of(&agent, second.addr), "old-review");
    assert_eq!(
        (row["mode"].as_str(), row["background"].as_str()),
        (Some("proxy-background"), Some("guardian")),
        "{row}"
    );
    // Stored so by its request: the page alone never writes.
    drop(second);
    let store = Store::open(&dir.path().join("data/usage.lbug")).unwrap();
    let rec = store
        .get_usage(&lge_core::stable_id(&["session", "old-review"]))
        .unwrap()
        .unwrap();
    let monitor: Value = serde_json::from_str(&rec.monitor).unwrap();
    assert_eq!(
        (rec.mode.as_str(), monitor["background"].as_str()),
        ("proxy-background", Some("guardian")),
        "{monitor}"
    );
}

/// A pack sent before a restart by a version whose ledger kept no pack text,
/// and the conversation's next request sent at once after the restart, while
/// the folder is indexed again. With `task`, the stored session's first typed
/// message as that version kept it. Returns (the prefix sent before the
/// restart, what was sent after it, the status after it).
fn the_next_request_while_the_folder_is_indexed_again(task: Option<&str>) -> (Vec<Value>, Vec<Value>, Value) {
    let (port, received) = provider();
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("work/shop");
    copy_dir(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/polyglot"),
        &repo,
    );
    // Enough files that indexing again takes a while.
    let generated = repo.join("generated");
    std::fs::create_dir_all(&generated).unwrap();
    for i in 0..1500 {
        std::fs::write(
            generated.join(format!("helper_{i}.py")),
            format!("def helper_{i}(value):\n    return value + {i}\n"),
        )
        .unwrap();
    }
    let repo = repo.canonicalize().unwrap();
    std::fs::create_dir_all(dir.path().join("data")).unwrap();
    let cfg = config(dir.path(), &repo, port);
    let agent = plain_agent();
    let cwd = repo.join("orders_api");
    let mut body = request_body(&cwd, "conv-restart", ABOUT_ROUTES);
    let _slot = Slot::take();
    let last_sent = || -> Vec<Value> {
        let r = received.lock().unwrap();
        let sent: Value = serde_json::from_slice(&r.last().unwrap().3).unwrap();
        sent["input"].as_array().unwrap().clone()
    };
    {
        let first = Serving::start(Daemon::open(cfg.clone()).unwrap());
        let response = agent
            .post(format!("http://{}/lge/api/repos", first.addr))
            .header("content-type", "application/json")
            .send(json!({ "path": repo.to_string_lossy() }).to_string())
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        let deadline = Instant::now() + Duration::from_secs(120);
        while status_of(&agent, first.addr)["repos"][0]["state"] != "ready" {
            assert!(Instant::now() < deadline, "repository not ready");
            std::thread::sleep(Duration::from_millis(100));
        }
        send_to(&agent, first.addr, &body);
    }
    let before = last_sent();
    assert!(has_context(&before[2]), "a pack: {before:?}");
    // As an older version stored it: no pack text.
    {
        let store = Store::open(&dir.path().join("data/usage.lbug")).unwrap();
        let id = lge_core::stable_id(&["session", "conv-restart"]);
        let mut rec = store.get_usage(&id).unwrap().unwrap();
        let mut monitor: Value = serde_json::from_str(&rec.monitor).unwrap();
        monitor["ledger"].as_object_mut().unwrap().remove("pack_texts");
        rec.monitor = monitor.to_string();
        if let Some(task) = task {
            rec.task = kept(task);
        }
        store.put_usage(&rec).unwrap();
    }
    // The folder is in daemon.json: the restarted daemon indexes it again.
    let second = Serving::start(Daemon::open(cfg).unwrap());
    assert_eq!(
        status_of(&agent, second.addr)["repos"][0]["state"],
        "indexing",
        "the folder is still being indexed when the request comes"
    );
    body["input"].as_array_mut().unwrap().push(user("go on"));
    send_to(&agent, second.addr, &body);
    let after = last_sent();
    let status = status_of(&agent, second.addr);
    drop(second);
    (before, after, status)
}

#[test]
fn the_first_request_after_a_restart_waits_for_the_graph_to_send_an_older_pack_again() {
    let (before, after, status) = the_next_request_while_the_folder_is_indexed_again(None);
    assert_eq!(after[..before.len()], before[..], "the cached prefix must not change");
    assert_eq!(session_row(&status, "conv-restart")["mode"], "proxy");
}

#[test]
fn a_restored_background_session_waits_for_the_graph_to_send_an_older_pack_again() {
    let task = format!("{}\n\nThe transcript.", lge_usage::GUARDIAN_PROMPT);
    let (before, after, status) = the_next_request_while_the_folder_is_indexed_again(Some(&task));
    assert_eq!(after[..before.len()], before[..], "the cached prefix must not change");
    let row = session_row(&status, "conv-restart");
    assert_eq!(
        (row["mode"].as_str(), row["background"].as_str()),
        (Some("proxy-background"), Some("guardian")),
        "{row}"
    );
}

#[test]
fn one_more_request_of_a_stored_background_session_keeps_its_earlier_requests_and_input() {
    let r = start_with(provider(), |data| {
        let store = Store::open(&data.join("usage.lbug")).unwrap();
        // Recorded before v0.0.3: no label, no request counted by label.
        let transcript = format!("{}\n\n{}", lge_usage::GUARDIAN_PROMPT, "the transcript ".repeat(30));
        let old = stored(
            "old-guardian",
            "proxy",
            &kept(&transcript),
            json!({ "requests": 60, "randomized": true }),
        );
        // Recorded by v0.0.3: a label and its requests, no input by label.
        let v3 = stored(
            "v3-title",
            "proxy-background",
            "name the task",
            json!({ "requests": 60, "background": "title", "background_requests": { "title": 60 } }),
        );
        for rec in [old, v3] {
            store.put_usage(&rec).unwrap();
        }
    });
    let label_row = |status: &Value, label: &str| -> (u64, u64, i64, i64) {
        let row = status["background"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["label"] == label)
            .cloned()
            .unwrap_or_else(|| panic!("no {label}: {}", status["background"]));
        (
            row["sessions"].as_u64().unwrap(),
            row["requests"].as_u64().unwrap(),
            row["input_tokens"].as_i64().unwrap(),
            row["request_input_tokens"].as_i64().unwrap(),
        )
    };
    let status = r.json("/lge/api/status");
    assert_eq!(label_row(&status, "guardian"), (1, 60, 12_000_000, 12_000_000));
    assert_eq!(label_row(&status, "title"), (1, 60, 12_000_000, 12_000_000));

    let cwd = Path::new("/somewhere/else");
    let transcript = format!("{}\n\n{}", lge_usage::GUARDIAN_PROMPT, "the transcript ".repeat(30));
    r.send(&request_body(cwd, "old-guardian", &transcript));
    r.send(&request_body(cwd, "v3-title", "name the task"));
    let status = r.json("/lge/api/status");
    assert_eq!(label_row(&status, "guardian"), (1, 61, 12_001_200, 12_001_200));
    assert_eq!(label_row(&status, "title"), (1, 61, 12_001_200, 12_001_200));
}

#[test]
fn a_stored_session_told_only_by_the_reviewers_model_stays_a_work_session_after_a_typed_request() {
    let r = start_with(provider(), |data| {
        let store = Store::open(&data.join("usage.lbug")).unwrap();
        // Recorded before v0.0.3, its last request to the reviewer.
        for key in ["typed-next", "reviewer-next"] {
            let mut rec = stored(key, "proxy", "fix the login form", json!({ "requests": 60 }));
            rec.model = "codex-auto-review".to_string();
            store.put_usage(&rec).unwrap();
        }
    });
    let status = r.json("/lge/api/status");
    for key in ["typed-next", "reviewer-next"] {
        assert_eq!(
            session_row(&status, key)["background"],
            "guardian",
            "the page cannot tell it from the reviewer's"
        );
    }
    let cwd = Path::new("/somewhere/else");
    r.send(&request_body(cwd, "typed-next", "fix the login form"));
    let mut reviewer = request_body(cwd, "reviewer-next", "fix the login form");
    reviewer["model"] = json!("codex-auto-review");
    r.send(&reviewer);
    let status = r.json("/lge/api/status");
    let typed = session_row(&status, "typed-next");
    assert_eq!(
        (typed["mode"].as_str(), typed["background"].as_str()),
        (Some("proxy"), None),
        "{typed}"
    );
    let review = session_row(&status, "reviewer-next");
    assert_eq!(
        (review["mode"].as_str(), review["background"].as_str()),
        (Some("proxy-background"), Some("guardian")),
        "{review}"
    );
}

#[test]
fn the_pack_line_leaves_sessions_of_v002_out_of_the_tokens_no_count_covers() {
    let key = |text: &str| {
        let mut bytes = b"context\0".to_vec();
        bytes.extend_from_slice(text.as_bytes());
        bytes.push(0);
        lge_core::content_hash(&bytes)[..12].to_string()
    };
    let r = start_with(provider(), |data| {
        let store = Store::open(&data.join("usage.lbug")).unwrap();
        // v0.0.2: no pack counts, two packs in its ledger.
        let ledger = json!({ "contexts": { key(ABOUT_ROUTES): true, key(ABOUT_AUDIT): true } });
        let mut v2 = stored(
            "packs-v2",
            "proxy",
            "a task",
            json!({ "requests": 9, "ledger": ledger }),
        );
        v2.added_tokens_sent = 1000;
        // Before v0.0.2: nothing but its tokens.
        let mut v1 = stored("packs-v1", "proxy", "an older task", Value::Null);
        v1.added_tokens_sent = 300;
        for rec in [v2, v1] {
            store.put_usage(&rec).unwrap();
        }
    });
    let packs = r.json("/lge/api/status")["packs"].clone();
    assert_eq!(
        (
            packs["added"].as_u64(),
            packs["tokens_sent"].as_i64(),
            packs["tokens_sent_uncounted"].as_i64()
        ),
        (Some(2), Some(1300), Some(300)),
        "{packs}"
    );
    let page = String::from_utf8(r.get("/").1).unwrap();
    assert!(page.contains("in sessions whose records count no pack"));
    assert!(page.contains("count their packs as added (from their ledger)"));
}

#[test]
fn the_tool_output_and_pack_cards_leave_out_sessions_of_an_ab_run() {
    let r = start();
    r.add_repo();
    let cwd = r.repo.join("orders_api");
    r.send(&request_body(&cwd, "work-session", ABOUT_ROUTES));
    r.api("/lge/api/settings", &json!({ "ab_arm": "on" }));
    r.send(&request_body(&cwd, "ab-session", ABOUT_ROUTES));
    r.api("/lge/api/settings", &json!({ "ab_arm": "" }));
    let status = r.json("/lge/api/status");
    let ab = session_row(&status, "ab-session");
    assert!(ab["removed_tokens_sent"].as_i64().unwrap() > 0, "shortened: {ab}");
    assert!(ab["added_tokens_sent"].as_i64().unwrap() > 0, "a pack: {ab}");
    let work = session_row(&status, "work-session");
    let saved = &status["saved"];
    assert_eq!(
        (
            saved["outputs_shortened"].as_u64(),
            saved["contexts_added"].as_u64(),
            saved["tokens_before"].as_i64().unwrap() - saved["tokens_after"].as_i64().unwrap()
        ),
        (Some(1), Some(1), work["removed_tokens_sent"].as_i64().unwrap()),
        "{saved}"
    );
}

#[test]
fn the_folder_note_reads_only_a_status_requested_after_the_folder_was_sent() {
    let r = start();
    let page = String::from_utf8(r.get("/").1).unwrap();
    // A status requested before the POST cannot clear the note.
    assert!(page.contains("if (pending && seq > pendingAfter)"));
    assert!(page.contains("function watch(root) { pending = root; pendingAfter = statusSeq; }"));
    assert!(page.contains("watch(a.root)"));
    // Re-index: watched once the daemon has the folder indexing.
    assert!(page.contains("await api('repos', 'POST', { path: b.dataset.reindex }); watch(b.dataset.reindex);"));
    // An answer older than the one shown is dropped.
    assert!(page.contains("if (seq < shownSeq) return;"));
}

#[test]
fn a_session_recorded_before_request_counts_shows_no_requests_instead_of_zero() {
    let r = start_with(provider(), |data| {
        let store = Store::open(&data.join("usage.lbug")).unwrap();
        store
            .put_usage(&stored("before-counts", "proxy", "an old task", Value::Null))
            .unwrap();
        store
            .put_usage(&stored("counted", "proxy", "a newer task", json!({ "requests": 4 })))
            .unwrap();
    });
    let status = r.json("/lge/api/status");
    let old = session_row(&status, "before-counts");
    assert_eq!(
        (old["requests"].clone(), old["mean_input_tokens"].clone()),
        (Value::Null, Value::Null),
        "{old}"
    );
    let counted = session_row(&status, "counted");
    assert_eq!(counted["requests"], 4);
    assert_eq!(status["holdout"]["left_out"], 2);
    assert!(
        status["holdout"]["left_out_note"]
            .as_str()
            .unwrap()
            .contains("1 sessions recorded before v0.0.2 have no request count"),
        "{}",
        status["holdout"]
    );
    let page = String::from_utf8(r.get("/").1).unwrap();
    assert!(page.contains("nn(x.requests)") && page.contains("left_out_note"));
}

#[test]
fn only_sessions_assigned_while_the_holdout_share_was_above_zero_are_compared() {
    let r = start();
    let cwd = Path::new("/somewhere/else");
    r.send(&request_body(cwd, "before-share", "run the tests"));
    r.api("/lge/api/settings", &json!({ "holdout_share": 1.0 }));
    r.send(&request_body(cwd, "held", "run the tests"));
    // A share so small that nobody new is held out, but every new conversation is placed at random.
    r.api("/lge/api/settings", &json!({ "holdout_share": 1e-12 }));
    r.send(&request_body(cwd, "after-share", "run the tests"));
    let status = r.json("/lge/api/status");
    let modes: Vec<(&str, String)> = ["before-share", "held", "after-share"]
        .iter()
        .map(|id| (*id, session_row(&status, id)["mode"].as_str().unwrap_or("").to_string()))
        .collect();
    assert_eq!(
        modes,
        [
            ("before-share", "proxy".to_string()),
            ("held", "proxy-holdout".to_string()),
            ("after-share", "proxy".to_string())
        ]
    );
    let h = &status["holdout"];
    assert_eq!(
        (
            h["proxy"]["sessions"].as_u64(),
            h["holdout"]["sessions"].as_u64(),
            h["left_out"].as_u64()
        ),
        (Some(1), Some(1), Some(1)),
        "{h}"
    );
}

#[test]
fn sessions_of_an_ab_run_are_kept_out_of_the_cards_but_still_listed_with_their_requests() {
    // A daemon stopped in the middle of a run starts without its arm.
    let r = start_with(provider(), |data| {
        std::fs::write(data.join("daemon.json"), r#"{"repos":[],"settings":{"ab_arm":"on"}}"#).unwrap();
    });
    assert_eq!(r.json("/lge/api/status")["settings"]["ab_arm"], "");
    let cwd = Path::new("/somewhere/else");
    assert_eq!(r.api("/lge/api/settings", &json!({ "ab_arm": "on" }))["ab_arm"], "on");
    r.send(&request_body(cwd, "ab-session", "run the tests"));
    r.api("/lge/api/settings", &json!({ "ab_arm": "" }));
    r.send(&request_body(cwd, "work-session", "run the tests"));
    let (status, _, _) = r.post(
        "/lge/api/settings",
        br#"{"ab_arm":"maybe"}"#,
        &[("content-type", "application/json")],
    );
    assert_eq!(status, 400);

    let status = r.json("/lge/api/status");
    let modes: Vec<(&str, u64)> = status["modes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["mode"].as_str().unwrap(), m["sessions"].as_u64().unwrap()))
        .collect();
    assert_eq!(modes, [("ab", 1), ("proxy", 1)], "{}", status["modes"]);
    let ab = session_row(&status, "ab-session");
    assert_eq!(
        (ab["mode"].as_str(), ab["ab_arm"].as_str(), ab["requests"].as_u64()),
        (Some("proxy"), Some("on"), Some(1)),
        "lge ab counts its requests from this row: {ab}"
    );
    assert_eq!(session_row(&status, "work-session")["ab_arm"], Value::Null);
}

#[test]
fn the_pack_line_separates_tokens_of_sessions_recorded_before_pack_counts() {
    let r = start_with(provider(), |data| {
        let store = Store::open(&data.join("usage.lbug")).unwrap();
        let mut older = stored("packs-v2", "proxy", "an older task", json!({ "requests": 5 }));
        older.added_tokens_sent = 1000;
        older.started_at -= 86_400;
        let mut newer = stored(
            "packs-v3",
            "proxy",
            "a newer task",
            json!({ "requests": 5, "packs_added": 1 }),
        );
        newer.added_tokens_sent = 500;
        for rec in [older, newer] {
            store.put_usage(&rec).unwrap();
        }
    });
    let status = r.json("/lge/api/status");
    let packs = &status["packs"];
    assert_eq!(
        (
            packs["tokens_sent"].as_i64(),
            packs["tokens_sent_uncounted"].as_i64(),
            packs["tokens_sent_background"].as_i64()
        ),
        (Some(1500), Some(1000), Some(0)),
        "{packs}"
    );
    let since = packs["since"].as_i64().expect("since when");
    assert_eq!(since, session_row(&status, "packs-v2")["started_at"].as_i64().unwrap());
    let page = String::from_utf8(r.get("/").1).unwrap();
    assert!(page.contains("tokens_sent_uncounted") && page.contains("pk.since"));
}

#[test]
fn compaction_requests_are_counted_with_their_input_tokens_not_as_sessions() {
    let r = start();
    let mut body = request_body(Path::new("/somewhere/else"), "conv-compacting", "run the tests");
    r.send(&body);
    body["input"].as_array_mut().unwrap().push(user("go on"));
    body["client_metadata"] = json!({ "x-codex-turn-metadata": r#"{"request_kind":"compaction"}"# });
    r.send(&body);
    let status = r.json("/lge/api/status");
    let row = status["background"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["label"] == "compaction")
        .cloned()
        .unwrap_or_else(|| panic!("{}", status["background"]));
    assert_eq!(
        (
            row["sessions"].clone(),
            row["requests"].as_u64(),
            row["request_input_tokens"].as_i64()
        ),
        (Value::Null, Some(1), Some(1200)),
        "{row}"
    );
    let session = session_row(&status, "conv-compacting");
    assert_eq!(
        (session["mode"].as_str(), session["input_tokens"].as_i64()),
        (Some("proxy"), Some(2400))
    );
    let page = String::from_utf8(r.get("/").1).unwrap();
    assert!(page.contains("Input tokens of these requests"));
}

#[test]
fn the_long_session_note_says_when_each_session_last_ran() {
    let r = start_with(provider(), |data| {
        let store = Store::open(&data.join("usage.lbug")).unwrap();
        let now = lge_retrieve::unix_now();
        let mut older = stored("long-before", "proxy", "a task two days ago", json!({ "requests": 60 }));
        older.started_at = now - 3 * 86_400;
        older.finished_at = now - 2 * 86_400;
        let mut newer = stored("long-now", "proxy", "a task today", json!({ "requests": 60 }));
        newer.finished_at = now - 60;
        for rec in [older, newer] {
            store.put_usage(&rec).unwrap();
        }
    });
    let status = r.json("/lge/api/status");
    let named: Vec<(&str, i64)> = status["long_sessions"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["id"].as_str().unwrap(), s["finished_at"].as_i64().expect("when")))
        .collect();
    assert_eq!(named.len(), 2, "{}", status["long_sessions"]);
    assert_eq!(
        (named[0].0, named[1].0),
        ("long-now", "long-before"),
        "the latest first"
    );
    assert!(named[0].1 > named[1].1);
    let page = String::from_utf8(r.get("/").1).unwrap();
    assert!(page.contains("last request ${esc(when(l.finished_at))}"));
}

#[test]
fn the_pack_line_counts_packs_of_older_versions_as_added() {
    // A conversation of v0.0.2: its ledger had three packs, no pack counts.
    let messages = [ABOUT_ROUTES, ABOUT_AUDIT, ABOUT_MAILER];
    let key = |text: &str| {
        let mut bytes = b"context\0".to_vec();
        bytes.extend_from_slice(text.as_bytes());
        bytes.push(0);
        lge_core::content_hash(&bytes)[..12].to_string()
    };
    let r = start_with(provider(), |data| {
        let store = Store::open(&data.join("usage.lbug")).unwrap();
        let contexts: serde_json::Map<String, Value> = messages.iter().map(|m| (key(m), json!(true))).collect();
        // Anchored to a history the conversation no longer has.
        let anchors: serde_json::Map<String, Value> = messages.iter().map(|m| (key(m), json!(1))).collect();
        let monitor = json!({ "requests": 9, "ledger": { "contexts": contexts, "anchors": anchors } });
        store
            .put_usage(&stored("older-packs", "proxy", ABOUT_ROUTES, monitor))
            .unwrap();
    });
    r.add_repo();
    let cwd = r.repo.join("orders_api");
    // The history Codex sends after a compaction: the messages, the context, a summary, a new message.
    let mut items: Vec<Value> = messages.iter().map(|m| user(m)).collect();
    items.push(user(&format!(
        "<environment_context>\n  <cwd>{}</cwd>\n</environment_context>",
        cwd.display()
    )));
    items.push(user("Another language model started to solve this problem and produced a summary of its thinking process. Here is the summary:\nhalf done."));
    items.push(user("ok, do it"));
    let body = json!({ "model": "gpt-test", "prompt_cache_key": "older-packs", "input": items, "stream": true });
    r.send_with(&body, &[]);
    let status = r.json("/lge/api/status");
    let packs = &status["packs"];
    assert_eq!(
        (packs["added"].as_u64(), packs["retired"].as_u64()),
        (Some(3), Some(3)),
        "{packs}"
    );
    let row = session_row(&status, "older-packs");
    assert_eq!(
        (row["packs_added"].as_u64(), row["packs_retired"].as_u64()),
        (Some(3), Some(3))
    );
    let page = String::from_utf8(r.get("/").1).unwrap();
    assert!(page.contains("retired after the history was rewritten"));
}

// ---- folders: Codex's own requests and folders --------------------------------------

fn git(dir: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .args([
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=t",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "git {args:?}");
}

/// A git folder with one commit of `file`.
fn git_folder(dir: &Path, file: &str, text: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join(file), text).unwrap();
    git(dir, &["init", "-q"]);
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "first"]);
}

fn saved_repos(r: &Running) -> Vec<String> {
    std::fs::read_to_string(r.dir.path().join("data/daemon.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| serde_json::from_value(v["repos"].clone()).ok())
        .unwrap_or_default()
}

#[test]
fn codex_background_requests_never_add_a_folder_to_the_graph() {
    let r = start();
    let notes = r.repo.parent().unwrap().join("notes");
    git_folder(&notes, "summary.py", "def summarise_notes(text):\n    return text\n");
    let notes = notes.canonicalize().unwrap();
    r.send_with(
        &request_body(&notes, "bg-consolidation", "summarise_notes should keep headings"),
        &[("x-openai-subagent", "memory_consolidation")],
    );
    let mut reviewer = request_body(&notes, "bg-reviewer", "summarise_notes should keep headings");
    reviewer["model"] = json!("codex-auto-review");
    r.send_with(&reviewer, &[]);
    r.send_with(&request_body(&notes, "bg-title", &title_prompt("keep headings")), &[]);
    let status = r.json("/lge/api/status");
    assert_eq!(status["repos"], json!([]), "{}", status["repos"]);
    assert!(saved_repos(&r).is_empty());
    // A conversation someone works in still adds it.
    r.send_with(&request_body(&notes, "fg", "summarise_notes should keep headings"), &[]);
    let status = r.json("/lge/api/status");
    assert_eq!(status["repos"][0]["name"], "notes", "{}", status["repos"]);
    assert_eq!(saved_repos(&r), [notes.to_string_lossy().to_string()]);
}

#[test]
fn codex_own_memory_folders_are_never_indexed() {
    let r = start();
    let workspace = r.repo.parent().unwrap().to_path_buf();
    // Codex's memory baseline: a one-commit repository with its marker.
    let memory = workspace.join("agent-home/memories_v2");
    git_folder(&memory, "memory_summary.md", "# Summary\n");
    std::fs::write(memory.join(".git/codex-baseline-version"), "1\n").unwrap();
    r.send_with(&request_body(&memory, "in-memory", "summarise the notes"), &[]);
    assert_eq!(r.json("/lge/api/status")["repos"], json!([]));
    let (status, _, answer) = r.post(
        "/lge/api/repos",
        json!({ "path": memory.to_string_lossy() }).to_string().as_bytes(),
        &[("content-type", "application/json")],
    );
    assert_eq!(status, 400);
    assert!(String::from_utf8_lossy(&answer).contains("Codex keeps for itself"));
    // A plain git folder beside it is added as before.
    let plain = workspace.join("plain");
    git_folder(&plain, "tool.py", "def run_tool():\n    return 1\n");
    r.send_with(&request_body(&plain, "in-plain", "run_tool should return two"), &[]);
    let status = r.json("/lge/api/status");
    assert_eq!(status["repos"][0]["name"], "plain", "{}", status["repos"]);
}

#[test]
fn folders_inside_codex_home_are_codexs_own_except_its_worktrees() {
    let (port, _received) = provider();
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let work = work.canonicalize().unwrap();
    let home = work.join("agent-home");
    let plugins = home.join(".tmp/plugins");
    git_folder(&plugins, "plugin.py", "def plugin():\n    return 1\n");
    let skills = home.join("vendor_imports/skills");
    git_folder(&skills, "skill.py", "def skill():\n    return 1\n");
    let nested = home.join("memories/memories_v2");
    git_folder(&nested, "memory_summary.md", "# Summary\n");
    // The desktop app's worktree of the user's own repository: its `.git` is a file.
    let tree = home.join("worktrees/ab12/shop");
    std::fs::create_dir_all(&tree).unwrap();
    std::fs::write(tree.join("cart.py"), "def cart():\n    return 1\n").unwrap();
    std::fs::write(tree.join(".git"), "gitdir: /elsewhere/.git/worktrees/ab12\n").unwrap();
    let mut cfg = config(dir.path(), &work.join("shop"), port);
    cfg.codex_home = Some(home);
    let _slot = Slot::take();
    let s = Serving::start(Daemon::open(cfg).unwrap());
    let post = |path: &Path| {
        let request = ureq::http::Request::builder()
            .method("POST")
            .uri(format!("http://{}/lge/api/repos", s.addr))
            .header("content-type", "application/json")
            .body(json!({ "path": path.to_string_lossy() }).to_string().into_bytes())
            .unwrap();
        let response = plain_agent().run(request).unwrap();
        let status = response.status().as_u16();
        (
            status,
            String::from_utf8_lossy(&response.into_body().read_to_vec().unwrap()).to_string(),
        )
    };
    for own in [&plugins, &skills, &nested] {
        let (status, answer) = post(own);
        assert_eq!(status, 400, "{}: {answer}", own.display());
        assert!(answer.contains("keeps for itself"), "{answer}");
    }
    let (status, answer) = post(&tree);
    assert_eq!(status, 200, "{answer}");
}

#[test]
fn a_saved_codex_memory_folder_is_left_out_when_the_daemon_starts() {
    let (port, _received) = provider();
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let work = work.canonicalize().unwrap();
    let home = work.join("agent-home");
    let marked = home.join("memories_v2");
    git_folder(&marked, "memory_summary.md", "# Summary\n");
    std::fs::write(marked.join(".git/codex-baseline-version"), "1\n").unwrap();
    // A reset cut short: no marker, but Codex's memory folder by its place.
    let unmarked = home.join("memories");
    git_folder(&unmarked, "memory_summary.md", "# Summary\n");
    let plain = work.join("plain");
    git_folder(&plain, "tool.py", "def run_tool():\n    return 1\n");
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let listed = [&marked, &unmarked, &plain].map(|p| p.to_string_lossy().to_string());
    std::fs::write(data.join("daemon.json"), json!({ "repos": listed }).to_string()).unwrap();
    let mut cfg = config(dir.path(), &work.join("shop"), port);
    cfg.codex_home = Some(home);
    let _slot = Slot::take();
    let s = Serving::start(Daemon::open(cfg).unwrap());
    let status = status_of(&plain_agent(), s.addr);
    let roots: Vec<&str> = status["repos"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["root"].as_str().unwrap())
        .collect();
    assert_eq!(roots, [plain.to_string_lossy()], "{}", status["repos"]);
    let file: Value = serde_json::from_str(&std::fs::read_to_string(data.join("daemon.json")).unwrap()).unwrap();
    assert_eq!(file["repos"], json!([plain.to_string_lossy()]));
}

#[test]
fn a_background_request_does_not_change_the_folder_the_graph_tools_answer_for() {
    let r = start();
    let other = r.repo.parent().unwrap().join("other");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("helper.py"), "def other_helper():\n    return 1\n").unwrap();
    let other = other.canonicalize().unwrap();
    r.api("/lge/api/repos", &json!({ "path": r.repo.to_string_lossy() }));
    r.api("/lge/api/repos", &json!({ "path": other.to_string_lossy() }));
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let status = r.json("/lge/api/status");
        let repos = status["repos"].as_array().unwrap();
        if repos.len() == 2 && repos.iter().all(|x| x["state"] == "ready") {
            break;
        }
        assert!(Instant::now() < deadline, "not ready: {status}");
        std::thread::sleep(Duration::from_millis(100));
    }
    r.send_with(&request_body(&r.repo.join("orders_api"), "fg-shop", ABOUT_ROUTES), &[]);
    r.send_with(
        &request_body(&other, "bg-other", "is this command safe"),
        &[("x-openai-subagent", "guardian")],
    );
    let (status, _, body) = r.post(
        "/mcp",
        json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "search_code_context", "arguments": { "question": "where is create_order" } } })
            .to_string()
            .as_bytes(),
        &[
            ("content-type", "application/json"),
            ("accept", "application/json, text/event-stream"),
        ],
    );
    assert_eq!(status, 200);
    let answer: Value = serde_json::from_slice(&body).unwrap();
    let text = answer["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("orders_api"), "{answer}");
}

#[test]
fn the_page_says_when_a_folder_is_indexed_and_shows_a_setting_it_could_not_save() {
    let r = start();
    let page = String::from_utf8(r.get("/").1).unwrap();
    assert!(
        page.contains("is indexed: ${n(r.files)} files"),
        "the indexing note is replaced when done"
    );
    assert!(page.contains("watch(a.root)"));
    assert!(
        page.contains("e.target.value = (state && state.settings && state.settings.router_apply) || 'off'"),
        "a select that was not saved shows the daemon's value again"
    );
}

// ---- the measures added after the first week of real data ------------------------

/// A guardian request of `parent` on `thread`: the policy as instructions and `items`
/// as messages of about 1,500 tokens each (their text is `word`, many times).
fn guardian_request(parent: &str, items: &[&str]) -> Value {
    let text = |w: &str| format!("{} ", w).repeat(1500);
    let mut input: Vec<Value> = items.iter().map(|w| user(&text(w))).collect();
    input.push(user(">>> APPROVAL REQUEST"));
    json!({
        "model": "codex-auto-review", "prompt_cache_key": format!("guardian:{parent}"),
        "instructions": "You are the approval reviewer.", "input": input, "stream": true,
    })
}

fn send_guardian(r: &Running, parent: &str, thread: &str, items: &[&str]) -> Value {
    let body = guardian_request(parent, items);
    let (status, _, _) = r.post(
        "/v1/responses",
        &serde_json::to_vec(&body).unwrap(),
        &[
            ("authorization", "Bearer test-key"),
            ("content-type", "application/json"),
            ("thread-id", thread),
            ("session-id", thread),
            ("x-codex-parent-thread-id", parent),
            ("x-openai-subagent", "guardian"),
        ],
    );
    assert_eq!(status, 200);
    body
}

#[test]
fn a_guardian_request_is_measured_against_the_previous_request_of_the_same_parent() {
    let r = start_with(provider_with(Box::new(|_, _, _| (200, stream_with(30_000, 0)))), |_| {});
    let first = send_guardian(&r, "p1", "t1", &["alpha", "beta", "gamma"]);
    let second = send_guardian(&r, "p1", "t2", &["alpha", "beta", "delta"]);
    // Another parent: no previous sibling, not counted.
    send_guardian(&r, "p2", "t3", &["alpha", "beta", "gamma"]);
    let status = r.json("/lge/api/status");
    let g = &status["guardian"];
    // Shared: the instructions, the tools and the first two items, as estimated for the bytes sent.
    let shared = lge_proxy::guardian::signature(&second).common_prefix_tokens(&lge_proxy::guardian::signature(&first));
    assert!(shared > 3000, "{shared}");
    assert_eq!(g["with_previous"], 1, "{g}");
    assert_eq!(g["lcp_tokens"].as_i64(), Some(shared), "{g}");
    assert_eq!(g["long_prefix"], 1);
    // The stand-in reports nothing cached: all of the shared prefix was shareable and uncached.
    assert_eq!(g["shareable_uncached"].as_i64(), Some(shared.min(30_000)), "{g}");
    assert_eq!(g["splits"]["key_same"]["requests"], 1, "{g}");
    assert_eq!(g["splits"]["new_thread"]["requests"], 1);
    assert_eq!(g["splits"]["lge_unchanged_bytes"]["requests"], 1);
    assert!(g["splits"].get("key_different").is_none());
    // Counts only: the measure holds no text of the requests.
    assert!(!g.to_string().contains("alpha"), "{g}");
}

#[test]
fn guardian_threads_are_counted_apart_from_the_parents_they_work_for() {
    let r = start_with(
        provider_with(Box::new(|_, _, _| (200, stream_with(30_000, 10_000)))),
        |_| {},
    );
    // Two parents over five reviewer threads, three of them with a single request.
    send_guardian(&r, "p1", "t1", &["a"]);
    send_guardian(&r, "p1", "t1", &["a", "b"]);
    send_guardian(&r, "p1", "t2", &["a", "b"]);
    send_guardian(&r, "p2", "t3", &["c"]);
    send_guardian(&r, "p2", "t4", &["c"]);
    send_guardian(&r, "p2", "t4", &["c", "d"]);
    send_guardian(&r, "p2", "t5", &["c", "d"]);
    let g = r.json("/lge/api/status")["guardian"].clone();
    assert_eq!(
        (g["parents"].as_u64(), g["threads"].as_u64()),
        (Some(2), Some(5)),
        "{g}"
    );
    assert_eq!(g["single_request_threads"], 3, "{g}");
    assert_eq!(g["requests"], 7);
    assert!((g["requests_per_parent"].as_f64().unwrap() - 3.5).abs() < 1e-9);
    // Cached tokens of the measured requests are the provider's.
    assert_eq!(
        g["cached_tokens"].as_i64(),
        Some(10_000 * g["with_previous"].as_i64().unwrap())
    );
}

#[test]
fn the_background_table_gives_the_cached_and_uncached_input_of_each_label() {
    let r = start_with(provider(), |data| {
        let store = Store::open(&data.join("usage.lbug")).unwrap();
        for (key, input, cached) in [("g-one", 33_000, 8_000), ("g-two", 74_000, 70_000)] {
            let mut rec = stored(
                key,
                "proxy-background",
                "review",
                json!({ "requests": 1, "background": "guardian", "background_requests": { "guardian": 1 } }),
            );
            rec.input_tokens = input;
            rec.cached_input_tokens = cached;
            store.put_usage(&rec).unwrap();
        }
    });
    let status = r.json("/lge/api/status");
    let row = status["background"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["label"] == "guardian")
        .unwrap()
        .clone();
    assert_eq!(
        (row["cached_tokens"].as_i64(), row["uncached_tokens"].as_i64()),
        (Some(78_000), Some(29_000)),
        "{row}"
    );
    assert_eq!(row["input_counted_requests"], 2);
}

#[test]
fn compaction_requests_of_older_records_are_not_shown_as_costing_nothing() {
    let r = start_with(provider(), |data| {
        let store = Store::open(&data.join("usage.lbug")).unwrap();
        // Recorded before v0.0.5: requests counted by label, no input by label.
        let mut rec = stored(
            "old-long",
            "proxy",
            "fix the cart",
            json!({ "requests": 60, "background_requests": { "compaction": 5 } }),
        );
        rec.input_tokens = 1000;
        store.put_usage(&rec).unwrap();
    });
    let status = r.json("/lge/api/status");
    let row = status["background"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["label"] == "compaction")
        .unwrap()
        .clone();
    assert_eq!(
        (row["requests"].as_u64(), row["input_counted_requests"].as_u64()),
        (Some(5), Some(0)),
        "{row}"
    );
    assert_eq!(row["cached_tokens"], Value::Null, "nothing known, not 0");
    // The page prints a dash for it, and says how many requests a sum leaves out.
    let page = String::from_utf8(r.get("/").1).unwrap();
    assert!(page.contains("input_counted_requests") && page.contains("requests not counted"));
}

#[test]
fn an_arm_without_sessions_has_no_medians_on_the_page() {
    let r = start();
    let status = r.json("/lge/api/status");
    assert_eq!(status["holdout"]["proxy"]["sessions"], 0);
    assert!(status["holdout"]["proxy"]["medians"].as_object().unwrap().is_empty());
    assert!(status["holdout"]["holdout"]["medians"].as_object().unwrap().is_empty());
    let page = String::from_utf8(r.get("/").1).unwrap();
    assert!(page.contains("m[k] != null ? n(Math.round(m[k])) : '–'"));
}

#[test]
fn a_message_seen_before_its_folder_has_a_graph_is_counted_and_a_folder_shows_its_index_time() {
    let r = start();
    // A folder that is no repository: no graph, so no pack, for good.
    r.send(&request_body(
        Path::new("/somewhere/else"),
        "conv-nograph",
        "fix the checkout total",
    ));
    let status = r.json("/lge/api/status");
    assert_eq!(status["packs"]["no_graph"], 1, "{}", status["packs"]);
    assert_eq!(session_row(&status, "conv-nograph")["packs_no_graph"], 1);
    r.add_repo();
    let status = r.json("/lge/api/status");
    assert!(
        status["repos"][0]["index_seconds"].as_f64().unwrap() > 0.0,
        "{}",
        status["repos"]
    );
}

#[test]
fn a_graph_on_disk_records_its_folder_is_listed_with_its_size_and_is_deleted_only_when_not_listed() {
    let r = start();
    r.add_repo();
    let status = r.json("/lge/api/status");
    let graphs = status["graphs"]["graphs"].as_array().unwrap().clone();
    assert_eq!(graphs.len(), 1, "{}", status["graphs"]);
    let g = &graphs[0];
    assert_eq!(g["root"].as_str(), Some(r.repo.to_string_lossy().as_ref()));
    assert!(g["bytes"].as_u64().unwrap() > 0);
    assert_eq!((g["listed"].as_bool(), g["exists"].as_bool()), (Some(true), Some(true)));
    let dir = g["dir"].as_str().unwrap().to_string();
    let data = r.dir.path().join("data/graphs").join(&dir);
    assert_eq!(
        std::fs::read_to_string(data.join("root.txt")).unwrap(),
        r.repo.to_string_lossy()
    );
    let delete = |query: &str| -> (u16, Value) {
        let response = r
            .agent
            .delete(format!("{}/lge/api/graphs?{query}", r.base))
            .call()
            .unwrap();
        let code = response.status().as_u16();
        (
            code,
            serde_json::from_slice(&response.into_body().read_to_vec().unwrap()).unwrap(),
        )
    };
    let (code, answer) = delete(&format!("dir={dir}"));
    assert_eq!(code, 400, "{answer}");
    assert!(data.exists(), "a listed folder keeps its graph");
    // Removed from the list: the graph stays, shown as not listed, and now it may be deleted.
    let removed = r
        .agent
        .delete(format!("{}/lge/api/repos?path={}", r.base, r.repo.display()))
        .call()
        .unwrap();
    assert_eq!(removed.status().as_u16(), 200);
    let status = r.json("/lge/api/status");
    assert_eq!(status["graphs"]["graphs"][0]["listed"], false);
    assert_eq!(status["graphs"]["unlisted"], 1);
    let (code, answer) = delete("dir=..");
    assert_eq!(code, 400, "{answer}");
    let (code, answer) = delete(&format!("path={}", r.repo.display()));
    assert_eq!(code, 200, "{answer}");
    assert!(answer["bytes"].as_u64().unwrap() > 0);
    assert!(!data.exists());
}

#[test]
fn the_configuration_column_names_only_keys_codex_reads() {
    let (port, _received) = provider();
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let home = dir.path().join("codex");
    let plugin = home.join("plugins/cache/market/computer/1");
    std::fs::create_dir_all(&plugin).unwrap();
    std::fs::write(
        plugin.join(".mcp.json"),
        r#"{"mcpServers":{"cua_repl":{"command":"x"}}}"#,
    )
    .unwrap();
    std::fs::write(home.join("config.toml"), "[mcp_servers.docs]\ncommand = \"docs\"\n").unwrap();
    let mut cfg = config(dir.path(), &work.canonicalize().unwrap().join("shop"), port);
    cfg.codex_home = Some(home);
    let _slot = Slot::take();
    let s = Serving::start(Daemon::open(cfg).unwrap());
    let agent = plain_agent();
    let function = |name: &str| json!({ "type": "function", "name": name, "description": "d", "parameters": { "type": "object" } });
    let namespace = |name: &str, tools: &[&str]| json!({ "type": "namespace", "name": name, "description": "d", "tools": tools.iter().map(|t| function(t)).collect::<Vec<_>>() });
    let mut body = request_body(Path::new("/somewhere/else"), "conv-cfg", "run the tests");
    body["tools"] = json!([
        function("exec_command"),
        function("request_user_input"),
        namespace(
            "collaboration",
            &[
                "spawn_agent",
                "wait_agent",
                "send_message",
                "followup_task",
                "interrupt_agent",
                "list_agents"
            ]
        ),
        namespace("clock", &["curr_time"]),
        namespace("mcp__cua_repl__", &["js"]),
        namespace("mcp__docs__", &["lookup"]),
        namespace("mcp__other__", &["x"]),
    ]);
    send_to(&agent, s.addr, &body);
    let status = status_of(&agent, s.addr);
    let groups = status["prefix"]["fixed"]["groups"].as_array().unwrap();
    let config_of = |g: &str| {
        groups.iter().find(|x| x["group"] == g).unwrap()["config"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert!(
        config_of("collaboration").starts_with("[agents] enabled = false"),
        "{}",
        config_of("collaboration")
    );
    assert_eq!(config_of("clock"), "", "no switch is known");
    assert_eq!(config_of("built-in"), "", "no key for a group of built-in tools");
    assert_eq!(config_of("docs"), "[mcp_servers.docs]");
    assert!(
        config_of("cua_repl").starts_with("[plugins.\"computer@market\"] enabled = false"),
        "{}",
        config_of("cua_repl")
    );
    assert_eq!(config_of("other"), "", "declared nowhere");
    let ghost = |name: &str| {
        status["ghost_tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["name"] == name)
            .unwrap()["config"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert!(ghost("request_user_input").starts_with("[tools.experimental_request_user_input] enabled = false"));
    assert_eq!(ghost("exec_command"), "", "no invented key");
    assert!(ghost("collaborationsend_message").contains("[agents] enabled = false"));
    // Only read: the file is as it was.
    assert_eq!(
        std::fs::read_to_string(dir.path().join("codex/config.toml")).unwrap(),
        "[mcp_servers.docs]\ncommand = \"docs\"\n"
    );
}

#[test]
fn unread_script_outputs_show_on_the_status_as_a_count_and_a_share_of_tool_output() {
    let r = start();
    let text: String = (0..300)
        .map(|i| format!("STEP 1: entry {i} describes the order flow through the checkout service and its retries\n"))
        .collect();
    let script = "const cmds = [\"rg -n x src\", \"git status --short\"];\nfor (let i = 0; i < cmds.length; i++) { const r = await tools.exec_command({cmd: cmds[i], workdir: \"/repo\"}); text(`STEP ${i+1}: ${cmds[i]}\\n${r.output}`); }";
    let mut body = request_body(Path::new("/somewhere/else"), "conv-unread", "check the repo");
    let items = body["input"].as_array_mut().unwrap();
    items.push(json!({ "type": "custom_tool_call", "call_id": "s1", "name": "exec", "input": script }));
    items.push(json!({ "type": "custom_tool_call_output", "call_id": "s1", "output": [
        { "type": "input_text", "text": "Script completed\nWall time 0.1 seconds\nOutput:\n" },
        { "type": "input_text", "text": text },
    ] }));
    r.send(&body);
    r.send(&body);
    let u = r.json("/lge/api/status")["unread_scripts"].clone();
    assert_eq!(u["outputs"], 1, "{u}");
    assert!(u["tokens"].as_u64().unwrap() > 2000);
    let share = u["share"].as_f64().unwrap();
    assert!(
        share > 0.0 && share < 1.0,
        "{u}: the pytest output of the request is tool output too"
    );
}

#[test]
fn a_mention_link_in_a_task_is_shown_by_its_label_only() {
    let page = String::from_utf8(start().get("/").1).unwrap();
    let from = page.find("function clean_task").expect("the page cleans task texts");
    let code = &page[from..from + page[from..].find("\n\n").unwrap()];
    let script = format!(
        "{code}\nconst out = clean_task('use [$figma](/p/SKILL.md) and [@T](thread://1), keep [a link](http://x) and [$a$&](q)');\nprocess.stdout.write(out);"
    );
    let Ok(output) = std::process::Command::new("node").arg("-e").arg(&script).output() else {
        eprintln!("node is not installed: the display function was not run");
        return;
    };
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "use $figma and @T, keep [a link](http://x) and $a$&"
    );
    for place in ["clean_task(x.task)", "clean_task(l.task)", "clean_task(c.task)"] {
        assert!(page.contains(place), "{place}");
    }
}

#[test]
fn the_compaction_estimate_endpoint_is_a_get_that_changes_nothing_and_is_cached_until_a_rollout_changes() {
    let (port, _received) = provider();
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let home = dir.path().join("codex");
    let day = home.join("sessions/2026/07/20");
    std::fs::create_dir_all(&day).unwrap();
    let mut cfg = config(dir.path(), &work.canonicalize().unwrap().join("shop"), port);
    cfg.codex_home = Some(home.clone());
    let _slot = Slot::take();
    let s = Serving::start(Daemon::open(cfg).unwrap());
    let agent = plain_agent();
    let get = || -> Value {
        serde_json::from_slice(
            &agent
                .get(format!("http://{}/lge/api/compact-whatif", s.addr))
                .call()
                .unwrap()
                .into_body()
                .read_to_vec()
                .unwrap(),
        )
        .unwrap()
    };
    let first = get();
    assert_eq!(first["sessions"].as_array().map(Vec::len), Some(0));
    let second = get();
    assert_eq!(
        first["generated_at"], second["generated_at"],
        "cached while no rollout changed"
    );
    assert!(
        !home.join("config.toml").exists(),
        "nothing is written to the agent home"
    );
    // A POST is not an endpoint.
    let post = agent
        .post(format!("http://{}/lge/api/compact-whatif", s.addr))
        .header("content-type", "application/json")
        .send("{}")
        .unwrap();
    assert_eq!(post.status().as_u16(), 400);
    // A rollout appears: the answer is made again.
    std::fs::write(
        day.join("rollout-2026-07-20T00-00-00-a.jsonl"),
        format!(
            "{}\n",
            json!({ "type": "session_meta", "payload": { "id": "a", "source": "cli" } })
        ),
    )
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let third = get();
    assert!(third["generated_at"].as_i64() >= first["generated_at"].as_i64());
    assert!(third["rollouts_read"].as_u64().is_some());
}

#[test]
fn the_page_has_the_compaction_estimate_block_under_the_long_session_note_and_loads_it_on_open() {
    let page = include_str!("../src/ui.html");
    let note = page.find("id=\"long_note\"").expect("the long-session note");
    let block = page.find("id=\"whatif_box\"").expect("the estimate block");
    assert!(block > note && block - note < 600, "directly under the note");
    assert!(page.contains("<details id=\"whatif_box\""), "collapsed");
    assert!(
        page.contains("api('compact-whatif')") && page.contains("'toggle'"),
        "one fetch, when opened"
    );
    assert!(
        page.contains("$('whatif_box').hidden = !longList.length"),
        "only when a long session is listed"
    );
    assert!(page.contains("lge never writes that file"));
}

#[test]
fn the_page_says_when_cached_counts_cover_fewer_input_tokens_and_shows_no_estimate_as_such() {
    let page = include_str!("../src/ui.html");
    assert!(page.contains("b.cached_basis_tokens < b.request_input_tokens"));
    assert!(page.contains("input tokens</span>"));
    assert!(page.contains("c.delta_pct == null ? `At ${n(c.limit)}: no estimate.`"));
}
