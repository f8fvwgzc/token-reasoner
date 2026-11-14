//! Embeddings: which rows still need one, storing them, and the cosine search.

mod common;

use common::*;
use lge_core::{kind, unresolved_id, MemoryRec, SymbolRec, VectorTable};
use lge_store::{GraphStore, Store};
use std::collections::BTreeSet;

/// The text a symbol is embedded by.
fn text_of(s: &SymbolRec) -> String {
    format!("{}\n{}\n{}", s.qualified_name, s.signature, s.doc)
}

/// (id, text) of every symbol that gets an embedding, in the order they are listed.
fn embeddable(fx: &Fixture) -> Vec<(String, String)> {
    let mut symbols: Vec<&SymbolRec> = fx
        .files
        .iter()
        .flat_map(|p| p.symbols.iter())
        .filter(|s| s.kind != kind::MODULE)
        .collect();
    symbols.sort_by(|a, b| (&a.qualified_name, &a.id).cmp(&(&b.qualified_name, &b.id)));
    symbols.iter().map(|s| (s.id.clone(), text_of(s))).collect()
}

/// A search, checked for what must hold whatever is asked.
fn nearest(store: &Store, table: VectorTable, query: &[f32], limit: usize, least: f64) -> Vec<(String, f64)> {
    let found = store.vector_search(table, query, limit, least).unwrap();
    assert!(found.len() <= limit);
    assert!(found.windows(2).all(|w| w[0].1 >= w[1].1), "not best first: {found:?}");
    assert!(
        found.iter().all(|(_, s)| *s >= least - 1e-6),
        "below {least}: {found:?}"
    );
    assert!(found.iter().all(|(_, s)| (-1.000001..=1.000001).contains(s)));
    let distinct: BTreeSet<&String> = found.iter().map(|(id, _)| id).collect();
    assert_eq!(distinct.len(), found.len(), "a row twice");
    found
}

/// The same rows with the same similarities, to the precision of a 32-bit float.
fn assert_close(found: &[(String, f64)], expected: &[(&str, f64)]) {
    let ids = |items: &[(String, f64)]| items.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>();
    let expected: Vec<(String, f64)> = expected.iter().map(|(id, s)| (id.to_string(), *s)).collect();
    assert_eq!(ids(found), ids(&expected), "{found:?}");
    for ((_, got), (_, want)) in found.iter().zip(&expected) {
        assert!((got - want).abs() < 1e-5, "{found:?} is not {expected:?}");
    }
}

#[test]
fn symbols_without_an_embedding_are_listed_with_their_text() {
    let (_dir, store, fx, _) = indexed();
    let expected = embeddable(&fx);
    // Every real symbol, tests included; no module and no placeholder.
    assert_eq!(expected.len(), fx.symbol_count() - fx.files.len());
    assert_eq!(store.missing_embeddings(VectorTable::Symbol, 1000).unwrap(), expected);
    let create = fx.symbol("orders_api.routes.create_order");
    assert!(expected.contains(&(
        create.id.clone(),
        "orders_api.routes.create_order\ndef create_order(payload)\nCreate an order and announce it on the queue."
            .to_string()
    )));
    // Qualified name, signature and doc are always three lines, also when one is empty.
    let handler = fx.symbol("web_gateway.src.routes.getOrderHandler");
    assert!(expected.contains(&(
        handler.id.clone(),
        "web_gateway.src.routes.getOrderHandler\nfunction getOrderHandler(...)\n".to_string()
    )));
    // In a fixed order, so that a limit always cuts the same rows.
    assert_eq!(store.missing_embeddings(VectorTable::Symbol, 3).unwrap(), expected[..3]);
    assert!(store.missing_embeddings(VectorTable::Symbol, 0).unwrap().is_empty());
    assert_eq!(
        store.missing_embeddings(VectorTable::Symbol, usize::MAX).unwrap(),
        expected
    );
    assert!(store.missing_embeddings(VectorTable::Memory, 10).unwrap().is_empty());

    // Embedded rows leave the list; unknown ids, a module and a placeholder change nothing for it.
    let module = fx.id("orders_api.routes");
    store
        .set_embeddings(
            VectorTable::Symbol,
            &[
                (expected[0].0.clone(), vec![1.0, 0.0]),
                ("no-such-id".to_string(), vec![1.0, 0.0]),
                (expected[2].0.clone(), vec![0.0, 1.0]),
                (module, vec![0.5, 0.5]),
                (unresolved_id("render"), vec![0.5, 0.5]),
            ],
        )
        .unwrap();
    let mut left = expected.clone();
    left.remove(2);
    left.remove(0);
    assert_eq!(store.missing_embeddings(VectorTable::Symbol, 1000).unwrap(), left);
}

