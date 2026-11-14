//! Identifier-aware search, the staged facts of a symbol, the symbol graph and
//! the metrics the ranking step stores.

mod common;

use common::*;
use lge_core::{
    fact, kind, lang, method, split_words, unresolved_id, EdgeType, FactSite, Hit, ParsedFile, Resolved, SymbolMetrics,
    SymbolRec,
};
use lge_store::{GraphStore, Store};
use std::collections::BTreeSet;

fn names(hits: &[Hit]) -> Vec<&str> {
    hits.iter().map(|h| h.name.as_str()).collect()
}

fn name_set(hits: &[Hit]) -> BTreeSet<&str> {
    hits.iter().map(|h| h.name.as_str()).collect()
}

/// What the full-text index reads for a symbol besides its raw columns.
fn stored_search_text(store: &Store, id: &str) -> String {
    let printed = store
        .raw(&format!("MATCH (s:Symbol) WHERE s.id = '{id}' RETURN s.search_text;"))
        .unwrap();
    printed.lines().nth(1).unwrap_or_default().to_string()
}

fn base_search_text(s: &SymbolRec) -> String {
    lge_core::search_text(&s.name, &s.qualified_name, &s.file_path, &s.signature, &s.doc)
}

/// A search that must hold what every search holds, on either backend.
fn search_in(store: &Store, text: &str, service: Option<&str>, limit: usize) -> Vec<Hit> {
    let hits = store.search_in(text, service, limit).unwrap();
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
    if let Some(service) = service {
        assert!(hits.iter().all(|h| h.service == service), "{text}: outside {service}");
    }
    hits
}

#[test]
fn identifiers_are_found_by_their_words() {
    on_both_backends(|store| {
        let (fx, _) = index_fixture(store);

        // The words of a camelCase name, as someone would say them.
        let hits = search_in(store, "order handler", None, 10);
        assert_eq!(
            name_set(&hits[..2]),
            BTreeSet::from(["createOrderHandler", "getOrderHandler"]),
            "{:?}",
            names(&hits)
        );
        assert_eq!(
            name_set(&search_in(store, "handler", None, 10)),
            BTreeSet::from(["createOrderHandler", "getOrderHandler"])
        );
        // The whole name, in any letter case, finds that symbol and no other.
        for query in ["getOrderHandler", "GETORDERHANDLER", "open getOrderHandler, please"] {
            assert_eq!(
                names(&search_in(store, query, None, 5)),
                vec!["getOrderHandler"],
                "{query}"
            );
        }
        // A word of the file name: everything in ordersClient.ts, never its module.
        assert_eq!(
            name_set(&search_in(store, "client", None, 10)),
            BTreeSet::from(["fetchOrder", "postOrder", "probe"])
        );
        // `search` is `search_in` without a service.
        for query in ["order handler", "client", "create order", "zzzqqq", ""] {
            assert_eq!(
                store.search(query, 10).unwrap(),
                store.search_in(query, None, 10).unwrap(),
                "{query}"
            );
        }

        // What is stored for the index: the distinct words of name, qualified
        // name, path, signature and doc.
        let handler = fx.symbol("web_gateway.src.routes.getOrderHandler");
        assert_eq!(
            stored_search_text(store, &handler.id),
            "get order handler web gateway src routes ts function"
        );
        for s in fx.files.iter().flat_map(|p| p.symbols.iter()) {
            assert_eq!(
                stored_search_text(store, &s.id),
                base_search_text(s),
                "{}",
                s.qualified_name
            );
        }
        // A placeholder has no words of its own.
        assert_eq!(stored_search_text(store, &unresolved_id("render")), "");
    });
}

