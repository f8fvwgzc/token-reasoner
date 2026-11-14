//! Long-term memory: facts, the code they mention, and how the two stay
//! joined while files are re-indexed.

mod common;

use common::*;
use lge_core::{file_id, memory_kind, service_id, unresolved_id, MemoryRec, Resolved, SymbolMetrics, VectorTable};
use lge_store::{GraphStore, Store};
use std::collections::BTreeSet;

/// One fact by id; panics when it is not stored.
fn read(store: &Store, id: &str) -> MemoryRec {
    let found = store.get_memories(&ids(&[id])).unwrap();
    assert_eq!(found.len(), 1, "fact {id:?}");
    found.into_iter().next().unwrap()
}

/// Number of MENTIONS edges, of one fact when an id is given.
fn mentions(store: &Store, fact: Option<&str>) -> i64 {
    match fact {
        Some(id) => count(
            store,
            &format!("MATCH (m:Memory)-[r:MENTIONS]->() WHERE m.id = '{id}' RETURN count(r);"),
        ),
        None => count(store, "MATCH (:Memory)-[r:MENTIONS]->() RETURN count(r);"),
    }
}

fn linked(id: &str, at: i64, links: &[&String]) -> MemoryRec {
    MemoryRec {
        links: links.iter().map(|l| l.to_string()).collect(),
        ..memory(id, &format!("fact {id}"), at)
    }
}

#[test]
fn facts_round_trip_with_the_nodes_they_mention() {
    let (_dir, store, fx, _) = indexed();
    assert_eq!(store.memory_counts().unwrap(), (0, 0));
    assert!(store.get_memories(&ids(&["m1"])).unwrap().is_empty());

    let create = fx.id("orders_api.routes.create_order");
    let file = file_id(ROUTES_PY);
    let service = service_id("orders_api");
    let m = MemoryRec {
        id: "m1".to_string(),
        kind: memory_kind::DECISION.to_string(),
        text: "Orders are retried twice before they are dropped.".to_string(),
        key: "orders.retry-policy".to_string(),
        tags: vec!["decision".to_string(), "orders".to_string()],
        source: "task-17".to_string(),
        created_at: 1000,
        valid_at: 990,
        invalid_at: 0,
        expired_at: 0,
        proof_count: 3,
        links: vec![create.clone(), file.clone(), service.clone()],
    };
    store.put_memory(&m).unwrap();
    assert_eq!(read(&store, "m1"), m);
    // One edge to each node, whatever its label.
    assert_eq!(mentions(&store, Some("m1")), 3);
    let labels = store
        .raw("MATCH (m:Memory)-[:MENTIONS]->(n) RETURN label(n) AS l ORDER BY l;")
        .unwrap();
    assert_eq!(
        labels.lines().skip(1).collect::<Vec<_>>(),
        vec!["File", "Service", "Symbol"]
    );

    // Links that lead nowhere are left out, a repeated one counts once, and a
    // placeholder is not something a fact is about.
    let loose = MemoryRec {
        id: "m2".to_string(),
        links: ids(&[&create, "no-such-id", &create, &unresolved_id("render"), "", &service]),
        ..m.clone()
    };
    store.put_memory(&loose).unwrap();
    let kept = MemoryRec {
        links: vec![create.clone(), service.clone()],
        ..loose.clone()
    };
    assert_eq!(read(&store, "m2"), kept);
    assert_eq!(mentions(&store, Some("m2")), 2);

    // By id: unknown ids are skipped, a repeated id is answered once, the
    // order is the order asked.
    assert_eq!(
        store.get_memories(&ids(&["m2", "no-such-id", "m1", "m2", ""])).unwrap(),
        vec![kept.clone(), m.clone()]
    );
    assert!(store.get_memories(&[]).unwrap().is_empty());

    // The same id again replaces every field and rewrites the edges.
    let rewritten = MemoryRec {
        id: "m1".to_string(),
        kind: memory_kind::CONVENTION.to_string(),
        text: "Retries are configured per queue.".to_string(),
        key: String::new(),
        tags: Vec::new(),
        source: "user".to_string(),
        created_at: 5,
        valid_at: 6,
        invalid_at: 7,
        expired_at: 8,
        proof_count: 1,
        links: vec![service.clone()],
    };
    store.put_memory(&rewritten).unwrap();
    assert_eq!(read(&store, "m1"), rewritten);
    assert_eq!(mentions(&store, Some("m1")), 1);
    assert_eq!(count(&store, "MATCH (m:Memory) RETURN count(m);"), 2);
    assert_eq!(store.memory_counts().unwrap(), (1, 1));
    // Without links there are no edges, and storing the same fact twice changes nothing.
    let bare = MemoryRec {
        links: Vec::new(),
        ..rewritten.clone()
    };
    store.put_memory(&bare).unwrap();
    store.put_memory(&bare).unwrap();
    assert_eq!(read(&store, "m1"), bare);
    assert_eq!(mentions(&store, Some("m1")), 0);
    assert_eq!(mentions(&store, None), 2);
    store.put_memory(&kept).unwrap();
    store.put_memory(&kept).unwrap();
    assert_eq!(mentions(&store, None), 2, "a fact is linked to a node once");

    // Facts are not part of the code graph: the graph counts are as they were.
    assert_eq!(stat(&store, "memories"), 1);
    assert_eq!(stat(&store, "memories_expired"), 1);
    assert_eq!(stat(&store, "symbols"), fx.symbol_count() as i64);
}

