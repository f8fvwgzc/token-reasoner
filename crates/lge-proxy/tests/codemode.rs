//! Code mode: the model runs tools from a JavaScript script
//! (`custom_tool_call` named `exec`), and the result comes back as a list of
//! parts: a status part, then what the script printed. The proxy reads the
//! one shell command a script runs and shortens its output as it would the
//! output of that command run directly. The script is never changed.
use lge_proxy::responses::transform;
use lge_proxy::session::{Options, Session};
use serde_json::{json, Value};

const HEADER: &str = "Script completed\nWall time 0.1 seconds\nOutput:\n";

/// A script that runs one command; `cmd` is the JavaScript literal for it.
fn script(cmd: &str) -> String {
    format!(
        "const r = await tools.exec_command({{cmd:{cmd},\"workdir\":\"/repo\",\"yield_time_ms\":10000}});\ntext(r.output);"
    )
}

fn exec(id: &str, script: &str) -> Value {
    json!({ "type": "custom_tool_call", "status": "completed", "call_id": id, "name": "exec", "input": script })
}

fn result(id: &str, printed: &str) -> Value {
    json!({ "type": "custom_tool_call_output", "call_id": id, "output": [
        { "type": "input_text", "text": HEADER },
        { "type": "input_text", "text": printed },
    ] })
}

fn user(text: &str) -> Value {
    json!({ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": text }] })
}

fn body(items: Vec<Value>) -> Value {
    let mut input = vec![
        user("<environment_context>\n  <cwd>/repo</cwd>\n</environment_context>"),
        user("find where orders are created and run the tests"),
    ];
    input.extend(items);
    json!({ "model": "gpt-test", "prompt_cache_key": "conv-1", "instructions": "system", "input": input, "tools": [{ "type": "custom", "name": "exec" }], "stream": true })
}

fn printed(body: &Value, item: usize) -> String {
    body["input"][item]["output"][1]["text"].as_str().unwrap().to_string()
}

/// What `rg -n` prints in a large repository, as the agent hands it back:
/// its own truncation notes around many match lines.
fn rg_output() -> String {
    let mut text = String::from("Warning: truncated output (original token count: 21873)\nTotal output lines: 640\n\n");
    for f in 0..40 {
        for l in 0..8 {
            text.push_str(&format!(
                "src/services/svc_{f}/orders.ts:{}:    const order = await createOrder(payload, {{ requestId: req.id }});\n",
                l * 11 + 4
            ));
        }
        if f == 19 {
            text.push_str("…9120 tokens truncated…\n");
        }
    }
    text
}

fn pytest_run() -> String {
    let mut run = String::from(
        "============================= test session starts ==============================\nplatform darwin -- Python 3.12.4, pytest-8.3.2\ncollected 300 items\n\n",
    );
    for i in 0..300 {
        run.push_str(&format!(
            "tests/test_orders.py::test_case_number_{i} PASSED                              [{:3}%]\n",
            i / 3
        ));
    }
    run + "\n============================= 300 passed in 9.87s ==============================\n"
}

fn cargo_run() -> String {
    let mut run = String::from(
        "   Compiling orders v0.1.0 (/repo)\n    Finished `test` profile [unoptimized + debuginfo] target(s) in 3.21s\n     Running unittests src/lib.rs (target/debug/deps/orders-3f2a1b)\n\nrunning 250 tests\n",
    );
    for i in 0..250 {
        run.push_str(&format!("test orders::tests::creates_an_order_in_case_{i} ... ok\n"));
    }
    run + "\ntest result: ok. 250 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.52s\n\n"
}

/// A file whose lines look like search matches: only the command tells
/// that it is a file read, which is left alone.
fn notes_file() -> String {
    (0..200)
        .map(|i| {
            format!(
                "docs/notes_{}.md:{i}: remember to check the order totals here\n",
                i % 25
            )
        })
        .collect()
}

#[test]
fn a_search_run_from_a_script_is_capped_and_the_script_is_untouched() {
    let rg = rg_output();
    let mut req = body(vec![
        exec("s1", &script("\"rg -n \\\"createOrder(\\\" src\"")),
        result("s1", &rg),
    ]);
    let before = req.clone();
    let mut session = Session::default();
    let seen = transform(&mut req, &mut session, None, &Options::default());
    assert_eq!(seen.outputs_replaced, 1);

    let short = printed(&req, 3);
    assert!(
        short.starts_with("Warning: truncated output (original token count: 21873)\nTotal output lines: 640\n\n"),
        "the agent's notes stay in place: {short}"
    );
    assert!(short.contains("…9120 tokens truncated…\n"), "{short}");
    assert!(
        short.ends_with("lge: 60 of 320 matching lines shown, from 10 of 40 files. Not shown: src/services/svc_18/orders.ts, src/services/svc_19/orders.ts, src/services/svc_2/orders.ts, src/services/svc_20/orders.ts, src/services/svc_21/orders.ts, src/services/svc_22/orders.ts, src/services/svc_23/orders.ts, src/services/svc_24/orders.ts, .... Narrow the pattern or search one file to see the rest.\n"),
        "{short}"
    );
    assert!(short.len() * 3 < rg.len());
    // The status part, the number of parts, the script and everything else stay as they were.
    let parts = req["input"][3]["output"].as_array().unwrap();
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0], before["input"][3]["output"][0]);
    for i in [0, 1, 2] {
        assert_eq!(req["input"][i], before["input"][i], "item {i}");
    }
    for field in ["model", "prompt_cache_key", "instructions", "tools", "stream"] {
        assert_eq!(req[field], before[field], "{field}");
    }
}

