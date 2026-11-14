//! Pages: markdown views that an agent reads instead of re-deriving what is
//! already known. Memory pages are built from tagged facts; graph pages are
//! built by `lge-retrieve` after each index run. Pages live in the database
//! and are mirrored to `.lge/pages/*.md` so people and tools can read them.

use crate::recall::date;
use anyhow::{Context, Result};
use lge_core::{memory_kind, stable_id, MemoryRec, PageRec};
use lge_store::GraphStore;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;

/// Kind of pages built from facts.
pub const MEMORY_PAGE_KIND: &str = "memory";

/// The seeded pages: (slug, title, tag, fact kind that always belongs).
/// A fact belongs to a page when it carries the tag or has that kind.
pub const SEEDED: [(&str, &str, &str, &str); 5] = [
    ("components", "Components", "component", ""),
    ("concepts", "Concepts", "concept", ""),
    ("conventions", "Conventions", "convention", memory_kind::CONVENTION),
    ("decisions", "Decisions", "decision", memory_kind::DECISION),
    ("feature-work", "Feature work", "feature-work", memory_kind::EXPERIENCE),
];

/// Facts shown on one page; older ones stay searchable through recall.
const MAX_FACTS_PER_PAGE: usize = 200;
/// Characters of page text returned per search hit.
pub const SNIPPET_CHARS: usize = 280;

fn render(title: &str, facts: &[MemoryRec]) -> String {
    let mut out = format!("# {title}\n\n");
    // Facts with a key are grouped under the first part of the key, so a page
    // reads by subject; the rest follow by date.
    let mut groups: BTreeMap<String, Vec<&MemoryRec>> = BTreeMap::new();
    for m in facts {
        let subject = m.key.split('.').next().unwrap_or("").to_string();
        groups.entry(subject).or_default().push(m);
    }
    for (subject, mut list) in groups {
        list.sort_by(|a, b| b.valid_at.cmp(&a.valid_at).then(a.id.cmp(&b.id)));
        if !subject.is_empty() {
            out.push_str(&format!("## {subject}\n"));
        } else if facts.iter().any(|m| !m.key.is_empty()) {
            out.push_str("## Other\n");
        }
        for m in list {
            let seen = if m.proof_count > 1 { format!(", confirmed {} times", m.proof_count) } else { String::new() };
            out.push_str(&format!("- {} _(since {}{seen})_\n", m.text, date(m.valid_at)));
        }
        out.push('\n');
    }
    out.trim_end().to_string() + "\n"
}

/// Rebuild the seeded memory pages whose facts changed. Rendering is
/// deterministic and cheap, so a page is rewritten exactly when its text
/// would differ. Returns the slugs that were written.
pub fn refresh_pages(store: &dyn GraphStore, now: i64) -> Result<Vec<String>> {
    let mut written = Vec::new();
    for (slug, title, tag, fact_kind) in SEEDED {
        let mut facts = store.recent_memories(None, &[tag.to_string()], MAX_FACTS_PER_PAGE)?;
        if !fact_kind.is_empty() {
            for m in store.recent_memories(Some(fact_kind), &[], MAX_FACTS_PER_PAGE)? {
                if !facts.iter().any(|f| f.id == m.id) {
                    facts.push(m);
                }
            }
        }
        facts.sort_by(|a, b| b.valid_at.cmp(&a.valid_at).then(a.id.cmp(&b.id)));
        facts.truncate(MAX_FACTS_PER_PAGE);
        let existing = store.get_page(slug)?;
        if facts.is_empty() && existing.is_none() {
            continue;
        }
        let body = render(title, &facts);
        if existing.as_ref().is_some_and(|p| p.body == body) {
            continue;
        }
        store.put_page(&PageRec {
            id: stable_id(&["page", slug]),
            slug: slug.to_string(),
            title: title.to_string(),
            body,
            tags: vec![tag.to_string()],
            kind: MEMORY_PAGE_KIND.to_string(),
            updated_at: now,
            last_memory_seen_at: facts.iter().map(|m| m.created_at).max().unwrap_or(0),
        })?;
        written.push(slug.to_string());
    }
    Ok(written)
}

/// A page as a markdown file with YAML front matter.
pub fn to_markdown(page: &PageRec) -> String {
    let tags = page.tags.iter().map(|t| format!("\"{}\"", t.replace('"', ""))).collect::<Vec<_>>().join(", ");
    format!(
        "---\ntitle: \"{}\"\nslug: {}\nkind: {}\ntags: [{}]\nupdated: {}\n---\n\n{}",
        page.title.replace('"', "'"),
        page.slug,
        page.kind,
        tags,
        date(page.updated_at),
        page.body
    )
}