#[test]
fn awkward_text_and_extreme_numbers_are_stored_exactly() {
    on_both_backends(|store| {
        let mut all = Vec::new();
        for (i, text) in AWKWARD.iter().enumerate() {
            let m = MemoryRec {
                id: format!("id {text}"),
                kind: text.to_string(),
                text: text.to_string(),
                key: text.to_string(),
                tags: vec![
                    text.to_string(),
                    "a,b".to_string(),
                    "[x]".to_string(),
                    "\"q\"".to_string(),
                    String::new(),
                    "two words".to_string(),
                ],
                source: text.to_string(),
                created_at: i as i64,
                valid_at: i64::MAX - i as i64,
                invalid_at: i64::MIN + i as i64,
                expired_at: (i % 2) as i64,
                proof_count: -1,
                links: vec![text.to_string()],
            };
            store.put_memory(&m).unwrap();
            // The link names no node, so it is not kept.
            all.push(MemoryRec { links: Vec::new(), ..m });
        }
        let asked: Vec<String> = all.iter().map(|m| m.id.clone()).collect();
        assert_eq!(store.get_memories(&asked).unwrap(), all);
        let expired = all.iter().filter(|m| m.expired_at != 0).count() as i64;
        assert_eq!(store.memory_counts().unwrap(), (all.len() as i64 - expired, expired));

        // Nothing but defaults, and a very long text.
        let empty = MemoryRec::default();
        store.put_memory(&empty).unwrap();
        assert_eq!(read(store, ""), empty);
        let long = memory("long", &"abcdefghij ".repeat(30_000), 7);
        store.put_memory(&long).unwrap();
        assert_eq!(read(store, "long").text.len(), long.text.len());

        // Searching with the same text finds or does not find, but never fails.
        for text in AWKWARD {
            let found = store.search_memories(text, 5, true).unwrap();
            assert!(found.len() <= 5, "{text:?}");
            store.memories_by_key(text).unwrap();
            store.recent_memories(Some(text), &ids(&[text]), 5).unwrap();
            store.memories_mentioning(&ids(&[text]), 5).unwrap();
        }
    });
}

/// A search that must hold what every search holds, as the ids found.
fn search(store: &Store, text: &str, limit: usize, include_expired: bool) -> Vec<String> {
    let found = store.search_memories(text, limit, include_expired).unwrap();
    assert!(found.len() <= limit, "{text}: more than {limit} facts");
    assert!(
        found.windows(2).all(|w| w[0].1 >= w[1].1),
        "{text}: not ordered by score"
    );
    assert!(
        found.iter().all(|(_, score)| *score > 0.0),
        "{text}: a fact without a score"
    );
    if !include_expired {
        assert!(found.iter().all(|(m, _)| m.expired_at == 0), "{text}: an expired fact");
    }
    // A found fact is the record a lookup by id gives.
    for (m, _) in &found {
        assert_eq!(&read(store, &m.id), m);
    }
    found.into_iter().map(|(m, _)| m.id).collect()
}

#[test]
fn facts_are_found_by_text_key_tags_and_the_code_they_mention() {
    on_both_backends(|store| {
        let (fx, _) = index_fixture(store);
        assert!(
            search(store, "throttles", 10, true).is_empty(),
            "nothing is remembered yet"
        );

        let by_text = memory("m-text", "The payment gateway throttles after forty requests.", 100);
        let by_key = MemoryRec {
            key: "billing.invoice-rounding".to_string(),
            ..memory("m-key", "Nothing in particular.", 200)
        };
        let by_tag = MemoryRec {
            tags: vec!["quokka".to_string(), "decision".to_string()],
            ..memory("m-tag", "Something else entirely.", 300)
        };
        let by_link = MemoryRec {
            links: vec![fx.id("web_gateway.src.routes.getOrderHandler")],
            ..memory("m-link", "Has to stay idempotent.", 400)
        };
        let one_word = memory("m-pay", "The payment form is plain.", 150);
        let about_a_bug = memory("m-bug", "Known bug: the exporter drops the last row.", 500);
        let old = MemoryRec {
            invalid_at: 55,
            expired_at: 60,
            ..memory("m-old", "The payment gateway throttles after ten requests.", 50)
        };
        for m in [&by_text, &by_key, &by_tag, &by_link, &one_word, &about_a_bug, &old] {
            store.put_memory(m).unwrap();
        }

        // By a word of the text; a superseded fact only when it is asked for.
        assert_eq!(search(store, "throttles", 10, false), vec!["m-text"]);
        assert_eq!(
            search(store, "throttles", 10, true)
                .into_iter()
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["m-old".to_string(), "m-text".to_string()])
        );
        assert_eq!(search(store, "THROTTLES after", 10, false)[0], "m-text");
        // By the words of the key and by a tag.
        assert_eq!(search(store, "invoice rounding", 10, false), vec!["m-key"]);
        assert_eq!(search(store, "quokka", 10, false), vec!["m-tag"]);
        // By the code it mentions: the name as written, and the words of the name.
        assert_eq!(search(store, "getOrderHandler", 10, false), vec!["m-link"]);
        assert_eq!(search(store, "order handler", 10, false), vec!["m-link"]);
        assert_eq!(
            store.search_memories("idempotent", 10, false).unwrap()[0].0,
            by_link,
            "found with its links"
        );
        // A fact about a bug is found by `bug`, a word a symbol search leaves out.
        assert_eq!(search(store, "bug", 10, false), vec!["m-bug"]);
        assert_eq!(search(store, "fix the exporter bug", 10, false), vec!["m-bug"]);

        // BM25 knows the forms of a word; the substring scorer compares letters.
        let stemmed = search(store, "throttling", 10, false);
        if store.fts_enabled() {
            assert_eq!(stemmed, vec!["m-text"], "`throttles` and `throttling` are one word");
        } else {
            assert!(stemmed.is_empty());
        }

        // The fact that has both words comes before the one that has one.
        assert_eq!(search(store, "payment requests", 10, false), vec!["m-text", "m-pay"]);
        assert_eq!(search(store, "payment requests", 1, false), vec!["m-text"]);
        assert!(search(store, "payment requests", 0, false).is_empty());
        assert_eq!(search(store, "payment requests", usize::MAX, false).len(), 2);

        // Nothing to search for, or nothing that matches.
        for text in [
            "",
            "   ",
            "the and for",
            "!!! ??? ---",
            "a b c",
            "zzzqqqxxx",
            "' OR 1=1 --",
            "$q",
        ] {
            assert!(search(store, text, 10, true).is_empty(), "{text:?}");
        }

        // The search follows the fact: new text is found, the old text is not.
        let changed = memory("m-text", "The cache is flushed at midnight.", 100);
        store.put_memory(&changed).unwrap();
        assert!(search(store, "throttles", 10, false).is_empty());
        assert_eq!(search(store, "throttles", 10, true), vec!["m-old"]);
        assert_eq!(search(store, "flushed midnight", 10, false), vec!["m-text"]);
        // Superseded, it leaves the default search and stays in the history.
        let superseded = MemoryRec {
            invalid_at: 900,
            expired_at: 901,
            ..changed
        };
        store.put_memory(&superseded).unwrap();
        assert!(search(store, "flushed midnight", 10, false).is_empty());
        assert_eq!(search(store, "flushed midnight", 10, true), vec!["m-text"]);
    });
}