#[test]
fn rationale_comments_are_searched_as_the_symbol_they_explain() {
    on_both_backends(|store| {
        let f = file("billing/tax.py", "billing", lang::PYTHON);
        let module = sym(&f, "billing.tax", kind::MODULE, 1, 60);
        let rate = sym(&f, "billing.tax.rate_for", kind::FUNCTION, 3, 12);
        let round = sym(&f, "billing.tax.round_half_up", kind::FUNCTION, 14, 20);
        let plain = sym(&f, "billing.tax.plain", kind::FUNCTION, 22, 30);
        let why = fact_site(
            &rate,
            fact::RATIONALE,
            "NOTE: the Zurich canton rounds differently, see QX-77",
            "",
            5,
            true,
        );
        let hack = fact_site(&rate, fact::RATIONALE, "HACK: legacyLedger expects cents", "", 9, true);
        let audit = fact_site(
            &round,
            fact::RATIONALE,
            "WHY: bankers rounding was rejected",
            "",
            15,
            true,
        );
        let not_a_comment = fact_site(&round, fact::DB_READ, "quokka_table", "", 16, true);
        let on_module = fact_site(&module, fact::RATIONALE, "IMPORTANT: generated nightly", "", 1, true);
        let mut facts = vec![
            why.clone(),
            hack.clone(),
            // The same row twice is the same row: its words are stored once.
            why.clone(),
            audit.clone(),
            not_a_comment,
            on_module,
        ];
        // Comments nobody would want to write, on the symbol they sit in.
        for (i, text) in AWKWARD.iter().enumerate() {
            facts.push(fact_site(&plain, fact::RATIONALE, text, "", 23 + i as i64, true));
        }
        let p = ParsedFile {
            file: Some(f),
            symbols: vec![module, rate.clone(), round.clone(), plain.clone()],
            facts,
            ..ParsedFile::default()
        };
        store.insert_parsed(&p, COMMIT, NOW).unwrap();

        assert_eq!(names(&search_in(store, "zurich canton", None, 5)), vec!["rate_for"]);
        // A camelCase word inside a comment is split like a name.
        assert_eq!(names(&search_in(store, "legacy ledger", None, 5)), vec!["rate_for"]);
        assert_eq!(names(&search_in(store, "bankers", None, 5)), vec!["round_half_up"]);
        // Facts of other kinds are not search text, and a module is never a hit.
        assert!(search_in(store, "quokka", None, 5).is_empty());
        assert!(search_in(store, "nightly", None, 5).is_empty());

        // The stored text: the words of the symbol, then those of each comment in source order.
        let words = |f: &FactSite| split_words(&f.key).join(" ");
        assert_eq!(
            stored_search_text(store, &rate.id),
            format!("{} {} {}", base_search_text(&rate), words(&why), words(&hack))
        );
        assert_eq!(
            stored_search_text(store, &round.id),
            format!("{} {}", base_search_text(&round), words(&audit))
        );
        let awkward: Vec<String> = AWKWARD.iter().flat_map(|t| split_words(t)).collect();
        assert_eq!(
            stored_search_text(store, &plain.id),
            format!("{} {}", base_search_text(&plain), awkward.join(" "))
        );

        // The words follow the file: without the comment the symbol is not found by it.
        let mut changed = p.clone();
        changed.facts.retain(|f| f.kind != fact::RATIONALE);
        store.insert_parsed(&changed, COMMIT, NOW + 1).unwrap();
        assert!(search_in(store, "zurich canton", None, 5).is_empty());
        assert_eq!(stored_search_text(store, &rate.id), base_search_text(&rate));
        assert_eq!(names(&search_in(store, "rate", None, 5)), vec!["rate_for"]);
    });
}

#[test]
fn search_in_keeps_to_one_service() {
    on_both_backends(|store| {
        index_fixture(store);
        let everywhere = search_in(store, "order", None, 50);
        let services: BTreeSet<&str> = everywhere.iter().map(|h| h.service.as_str()).collect();
        assert_eq!(services, BTreeSet::from(["notifier", "orders_api", "web_gateway"]));

        // One service: the same hits with the same scores, the others left out.
        for service in ["notifier", "orders_api", "web_gateway"] {
            let expected: Vec<Hit> = everywhere.iter().filter(|h| h.service == service).cloned().collect();
            assert!(!expected.is_empty(), "{service}");
            assert_eq!(search_in(store, "order", Some(service), 50), expected, "{service}");
            // The limit is applied to what is left after the filter.
            assert_eq!(search_in(store, "order", Some(service), 1), expected[..1], "{service}");
        }
        // BM25 knows the forms of a word (the doc of `save_order` says `Persist
        // the order row.`); the substring scorer compares letters.
        let stemmed = search_in(store, "persisted", Some("orders_api"), 5);
        if store.fts_enabled() {
            assert_eq!(names(&stemmed), vec!["save_order"]);
            assert!(search_in(store, "persisted", Some("notifier"), 5).is_empty());
        } else {
            assert!(stemmed.is_empty());
        }
        // `render` is defined in two services.
        let hits = search_in(store, "render", Some("notifier"), 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].file_path, TEMPLATES_TS);
        assert_eq!(search_in(store, "render", None, 10).len(), 2);

        // A service that does not exist, whatever it is called.
        for service in ["billing", "ORDERS_API", "orders", ""].iter().chain(AWKWARD) {
            assert!(search_in(store, "order", Some(service), 50).is_empty(), "{service:?}");
        }
        assert!(search_in(store, "", Some("orders_api"), 10).is_empty());
        assert!(search_in(store, "the and for", Some("orders_api"), 10).is_empty());
        assert!(search_in(store, "order", Some("orders_api"), 0).is_empty());
        // Awkward text is a query like any other: it is searched by the words in it.
        for text in AWKWARD {
            let hits = search_in(store, text, Some("orders_api"), 10);
            let by_its_words = search_in(store, &lge_store::tokens(text).join(" "), Some("orders_api"), 10);
            assert_eq!(hits, by_its_words, "{text:?}");
        }
        assert_eq!(
            names(&search_in(store, "carriage\r\nreturn", Some("orders_api"), 10)),
            vec!["get_order"],
            "its doc line says `Return one order.`"
        );
    });
}