#[test]
fn embeddings_are_searched_by_cosine_similarity() {
    on_both_backends(|store| {
        let (fx, resolved) = index_fixture(store);
        let symbol = VectorTable::Symbol;
        // Nothing is embedded yet: there is nothing to find, whatever is asked.
        assert!(nearest(store, symbol, &[1.0, 0.0, 0.0], 10, -1.0).is_empty());
        assert!(nearest(store, symbol, &[], 10, -1.0).is_empty());
        assert!(nearest(store, VectorTable::Memory, &[1.0], 10, -1.0).is_empty());

        let a = fx.id("orders_api.routes.create_order");
        let b = fx.id("orders_api.repo.save_order");
        let c = fx.id("orders_api.repo.load_order");
        let d = fx.id("orders_api.routes.get_order");
        let e = fx.id("orders_api.events.publish_created");
        store
            .set_embeddings(
                symbol,
                &[
                    (a.clone(), vec![1.0, 0.0, 0.0]),
                    (b.clone(), vec![0.6, 0.8, 0.0]),
                    ("no-such-id".to_string(), vec![0.0, 0.0, 1.0]),
                    (c.clone(), vec![0.0, 1.0, 0.0]),
                    (d.clone(), vec![-1.0, 0.0, 0.0]),
                ],
            )
            .unwrap();

        // Best first, from the same direction to the opposite one.
        let all = nearest(store, symbol, &[1.0, 0.0, 0.0], 10, -1.0);
        assert_close(&all, &[(&a, 1.0), (&b, 0.6), (&c, 0.0), (&d, -1.0)]);
        // The length of the query does not matter, only its direction.
        assert_close(
            &nearest(store, symbol, &[250.0, 0.0, 0.0], 10, -1.0),
            &[(&a, 1.0), (&b, 0.6), (&c, 0.0), (&d, -1.0)],
        );
        assert_close(
            &nearest(store, symbol, &[0.0, 0.5, 0.0], 10, 0.5),
            &[(&c, 1.0), (&b, 0.8)],
        );
        // A floor on the similarity, and a limit on the number.
        assert_close(
            &nearest(store, symbol, &[1.0, 0.0, 0.0], 10, 0.5),
            &[(&a, 1.0), (&b, 0.6)],
        );
        assert_close(&nearest(store, symbol, &[1.0, 0.0, 0.0], 10, 0.61), &[(&a, 1.0)]);
        assert!(nearest(store, symbol, &[0.0, 0.0, 1.0], 10, 0.1).is_empty());
        assert_close(
            &nearest(store, symbol, &[1.0, 0.0, 0.0], 2, -1.0),
            &[(&a, 1.0), (&b, 0.6)],
        );
        assert!(nearest(store, symbol, &[1.0, 0.0, 0.0], 0, -1.0).is_empty());
        assert_eq!(nearest(store, symbol, &[1.0, 0.0, 0.0], usize::MAX, -1.0), all);

        // One table holds vectors of one length: another length is an error,
        // for a query and for a row, and such a call stores nothing.
        let err = store.vector_search(symbol, &[1.0, 0.0], 10, 0.0).unwrap_err();
        assert!(format!("{err:#}").contains("3 numbers"), "{err:#}");
        assert!(store.vector_search(symbol, &[], 10, 0.0).is_err());
        assert!(store.vector_search(symbol, &[1.0, 0.0, 0.0, 0.0], 0, 0.0).is_err());
        let err = store
            .set_embeddings(symbol, &[(e.clone(), vec![1.0, 0.0])])
            .unwrap_err();
        assert!(format!("{err:#}").contains("3 numbers"), "{err:#}");
        // Two lengths in one call: the row with the right length is not stored either.
        assert!(store
            .set_embeddings(symbol, &[(e.clone(), vec![0.0, 0.0, 1.0]), (a.clone(), vec![1.0])])
            .is_err());
        // Vectors nothing can be compared with.
        for bad in [vec![], vec![f32::NAN, 0.0, 0.0], vec![0.0, f32::INFINITY, 0.0]] {
            assert!(store.set_embeddings(symbol, &[(e.clone(), bad)]).is_err());
        }
        assert!(
            nearest(store, symbol, &[0.0, 0.0, 1.0], 10, 0.1).is_empty(),
            "nothing was stored for e"
        );
        assert!(store
            .missing_embeddings(symbol, 1000)
            .unwrap()
            .iter()
            .any(|(id, _)| id == &e));
        assert_eq!(nearest(store, symbol, &[1.0, 0.0, 0.0], 10, -1.0), all);

        // Nothing to store is not an error, and decides nothing.
        store.set_embeddings(symbol, &[]).unwrap();
        store.set_embeddings(VectorTable::Memory, &[]).unwrap();
        // Ids that name no row, whatever they look like, store nothing.
        let nowhere: Vec<(String, Vec<f32>)> = AWKWARD
            .iter()
            .map(|text| (text.to_string(), vec![0.0, 0.0, 1.0]))
            .collect();
        store.set_embeddings(symbol, &nowhere).unwrap();
        assert_eq!(nearest(store, symbol, &[1.0, 0.0, 0.0], 10, -1.0), all);
        // Stored again, an embedding is replaced; of two for one row the later one counts.
        store
            .set_embeddings(
                symbol,
                &[(a.clone(), vec![0.0, 1.0, 0.0]), (a.clone(), vec![0.0, 0.0, 1.0])],
            )
            .unwrap();
        assert_close(&nearest(store, symbol, &[0.0, 0.0, 1.0], 10, 0.1), &[(&a, 1.0)]);
        // Equal similarities are ordered by id.
        let mut level = [a.as_str(), c.as_str()];
        level.sort();
        assert_close(
            &nearest(store, symbol, &[1.0, 0.0, 0.0], 10, -1.0),
            &[(&b, 0.6), (level[0], 0.0), (level[1], 0.0), (&d, -1.0)],
        );

        // Resolving does not touch the symbols, so their embeddings stay.
        store.replace_edges(&resolved, COMMIT, NOW + 9).unwrap();
        assert_close(&nearest(store, symbol, &[0.0, 0.0, 1.0], 10, 0.1), &[(&a, 1.0)]);
    });
}