#[test]
fn memories_mentioning_counts_the_shared_nodes() {
    let (_dir, store, fx, _) = indexed();
    let a = fx.id("orders_api.routes.create_order");
    let b = fx.id("orders_api.repo.save_order");
    let c = file_id(ROUTES_PY);
    let d = service_id("orders_api");
    let m1 = linked("m1", 100, &[&a, &b, &c]);
    let m2 = linked("m2", 300, &[&a]);
    let m3 = linked("m3", 200, &[&b, &c]);
    let m4 = linked("m4", 250, &[&b, &c]);
    // Same time as m3: the id decides.
    let m7 = linked("m7", 200, &[&c, &b]);
    let expired = MemoryRec {
        expired_at: 9,
        ..linked("m5", 400, &[&a, &b, &c])
    };
    let unlinked = linked("m6", 500, &[]);
    for m in [&m1, &m2, &m3, &m4, &m7, &expired, &unlinked] {
        store.put_memory(m).unwrap();
    }
    let mentioning = |nodes: &[&String], limit: usize| -> Vec<(String, i64)> {
        let asked: Vec<String> = nodes.iter().map(|n| n.to_string()).collect();
        let found = store.memories_mentioning(&asked, limit).unwrap();
        assert!(found.len() <= limit);
        for (m, shared) in &found {
            assert_eq!(m.expired_at, 0, "an expired fact");
            assert_eq!(&read(&store, &m.id), m);
            assert!(*shared >= 1 && *shared <= m.links.len() as i64);
        }
        found.into_iter().map(|(m, shared)| (m.id, shared)).collect()
    };
    let expect =
        |items: &[(&str, i64)]| -> Vec<(String, i64)> { items.iter().map(|(id, n)| (id.to_string(), *n)).collect() };

    // Most shared first, then the newest, then by id.
    assert_eq!(
        mentioning(&[&a, &b, &c], 10),
        expect(&[("m1", 3), ("m4", 2), ("m3", 2), ("m7", 2), ("m2", 1)])
    );
    assert_eq!(mentioning(&[&a], 10), expect(&[("m2", 1), ("m1", 1)]));
    assert_eq!(
        mentioning(&[&c], 10),
        expect(&[("m4", 1), ("m3", 1), ("m7", 1), ("m1", 1)])
    );
    // A node asked for twice is one node; unknown nodes share nothing.
    let unknown = "no-such-id".to_string();
    assert_eq!(
        mentioning(&[&a, &a, &unknown, &b, &a], 10),
        expect(&[("m1", 2), ("m2", 1), ("m4", 1), ("m3", 1), ("m7", 1)])
    );
    // The limit cuts from the bottom.
    assert_eq!(mentioning(&[&a, &b, &c], 2), expect(&[("m1", 3), ("m4", 2)]));
    assert!(mentioning(&[&a, &b, &c], 0).is_empty());
    assert_eq!(mentioning(&[&a, &b, &c], usize::MAX).len(), 5);
    // Nobody mentions the service; nothing mentions nothing.
    assert!(mentioning(&[&d], 10).is_empty());
    assert!(mentioning(&[], 10).is_empty());
    assert!(mentioning(&[&unknown], 10).is_empty());

    // A fact that expires leaves the list; one that is linked anew joins it.
    store
        .put_memory(&MemoryRec {
            expired_at: 77,
            ..m1.clone()
        })
        .unwrap();
    store.put_memory(&linked("m6", 500, &[&d, &a])).unwrap();
    assert_eq!(
        mentioning(&[&a, &b, &c, &d], 3),
        expect(&[("m6", 2), ("m4", 2), ("m3", 2)])
    );
}