#[test]
fn facts_of_gives_the_staged_sites_of_the_given_symbols() {
    let (_dir, store, fx, _) = indexed();
    let create = fx.id("orders_api.routes.create_order");
    let get = fx.id("orders_api.routes.get_order");
    let handle = fx.id("notifier.src.consumer.handleOrderCreated");
    let save = fx.id("orders_api.repo.save_order");
    let load = fx.id("orders_api.repo.load_order");

    // In source order, whatever the order of the ids.
    let exposed = store
        .facts_of(&[get.clone(), create.clone()], fact::API_EXPOSE)
        .unwrap();
    assert_eq!(exposed, fx.get(ROUTES_PY).facts);
    // One kind at a time.
    let consumer = &fx.get(CONSUMER_TS).facts;
    assert_eq!(
        store
            .facts_of(std::slice::from_ref(&handle), fact::QUEUE_CONSUME)
            .unwrap(),
        vec![consumer[0].clone()]
    );
    assert_eq!(
        store.facts_of(std::slice::from_ref(&handle), fact::DB_READ).unwrap(),
        vec![consumer[1].clone()]
    );
    // Across files: by file, then by line.
    let reads = store
        .facts_of(&[load.clone(), handle.clone(), save.clone()], fact::DB_READ)
        .unwrap();
    assert_eq!(reads, vec![consumer[1].clone(), fx.get(REPO_PY).facts[1].clone()]);

    // Unknown and repeated ids neither add nor lose a row; no ids, no rows.
    let asked = ids(&[&create, "no-such-id", &create, "", &get, &create]);
    assert_eq!(store.facts_of(&asked, fact::API_EXPOSE).unwrap(), exposed);
    assert!(store.facts_of(&[], fact::API_EXPOSE).unwrap().is_empty());
    assert!(store
        .facts_of(&ids(&["no-such-id"]), fact::API_EXPOSE)
        .unwrap()
        .is_empty());
    // A kind nothing has, and text that is no kind at all.
    assert!(store
        .facts_of(std::slice::from_ref(&create), fact::RATIONALE)
        .unwrap()
        .is_empty());
    for text in AWKWARD {
        assert!(store.facts_of(std::slice::from_ref(&create), text).unwrap().is_empty());
        assert!(store.facts_of(&ids(&[text]), fact::API_EXPOSE).unwrap().is_empty());
    }

    // The facts follow the file they were staged from.
    store.delete_file(ROUTES_PY).unwrap();
    assert!(store
        .facts_of(&[create.clone(), get.clone()], fact::API_EXPOSE)
        .unwrap()
        .is_empty());
    store.insert_parsed(fx.get(ROUTES_PY), COMMIT, NOW).unwrap();
    assert_eq!(store.facts_of(&[create, get], fact::API_EXPOSE).unwrap(), exposed);
}

