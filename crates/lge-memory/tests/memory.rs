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

#[test]
fn a_changed_value_supersedes_instead_of_merging() {
    let (_dir, store) = indexed();
    let memo = |n: u32| {
        format!("The nightly export job writes its summary to the shared bucket and then notifies the team channel memo {n}")
    };
    let mut ids: Vec<String> = Vec::new();
    for n in 1..=3u32 {
        let r = retain(
            &store,
            &fact(&memo(n), memory_kind::DECISION, "export.summary", &[]),
            T0 + i64::from(n) * 60,
        )
        .unwrap();
        let expected = match ids.last() {
            None => Outcome::Stored,
            Some(prev) => Outcome::Superseded(vec![prev.clone()]),
        };
        assert_eq!(r.outcome, expected, "memo {n}");
        ids.push(r.id);
    }
    assert_eq!(store.memory_counts().unwrap(), (1, 2));
    let found = recall(
        &store,
        "what does the nightly export job do with its summary?",
        &RecallOptions {
            now: T0 + DAY,
            ..RecallOptions::default()
        },
        None,
    )
    .unwrap();
    assert_eq!(found.items[0].memory.text, memo(3));
    for m in store.get_memories(&ids).unwrap() {
        assert_ne!(m.proof_count, 4, "no fact absorbed the others");
    }

    // The same text twice is still one fact with two proofs.
    let once = retain(
        &store,
        &fact(&memo(3), memory_kind::DECISION, "export.summary", &[]),
        T0 + 600,
    )
    .unwrap();
    assert_eq!((once.outcome, once.id.as_str()), (Outcome::Repeated, ids[2].as_str()));
    assert_eq!(store.get_memories(&ids[2..]).unwrap()[0].proof_count, 2);
}

// ---- the relevance floor: recency re-ranks, it never admits ------------------

/// 12 unrelated commit facts, oldest first, then one convention that is the oldest of all.
/// Returns the commit ids oldest first and the convention id.
fn commit_corpus(store: &Store) -> (Vec<String>, String) {
    let convention = retain(
        store,
        &fact(
            "Handlers stay thin; logic lives in services",
            memory_kind::CONVENTION,
            "",
            &["convention"],
        ),
        T0 - 100,
    )
    .unwrap()
    .id;
    let commits = (0..12)
        .map(|i| {
            let r = retain(
                store,
                &fact(
                    &format!("Commit {i} changed file_{i}.py"),
                    memory_kind::EXPERIENCE,
                    "",
                    &[],
                ),
                T0 + i,
            )
            .unwrap();
            assert_eq!(r.outcome, Outcome::Stored, "commit {i} is its own fact");
            r.id
        })
        .collect();
    (commits, convention)
}

fn pack_options() -> RecallOptions {
    RecallOptions {
        now: T0 + DAY,
        limit: 8,
        ..RecallOptions::default()
    }
}

fn ids(found: &Recall) -> Vec<String> {
    found.items.iter().map(|i| i.memory.id.clone()).collect()
}

#[test]
fn a_query_that_matches_nothing_recalls_nothing_unless_recency_is_admitted() {
    let (_dir, store) = indexed();
    let (commits, _) = commit_corpus(&store);
    assert_eq!(store.memory_counts().unwrap(), (13, 0));
    let junk = "zzzzqq nonexistentword";

    let floor = recall(&store, junk, &pack_options(), None).unwrap();
    assert!(floor.items.is_empty() && floor.text.is_empty(), "{}", floor.text);
    assert_eq!((floor.tokens, floor.omitted), (0, 0));
    assert!(floor.recency_only >= 8, "{}", floor.recency_only);

    let explicit = recall(
        &store,
        junk,
        &RecallOptions {
            admit_recent_only: true,
            ..pack_options()
        },
        None,
    )
    .unwrap();
    // The old behaviour: the newest eight facts, newest first.
    let golden: Vec<String> = commits.iter().rev().take(8).cloned().collect();
    assert_eq!(ids(&explicit), golden);
    assert!(explicit.tokens > 0 && explicit.tokens == count(&explicit.text));
    assert_eq!(
        explicit.recency_only, floor.recency_only,
        "the counter is filled in both modes"
    );
}