#[test]
fn test_runs_from_a_script_are_reduced_to_their_result() {
    let mut req = body(vec![
        exec("t1", &script("'cd /repo && python -m pytest -q 2>&1 | tail -n 400'")),
        result("t1", &pytest_run()),
        exec("t2", &script("`cargo test -p orders`")),
        json!({ "type": "custom_tool_call_output", "call_id": "t2", "output": [
            { "type": "input_text", "text": HEADER },
            { "type": "input_image", "image_url": "data:image/png;base64,AAAA" },
            { "type": "input_text", "text": cargo_run() },
        ] }),
    ]);
    let before = req.clone();
    let mut session = Session::default();
    let seen = transform(&mut req, &mut session, None, &Options::default());
    assert_eq!(seen.outputs_replaced, 2);
    let pytest = printed(&req, 3);
    assert!(pytest.starts_with("ok") && pytest.contains("300 passed"), "{pytest}");
    assert!(
        pytest.contains("lge: output of `pytest` reduced to its result"),
        "{pytest}"
    );
    let parts = req["input"][5]["output"].as_array().unwrap();
    assert_eq!(parts.len(), 3, "the number of parts never changes");
    assert_eq!(
        parts[1], before["input"][5]["output"][1],
        "a part that is not text stays"
    );
    let cargo = parts[2]["text"].as_str().unwrap();
    assert!(cargo.starts_with("ok"), "{cargo}");
    assert!(
        cargo.contains("lge: output of `cargo` reduced to its result"),
        "{cargo}"
    );
    assert_eq!(req["input"][2], before["input"][2]);
    assert_eq!(req["input"][4], before["input"][4]);
}

#[test]
fn reads_from_a_script_are_left_whole_and_a_repeat_points_to_the_first_copy() {
    let notes = notes_file();
    let mut req = body(vec![
        exec("r1", &script("'cat docs/notes.md'")),
        result("r1", &notes),
        exec("r2", &script("\"sed -n '1,200p' docs/notes.md\"")),
        result("r2", &notes),
    ]);
    let mut session = Session::default();
    let seen = transform(&mut req, &mut session, None, &Options::default());
    assert_eq!(seen.outputs_replaced, 1);
    assert_eq!(
        printed(&req, 3),
        notes,
        "a file read is not a search, whatever its lines look like"
    );
    assert_eq!(
        printed(&req, 5),
        "lge: identical to the output of `cat docs/notes.md` earlier in this conversation (200 lines, unchanged); it is not repeated here.\n"
    );
    assert_eq!(session.saved.outputs_shortened, 1);
}