#[test]
fn facts_keep_awkward_text_and_come_in_thousands() {
    let (_dir, store) = open();
    let (mut files, _, symbol_ids) = call_chain(5000);
    let mut expected: Vec<FactSite> = Vec::new();
    for p in &mut files {
        for (i, s) in p.symbols.clone().iter().enumerate() {
            let text = AWKWARD[(s.start_line as usize + i) % AWKWARD.len()];
            let why = fact_site(s, fact::RATIONALE, &format!("NOTE: {text}"), text, s.start_line, false);
            expected.push(why.clone());
            p.facts.push(why);
            // Another kind on every third symbol, which must not come back.
            if s.start_line % 3 == 0 {
                p.facts
                    .push(fact_site(s, fact::DB_READ, "steps", "", s.start_line, true));
            }
        }
    }
    store.insert_parsed_bulk(&files, COMMIT, NOW).unwrap();

    // Every id twice and an unknown id in between.
    let mut asked = Vec::new();
    for (i, id) in symbol_ids.iter().enumerate() {
        asked.push(id.clone());
        asked.push(format!("unknown-{i}"));
        asked.push(id.clone());
    }
    let facts = store.facts_of(&asked, fact::RATIONALE).unwrap();
    expected.sort_by(|a, b| (&a.file_path, a.line, &a.id).cmp(&(&b.file_path, b.line, &b.id)));
    assert_eq!(facts.len(), 5000);
    assert_eq!(facts, expected, "key, detail and the literal flag come back as staged");
    assert_eq!(store.facts_of(&asked, fact::DB_READ).unwrap().len(), 1667);
    assert_eq!(store.facts_of(&asked[..3], fact::RATIONALE).unwrap().len(), 1);
}

#[test]
fn symbol_graph_is_every_resolved_edge_between_symbols() {
    let (_dir, store) = open();
    assert!(store.symbol_graph().unwrap().is_empty());

    let (fx, mut resolved) = index_fixture(&store);
    let between_symbols = [
        EdgeType::Calls,
        EdgeType::Inherits,
        EdgeType::Implements,
        EdgeType::References,
        EdgeType::TestedBy,
    ];
    let mut expected: Vec<(String, String, EdgeType, f64)> = resolved
        .edges
        .iter()
        .filter(|e| between_symbols.contains(&e.edge_type) && e.method != method::UNRESOLVED)
        .map(|e| (e.from.clone(), e.to.clone(), e.edge_type, e.confidence))
        .collect();
    expected.sort_by(|a, b| (&a.0, &a.1, a.2).cmp(&(&b.0, &b.1, b.2)));
    let graph = store.symbol_graph().unwrap();
    assert_eq!(graph, expected);
    // Every type that joins two symbols is there, with static and inferred edges alike.
    let types: BTreeSet<EdgeType> = graph.iter().map(|e| e.2).collect();
    assert_eq!(types, BTreeSet::from(between_symbols));
    let confidences: BTreeSet<u64> = graph.iter().map(|e| (e.3 * 100.0).round() as u64).collect();
    assert_eq!(confidences, BTreeSet::from([40, 50, 90, 95]));
    // No placeholder on either end, no file, service, endpoint, table or queue.
    let symbols: BTreeSet<String> = fx
        .files
        .iter()
        .flat_map(|p| p.symbols.iter().map(|s| s.id.clone()))
        .collect();
    assert!(graph.iter().all(|e| symbols.contains(&e.0) && symbols.contains(&e.1)));

    // An edge to a placeholder is not part of the graph, whatever method it claims.
    resolved.edges.push(edge(
        &fx.id("notifier.src.mailer.SmtpMailer"),
        &unresolved_id("render"),
        EdgeType::References,
        0.5,
        method::INFERRED,
        MAILER_TS,
        1,
        1,
    ));
    store.replace_edges(&resolved, COMMIT, NOW + 6).unwrap();
    assert_eq!(store.last_skipped_edges(), 0);
    assert_eq!(store.symbol_graph().unwrap(), expected);

    // No edges, no graph.
    store.replace_edges(&Resolved::default(), COMMIT, NOW + 7).unwrap();
    assert!(store.symbol_graph().unwrap().is_empty());
}

fn metrics(id: &str, rank: f64, community: i64, degree: i64) -> SymbolMetrics {
    SymbolMetrics {
        id: id.to_string(),
        rank,
        community,
        degree,
    }
}

fn real_symbols(fx: &Fixture) -> Vec<&SymbolRec> {
    fx.files.iter().flat_map(|p| p.symbols.iter()).collect()
}

