//! Code mode: the model runs tools from a JavaScript script
//! (`custom_tool_call` named `exec`), and the result comes back as a list of
//! parts: a status part, then what the script printed, one part per `text()`
//! call. The proxy reads the shell commands a script runs and shortens each
//! printed output as it would the output of that command run directly. The
//! script is never changed.
use lge_proxy::responses::transform;
use lge_proxy::rules::{Mode, RuleId};
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
    // Every file left out is named, with its number of matching lines.
    let mut hidden: Vec<String> = (0..40).map(|f| format!("src/services/svc_{f}/orders.ts")).collect();
    hidden.sort();
    let hidden: Vec<String> = hidden[10..].iter().map(|p| format!("{p} (8)")).collect();
    assert!(
        short.ends_with(&format!("lge: 60 of 320 matching lines shown, from 10 of 40 files. Not shown: {}. Narrow the pattern or search one file to see the rest, or run the same search again before changing anything to see it whole.\n", hidden.join(", "))),
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
    assert_eq!(seen.outputs_replaced, 1);
    // `| tail` without pipefail: the status is tail's and the run may be cut
    // above its failure, so a pass is not vouched for and the output stays whole.
    assert_eq!(printed(&req, 3), printed(&before, 3));
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
        "lge: identical to the output of run 1 of `cat docs/notes.md` earlier in this conversation (200 lines, first line: \"docs/notes_0.md:0: remember to check the order totals here\"); it is not repeated here.\n"
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
    // The one new typed message is the only new count (first seen with no graph).
    assert_eq!(session.saved.contexts_no_graph, saved.contexts_no_graph + 1);
    let mut again = session.saved.clone();
    again.contexts_no_graph = saved.contexts_no_graph;
    assert_eq!(again, saved, "decided once, counted once");

    // Settings change mid-conversation (rules switched, shortening off, the
    // holdout arm): past items keep their bytes.
    let mut switched = opts.clone();
    switched.rules.check = Mode::Off;
    switched.rules.search_cap = Mode::Shadow;
    switched.rules.repeat_pointer = Mode::Off;
    for later in [
        switched,
        Options {
            shorten_outputs: false,
            ..opts.clone()
        },
        Options {
            holdout: true,
            ..opts.clone()
        },
    ] {
        let mut turn = body(items.clone());
        turn["input"].as_array_mut().unwrap().push(user("thanks, now fix it"));
        let seen = transform(&mut turn, &mut session, None, &later);
        assert_eq!(
            prefix(&turn),
            prefix(&first),
            "the cached prefix must not change: {later:?}"
        );
        assert_eq!(seen.lge_break_item, None);
    }
    let mut after = session.saved.clone();
    after.contexts_no_graph = saved.contexts_no_graph;
    assert_eq!(after, saved);

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

fn fixture(name: &str) -> (Value, Value) {
    let path = format!("{}/../../tests/fixtures/codex/{name}", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let v: Value = serde_json::from_str(&text).unwrap();
    (v["call"].clone(), v["result"].clone())
}

/// The text of part `part` of the output of item `item`.
fn part(body: &Value, item: usize, part: usize) -> String {
    body["input"][item]["output"][part]["text"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn whole_exec_results_printed_as_json_lines_are_shortened_inside_their_line() {
    // Built as Codex lays it out: a script printed three exec results with
    // text(obj); over budget, Codex merged them under its truncation note and
    // cut the third line in the middle.
    let (call, result) = fixture("exec_json_lines.json");
    let mut req = body(vec![call.clone(), result.clone()]);
    let before = req.clone();
    let mut session = Session::default();
    let seen = transform(&mut req, &mut session, None, &Options::default());
    assert_eq!(seen.outputs_replaced, 1);
    let old = part(&before, 3, 1);
    let new = part(&req, 3, 1);
    let old_lines: Vec<&str> = old.split('\n').collect();
    let new_lines: Vec<&str> = new.split('\n').collect();
    assert_eq!(old_lines.len(), new_lines.len());
    // The note, the first line and the cut third line stay byte for byte.
    for k in [0, 1, 2, 3, 5] {
        assert_eq!(old_lines[k], new_lines[k], "line {k}");
    }
    // The second line is still one exec result, with the run reduced to its
    // result and every other field as it was.
    let was: Value = serde_json::from_str(old_lines[4]).unwrap();
    let now: Value = serde_json::from_str(new_lines[4]).unwrap();
    let output = now["output"].as_str().unwrap();
    assert!(output.starts_with("ok: 300 passed"), "{output}");
    for key in ["chunk_id", "wall_time_seconds", "exit_code", "original_token_count"] {
        assert_eq!(now[key], was[key], "{key}");
    }
    let prefix_len = old_lines[4].find("\"output\":").unwrap();
    assert_eq!(&new_lines[4][..prefix_len], &old_lines[4][..prefix_len]);
    assert!(new_lines[4].ends_with("\"}"));
    assert_eq!(req["input"][3]["output"][0], before["input"][3]["output"][0]);
    assert_eq!(req["input"][2], before["input"][2], "the script is never touched");

    // Stable across turns, and for a fresh session.
    let mut next = body(vec![call.clone(), result.clone()]);
    transform(&mut next, &mut session, None, &Options::default());
    assert_eq!(next, req);
    let mut fresh = body(vec![call, result]);
    transform(&mut fresh, &mut Session::default(), None, &Options::default());
    assert_eq!(fresh, req);
}

/// One exec result as Codex returns it to a script and `JSON.stringify` prints it.
fn exec_line(exit: Option<i64>, output: &str) -> String {
    let mut object = serde_json::Map::new();
    object.insert("chunk_id".into(), json!("4b1e9c"));
    object.insert("wall_time_seconds".into(), json!(9.912834));
    match exit {
        Some(code) => object.insert("exit_code".into(), json!(code)),
        None => object.insert("session_id".into(), json!(41)),
    };
    object.insert("original_token_count".into(), json!(5460));
    object.insert("output".into(), json!(output));
    serde_json::to_string(&object).unwrap()
}

#[test]
fn a_script_printing_its_one_result_whole_is_judged_by_its_command() {
    let printing =
        "const r = await tools.exec_command({cmd: 'pytest -v tests/test_orders.py', workdir: '/repo'});\ntext(r);";
    let passed = exec_line(Some(0), &pytest_run());
    let mut req = body(vec![exec("j1", printing), result("j1", &passed)]);
    let mut session = Session::default();
    let seen = transform(&mut req, &mut session, None, &Options::default());
    assert_eq!(seen.outputs_replaced, 1);
    let line: Value = serde_json::from_str(&printed(&req, 3)).unwrap();
    let output = line["output"].as_str().unwrap();
    assert!(output.starts_with("ok"), "{output}");
    assert!(
        output.contains("lge: output of `pytest` reduced"),
        "by its command: {output}"
    );
    let again = req.clone();
    let mut next = body(vec![exec("j1", printing), result("j1", &passed)]);
    transform(&mut next, &mut session, None, &Options::default());
    assert_eq!(next, again, "the decision is stable across turns");

    // Exit code 1 with a passing tally, a process still running, a line Codex
    // cut: each is sent as it is.
    let mut cut = passed.clone();
    cut.truncate(passed.len() / 2);
    cut.push_str("…812 tokens truncated…\"}");
    for text in [exec_line(Some(1), &pytest_run()), exec_line(None, &pytest_run()), cut] {
        let mut req = body(vec![exec("j2", printing), result("j2", &text)]);
        let before = req.clone();
        let seen = transform(&mut req, &mut Session::default(), None, &Options::default());
        assert_eq!(seen.outputs_replaced, 0, "{}", &text[..80]);
        assert_eq!(req, before);
    }
}

#[test]
fn each_printed_part_of_a_three_command_script_gets_its_own_decision() {
    let (call, result) = fixture("multi_command_parts.json");
    let mut req = body(vec![call.clone(), result.clone()]);
    let before = req.clone();
    let mut session = Session::default();
    let seen = transform(&mut req, &mut session, None, &Options::default());
    assert_eq!(seen.outputs_replaced, 2);
    assert_eq!(part(&req, 3, 1), part(&before, 3, 1), "the sed read stays whole");
    let search = part(&req, 3, 2);
    assert!(search.contains("lge: 60 of 320 matching lines shown"), "{search}");
    let tests = part(&req, 3, 3);
    assert!(tests.starts_with("ok: 300 passed"), "{tests}");
    assert!(
        tests.contains("lge: output of `pytest` reduced"),
        "matched to its command: {tests}"
    );
    assert_eq!(req["input"][3]["output"][0], before["input"][3]["output"][0]);
    assert_eq!(req["input"][2], before["input"][2]);
    let mut next = body(vec![call.clone(), result.clone()]);
    next["input"].as_array_mut().unwrap().push(user("ok"));
    transform(&mut next, &mut session, None, &Options::default());
    assert_eq!(next["input"][3], req["input"][3], "identical bytes next turn");

    // The script failed after printing: Codex added its error as one more
    // part, so the parts no longer match the text() calls one to one. Each
    // part is then judged by its content alone.
    let mut failed = result.clone();
    failed["output"]
        .as_array_mut()
        .unwrap()
        .push(json!({ "type": "input_text", "text": "Script error:\nError: boom" }));
    let mut req = body(vec![call, failed]);
    let seen = transform(&mut req, &mut Session::default(), None, &Options::default());
    assert_eq!(seen.outputs_replaced, 2);
    let tests = part(&req, 3, 3);
    assert!(
        tests.contains("lge: output of the command reduced"),
        "by content: {tests}"
    );
}

// ---- Scripts lge cannot read are counted, not changed ----

const LOOP_SCRIPT: &str = "const cmds = [\"rg -n x src\", \"git status --short\"];\nfor (let i = 0; i < cmds.length; i++) { const r = await tools.exec_command({cmd: cmds[i], workdir: \"/repo\"}); text(`STEP ${i+1}: ${cmds[i]}\\n${r.output}`); }";

/// About `lines * 12` tokens of ordinary text.
fn prose(lines: usize) -> String {
    (0..lines)
        .map(|i| format!("STEP 1: entry {i} describes the order flow through the checkout service and its retries\n"))
        .collect()
}

#[test]
fn a_loop_script_that_runs_a_command_per_array_element_is_counted_as_an_unread_script() {
    let big = prose(300);
    let tokens = lge_retrieve::budget::count(&big) as u64;
    assert!(tokens > 2000, "{tokens}");
    let opts = Options::default();
    let mut session = Session::default();
    let mut request = body(vec![exec("s1", LOOP_SCRIPT), result("s1", &big)]);
    transform(&mut request, &mut session, None, &opts);
    assert_eq!(session.saved.unread_script_outputs, 1);
    assert_eq!(session.saved.unread_script_tokens, tokens);
    assert!(session.saved.output_tokens_seen >= tokens);
    // The same output on the next request, with one more item: not counted again.
    let mut next = body(vec![exec("s1", LOOP_SCRIPT), result("s1", &big), user("and now?")]);
    transform(&mut next, &mut session, None, &opts);
    assert_eq!(session.saved.unread_script_outputs, 1);
    // A restarted daemon restores the ledger: the item counted before is not counted again.
    let mut restarted = Session::default();
    restarted.restore(session.ledger().clone());
    let mut again = body(vec![exec("s1", LOOP_SCRIPT), result("s1", &big), user("and now?")]);
    transform(&mut again, &mut restarted, None, &opts);
    assert_eq!(restarted.saved.unread_script_outputs, 0);
    assert_eq!(restarted.saved.unread_script_tokens, 0);
}

#[test]
fn a_script_that_runs_one_command_is_not_counted_as_unread() {
    let mut session = Session::default();
    let mut request = body(vec![exec("s1", &script("\"rg -n x src\"")), result("s1", &prose(300))]);
    transform(&mut request, &mut session, None, &Options::default());
    assert_eq!(session.saved.unread_script_outputs, 0);
    assert_eq!(session.saved.unread_script_tokens, 0);
}

#[test]
fn an_unread_script_output_below_four_hundred_tokens_is_not_counted() {
    let mut session = Session::default();
    let mut request = body(vec![exec("s1", LOOP_SCRIPT), result("s1", &prose(10))]);
    transform(&mut request, &mut session, None, &Options::default());
    assert_eq!(session.saved.unread_script_outputs, 0);
}

#[test]
fn counting_an_unread_script_changes_no_byte_of_the_request() {
    let big = prose(300);
    let mut counted = body(vec![exec("s1", LOOP_SCRIPT), result("s1", &big)]);
    let mut options = Options::default();
    for rule in RuleId::ALL {
        options.rules.set(rule, Mode::Off);
    }
    let before = counted.clone();
    transform(&mut counted, &mut Session::default(), None, &options);
    assert_eq!(counted, before);
}
