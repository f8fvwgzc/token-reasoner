//! Recalling facts for a query, under a token budget.
//!
//! The design follows hindsight's recall: several cheap retrieval arms run
//! side by side, each returning a ranked list of ids; the lists are fused by
//! rank; small boosts favour recent and repeatedly confirmed facts; the result
//! is packed in rank order until the budget is full.

use anyhow::Result;
use lge_core::{MemoryRec, VectorTable};
use lge_retrieve::algo::{rrf, RRF_K};
use lge_retrieve::budget::{count, Budget};
use lge_retrieve::{find_seeds, SeedOptions};
use lge_store::GraphStore;
use serde::Serialize;
use std::collections::{HashMap, HashSet};

/// Turns text into vectors. Optional: without one, recall uses the other arms.
pub trait Embedder: Send + Sync {
    /// A name that changes when the vectors would: model and dimension.
    fn name(&self) -> &str;
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
}

#[derive(Debug, Clone)]
pub struct RecallOptions {
    /// Tokens the answer may take.
    pub budget: usize,
    /// Facts considered per arm.
    pub limit: usize,
    /// Keep only these kinds; empty = all.
    pub kinds: Vec<String>,
    /// Keep only facts carrying one of these tags; empty = all.
    pub tags: Vec<String>,
    /// Unix seconds, for the recency boost.
    pub now: i64,
    /// Admit facts that only the recency arm returned. `false` (the default, for
    /// injected packs): recency re-ranks facts another arm found, it never admits
    /// one, so a query with no keyword, entity or vector hit recalls nothing.
    /// `true` (explicit recall, by a person or the agent): the old behaviour, the
    /// newest facts fill the answer when nothing matches.
    pub admit_recent_only: bool,
}