#[test]
fn metrics_are_stored_on_symbols_and_read_back() {
    let (_dir, store, fx, _) = indexed();
    let all: Vec<String> = real_symbols(&fx).iter().map(|s| s.id.clone()).collect();

    // Not computed yet: every symbol starts at rank 0, no community, degree 0.
    let fresh = store.metrics(&all).unwrap();
    assert_eq!(
        fresh,
        all.iter().map(|id| metrics(id, 0.0, -1, 0)).collect::<Vec<_>>(),
        "one row per symbol, in the order asked"
    );

    let a = fx.id("orders_api.routes.create_order");
    let b = fx.id("orders_api.repo.save_order");
    let c = fx.id("web_gateway.src.routes.getOrderHandler");
    let placeholder = unresolved_id("render");
    store
        .set_metrics(&[
            metrics(&a, 0.25, 0, 3),
            metrics("no-such-id", 0.9, 7, 7),
            metrics(&b, 0.125, 1, 1),
            metrics(&placeholder, 0.9, 7, 7),
            metrics("", 0.9, 7, 7),
        ])
        .unwrap();
    // Unknown ids and placeholders are skipped on the way in and on the way out;
    // a repeated id is answered once.
    let asked = ids(&[&b, "no-such-id", &a, &b, &placeholder, ""]);
    assert_eq!(
        store.metrics(&asked).unwrap(),
        vec![metrics(&b, 0.125, 1, 1), metrics(&a, 0.25, 0, 3)]
    );
    assert_eq!(
        count(
            &store,
            "MATCH (s:Symbol) WHERE s.kind = 'unresolved' AND s.rank <> 0.0 RETURN count(s);"
        ),
        0
    );
    // A symbol that was not listed is as it was.
    assert_eq!(
        store.metrics(std::slice::from_ref(&c)).unwrap(),
        vec![metrics(&c, 0.0, -1, 0)]
    );

    // Stored again: the listed symbol is replaced (the later row of a repeated
    // id counts), every other one keeps what it had.
    store
        .set_metrics(&[metrics(&a, 0.5, 2, 9), metrics(&a, 0.75, 4, 10)])
        .unwrap();
    assert_eq!(
        store.metrics(&ids(&[&a, &b, &c])).unwrap(),
        vec![
            metrics(&a, 0.75, 4, 10),
            metrics(&b, 0.125, 1, 1),
            metrics(&c, 0.0, -1, 0)
        ]
    );
    // Nothing to store and nothing to read.
    store.set_metrics(&[]).unwrap();
    assert!(store.metrics(&[]).unwrap().is_empty());
    assert!(store.metrics(&ids(&["no-such-id"])).unwrap().is_empty());
    for text in AWKWARD {
        store.set_metrics(&[metrics(text, 1.0, 1, 1)]).unwrap();
        assert!(store.metrics(&ids(&[text])).unwrap().is_empty(), "{text:?}");
    }

    // Numbers at the edges of their types come back bit for bit.
    let extremes = [
        (f64::MAX, i64::MAX, i64::MAX),
        (f64::MIN_POSITIVE, i64::MIN, i64::MIN),
        (5e-324, 0, 0),
        (-1.5, -1, -7),
        (1.0 / 3.0, 12, 34),
    ];
    for (rank, community, degree) in extremes {
        store.set_metrics(&[metrics(&c, rank, community, degree)]).unwrap();
        let read = store.metrics(std::slice::from_ref(&c)).unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(
            (read[0].rank.to_bits(), read[0].community, read[0].degree),
            (rank.to_bits(), community, degree)
        );
    }
}