#[test]
fn recent_memories_are_filtered_by_kind_and_tags() {
    let (_dir, store) = open();
    assert!(store.recent_memories(None, &[], 10).unwrap().is_empty());
    let tagged = |id: &str, kind: &str, tags: &[&str], at: i64| MemoryRec {
        kind: kind.to_string(),
        tags: ids(tags),
        ..memory(id, &format!("fact {id}"), at)
    };
    let (world, decision, convention) = (memory_kind::WORLD, memory_kind::DECISION, memory_kind::CONVENTION);
    let r1 = tagged("r1", world, &["component", "api"], 100);
    let r2 = tagged("r2", decision, &["decision"], 300);
    let r3 = tagged("r3", decision, &["decision", "api"], 200);
    let r4 = tagged("r4", convention, &[], 400);
    let expired = MemoryRec {
        expired_at: 1,
        ..tagged("r5", decision, &["decision", "api"], 500)
    };
    // Tags that only look like `decision` to a careless comparison.
    let lookalikes = tagged("r6", world, &["x\"decision\"y", "decisions", "deci", "DECISION"], 50);
    for m in [&r1, &r2, &r3, &r4, &expired, &lookalikes] {
        store.put_memory(m).unwrap();
    }
    let recent = |kind: Option<&str>, tags: &[&str], limit: usize| -> Vec<String> {
        let found = store.recent_memories(kind, &ids(tags), limit).unwrap();
        assert!(found.len() <= limit);
        assert!(found.iter().all(|m| m.expired_at == 0), "an expired fact");
        assert!(
            found.windows(2).all(|w| w[0].valid_at >= w[1].valid_at),
            "not newest first"
        );
        for m in &found {
            assert_eq!(&read(&store, &m.id), m);
        }
        found.into_iter().map(|m| m.id).collect()
    };

    // Newest first; expired facts are history, not memory.
    assert_eq!(recent(None, &[], 10), vec!["r4", "r2", "r3", "r1", "r6"]);
    assert_eq!(recent(Some(decision), &[], 10), vec!["r2", "r3"]);
    assert_eq!(recent(Some(convention), &[], 10), vec!["r4"]);
    assert!(recent(Some("no such kind"), &[], 10).is_empty());
    // Any of the tags, each compared whole.
    assert_eq!(recent(None, &["api"], 10), vec!["r3", "r1"]);
    assert_eq!(recent(None, &["decision"], 10), vec!["r2", "r3"]);
    assert_eq!(recent(None, &["api", "decision", "api"], 10), vec!["r2", "r3", "r1"]);
    assert_eq!(recent(None, &["component", "nothing"], 10), vec!["r1"]);
    assert_eq!(recent(None, &["x\"decision\"y"], 10), vec!["r6"]);
    assert_eq!(recent(None, &["deci"], 10), vec!["r6"]);
    assert!(recent(None, &["nothing"], 10).is_empty());
    assert!(recent(None, &["ap"], 10).is_empty());
    assert!(recent(None, &[""], 10).is_empty());
    // Kind and tags together.
    assert_eq!(recent(Some(decision), &["api"], 10), vec!["r3"]);
    assert_eq!(recent(Some(world), &["api", "decision"], 10), vec!["r1"]);
    assert!(recent(Some(convention), &["api"], 10).is_empty());
    // The limit counts what is left after the filters.
    assert_eq!(recent(None, &[], 2), vec!["r4", "r2"]);
    assert_eq!(recent(None, &["api"], 1), vec!["r3"]);
    assert_eq!(recent(None, &["api", "decision"], 2), vec!["r2", "r3"]);
    assert!(recent(None, &[], 0).is_empty());
    assert!(recent(None, &["api"], 0).is_empty());
    assert_eq!(recent(None, &[], usize::MAX).len(), 5);
    // More tags than a statement gets conditions for.
    let mut many: Vec<String> = (0..500).map(|i| format!("tag-{i}")).collect();
    many.push("api".to_string());
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    assert_eq!(recent(None, &many, 10), vec!["r3", "r1"]);
    assert_eq!(recent(Some(world), &many, 10), vec!["r1"]);

    // Facts that became true at the same time: the one stored later comes first, then by id.
    let (_dir, store) = open();
    for (id, created_at) in [("b", 5), ("c", 9), ("a", 5)] {
        store
            .put_memory(&MemoryRec {
                created_at,
                ..memory(id, "same time", 100)
            })
            .unwrap();
    }
    let order: Vec<String> = store
        .recent_memories(None, &[], 10)
        .unwrap()
        .into_iter()
        .map(|m| m.id)
        .collect();
    assert_eq!(order, vec!["c", "a", "b"]);

    // Awkward tags are tags like any other.
    for (i, text) in AWKWARD.iter().enumerate() {
        let m = MemoryRec {
            tags: vec![text.to_string(), "common".to_string()],
            ..memory(&format!("t{i:02}"), "tagged", 1000 + i as i64)
        };
        store.put_memory(&m).unwrap();
    }
    for (i, text) in AWKWARD.iter().enumerate() {
        let found = store.recent_memories(None, &ids(&[text]), 100).unwrap();
        assert_eq!(
            found.iter().map(|m| m.id.clone()).collect::<Vec<_>>(),
            vec![format!("t{i:02}")],
            "{text:?}"
        );
    }
    assert_eq!(
        store.recent_memories(None, &ids(&["common"]), 100).unwrap().len(),
        AWKWARD.len()
    );
}

#[test]
fn memories_by_key_are_the_current_facts_with_exactly_that_key() {
    let (_dir, store) = open();
    assert!(store.memories_by_key("auth.token-expiry").unwrap().is_empty());
    let keyed = |id: &str, key: &str, at: i64| MemoryRec {
        key: key.to_string(),
        ..memory(id, &format!("fact {id}"), at)
    };
    let k1 = keyed("k1", "auth.token-expiry", 100);
    let k2 = keyed("k2", "auth.token-expiry", 200);
    let old = MemoryRec {
        invalid_at: 100,
        expired_at: 101,
        ..keyed("k3", "auth.token-expiry", 50)
    };
    let prefix = keyed("k4", "auth.token", 300);
    let keyless = keyed("k5", "", 400);
    for m in [&k1, &k2, &old, &prefix, &keyless] {
        store.put_memory(m).unwrap();
    }
    let by_key = |key: &str| -> Vec<MemoryRec> { store.memories_by_key(key).unwrap() };

    // Newest first; the superseded one is not current.
    assert_eq!(by_key("auth.token-expiry"), vec![k2.clone(), k1.clone()]);
    // The whole key, in its case.
    assert_eq!(by_key("auth.token"), vec![prefix.clone()]);
    assert!(by_key("AUTH.TOKEN-EXPIRY").is_empty());
    assert!(by_key("auth.token-expiry ").is_empty());
    assert!(by_key("auth").is_empty());
    assert!(by_key("no such key").is_empty());
    // "" is the key of the facts that have none.
    assert_eq!(by_key(""), vec![keyless.clone()]);
    assert_eq!(store.memory_counts().unwrap(), (4, 1));

    // Superseding is a write like any other: the old fact gets its end dates.
    store
        .put_memory(&MemoryRec {
            invalid_at: 200,
            expired_at: 205,
            ..k1.clone()
        })
        .unwrap();
    assert_eq!(by_key("auth.token-expiry"), vec![k2.clone()]);
    assert_eq!(store.memory_counts().unwrap(), (3, 2));
    assert_eq!(stat(&store, "memories"), 3);
    assert_eq!(stat(&store, "memories_expired"), 2);
    // Nothing was deleted.
    assert_eq!(store.get_memories(&ids(&["k1", "k3"])).unwrap().len(), 2);

    // Awkward keys are compared whole as well.
    for (i, text) in AWKWARD.iter().enumerate() {
        store
            .put_memory(&keyed(&format!("a{i:02}"), &format!("key {text}"), 1000))
            .unwrap();
    }
    for (i, text) in AWKWARD.iter().enumerate() {
        let found = by_key(&format!("key {text}"));
        assert_eq!(found.len(), 1, "{text:?}");
        assert_eq!(found[0].id, format!("a{i:02}"));
    }
}

