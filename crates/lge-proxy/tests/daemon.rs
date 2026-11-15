//! The daemon between a client and a stand-in provider, over real sockets.
use lge_proxy::server::{bind, run, Config, Daemon};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const STREAM: &str = "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n\
event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"done\"}\n\n\
event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":1200,\"input_tokens_details\":{\"cached_tokens\":1000},\"output_tokens\":300,\"output_tokens_details\":{\"reasoning_tokens\":250},\"total_tokens\":1500}}}\n\n";

/// What the stand-in provider received: (method, url, headers, body).
type Received = Arc<Mutex<Vec<(String, String, Vec<(String, String)>, Vec<u8>)>>>;

/// A provider that records every request and answers `/responses` with a
/// fixed event stream.
fn provider() -> (u16, Received) {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    let received: Received = Arc::new(Mutex::new(Vec::new()));
    let log = received.clone();
    std::thread::spawn(move || {
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
            log.lock()
                .unwrap()
                .push((request.method().as_str().to_string(), url.clone(), headers, body));
            let header = |k: &str, v: &str| tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()).unwrap();
            let response = if url.ends_with("/responses") {
                tiny_http::Response::from_string(STREAM)
                    .with_header(header("Content-Type", "text/event-stream"))
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

struct Running {
    _dir: tempfile::TempDir,
    base: String,
    received: Received,
    agent: ureq::Agent,
    repo: PathBuf,
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
    let (provider_port, received) = provider();
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("work/shop");
    copy_dir(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/polyglot"),
        &repo,
    );
    let repo = repo.canonicalize().unwrap();
    let mut cfg = Config::new(dir.path().join("data"));
    cfg.port = 0;
    cfg.workspaces = vec![repo.parent().unwrap().to_path_buf()];
    cfg.upstream_api = format!("http://127.0.0.1:{provider_port}/v1");
    cfg.upstream_chatgpt = format!("http://127.0.0.1:{provider_port}/chatgpt");
    let daemon = Daemon::open(cfg).unwrap();
    let (server, addr) = bind(&daemon).unwrap();
    std::thread::spawn(move || run(daemon, server));
    let agent = ureq::Agent::new_with_config(ureq::Agent::config_builder().http_status_as_error(false).build());
    Running {
        _dir: dir,
        base: format!("http://{addr}"),
        received,
        agent,
        repo,
    }
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
    assert_eq!(session["task"], "run the tests");
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
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::new(dir.path().join("data"));
    cfg.port = 0;
    cfg.upstream_api = "http://127.0.0.1:9/v1".to_string();
    let daemon = Daemon::open(cfg).unwrap();
    let (server, addr) = bind(&daemon).unwrap();
    std::thread::spawn(move || run(daemon, server));
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
