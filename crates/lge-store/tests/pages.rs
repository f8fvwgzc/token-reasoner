//! Pages: markdown views kept by slug, and the search over them.

mod common;

use common::*;
use lge_core::PageRec;
use lge_store::{GraphStore, Store};

fn slugs(pages: &[PageRec]) -> Vec<&str> {
    pages.iter().map(|p| p.slug.as_str()).collect()
}

#[test]
fn pages_are_kept_by_slug() {
    let (_dir, store) = open();
    assert!(store.list_pages().unwrap().is_empty());
    assert_eq!(store.get_page("decisions").unwrap(), None);
    assert_eq!(stat(&store, "pages"), 0);

    let decisions = PageRec {
        id: "p-1".to_string(),
        slug: "decisions".to_string(),
        title: "Decisions".to_string(),
        body: "# Decisions\n\n- Orders are retried twice.\n".to_string(),
        tags: vec!["decision".to_string()],
        kind: "memory".to_string(),
        updated_at: 100,
        last_memory_seen_at: 90,
    };
    let overview = PageRec {
        id: "p-2".to_string(),
        slug: "graph-overview".to_string(),
        title: "Graph overview".to_string(),
        body: "Three services, eleven files.".to_string(),
        tags: Vec::new(),
        kind: "graph".to_string(),
        updated_at: 200,
        last_memory_seen_at: 0,
    };
    let conventions = PageRec {
        tags: vec!["convention".to_string(), "style".to_string()],
        ..page("conventions", "Conventions", "Handlers are named after their route.")
    };
    for p in [&overview, &decisions, &conventions] {
        store.put_page(p).unwrap();
    }
    assert_eq!(store.get_page("decisions").unwrap(), Some(decisions.clone()));
    assert_eq!(store.get_page("graph-overview").unwrap(), Some(overview.clone()));
    // Listed by slug, whatever the order they were stored in.
    assert_eq!(
        store.list_pages().unwrap(),
        vec![conventions.clone(), decisions.clone(), overview.clone()]
    );
    // A slug is compared whole and in its case.
    for slug in ["Decisions", "decision", "decisions ", "", "p-1"] {
        assert_eq!(store.get_page(slug).unwrap(), None, "{slug:?}");
    }

    // The same slug again replaces the page, id included: there is one page per slug.
    let rebuilt = PageRec {
        id: "p-9".to_string(),
        slug: "decisions".to_string(),
        title: "Decisions, rebuilt".to_string(),
        body: "# Decisions\n\n- Orders are retried three times.\n".to_string(),
        tags: vec!["decision".to_string(), "adr".to_string()],
        kind: "manual".to_string(),
        updated_at: 300,
        last_memory_seen_at: 250,
    };
    store.put_page(&rebuilt).unwrap();
    store.put_page(&rebuilt).unwrap();
    assert_eq!(store.get_page("decisions").unwrap(), Some(rebuilt.clone()));
    assert_eq!(
        slugs(&store.list_pages().unwrap()),
        vec!["conventions", "decisions", "graph-overview"]
    );
    assert_eq!(stat(&store, "pages"), 3);

    // Deleted by slug; a slug that names no page deletes nothing.
    store.delete_page("decisions").unwrap();
    assert_eq!(store.get_page("decisions").unwrap(), None);
    assert_eq!(
        slugs(&store.list_pages().unwrap()),
        vec!["conventions", "graph-overview"]
    );
    store.delete_page("decisions").unwrap();
    store.delete_page("never-there").unwrap();
    store.delete_page("").unwrap();
    assert_eq!(stat(&store, "pages"), 2);
    // And it can be written again.
    store.put_page(&decisions).unwrap();
    assert_eq!(store.get_page("decisions").unwrap(), Some(decisions));
    assert_eq!(store.list_pages().unwrap().len(), 3);
}

#[test]
fn pages_keep_awkward_text() {
    on_both_backends(|store| {
        let mut all = Vec::new();
        for (i, text) in AWKWARD.iter().enumerate() {
            let p = PageRec {
                id: text.to_string(),
                slug: format!("slug {text}"),
                title: text.to_string(),
                body: text.to_string(),
                tags: vec![text.to_string(), "a,b".to_string(), String::new(), "\"q\"".to_string()],
                kind: text.to_string(),
                updated_at: i64::MAX - i as i64,
                last_memory_seen_at: i64::MIN + i as i64,
            };
            store.put_page(&p).unwrap();
            assert_eq!(store.get_page(&p.slug).unwrap(), Some(p.clone()), "{text:?}");
            all.push(p);
        }
        all.sort_by(|a, b| a.slug.cmp(&b.slug));
        assert_eq!(store.list_pages().unwrap(), all);

        // Nothing but defaults, and a long body.
        let empty = PageRec::default();
        store.put_page(&empty).unwrap();
        assert_eq!(store.get_page("").unwrap(), Some(empty));
        let long = page("long", "Long", &"lorem ipsum dolor ".repeat(20_000));
        store.put_page(&long).unwrap();
        assert_eq!(
            store.get_page("long").unwrap().map(|p| p.body.len()),
            Some(long.body.len())
        );

        // A search with the same text finds or does not find, but never fails.
        for text in AWKWARD {
            assert!(store.search_pages(text, 5).unwrap().len() <= 5, "{text:?}");
        }
        // Deleting by an awkward slug removes exactly that page.
        let before = store.list_pages().unwrap().len();
        for (i, text) in AWKWARD.iter().enumerate() {
            store.delete_page(&format!("slug {text}")).unwrap();
            assert_eq!(store.list_pages().unwrap().len(), before - i - 1, "{text:?}");
        }
        assert_eq!(slugs(&store.list_pages().unwrap()), vec!["", "long"]);
    });
}