#[test]
fn mentions_are_restored_when_a_file_is_reindexed() {
    let (_dir, store, fx, resolved) = indexed();
    let create = fx.id("orders_api.routes.create_order");
    let get = fx.id("orders_api.routes.get_order");
    let save = fx.id("orders_api.repo.save_order");
    let file = file_id(ROUTES_PY);
    let service = service_id("orders_api");
    let m = linked("m", 100, &[&create, &get, &save, &file, &service]);
    // An expired fact keeps its place in the graph too.
    let old = MemoryRec {
        expired_at: 5,
        ..linked("old", 50, &[&create])
    };
    store.put_memory(&m).unwrap();
    store.put_memory(&old).unwrap();
    assert_eq!(mentions(&store, None), 6);
    let mentioning = |nodes: &[&String]| -> Vec<(String, i64)> {
        let asked: Vec<String> = nodes.iter().map(|n| n.to_string()).collect();
        store
            .memories_mentioning(&asked, 10)
            .unwrap()
            .into_iter()
            .map(|(m, shared)| (m.id, shared))
            .collect()
    };
    assert_eq!(mentioning(&[&create, &get, &file]), vec![("m".to_string(), 3)]);

    // The file goes, and its symbols and the edges to them go with it. A
    // reader is given the links that still lead somewhere.
    store.delete_file(ROUTES_PY).unwrap();
    assert_eq!(mentions(&store, None), 2);
    assert_eq!(read(&store, "m").links, vec![save.clone(), service.clone()]);
    assert!(read(&store, "old").links.is_empty());
    assert!(mentioning(&[&create, &get, &file]).is_empty());
    // The row still knows what the fact was about.
    let recorded = store.raw("MATCH (m:Memory) WHERE m.id = 'm' RETURN m.links;").unwrap();
    for id in [&create, &get, &save, &file, &service] {
        assert!(recorded.contains(id.as_str()), "{recorded}");
    }
    // A resolve while the file is away has nothing to put back.
    store.replace_edges(&resolved, COMMIT, NOW + 8).unwrap();
    assert_eq!(mentions(&store, None), 2);

    // The file comes back: the same ids name new rows. Their links can be
    // read again at once; the edges come back with the resolve that follows.
    store.insert_parsed(fx.get(ROUTES_PY), COMMIT, NOW + 10).unwrap();
    assert_eq!(read(&store, "m"), m);
    assert_eq!(mentions(&store, None), 2);
    store.replace_edges(&resolved, COMMIT, NOW + 10).unwrap();
    assert_eq!(store.last_skipped_edges(), 0);
    assert_eq!(mentions(&store, None), 6);
    assert_eq!(mentions(&store, Some("old")), 1);
    assert_eq!(read(&store, "m"), m);
    assert_eq!(read(&store, "old"), old);
    assert_eq!(mentioning(&[&create, &get, &file]), vec![("m".to_string(), 3)]);
    // Resolving again adds nothing: an edge that is there is not made twice.
    store.replace_edges(&resolved, COMMIT, NOW + 11).unwrap();
    store.replace_edges(&Resolved::default(), COMMIT, NOW + 12).unwrap();
    assert_eq!(mentions(&store, None), 6);

    // Every file replaced in one go, as a full re-index does it.
    store.insert_parsed_bulk(&fx.files, COMMIT, NOW + 20).unwrap();
    assert_eq!(mentions(&store, None), 1, "only the service was not rewritten");
    store.replace_edges(&resolved, COMMIT, NOW + 20).unwrap();
    assert_eq!(mentions(&store, None), 6);
    assert_eq!(
        mentioning(&[&create, &get, &save, &file, &service]),
        vec![("m".to_string(), 5)]
    );

    // A whole service removed and indexed again: its node is new as well.
    for path in [ROUTES_PY, REPO_PY, EVENTS_PY, TEST_PY] {
        store.delete_file(path).unwrap();
    }
    assert!(store.nodes(std::slice::from_ref(&service)).unwrap().is_empty());
    assert_eq!(mentions(&store, None), 0);
    assert!(read(&store, "m").links.is_empty());
    store.replace_edges(&resolved, COMMIT, NOW + 30).unwrap();
    assert_eq!(mentions(&store, None), 0);
    for path in [ROUTES_PY, REPO_PY, EVENTS_PY, TEST_PY] {
        store.insert_parsed(fx.get(path), COMMIT, NOW + 31).unwrap();
    }
    store.replace_edges(&resolved, COMMIT, NOW + 31).unwrap();
    assert_eq!(mentions(&store, None), 6);
    assert_eq!(read(&store, "m"), m);

    // A file that is gone for good: its links stay out, the others stay in.
    store.delete_file(ROUTES_PY).unwrap();
    store.replace_edges(&resolved, COMMIT, NOW + 40).unwrap();
    store.replace_edges(&resolved, COMMIT, NOW + 41).unwrap();
    assert_eq!(mentions(&store, None), 2);
    assert_eq!(read(&store, "m").links, vec![save.clone(), service.clone()]);
    assert_eq!(mentioning(&[&create, &save]), vec![("m".to_string(), 1)]);
    // Stored again in that state, the fact is about what is there.
    store.put_memory(&m).unwrap();
    store.insert_parsed(fx.get(ROUTES_PY), COMMIT, NOW + 50).unwrap();
    store.replace_edges(&resolved, COMMIT, NOW + 50).unwrap();
    assert_eq!(read(&store, "m").links, vec![save, service]);
    assert_eq!(mentions(&store, Some("m")), 2);
}