#[test]
fn a_matching_query_admits_only_the_hit_and_counts_what_it_withheld() {
    let (_dir, store) = indexed();
    let (_, convention) = commit_corpus(&store);
    let opts = RecallOptions {
        limit: 20,
        ..pack_options()
    };
    let found = recall(&store, "thin handlers", &opts, None).unwrap();
    assert_eq!(ids(&found), vec![convention.clone()], "{}", found.text);
    // The recent arm saw all 13 facts; 12 of them are not the hit.
    assert_eq!(found.recency_only, 12);

    let explicit = recall(
        &store,
        "thin handlers",
        &RecallOptions {
            admit_recent_only: true,
            ..opts
        },
        None,
    )
    .unwrap();
    assert_eq!(explicit.items[0].memory.id, convention, "the hit is still first");
    assert_eq!(explicit.items.len(), 13, "explicit recall also lists the recent facts");
    assert_eq!(explicit.recency_only, 12);
}

#[test]
fn recency_adds_rank_to_a_hit_but_never_admits_a_miss() {
    let (_dir, store) = indexed();
    let mut stored = Vec::new();
    // The older hit, then five facts that do not contain the query word, then the newest hit.
    let old = retain(
        &store,
        &fact("quokka retries use exponential backoff", memory_kind::DECISION, "", &[]),
        T0,
    )
    .unwrap()
    .id;
    for i in 0..5 {
        stored.push(
            retain(
                &store,
                &fact(
                    &format!("Commit {i} changed file_{i}.py"),
                    memory_kind::EXPERIENCE,
                    "",
                    &[],
                ),
                T0 + 10 + i,
            )
            .unwrap()
            .id,
        );
    }
    let new = retain(
        &store,
        &fact(
            "quokka timeouts are logged with context",
            memory_kind::DECISION,
            "",
            &[],
        ),
        T0 + 100,
    )
    .unwrap()
    .id;
    // limit 3: the recent arm returns the newest hit and two commits, not the older hit.
    let opts = RecallOptions {
        limit: 3,
        ..pack_options()
    };
    let floor = recall(&store, "quokka", &opts, None).unwrap();
    assert_eq!(
        ids(&floor),
        vec![new.clone(), old.clone()],
        "the hit that is also recent ranks first"
    );
    assert_eq!(floor.recency_only, 2, "two commits came only from the recent arm");

    let explicit = recall(
        &store,
        "quokka",
        &RecallOptions {
            admit_recent_only: true,
            ..opts
        },
        None,
    )
    .unwrap();
    let order = ids(&explicit);
    assert_eq!(order.len(), 4);
    assert_eq!(&order[..2], &[new, old], "no zero-hit fact above a hit");
    assert!(order[2..].iter().all(|id| stored.contains(id)));
}

#[test]
fn the_best_admitted_fact_is_forced_and_filters_still_apply() {
    let (_dir, store) = indexed();
    let (_, convention) = commit_corpus(&store);
    let tiny = |admit: bool| RecallOptions {
        budget: 1,
        admit_recent_only: admit,
        ..pack_options()
    };
    let hits = recall(&store, "Commit changed", &tiny(false), None).unwrap();
    assert_eq!(hits.items.len(), 1, "the best matching fact is forced past the budget");
    assert!(hits.omitted > 0);
    assert_eq!(recall(&store, "zzzzqq", &tiny(true), None).unwrap().items.len(), 1);
    let none = recall(&store, "zzzzqq", &tiny(false), None).unwrap();
    assert!(none.items.is_empty() && none.omitted == 0, "no hit, nothing to force");

    // Kind and tag filters.
    let by_kind = recall(
        &store,
        "thin handlers",
        &RecallOptions {
            kinds: vec![memory_kind::EXPERIENCE.to_string()],
            ..pack_options()
        },
        None,
    )
    .unwrap();
    assert!(
        by_kind.items.is_empty(),
        "the only hit is filtered out; the commits are not admitted"
    );
    let by_tag = recall(
        &store,
        "thin handlers",
        &RecallOptions {
            tags: vec!["convention".to_string()],
            ..pack_options()
        },
        None,
    )
    .unwrap();
    assert_eq!(ids(&by_tag), vec![convention]);
    let unknown = recall(
        &store,
        "thin handlers",
        &RecallOptions {
            tags: vec!["no-such-tag".to_string()],
            admit_recent_only: true,
            ..pack_options()
        },
        None,
    )
    .unwrap();
    assert!(unknown.items.is_empty() && unknown.text.is_empty());
}

