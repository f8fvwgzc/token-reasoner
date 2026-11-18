//! The proxy's decisions for one conversation, against a real indexed repository.
use lge_proxy::responses::{transform, Seen};
use lge_proxy::rules::{Mode, RuleModes};
use lge_proxy::session::{
    pack_budget, Graph, Ledger, Options, PrefixStats, Session, CONTEXT_CLOSE, CONTEXT_OPEN, FOLLOW_UP_PACK_TOKENS,
    LOCAL_CONTEXT_BUDGET, MIN_PACK_ROOM, PACK_HEADER,
};
use lge_retrieve::{budget::count, index_repo, IndexOptions};
use lge_store::Store;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

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

/// A long module: six functions of thirty lines each.
fn billing_module() -> String {
    let mut text = String::from("\"\"\"Billing operations.\"\"\"\n\n");
    for name in [
        "charge_card",
        "handle_refund",
        "issue_invoice",
        "apply_discount",
        "close_period",
        "export_ledger",
    ] {
        text.push_str(&format!(
            "def {name}(order_id, amount):\n    \"\"\"Do the {name} step.\"\"\"\n"
        ));
        for i in 0..30 {
            text.push_str(&format!(
                "    step_{i} = ledger.record(order_id, amount, stage=\"{name}\", sequence={i})\n"
            ));
        }
        text.push_str("    return order_id\n\n\n");
    }
    text
}

fn repo() -> (tempfile::TempDir, PathBuf, Store) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    copy_dir(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/polyglot"),
        &root,
    );
    std::fs::write(root.join("orders_api/billing.py"), billing_module()).unwrap();
    let store = Store::open(&dir.path().join("graph.lbug")).unwrap();
    index_repo(&store, &root, &IndexOptions::default()).unwrap();
    (dir, root, store)
}

#[test]
fn context_is_added_for_a_message_about_the_code_and_repeated_exactly() {
    let (_dir, root, store) = repo();
    let graph = Graph {
        store: &store,
        root: &root,
    };
    let opts = Options::default();
    let mut session = Session::default();

    let first = session
        .context_for(&graph, "create_order must validate the payload", &opts)
        .expect("the message names a symbol");
    assert!(
        first.starts_with(CONTEXT_OPEN) && first.ends_with(CONTEXT_CLOSE),
        "{first}"
    );
    assert!(
        first.contains("## Start here\norders_api/routes.py:10-15  function orders_api.routes.create_order"),
        "{first}"
    );
    assert!(count(&first) <= opts.context_budget + count(CONTEXT_OPEN) + count(CONTEXT_CLOSE) + 2);
    // The same message in the next request gets the same bytes, and is counted once.
    assert_eq!(
        session
            .context_for(&graph, "create_order must validate the payload", &opts)
            .as_deref(),
        Some(first.as_str())
    );
    assert_eq!(session.saved.contexts_added, 1);

    // Talk that is not about code gets nothing.
    // Neither does a message that shares only one word with what search found.
    for chat in [
        "ok, do it",
        "thanks, looks good",
        "yes",
        "why did that fail?",
        "the email is sent twice",
    ] {
        assert_eq!(session.context_for(&graph, chat, &opts), None, "{chat}");
    }
    // The same starting point again adds nothing new.
    assert_eq!(
        session.context_for(&graph, "also add logging to create_order", &opts),
        None
    );
    // A message found by search, not by name: two of its words are in the symbol.
    let found = session
        .context_for(&graph, "record the delivery of the email in the audit log", &opts)
        .expect("search finds recordDelivery");
    assert!(found.contains("recordDelivery"), "{found}");
    assert_eq!(session.saved.contexts_added, 2);

    // A decision stored in an earlier session comes with the context.
    lge_memory::retain(
        &store,
        &lge_memory::RetainInput {
            text: "notifier/src/audit.ts must never block the email: audit writes are fire-and-forget".into(),
            kind: "decision".into(),
            key: "notifier.audit".into(),
            ..lge_memory::RetainInput::default()
        },
        1_794_441_600,
    )
    .unwrap();
    let mut fresh = Session::default();
    let with_memory = fresh
        .context_for(&graph, "recordDelivery should log the delivery time", &opts)
        .expect("names a symbol");
    assert!(
        with_memory
            .contains("## From project memory (decisions, conventions, recent commits)\n- [decision] notifier/src/audit.ts must never block the email"),
        "{with_memory}"
    );

    let off = Options {
        inject_context: false,
        ..Options::default()
    };
    assert_eq!(
        Session::default().context_for(&graph, "create_order must validate the payload", &off),
        None
    );
}

#[test]
fn a_question_about_one_file_that_nothing_else_uses_gets_a_small_pack() {
    let (_dir, root, store) = repo();
    let graph = Graph {
        store: &store,
        root: &root,
    };
    let opts = Options::default();
    let local = "handle_refund should reject a negative amount";
    let shared = "create_order must validate the payload";

    // handle_refund is in billing.py and nothing outside that file uses it;
    // create_order is used by a test in another file and serves an endpoint.
    assert_eq!(pack_budget(&store, local, &opts), LOCAL_CONTEXT_BUDGET);
    assert_eq!(pack_budget(&store, shared, &opts), opts.context_budget);
    // A smaller configured budget is never raised.
    let tight = Options {
        context_budget: 400,
        ..Options::default()
    };
    assert_eq!(pack_budget(&store, local, &tight), 400);

    let overhead = count(CONTEXT_OPEN) + count(CONTEXT_CLOSE) + 2;
    let mut session = Session::default();
    let small = session.context_for(&graph, local, &opts).expect("names a symbol");
    assert!(
        small.contains("orders_api/billing.py") && small.contains("handle_refund"),
        "{small}"
    );
    assert!(count(&small) <= LOCAL_CONTEXT_BUDGET + overhead, "{}", count(&small));
    assert_eq!(
        session.saved.context_tokens,
        count(&small),
        "the pack's tokens are recorded"
    );
    // Decided from the graph alone: the same message gives the same bytes in another session.
    assert_eq!(
        Session::default().context_for(&graph, local, &opts).as_deref(),
        Some(small.as_str())
    );

    let full = Session::default()
        .context_for(&graph, shared, &opts)
        .expect("names a symbol");
    let full_pack = lge_retrieve::evidence_pack(
        &store,
        shared,
        &lge_retrieve::PackOptions {
            budget: opts.context_budget,
            header: Some(PACK_HEADER.to_string()),
            ..lge_retrieve::PackOptions::default()
        },
    )
    .unwrap();
    assert!(
        full.contains(full_pack.text.trim_end()),
        "the configured budget is used: {full}"
    );
    // After the conversation's first pack, a pack is a follow-up: small.
    let follow = session
        .context_for(&graph, shared, &opts)
        .expect("a starting point in a new file");
    assert!(count(&follow) <= FOLLOW_UP_PACK_TOKENS, "{}", count(&follow));
    assert_eq!(session.saved.contexts_added, 2);
    assert_eq!(session.saved.context_tokens, count(&small) + count(&follow));
}

