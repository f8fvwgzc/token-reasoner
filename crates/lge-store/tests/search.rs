//! Seed search and name lookup. The assertions hold for both search backends:
//! BM25 when the fts extension is installed on the machine, the substring
//! scorer when it is not.

mod common;

use common::*;
use lge_core::{kind, lang, Hit, ParsedFile, Resolved};
use lge_store::{tokens, GraphStore, Store};

fn names(hits: &[Hit]) -> Vec<&str> {
    hits.iter().map(|h| h.name.as_str()).collect()
}

/// Run a search and check what must hold for any query on any backend.
fn search(store: &Store, text: &str, limit: usize) -> Vec<Hit> {
    let hits = store.search(text, limit).unwrap();
    assert!(hits.len() <= limit, "{text}: more than {limit} hits");
    assert!(
        hits.windows(2).all(|w| w[0].score >= w[1].score),
        "{text}: not ordered by score"
    );
    assert!(hits.iter().all(|h| h.score > 0.0), "{text}: a hit without a score");
    assert!(
        hits.iter()
            .all(|h| h.kind != kind::MODULE && h.kind != kind::UNRESOLVED),
        "{text}: module or placeholder"
    );
    let mut distinct: Vec<&str> = hits.iter().map(|h| h.id.as_str()).collect();
    distinct.sort();
    distinct.dedup();
    assert_eq!(distinct.len(), hits.len(), "{text}: a symbol twice");
    hits
}

#[test]
fn tokens_drop_short_words_stopwords_and_repeats() {
    assert_eq!(
        tokens("Fix the bug in create_order when the Order is empty"),
        vec!["create", "order", "empty"]
    );
    assert_eq!(tokens("add support for retries"), vec!["retries"]);
    assert_eq!(
        tokens("TaskService.create_task(title)"),
        vec!["taskservice", "create", "task", "title"]
    );
    assert!(tokens("a an of to").is_empty());
    assert!(tokens("").is_empty());
}

#[test]
fn search_finds_symbols_by_name_qualified_name_and_doc() {
    let (_dir, store, fx, _) = indexed();
    println!(
        "search backend: {}",
        if store.fts_enabled() { "BM25" } else { "substring" }
    );

    // By the words of a snake_case name.
    let hits = search(&store, "create order", 10);
    assert_eq!(hits[0].name, "create_order", "{:?}", names(&hits));
    assert!(names(&hits).contains(&"test_create_order"));
    let create_order = fx.symbol("orders_api.routes.create_order");
    assert_eq!(
        Hit {
            score: 0.0,
            ..hits[0].clone()
        },
        Hit {
            id: create_order.id.clone(),
            name: "create_order".to_string(),
            qualified_name: "orders_api.routes.create_order".to_string(),
            kind: kind::FUNCTION.to_string(),
            file_path: ROUTES_PY.to_string(),
            service: "orders_api".to_string(),
            start_line: 5,
            end_line: 12,
            is_test: false,
            score: 0.0,
        }
    );

    // By a whole camelCase name, in any letter case.
    for query in ["recordDelivery", "RECORDDELIVERY", "please fix recordDelivery"] {
        let hits = search(&store, query, 5);
        assert_eq!(names(&hits), vec!["recordDelivery"], "{query}");
    }

    // By a word that only the doc comment has.
    let hits = search(&store, "confirmation", 5);
    assert_eq!(names(&hits), vec!["handleOrderCreated"]);
    let hits = search(&store, "announce", 5);
    assert_eq!(names(&hits), vec!["create_order"]);

    // By a word of the qualified name: everything defined in that module, never the module itself.
    let hits = search(&store, "mailer", 20);
    for expected in ["Mailer", "sendEmail"] {
        assert!(names(&hits).contains(&expected), "{expected} not in {:?}", names(&hits));
    }
    assert!(hits.iter().all(|h| h.file_path == MAILER_TS), "{:?}", names(&hits));

    // `render` is defined twice and also exists as a placeholder; only the two real ones are hits.
    let hits = search(&store, "render", 10);
    assert_eq!(names(&hits), vec!["render", "render"]);
    assert!(hits.iter().all(|h| !h.id.starts_with("unresolved:")));

    // The limit cuts from the bottom.
    let all = search(&store, "order", 50);
    assert!(all.len() >= 3, "{:?}", names(&all));
    let top = search(&store, "order", 2);
    assert_eq!(top.len(), 2);
    assert_eq!(top[0].score, all[0].score);

    // Nothing to search for, or nothing that matches.
    assert!(search(&store, "", 10).is_empty());
    assert!(search(&store, "the and for", 10).is_empty());
    assert!(search(&store, "zzzqqqxxx", 10).is_empty());
    assert!(search(&store, "create order", 0).is_empty());
}

