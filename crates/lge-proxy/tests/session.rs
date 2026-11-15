//! The proxy's decisions for one conversation, against a real indexed repository.
use lge_proxy::session::{Graph, Options, Session, CONTEXT_CLOSE, CONTEXT_OPEN};
use lge_retrieve::{budget::count, index_repo, IndexOptions};
use lge_store::Store;
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
fn a_file_read_from_the_top_becomes_a_skeleton_once() {
    let (_dir, root, store) = repo();
    let graph = Graph {
        store: &store,
        root: &root,
    };
    let opts = Options {
        shorten_reads: true,
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
        shorten_reads: true,
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
    // Whole-file skeletons are opt-in: with the defaults a read passes through.
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
    // Anything the proxy does not recognise passes through.
    assert_eq!(session.rewrite_output(None, "git diff", None, &matches, &opts), None);
    assert_eq!(
        session.rewrite_output(None, "python manage.py migrate", None, &run, &opts),
        None
    );
    assert_eq!(session.saved.outputs_shortened, 2);
}