#[test]
fn a_file_read_from_the_top_becomes_a_skeleton_once() {
    let (_dir, root, store) = repo();
    let graph = Graph {
        store: &store,
        root: &root,
    };
    let opts = Options {
        rules: RuleModes {
            skeleton: Mode::On,
            ..RuleModes::default()
        },
        ..Options::default()
    };
    let mut session = Session::default();
    let file = billing_module();

    // The user is working on one function of the module.
    session.context_for(&graph, "handle_refund should reject a negative amount", &opts);
    let short = session
        .rewrite_output(Some(&graph), "cat orders_api/billing.py", None, &file, &opts)
        .expect("a long file read is shortened");
    assert!(
        short.starts_with("# lge: shortened view of orders_api/billing.py (lines 1-"),
        "{short}"
    );
    assert!(
        short.contains("    step_29 = ledger.record(order_id, amount, stage=\"handle_refund\", sequence=29)"),
        "the function the task is about stays whole"
    );
    assert!(!short.contains("stage=\"charge_card\", sequence=5"), "{short}");
    assert!(
        short.contains("lines of function `charge_card` left out; read them with: sed -n '"),
        "{short}"
    );
    assert!(
        count(&short) * 2 < count(&file),
        "{} of {}",
        count(&short),
        count(&file)
    );

    // The identical item in a later request: the identical replacement.
    assert_eq!(
        session
            .rewrite_output(Some(&graph), "cat orders_api/billing.py", None, &file, &opts)
            .as_deref(),
        Some(short.as_str())
    );
    assert_eq!(session.saved.outputs_shortened, 1);
    assert!(session.saved.tokens_after < session.saved.tokens_before);

    // The same command again, in a later call of the same conversation: the
    // reader asked again, so it gets the whole file, and each call keeps its
    // own bytes on later turns.
    let mut calls = Session::default();
    calls.context_for(&graph, "handle_refund should reject a negative amount", &opts);
    let first = calls.rewrite_call_output(Some(&graph), "c1", "cat orders_api/billing.py", None, &file, &opts);
    assert_eq!(first.as_deref(), Some(short.as_str()));
    assert_eq!(
        calls.rewrite_call_output(Some(&graph), "c2", "cat orders_api/billing.py", None, &file, &opts),
        None,
        "a second read is sent whole"
    );
    assert_eq!(
        calls.rewrite_call_output(Some(&graph), "c1", "cat orders_api/billing.py", None, &file, &opts),
        first
    );
    assert_eq!(
        calls.rewrite_call_output(Some(&graph), "c2", "cat orders_api/billing.py", None, &file, &opts),
        None
    );
    assert_eq!(calls.saved.outputs_shortened, 1);

    // Reading the file again another way means the rest is needed: sent whole.
    let top: String = file.lines().take(120).map(|l| format!("{l}\n")).collect();
    assert_eq!(
        session.rewrite_output(Some(&graph), "sed -n '1,120p' orders_api/billing.py", None, &top, &opts),
        None
    );
    // A range further down is a fetch of known lines: never touched.
    let middle: String = file.lines().skip(40).take(80).map(|l| format!("{l}\n")).collect();
    assert_eq!(
        Session::default().rewrite_output(
            Some(&graph),
            "sed -n '41,120p' orders_api/billing.py",
            None,
            &middle,
            &opts
        ),
        None
    );
    // The command ran in a sub-directory.
    let mut other = Session::default();
    let cwd = root.join("orders_api");
    assert!(other
        .rewrite_output(Some(&graph), "cat billing.py", Some(&cwd), &file, &opts)
        .is_some());
}

#[test]
fn a_read_is_left_alone_when_the_proxy_cannot_be_sure_of_it() {
    let (_dir, root, store) = repo();
    let graph = Graph {
        store: &store,
        root: &root,
    };
    let opts = Options {
        rules: RuleModes {
            skeleton: Mode::On,
            ..RuleModes::default()
        },
        ..Options::default()
    };
    let file = billing_module();

    // Without a graph there is nothing to decide relevance with.
    assert_eq!(
        Session::default().rewrite_output(None, "cat orders_api/billing.py", None, &file, &opts),
        None
    );
    // Output that is not exactly the file (a wrapper line, line numbers).
    let wrapped = format!("Exit code: 0\n{file}");
    assert_eq!(
        Session::default().rewrite_output(Some(&graph), "cat orders_api/billing.py", None, &wrapped, &opts),
        None
    );
    // A file outside the repository, a file that is not indexed, a small file.
    assert_eq!(
        Session::default().rewrite_output(Some(&graph), "cat /etc/hosts", None, &file, &opts),
        None
    );
    assert_eq!(
        Session::default().rewrite_output(Some(&graph), "cat orders_api/nope.py", None, &file, &opts),
        None
    );
    let small = std::fs::read_to_string(root.join("orders_api/routes.py")).unwrap();
    assert_eq!(
        Session::default().rewrite_output(Some(&graph), "cat orders_api/routes.py", None, &small, &opts),
        None
    );
    // The file changed after it was indexed: line numbers cannot be trusted.
    std::fs::write(root.join("orders_api/billing.py"), format!("# edited\n{file}")).unwrap();
    assert_eq!(
        Session::default().rewrite_output(
            Some(&graph),
            "cat orders_api/billing.py",
            None,
            &format!("# edited\n{file}"),
            &opts
        ),
        None
    );
    // Whole-file skeletons are in shadow by default: a read passes through.
    assert_eq!(
        Session::default().rewrite_output(
            Some(&graph),
            "cat orders_api/billing.py",
            None,
            &format!("# edited\n{file}"),
            &Options::default()
        ),
        None
    );
    // Shortening switched off.
    let off = Options {
        shorten_outputs: false,
        ..Options::default()
    };
    assert_eq!(
        Session::default().rewrite_output(Some(&graph), "pytest -q", None, "x", &off),
        None
    );
}

