//! Retain, recall and pages against a real store with an indexed repository.
use lge_core::{file_id, memory_kind};
use lge_memory::*;
use lge_retrieve::{budget::count, index_repo, IndexOptions};
use lge_store::{GraphStore, Store};
use std::path::Path;

const DAY: i64 = 86_400;
const T0: i64 = 1_794_441_600; // 2026-11-12

fn indexed() -> (tempfile::TempDir, Store) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/polyglot");
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("graph.lbug")).unwrap();
    index_repo(&store, &root, &IndexOptions::default()).unwrap();
    (dir, store)
}

fn fact(text: &str, kind: &str, key: &str, tags: &[&str]) -> RetainInput {
    RetainInput {
        text: text.into(),
        kind: kind.into(),
        key: key.into(),
        tags: tags.iter().map(|t| t.to_string()).collect(),
        source: "test".into(),
        valid_at: None,
    }
}

#[test]
fn a_fact_is_stored_once_confirmed_by_repeats_and_superseded_by_key() {
    let (_dir, store) = indexed();
    let text = "create_order must reject an empty payload; the check lives in orders_api/routes.py";
    let first = retain(
        &store,
        &fact(text, memory_kind::DECISION, "orders.validation", &["Decision"]),
        T0,
    )
    .unwrap();
    assert_eq!(first.outcome, Outcome::Stored);
    let handler = store.find("orders_api.routes.create_order").unwrap()[0].id.clone();
    assert!(
        first.links.contains(&handler) && first.links.contains(&file_id("orders_api/routes.py")),
        "{:?}",
        first.links
    );

    // The same statement again, and a near copy of it, confirm the stored fact.
    let again = retain(
        &store,
        &fact(&format!("  {text}. "), memory_kind::DECISION, "orders.validation", &[]),
        T0 + 60,
    )
    .unwrap();
    assert_eq!(
        (again.outcome, again.id.as_str()),
        (Outcome::Repeated, first.id.as_str())
    );
    let typo = retain(
        &store,
        &fact(&text.replace("payload", "payloud"), memory_kind::DECISION, "", &[]),
        T0 + 120,
    )
    .unwrap();
    assert_eq!((typo.outcome, typo.id.as_str()), (Outcome::Repeated, first.id.as_str()));
    let stored = &store.get_memories(std::slice::from_ref(&first.id)).unwrap()[0];
    assert_eq!(
        (stored.proof_count, stored.tags.as_slice(), stored.expired_at),
        (3, ["decision".to_string()].as_slice(), 0)
    );
    assert_eq!(store.memory_counts().unwrap(), (1, 0));

    // A different statement about the same thing closes the old one. Nothing is deleted.
    let newer = retain(
        &store,
        &fact(
            "create_order validates the payload with a pydantic model in orders_api/schemas.py",
            memory_kind::DECISION,
            "orders.validation",
            &[],
        ),
        T0 + DAY,
    )
    .unwrap();
    assert_eq!(newer.outcome, Outcome::Superseded(vec![first.id.clone()]));
    let old = &store.get_memories(std::slice::from_ref(&first.id)).unwrap()[0];
    assert_eq!((old.invalid_at, old.expired_at), (T0 + DAY, T0 + DAY));
    assert_eq!(store.memory_counts().unwrap(), (1, 1));

    // Recall returns what holds now.
    let found = recall(
        &store,
        "how is the payload of create_order validated?",
        &RecallOptions {
            now: T0 + 2 * DAY,
            ..RecallOptions::default()
        },
        None,
    )
    .unwrap();
    assert_eq!(found.items[0].memory.id, newer.id);
    assert!(
        found.items.iter().all(|i| i.memory.id != first.id),
        "a superseded fact is not recalled"
    );
    assert!(
        found
            .text
            .starts_with("- [decision] create_order validates the payload with a pydantic model"),
        "{}",
        found.text
    );
    assert!(found.text.contains("(orders.validation; 2026-11-13)"), "{}", found.text);
    assert_eq!(found.tokens, count(&found.text));
}