#[test]
fn facts_survive_reopening() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.lbug");
    let (fx, resolved, facts) = {
        let store = Store::open(&path).unwrap();
        let (fx, resolved) = index_fixture(&store);
        let create = fx.id("orders_api.routes.create_order");
        let service = service_id("orders_api");
        let facts = vec![
            MemoryRec {
                key: "orders.retry-policy".to_string(),
                tags: vec!["decision".to_string()],
                ..linked("m1", 100, &[&create, &service])
            },
            MemoryRec {
                expired_at: 9,
                ..memory("m2", "An older truth about the quokka exporter.", 50)
            },
            memory("m3", "it's \"quoted\"\nand has a line break", 150),
        ];
        for m in &facts {
            store.put_memory(m).unwrap();
        }
        (fx, resolved, facts)
    };

    let store = Store::open(&path).unwrap();
    let asked: Vec<String> = facts.iter().map(|m| m.id.clone()).collect();
    assert_eq!(store.get_memories(&asked).unwrap(), facts);
    assert_eq!(store.memory_counts().unwrap(), (2, 1));
    assert_eq!(mentions(&store, None), 2);
    assert_eq!(
        store.memories_by_key("orders.retry-policy").unwrap(),
        vec![facts[0].clone()]
    );
    assert_eq!(
        store.recent_memories(None, &[], 10).unwrap(),
        vec![facts[2].clone(), facts[0].clone()]
    );
    let found = store.search_memories("quokka exporter", 5, true).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].0, facts[1]);
    let create = fx.id("orders_api.routes.create_order");
    assert_eq!(store.memories_mentioning(&[create], 5).unwrap()[0].0, facts[0]);

    // And the reopened store goes on: a re-index, a new fact.
    store.insert_parsed_bulk(&fx.files, COMMIT, NOW + 100).unwrap();
    store.replace_edges(&resolved, COMMIT, NOW + 100).unwrap();
    assert_eq!(mentions(&store, None), 2);
    store.put_memory(&memory("m4", "Stored after reopening.", 200)).unwrap();
    assert_eq!(store.memory_counts().unwrap(), (3, 1));
}

#[test]
fn a_fact_mentions_thousands_of_nodes_and_thousands_of_facts_are_listed() {
    let (_dir, store) = open();
    let (files, resolved, chain) = call_chain(5000);
    store.insert_parsed_bulk(&files, COMMIT, NOW).unwrap();
    store.replace_edges(&resolved, COMMIT, NOW).unwrap();

    // Every step, with an unknown id and a repeat in between.
    let mut links = Vec::new();
    for (i, id) in chain.iter().enumerate() {
        links.push(id.clone());
        links.push(format!("unknown-{i}"));
        links.push(id.clone());
    }
    let wide = MemoryRec {
        links: links.clone(),
        ..memory("wide", "mentions everything", 10)
    };
    let started = std::time::Instant::now();
    store.put_memory(&wide).unwrap();
    println!("stored a fact with {} links in {:?}", links.len(), started.elapsed());
    assert_eq!(read(&store, "wide").links, chain);
    assert_eq!(mentions(&store, Some("wide")), 5000);

    // Many small facts beside it.
    for i in 0..300 {
        let m = MemoryRec {
            links: chain[i * 3..i * 3 + 3].to_vec(),
            tags: vec![format!("tag-{}", i % 5)],
            ..memory(&format!("small-{i:03}"), "mentions three steps", 100 + i as i64)
        };
        store.put_memory(&m).unwrap();
    }
    assert_eq!(store.memory_counts().unwrap(), (301, 0));
    let found = store.memories_mentioning(&links, 1000).unwrap();
    assert_eq!(found.len(), 301);
    assert_eq!((found[0].0.id.as_str(), found[0].1), ("wide", 5000));
    assert!(found[1..].iter().all(|(_, shared)| *shared == 3));
    assert_eq!(found[1].0.id, "small-299", "of equally many, the newest first");
    assert_eq!(store.memories_mentioning(&links, 5).unwrap(), found[..5]);
    // The first step only: the wide fact and the first small one.
    assert_eq!(store.memories_mentioning(&chain[..1], 10).unwrap().len(), 2);
    assert_eq!(store.recent_memories(None, &ids(&["tag-3"]), 1000).unwrap().len(), 60);
    let all: Vec<String> = (0..300).map(|i| format!("small-{i:03}")).collect();
    assert_eq!(store.get_memories(&all).unwrap().len(), 300);

    // A re-index of everything: all 5,900 edges are put back, once.
    store.insert_parsed_bulk(&files, COMMIT, NOW + 1).unwrap();
    assert_eq!(mentions(&store, None), 0);
    let started = std::time::Instant::now();
    store.replace_edges(&resolved, COMMIT, NOW + 1).unwrap();
    println!("resolve with 5,900 mentions to restore: {:?}", started.elapsed());
    assert_eq!(mentions(&store, None), 5900);
    store.replace_edges(&resolved, COMMIT, NOW + 2).unwrap();
    assert_eq!(mentions(&store, None), 5900);
    // Stored again with half of its links, the fact has half of its edges.
    store
        .put_memory(&MemoryRec {
            links: chain[..2500].to_vec(),
            ..wide
        })
        .unwrap();
    assert_eq!(mentions(&store, Some("wide")), 2500);
}

