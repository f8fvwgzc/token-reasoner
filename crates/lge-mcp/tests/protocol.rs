//! The server over its real transport: JSON lines in, JSON lines out.
use lge_mcp::{tools, Server, INSTRUCTIONS};
use lge_retrieve::IndexOptions;
use lge_store::Store;
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;

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

/// A server over a private copy of the mixed-language fixture.
fn server() -> (tempfile::TempDir, Server) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    copy_dir(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/polyglot"),
        &root,
    );
    let store = Store::open(&dir.path().join("graph.lbug")).unwrap();
    let mut server = Server::new(Box::new(store), &root, IndexOptions::default());
    server.fresh_for = Duration::ZERO;
    (dir, server)
}

fn exchange(server: &mut Server, messages: &[Value]) -> Vec<Value> {
    let input: String = messages.iter().map(|m| format!("{m}\n")).collect();
    let mut out = Vec::new();
    server.serve(input.as_bytes(), &mut out).unwrap();
    String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn call(server: &mut Server, id: u64, name: &str, args: Value) -> (String, bool) {
    let answers = exchange(
        server,
        &[json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": { "name": name, "arguments": args } })],
    );
    let result = &answers[0]["result"];
    (
        result["content"][0]["text"].as_str().unwrap().to_string(),
        result["isError"].as_bool().unwrap(),
    )
}

#[test]
fn handshake_listing_and_errors() {
    let (_dir, mut server) = server();
    let answers = exchange(
        &mut server,
        &[
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "test", "version": "0" } } }),
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
            json!({ "jsonrpc": "2.0", "id": 3, "method": "ping" }),
            json!({ "jsonrpc": "2.0", "id": 4, "method": "no/such" }),
            json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": { "name": "nope", "arguments": {} } }),
        ],
    );
    assert_eq!(answers.len(), 5, "a notification gets no answer: {answers:?}");
    let init = &answers[0]["result"];
    assert_eq!(init["protocolVersion"], "2025-06-18");
    assert_eq!(init["serverInfo"]["name"], "lge");
    assert_eq!(init["instructions"], INSTRUCTIONS);
    assert!(init["capabilities"]["tools"].is_object());
    assert_eq!(answers[1]["result"]["tools"].as_array().unwrap().len(), 8);
    assert_eq!(answers[2], json!({ "jsonrpc": "2.0", "id": 3, "result": {} }));
    assert_eq!(answers[3]["error"]["code"], -32601);
    assert_eq!(answers[4]["result"]["isError"], true);

    // A version the server does not know is answered with one it does.
    let other = exchange(
        &mut server,
        &[json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "1999-01-01" } })],
    );
    assert_eq!(other[0]["result"]["protocolVersion"], "2025-06-18");

    // Broken input is answered, not fatal.
    let mut out = Vec::new();
    server.serve("{not json\n\n".as_bytes(), &mut out).unwrap();
    let broken: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(broken["error"]["code"], -32700);
}

#[test]
fn what_every_session_pays_for_the_tools_stays_small() {
    // The tool list and the instructions are part of every session's prompt.
    let listing: Vec<Value> = tools()
        .iter()
        .map(|t| json!({ "name": t.name, "description": t.description, "inputSchema": t.input_schema }))
        .collect();
    let tokens = lge_retrieve::budget::count(&serde_json::to_string(&listing).unwrap())
        + lge_retrieve::budget::count(INSTRUCTIONS);
    assert!(
        tokens <= 1100,
        "tool list plus instructions cost {tokens} tokens; keep descriptions short"
    );
    for t in tools() {
        assert!(
            t.description.len() <= 130 && !t.description.contains('\n'),
            "{}: one short line",
            t.name
        );
        assert!(
            INSTRUCTIONS.contains(&format!("`{}`", t.name)),
            "the instructions do not mention `{}`",
            t.name
        );
    }
    // Every tool the instructions name exists.
    let names: Vec<&str> = tools().iter().map(|t| t.name).collect();
    for (i, part) in INSTRUCTIONS.split('`').enumerate() {
        if i % 2 == 1 {
            assert!(
                names.contains(&part),
                "the instructions name `{part}`, which is not a tool"
            );
        }
    }
}