#[test]
fn searches_and_check_runs_are_shortened_without_a_graph_too() {
    let opts = Options::default();
    let mut session = Session::default();
    let mut matches = String::new();
    for f in 0..40 {
        for l in 0..8 {
            matches.push_str(&format!(
                "services/svc_{f}/handlers.py:{}:    order = create_order(payload, request_id=request.id)\n",
                l * 5 + 2
            ));
        }
    }
    let capped = session
        .rewrite_output(None, "rg -n create_order", None, &matches, &opts)
        .expect("320 matching lines are capped");
    assert!(capped.contains("lge: 60 of 320 matching lines shown"), "{capped}");

    let mut run = String::from(
        "============================= test session starts ==============================\ncollected 300 items\n\n",
    );
    for i in 0..300 {
        run.push_str(&format!(
            "tests/test_orders.py::test_case_number_{i} PASSED                              [{:3}%]\n",
            i / 3
        ));
    }
    run.push_str("\n============================= 300 passed in 9.87s ==============================\n");
    let passed = session
        .rewrite_output(None, "cd orders_api && pytest -v", None, &run, &opts)
        .expect("a passing run is one line");
    assert!(passed.starts_with("ok") && count(&passed) < 60, "{passed}");
    // Output the proxy does not recognise passes through.
    assert_eq!(session.rewrite_output(None, "git diff", None, &matches, &opts), None);
    let plain: String = (0..300).map(|i| format!("Applying migration {i}... OK\n")).collect();
    assert_eq!(
        session.rewrite_output(None, "python manage.py migrate", None, &plain, &opts),
        None
    );
    // An unknown runner that prints a recognised test run is shortened by its content.
    let via_npm = session
        .rewrite_output(None, "npm test", None, &run, &opts)
        .expect("a pytest run behind npm");
    assert!(via_npm.starts_with("ok"), "{via_npm}");
    assert_eq!(session.saved.outputs_shortened, 3);
}

/// A module over 16 KB: ten functions of forty lines each.
fn ledger_module() -> String {
    let mut text = String::from("\"\"\"Ledger operations.\"\"\"\n\n");
    for f in 0..10 {
        text.push_str(&format!(
            "def post_entry_{f}(account, amount):\n    \"\"\"Post entry kind {f}.\"\"\"\n"
        ));
        for i in 0..40 {
            text.push_str(&format!(
                "    line_{i} = journal.write(account, amount, kind=\"entry_{f}\", position={i})\n"
            ));
        }
        text.push_str("    return account\n\n\n");
    }
    text
}

/// The repository with the ledger module indexed too.
fn ledger_repo() -> (tempfile::TempDir, PathBuf, Store, String) {
    let (dir, root, store) = repo();
    let ledger = ledger_module();
    assert!(ledger.len() > 16 * 1024, "{}", ledger.len());
    std::fs::write(root.join("orders_api/ledger.py"), &ledger).unwrap();
    index_repo(&store, &root, &IndexOptions::default()).unwrap();
    (dir, root, store, ledger)
}

fn exec_call(id: &str, command: &str, workdir: &Path) -> Value {
    json!({ "type": "function_call", "name": "exec_command", "call_id": id, "arguments": json!({ "cmd": command, "workdir": workdir }).to_string() })
}

fn exec_output(id: &str, text: &str) -> Value {
    json!({ "type": "function_call_output", "call_id": id, "output": text })
}

#[test]
fn a_shadow_skeleton_records_its_cohort_and_leaves_reads_alone() {
    let (_dir, root, store, ledger) = ledger_repo();
    let graph = Graph {
        store: &store,
        root: &root,
    };
    let opts = Options::default();
    let mut session = Session::default();
    assert_eq!(
        session.rewrite_call_output(Some(&graph), "c1", "cat orders_api/ledger.py", None, &ledger, &opts),
        None,
        "shadow forwards the original"
    );
    let cohort = session.saved.cohorts["python:16-64K"];
    assert_eq!(cohort.reads, 1);
    assert!(cohort.would_save_tokens > 1000, "{cohort:?}");
    let s = session.saved.by_rule["skeleton"];
    assert_eq!((s.items, s.shadow_items), (0, 1));
    // The read in shadow did not count as a shortened file: promoted, the
    // next first read of it gets the skeleton.
    let on = Options {
        rules: RuleModes {
            skeleton: Mode::On,
            ..RuleModes::default()
        },
        ..Options::default()
    };
    let short = session
        .rewrite_call_output(Some(&graph), "c2", "cat orders_api/ledger.py", None, &ledger, &on)
        .expect("a skeleton in on");
    assert!(short.contains("sed -n '"), "{short}");
    assert_eq!(session.saved.cohorts["python:16-64K"].reads, 2);
    // A numbered read gets a skeleton that keeps the real line numbers.
    let numbered: String = ledger
        .lines()
        .enumerate()
        .map(|(i, l)| format!("{:6}\t{l}\n", i + 1))
        .collect();
    let short = Session::default()
        .rewrite_call_output(Some(&graph), "c1", "nl -ba orders_api/ledger.py", None, &numbered, &on)
        .expect("a numbered read is shortened");
    assert!(short.contains("     3\tdef post_entry_0(account, amount):"), "{short}");
    assert!(
        short.contains("read them with: sed -n '5,45p' orders_api/ledger.py"),
        "{short}"
    );
}

#[test]
fn follow_ups_of_a_skeleton_are_counted_with_its_cohort() {
    let (_dir, root, store, ledger) = ledger_repo();
    let graph = Graph {
        store: &store,
        root: &root,
    };
    let filler = |id: &str| [exec_call(id, "git status", &root), exec_output(id, "On branch main\n")];
    // Shadow skeleton, then a patch of the read file within five calls.
    let mut items = vec![
        exec_call("c1", "cat orders_api/ledger.py", &root),
        exec_output("c1", &ledger),
        json!({ "type": "custom_tool_call", "call_id": "p1", "name": "apply_patch", "input": "*** Begin Patch\n*** Update File: orders_api/ledger.py\n@@\n-a\n+b\n*** End Patch\n" }),
        json!({ "type": "custom_tool_call_output", "call_id": "p1", "output": "Success." }),
    ];
    for id in ["c3", "c4", "c5", "c6"] {
        items.extend(filler(id));
    }
    let mut session = Session::default();
    let mut body = json!({ "input": items });
    transform(&mut body, &mut session, Some(&graph), &Options::default());
    let s = session.saved.by_rule["skeleton"];
    assert_eq!((s.shadow_judged, s.shadow_followed), (1, 1));
    assert_eq!(session.saved.cohorts["python:16-64K"].patched_within_5, 1);

    // A skeleton in on, then a read of a range its marker named: recovered.
    let on = Options {
        rules: RuleModes {
            skeleton: Mode::On,
            ..RuleModes::default()
        },
        ..Options::default()
    };
    let mut first =
        json!({ "input": [exec_call("c1", "cat orders_api/ledger.py", &root), exec_output("c1", &ledger)] });
    let mut session = Session::default();
    transform(&mut first, &mut session, Some(&graph), &on);
    let short = first["input"][1]["output"].as_str().unwrap().to_string();
    let range = short
        .split("read them with: ")
        .nth(1)
        .and_then(|r| r.lines().next())
        .unwrap()
        .to_string();
    let mut items = vec![
        exec_call("c1", "cat orders_api/ledger.py", &root),
        exec_output("c1", &ledger),
        exec_call("c2", &range, &root),
        exec_output("c2", "    line_0 = ...\n"),
    ];
    for id in ["c3", "c4", "c5", "c6"] {
        items.extend(filler(id));
    }
    let mut body = json!({ "input": items });
    transform(&mut body, &mut session, Some(&graph), &on);
    assert_eq!(body["input"][1]["output"], short.as_str(), "the same bytes");
    let s = session.saved.by_rule["skeleton"];
    assert_eq!((s.judged, s.recovered), (1, 1));
    assert_eq!(session.saved.cohorts["python:16-64K"].patched_within_5, 0);
}