#[test]
fn search_follows_the_index_as_files_change() {
    let (_dir, store) = open();
    assert!(search(&store, "invoice", 10).is_empty());

    // Searchable right after the insert, before any edges are resolved.
    let f = file("billing/invoice.py", "billing", lang::PYTHON);
    let module = sym(&f, "billing.invoice", kind::MODULE, 1, 20);
    let mut total = sym(&f, "billing.invoice.compute_total", kind::FUNCTION, 3, 9);
    total.doc = "Sum the invoice lines.".to_string();
    let void = sym(&f, "billing.invoice.void_invoice", kind::FUNCTION, 11, 20);
    let p = ParsedFile {
        file: Some(f),
        symbols: vec![module, total.clone(), void],
        ..ParsedFile::default()
    };
    store.insert_parsed(&p, COMMIT, NOW).unwrap();
    assert_eq!(names(&search(&store, "void invoice", 10))[0], "void_invoice");
    assert_eq!(names(&search(&store, "compute total", 10)), vec!["compute_total"]);

    // Still searchable after the resolver's output is stored.
    store.replace_edges(&Resolved::default(), COMMIT, NOW).unwrap();
    assert_eq!(names(&search(&store, "compute total", 10)), vec!["compute_total"]);

    // A changed file is searched by its new content.
    let mut changed = p.clone();
    changed.symbols.retain(|s| s.name != "compute_total");
    let f = changed.file.clone().unwrap();
    changed
        .symbols
        .push(sym(&f, "billing.invoice.apply_discount", kind::FUNCTION, 3, 9));
    store.insert_parsed(&changed, COMMIT, NOW + 1).unwrap();
    assert!(search(&store, "compute total", 10).is_empty());
    assert_eq!(names(&search(&store, "apply discount", 10)), vec!["apply_discount"]);

    // With BM25 the answers above came from the full-text index itself, not
    // from the substring fallback: the database keeps the index up to date.
    if store.fts_enabled() {
        let indexed = |word: &str| {
            let q = format!("CALL QUERY_FTS_INDEX('Symbol', 'sym_fts', '{word}') RETURN node.name ORDER BY node.name;");
            store
                .raw(&q)
                .unwrap()
                .lines()
                .skip(1)
                .map(str::to_string)
                .collect::<Vec<_>>()
        };
        assert_eq!(indexed("discount"), vec!["apply_discount"]);
        assert_eq!(indexed("void"), vec!["void_invoice"]);
        assert!(indexed("compute").is_empty(), "the replaced symbol left the index");
    }

    // A deleted file is not found any more.
    store.delete_file("billing/invoice.py").unwrap();
    assert!(search(&store, "void invoice", 10).is_empty());
    assert!(search(&store, "apply discount", 10).is_empty());
}

#[test]
fn find_matches_id_qualified_name_or_name_exactly() {
    let (_dir, store, fx, _) = indexed();
    let create_order = fx.symbol("orders_api.routes.create_order");

    for needle in [
        create_order.id.as_str(),
        "orders_api.routes.create_order",
        "create_order",
    ] {
        let hits = store.find(needle).unwrap();
        assert_eq!(hits.len(), 1, "{needle}");
        assert_eq!(
            (hits[0].id.as_str(), hits[0].file_path.as_str(), hits[0].start_line),
            (create_order.id.as_str(), ROUTES_PY, 5)
        );
        assert_eq!(hits[0].score, 0.0);
    }

    // A name defined twice gives both, in a fixed order. The placeholder of
    // the same name is not a symbol.
    let hits = store.find("render").unwrap();
    assert_eq!(
        hits.iter().map(|h| h.qualified_name.as_str()).collect::<Vec<_>>(),
        vec!["notifier.src.templates.render", "web_gateway.src.views.render"]
    );
    assert!(store.find("unresolved:render").unwrap().is_empty());

    // Modules can be looked up by name; search leaves them out, find does not.
    let hits = store.find("orders_api.routes").unwrap();
    assert_eq!(names(&hits), vec!["routes"]);
    assert_eq!(hits[0].kind, kind::MODULE);

    // Exact means exact.
    assert!(store.find("create").unwrap().is_empty());
    assert!(store.find("CREATE_ORDER").unwrap().is_empty());
    assert!(store.find("").unwrap().is_empty());
}

