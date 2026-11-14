//! Seeds: the symbols a task starts from.
//!
//! Two signals are combined. A name written in the task (`verify_token`,
//! `TaskService.create_task`, a plain word that is exactly a symbol name) is an
//! exact seed. Full-text search over names, signatures, docs and split
//! identifier words gives the rest. Few seeds are better than many: every seed
//! is expanded and shown in full.

use anyhow::Result;
use lge_core::Hit;
use lge_store::GraphStore;
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize)]
pub struct Seed {
    pub hit: Hit,
    /// 1.0 for a name written in the task; below 1.0, the share of the best
    /// search score.
    pub relevance: f64,
    /// The task names this symbol.
    pub exact: bool,
}

#[derive(Debug, Clone)]
pub struct SeedOptions {
    pub max_seeds: usize,
    /// Restrict search to one service.
    pub service: Option<String>,
    /// Search hits scoring below this share of the best hit are dropped.
    pub score_ratio: f64,
    /// A written name that matches more symbols than this is too vague to be
    /// an exact seed; search decides instead.
    pub max_same_name: usize,
}

impl Default for SeedOptions {
    fn default() -> Self {
        SeedOptions {
            max_seeds: 3,
            service: None,
            score_ratio: 0.6,
            max_same_name: 4,
        }
    }
}

/// Weight of a test symbol when the task does not talk about tests.
const TEST_WEIGHT: f64 = 0.3;

/// Words of the task that could be symbol names, most specific first:
/// backticked spans, then tokens shaped like identifiers or paths, then plain
/// words of four letters or more.
pub fn name_candidates(task: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |s: &str| {
        let s = s.trim_matches(|c: char| !(c.is_alphanumeric() || c == '_'));
        if s.chars().count() >= 3 && !out.iter().any(|o| o == s) {
            out.push(s.to_string());
        }
    };
    for (i, span) in task.split('`').enumerate() {
        if i % 2 == 1 {
            push(span);
        }
    }
    let tokens: Vec<&str> = task
        .split(|c: char| c.is_whitespace() || matches!(c, '`' | ',' | ';' | '(' | ')' | '"' | '\''))
        .collect();
    for t in &tokens {
        let t = t.trim_end_matches(['.', ':', '!', '?']);
        let shaped = t.contains('_')
            || t.contains("::")
            || t.contains('/')
            || t.split('.').filter(|p| !p.is_empty()).count() > 1
            || t.chars()
                .zip(t.chars().skip(1))
                .any(|(a, b)| a.is_lowercase() && b.is_uppercase());
        if shaped {
            push(t);
            // `TaskService.create_task` also names `create_task`; a path names its file.
            let last = match t.rsplit_once('/') {
                Some((_, file)) => file,
                None => t.rsplit(['.', ':']).next().unwrap_or(t),
            };
            push(last);
        }
    }
    for t in &tokens {
        let t = t.trim_end_matches(['.', ':', '!', '?']);
        if t.chars().count() >= 4 && t.chars().all(|c| c.is_alphanumeric()) {
            push(t);
        }
    }
    out.truncate(32);
    out
}

/// Full-text search, within a service when the store supports it.
pub(crate) fn search(store: &dyn GraphStore, text: &str, service: Option<&str>, limit: usize) -> Result<Vec<Hit>> {
    match store.search_in(text, service, limit) {
        Ok(hits) => Ok(hits),
        // A store without `search_in`: search everything and filter here.
        Err(_) => {
            let mut hits = store.search(text, if service.is_some() { limit * 4 } else { limit })?;
            if let Some(s) = service {
                hits.retain(|h| h.service == s);
            }
            hits.truncate(limit);
            Ok(hits)
        }
    }
}

/// The symbols `task` starts from, best first.
pub fn find_seeds(store: &dyn GraphStore, task: &str, opts: &SeedOptions) -> Result<Vec<Seed>> {
    let about_tests = lge_core::split_words(task)
        .iter()
        .any(|w| w == "test" || w == "tests" || w == "spec");
    let in_scope = |h: &Hit| opts.service.as_deref().is_none_or(|s| h.service == s);
    // id -> seed, keeping the highest relevance.
    let mut found: BTreeMap<String, Seed> = BTreeMap::new();

    for name in name_candidates(task) {
        let mut hits: Vec<Hit> = store
            .find(&name)?
            .into_iter()
            .filter(|h| in_scope(h) && h.kind != lge_core::kind::MODULE)
            .collect();
        if !about_tests && hits.iter().any(|h| !h.is_test) {
            hits.retain(|h| !h.is_test);
        }
        if hits.is_empty() || hits.len() > opts.max_same_name {
            continue;
        }
        for hit in hits {
            found.insert(
                hit.id.clone(),
                Seed {
                    hit,
                    relevance: 1.0,
                    exact: true,
                },
            );
        }
    }

    let hits = search(store, task, opts.service.as_deref(), opts.max_seeds.max(1) * 4)?;
    let weight = |h: &Hit| if h.is_test && !about_tests { TEST_WEIGHT } else { 1.0 };
    let best = hits.iter().map(|h| h.score * weight(h)).fold(0.0, f64::max);
    // When the task names a symbol, search only adds what scores almost as well
    // as its best hit: the named symbol is where the work starts.
    let ratio = if found.is_empty() {
        opts.score_ratio
    } else {
        opts.score_ratio.max(0.85)
    };
    for hit in hits {
        let score = hit.score * weight(&hit);
        if best <= 0.0 || score < best * ratio {
            continue;
        }
        // Search never outranks a name the task wrote down.
        let relevance = 0.9 * score / best;
        found.entry(hit.id.clone()).or_insert(Seed {
            hit,
            relevance,
            exact: false,
        });
    }

    let mut seeds: Vec<Seed> = found.into_values().collect();
    seeds.sort_by(|a, b| {
        b.relevance
            .total_cmp(&a.relevance)
            .then(a.hit.is_test.cmp(&b.hit.is_test))
            .then(a.hit.qualified_name.cmp(&b.hit.qualified_name))
    });
    seeds.truncate(opts.max_seeds);
    Ok(seeds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_written_in_a_task_are_candidates() {
        let c = name_candidates(
            "Fix `TaskService.create_task`: verify_token must check expiry in auth_svc/tokens.py, see getOrderHandler.",
        );
        for want in [
            "TaskService.create_task",
            "create_task",
            "verify_token",
            "auth_svc/tokens.py",
            "tokens.py",
            "getOrderHandler",
            "expiry",
            "check",
        ] {
            assert!(c.iter().any(|x| x == want), "{want} missing in {c:?}");
        }
        assert_eq!(c[0], "TaskService.create_task", "backticked names come first");
        assert!(!c.iter().any(|x| x == "in" || x == "see" || x == "Fix"), "{c:?}");
        assert!(name_candidates("").is_empty());
    }
}