#[test]
fn code_tools_answer_compactly_and_end_with_the_commit() {
    let (_dir, mut server) = server();
    let (pack, err) = call(
        &mut server,
        1,
        "search_code_context",
        json!({ "question": "create_order must validate the payload" }),
    );
    assert!(
        !err && pack.contains("## Start here") && pack.contains("def create_order(payload: dict):"),
        "{pack}"
    );
    assert!(
        lge_retrieve::budget::count(&pack) <= 1500,
        "{}",
        lge_retrieve::budget::count(&pack)
    );

    // The source was just shown: asking for it again costs one line.
    let (again, _) = call(
        &mut server,
        2,
        "get_relevant_source",
        json!({ "symbols": ["create_order"] }),
    );
    assert!(
        again.contains("orders_api.routes.create_order  (source already shown above in this session, unchanged)"),
        "{again}"
    );
    assert!(!again.contains("def create_order"), "{again}");
    // Another symbol is returned in full, once.
    let (first, _) = call(
        &mut server,
        3,
        "get_relevant_source",
        json!({ "symbols": ["insert_order"] }),
    );
    assert!(first.contains("orders_api/repo.py:13-20  function orders_api.repo.insert_order\n```\ndef insert_order(customer_id, total):"), "{first}");
    let (second, _) = call(
        &mut server,
        4,
        "get_relevant_source",
        json!({ "symbols": ["insert_order"] }),
    );
    assert!(
        second.contains("already shown above") && !second.contains("def insert_order"),
        "{second}"
    );

    let (lines, _) = call(
        &mut server,
        5,
        "get_relevant_source",
        json!({ "file": "orders_api/routes.py", "start_line": 10, "end_line": 11 }),
    );
    assert!(
        lines.starts_with(
            "orders_api/routes.py:10-11\n```\n@router.post(\"\")\ndef create_order(payload: dict):\n```\n"
        ),
        "{lines}"
    );
    let (escape, err) = call(
        &mut server,
        6,
        "get_relevant_source",
        json!({ "file": "../graph.lbug" }),
    );
    assert!(
        err && escape.contains("must be a path inside the repository"),
        "{escape}"
    );

    let (impact, _) = call(
        &mut server,
        7,
        "analyze_change_impact",
        json!({ "target": "orders_api/routes.py" }),
    );
    assert!(
        impact.contains("web_gateway.src.ordersClient.createOrder") && impact.contains("## Tests to run"),
        "{impact}"
    );
    let (flow, _) = call(
        &mut server,
        8,
        "trace_execution_flow",
        json!({ "symbol": "POST /api/orders", "max_depth": 6 }),
    );
    assert!(
        flow.contains("createOrderHandler") && flow.contains("endpoint POST /orders") && flow.contains("create_order"),
        "{flow}"
    );
    let (deps, _) = call(
        &mut server,
        9,
        "find_dependencies",
        json!({ "target": "order.created" }),
    );
    assert!(deps.contains("PUBLISHES") && deps.contains("CONSUMES"), "{deps}");
    let (arch, _) = call(&mut server, 10, "get_architecture_context", json!({}));
    assert!(arch.contains("- notifier: depends on orders_api"), "{arch}");
    let (vague, err) = call(&mut server, 11, "trace_execution_flow", json!({ "symbol": "render" }));
    assert!(
        !err && vague.contains("names 2 things. Ask again with one qualified name"),
        "{vague}"
    );
    let (missing, err) = call(
        &mut server,
        12,
        "analyze_change_impact",
        json!({ "target": "no_such_symbol" }),
    );
    assert!(err && missing.contains("nothing in the graph is called"), "{missing}");

    for text in [&pack, &again, &impact, &flow, &deps, &arch] {
        assert!(text.trim_end().lines().last().unwrap().starts_with("commit "), "{text}");
    }
}

#[test]
fn an_edited_file_is_reindexed_before_the_next_answer() {
    let (dir, mut server) = server();
    let (before, _) = call(
        &mut server,
        1,
        "search_code_context",
        json!({ "question": "cancel_order" }),
    );
    assert!(!before.contains("orders_api.routes.cancel_order"), "{before}");
    let file = dir.path().join("repo/orders_api/routes.py");
    let mut text = std::fs::read_to_string(&file).unwrap();
    text.push_str("\n\ndef cancel_order(order_id):\n    \"\"\"Cancel an order that was not shipped.\"\"\"\n    return find_order(order_id)\n");
    std::fs::write(&file, text).unwrap();
    let (after, _) = call(
        &mut server,
        2,
        "search_code_context",
        json!({ "question": "cancel_order" }),
    );
    assert!(
        after.contains("function orders_api.routes.cancel_order  def cancel_order(order_id)"),
        "{after}"
    );
    assert!(after.contains("Cancel an order that was not shipped."), "{after}");
}
