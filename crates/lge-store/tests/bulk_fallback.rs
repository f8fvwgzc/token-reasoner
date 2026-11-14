//! The bulk edge load needs a directory for its files. Without one the edges
//! are still stored, by statements. This test changes `TMPDIR` for its whole
//! process, which is why it lives alone in this file.
#![cfg(unix)]

mod common;

use common::*;
use lge_core::{kind, lang, method, Direction, EdgeType, Resolved};
use lge_store::GraphStore;

#[test]
fn edges_are_stored_by_statements_when_the_bulk_load_cannot_run() {
    let (dir, store) = open();
    let f = file("big/chain.py", "big", lang::PYTHON);
    let symbols: Vec<_> = (0..2500)
        .map(|i| sym(&f, &format!("big.chain.step_{i}"), kind::FUNCTION, i, i))
        .collect();
    let edges: Vec<_> = symbols
        .windows(2)
        .map(|w| {
            edge(
                &w[0].id,
                &w[1].id,
                EdgeType::Calls,
                0.95,
                method::STATIC,
                "big/chain.py",
                w[0].start_line,
                w[0].end_line,
            )
        })
        .collect();
    let mut resolved = Resolved {
        edges,
        ..Resolved::default()
    };
    resolved.edges.push(edge(
        &symbols[0].id,
        "missing",
        EdgeType::Calls,
        0.5,
        method::INFERRED,
        "big/chain.py",
        1,
        1,
    ));
    store.insert_parsed(&parsed(f, symbols.clone()), COMMIT, NOW).unwrap();
    let from: Vec<String> = symbols.iter().map(|s| s.id.clone()).collect();
    let stored = |expected: &Resolved| {
        let mut want = expected.edges.clone();
        want.retain(|e| e.to != "missing");
        assert_eq!(sorted(store.edges(&from, &[], Direction::Out).unwrap()), sorted(want));
        assert_eq!(store.last_skipped_edges(), 1);
    };

    // With a temporary directory the bulk load runs.
    store.replace_edges(&resolved, COMMIT, NOW).unwrap();
    assert_eq!(store.last_bulk_load_error(), None);
    stored(&resolved);

    // Without one it cannot, and says so, but the same edges are stored.
    let saved = std::env::var_os("TMPDIR");
    std::env::set_var("TMPDIR", dir.path().join("does").join("not").join("exist"));
    resolved.edges.reverse();
    let result = store.replace_edges(&resolved, COMMIT, NOW + 1);
    match saved {
        Some(v) => std::env::set_var("TMPDIR", v),
        None => std::env::remove_var("TMPDIR"),
    }
    result.unwrap();
    let why = store.last_bulk_load_error().expect("the bulk load should have failed");
    assert!(why.contains("cannot create"), "{why}");
    stored(&resolved);

    // The next run is back to the bulk load and forgets the old error.
    store.replace_edges(&resolved, COMMIT, NOW + 2).unwrap();
    assert_eq!(store.last_bulk_load_error(), None);
    stored(&resolved);
}