/// A search that must hold what every search holds, as the slugs found.
fn search(store: &Store, text: &str, limit: usize) -> Vec<String> {
    let found = store.search_pages(text, limit).unwrap();
    assert!(found.len() <= limit, "{text}: more than {limit} pages");
    assert!(
        found.windows(2).all(|w| w[0].1 >= w[1].1),
        "{text}: not ordered by score"
    );
    assert!(
        found.iter().all(|(_, score)| *score > 0.0),
        "{text}: a page without a score"
    );
    // A found page is the page a lookup by slug gives.
    for (p, _) in &found {
        assert_eq!(store.get_page(&p.slug).unwrap().as_ref(), Some(p));
    }
    found.into_iter().map(|(p, _)| p.slug).collect()
}

#[test]
fn pages_are_found_by_title_and_body() {
    on_both_backends(|store| {
        assert!(search(store, "quokka", 10).is_empty(), "there are no pages yet");
        let policy = page(
            "retry-policy",
            "Retry policy",
            "Uploads are retried twice, then parked in the dead letter queue.",
        );
        let deploys = page(
            "deploys",
            "Deploys",
            "A deploy needs a green pipeline. The retry policy does not apply to it.",
        );
        let glossary = page("glossary", "Glossary", "Quokka: the internal name of the exporter.");
        let bugs = page(
            "known-bugs",
            "Known bugs",
            "The exporter drops the last row. No fix yet.",
        );
        for p in [&policy, &deploys, &glossary, &bugs] {
            store.put_page(p).unwrap();
        }

        // By a word of the body, in any case, and by a word of the title.
        assert_eq!(search(store, "quokka", 10), vec!["glossary"]);
        assert_eq!(search(store, "QUOKKA?", 10), vec!["glossary"]);
        assert_eq!(search(store, "glossary", 10), vec!["glossary"]);
        assert_eq!(search(store, "pipeline", 10), vec!["deploys"]);
        // The page about the words comes before the page that mentions them.
        assert_eq!(search(store, "retry policy", 10), vec!["retry-policy", "deploys"]);
        assert_eq!(search(store, "retry policy", 1), vec!["retry-policy"]);
        assert!(search(store, "retry policy", 0).is_empty());
        assert_eq!(search(store, "retry policy", usize::MAX).len(), 2);
        // The page comes back whole.
        assert_eq!(store.search_pages("quokka", 10).unwrap()[0].0, glossary);
        // Words a symbol search leaves out are words of a page.
        assert_eq!(search(store, "bugs", 10), vec!["known-bugs"]);

        // BM25 knows the forms of a word; the substring scorer compares letters.
        let stemmed = search(store, "parking", 10);
        if store.fts_enabled() {
            assert_eq!(stemmed, vec!["retry-policy"], "`parked` and `parking` are one word");
        } else {
            assert!(stemmed.is_empty());
        }

        // Nothing to search for, or nothing that matches.
        for text in [
            "",
            "   ",
            "the and for",
            "!!! ---",
            "a b",
            "zzzqqqxxx",
            "' OR 1=1 --",
            "$q",
        ] {
            assert!(search(store, text, 10).is_empty(), "{text:?}");
        }

        // The search follows the page: its new text is found, its old text is not.
        store
            .put_page(&page(
                "glossary",
                "Glossary",
                "Wombat: the internal name of the importer.",
            ))
            .unwrap();
        assert!(search(store, "quokka", 10).is_empty());
        assert_eq!(search(store, "wombat importer", 10), vec!["glossary"]);
        // A deleted page is not found any more.
        store.delete_page("glossary").unwrap();
        assert!(search(store, "wombat importer", 10).is_empty());
        assert_eq!(search(store, "exporter", 10), vec!["known-bugs"]);
    });
}

/// A word no other page has: the number of the page spelled in letters.
fn marker(i: i64) -> String {
    let letters: String = i
        .to_string()
        .bytes()
        .map(|digit| char::from(b'a' + (digit - b'0')))
        .collect();
    format!("zq{letters}")
}

#[test]
fn pages_survive_reopening_and_come_in_hundreds() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.lbug");
    let mut pages: Vec<PageRec> = (0..300)
        .map(|i| PageRec {
            tags: vec![format!("community-{}", i % 7)],
            updated_at: 1000 + i,
            ..page(
                &format!("community-{i:03}"),
                &format!("Community {i}"),
                &format!("Symbols of community {i}: {}.\nIt's \"generated\".", marker(i)),
            )
        })
        .collect();
    {
        let store = Store::open(&path).unwrap();
        // Stored in reverse, listed by slug.
        for p in pages.iter().rev() {
            store.put_page(p).unwrap();
        }
        assert_eq!(store.list_pages().unwrap(), pages);
    }

    let store = Store::open(&path).unwrap();
    assert_eq!(store.list_pages().unwrap(), pages);
    assert_eq!(stat(&store, "pages"), 300);
    assert_eq!(store.get_page("community-217").unwrap().as_ref(), Some(&pages[217]));
    // Every page has the word; the limit is kept.
    assert_eq!(search(&store, "symbols community", 25).len(), 25);
    assert_eq!(search(&store, "symbols community", 1000).len(), 300);
    assert_eq!(search(&store, &marker(217), 5), vec!["community-217"]);
    assert_eq!(
        search(&store, &format!("symbols {}", marker(42)), 5)[0],
        "community-042"
    );

    // Rewritten and deleted after reopening.
    pages[5].body = "Rebuilt after the index run.".to_string();
    pages[5].last_memory_seen_at = 77;
    store.put_page(&pages[5]).unwrap();
    store.delete_page("community-299").unwrap();
    pages.pop();
    assert_eq!(store.list_pages().unwrap(), pages);
    assert_eq!(search(&store, "rebuilt", 5), vec!["community-005"]);
}