#[test]
fn a_script_that_is_not_certain_is_judged_by_its_content() {
    let notes = notes_file();
    let two_calls = "const a = await tools.exec_command({cmd: 'cat docs/notes.md'});\nconst b = await tools.exec_command({cmd: 'git status'});\ntext(a.output); text(b.output);";
    let computed = "const file = 'docs/notes.md';\nconst r = await tools.exec_command({cmd: `cat ${file}`, workdir: '/repo'});\ntext(r.output);";
    let in_a_loop = "for (const f of ['docs/notes.md']) {\n  const r = await tools.exec_command({cmd: 'cat docs/notes.md'});\n  text(r.output);\n}";
    for (i, script) in [two_calls, computed, in_a_loop].into_iter().enumerate() {
        let mut req = body(vec![exec("x", script), result("x", &notes)]);
        let before = req.clone();
        let seen = transform(&mut req, &mut Session::default(), None, &Options::default());
        assert_eq!(seen.outputs_replaced, 1, "script {i}");
        assert!(
            printed(&req, 3).contains("lge: 60 of 200 matching lines shown"),
            "without a command, the lines are judged as they look: script {i}"
        );
        assert_eq!(req["input"][2], before["input"][2], "the script is never touched");
    }
    // Another tool, an output without the status part: judged by content as before.
    let mut other = body(vec![
        json!({ "type": "custom_tool_call", "call_id": "p1", "name": "apply_patch", "input": "*** Begin Patch\n*** End Patch" }),
        json!({ "type": "custom_tool_call_output", "call_id": "p1", "output": "Done!" }),
        exec("y", &script("'pytest -q'")),
        json!({ "type": "custom_tool_call_output", "call_id": "y", "output": [{ "type": "input_text", "text": pytest_run() }] }),
    ]);
    let before = other.clone();
    let seen = transform(&mut other, &mut Session::default(), None, &Options::default());
    assert_eq!(seen.outputs_replaced, 1);
    assert_eq!(other["input"][3], before["input"][3]);
    assert!(other["input"][5]["output"][0]["text"]
        .as_str()
        .unwrap()
        .starts_with("ok"));
}

#[test]
fn an_output_given_as_one_string_keeps_its_status_lines() {
    let framed = format!("{HEADER}{}", pytest_run());
    let mut req = body(vec![
        exec("s1", &script("'pytest -v'")),
        json!({ "type": "custom_tool_call_output", "call_id": "s1", "output": framed }),
    ]);
    transform(&mut req, &mut Session::default(), None, &Options::default());
    let out = req["input"][3]["output"].as_str().unwrap();
    assert!(out.starts_with(&format!("{HEADER}ok")), "{out}");
}

#[test]
fn the_same_items_get_the_same_bytes_on_every_turn_and_are_counted_once() {
    let items = vec![
        exec("s1", &script("\"rg -n \\\"createOrder(\\\" src\"")),
        result("s1", &rg_output()),
        exec("s2", &script("'pytest -q'")),
        result("s2", &pytest_run()),
        exec("s3", &script("'cat docs/notes.md'")),
        result("s3", &notes_file()),
        exec("s4", &script("'cat docs/notes.md'")),
        result("s4", &notes_file()),
    ];
    let mut session = Session::default();
    let opts = Options::default();
    let mut first = body(items.clone());
    transform(&mut first, &mut session, None, &opts);
    let saved = session.saved.clone();
    assert_eq!(saved.outputs_shortened, 3);

    // The next turn resends the conversation with one more message.
    let mut next = body(items.clone());
    next["input"].as_array_mut().unwrap().push(user("thanks, now fix it"));
    transform(&mut next, &mut session, None, &opts);
    let prefix = |v: &Value| serde_json::to_string(&v["input"].as_array().unwrap()[..10]).unwrap();
    assert_eq!(prefix(&next), prefix(&first), "the cached prefix must not change");
    assert_eq!(session.saved, saved, "decided once, counted once");

    // A fresh session (a restarted daemon) decides the same from the same items.
    let mut again = body(items);
    transform(&mut again, &mut Session::default(), None, &opts);
    assert_eq!(
        serde_json::to_string(&again).unwrap(),
        serde_json::to_string(&first).unwrap()
    );
}

