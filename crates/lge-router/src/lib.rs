//! Reasoning router v0.
//!
//! Chooses the reasoning level for a task from the graph before the session
//! starts: one file = low, several files = medium, cross-service = top.
//! v0 uses search seeds plus reverse call traversal. v1 (phase 2) will use
//! the full change-impact result including API, database and queue edges.

use anyhow::Result;
use lge_core::Hit;
use lge_store::Store;
use serde::Serialize;
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Low,
    Medium,
    Top,
}

#[derive(Debug, Clone)]
pub struct RouterConfig {
    /// How many search hits are treated as the place the change starts.
    pub max_seeds: usize,
    /// Reverse traversal depth for callers.
    pub depth: u8,
    /// Seeds scoring below this share of the best seed are dropped.
    pub seed_score_ratio: f64,
}

impl Default for RouterConfig {
    fn default() -> Self {
        RouterConfig { max_seeds: 3, depth: 2, seed_score_ratio: 0.6 }
    }
}

#[derive(Debug, Serialize)]
pub struct Route {
    pub level: Level,
    pub reason: String,
    pub seeds: Vec<Hit>,
    /// Production files expected to be touched or affected.
    pub files: Vec<String>,
    pub services: Vec<String>,
    /// Tests that reach the seeds; run these locally after the change.
    pub tests: Vec<String>,
    pub unresolved_edges: i64,
}

pub fn route(store: &Store, task: &str, cfg: &RouterConfig) -> Result<Route> {
    let mut seeds = store.search(task, cfg.max_seeds * 3)?;
    seeds.retain(|h| !h.is_test);
    if let Some(best) = seeds.first().map(|h| h.score) {
        seeds.retain(|h| h.score >= best * cfg.seed_score_ratio);
    }
    seeds.truncate(cfg.max_seeds);

    let mut files = BTreeSet::new();
    let mut services = BTreeSet::new();
    let mut tests = BTreeSet::new();
    let mut unresolved = 0;
    for seed in &seeds {
        files.insert(seed.file_path.clone());
        services.insert(seed.service.clone());
        unresolved += store.unresolved_out(&seed.id)?;
        for c in store.callers(&seed.id, cfg.depth)? {
            if c.is_test {
                tests.insert(c.qualified_name);
            } else {
                files.insert(c.file_path);
                services.insert(c.service);
            }
        }
    }

    let (level, reason) = if seeds.is_empty() {
        (Level::Top, "no matching symbols in the graph; cannot bound the change".to_string())
    } else if services.len() > 1 {
        (Level::Top, format!("change reaches {} services", services.len()))
    } else if unresolved > 0 {
        // Safety rule: when the graph is unsure around the seeds, go one level up.
        let base = if files.len() > 1 { Level::Top } else { Level::Medium };
        (base, format!("{} file(s), raised one level because {unresolved} call(s) near the seeds are unresolved", files.len()))
    } else if files.len() > 1 {
        (Level::Medium, format!("{} files in one service", files.len()))
    } else {
        (Level::Low, "one file".to_string())
    };

    Ok(Route {
        level,
        reason,
        seeds,
        files: files.into_iter().collect(),
        services: services.into_iter().collect(),
        tests: tests.into_iter().collect(),
        unresolved_edges: unresolved,
    })
}