#[test]
fn top_ranked_orders_real_symbols_by_rank() {
    let (_dir, store, fx, _) = indexed();
    let qualified = |hits: &[Hit]| hits.iter().map(|h| h.qualified_name.clone()).collect::<Vec<_>>();

    // Before any ranking every symbol has rank 0: the order is by qualified
    // name, and modules, tests and placeholders are not listed.
    let mut expected: Vec<String> = real_symbols(&fx)
        .iter()
        .filter(|s| s.kind != kind::MODULE && !s.is_test)
        .map(|s| s.qualified_name.clone())
        .collect();
    expected.sort();
    let unranked = store.top_ranked(None, None, 100).unwrap();
    assert_eq!(qualified(&unranked), expected);
    assert!(unranked.iter().all(|h| h.score == 0.0));

    let id = |qn: &str| fx.id(qn);
    store
        .set_metrics(&[
            metrics(&id("orders_api.repo.save_order"), 0.4, 0, 2),
            metrics(&id("orders_api.routes.create_order"), 0.3, 0, 4),
            metrics(&id("web_gateway.src.routes.getOrderHandler"), 0.3, 1, 1),
            metrics(&id("notifier.src.mailer.sendEmail"), 0.1, 1, 2),
            // A test, a module and a placeholder with the highest ranks of all.
            metrics(&id("orders_api.tests.test_routes.test_create_order"), 0.9, 0, 1),
            metrics(&id("orders_api.routes"), 0.8, 0, 1),
            metrics(&unresolved_id("render"), 0.7, 0, 1),
        ])
        .unwrap();
    let ranked = |service: Option<&str>, community: Option<i64>, limit: usize| -> Vec<(String, f64)> {
        store
            .top_ranked(service, community, limit)
            .unwrap()
            .into_iter()
            .map(|h| (h.name, h.score))
            .collect()
    };
    let named =
        |items: &[(&str, f64)]| -> Vec<(String, f64)> { items.iter().map(|(n, r)| (n.to_string(), *r)).collect() };

    // Highest rank first; equal ranks by qualified name. `score` is the rank.
    assert_eq!(
        ranked(None, None, 4),
        named(&[
            ("save_order", 0.4),
            ("create_order", 0.3),
            ("getOrderHandler", 0.3),
            ("sendEmail", 0.1)
        ])
    );
    let all = store.top_ranked(None, None, usize::MAX).unwrap();
    assert_eq!(all.len(), expected.len());
    assert!(all.windows(2).all(|w| w[0].score >= w[1].score));
    assert!(all.iter().all(|h| h.kind != kind::MODULE && !h.is_test));
    // A hit says where the symbol is.
    let save = fx.symbol("orders_api.repo.save_order");
    assert_eq!(
        all[0],
        Hit {
            id: save.id.clone(),
            name: "save_order".to_string(),
            qualified_name: save.qualified_name.clone(),
            kind: kind::FUNCTION.to_string(),
            file_path: REPO_PY.to_string(),
            service: "orders_api".to_string(),
            start_line: 3,
            end_line: 7,
            is_test: false,
            score: 0.4,
        }
    );

    // Within one service: its ranked symbols, then the rest of it by name.
    assert_eq!(
        ranked(Some("orders_api"), None, 10),
        named(&[
            ("save_order", 0.4),
            ("create_order", 0.3),
            ("publish_created", 0.0),
            ("load_order", 0.0),
            ("get_order", 0.0)
        ])
    );
    // Within one community, and within both.
    assert_eq!(
        ranked(None, Some(1), 10),
        named(&[("getOrderHandler", 0.3), ("sendEmail", 0.1)])
    );
    assert_eq!(
        ranked(None, Some(0), 10),
        named(&[("save_order", 0.4), ("create_order", 0.3)])
    );
    assert_eq!(
        ranked(Some("web_gateway"), Some(1), 10),
        named(&[("getOrderHandler", 0.3)])
    );
    assert!(ranked(Some("orders_api"), Some(1), 10).is_empty());
    // Community -1 is "none": the symbols no ranking run has placed.
    assert_eq!(ranked(None, Some(-1), 100).len(), expected.len() - 4);
    assert!(ranked(None, Some(99), 10).is_empty());

    // Limits, and services that do not exist.
    assert_eq!(ranked(None, None, 1), named(&[("save_order", 0.4)]));
    assert!(ranked(None, None, 0).is_empty());
    for service in ["billing", ""].iter().chain(AWKWARD) {
        assert!(ranked(Some(service), None, 10).is_empty(), "{service:?}");
    }
}

