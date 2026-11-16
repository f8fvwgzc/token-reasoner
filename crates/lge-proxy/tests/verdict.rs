//! A run is called passed only when lge can vouch for it: never on a status the
//! shell masked (a pipe, `|| true`, statements joined by `;`), and never when a
//! line says the run failed. Real captures where marked.

use lge_proxy::responses::transform;
use lge_proxy::session::{Options, Session};
use lge_proxy::shorten::{check_run, CheckDecision, RunStatus};
use serde_json::{json, Value};

fn user(text: &str) -> Value {
    json!({ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": text }] })
}

fn call(id: &str, cmd: &str) -> Value {
    json!({ "type": "function_call", "call_id": id, "name": "exec_command",
            "arguments": json!({"cmd": cmd, "workdir": "/repo"}).to_string() })
}

fn out(id: &str, exit: i32, text: &str) -> Value {
    json!({ "type": "function_call_output", "call_id": id,
            "output": format!("Chunk ID: ab12\nWall time: 3.2 seconds\nProcess exited with code {exit}\nOriginal token count: 2000\nOutput:\n{text}") })
}

/// What the proxy sends for the output of `cmd`, which exited `exit`.
fn sent(cmd: &str, exit: i32, text: &str) -> String {
    let mut body = json!({
        "model": "gpt-test",
        "prompt_cache_key": "conv-verdict",
        "input": [
            user("<environment_context>\n  <cwd>/repo</cwd>\n</environment_context>"),
            user("fix the cart totals and run the checks"),
            call("c1", cmd),
            out("c1", exit, text),
        ],
        "tools": [],
        "stream": true,
    });
    let mut session = Session::default();
    transform(&mut body, &mut session, None, &Options::default());
    let output = body["input"][3]["output"].as_str().unwrap();
    output.split_once("Output:\n").unwrap().1.to_string()
}

fn testdata(dir: &str, name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(dir).join(name);
    std::fs::read_to_string(path).unwrap()
}

fn cargo_pass(n: usize) -> String {
    let mut s = String::from(
        "   Compiling shop v0.1.0 (/repo)\n    Finished `test` profile [unoptimized + debuginfo] target(s) in 1.52s\n     Running unittests src/lib.rs (target/debug/deps/shop-1a2b3c4d5e6f7a8b)\n\n",
    );
    s.push_str(&format!("running {n} tests\n"));
    for i in 0..n {
        s.push_str(&format!("test cart::tests::case_{i} ... ok\n"));
    }
    s.push_str(&format!(
        "\ntest result: ok. {n} passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n\n"
    ));
    s
}

#[test]
fn a_tail_of_a_failed_run_is_sent_whole() {
    // Real `cargo test --no-fail-fast 2>&1 | tail -n 100`: cargo exited 101, the
    // failing test is cut off, `error: 1 target failed:` is left.
    let cargo = testdata("../lge-filter/testdata", "cargo_tail_target_failed.txt");
    assert_eq!(sent("cargo test --no-fail-fast 2>&1 | tail -n 100", 0, &cargo), cargo);
    // Real `go test ./... 2>&1 | tail -n 46`: the failing package is cut off.
    let go = testdata("../lge-filter/testdata", "go_tail_fail.txt");
    assert_eq!(sent("go test ./... 2>&1 | tail -n 46", 0, &go), go);
    // A whole passing run behind a pipe is not vouched for either.
    let pass = cargo_pass(60);
    assert_eq!(sent("cargo test 2>&1 | tail -n 200", 0, &pass), pass);
    assert_eq!(sent("cargo test || true", 0, &pass), pass);
    // With pipefail the status is cargo's own.
    assert!(sent("set -o pipefail; cargo test 2>&1 | tail -n 200", 0, &pass).starts_with("ok: 60 passed"));
}

#[test]
fn a_status_unknown_run_that_says_it_failed_is_not_a_pass() {
    let unknown = RunStatus::default();
    let make = (0..12)
        .map(|f| {
            format!(
                "src/module_{f}.c:{}:9: warning: unused variable 'scratch_{f}' [-Wunused-variable]\n   {} |     int scratch_{f};\n      |         ^~~~~~~~~\n1 warning generated.\n",
                10 + f,
                10 + f
            )
        })
        .collect::<String>()
        + "make: *** [Makefile:14: build/parser.o] Error 1\nmake: *** Waiting for unfinished jobs....\n";
    assert_eq!(check_run("", "", &make, unknown), CheckDecision::AsIs);

    let mut cov = String::new();
    for i in 0..30 {
        cov.push_str(&format!(
            "tests/test_orders.py::test_case_{i} PASSED                                  [{:3}%]\n",
            70 + i
        ));
    }
    cov.push_str("\nTOTAL                     200     42    79%\n\nFAIL Required test coverage of 90% not reached. Total coverage: 79.00%\n============================== 100 passed in 2.31s ==============================\n");
    assert_eq!(check_run("", "", &cov, unknown), CheckDecision::AsIs);

    let mut gradle = String::new();
    for i in 0..55 {
        gradle.push_str(&format!(
            "          -    private static final int VALUE_{i} =  {i};\n          +    private static final int VALUE_{i} = {i};\n"
        ));
    }
    gradle.push_str("  Run './gradlew :app:spotlessApply' to fix these violations.\n\n* Try:\n> Run with --stacktrace option to get the stack trace.\n\nBUILD FAILED in 6s\n7 actionable tasks: 2 executed, 5 up-to-date\n");
    assert_eq!(check_run("", "", &gradle, unknown), CheckDecision::AsIs);

    // A whole passing run of unknown status (a code-mode part) is still shortened.
    assert!(matches!(
        check_run("cargo", "cargo", &cargo_pass(60), unknown),
        CheckDecision::Shorter(_)
    ));
}

#[test]
fn a_chain_is_judged_on_the_whole_output() {
    // Real `bash -c 'cargo fmt --check; cargo test'`: bash exited 0, fmt printed
    // its diff, then the tests passed.
    let text = testdata("testdata", "fmt_check_then_test.txt");
    assert_eq!(sent("cargo fmt --check; cargo test", 0, &text), text);
    assert_eq!(sent("cargo fmt --check\ncargo test", 0, &text), text);
    let black =
        "would reformat /repo/src/app.py\n\nOh no!\n1 file would be reformatted, 12 files would be left unchanged.\n";
    let mut py = String::from("============================= test session starts ==============================\nplatform linux -- Python 3.12.4, pytest-8.3.2, pluggy-1.5.0\nrootdir: /repo\ncollected 40 items\n\n");
    for i in 0..40 {
        py.push_str(&format!(
            "tests/test_app.py::test_case_{i} PASSED                         [{:3}%]\n",
            (i + 1) * 100 / 40
        ));
    }
    py.push_str("\n============================== 40 passed in 0.41s ==============================\n");
    let both = format!("{black}{py}");
    assert_eq!(sent("black --check .; pytest -v", 0, &both), both);
    // `&&` stops at the first failure: exit 0 means every step passed.
    assert!(sent("cargo build && cargo test", 0, &cargo_pass(60)).starts_with("ok: 60 passed"));
}

#[test]
fn lge_raw_in_a_chain_is_sent_whole() {
    let text = testdata("testdata", "fmt_check_then_test.txt");
    for cmd in [
        "export LGE_RAW=1; cargo fmt --check; cargo test",
        "LGE_RAW=1 cargo test 2>&1 | tail -n 200; echo done",
    ] {
        assert_eq!(sent(cmd, 0, &text), text, "{cmd}");
    }
}