#[test]
fn threads_remember_and_recall_next_to_an_index_run() {
    let (_dir, store, fx, resolved) = indexed();
    let create = fx.id("orders_api.routes.create_order");
    let service = service_id("orders_api");
    std::thread::scope(|scope| {
        for t in 0..4i64 {
            let (store, create, service) = (&store, &create, &service);
            scope.spawn(move || {
                for i in 0..25i64 {
                    let m = MemoryRec {
                        tags: vec![format!("thread-{t}")],
                        ..linked(&format!("t{t}-{i:02}"), t * 100 + i, &[create, service])
                    };
                    store.put_memory(&m).unwrap();
                    store
                        .put_page(&page(&format!("page-{t}-{i:02}"), "Page", "written by a thread"))
                        .unwrap();
                    // Readers run next to the writers, and see at least their own writes.
                    assert_eq!(read(store, &m.id).text, m.text);
                    assert!(!store.recent_memories(None, &ids(&[&m.tags[0]]), 5).unwrap().is_empty());
                    assert!(!store.search_memories("fact", 5, false).unwrap().is_empty());
                    assert!(!store.search_pages("thread", 5).unwrap().is_empty());
                    store.memories_mentioning(std::slice::from_ref(create), 5).unwrap();
                }
            });
        }
        // An index run next to them: the file of the mentioned symbol is
        // indexed again and resolved, over and over.
        let (store, fx, resolved) = (&store, &fx, &resolved);
        scope.spawn(move || {
            for round in 0..5 {
                store.insert_parsed(fx.get(ROUTES_PY), COMMIT, NOW + round).unwrap();
                store.replace_edges(resolved, COMMIT, NOW + round).unwrap();
            }
        });
    });

    // Every fact and page once; after a last resolve every fact is joined to
    // both of its nodes, once.
    store.replace_edges(&resolved, COMMIT, NOW + 99).unwrap();
    assert_eq!(store.memory_counts().unwrap(), (100, 0));
    assert_eq!(store.list_pages().unwrap().len(), 100);
    assert_eq!(mentions(&store, None), 200);
    let about = store
        .memories_mentioning(&[create.clone(), service.clone()], 1000)
        .unwrap();
    assert_eq!(about.len(), 100);
    assert!(about
        .iter()
        .all(|(m, shared)| *shared == 2 && m.links == [create.clone(), service.clone()]));
}

#[test]
fn full_text_indexes_are_built_with_the_first_rows() {
    let indexes = |store: &Store| -> BTreeSet<String> {
        store
            .raw("CALL SHOW_INDEXES() RETURN table_name, index_name, property_names, index_type;")
            .unwrap()
            .lines()
            .filter_map(|l| l.strip_suffix("|FTS"))
            .map(str::to_string)
            .collect()
    };
    let symbol = "Symbol|sym_fts|[name,qualified_name,signature,doc,search_text]".to_string();
    let fact = "Memory|mem_fts|[text,key,tags_text,signals]".to_string();
    let page_index = "Page|page_fts|[title,body]".to_string();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.lbug");
    let store = Store::open(&path).unwrap();
    if !store.fts_enabled() {
        // Without the extension there is never an index.
        store.put_memory(&memory("m1", "a fact", 1)).unwrap();
        store.put_page(&page("p1", "A page", "body")).unwrap();
        assert!(indexes(&store).is_empty());
        return;
    }
    // An empty table has no index: there is nothing to find in it.
    assert!(indexes(&store).is_empty());
    store.put_memory(&memory("m1", "a fact about quokkas", 1)).unwrap();
    assert_eq!(indexes(&store), BTreeSet::from([fact.clone()]));
    store.put_page(&page("p1", "A page", "about wombats")).unwrap();
    assert_eq!(indexes(&store), BTreeSet::from([fact.clone(), page_index.clone()]));
    store.insert_parsed(fixture().get(ROUTES_PY), COMMIT, NOW).unwrap();
    let all = BTreeSet::from([symbol, fact.clone(), page_index]);
    assert_eq!(indexes(&store), all);
    // Each answers from the index, not from the substring fallback.
    for (table, index, word) in [
        ("Memory", "mem_fts", "quokkas"),
        ("Page", "page_fts", "wombats"),
        ("Symbol", "sym_fts", "announce"),
    ] {
        let q = format!("CALL QUERY_FTS_INDEX('{table}', '{index}', '{word}') RETURN count(*);");
        assert_eq!(count(&store, &q), 1, "{table}");
    }

    // An index that was dropped behind the store's back is rebuilt by the
    // next write to its table, by the next resolve, or by the next open.
    store.raw("CALL DROP_FTS_INDEX('Memory', 'mem_fts');").unwrap();
    assert_eq!(
        store.search_memories("quokkas", 5, false).unwrap().len(),
        1,
        "by substring"
    );
    store.put_memory(&memory("m2", "another fact", 2)).unwrap();
    assert_eq!(indexes(&store), all);
    store.raw("CALL DROP_FTS_INDEX('Memory', 'mem_fts');").unwrap();
    store.raw("CALL DROP_FTS_INDEX('Page', 'page_fts');").unwrap();
    store.replace_edges(&Resolved::default(), COMMIT, NOW).unwrap();
    assert_eq!(indexes(&store), all);
    store.raw("CALL DROP_FTS_INDEX('Page', 'page_fts');").unwrap();
    // An index over other columns is replaced by the right one.
    store.raw("CALL DROP_FTS_INDEX('Memory', 'mem_fts');").unwrap();
    store
        .raw("CALL CREATE_FTS_INDEX('Memory', 'mem_fts', ['text']);")
        .unwrap();
    drop(store);
    let store = Store::open(&path).unwrap();
    assert_eq!(indexes(&store), all);
    assert_eq!(store.search_memories("quokkas", 5, false).unwrap().len(), 1);
    assert_eq!(store.search_pages("wombats", 5).unwrap().len(), 1);
}