#[test]
fn what_is_not_a_fact_is_refused() {
    let (_dir, store) = indexed();
    assert!(retain(&store, &fact("   ", "", "", &[]), T0).is_err());
    assert!(retain(&store, &fact("x", "rumour", "", &[]), T0)
        .unwrap_err()
        .to_string()
        .contains("unknown kind"));
    assert!(retain(&store, &fact(&"long ".repeat(500), "", "", &[]), T0)
        .unwrap_err()
        .to_string()
        .contains("one statement"));
    assert_eq!(store.memory_counts().unwrap(), (0, 0));
    // An empty kind is a world fact; a plain word is not linked to code.
    let kept = retain(
        &store,
        &fact("Deploys happen on Tuesdays and the order of steps matters", "", "", &[]),
        T0,
    )
    .unwrap();
    assert!(kept.links.is_empty(), "{:?}", kept.links);
    assert_eq!(store.get_memories(&[kept.id]).unwrap()[0].kind, memory_kind::WORLD);
}

#[test]
fn recall_finds_facts_by_the_code_they_mention_and_keeps_to_its_budget() {
    let (_dir, store) = indexed();
    // No word in common with the query below; linked to the consumer's file.
    retain(
        &store,
        &fact(
            "notifier/src/consumer.ts acks a message only after the mail went out",
            memory_kind::CONVENTION,
            "",
            &["convention"],
        ),
        T0,
    )
    .unwrap();
    for i in 0..30 {
        retain(
            &store,
            &fact(
                &format!(
                    "Unrelated operational note number {i}: rotate the staging credentials of vendor {i} every quarter"
                ),
                memory_kind::WORLD,
                "",
                &[],
            ),
            T0 + i,
        )
        .unwrap();
    }
    let opts = RecallOptions {
        now: T0 + DAY,
        ..RecallOptions::default()
    };
    let found = recall(&store, "handleOrderCreated should retry", &opts, None).unwrap();
    assert!(
        found
            .items
            .iter()
            .any(|i| i.memory.text.starts_with("notifier/src/consumer.ts acks")),
        "found through the file the symbol is in:\n{}",
        found.text
    );

    // A small budget: fewer facts, the count of the rest, and never nothing.
    let tight = recall(
        &store,
        "rotate the staging credentials",
        &RecallOptions {
            budget: 60,
            ..opts.clone()
        },
        None,
    )
    .unwrap();
    assert!(
        tight.tokens <= 60 && tight.omitted > 0 && !tight.items.is_empty(),
        "{} tokens, {} omitted",
        tight.tokens,
        tight.omitted
    );
    let one = recall(
        &store,
        "rotate the staging credentials",
        &RecallOptions {
            budget: 1,
            ..opts.clone()
        },
        None,
    )
    .unwrap();
    assert_eq!(one.items.len(), 1, "the best fact is always returned");
    // Kind and tag filters.
    let only = recall(
        &store,
        "credentials consumer",
        &RecallOptions {
            kinds: vec![memory_kind::CONVENTION.to_string()],
            ..opts.clone()
        },
        None,
    )
    .unwrap();
    assert!(!only.items.is_empty() && only.items.iter().all(|i| i.memory.kind == memory_kind::CONVENTION));
    let none = recall(
        &store,
        "zzzqqq",
        &RecallOptions {
            tags: vec!["no-such-tag".to_string()],
            ..opts
        },
        None,
    )
    .unwrap();
    assert!(none.items.is_empty() && none.text.is_empty());
}