#[test]
fn find_prefix_ignores_case_and_prefers_short_names() {
    let (_dir, store, _fx, _) = indexed();

    let hits = store.find_prefix("send", 10).unwrap();
    assert_eq!(names(&hits), vec!["send", "sendEmail"]);
    assert_eq!(
        names(&store.find_prefix("SEND", 10).unwrap()),
        vec!["send", "sendEmail"]
    );
    assert_eq!(names(&store.find_prefix("sende", 10).unwrap()), vec!["sendEmail"]);
    assert_eq!(names(&store.find_prefix("send", 1).unwrap()), vec!["send"]);

    // It is a prefix of the name, not of the qualified name, and not a substring.
    assert!(store.find_prefix("orders_api", 10).unwrap().is_empty());
    assert!(store.find_prefix("handler", 10).unwrap().is_empty());
    assert!(store.find_prefix("email", 10).unwrap().is_empty());
    // Modules are symbols too: `ordersClient` is the module of ordersClient.ts.
    assert_eq!(names(&store.find_prefix("order", 10).unwrap()), vec!["ordersClient"]);
    assert_eq!(store.find_prefix("order", 10).unwrap()[0].kind, kind::MODULE);

    // Placeholders are left out: two real `render`s, not three.
    assert_eq!(store.find_prefix("rend", 10).unwrap().len(), 2);
    assert!(store.find_prefix("zzz", 10).unwrap().is_empty());
    assert!(store.find_prefix("send", 0).unwrap().is_empty());
}

/// The full-text indexes of the database, as `table|name|columns`.
fn fts_indexes(store: &Store) -> Vec<String> {
    store
        .raw("CALL SHOW_INDEXES() RETURN table_name, index_name, property_names, index_type;")
        .unwrap()
        .lines()
        .filter_map(|l| l.strip_suffix("|FTS"))
        .map(str::to_string)
        .collect()
}

#[test]
fn replace_edges_restores_a_missing_or_wrong_full_text_index() {
    let (_dir, store, _fx, resolved) = indexed();
    if !store.fts_enabled() {
        // Without the extension there is no index to look after.
        assert!(fts_indexes(&store).is_empty());
        return;
    }
    let expected = vec!["Symbol|sym_fts|[name,qualified_name,signature,doc]".to_string()];
    assert_eq!(fts_indexes(&store), expected, "created when the store was opened");
    let bm25 = store.search("create order", 5).unwrap();

    // Dropped behind the store's back: search still answers (substring), and
    // the next resolve puts the index back.
    store.raw("CALL DROP_FTS_INDEX('Symbol', 'sym_fts');").unwrap();
    assert_eq!(store.search("create order", 5).unwrap()[0].name, "create_order");
    store.replace_edges(&resolved, COMMIT, NOW + 5).unwrap();
    assert_eq!(fts_indexes(&store), expected);
    assert_eq!(store.search("create order", 5).unwrap(), bm25);

    // An index over other columns is replaced by the right one.
    store.raw("CALL DROP_FTS_INDEX('Symbol', 'sym_fts');").unwrap();
    store
        .raw("CALL CREATE_FTS_INDEX('Symbol', 'sym_fts', ['name']);")
        .unwrap();
    store.replace_edges(&resolved, COMMIT, NOW + 5).unwrap();
    assert_eq!(fts_indexes(&store), expected);
    assert_eq!(store.search("create order", 5).unwrap(), bm25);
}

#[test]
fn install_fts_keeps_an_installed_extension_working() {
    let (_dir, store, _fx, _) = indexed();
    if !store.fts_enabled() {
        // Installing downloads the extension. That is the job of `lge setup`, not of a test.
        return;
    }
    // Already installed: LadybugDB skips the download, the index is left alone.
    let before = store.search("create order", 5).unwrap();
    store.install_fts().unwrap();
    assert!(store.fts_enabled());
    assert_eq!(fts_indexes(&store).len(), 1);
    assert_eq!(store.search("create order", 5).unwrap(), before);
}
