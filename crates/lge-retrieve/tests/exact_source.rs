//! The source of the starting symbols in a pack is the file's text, with the
//! doc comments and attributes written above them, while the file is the one
//! that was indexed.
use lge_retrieve::*;
use lge_store::Store;
use std::path::PathBuf;

const CART: &str = r#"//! Cart arithmetic.

use std::fmt;

/// Totals a cart.
///
/// Prices are in cents.
#[inline]
#[must_use]
pub fn cart_total(items: &[u32]) -> u32 {
    items.iter().sum()
}

/// A line of the cart.
#[derive(
    Debug,
    Clone,
)]
pub struct LineItem {
    pub cents: u32,
}

/// Pays for a cart.
pub fn checkout(items: &[u32]) -> String {
    format!("{}", cart_total(items))
}
"#;

fn indexed() -> (tempfile::TempDir, PathBuf, Store) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("shop");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/cart.rs"), CART).unwrap();
    let store = Store::open(&dir.path().join("graph.lbug")).unwrap();
    let report = index_repo(&store, &root, &IndexOptions::default()).unwrap();
    assert_eq!((report.indexed, report.failed.len()), (1, 0), "{report:?}");
    (dir, root, store)
}

fn pack(store: &Store, task: &str, budget: usize) -> EvidencePack {
    evidence_pack(
        store,
        task,
        &PackOptions {
            budget,
            ..PackOptions::default()
        },
    )
    .unwrap()
}

/// The text of the source block that follows the line of `name` under "Start here".
fn block_after<'a>(text: &'a str, name: &str) -> &'a str {
    let line = text
        .lines()
        .find(|l| l.contains(&format!(" {name}")) && l.starts_with("src/"))
        .unwrap_or_else(|| panic!("no line for {name}\n{text}"));
    let after = &text[text.find(line).unwrap() + line.len()..];
    let start = after
        .find("```\n")
        .unwrap_or_else(|| panic!("no source for {name}\n{text}"))
        + 4;
    let end = start + after[start..].find("\n```").unwrap();
    &after[start..end]
}

#[test]
fn the_starting_symbol_is_shown_with_its_doc_comment_and_attributes_as_written() {
    let (_dir, _root, store) = indexed();
    let pack = pack(&store, "cart_total must skip free items", 6000);
    assert_eq!(pack.seeds[0].qualified_name, "src.cart.cart_total", "{:?}", pack.seeds);
    let text = &pack.text;
    assert_eq!(
        block_after(text, "src.cart.cart_total"),
        "/// Totals a cart.\n///\n/// Prices are in cents.\n#[inline]\n#[must_use]\npub fn cart_total(items: &[u32]) -> u32 {\n    items.iter().sum()\n}",
        "{text}"
    );
    // The one-line summary is not repeated over the source that has the doc.
    let seed_line = text.lines().find(|l| l.contains(" src.cart.cart_total")).unwrap();
    assert!(!seed_line.contains("--"), "{seed_line}");
    assert!(!text.contains("-- Totals a cart."), "{text}");
    // A neighbour keeps its one line with the doc.
    assert!(
        text.lines()
            .any(|l| l.contains("src.cart.checkout") && l.contains("  -- Pays for a cart.")),
        "{text}"
    );
    assert!(pack.tokens <= pack.budget && pack.tokens == budget::count(text));
    assert_eq!(
        pack.text,
        self::pack(&store, "cart_total must skip free items", 6000).text
    );
}

#[test]
fn an_attribute_over_several_lines_comes_whole() {
    let (_dir, _root, store) = indexed();
    let pack = pack(&store, "LineItem needs a quantity", 6000);
    assert_eq!(pack.seeds[0].qualified_name, "src.cart.LineItem", "{:?}", pack.seeds);
    assert_eq!(
        block_after(&pack.text, "src.cart.LineItem"),
        "/// A line of the cart.\n#[derive(\n    Debug,\n    Clone,\n)]\npub struct LineItem {\n    pub cents: u32,\n}",
        "{}",
        pack.text
    );
}

#[test]
fn a_file_changed_since_it_was_indexed_falls_back_to_the_stored_body() {
    let (_dir, root, store) = indexed();
    // Lines move: what was read above the symbol would be the wrong lines.
    std::fs::write(root.join("src/cart.rs"), format!("// moved\n\n{CART}")).unwrap();
    let pack = pack(&store, "cart_total must skip free items", 6000);
    let block = block_after(&pack.text, "src.cart.cart_total");
    assert_eq!(
        block, "pub fn cart_total(items: &[u32]) -> u32 {\n    items.iter().sum()\n}",
        "{}",
        pack.text
    );
    let seed_line = pack.text.lines().find(|l| l.contains(" src.cart.cart_total")).unwrap();
    assert!(!seed_line.contains("--"), "the source is shown: {seed_line}");

    // Deleted: the same.
    std::fs::remove_file(root.join("src/cart.rs")).unwrap();
    let pack = self::pack(&store, "cart_total must skip free items", 6000);
    assert!(block_after(&pack.text, "src.cart.cart_total").starts_with("pub fn cart_total"));
}

