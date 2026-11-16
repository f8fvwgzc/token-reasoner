//! Replaying past Codex sessions through the rules, on rollout files built in
//! a temporary Codex home.
use lge_proxy::discover::replay;
use lge_proxy::session::shadow_decide;
use lge_proxy::shorten::{check_run, CheckDecision, RunStatus};
use lge_retrieve::budget::count;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

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

fn matches() -> String {
    let mut out = String::new();
    for f in 0..40 {
        for l in 0..8 {
            out.push_str(&format!(
                "services/svc_{f}/handlers.py:{}:    order = create_order(payload, request_id=request.id)\n",
                l * 5 + 2
            ));
        }
    }
    out
}

const USAGE_ERROR: &str = "error: unexpected argument '--nocapturee' found\n\n  tip: to pass '--nocapturee' as a value, use '-- --nocapturee'\n\nUsage: cargo test [OPTIONS] [TESTNAME] [-- [ARGS]...]\n\nFor more information, try '--help'.\n";

/// The frame Codex's exec_command puts around a command's text.
fn framed(exit: i32, text: &str) -> String {
    format!("Chunk ID: 1a2b\nWall time: 1.0 seconds\nProcess exited with code {exit}\nOriginal token count: 10\nOutput:\n{text}")
}

struct Rollout {
    lines: Vec<Value>,
}

impl Rollout {
    fn new(id: &str) -> Rollout {
        Rollout {
            lines: vec![
                json!({ "timestamp": "2026-11-10T10:00:00.000Z", "type": "session_meta", "payload": { "id": id, "cwd": "/work/shop" } }),
            ],
        }
    }

    fn item(&mut self, payload: Value) -> &mut Rollout {
        self.lines
            .push(json!({ "timestamp": "2026-11-10T10:00:01.000Z", "type": "response_item", "payload": payload }));
        self
    }

    fn request(&mut self) -> &mut Rollout {
        self.lines.push(json!({ "timestamp": "2026-11-10T10:00:02.000Z", "type": "event_msg", "payload": {
            "type": "token_count",
            "info": { "total_token_usage": { "input_tokens": 10, "output_tokens": 1 }, "last_token_usage": { "input_tokens": 10, "output_tokens": 1 } }
        } }));
        self
    }

    fn shell(&mut self, call: &str, cmd: &str, output: String) -> &mut Rollout {
        self.item(json!({ "type": "function_call", "name": "exec_command", "call_id": call, "arguments": json!({ "cmd": cmd }).to_string() }))
            .item(json!({ "type": "function_call_output", "call_id": call, "output": output }))
    }

    fn write(&self, home: &Path, name: &str) -> PathBuf {
        let dir = home.join("sessions/2026/11/10");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let text: String = self.lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(&path, text).unwrap();
        path
    }
}

/// Every file under `dir` with its size and modification time.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, (u64, std::time::SystemTime)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).unwrap().flatten() {
            let meta = entry.metadata().unwrap();
            if meta.is_dir() {
                stack.push(entry.path());
            }
            out.insert(entry.path(), (meta.len(), meta.modified().unwrap()));
        }
    }
    out
}

fn home() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    let mut a = Rollout::new("00000000-0000-7000-8000-00000000000a");
    a.request()
        .shell("c1", "pytest -q", framed(0, &passing_run()))
        .request()
        .shell("c2", "rg -n create_order", framed(0, &matches()))
        .shell("c3", "cargo test --nocapturee", framed(1, USAGE_ERROR))
        .shell("c4", "cargo test -- --nocapture", framed(0, "running 0 tests\n"))
        .request()
        .item(json!({ "type": "custom_tool_call", "name": "exec", "call_id": "c5", "input": "const r = await tools.exec_command({cmd: \"pytest -v\"});\ntext(r.output);\n" }))
        .item(json!({ "type": "custom_tool_call_output", "call_id": "c5", "output": [
            { "type": "input_text", "text": "Script completed\nWall time 9.9 seconds\nOutput:\n" },
            { "type": "input_text", "text": passing_run() }
        ] }))
        .item(json!({ "type": "function_call", "name": "lookup", "namespace": "mcp__docs__", "call_id": "c6", "arguments": "{}" }))
        .item(json!({ "type": "function_call_output", "call_id": "c6", "output": "one short answer\n" }))
        // An output whose call is not in the file is not paired.
        .item(json!({ "type": "function_call_output", "call_id": "elsewhere", "output": passing_run() }));
    a.write(
        home.path(),
        "rollout-2026-11-10T10-00-00-00000000-0000-7000-8000-00000000000a.jsonl",
    );
    let mut b = Rollout::new("00000000-0000-7000-8000-00000000000b");
    b.request().shell("d1", "ls", framed(0, "a.py\nb.py\n"));
    b.write(
        home.path(),
        "rollout-2026-11-10T11-00-00-00000000-0000-7000-8000-00000000000b.jsonl",
    );
    std::fs::write(home.path().join("sessions/2026/11/10/notes.txt"), "not a rollout").unwrap();
    home
}