#[test]
fn facts_are_embedded_while_they_are_current() {
    let (_dir, store, fx, _) = indexed();
    let fact = VectorTable::Memory;
    let m1 = memory("m1", "The gateway throttles after forty requests.", 100);
    let m2 = memory("m2", "Uploads are retried twice.\nThen they are parked.", 200);
    let old = MemoryRec {
        expired_at: 60,
        ..memory("m3", "The gateway throttles after ten requests.", 50)
    };
    for m in [&m2, &old, &m1] {
        store.put_memory(m).unwrap();
    }
    // A fact is embedded by its text; a superseded one is not embedded at all.
    let wanted = vec![("m1".to_string(), m1.text.clone()), ("m2".to_string(), m2.text.clone())];
    assert_eq!(store.missing_embeddings(fact, 10).unwrap(), wanted);
    assert_eq!(store.missing_embeddings(fact, 1).unwrap(), wanted[..1]);
    assert!(store.missing_embeddings(fact, 0).unwrap().is_empty());

    store
        .set_embeddings(
            fact,
            &[
                ("m1".to_string(), vec![1.0, 0.0]),
                ("m2".to_string(), vec![0.0, 1.0]),
                ("m3".to_string(), vec![1.0, 0.0]),
                ("no-such-id".to_string(), vec![1.0, 0.0]),
            ],
        )
        .unwrap();
    assert!(store.missing_embeddings(fact, 10).unwrap().is_empty());
    // The expired fact points the same way as m1 and is still not an answer.
    assert_close(
        &nearest(&store, fact, &[1.0, 0.0], 10, -1.0),
        &[("m1", 1.0), ("m2", 0.0)],
    );
    assert_close(&nearest(&store, fact, &[0.0, 3.0], 10, 0.5), &[("m2", 1.0)]);

    // The two tables are embedded apart: symbols may have another length.
    let create = fx.id("orders_api.routes.create_order");
    store
        .set_embeddings(VectorTable::Symbol, &[(create.clone(), vec![0.0, 0.0, 1.0])])
        .unwrap();
    assert_close(
        &nearest(&store, VectorTable::Symbol, &[0.0, 0.0, 2.0], 10, 0.5),
        &[(&create, 1.0)],
    );
    assert!(store.vector_search(fact, &[0.0, 0.0, 1.0], 10, 0.0).is_err());
    assert!(store.vector_search(VectorTable::Symbol, &[1.0, 0.0], 10, 0.0).is_err());
    assert!(store
        .set_embeddings(fact, &[("m1".to_string(), vec![1.0, 0.0, 0.0])])
        .is_err());

    // An embedding belongs to the text it was made from: stored again with
    // the same text the fact keeps it, with another text it needs a new one.
    store
        .put_memory(&MemoryRec {
            tags: vec!["performance".to_string()],
            proof_count: 2,
            ..m1.clone()
        })
        .unwrap();
    assert!(store.missing_embeddings(fact, 10).unwrap().is_empty());
    assert_close(&nearest(&store, fact, &[1.0, 0.0], 10, 0.5), &[("m1", 1.0)]);
    let reworded = memory("m1", "The gateway throttles after fifty requests.", 100);
    store.put_memory(&reworded).unwrap();
    assert_eq!(
        store.missing_embeddings(fact, 10).unwrap(),
        vec![("m1".to_string(), reworded.text.clone())]
    );
    assert!(nearest(&store, fact, &[1.0, 0.0], 10, 0.5).is_empty());

    // A fact that is superseded leaves the search and the list of work to do.
    store
        .put_memory(&MemoryRec {
            invalid_at: 300,
            expired_at: 301,
            ..m2.clone()
        })
        .unwrap();
    assert!(nearest(&store, fact, &[0.0, 1.0], 10, 0.5).is_empty());
    store
        .put_memory(&MemoryRec {
            expired_at: 302,
            ..reworded
        })
        .unwrap();
    assert!(store.missing_embeddings(fact, 10).unwrap().is_empty());
    assert!(nearest(&store, fact, &[1.0, 0.0], 10, -1.0).is_empty());
}