#[test]
fn the_floor_is_deterministic() {
    let (_dir, store) = indexed();
    commit_corpus(&store);
    // Equal facts rank equally: ties break by id.
    for query in ["Commit changed file", "zzzzqq"] {
        for admit in [false, true] {
            let opts = RecallOptions {
                admit_recent_only: admit,
                ..pack_options()
            };
            let a = recall(&store, query, &opts, None).unwrap();
            let b = recall(&store, query, &opts, None).unwrap();
            assert_eq!(a.text, b.text, "{query:?} admit={admit}");
            assert_eq!(a.text.as_bytes(), b.text.as_bytes());
        }
    }
    let found = recall(&store, "Commit changed file", &pack_options(), None).unwrap();
    for pair in found.items.windows(2) {
        assert!(
            pair[0].score > pair[1].score || pair[0].memory.id < pair[1].memory.id,
            "equal scores are ordered by id"
        );
    }
}

// ---- mass commits: no links for huge commits, no admission by the query arms -----

/// A git repository of `n` generated files under `svc_alpha/`, committed in one
/// commit, and a store with the files indexed. File `i` defines `handler_{i:03}`.
fn mass_repo(n: usize) -> (tempfile::TempDir, std::path::PathBuf, Store) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(root.join("svc_alpha")).unwrap();
    git(&root, &["init", "-q"]);
    for i in 0..n {
        std::fs::write(
            root.join(format!("svc_alpha/file_{i:03}.py")),
            format!("def handler_{i:03}(x):\n    return x\n"),
        )
        .unwrap();
    }
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "Initial import"]);
    let store = Store::open(&dir.path().join("graph.lbug")).unwrap();
    index_repo(&store, &root, &IndexOptions::default()).unwrap();
    (dir, root, store)
}

fn file_ids(range: std::ops::Range<usize>) -> Vec<String> {
    range.map(|i| file_id(&format!("svc_alpha/file_{i:03}.py"))).collect()
}

/// A fact stored the way the code before the mass-commit change stored a commit:
/// straight through `put_memory`, linked to every file given.
fn put_linked(store: &Store, key: &str, text: &str, links: Vec<String>, valid_at: i64) -> String {
    let id = lge_core::stable_id(&["memory", key]);
    store
        .put_memory(&lge_core::MemoryRec {
            id: id.clone(),
            kind: memory_kind::EXPERIENCE.to_string(),
            text: text.to_string(),
            key: key.to_string(),
            tags: vec!["commit".to_string()],
            source: "test".to_string(),
            created_at: valid_at,
            valid_at,
            invalid_at: 0,
            expired_at: 0,
            proof_count: 1,
            links,
        })
        .unwrap();
    id
}

#[test]
fn a_mass_commit_is_stored_without_links_and_a_small_one_keeps_all_of_them() {
    let (_dir, root, store) = mass_repo(250);
    assert_eq!(ingest_repo_history(&store, &root, T0).unwrap(), 1);
    let mass = store.recent_memories(None, &["mass-commit".to_string()], 5).unwrap();
    assert_eq!(mass.len(), 1);
    assert!(mass[0].links.is_empty(), "{} links", mass[0].links.len());
    assert!(mass[0].tags.contains(&"commit".to_string()), "{:?}", mass[0].tags);
    assert!(
        mass[0].text.starts_with("Commit ") && mass[0].text.ends_with("and 242 more)"),
        "{}",
        mass[0].text
    );
    assert_eq!(ingest_repo_history(&store, &root, T0).unwrap(), 0, "idempotent");

    // The boundary: exactly MASS_FILES files keeps its links, one more loses them.
    assert_eq!(MASS_FILES, 200);
    let paths = |n: usize| (0..n).map(|i| format!("svc_alpha/file_{i:03}.py")).collect::<Vec<_>>();
    let at = |sha: &str, n: usize| Commit {
        sha: sha.into(),
        time: T0,
        subject: "Touch files".into(),
        files: paths(n),
        merge: false,
    };
    assert_eq!(ingest_commits(&store, &[at("aaaa00000001", 200)], T0).unwrap(), 1);
    assert_eq!(ingest_commits(&store, &[at("bbbb00000002", 201)], T0).unwrap(), 1);
    let kept = store.memories_by_key(&commit_key("aaaa00000001")).unwrap();
    let dropped = store.memories_by_key(&commit_key("bbbb00000002")).unwrap();
    assert_eq!((kept[0].links.len(), dropped[0].links.len()), (200, 0));
    assert!(!kept[0].tags.contains(&"mass-commit".to_string()));
    assert_eq!(dropped[0].tags, vec!["commit".to_string(), "mass-commit".to_string()]);

    // A merge that is also a mass commit: tags in the fixed order.
    let merge = Commit {
        merge: true,
        ..at("cccc00000003", 250)
    };
    ingest_commits(&store, &[merge], T0).unwrap();
    let merged = store.memories_by_key(&commit_key("cccc00000003")).unwrap();
    assert_eq!(
        merged[0].tags,
        vec!["commit".to_string(), "merge".to_string(), "mass-commit".to_string()]
    );
}