impl Default for RecallOptions {
    fn default() -> Self {
        RecallOptions {
            budget: 800,
            limit: 20,
            kinds: Vec::new(),
            tags: Vec::new(),
            now: 0,
            admit_recent_only: false,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Recalled {
    pub memory: MemoryRec,
    pub score: f64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Recall {
    /// The facts that fit the budget, best first.
    pub items: Vec<Recalled>,
    /// The text for the model: one line per fact.
    pub text: String,
    pub tokens: usize,
    /// Facts that matched but did not fit.
    pub omitted: usize,
    /// Facts only the recency arm returned; withheld unless `admit_recent_only`.
    /// Filled in both modes, so the two behaviours can be compared from one call.
    pub recency_only: usize,
    /// Distinct mass facts (more than `MASS_LINKS` links) kept out of the keyword,
    /// entity and vector arms in this call. Filled in both modes.
    pub mass_skipped: usize,
}

/// A fact linked to more nodes than this is a mass fact (a huge commit). It is
/// about everything and so about nothing: the query arms never return it.
pub const MASS_LINKS: usize = crate::history::MASS_FILES;

/// Cosine similarity below this is not a match (hindsight's semantic floor).
const MIN_SIMILARITY: f64 = 0.3;
const DAY: f64 = 86_400.0;

/// `clamp(1 - days/365, 0.1, 1)`: a fact from today is 1, a year old is 0.1.
fn recency(valid_at: i64, now: i64) -> f64 {
    let days = (now - valid_at).max(0) as f64 / DAY;
    (1.0 - days / 365.0).clamp(0.1, 1.0)
}

/// `clamp(0.5 + ln(proof_count)/10, 0, 1)`: confirmed facts rank a little higher.
fn proof(proof_count: i64) -> f64 {
    (0.5 + (proof_count.max(1) as f64).ln() / 10.0).clamp(0.0, 1.0)
}

/// One line per fact: `- [decision] text (key; 2026-11-12; x3)`.
pub fn fact_line(m: &MemoryRec) -> String {
    let mut notes: Vec<String> = Vec::new();
    if !m.key.is_empty() {
        notes.push(m.key.clone());
    }
    notes.push(date(m.valid_at));
    if m.proof_count > 1 {
        notes.push(format!("x{}", m.proof_count));
    }
    format!("- [{}] {} ({})", m.kind, m.text, notes.join("; "))
}

/// `YYYY-MM-DD` of a unix time, UTC.
pub fn date(unix: i64) -> String {
    // Civil-from-days (Howard Hinnant's algorithm), to avoid a date dependency.
    let days = unix.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}")
}

fn is_mass(m: &MemoryRec) -> bool {
    m.links.len() > MASS_LINKS
}

/// `facts` without the mass ones, which are remembered in `mass`.
fn without_mass(facts: Vec<MemoryRec>, mass: &mut HashSet<String>) -> Vec<MemoryRec> {
    facts
        .into_iter()
        .filter(|m| {
            let big = is_mass(m);
            if big {
                mass.insert(m.id.clone());
            }
            !big
        })
        .collect()
}

/// The entity arm's list: mass facts dropped, the rest ordered by the nodes
/// they share with the query over the square root of their own link count (a
/// fact about two things that both matter beats one about forty that touches
/// one of them), ties by newest `valid_at`, then id; cut to `wide`.
fn damped(found: Vec<(MemoryRec, i64)>, wide: usize, mass: &mut HashSet<String>) -> Vec<MemoryRec> {
    let mut scored: Vec<(f64, MemoryRec)> = found
        .into_iter()
        .filter(|(m, _)| {
            let big = is_mass(m);
            if big {
                mass.insert(m.id.clone());
            }
            !big
        })
        .map(|(m, shared)| (shared as f64 / (m.links.len().max(1) as f64).sqrt(), m))
        .collect();
    scored.sort_by(|(a, x), (b, y)| b.total_cmp(a).then(y.valid_at.cmp(&x.valid_at)).then(x.id.cmp(&y.id)));
    scored.truncate(wide);
    scored.into_iter().map(|(_, m)| m).collect()
}

/// Facts relevant to `query`, best first, packed to the budget.
pub fn recall(
    store: &dyn GraphStore,
    query: &str,
    opts: &RecallOptions,
    embedder: Option<&dyn Embedder>,
) -> Result<Recall> {
    let wide = opts.limit.max(1) * 2;
    let mut by_id: HashMap<String, MemoryRec> = HashMap::new();
    let mut lists: Vec<(f64, Vec<String>)> = Vec::new();
    // Ids returned by the arms that depend on the query (keyword, entity, vector).
    // The recent arm is not one of them: it ranks, it does not admit.
    let mut eligible: HashSet<String> = HashSet::new();
    // Mass facts the query arms dropped, counted once each.
    let mut mass_ids: HashSet<String> = HashSet::new();
    let mut arm = |weight: f64, relevant: bool, facts: Vec<MemoryRec>, by_id: &mut HashMap<String, MemoryRec>| {
        let ids: Vec<String> = facts.iter().map(|m| m.id.clone()).collect();
        if relevant {
            eligible.extend(ids.iter().cloned());
        }
        for m in facts {
            by_id.entry(m.id.clone()).or_insert(m);
        }
        lists.push((weight, ids));
    };

    // Keyword arm. A mass fact matches through the huge `signals` text; drop it.
    // Fetch wider than `wide` so mass facts do not take the slots of real hits.
    let keyword: Vec<MemoryRec> = store
        .search_memories(query, (wide * 4).max(64), false)?
        .into_iter()
        .map(|(m, _)| m)
        .collect();
    let mut keyword = without_mass(keyword, &mut mass_ids);
    keyword.truncate(wide);
    arm(1.0, true, keyword, &mut by_id);

    // Entity arm: facts that mention the code the query is about.
    let seeds = find_seeds(
        store,
        query,
        &SeedOptions {
            max_seeds: 5,
            ..SeedOptions::default()
        },
    )?;
    let mut node_ids: Vec<String> = seeds.iter().map(|s| s.hit.id.clone()).collect();
    node_ids.extend(seeds.iter().map(|s| lge_core::file_id(&s.hit.file_path)));
    node_ids.sort();
    node_ids.dedup();
    if !node_ids.is_empty() {
        let fetch = (wide * 4).max(64);
        let mentioning = store.memories_mentioning(&node_ids, fetch)?;
        arm(1.0, true, damped(mentioning, wide, &mut mass_ids), &mut by_id);
    }

    // Recent arm, at half weight: being new is not being relevant.
    arm(
        0.5,
        false,
        store.recent_memories(None, &opts.tags, opts.limit.max(1))?,
        &mut by_id,
    );

    // Vector arm, when an embedder is plugged in and the table has vectors.
    if let Some(e) = embedder {
        if let Some(v) = e.embed(&[query.to_string()])?.into_iter().next() {
            let hits = store.vector_search(VectorTable::Memory, &v, wide, MIN_SIMILARITY)?;
            let ids: Vec<String> = hits.into_iter().map(|(id, _)| id).collect();
            let facts = store.get_memories(&ids)?;
            let order: HashMap<&str, usize> = ids.iter().enumerate().map(|(i, id)| (id.as_str(), i)).collect();
            let mut facts: Vec<MemoryRec> = facts.into_iter().filter(|m| m.expired_at == 0).collect();
            facts.sort_by_key(|m| order.get(m.id.as_str()).copied().unwrap_or(usize::MAX));
            arm(1.0, true, without_mass(facts, &mut mass_ids), &mut by_id);
        }
    }

    let keep = |m: &MemoryRec| {
        (opts.kinds.is_empty() || opts.kinds.contains(&m.kind))
            && (opts.tags.is_empty() || m.tags.iter().any(|t| opts.tags.contains(t)))
    };
    let fused: Vec<MemoryRec> = rrf(&lists, RRF_K)
        .into_iter()
        .filter_map(|(id, _)| by_id.remove(&id))
        .filter(keep)
        .collect();

    // The relevance floor: a fact no query-dependent arm returned is withheld.
    let recency_only = fused.iter().filter(|m| !eligible.contains(&m.id)).count();
    let fused: Vec<MemoryRec> = if opts.admit_recent_only {
        fused
    } else {
        fused.into_iter().filter(|m| eligible.contains(&m.id)).collect()
    };
    let mass_skipped = mass_ids.len();
    if fused.is_empty() {
        return Ok(Recall {
            recency_only,
            mass_skipped,
            ..Recall::default()
        });
    }

    // Rank-seeded score with small boosts, then the final order.
    let n = fused.len();
    let mut scored: Vec<Recalled> = fused
        .into_iter()
        .enumerate()
        .map(|(rank, memory)| {
            let base = if n > 1 {
                1.0 - 0.9 * rank as f64 / (n - 1) as f64
            } else {
                1.0
            };
            let score = base
                * (1.0 + 0.2 * (recency(memory.valid_at, opts.now) - 0.5))
                * (1.0 + 0.1 * (proof(memory.proof_count) - 0.5));
            Recalled { memory, score }
        })
        .collect();
    scored.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.memory.id.cmp(&b.memory.id)));

    // Pack in rank order: an oversized fact is skipped and packing continues;
    // the best admitted fact is always returned.
    let mut out = Recall {
        recency_only,
        mass_skipped,
        ..Recall::default()
    };
    let mut budget = Budget::new(opts.budget);
    for (i, item) in scored.into_iter().enumerate() {
        let line = fact_line(&item.memory);
        let cost = count(&line) + 1;
        if i == 0 {
            budget.force(cost);
        } else if !budget.take(cost) {
            out.omitted += 1;
            continue;
        }
        out.text.push_str(&line);
        out.text.push('\n');
        out.items.push(item);
    }
    out.tokens = count(&out.text);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linked(id: &str, links: usize, valid_at: i64) -> MemoryRec {
        MemoryRec {
            id: id.into(),
            valid_at,
            links: (0..links).map(|i| format!("n{i}")).collect(),
            ..MemoryRec::default()
        }
    }

    fn order(found: Vec<(MemoryRec, i64)>, wide: usize) -> Vec<String> {
        damped(found, wide, &mut HashSet::new())
            .into_iter()
            .map(|m| m.id)
            .collect()
    }

    #[test]
    fn damping_ranks_by_shared_over_root_of_links() {
        // 2 shared of 2 links (1.41) beats 1 shared of 40 (0.16), whatever the age.
        let found = vec![(linked("b", 40, 900), 1), (linked("a", 2, 100), 2)];
        assert_eq!(order(found, 10), ["a", "b"]);
        // Equal shared counts: the fact with fewer links wins.
        let found = vec![(linked("many", 9, 900), 2), (linked("few", 4, 100), 2)];
        assert_eq!(order(found, 10), ["few", "many"]);
        // Equal scores: newest valid_at, then id, whatever the input order.
        let tied = || {
            vec![
                (linked("c", 4, 100), 2),
                (linked("b", 4, 200), 2),
                (linked("a", 4, 200), 2),
            ]
        };
        let mut reversed = tied();
        reversed.reverse();
        assert_eq!(order(tied(), 10), ["a", "b", "c"]);
        assert_eq!(order(reversed, 10), ["a", "b", "c"]);
        assert_eq!(order(tied(), 2), ["a", "b"], "cut to wide");
        // A fact with no live links does not divide by zero.
        assert_eq!(order(vec![(linked("z", 0, 1), 1)], 5), ["z"]);
    }

    #[test]
    fn mass_facts_are_dropped_and_counted_once() {
        let mut mass = HashSet::new();
        let big = linked("big", MASS_LINKS + 1, 1);
        let edge = linked("edge", MASS_LINKS, 1);
        let kept = damped(vec![(big.clone(), 3), (edge.clone(), 1)], 10, &mut mass);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].id, "edge", "exactly MASS_LINKS links is not mass");
        let kept = without_mass(vec![big, edge], &mut mass);
        assert_eq!(kept.len(), 1);
        assert_eq!(mass.len(), 1, "the same fact in two arms counts once");
    }

    #[test]
    fn boosts_and_dates() {
        assert_eq!(recency(1000, 1000), 1.0);
        assert!((recency(0, 365 * 86_400) - 0.1).abs() < 1e-9);
        assert_eq!(recency(2000, 1000), 1.0, "a fact dated in the future is not penalised");
        assert_eq!(proof(1), 0.5);
        assert!(proof(20) > proof(2) && proof(1_000_000) <= 1.0);
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(1_794_441_600), "2026-11-12");
        assert_eq!(date(951_782_400), "2000-02-29");
    }

    #[test]
    fn fact_lines_are_compact() {
        let m = MemoryRec {
            kind: "decision".into(),
            text: "Use pnpm in web_gateway".into(),
            key: "web_gateway.package-manager".into(),
            valid_at: 1_794_441_600,
            proof_count: 3,
            ..MemoryRec::default()
        };
        assert_eq!(
            fact_line(&m),
            "- [decision] Use pnpm in web_gateway (web_gateway.package-manager; 2026-11-12; x3)"
        );
        let bare = MemoryRec {
            kind: "world".into(),
            text: "t".into(),
            proof_count: 1,
            ..MemoryRec::default()
        };
        assert_eq!(fact_line(&bare), "- [world] t (1970-01-01)");
    }
}