#[test]
fn a_budget_too_small_for_the_source_keeps_the_limit() {
    let (_dir, _root, store) = indexed();
    let mut without_source = 0;
    for budget in [60, 120, 160, 200, 400, 6000] {
        let pack = pack(&store, "cart_total must skip free items", budget);
        let text = &pack.text;
        let seed_line = text
            .lines()
            .find(|l| l.contains(" src.cart.cart_total") && l.starts_with("src/"))
            .unwrap();
        if text.contains("```") {
            assert!(!seed_line.contains("--"), "{budget}: {text}");
        } else {
            without_source += 1;
            // The summary is all the reader gets of the doc then, when it fits.
            if seed_line.ends_with("-- Totals a cart.") {
                assert!(pack.tokens <= budget, "{budget}: {text}");
            }
        }
        // Below that, the lines that are never cut take more than the budget.
        if budget >= 200 {
            assert!(pack.tokens <= budget, "{budget}: {} tokens\n{text}", pack.tokens);
        }
    }
    assert!(without_source > 0, "a budget too small for the source was tried");
}

// ---- adversarial: what is not a leading doc or attribute ------------------------

/// Index `files` (path, text) under a fresh repository.
fn indexed_files(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf, Store) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("proj");
    for (path, text) in files {
        let p = root.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
    let store = Store::open(&dir.path().join("graph.lbug")).unwrap();
    let report = index_repo(&store, &root, &IndexOptions::default()).unwrap();
    assert_eq!(report.failed.len(), 0, "{report:?}");
    (dir, root, store)
}

/// The source block that follows the "Start here" line of `qualified_name`.
fn seed_block<'a>(text: &'a str, qualified_name: &str) -> Option<&'a str> {
    let start_here = text.find("## Start here\n")?;
    let rest = &text[start_here..];
    let line = rest
        .lines()
        .find(|l| l.ends_with(&format!(" {qualified_name}")) || l.contains(&format!(" {qualified_name}  ")))?;
    let after = &rest[rest.find(line)? + line.len() + 1..];
    let block = after.strip_prefix("```\n")?;
    Some(&block[..block.find("\n```")?])
}

#[test]
fn a_comment_at_the_end_of_the_previous_python_block_is_not_the_next_functions_doc() {
    let src = "def first(x):\n    y = x + 1\n    # the end of first, about y\ndef refund_order(order):\n    return order.total\n";
    let (_dir, _root, store) = indexed_files(&[("shop/orders.py", src)]);
    let pack = pack(&store, "refund_order must refund the whole total", 6000);
    assert_eq!(
        pack.seeds[0].qualified_name, "shop.orders.refund_order",
        "{:?}",
        pack.seeds
    );
    let block = seed_block(&pack.text, "shop.orders.refund_order").unwrap_or_else(|| panic!("{}", pack.text));
    assert!(block.starts_with("def refund_order(order):"), "{block}");
}

#[test]
fn a_python_comment_that_starts_with_a_bracket_does_not_pull_code_into_the_doc() {
    let src = "#[note]\nLIMITS = [\n    1,\n    2,\n]\ndef refund_order(order):\n    return order.total\n";
    let (_dir, _root, store) = indexed_files(&[("shop/orders.py", src)]);
    let pack = pack(&store, "refund_order must refund the whole total", 6000);
    let block = seed_block(&pack.text, "shop.orders.refund_order").unwrap_or_else(|| panic!("{}", pack.text));
    assert!(block.starts_with("def refund_order(order):"), "{block}");
}

#[test]
fn a_markdown_bullet_above_a_heading_is_not_part_of_the_section() {
    let src = "# Guide\n\n## Setup\n* install the toolchain\n## Deployment rollback\nRun the rollback script.\nThen check the dashboards.\n";
    let (_dir, _root, store) = indexed_files(&[("docs/guide.md", src)]);
    let pack = pack(&store, "Deployment rollback section needs a step", 6000);
    let Some(seed) = pack
        .seeds
        .iter()
        .find(|s| s.qualified_name.ends_with("Deployment rollback"))
    else {
        panic!("the section is not a seed: {:?}\n{}", pack.seeds, pack.text);
    };
    let block = seed_block(&pack.text, &seed.qualified_name).unwrap_or_else(|| panic!("{}", pack.text));
    assert!(!block.contains("install the toolchain"), "{block}");
}

#[test]
fn the_pack_is_byte_stable_while_the_file_matches_the_index() {
    let (_dir, _root, store) = indexed();
    let task = "cart_total and checkout must skip free items";
    let first = pack(&store, task, 1500);
    for _ in 0..3 {
        let again = pack(&store, task, 1500);
        assert_eq!(again.text, first.text);
        assert_eq!(again.tokens, first.tokens);
    }
    assert!(first.tokens <= first.budget, "{}", first.text);
    // No seed line both shows a source block and repeats its doc.
    let lines: Vec<&str> = first.text.lines().collect();
    for (i, l) in lines.iter().enumerate() {
        if l.contains("  -- ") && lines.get(i + 1) == Some(&"```") {
            panic!("doc repeated over a source block: {l}\n{}", first.text);
        }
    }
}