#[test]
fn rows_of_an_indexed_table_are_not_written_without_the_extension() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.lbug");
    let fx = fixture();
    let create = fx.id("orders_api.routes.create_order");
    let m = linked("m1", 100, &[&create]);
    {
        let store = Store::open(&path).unwrap();
        if !store.fts_enabled() {
            // Without the extension on this machine no index is left behind.
            return;
        }
        index_fixture(&store);
        store.put_memory(&m).unwrap();
        store
            .put_page(&page("decisions", "Decisions", "We retry twice."))
            .unwrap();
        store
            .set_embeddings(VectorTable::Symbol, &[(create.clone(), vec![1.0, 0.0])])
            .unwrap();
    }

    // The same database in a process that cannot load fts: everything can be
    // read, and the searches answer by substring.
    let store = Store::open_without_fts(&path).unwrap();
    assert_eq!(read(&store, "m1"), m);
    assert_eq!(store.search_memories("fact", 5, false).unwrap().len(), 1);
    assert_eq!(
        store
            .memories_mentioning(std::slice::from_ref(&create), 5)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(store.search_pages("retry", 5).unwrap().len(), 1);
    assert_eq!(store.list_pages().unwrap().len(), 1);
    assert_eq!(
        store
            .vector_search(VectorTable::Symbol, &[1.0, 0.0], 5, 0.5)
            .unwrap()
            .len(),
        1
    );
    // A row of a table that has an index cannot be added, deleted or
    // re-worded, and the refusal says why.
    for err in [
        store.put_memory(&memory("m2", "another", 5)).unwrap_err(),
        store.put_memory(&m).unwrap_err(),
        store.put_page(&page("other", "Other", "text")).unwrap_err(),
        store.delete_page("decisions").unwrap_err(),
        store.insert_parsed(fx.get(ROUTES_PY), COMMIT, NOW).unwrap_err(),
    ] {
        assert!(format!("{err:#}").contains("fts extension is not loaded"), "{err:#}");
    }
    assert_eq!(store.memory_counts().unwrap(), (1, 0));
    assert_eq!(store.list_pages().unwrap().len(), 1);
    // The columns outside the index are written as ever: metrics and embeddings.
    let ranked = SymbolMetrics {
        id: create.clone(),
        rank: 0.5,
        community: 1,
        degree: 1,
    };
    store.set_metrics(std::slice::from_ref(&ranked)).unwrap();
    assert_eq!(
        store.metrics(std::slice::from_ref(&create)).unwrap(),
        vec![ranked.clone()]
    );
    store
        .set_embeddings(VectorTable::Symbol, &[(create.clone(), vec![0.0, 1.0])])
        .unwrap();
    store
        .set_embeddings(VectorTable::Memory, &[("m1".to_string(), vec![1.0])])
        .unwrap();
    assert!(store
        .vector_search(VectorTable::Symbol, &[1.0, 0.0], 5, 0.5)
        .unwrap()
        .is_empty());
    assert_eq!(
        store.vector_search(VectorTable::Memory, &[1.0], 5, 0.5).unwrap().len(),
        1
    );
    drop(store);

    // With the extension back, the same database is written again, and what
    // was stored without it is there.
    let store = Store::open(&path).unwrap();
    assert_eq!(store.metrics(std::slice::from_ref(&create)).unwrap(), vec![ranked]);
    store.put_memory(&memory("m2", "another", 5)).unwrap();
    store.delete_page("decisions").unwrap();
    store.insert_parsed(fx.get(ROUTES_PY), COMMIT, NOW).unwrap();
    assert_eq!(store.memory_counts().unwrap(), (2, 0));
    assert!(store.list_pages().unwrap().is_empty());
}

#[test]
fn a_table_without_an_index_is_written_without_the_extension() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.lbug");
    let fx = fixture();
    let create = fx.id("orders_api.routes.create_order");
    {
        // Only the code is indexed: nothing is remembered yet.
        let store = Store::open(&path).unwrap();
        index_fixture(&store);
    }
    let fact = linked("m1", 100, &[&create]);
    let decisions = page("decisions", "Decisions", "We retry twice.");
    {
        // Facts and pages have no index to keep up, so they are stored; the
        // symbols have one when the first process could load the extension.
        let store = Store::open_without_fts(&path).unwrap();
        store.put_memory(&fact).unwrap();
        store.put_page(&decisions).unwrap();
        assert_eq!(read(&store, "m1"), fact);
        assert_eq!(mentions(&store, None), 1);
        assert_eq!(store.search_memories("fact", 5, false).unwrap().len(), 1);
        assert_eq!(store.search_pages("retry", 5).unwrap().len(), 1);
    }
    // The next process that has the extension indexes what it finds.
    let store = Store::open(&path).unwrap();
    assert_eq!(read(&store, "m1"), fact);
    assert_eq!(store.get_page("decisions").unwrap(), Some(decisions));
    if store.fts_enabled() {
        for (table, index, word) in [("Memory", "mem_fts", "fact"), ("Page", "page_fts", "retry")] {
            let q = format!("CALL QUERY_FTS_INDEX('{table}', '{index}', '{word}') RETURN count(*);");
            assert_eq!(count(&store, &q), 1, "{table}");
        }
    }
}
