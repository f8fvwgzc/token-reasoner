//! Storing a fact. The agent writes the fact itself at the end of a task, so
//! no extraction model runs here. The store keeps one current fact per thing
//! known: a repeat raises a counter, and a newer fact with the same key closes
//! the older one instead of deleting it.

use crate::dedup::{normalize, same_fact};
use anyhow::{bail, Result};
use lge_core::{file_id, kind, memory_kind, service_id, stable_id, MemoryRec, NodeLabel};
use lge_retrieve::name_candidates;
use lge_store::GraphStore;
use serde::Serialize;
use std::collections::BTreeSet;

/// Longest fact stored. A fact is one statement, not a document.
pub const MAX_FACT_CHARS: usize = 2000;
/// A name in a fact that matches more symbols than this links to none of them.
const MAX_LINKS_PER_NAME: usize = 3;

#[derive(Debug, Clone, Default)]
pub struct RetainInput {
    pub text: String,
    /// One of `lge_core::memory_kind`; "" means `world`.
    pub kind: String,
    /// What the fact is about (`auth.token-expiry`). A newer fact with the same
    /// key supersedes the current one. "" = never superseded.
    pub key: String,
    pub tags: Vec<String>,
    /// A task id, a session id, a file path or `user`.
    pub source: String,
    /// When the fact became true; `None` means now.
    pub valid_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum Outcome {
    /// A new fact was stored.
    Stored,
    /// The same fact was already there; its proof count went up.
    Repeated,
    /// A new fact was stored and it closed these older facts with the same key.
    Superseded(Vec<String>),
}

#[derive(Debug, Clone, Serialize)]
pub struct Retained {
    pub id: String,
    pub outcome: Outcome,
    /// Code entities the fact was linked to.
    pub links: Vec<String>,
}

fn clean_tags(tags: &[String]) -> Vec<String> {
    let set: BTreeSet<String> = tags
        .iter()
        .map(|t| t.trim().to_lowercase())
        .filter(|t| !t.is_empty())
        .collect();
    set.into_iter().collect()
}

/// Ids of the symbols, files and services a text mentions by name. A name is
/// linked only when it is specific: at most a few symbols carry it.
pub fn entity_links(store: &dyn GraphStore, text: &str) -> Result<Vec<String>> {
    let mut links: BTreeSet<String> = BTreeSet::new();
    let services = store.nodes_by_label(NodeLabel::Service, 10_000)?;
    for name in name_candidates(text) {
        let path = name.trim_start_matches("./");
        if store.file_info(path)?.is_some() {
            links.insert(file_id(path));
            continue;
        }
        if services.iter().any(|s| s.name == name) {
            links.insert(service_id(&name));
            continue;
        }
        // Plain words are too often ordinary language: only shaped names
        // (snake_case, camelCase, dotted) link to symbols.
        let shaped = name.contains('_')
            || name.contains('.')
            || name
                .chars()
                .zip(name.chars().skip(1))
                .any(|(a, b)| a.is_lowercase() && b.is_uppercase());
        if !shaped {
            continue;
        }
        let hits: Vec<_> = store
            .find(&name)?
            .into_iter()
            .filter(|h| h.kind != kind::MODULE && !h.is_test)
            .collect();
        if !hits.is_empty() && hits.len() <= MAX_LINKS_PER_NAME {
            links.extend(hits.into_iter().map(|h| h.id));
        }
    }
    Ok(links.into_iter().collect())
}

/// Store a fact. `now` is unix seconds.
pub fn retain(store: &dyn GraphStore, input: &RetainInput, now: i64) -> Result<Retained> {
    let text = input.text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        bail!("a fact needs text");
    }
    if text.chars().count() > MAX_FACT_CHARS {
        bail!("a fact is one statement of at most {MAX_FACT_CHARS} characters; split it or store a page");
    }
    let fact_kind = if input.kind.is_empty() {
        memory_kind::WORLD
    } else {
        input.kind.as_str()
    };
    if !memory_kind::ALL.contains(&fact_kind) {
        bail!("unknown kind `{fact_kind}`; use one of {}", memory_kind::ALL.join(", "));
    }
    let key = input.key.trim().to_lowercase();
    let tags = clean_tags(&input.tags);
    let links = entity_links(store, &text)?;
    let valid_at = input.valid_at.unwrap_or(now);

    // The same fact again: raise its proof count and keep the older record.
    let same_key = if key.is_empty() {
        Vec::new()
    } else {
        store.memories_by_key(&key)?
    };
    let mut candidates: Vec<MemoryRec> = same_key.clone();
    candidates.extend(store.search_memories(&text, 20, false)?.into_iter().map(|(m, _)| m));
    if let Some(existing) = candidates
        .iter()
        .find(|m| m.kind == fact_kind && same_fact(&text, &m.text))
    {
        let mut updated = existing.clone();
        updated.proof_count = existing.proof_count.max(1) + 1;
        updated.tags = clean_tags(&[existing.tags.clone(), tags].concat());
        updated.links = [existing.links.clone(), links]
            .concat()
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if updated.key.is_empty() {
            updated.key = key;
        }
        store.put_memory(&updated)?;
        return Ok(Retained {
            id: updated.id.clone(),
            outcome: Outcome::Repeated,
            links: updated.links,
        });
    }

    // A different fact about the same thing: close the old ones, never delete.
    let mut replaced: Vec<String> = Vec::new();
    for mut old in same_key {
        old.invalid_at = valid_at;
        old.expired_at = now;
        store.put_memory(&old)?;
        replaced.push(old.id);
    }

    let id = stable_id(&["memory", fact_kind, &normalize(&text), &now.to_string()]);
    let rec = MemoryRec {
        id: id.clone(),
        kind: fact_kind.to_string(),
        text,
        key,
        tags,
        source: input.source.clone(),
        created_at: now,
        valid_at,
        invalid_at: 0,
        expired_at: 0,
        proof_count: 1,
        links,
    };
    store.put_memory(&rec)?;
    let outcome = if replaced.is_empty() {
        Outcome::Stored
    } else {
        Outcome::Superseded(replaced)
    };
    Ok(Retained {
        id,
        outcome,
        links: rec.links,
    })
}