#[test]
fn metrics_take_thousands_of_rows() {
    let (_dir, store) = open();
    let (files, resolved, symbol_ids) = call_chain(5000);
    store.insert_parsed_bulk(&files, COMMIT, NOW).unwrap();
    store.replace_edges(&resolved, COMMIT, NOW).unwrap();

    let mut rows: Vec<SymbolMetrics> = symbol_ids
        .iter()
        .enumerate()
        .map(|(i, id)| metrics(id, (i + 1) as f64 / 5000.0, (i % 7) as i64, 2))
        .collect();
    // Unknown ids spread over the chunks.
    for i in 0..2500 {
        rows.insert(i * 3, metrics(&format!("unknown-{i}"), 1.0, 1, 1));
    }
    let started = std::time::Instant::now();
    store.set_metrics(&rows).unwrap();
    println!("stored {} metrics rows in {:?}", rows.len(), started.elapsed());

    // Asked for with repeats and unknown ids in between.
    let mut asked = Vec::new();
    for (i, id) in symbol_ids.iter().enumerate() {
        asked.push(id.clone());
        asked.push(format!("unknown-{i}"));
        asked.push(id.clone());
    }
    let expected: Vec<SymbolMetrics> = rows.iter().filter(|m| !m.id.starts_with("unknown-")).cloned().collect();
    assert_eq!(store.metrics(&asked).unwrap(), expected);

    let top = store.top_ranked(None, None, 3).unwrap();
    assert_eq!(
        top.iter().map(|h| (h.id.clone(), h.score)).collect::<Vec<_>>(),
        vec![
            (symbol_ids[4999].clone(), 1.0),
            (symbol_ids[4998].clone(), 4999.0 / 5000.0),
            (symbol_ids[4997].clone(), 4998.0 / 5000.0)
        ]
    );
    assert_eq!(
        store.top_ranked(Some("big"), Some(3), usize::MAX).unwrap().len(),
        (0..5000).filter(|i| i % 7 == 3).count()
    );

    // The graph they were computed from: one call per step, in a fixed order.
    let mut expected: Vec<(String, String, EdgeType, f64)> = symbol_ids
        .windows(2)
        .map(|w| (w[0].clone(), w[1].clone(), EdgeType::Calls, 0.95))
        .collect();
    expected.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    assert_eq!(store.symbol_graph().unwrap(), expected);
}

#[test]
fn metrics_survive_a_resolve_and_start_over_when_a_file_is_reindexed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.lbug");
    let store = Store::open(&path).unwrap();
    let (fx, resolved) = index_fixture(&store);
    let rows: Vec<SymbolMetrics> = real_symbols(&fx)
        .iter()
        .enumerate()
        .map(|(i, s)| metrics(&s.id, 1.0 / (i + 2) as f64, (i % 3) as i64, i as i64 + 1))
        .collect();
    let all: Vec<String> = rows.iter().map(|m| m.id.clone()).collect();
    store.set_metrics(&rows).unwrap();

    // Resolving rewrites edges and derived nodes, not the symbols.
    store.replace_edges(&resolved, COMMIT, NOW + 9).unwrap();
    assert_eq!(store.metrics(&all).unwrap(), rows);
    store.replace_edges(&Resolved::default(), COMMIT, NOW + 10).unwrap();
    store.replace_edges(&resolved, COMMIT, NOW + 11).unwrap();
    assert_eq!(store.metrics(&all).unwrap(), rows);
    let top = store.top_ranked(None, None, 5).unwrap();

    // They are in the database file, not in the process.
    drop(store);
    let store = Store::open(&path).unwrap();
    assert_eq!(store.metrics(&all).unwrap(), rows);
    assert_eq!(store.top_ranked(None, None, 5).unwrap(), top);

    // A deleted file takes the metrics of its symbols with it.
    let reindexed: BTreeSet<&str> = fx.get(ROUTES_PY).symbols.iter().map(|s| s.id.as_str()).collect();
    let untouched: Vec<SymbolMetrics> = rows
        .iter()
        .filter(|m| !reindexed.contains(m.id.as_str()))
        .cloned()
        .collect();
    store.delete_file(ROUTES_PY).unwrap();
    assert_eq!(store.metrics(&all).unwrap(), untouched);
    // Indexed again, its symbols are new rows with the same ids: not computed
    // yet, while every other symbol keeps its numbers through the resolve.
    store.insert_parsed(fx.get(ROUTES_PY), COMMIT, NOW + 20).unwrap();
    store.replace_edges(&resolved, COMMIT, NOW + 20).unwrap();
    assert_eq!(store.last_skipped_edges(), 0);
    let expected: Vec<SymbolMetrics> = rows
        .iter()
        .map(|m| {
            if reindexed.contains(m.id.as_str()) {
                metrics(&m.id, 0.0, -1, 0)
            } else {
                m.clone()
            }
        })
        .collect();
    assert_eq!(store.metrics(&all).unwrap(), expected);
    // The same through the bulk path, for every file at once.
    store.insert_parsed_bulk(&fx.files, COMMIT, NOW + 30).unwrap();
    store.replace_edges(&resolved, COMMIT, NOW + 30).unwrap();
    assert!(store
        .metrics(&all)
        .unwrap()
        .iter()
        .all(|m| (m.rank, m.community, m.degree) == (0.0, -1, 0)));
}