#[test]
fn a_reindexed_symbol_loses_its_embedding() {
    let (_dir, store, fx, resolved) = indexed();
    let symbol = VectorTable::Symbol;
    let all = embeddable(&fx);
    // One direction per symbol, so that each is its own nearest neighbour.
    let length = all.len();
    let unit = |i: usize| -> Vec<f32> { (0..length).map(|d| if d == i { 1.0 } else { 0.0 }).collect() };
    let vectors: Vec<(String, Vec<f32>)> = all
        .iter()
        .enumerate()
        .map(|(i, (id, _))| (id.clone(), unit(i)))
        .collect();
    store.set_embeddings(symbol, &vectors).unwrap();
    assert!(store.missing_embeddings(symbol, 1000).unwrap().is_empty());
    let position = |qn: &str| all.iter().position(|(id, _)| id == &fx.id(qn)).unwrap();
    let create = position("orders_api.routes.create_order");
    let get = position("orders_api.routes.get_order");
    let save = position("orders_api.repo.save_order");
    assert_close(
        &nearest(&store, symbol, &unit(create), 5, 0.5),
        &[(&all[create].0, 1.0)],
    );

    // A resolve, and the deletion of another file, leave the embeddings alone.
    store.replace_edges(&resolved, COMMIT, NOW + 9).unwrap();
    store.delete_file(VIEWS_TS).unwrap();
    store.insert_parsed(fx.get(VIEWS_TS), COMMIT, NOW + 10).unwrap();
    store.replace_edges(&resolved, COMMIT, NOW + 10).unwrap();
    let render = position("web_gateway.src.views.render");
    assert_eq!(
        store.missing_embeddings(symbol, 1000).unwrap(),
        vec![all[render].clone()],
        "only the symbol of the file that was indexed again"
    );
    assert_close(
        &nearest(&store, symbol, &unit(create), 5, 0.5),
        &[(&all[create].0, 1.0)],
    );
    assert!(nearest(&store, symbol, &unit(render), 5, 0.5).is_empty());

    // The file of two embedded symbols is indexed again: both are new rows
    // with the old ids, and both need an embedding again.
    store.delete_file(ROUTES_PY).unwrap();
    assert_eq!(
        store.missing_embeddings(symbol, 1000).unwrap(),
        vec![all[render].clone()],
        "a deleted symbol is not work to do"
    );
    store.insert_parsed(fx.get(ROUTES_PY), COMMIT, NOW + 20).unwrap();
    store.replace_edges(&resolved, COMMIT, NOW + 20).unwrap();
    // `all` is in the order of the list, so the positions say where each one stands.
    let mut positions = [create, get, render];
    positions.sort();
    let missing: Vec<(String, String)> = positions.iter().map(|i| all[*i].clone()).collect();
    assert_eq!(store.missing_embeddings(symbol, 1000).unwrap(), missing);
    assert!(nearest(&store, symbol, &unit(create), 5, 0.5).is_empty());
    assert_close(&nearest(&store, symbol, &unit(save), 5, 0.5), &[(&all[save].0, 1.0)]);

    // Embedded again, they are found again. The length the table had is still its length.
    store
        .set_embeddings(
            symbol,
            &[
                (all[create].0.clone(), unit(create)),
                (all[get].0.clone(), unit(get)),
                (all[render].0.clone(), unit(render)),
            ],
        )
        .unwrap();
    assert!(store.missing_embeddings(symbol, 1000).unwrap().is_empty());
    assert_close(&nearest(&store, symbol, &unit(get), 5, 0.5), &[(&all[get].0, 1.0)]);

    // A full re-index through the bulk path: every embedding is gone, the
    // search is empty, and the table still takes vectors of its length only.
    store.insert_parsed_bulk(&fx.files, COMMIT, NOW + 30).unwrap();
    store.replace_edges(&resolved, COMMIT, NOW + 30).unwrap();
    assert_eq!(store.missing_embeddings(symbol, 1000).unwrap(), all);
    assert!(nearest(&store, symbol, &unit(create), 5, -1.0).is_empty());
    assert!(store
        .set_embeddings(symbol, &[(all[create].0.clone(), vec![1.0, 0.0])])
        .is_err());
    store.set_embeddings(symbol, &vectors).unwrap();
    assert!(store.missing_embeddings(symbol, 1000).unwrap().is_empty());
}