#[test]
fn pages_follow_the_facts_and_are_written_only_when_they_change() {
    let (dir, store) = indexed();
    assert!(refresh_pages(&store, T0).unwrap().is_empty(), "no facts, no pages");
    let old = retain(
        &store,
        &fact(
            "Tokens expire after 15 minutes",
            memory_kind::DECISION,
            "auth.token-expiry",
            &[],
        ),
        T0,
    )
    .unwrap();
    retain(
        &store,
        &fact(
            "Handlers stay thin; logic lives in services",
            memory_kind::CONVENTION,
            "",
            &[],
        ),
        T0,
    )
    .unwrap();
    let mut written = refresh_pages(&store, T0 + 1).unwrap();
    written.sort();
    assert_eq!(written, ["conventions", "decisions"]);
    assert!(refresh_pages(&store, T0 + 2).unwrap().is_empty(), "nothing changed");

    // A superseding fact rewrites the page it is on, and only that page.
    retain(
        &store,
        &fact(
            "Tokens expire after 45 minutes",
            memory_kind::DECISION,
            "auth.token-expiry",
            &[],
        ),
        T0 + DAY,
    )
    .unwrap();
    assert_eq!(refresh_pages(&store, T0 + DAY).unwrap(), ["decisions"]);
    let page = store.get_page("decisions").unwrap().unwrap();
    assert!(
        page.body.contains("Tokens expire after 45 minutes") && !page.body.contains("15 minutes"),
        "{}",
        page.body
    );
    assert_eq!(
        (page.kind.as_str(), page.last_memory_seen_at),
        (MEMORY_PAGE_KIND, T0 + DAY)
    );
    assert!(store.get_memories(&[old.id]).unwrap()[0].expired_at != 0);

    // Search answers with a snippet; export mirrors pages to files once.
    let hits = search_pages(&store, "token expiry", 5).unwrap();
    assert_eq!(hits[0].slug, "decisions");
    assert!(
        hits[0].snippet.contains("Tokens expire after 45 minutes")
            && hits[0].snippet.chars().count() <= SNIPPET_CHARS + 4,
        "{:?}",
        hits[0]
    );
    let out = dir.path().join("pages");
    assert!(export_pages(&store, &out).unwrap() >= 2);
    assert_eq!(
        export_pages(&store, &out).unwrap(),
        0,
        "unchanged pages are not rewritten"
    );
    let file = std::fs::read_to_string(out.join("decisions.md")).unwrap();
    assert!(
        file.starts_with("---\ntitle: \"Decisions\"\nslug: decisions\nkind: memory\n")
            && file.contains("- Tokens expire after 45 minutes"),
        "{file}"
    );
}

fn git(dir: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .args([
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=t",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "git {args:?}");
}

#[test]
fn commits_and_merged_pull_requests_are_remembered_with_their_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q"]);
    std::fs::write(
        root.join("pricing.py"),
        "def apply_discount(total, code):\n    return total\n",
    )
    .unwrap();
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "Add pricing"]);
    git(&root, &["checkout", "-q", "-b", "fix-discount"]);
    std::fs::write(
        root.join("pricing.py"),
        "def apply_discount(total, code):\n    if code is None:\n        raise ValueError(code)\n    return total\n",
    )
    .unwrap();
    git(&root, &["commit", "-q", "-am", "Reject unknown discount codes"]);
    git(&root, &["checkout", "-q", "main"]);
    git(
        &root,
        &[
            "merge",
            "-q",
            "--no-ff",
            "fix-discount",
            "-m",
            "Merge pull request #7 from fix-discount",
        ],
    );

    let store = Store::open(&dir.path().join("graph.lbug")).unwrap();
    index_repo(&store, &root, &IndexOptions::default()).unwrap();
    assert_eq!(ingest_repo_history(&store, &root, T0).unwrap(), 3);
    assert_eq!(
        ingest_repo_history(&store, &root, T0).unwrap(),
        0,
        "nothing new: one lookup"
    );

    let merged = store.recent_memories(None, &["merge".to_string()], 5).unwrap();
    assert_eq!(merged.len(), 1);
    assert!(
        merged[0].text.starts_with("Merged ")
            && merged[0]
                .text
                .contains(": Merge pull request #7 from fix-discount (changed pricing.py)"),
        "{}",
        merged[0].text
    );
    assert_eq!(merged[0].links, vec![file_id("pricing.py")]);

    // Asking about the code brings its history along.
    let found = recall(
        &store,
        "apply_discount should accept lowercase codes",
        &RecallOptions {
            now: T0,
            ..RecallOptions::default()
        },
        None,
    )
    .unwrap();
    assert!(found.text.contains("Reject unknown discount codes"), "{}", found.text);

    // A new commit is picked up on the next call.
    std::fs::write(
        root.join("pricing.py"),
        "def apply_discount(total, code):\n    return round(total, 2)\n",
    )
    .unwrap();
    git(&root, &["commit", "-q", "-am", "Round totals"]);
    assert_eq!(ingest_repo_history(&store, &root, T0).unwrap(), 1);
}
