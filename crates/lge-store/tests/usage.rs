//! The usage log and the handoff notes.

mod common;

use common::*;
use lge_core::{outcome, HandoffNote, UsageRecord};
use lge_store::GraphStore;

fn usage(id: &str, task_id: &str, attempt: i64, started_at: i64) -> UsageRecord {
    UsageRecord {
        id: id.to_string(),
        task_id: task_id.to_string(),
        task: format!("task {task_id}: fix the 'orders' endpoint\nand its \"tests\""),
        attempt,
        level: "medium".to_string(),
        reason: "several files in one service".to_string(),
        runner: "codex".to_string(),
        model: "gpt-5-codex".to_string(),
        session_id: format!("session-{id}"),
        started_at,
        finished_at: started_at + 90,
        input_tokens: 120_000 + attempt,
        cached_input_tokens: 80_000,
        reasoning_tokens: 9_000,
        output_tokens: 4_321,
        pack_tokens: 5_900,
        outcome: outcome::SUCCESS.to_string(),
        escalated: attempt > 1,
        files: vec![
            "orders_api/routes.py".to_string(),
            "orders_api/repo, with comma.py".to_string(),
        ],
        services: vec!["orders_api".to_string()],
    }
}

fn note(id: &str, created_at: i64) -> HandoffNote {
    HandoffNote {
        id: id.to_string(),
        task_id: format!("task-{id}"),
        task: "fix the orders endpoint".to_string(),
        created_at,
        text: format!("note {id}\n- changed `create_order`\n- tests: 'ok', \"all green\""),
    }
}

#[test]
fn usage_records_round_trip_oldest_first() {
    let (_dir, store) = open();
    assert!(store.list_usage(0).unwrap().is_empty());

    let first = usage("u1", "t1", 1, 1000);
    let retry = usage("u2", "t1", 2, 1200);
    let other = usage("u3", "t2", 1, 1100);
    // Empty lists, empty strings and a failure are records too.
    let bare = UsageRecord {
        id: "u4".to_string(),
        task_id: "t3".to_string(),
        attempt: 1,
        started_at: 1300,
        outcome: outcome::FAILURE.to_string(),
        ..UsageRecord::default()
    };
    for rec in [&retry, &bare, &first, &other] {
        store.put_usage(rec).unwrap();
    }

    assert_eq!(
        store.list_usage(0).unwrap(),
        vec![first.clone(), other.clone(), retry.clone(), bare.clone()]
    );
    // `since` is inclusive.
    assert_eq!(
        store.list_usage(1100).unwrap(),
        vec![other.clone(), retry.clone(), bare.clone()]
    );
    assert_eq!(store.list_usage(1101).unwrap(), vec![retry.clone(), bare.clone()]);
    assert!(store.list_usage(5000).unwrap().is_empty());

    // Putting a record with a known id replaces it.
    let finished = UsageRecord {
        outcome: outcome::FAILURE.to_string(),
        output_tokens: 99,
        files: vec![],
        services: vec!["orders_api".to_string(), "notifier".to_string()],
        ..first.clone()
    };
    store.put_usage(&finished).unwrap();
    assert_eq!(store.list_usage(0).unwrap(), vec![finished, other, retry, bare]);
}

#[test]
fn attempts_of_one_task_in_the_same_second_stay_in_attempt_order() {
    let (_dir, store) = open();
    // Ids chosen so that id order and attempt order disagree.
    let a1 = usage("z", "t1", 1, 500);
    let a2 = usage("y", "t1", 2, 500);
    let a3 = usage("x", "t1", 3, 500);
    for rec in [&a3, &a1, &a2] {
        store.put_usage(rec).unwrap();
    }
    assert_eq!(store.list_usage(500).unwrap(), vec![a1, a2, a3]);
}

#[test]
fn handoff_notes_round_trip_newest_first() {
    let (_dir, store) = open();
    assert!(store.latest_handoffs(5).unwrap().is_empty());

    let (old, mid, new) = (note("a", 100), note("b", 200), note("c", 300));
    for n in [&mid, &new, &old] {
        store.put_handoff(n).unwrap();
    }
    assert_eq!(
        store.latest_handoffs(10).unwrap(),
        vec![new.clone(), mid.clone(), old.clone()]
    );
    assert_eq!(store.latest_handoffs(1).unwrap(), vec![new.clone()]);
    assert_eq!(store.latest_handoffs(2).unwrap(), vec![new.clone(), mid.clone()]);
    assert!(store.latest_handoffs(0).unwrap().is_empty());

    // Same id: the note is replaced, and its new time decides its place.
    let rewritten = HandoffNote {
        created_at: 400,
        text: "rewritten".to_string(),
        ..old
    };
    store.put_handoff(&rewritten).unwrap();
    assert_eq!(store.latest_handoffs(10).unwrap(), vec![rewritten, new, mid]);
}

#[test]
fn the_usage_log_is_independent_of_the_graph() {
    let (_dir, store, _fx, resolved) = indexed();
    let rec = usage("u1", "t1", 1, 1000);
    store.put_usage(&rec).unwrap();
    store.put_handoff(&note("a", 100)).unwrap();
    let stats = store.stats().unwrap();

    // Re-resolving and deleting files leave it alone.
    store.replace_edges(&resolved, COMMIT, NOW + 50).unwrap();
    store.delete_file(ROUTES_PY).unwrap();
    assert_eq!(store.list_usage(0).unwrap(), vec![rec]);
    assert_eq!(store.latest_handoffs(5).unwrap(), vec![note("a", 100)]);
    // And it does not show up in the graph counts.
    store.put_usage(&usage("u2", "t1", 2, 1001)).unwrap();
    store.insert_parsed(fixture().get(ROUTES_PY), COMMIT, NOW).unwrap();
    store.replace_edges(&resolved, COMMIT, NOW + 5).unwrap();
    assert_eq!(store.stats().unwrap(), stats);
}