#[test]
fn a_twelve_file_commit_keeps_twelve_links() {
    let (_dir, root, store) = mass_repo(12);
    assert_eq!(ingest_repo_history(&store, &root, T0).unwrap(), 1);
    let facts = store.recent_memories(None, &["commit".to_string()], 5).unwrap();
    assert_eq!(facts[0].links.len(), 12);
    assert!(!facts[0].tags.contains(&"mass-commit".to_string()));
    assert_eq!(ingest_repo_history(&store, &root, T0).unwrap(), 0);
}

#[test]
fn a_fact_linked_to_every_file_is_never_admitted_by_a_query_arm() {
    let (_dir, root, store) = mass_repo(210);
    // One more file, with a name no other file has, for the mass fact to mention.
    std::fs::write(
        root.join("svc_alpha/zebra_ledger.py"),
        "def zebra_ledger(x):\n    return x\n",
    )
    .unwrap();
    index_repo(&store, &root, &IndexOptions::default()).unwrap();
    let mut every_file = file_ids(0..210);
    every_file.push(file_id("svc_alpha/zebra_ledger.py"));
    // Legacy data: stored through the pre-change path, 210 links, no migration.
    let m = put_linked(
        &store,
        "commit:legacy0001",
        "Commit legacy1 on 2026-11-12: Initial import (changed svc_alpha/file_000.py and 209 more)",
        every_file,
        T0,
    );
    let stored = store.get_memories(std::slice::from_ref(&m)).unwrap();
    assert_eq!(stored[0].links.len(), 211, "the row keeps all its links");
    let s = put_linked(
        &store,
        "decision:handler17",
        "Marmot retries wrap the seventeenth handler",
        file_ids(17..18),
        T0 - DAY,
    );

    let opts = RecallOptions {
        now: T0,
        limit: 8,
        ..RecallOptions::default()
    };
    let found = recall(&store, "handler_017 should retry", &opts, None).unwrap();
    assert_eq!(ids(&found), vec![s.clone()], "{}", found.text);
    assert!(found.mass_skipped >= 1, "{}", found.mass_skipped);
    assert_eq!(found.recency_only, 1, "the mass fact is still listed by the recent arm");

    // Neither the keyword arm (the names of the 211 linked files are in its text
    // index) nor the entity arm (a seed it is linked to) admits it.
    let by_signal = recall(&store, "zebra_ledger", &opts, None).unwrap();
    assert!(by_signal.items.is_empty(), "{}", by_signal.text);
    assert!(by_signal.mass_skipped >= 1);

    // Explicit recall keeps the old behaviour: the recent arm may return it.
    let explicit = recall(
        &store,
        "handler_017 should retry",
        &RecallOptions {
            admit_recent_only: true,
            ..opts.clone()
        },
        None,
    )
    .unwrap();
    assert_eq!(
        ids(&explicit),
        vec![s, m],
        "the hit first, then the recent mass commit, as before"
    );
    assert!(explicit.mass_skipped >= 1);
}

#[test]
fn the_entity_arm_prefers_the_fact_about_fewer_things() {
    let (_dir, _root, store) = mass_repo(60);
    let query = "handler_001 handler_002";
    // Both facts share the two seed files; the tie is decided by their link counts.
    let a = put_linked(
        &store,
        "k:a",
        "Alpha note about two files",
        file_ids(1..3),
        T0 - 5 * DAY,
    );
    let mut broad = Vec::new();
    for i in 0..3 {
        broad.push(put_linked(
            &store,
            &format!("k:b{i}"),
            &format!("Bravo note {i} about forty files"),
            file_ids(1..41),
            T0 - i,
        ));
    }
    let opts = RecallOptions {
        now: T0,
        limit: 8,
        ..RecallOptions::default()
    };
    let found = recall(&store, query, &opts, None).unwrap();
    assert_eq!(found.items.len(), 4, "{}", found.text);
    assert_eq!(found.mass_skipped, 0, "forty links is not a mass fact");
    for _ in 0..5 {
        let again = recall(&store, query, &opts, None).unwrap();
        assert_eq!(again.text.as_bytes(), found.text.as_bytes());
    }

    // With room for two facts per arm, the store alone would hand the entity arm
    // the two newest broad facts; the wider fetch lets the narrow one in.
    let narrow = recall(&store, query, &RecallOptions { limit: 1, ..opts }, None).unwrap();
    assert!(ids(&narrow).contains(&a), "{}", narrow.text);
}