/// The bits of `i` followed by ones: no two rows below 4096 point the same way.
fn spread(i: usize, length: usize) -> Vec<f32> {
    (0..length)
        .map(|d| if d < 12 { ((i >> d) & 1) as f32 } else { 1.0 })
        .collect()
}

#[test]
fn embeddings_survive_reopening_and_come_in_thousands() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.lbug");
    let (files, resolved, chain) = call_chain(3000);
    let length = 24;
    // Every step, with unknown ids in between.
    let mut vectors: Vec<(String, Vec<f32>)> = Vec::new();
    for (i, id) in chain.iter().enumerate() {
        vectors.push((id.clone(), spread(i, length)));
        if i % 2 == 0 {
            vectors.push((format!("unknown-{i}"), spread(i + 1, length)));
        }
    }
    let probe = |store: &Store| {
        let found = nearest(store, VectorTable::Symbol, &spread(1234, length), 5, 0.0);
        assert_eq!(found.len(), 5);
        assert_eq!(found[0].0, chain[1234]);
        assert!((found[0].1 - 1.0).abs() < 1e-5, "{found:?}");
        found
    };
    let found = {
        let store = Store::open(&path).unwrap();
        store.insert_parsed_bulk(&files, COMMIT, NOW).unwrap();
        store.replace_edges(&resolved, COMMIT, NOW).unwrap();
        assert_eq!(
            store.missing_embeddings(VectorTable::Symbol, usize::MAX).unwrap().len(),
            3000
        );
        let started = std::time::Instant::now();
        store.set_embeddings(VectorTable::Symbol, &vectors).unwrap();
        println!("stored {} embeddings in {:?}", vectors.len(), started.elapsed());
        assert!(store
            .missing_embeddings(VectorTable::Symbol, usize::MAX)
            .unwrap()
            .is_empty());
        store.put_memory(&memory("m1", "a fact", 1)).unwrap();
        store
            .set_embeddings(VectorTable::Memory, &[("m1".to_string(), vec![0.25, -0.5])])
            .unwrap();
        let started = std::time::Instant::now();
        let found = probe(&store);
        println!("searched 3000 embeddings in {:?}", started.elapsed());
        found
    };

    // The vectors and the length of each table are in the database file.
    let store = Store::open(&path).unwrap();
    assert_eq!(probe(&store), found);
    assert!(store.missing_embeddings(VectorTable::Symbol, 10).unwrap().is_empty());
    assert_close(
        &nearest(&store, VectorTable::Memory, &[0.5, -1.0], 5, 0.9),
        &[("m1", 1.0)],
    );
    assert!(store.vector_search(VectorTable::Symbol, &[1.0, 0.0], 5, 0.0).is_err());
    assert!(store
        .set_embeddings(VectorTable::Symbol, &[(chain[0].clone(), vec![1.0; 25])])
        .is_err());
    assert!(store
        .set_embeddings(VectorTable::Memory, &[("m1".to_string(), vec![1.0; 24])])
        .is_err());
    store
        .set_embeddings(VectorTable::Symbol, &[(chain[0].clone(), spread(1234, length))])
        .unwrap();
    let again = nearest(&store, VectorTable::Symbol, &spread(1234, length), 2, 0.999);
    assert_eq!(
        again.iter().map(|(id, _)| id.clone()).collect::<BTreeSet<_>>(),
        BTreeSet::from([chain[0].clone(), chain[1234].clone()])
    );
}