#[test]
fn per_rule_totals_equal_the_shorteners_run_by_hand() {
    let home = home();
    let report = replay(home.path(), 30).unwrap();
    assert_eq!((report.files, report.unreadable), (2, 0));

    // By hand: the same texts, framed the same way, through the same rules.
    let ok = RunStatus {
        exit: Some(0),
        running: false,
        masked: false,
    };
    let failed = RunStatus {
        exit: Some(1),
        running: false,
        masked: false,
    };
    let script = RunStatus::default();
    let units: Vec<(&str, String, RunStatus)> = vec![
        ("pytest -q", passing_run(), ok),
        ("rg -n create_order", matches(), ok),
        ("cargo test --nocapturee", USAGE_ERROR.to_string(), failed),
        ("cargo test -- --nocapture", "running 0 tests\n".to_string(), ok),
        ("pytest -v", passing_run(), script),
        ("", "one short answer\n".to_string(), RunStatus::default()),
        ("ls", "a.py\nb.py\n".to_string(), ok),
    ];
    let mut by_rule: BTreeMap<String, u64> = BTreeMap::new();
    let (mut tokens, mut unrecognised) = (0u64, 0u64);
    for (cmd, text, status) in &units {
        tokens += count(text) as u64;
        let fired = shadow_decide(cmd, text, *status);
        if fired.is_empty() {
            unrecognised += count(text) as u64;
        }
        for (rule, before, after) in fired {
            *by_rule.entry(rule.as_str().to_string()).or_default() += (before - after) as u64;
        }
    }
    assert_eq!(report.outputs, units.len() as u64);
    assert_eq!(report.output_tokens, tokens);
    assert_eq!(report.by_rule, by_rule);
    assert_eq!(report.unrecognised_tokens, unrecognised);

    // And the check rule against the check reader itself.
    let CheckDecision::Shorter(short) = check_run("pytest", "pytest", &passing_run(), ok) else {
        panic!("a passing pytest run is shortened");
    };
    let one = (count(&passing_run()) - count(&short)) as u64;
    assert_eq!(report.by_rule["check"], 2 * one, "the shell run and the script's part");
    assert!(report.by_rule["search_cap"] > 0, "{:?}", report.by_rule);

    let names: Vec<&str> = report.families.iter().map(|f| f.family.as_str()).collect();
    let family = |name: &str| {
        report
            .families
            .iter()
            .find(|f| f.family == name)
            .unwrap_or_else(|| panic!("{name} not in {names:?}"))
    };
    assert_eq!(
        family("pytest").outputs,
        2,
        "the script's part is counted under its command"
    );
    assert_eq!(family("pytest").by_rule["check"], 2 * one);
    assert_eq!(family("rg").outputs, 1);
    assert_eq!(family("cargo test").outputs, 2);
    assert_eq!(family("(tool) mcp__docs__lookup").unrecognised_tokens, 4);
    assert_eq!(report.families[0].family, "pytest", "largest first");
    assert_eq!(report.scripts.scripts, 1);
    assert_eq!(report.scripts.single_exec, 1);
    assert_eq!(
        (report.requests.sessions, report.requests.total, report.requests.max),
        (2, 4, 3)
    );
    assert_eq!(report.requests.median, 2.0);
}

#[test]
fn a_corrected_command_is_found_once() {
    let home = home();
    let report = replay(home.path(), 30).unwrap();
    assert_eq!(report.corrected.len(), 1, "{:?}", report.corrected);
    let c = &report.corrected[0];
    assert_eq!(
        (c.wrong.as_str(), c.right.as_str(), c.class.as_str(), c.count),
        (
            "cargo test --nocapturee",
            "cargo test -- --nocapture",
            "unexpected argument",
            1
        )
    );
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["corrected"][0]["count"], 1);
}

#[test]
fn nothing_is_written() {
    let home = home();
    let before = snapshot(home.path());
    let report = replay(home.path(), 30).unwrap();
    assert_eq!(report.files, 2);
    assert_eq!(snapshot(home.path()), before, "the Codex home is unchanged");
    // An empty or missing home is an empty report, not an error.
    let empty = tempfile::tempdir().unwrap();
    let nothing = replay(&empty.path().join("missing"), 30).unwrap();
    assert_eq!((nothing.files, nothing.outputs), (0, 0));
    assert!(snapshot(empty.path()).is_empty());
}