// ---- review fixes ------------------------------------------------------------

#[test]
fn facts_the_guard_keeps_apart_get_their_own_ids_even_in_the_same_second() {
    let (_dir, store) = indexed();
    // Same kind, same second, texts that normalize alike but differ in case.
    for (key, a, b) in [
        (
            "",
            "TLS 1.3 is required for the public gateway listener port",
            "tls 1.3 is required for the public gateway listener port",
        ),
        ("", "fooBar helper", "foobar helper"),
        (
            "tls",
            "TLS 1.3 is required for the public listener port of the gateway",
            "tls 1.3 is required for the public listener port of the gateway",
        ),
    ] {
        let first = retain(&store, &fact(a, memory_kind::WORLD, key, &[]), T0).unwrap();
        let second = retain(&store, &fact(b, memory_kind::WORLD, key, &[]), T0).unwrap();
        assert_ne!(first.id, second.id, "{a:?} against {b:?}");
        assert_ne!(second.outcome, Outcome::Repeated);
        let rows = store.get_memories(&[first.id.clone(), second.id.clone()]).unwrap();
        assert_eq!(rows.len(), 2, "both facts exist: {a:?}");
        assert!(rows.iter().any(|m| m.text == a), "the first text was not overwritten");
        assert!(rows.iter().any(|m| m.text == b));
        if key.is_empty() {
            assert!(rows.iter().all(|m| m.expired_at == 0));
        } else {
            let old = rows.iter().find(|m| m.id == first.id).unwrap();
            assert_eq!((old.invalid_at, old.expired_at), (T0, T0), "superseded, not rewritten");
            assert_eq!(second.outcome, Outcome::Superseded(vec![first.id.clone()]));
        }
    }
    // The same exact text in the same second is still one fact.
    let t = "Retries use jittered backoff in the gateway client";
    let x = retain(&store, &fact(t, memory_kind::WORLD, "", &[]), T0).unwrap();
    let y = retain(&store, &fact(t, memory_kind::WORLD, "", &[]), T0).unwrap();
    assert_eq!((y.outcome, y.id.as_str()), (Outcome::Repeated, x.id.as_str()));
}

#[test]
fn mass_facts_do_not_take_the_keyword_slots_of_real_hits() {
    let (_dir, root, store) = mass_repo(210);
    std::fs::write(root.join("svc_alpha/extra.py"), "def extra(x):\n    return x\n").unwrap();
    index_repo(&store, &root, &IndexOptions::default()).unwrap();
    // Ten legacy mass facts that match the query many times over, then one real fact.
    for i in 0..10 {
        put_linked(
            &store,
            &format!("commit:legacy{i:04}"),
            &format!("marmot marmot marmot marmot marmot marmot import {i}"),
            file_ids(0..210),
            T0 - i,
        );
    }
    let real = put_linked(
        &store,
        "decision:marmot",
        "The sanctuary feeds each marmot twice a day",
        file_ids(3..4),
        T0 - DAY,
    );
    let found = recall(
        &store,
        "marmot",
        &RecallOptions {
            now: T0,
            limit: 2,
            ..RecallOptions::default()
        },
        None,
    )
    .unwrap();
    assert_eq!(ids(&found), vec![real], "{}", found.text);
    assert_eq!(found.mass_skipped, 10);
}

struct Constant;
impl Embedder for Constant {
    fn name(&self) -> &str {
        "constant-1"
    }
    fn embed(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| vec![1.0]).collect())
    }
}

#[test]
fn the_vector_arm_never_returns_a_mass_fact() {
    let (_dir, _root, store) = mass_repo(210);
    let mass = put_linked(
        &store,
        "commit:legacy0001",
        "Initial import of everything",
        file_ids(0..210),
        T0,
    );
    let real = put_linked(
        &store,
        "decision:plain",
        "Sanctuary opening hours are fixed",
        file_ids(3..4),
        T0 - DAY,
    );
    store
        .set_embeddings(
            lge_core::VectorTable::Memory,
            &[(mass.clone(), vec![1.0]), (real.clone(), vec![1.0])],
        )
        .unwrap();
    let opts = RecallOptions {
        now: T0,
        limit: 8,
        ..RecallOptions::default()
    };
    // No keyword or entity hit: only the vector arm admits anything.
    let found = recall(&store, "zzzzqq", &opts, Some(&Constant)).unwrap();
    assert_eq!(ids(&found), vec![real], "{}", found.text);
    assert_eq!(found.mass_skipped, 1);
}