/// Mirror every page to `dir/<slug>.md`. A file is written only when its
/// content changes, so the directory is quiet under version control and file
/// watchers. Returns the number of files written.
pub fn export_pages(store: &dyn GraphStore, dir: &Path) -> Result<usize> {
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let mut written = 0;
    for page in store.list_pages()? {
        // A slug is a file name: anything else stays in the database only.
        if page.slug.is_empty() || !page.slug.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            continue;
        }
        let path = dir.join(format!("{}.md", page.slug));
        let text = to_markdown(&page);
        if std::fs::read_to_string(&path).is_ok_and(|old| old == text) {
            continue;
        }
        std::fs::write(&path, text).with_context(|| format!("cannot write {}", path.display()))?;
        written += 1;
    }
    Ok(written)
}

#[derive(Debug, Clone, Serialize)]
pub struct PageHit {
    pub slug: String,
    pub title: String,
    /// The part of the page around the first matching word.
    pub snippet: String,
    pub score: f64,
}

/// The stretch of `body` around the first word of `query` found in it.
pub fn snippet(body: &str, query: &str, max_chars: usize) -> String {
    let lower = body.to_lowercase();
    let at = lge_core::split_words(query).iter().filter_map(|w| lower.find(w.as_str())).min().unwrap_or(0);
    // Start at the beginning of the line that holds the match.
    let mut start = lower[..at.min(lower.len())].rfind('\n').map(|i| i + 1).unwrap_or(0);
    while !body.is_char_boundary(start) {
        start -= 1;
    }
    let out: String = body[start..].chars().take(max_chars).collect();
    let out = out.trim().to_string();
    if body[start..].chars().count() > max_chars {
        out + " ..."
    } else {
        out
    }
}

/// Search pages by keyword and return snippets, best first. A search never
/// returns whole pages or a bare list of titles: the snippet is the answer or
/// the reason to open the page.
pub fn search_pages(store: &dyn GraphStore, query: &str, limit: usize) -> Result<Vec<PageHit>> {
    Ok(store
        .search_pages(query, limit)?
        .into_iter()
        .map(|(p, score)| PageHit { snippet: snippet(&p.body, query, SNIPPET_CHARS), slug: p.slug, title: p.title, score })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fact(text: &str, key: &str, valid_at: i64, proof_count: i64) -> MemoryRec {
        MemoryRec { id: text.to_string(), text: text.to_string(), key: key.to_string(), valid_at, proof_count, ..MemoryRec::default() }
    }

    #[test]
    fn pages_group_facts_by_subject_and_are_deterministic() {
        let facts = [
            fact("Use pnpm, not npm", "web_gateway.package-manager", 1_794_441_600, 3),
            fact("Handlers stay thin; logic lives in services", "", 1_794_355_200, 1),
            fact("Routes are registered in src/routes.ts", "web_gateway.routes", 1_794_528_000, 1),
        ];
        let body = render("Conventions", &facts);
        assert_eq!(
            body,
            "# Conventions\n\n## Other\n- Handlers stay thin; logic lives in services _(since 2026-11-11)_\n\n\
             ## web_gateway\n- Routes are registered in src/routes.ts _(since 2026-11-13)_\n\
             - Use pnpm, not npm _(since 2026-11-12, confirmed 3 times)_\n"
        );
        let mut reversed = facts.to_vec();
        reversed.reverse();
        assert_eq!(render("Conventions", &reversed), body);
        assert_eq!(render("Empty", &[]), "# Empty\n");
    }

    #[test]
    fn snippets_start_at_the_matching_line() {
        let body = "# Decisions\n\n## auth\n- Tokens expire after 15 minutes _(since 2026-11-12)_\n- Refresh tokens rotate\n";
        assert_eq!(snippet(body, "token expiry", 40), "- Tokens expire after 15 minutes _(since ...");
        assert_eq!(snippet(body, "nothing matches", 12), "# Decisions ...");
        assert_eq!(snippet("ünï ß short", "ß", 280), "ünï ß short");
        assert_eq!(snippet("", "x", 10), "");
    }

    #[test]
    fn markdown_export_has_front_matter() {
        let page = PageRec {
            slug: "decisions".into(),
            title: "Decisions".into(),
            body: "# Decisions\n".into(),
            tags: vec!["decision".into()],
            kind: MEMORY_PAGE_KIND.into(),
            updated_at: 1_794_441_600,
            ..PageRec::default()
        };
        assert_eq!(
            to_markdown(&page),
            "---\ntitle: \"Decisions\"\nslug: decisions\nkind: memory\ntags: [\"decision\"]\nupdated: 2026-11-12\n---\n\n# Decisions\n"
        );
    }
}