#[test]
fn a_request_with_nothing_to_shorten_is_left_exactly_as_it_was() {
    let mut req = body(vec![
        exec("s1", &script("'git status'")),
        result("s1", "On branch main\nnothing to commit, working tree clean\n"),
        exec("s2", &script("'rg -n createOrder src'")),
        result("s2", "src/a.ts:4:createOrder(p)\n"),
    ]);
    let before = req.clone();
    let seen = transform(&mut req, &mut Session::default(), None, &Options::default());
    assert_eq!(seen.outputs_replaced, 0);
    assert_eq!(req, before);
}

/// A config file whose keys look like `path:` prefixes.
fn config_file() -> String {
    (0..150)
        .map(|i| format!("app.feature_{i}.enabled: true # toggles feature number {i} for the service\n"))
        .collect()
}

#[test]
fn file_contents_are_not_taken_for_search_matches() {
    // Two commands: no single command, so the printed text is judged by its
    // content, and a config file is not a list of matches.
    let two_reads = "const a = await tools.exec_command({cmd: 'cat config/app.yml'});\nconst b = await tools.exec_command({cmd: 'cat config/dev.yml'});\ntext(a.output); text(b.output);";
    // A search piped into another program prints files, not matches.
    let piped = script("'rg -l feature config | xargs cat'");
    for script in [two_reads, piped.as_str()] {
        let mut req = body(vec![exec("x", script), result("x", &config_file())]);
        let before = req.clone();
        let seen = transform(&mut req, &mut Session::default(), None, &Options::default());
        assert_eq!(seen.outputs_replaced, 0, "{script}");
        assert_eq!(req, before, "{script}");
    }
}

#[test]
fn a_test_run_cut_before_its_result_is_sent_whole() {
    // The script yielded while pytest was still running: what it printed so
    // far has no failure yet, which is not a pass, and it is not a search.
    let mut partial = pytest_run();
    partial.truncate(partial.find("tests/test_orders.py::test_case_number_200 ").unwrap());
    let running = "Script running with cell ID 7\nWall time 10.0 seconds\nOutput:\n";
    let two_runs = "const a = await tools.exec_command({cmd: 'pytest -v tests/test_orders.py'});\ntext(a.output);\nconst b = await tools.exec_command({cmd: 'pytest -v tests/test_cart.py'});\ntext(b.output);";
    for script in [script("'pytest -v tests/test_orders.py'"), two_runs.to_string()] {
        let mut req = body(vec![
            exec("t1", &script),
            json!({ "type": "custom_tool_call_output", "call_id": "t1", "output": [
                { "type": "input_text", "text": running },
                { "type": "input_text", "text": partial },
            ] }),
        ]);
        let before = req.clone();
        let seen = transform(&mut req, &mut Session::default(), None, &Options::default());
        assert_eq!(seen.outputs_replaced, 0, "{script}");
        assert_eq!(req, before, "{script}");
    }
}

#[test]
fn a_script_that_repeats_its_command_in_a_loop_body_is_not_read() {
    // `else` belongs to the loop body: the command may run many times.
    let looped = "for (const f of ['a', 'b']) if (f === 'a') { text(f) } else await tools.exec_command({cmd: 'cat docs/notes.md'});";
    let notes = notes_file();
    let mut req = body(vec![exec("l1", looped), result("l1", &notes)]);
    let before = req.clone();
    transform(&mut req, &mut Session::default(), None, &Options::default());
    assert!(
        printed(&req, 3).contains("lge: 60 of 200 matching lines shown"),
        "judged by its content, as a script with no single command"
    );
    assert_eq!(req["input"][2], before["input"][2], "the script is never touched");
}
