//! Scratch probes (reviewer). Deleted before finishing.
use lge_filter::*;

fn failed() -> FilterOptions {
    FilterOptions {
        passed: Some(false),
        ..FilterOptions::default()
    }
}

fn show(label: &str, tool: &str, raw: &str) {
    let (failures, text) = filter_with(tool, raw, &failed());
    println!("##### {label}: {} failures, {} bytes", failures.len(), text.len());
    for f in failures.iter().take(12) {
        println!(
            "  NAME={:?} FILE={:?} LINE={:?} MSG={:?}",
            f.name, f.file, f.line, f.message
        );
    }
    println!("{}", text.chars().take(1800).collect::<String>());
}

#[test]
fn q1_nextest() {
    let raw = "   Compiling shop v0.1.0 (/home/dev/shop)\n    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.42s\n------------\n Nextest run ID 5d2f with nextest profile: default\n    Starting 3 tests across 1 binary\n        PASS [   0.005s] shop pricing::tests::ok\n        FAIL [   0.005s] shop cart::tests::totals_items\n\n--- STDOUT:              shop cart::tests::totals_items ---\n\nrunning 1 test\ntest cart::tests::totals_items ... FAILED\n\nfailures:\n\nfailures:\n    cart::tests::totals_items\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 2 filtered out; finished in 0.00s\n\n--- STDERR:              shop cart::tests::totals_items ---\nthread 'cart::tests::totals_items' panicked at src/cart.rs:31:9:\nassertion `left == right` failed\n  left: 1.0\n right: 1.01\n\n        FAIL [   0.004s] shop cart::tests::other\n--- STDERR:              shop cart::tests::other ---\nthread 'cart::tests::other' panicked at src/cart.rs:40:9:\nboom\n\n------------\n     Summary [   0.006s] 3 tests run: 1 passed, 2 failed, 0 skipped\n        FAIL [   0.005s] shop cart::tests::totals_items\n        FAIL [   0.004s] shop cart::tests::other\nerror: test run failed\n";
    show("nextest", "cargo nextest run", raw);
    let plain = "   Compiling shop v0.1.0 (/home/dev/shop)\n    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.42s\n------------\n Nextest run ID 5d2f with nextest profile: default\n    Starting 2 tests across 1 binary\n        PASS [   0.005s] shop pricing::tests::ok\n        FAIL [   0.005s] shop cart::tests::totals_items\n--- STDERR:              shop cart::tests::totals_items ---\nthread 'cart::tests::totals_items' panicked at src/cart.rs:31:9:\nboom\n\n------------\n     Summary [   0.006s] 2 tests run: 1 passed, 1 failed, 0 skipped\n        FAIL [   0.005s] shop cart::tests::totals_items\nerror: test run failed\n";
    show("nextest plain", "cargo nextest run", plain);
}

#[test]
fn q2_warnings_then_other_tool() {
    let raw = "\n> web@1.0.0 lint\n> eslint . && prettier --check .\n\n\n/home/dev/web/src/app.ts\n  30:15  warning  Unexpected any. Specify a different type  @typescript-eslint/no-explicit-any\n\n✖ 1 problem (0 errors, 1 warning)\n\nChecking formatting...\n[warn] src/routes.ts\n[warn] src/views.tsx\n[warn] Code style issues found in 2 files. Run Prettier with --write to fix.\n";
    show("eslint warn + prettier", "npm run lint", raw);
    let raw = "warning: unused variable: `x`\n --> src/lib.rs:3:9\n  |\n3 |     let x = 1;\n  |         ^\n  |\n  = note: `#[warn(unused_variables)]` on by default\n\nwarning: `shop` (lib) generated 1 warning\n    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.02s\nDiff in /home/dev/shop/src/lib.rs:1:\n-fn  main() {}\n+fn main() {}\n";
    show("clippy warn + fmt", "cargo clippy && cargo fmt --check", raw);
}

#[test]
fn q3_go_many() {
    let mut raw = String::new();
    for i in 0..3000 {
        raw.push_str(&format!("=== RUN   TestOk{i}\n--- PASS: TestOk{i} (0.00s)\n"));
        if i % 10 == 0 {
            raw.push_str(&format!(
                "=== RUN   TestBad{i}\n    x_test.go:{i}: got 1, want 2\n--- FAIL: TestBad{i} (0.00s)\n"
            ));
        }
    }
    raw.push_str("FAIL\nFAIL\texample.com/shop\t0.512s\nFAIL\n");
    let (_, text) = filter_with("go test ./...", &raw, &failed());
    let kept = (0..3000)
        .step_by(10)
        .filter(|i| text.contains(&format!("--- FAIL: TestBad{i} ")))
        .count();
    println!("##### go: kept {kept} of 300 failing names in {} bytes", text.len());
    let tap = "TAP version 13\nok 1 - a\nnot ok 2 - b\nok 3 - c\n".to_string()
        + &"ok 9 - filler\n".repeat(100)
        + "1..103\n# pass 102\n";
    show("tap", "node --test", &tap);
}

#[test]
fn q4_pytest_misc() {
    let raw = "============================= test session starts ==============================\ncollected 2 items\n\ntests/test_a.py ..                                                       [100%]\n\n---------- coverage: platform linux, python 3.11.9-final-0 -----------\nName    Stmts   Miss  Cover\nTOTAL     100     20    80%\n\nFAIL Required test coverage of 90% not reached. Total coverage: 80.00%\n============================== 2 passed in 0.12s ===============================\n";
    show("pytest cov", "pytest --cov", raw);
    let raw = "[gw0] [ 50%] PASSED tests/test_a.py::test_ok \n[gw1] [100%] FAILED tests/test_a.py::test_bad[a b] \n\n=================================== FAILURES ===================================\n_______________________________ test_bad[a b] ________________________________\n[gw1] linux -- Python 3.11.9 /home/dev/shop/.venv/bin/python\n\n    def test_bad(v):\n>       assert 0\nE       assert 0\n\ntests/test_a.py:5: AssertionError\n======================== 1 failed, 1 passed in 0.50s =========================\n";
    show("pytest xdist no summary", "pytest -n 2 -rN -v", raw);
    let raw = "F.F                                                                      [100%]\n1 failed, 1 passed, 1 warning in 0.01s\n";
    show("pytest -q --tb=no -rN", "pytest", raw);
    let raw = "..F\n=================================== FAILURES ===================================\n____________________________ TestA.test_x[1 - 2] _____________________________\ntests/test_a.py:5: in test_x\n    assert 0\nE   assert 0\n=========================== short test summary info ============================\nFAILED tests/test_a.py::TestA::test_x[1 - 2] - assert 0\nFAILED tests/test_a.py::TestA::test_y[a]b] - assert 0\nFAILED tests/test_a.py::test_z[a - b]\nFAILED tests/test_a.py::test_w[[ - x] - boom\n4 failed, 2 passed in 0.01s\n";
    show("pytest params 2", "pytest", raw);
}