#[test]
fn a_holdout_conversation_decides_packs_but_never_adds_them() {
    let (_dir, root, store) = repo();
    let graph = Graph {
        store: &store,
        root: &root,
    };
    let opts = Options {
        holdout: true,
        ..Options::default()
    };
    let mut session = Session::default();
    assert_eq!(
        session.context_for(&graph, "create_order must validate the payload", &opts),
        None
    );
    assert_eq!(session.saved.contexts_added, 0);
    assert_eq!(session.saved.contexts_would_add, 1);
    assert!(session.saved.context_tokens_would_add > 100);
    let mut body = json!({ "input": [{ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "create_order must validate the payload" }] }] });
    let before = body.clone();
    let seen = transform(&mut body, &mut Session::default(), Some(&graph), &opts);
    assert_eq!(body, before);
    assert_eq!((seen.contexts_present, seen.added_present), (0, 0));
}

// ------------------------------------------------- packs in long conversations

fn message(text: &str) -> Value {
    json!({ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": text }] })
}

fn environment(root: &Path) -> Value {
    message(&format!(
        "<environment_context>\n  <cwd>{}</cwd>\n</environment_context>",
        root.display()
    ))
}

/// Send `items` as one request of the conversation; the items forwarded.
fn turn(session: &mut Session, graph: &Graph, items: &[Value], opts: &Options) -> (Vec<Value>, Seen) {
    let mut body = json!({ "input": items });
    let seen = transform(&mut body, session, Some(graph), opts);
    (body["input"].as_array().unwrap().clone(), seen)
}

/// The text of a pack lge added, or `None` for any other item.
fn pack_text(item: &Value) -> Option<&str> {
    item["content"][0]["text"]
        .as_str()
        .filter(|t| t.starts_with("<lge_context>"))
}

/// Packs among forwarded items.
fn packs(items: &[Value]) -> Vec<&str> {
    items.iter().filter_map(pack_text).collect()
}

const ABOUT_ROUTES: &str = "create_order must validate the payload";
const ABOUT_AUDIT: &str = "record the delivery of the email in the audit log";
const ABOUT_MAILER: &str = "sendEmail should retry on a timeout";
const ABOUT_REPO: &str = "find_order should use the cache";

#[test]
fn a_follow_up_pack_is_small_and_only_for_a_file_not_seen_before() {
    let (_dir, root, store) = repo();
    let graph = Graph {
        store: &store,
        root: &root,
    };
    let opts = Options::default();
    // Alone, the message about the audit log gets a pack over the follow-up size.
    let alone = Session::default()
        .context_for(&graph, ABOUT_AUDIT, &opts)
        .expect("names a symbol");
    assert!(count(&alone) > FOLLOW_UP_PACK_TOKENS, "{}", count(&alone));

    let mut session = Session::default();
    let mut items = vec![environment(&root), message(ABOUT_ROUTES)];
    let (first, _) = turn(&mut session, &graph, &items, &opts);
    let pack = pack_text(&first[2]).expect("the first pack");
    assert!(pack.contains("orders_api/repo.py"), "{pack}");
    assert!(!pack.contains("notifier/src/"), "{pack}");

    // A search names mailer.ts; the message is about repo.py, which the pack named.
    items.push(exec_call("c1", "rg -n sendEmail notifier", &root));
    items.push(exec_output(
        "c1",
        "notifier/src/mailer.ts:26:export async function sendEmail(to: string, subject: string, body: string): Promise<void> {\n",
    ));
    items.push(message(ABOUT_REPO));
    let (second, _) = turn(&mut session, &graph, &items, &opts);
    assert_eq!(second.len(), items.len() + 1, "already known: no pack");
    assert_eq!(second[..first.len()], first[..], "the cached prefix must not change");
    assert_eq!(session.saved.contexts_known, 1);

    // About mailer.ts, which the search named.
    items.push(message(ABOUT_MAILER));
    let (third, _) = turn(&mut session, &graph, &items, &opts);
    assert_eq!(third.len(), items.len() + 1, "already known: no pack");
    assert_eq!(third[..second.len()], second[..], "the cached prefix must not change");
    assert_eq!(session.saved.contexts_known, 2);

    // About audit.ts, which nothing before named: a small pack.
    items.push(message(ABOUT_AUDIT));
    let (fourth, _) = turn(&mut session, &graph, &items, &opts);
    assert_eq!(fourth.len(), items.len() + 2);
    assert_eq!(fourth[..third.len()], third[..], "the cached prefix must not change");
    let follow = pack_text(fourth.last().unwrap()).expect("a follow-up pack");
    assert!(follow.contains("recordDelivery"), "{follow}");
    assert!(count(follow) <= FOLLOW_UP_PACK_TOKENS, "{}", count(follow));
    assert_eq!((session.saved.contexts_added, session.saved.contexts_known), (2, 2));
    // Decided from the items before each message: the same request gives the same bytes.
    let (again, _) = turn(&mut session, &graph, &items, &opts);
    assert_eq!(again, fourth);
    assert_eq!(session.saved.contexts_added, 2, "counted once");
}

#[test]
fn the_packs_of_a_request_never_exceed_the_conversation_budget() {
    let (_dir, root, store) = repo();
    let graph = Graph {
        store: &store,
        root: &root,
    };
    let budget = 1200;
    let opts = Options {
        pack_conversation_budget: budget,
        ..Options::default()
    };
    let mut session = Session::default();
    let mut items = vec![environment(&root), message(ABOUT_ROUTES)];
    let (first, seen) = turn(&mut session, &graph, &items, &opts);
    assert_eq!(packs(&first).len(), 1);
    let mut present = seen.added_present;
    assert!(present as usize <= budget);

    // A follow-up gets at most the room left, and at most the follow-up size.
    items.push(message(ABOUT_AUDIT));
    let (second, seen) = turn(&mut session, &graph, &items, &opts);
    assert_eq!(packs(&second).len(), 2);
    let follow = count(packs(&second)[1]);
    assert!(
        follow <= FOLLOW_UP_PACK_TOKENS.min(budget - present as usize),
        "{follow}"
    );
    present = seen.added_present;
    assert!(present as usize <= budget, "{present}");

    // Less than the minimum room is left: no pack, counted.
    assert!(budget - (present as usize) < MIN_PACK_ROOM, "{present}");
    items.push(message(ABOUT_MAILER));
    let (third, seen) = turn(&mut session, &graph, &items, &opts);
    assert_eq!(packs(&third).len(), 2);
    assert_eq!(third[..second.len()], second[..], "the cached prefix must not change");
    assert_eq!(seen.added_present, present);
    assert_eq!(session.saved.contexts_over_budget, 1);

    // A budget below the minimum room adds nothing; a small one shrinks even the first pack.
    let tiny = Options {
        pack_conversation_budget: MIN_PACK_ROOM - 1,
        ..Options::default()
    };
    let mut none = Session::default();
    assert_eq!(none.context_for(&graph, ABOUT_ROUTES, &tiny), None);
    assert_eq!(none.saved.contexts_over_budget, 1);
    let alone = count(&Session::default().context_for(&graph, ABOUT_ROUTES, &opts).unwrap());
    let small = Options {
        pack_conversation_budget: alone - 60,
        ..Options::default()
    };
    let squeezed = Session::default()
        .context_for(&graph, ABOUT_ROUTES, &small)
        .expect("a smaller first pack");
    assert!(count(&squeezed) <= alone - 60, "{} of {alone}", count(&squeezed));
}

/// Two turns: a pack after the first message, a follow-up pack after the
/// second. The items sent, and the forwarded items of the second turn.
fn two_turns(session: &mut Session, graph: &Graph, root: &Path, opts: &Options) -> (Vec<Value>, Vec<Value>) {
    let mut items = vec![environment(root), message(ABOUT_ROUTES)];
    let (first, _) = turn(session, graph, &items, opts);
    items.push(exec_call("c1", "git status", root));
    items.push(exec_output("c1", "On branch main\n"));
    items.push(message(ABOUT_AUDIT));
    let (second, _) = turn(session, graph, &items, opts);
    assert_eq!(second[..first.len()], first[..], "the cached prefix must not change");
    assert_eq!(packs(&second).len(), 2);
    (items, second)
}

/// [`two_turns`], then a third turn that gets nothing. The items sent, and the
/// forwarded items of the third turn: env, routes, pack, call, output, audit,
/// pack, call, output, thanks.
fn three_turns(session: &mut Session, graph: &Graph, root: &Path, opts: &Options) -> (Vec<Value>, Vec<Value>) {
    let (mut items, second) = two_turns(session, graph, root, opts);
    items.push(exec_call("c2", "git status", root));
    items.push(exec_output("c2", "On branch main\n"));
    items.push(message("thanks, looks good"));
    let (third, _) = turn(session, graph, &items, opts);
    assert_eq!(third[..second.len()], second[..], "the cached prefix must not change");
    assert_eq!(packs(&third).len(), 2);
    assert_eq!(session.prefix, PrefixStats::default());
    (items, third)
}

/// How Codex opens the summary of a compaction (codex-rs/prompts/templates/compact/summary_prefix.md).
const SUMMARY: &str = "Another language model started to solve this problem and produced a summary of its thinking process. Here is the summary:\nThe payload check is half done.";

/// Codex's history after it compacts (codex-rs/core/src/compact.rs,
/// build_compacted_history and insert_initial_context_before_last_real_user_or_summary):
/// the user messages it keeps, the initial context before the last of them,
/// and its summary.
fn compacted(root: &Path, kept: &[&str]) -> Vec<Value> {
    let mut items: Vec<Value> = kept.iter().map(|text| message(text)).collect();
    items.insert(kept.len().saturating_sub(1), environment(root));
    items.push(message(SUMMARY));
    items
}

#[test]
fn after_a_compaction_older_packs_are_retired_and_every_other_byte_replays() {
    let (_dir, root, store) = repo();
    let graph = Graph {
        store: &store,
        root: &root,
    };
    let opts = Options::default();
    let mut session = Session::default();
    let (_, second) = two_turns(&mut session, &graph, &root, &opts);
    let audit_pack = packs(&second)[1].to_string();

    // Codex compacts in the middle of the turn. The pack of the older message
    // goes; the newest typed message keeps its pack, which fits the budget.
    let mut items = compacted(&root, &[ABOUT_ROUTES, ABOUT_AUDIT]);
    let (fourth, seen) = turn(&mut session, &graph, &items, &opts);
    assert_eq!(fourth.len(), items.len() + 1, "{fourth:?}");
    assert_eq!(fourth[..3], items[..3], "the first message has no pack any more");
    assert_eq!(pack_text(&fourth[3]), Some(audit_pack.as_str()), "the same bytes");
    assert_eq!(fourth[4], items[3], "the summary gets no pack");
    assert_eq!((seen.lge_break_item, seen.client_break_item), (None, Some(0)));
    assert_eq!(
        session.prefix,
        PrefixStats {
            lge_breaks: 0,
            client_breaks: 1,
            first_lge_break_item: None,
        },
        "the client's break, not lge's"
    );
    assert_eq!(session.saved.contexts_retired, 1);

    // Two more turns: every byte sent before is sent again.
    items.push(exec_call("c3", "ls", &root));
    items.push(exec_output("c3", "a\nb\n"));
    items.push(message("ok, do it"));
    let (fifth, seen) = turn(&mut session, &graph, &items, &opts);
    assert_eq!(fifth[..fourth.len()], fourth[..], "the cached prefix must not change");
    assert_eq!((seen.lge_break_item, seen.client_break_item), (None, None));
    items.push(message(ABOUT_MAILER));
    let (sixth, seen) = turn(&mut session, &graph, &items, &opts);
    assert_eq!(sixth[..fifth.len()], fifth[..], "the cached prefix must not change");
    assert_eq!((seen.lge_break_item, seen.client_break_item), (None, None));
    let new = pack_text(sixth.last().unwrap()).expect("a pack about a file not seen before");
    assert!(count(new) <= FOLLOW_UP_PACK_TOKENS);
    assert_eq!((session.prefix.lge_breaks, session.prefix.client_breaks), (0, 1));
    assert_eq!(session.saved.contexts_retired, 1, "retired once");

    // After a restart the ledger gives the same bytes: the retired pack stays
    // out, and the others are built again to their size.
    let mut restarted = Session::default();
    restarted.restore(session.ledger().clone());
    let (again, _) = turn(&mut restarted, &graph, &items, &opts);
    assert_eq!(again, sixth, "the same bytes after a restart");
    assert_eq!(restarted.saved.contexts_retired, 0, "nothing retired again");
}

#[test]
fn after_a_compaction_the_newest_pack_goes_too_when_it_no_longer_fits() {
    let (_dir, root, store) = repo();
    let graph = Graph {
        store: &store,
        root: &root,
    };
    let mut session = Session::default();
    two_turns(&mut session, &graph, &root, &Options::default());
    let tight = Options {
        pack_conversation_budget: 100,
        ..Options::default()
    };
    let items = compacted(&root, &[ABOUT_ROUTES, ABOUT_AUDIT]);
    let (fourth, seen) = turn(&mut session, &graph, &items, &tight);
    assert_eq!(fourth, items, "no pack is left");
    assert_eq!(session.saved.contexts_retired, 2);
    assert_eq!((seen.lge_break_item, seen.client_break_item), (None, Some(0)));
    // A retry of the same request (the provider refused it) retires them again, counted once.
    session.revert_new();
    let (retry, _) = turn(&mut session, &graph, &items, &tight);
    assert_eq!(retry, items);
    assert_eq!(session.saved.contexts_retired, 2);
}

#[test]
fn an_edit_a_rollback_or_a_rewrite_late_in_the_history_keeps_every_byte_before_it() {
    let (_dir, root, store) = repo();
    let graph = Graph {
        store: &store,
        root: &root,
    };
    let opts = Options::default();
    let no_break = |session: &Session| {
        assert_eq!(
            session.prefix,
            PrefixStats {
                lge_breaks: 0,
                client_breaks: 1,
                first_lge_break_item: None,
            }
        );
        assert_eq!(session.saved.contexts_retired, 0, "no pack is retired");
    };

    // The person edits their last message.
    let mut session = Session::default();
    let (items, third) = three_turns(&mut session, &graph, &root, &opts);
    let mut edited = items[..7].to_vec();
    edited.push(message("thanks, now run the tests please"));
    let (fourth, seen) = turn(&mut session, &graph, &edited, &opts);
    assert_eq!(
        fourth[..third.len() - 1],
        third[..third.len() - 1],
        "the cached prefix must not change"
    );
    assert_eq!((seen.lge_break_item, seen.client_break_item), (None, Some(7)));
    assert_eq!(packs(&fourth).len(), 2);
    no_break(&session);
    // The next turn replays every byte.
    edited.push(exec_call("c3", "ls", &root));
    edited.push(exec_output("c3", "a\nb\n"));
    edited.push(message("ok"));
    let (fifth, seen) = turn(&mut session, &graph, &edited, &opts);
    assert_eq!(fifth[..fourth.len()], fourth[..], "the cached prefix must not change");
    assert_eq!((seen.lge_break_item, seen.client_break_item), (None, None));

    // Codex rolls the last turn back, and the person types another message.
    let mut session = Session::default();
    let (items, third) = three_turns(&mut session, &graph, &root, &opts);
    let mut rolled_back = items[..5].to_vec();
    rolled_back.push(message("now run the tests"));
    let (fourth, seen) = turn(&mut session, &graph, &rolled_back, &opts);
    assert_eq!(fourth[..7], third[..7], "the cached prefix must not change");
    assert_eq!((seen.lge_break_item, seen.client_break_item), (None, Some(5)));
    no_break(&session);

    // Codex rewrites one late tool output.
    let mut session = Session::default();
    let (mut items, third) = three_turns(&mut session, &graph, &root, &opts);
    items[6] = exec_output("c2", "On branch main (output dropped)\n");
    items.push(message("go on"));
    let (fourth, seen) = turn(&mut session, &graph, &items, &opts);
    assert_eq!(fourth[..8], third[..8], "the cached prefix must not change");
    assert_eq!((seen.lge_break_item, seen.client_break_item), (None, Some(6)));
    assert_eq!(packs(&fourth).len(), 2);
    no_break(&session);
}

#[test]
fn a_compaction_right_after_a_restart_retires_older_packs_the_same_way() {
    let (_dir, root, store) = repo();
    let graph = Graph {
        store: &store,
        root: &root,
    };
    let opts = Options::default();
    let mut session = Session::default();
    two_turns(&mut session, &graph, &root, &opts);
    let mut restarted = Session::default();
    restarted.restore(session.ledger().clone());

    let items = compacted(&root, &[ABOUT_ROUTES, ABOUT_AUDIT]);
    let (after_restart, _) = turn(&mut restarted, &graph, &items, &opts);
    let (without, _) = turn(&mut session, &graph, &items, &opts);
    assert_eq!(packs(&after_restart).len(), 1, "the older pack is retired");
    assert_eq!(after_restart, without, "the same bytes with or without a restart");
    assert_eq!(restarted.saved.contexts_retired, 1);

    // A later request after one more restart keeps them.
    let mut again = Session::default();
    again.restore(restarted.ledger().clone());
    let (later, _) = turn(&mut again, &graph, &items, &opts);
    assert_eq!(later, without);
    assert_eq!(again.saved.contexts_retired, 0);
}

#[test]
fn after_a_compaction_a_message_about_a_retired_packs_code_gets_a_pack() {
    let (_dir, root, store) = repo();
    let graph = Graph {
        store: &store,
        root: &root,
    };
    let opts = Options::default();
    let mut session = Session::default();
    two_turns(&mut session, &graph, &root, &opts);
    let mut items = compacted(&root, &[ABOUT_ROUTES, ABOUT_AUDIT]);
    let (fourth, _) = turn(&mut session, &graph, &items, &opts);
    assert_eq!(packs(&fourth).len(), 1);
    assert!(
        !fourth.iter().any(|i| i.to_string().contains("orders_api/routes.py")),
        "the retired pack's file is named nowhere"
    );
    let ledger = session.ledger().clone();

    items.push(message("create_order must validate the payload total too"));
    let (fifth, _) = turn(&mut session, &graph, &items, &opts);
    assert_eq!(fifth[..fourth.len()], fourth[..], "the cached prefix must not change");
    assert_eq!(packs(&fifth).len(), 2, "the retired pack's code is not in the request");
    let new = pack_text(fifth.last().unwrap()).expect("a pack after the new message");
    assert!(new.contains("orders_api/routes.py"), "{new}");

    // Decided from the items before the message: a restart gives the same.
    let mut restarted = Session::default();
    restarted.restore(ledger);
    let (again, _) = turn(&mut restarted, &graph, &items, &opts);
    assert_eq!(again, fifth);
}

#[test]
fn a_copy_of_a_message_gets_no_second_pack() {
    let (_dir, root, store) = repo();
    let graph = Graph {
        store: &store,
        root: &root,
    };
    let budget = 1000;
    let opts = Options {
        pack_conversation_budget: budget,
        ..Options::default()
    };
    let mut session = Session::default();
    let mut items = vec![environment(&root), message(ABOUT_ROUTES)];
    let (first, _) = turn(&mut session, &graph, &items, &opts);
    assert_eq!(packs(&first).len(), 1);
    let ledger = session.ledger().clone();
    items.push(exec_call("c1", "git status", &root));
    items.push(exec_output("c1", "On branch main\n"));
    items.push(message(ABOUT_ROUTES));
    let (second, seen) = turn(&mut session, &graph, &items, &opts);
    assert_eq!(second[..first.len()], first[..], "the cached prefix must not change");
    assert_eq!(second[first.len()..], items[2..], "the copy gets none");
    assert!(seen.added_present as usize <= budget, "{}", seen.added_present);
    let (again, _) = turn(&mut session, &graph, &items, &opts);
    assert_eq!(again, second);

    // A conversation of an older version, where a copy got its text's pack,
    // keeps doing so: its bytes replay.
    let mut older = ledger;
    older.no_pack_for_copies = false;
    let mut restarted = Session::default();
    restarted.restore(older);
    let (old, _) = turn(&mut restarted, &graph, &items, &opts);
    assert_eq!(old[..first.len()], first[..]);
    assert_eq!(packs(&old).len(), 2, "the copy replays the pack");
}

// ------------------------------------------------- pack sizes, stored packs, restarts

/// A module of four functions whose signatures are `params` long arguments:
/// each starting point's heading, which a pack never cuts, is large.
fn settlement_module(params: usize) -> String {
    let mut text = String::from("\"\"\"Settlement batches.\"\"\"\n\n");
    for name in SETTLEMENT {
        let args: Vec<String> = (0..params).map(|i| format!("{name}_argument_{i}")).collect();
        text.push_str(&format!(
            "def {name}({}):\n    \"\"\"Do the {name} step.\"\"\"\n    return None\n\n\n",
            args.join(", ")
        ));
    }
    text
}

const SETTLEMENT: [&str; 4] = [
    "reconcile_settlement_batch",
    "normalise_gateway_records",
    "allocate_partial_refunds",
    "publish_settlement_report",
];

/// The repository with the settlement module (signatures of `params`
/// arguments) indexed in a folder nothing else names, `depth` folders of
/// long names below `orders_api`.
fn settlement_repo(params: usize, depth: usize) -> (tempfile::TempDir, PathBuf, Store) {
    let (dir, root, store) = repo();
    let mut folder = root.join("orders_api/settlement/reconciliation");
    for level in 0..depth {
        folder = folder.join(format!(
            "batch_processing_layer_for_the_legacy_payment_gateway_adapters_and_their_ledgers_{level}"
        ));
    }
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("batch_settlement.py"), settlement_module(params)).unwrap();
    index_repo(&store, &root, &IndexOptions::default()).unwrap();
    (dir, root, store)
}

/// A follow-up that names the four settlement functions.
fn about_settlement() -> String {
    format!("{} must round every amount to whole cents", SETTLEMENT.join(", "))
}

#[test]
fn a_follow_up_about_a_new_file_gets_a_small_pack_in_a_large_workspace() {
    let (_dir, root, store) = settlement_repo(10, 0);
    let graph = Graph {
        store: &store,
        root: &root,
    };
    let opts = Options::default();
    // With all four starting points, the headings alone make a pack larger
    // than a follow-up may be.
    let mut alone = Session::default();
    let four = alone
        .context_for(&graph, &about_settlement(), &opts)
        .expect("names symbols");
    assert!(count(&four) > FOLLOW_UP_PACK_TOKENS, "{}", count(&four));

    let mut session = Session::default();
    let mut items = vec![environment(&root), message(ABOUT_ROUTES)];
    let (first, _) = turn(&mut session, &graph, &items, &opts);
    assert_eq!(packs(&first).len(), 1);
    items.push(message(&about_settlement()));
    let (second, _) = turn(&mut session, &graph, &items, &opts);
    assert_eq!(second[..first.len()], first[..], "the cached prefix must not change");
    let follow = pack_text(second.last().unwrap()).expect("a follow-up pack, with fewer starting points");
    assert!(count(follow) <= FOLLOW_UP_PACK_TOKENS, "{}", count(follow));
    assert!(follow.contains("batch_settlement.py"), "{follow}");
    assert_eq!(
        (
            session.saved.contexts_added,
            session.saved.contexts_over_budget,
            session.saved.contexts_too_large
        ),
        (2, 0, 0)
    );
    // The ledger keeps how many starting points it was built with, so a
    // restart without its text builds the same pack.
    let mut ledger = session.ledger().clone();
    assert_eq!(ledger.pack_seeds.len(), 1, "{:?}", ledger.pack_seeds);
    ledger.pack_texts.clear();
    let mut restarted = Session::default();
    restarted.restore(ledger);
    let (again, _) = turn(&mut restarted, &graph, &items, &opts);
    assert_eq!(again, second);
}

#[test]
fn a_follow_up_too_large_for_its_size_is_not_counted_as_over_the_conversation_budget() {
    // One starting point alone, its path and name repeated in the pack's
    // conclusions and its heading, is larger than a follow-up may be.
    let (_dir, root, store) = settlement_repo(10, 6);
    let graph = Graph {
        store: &store,
        root: &root,
    };
    let opts = Options::default();
    let mut session = Session::default();
    let mut items = vec![environment(&root), message(ABOUT_ROUTES)];
    let (first, seen) = turn(&mut session, &graph, &items, &opts);
    assert_eq!(packs(&first).len(), 1);
    assert!(
        opts.pack_conversation_budget - seen.added_present as usize > FOLLOW_UP_PACK_TOKENS,
        "the budget has room: {}",
        seen.added_present
    );
    items.push(message(&format!("{} must round to whole cents", SETTLEMENT[0])));
    let (second, _) = turn(&mut session, &graph, &items, &opts);
    assert_eq!(packs(&second).len(), 1, "no follow-up pack");
    assert_eq!(
        (session.saved.contexts_over_budget, session.saved.contexts_too_large),
        (0, 1)
    );
}

#[test]
fn after_a_restart_a_pack_keeps_its_bytes_though_its_file_changed() {
    let (_dir, root, store) = repo();
    let graph = Graph {
        store: &store,
        root: &root,
    };
    let opts = Options::default();
    let mut session = Session::default();
    let items = vec![environment(&root), message(ABOUT_ROUTES)];
    let (first, _) = turn(&mut session, &graph, &items, &opts);
    assert_eq!(packs(&first).len(), 1);
    // The agent edits create_order, and the graph follows.
    let routes = root.join("orders_api/routes.py");
    let text = std::fs::read_to_string(&routes).unwrap();
    let edited = text.replacen(
        "def create_order",
        "# checked first\n# then stored\ndef create_order",
        1,
    );
    assert_ne!(edited, text);
    std::fs::write(&routes, edited).unwrap();
    index_repo(&store, &root, &IndexOptions::default()).unwrap();
    // A pack built now differs, but a restarted daemon sends the pack as it was.
    assert_ne!(
        Session::default().context_for(&graph, ABOUT_ROUTES, &opts).as_deref(),
        pack_text(&first[2])
    );
    let mut restarted = Session::default();
    restarted.restore(session.ledger().clone());
    let (again, seen) = turn(&mut restarted, &graph, &items, &opts);
    assert_eq!(again, first);
    assert_eq!(restarted.saved, Default::default(), "counted once");
    assert_eq!(seen.lge_break_item, None);
    // A retired pack's text is not kept.
    let mut compacted_items = compacted(&root, &[ABOUT_ROUTES, "ok, go on"]);
    compacted_items.push(message("thanks"));
    turn(&mut restarted, &graph, &compacted_items, &opts);
    assert!(
        restarted.ledger().pack_texts.is_empty(),
        "{:?}",
        restarted.ledger().retired
    );
}

#[test]
fn a_pack_does_not_repeat_the_message_it_follows() {
    let (_dir, root, store) = repo();
    let graph = Graph {
        store: &store,
        root: &root,
    };
    let opts = Options::default();
    let detail = "Each payload field arrives as text from the browser form and the handler trusts it today. ";
    let long = format!("create_order must validate the payload. {}", detail.repeat(55));
    assert!(count(&long) > 900 && long.len() < 8000, "{}", count(&long));
    let items = vec![environment(&root), message(&long)];
    let mut session = Session::default();
    let (first, _) = turn(&mut session, &graph, &items, &opts);
    let pack = pack_text(&first[2]).expect("names create_order");
    assert!(!pack.contains(detail.trim()), "{pack}");
    assert!(pack.contains(PACK_HEADER), "{pack}");
    let frame = count(CONTEXT_OPEN) + count(CONTEXT_CLOSE) + 2;
    assert!(count(pack) <= opts.context_budget + frame + 300, "{}", count(pack));

    // A conversation of an older version keeps its packs' heading: one
    // whose ledger keeps no text is built again with the message in it,
    // byte for byte as that version built it.
    let mut older = Session::default();
    older.restore(serde_json::from_value::<Ledger>(json!({})).unwrap());
    assert_eq!(
        older.ledger().pack_format,
        0,
        "a ledger written before the field existed"
    );
    let (old_first, _) = turn(&mut older, &graph, &items, &opts);
    let old_pack = pack_text(&old_first[2]).expect("names create_order");
    assert!(
        old_pack.contains(&format!("# Context for: {}", long.trim())),
        "{old_pack}"
    );
    let mut stored = serde_json::to_value(older.ledger()).unwrap();
    let fields = stored.as_object_mut().unwrap();
    fields.remove("pack_texts");
    fields.remove("pack_format");
    fields.remove("pack_seeds");
    let mut restarted = Session::default();
    restarted.restore(serde_json::from_value(stored).unwrap());
    let (again, _) = turn(&mut restarted, &graph, &items, &opts);
    assert_eq!(again, old_first);
}

#[test]
fn a_pack_decided_before_a_restart_comes_back_once_the_graph_is_ready() {
    let (_dir, root, store) = repo();
    let graph = Graph {
        store: &store,
        root: &root,
    };
    let opts = Options::default();
    let mut session = Session::default();
    let mut items = vec![environment(&root), message(ABOUT_ROUTES)];
    let (first, _) = turn(&mut session, &graph, &items, &opts);
    assert_eq!(packs(&first).len(), 1);
    let send = |s: &mut Session, items: &[Value], graph: Option<&Graph>| {
        let mut body = json!({ "input": items });
        transform(&mut body, s, graph, &opts);
        body["input"].as_array().unwrap().clone()
    };
    items.push(message("go on"));

    // A ledger of an older version keeps no text: without the graph the
    // pack is left out, and it comes back as soon as the graph is ready.
    let mut older = session.ledger().clone();
    older.pack_texts.clear();
    let mut restarted = Session::default();
    restarted.restore(older);
    let without = send(&mut restarted, &items[..2], None);
    assert!(packs(&without).is_empty());
    let with = send(&mut restarted, &items, Some(&graph));
    assert_eq!(
        with[..first.len()],
        first[..],
        "the pack decided before, the same bytes"
    );

    // A ledger that keeps the text sends it with or without the graph.
    let mut restarted = Session::default();
    restarted.restore(session.ledger().clone());
    let without = send(&mut restarted, &items, None);
    assert_eq!(without[..first.len()], first[..]);
}

#[test]
fn a_restart_keeps_the_limit_of_packs_per_conversation() {
    let (_dir, root, store) = repo();
    let graph = Graph {
        store: &store,
        root: &root,
    };
    let opts = Options {
        max_contexts: 2,
        pack_conversation_budget: 100_000,
        ..Options::default()
    };
    let mut session = Session::default();
    two_turns(&mut session, &graph, &root, &opts);
    // A compaction keeps only the newest message; then a message about a third file.
    let mut items = compacted(&root, &[ABOUT_AUDIT]);
    turn(&mut session, &graph, &items, &opts);
    let ledger = session.ledger().clone();
    items.push(message(ABOUT_MAILER));
    let (kept, _) = turn(&mut session, &graph, &items, &opts);
    assert_eq!(packs(&kept).len(), 1, "the conversation had its packs");
    let mut restarted = Session::default();
    restarted.restore(ledger);
    let (after, _) = turn(&mut restarted, &graph, &items, &opts);
    assert_eq!(after, kept, "the same with or without a restart");

    // A held-out conversation: its next would-be pack is a follow-up after a restart too.
    let held = Options {
        holdout: true,
        ..Options::default()
    };
    let mut first = Session::default();
    let mut items = vec![environment(&root), message(ABOUT_ROUTES)];
    turn(&mut first, &graph, &items, &held);
    assert_eq!(first.saved.contexts_would_add, 1);
    items.push(message(ABOUT_AUDIT));
    let mut restarted = Session::default();
    restarted.restore(first.ledger().clone());
    restarted.restore_would_add(first.saved.contexts_would_add);
    turn(&mut restarted, &graph, &items, &held);
    assert_eq!(restarted.saved.contexts_would_add, 1, "counted once");
    assert!(
        restarted.saved.context_tokens_would_add <= FOLLOW_UP_PACK_TOKENS,
        "{}",
        restarted.saved.context_tokens_would_add
    );
}
