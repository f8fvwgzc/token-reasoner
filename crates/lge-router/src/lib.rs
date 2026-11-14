//! Reasoning router v1.
//!
//! Chooses the reasoning level for a task from the graph before the session
//! starts, from the full change impact of the task's seed symbols: one file =
//! low, several files in one service = medium, more than one service = top.
//!
//! The router only recommends. Whether the recommendation is applied is the
//! session's decision: in fixed mode (the default) the session keeps the user's
//! reasoning level and logs what the router would have chosen.

use anyhow::Result;
use lge_core::Hit;
pub use lge_core::Level;
use lge_retrieve::{find_seeds, impact, SeedOptions, WalkOptions};
use lge_store::GraphStore;
use serde::Serialize;

#[derive(Debug, Clone)]
pub struct RouterConfig {
    /// How many symbols are treated as the place the change starts.
    pub max_seeds: usize,
    /// How far the change impact is followed.
    pub depth: u8,
    /// Search hits scoring below this share of the best hit are not seeds.
    pub seed_score_ratio: f64,
}

impl Default for RouterConfig {
    fn default() -> Self {
        RouterConfig {
            max_seeds: 3,
            depth: 2,
            seed_score_ratio: 0.6,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Route {
    pub level: Level,
    pub reason: String,
    pub seeds: Vec<Hit>,
    /// Production files expected to be touched or affected.
    pub files: Vec<String>,
    pub services: Vec<String>,
    /// Tests that reach the seeds; run these locally after the change.
    pub tests: Vec<String>,
    /// Endpoints, tables and queues the change reaches.
    pub resources: Vec<String>,
    /// Places around the seeds where the graph has several candidates and no target.
    pub unresolved_edges: i64,
    /// Affected symbols that are only reached through a name match.
    pub inferred_paths: usize,
}

fn one_up(level: Level) -> Level {
    level.escalate().unwrap_or(Level::Top)
}

pub fn route(store: &dyn GraphStore, task: &str, cfg: &RouterConfig) -> Result<Route> {
    let seed_opts = SeedOptions {
        max_seeds: cfg.max_seeds,
        score_ratio: cfg.seed_score_ratio,
        ..SeedOptions::default()
    };
    let seeds: Vec<Hit> = find_seeds(store, task, &seed_opts)?
        .into_iter()
        .map(|s| s.hit)
        .filter(|h| !h.is_test)
        .collect();
    if seeds.is_empty() {
        return Ok(Route {
            level: Level::Top,
            reason: "no matching symbols in the graph; cannot bound the change".to_string(),
            seeds,
            files: Vec::new(),
            services: Vec::new(),
            tests: Vec::new(),
            resources: Vec::new(),
            unresolved_edges: 0,
            inferred_paths: 0,
        });
    }
    let ids: Vec<String> = seeds.iter().map(|h| h.id.clone()).collect();
    let found = impact(
        store,
        &ids,
        &WalkOptions {
            depth: cfg.depth,
            ..WalkOptions::default()
        },
    )?;

    let (files, services) = (found.files.len(), found.services.len());
    let (level, reason) = if services > 1 {
        (Level::Top, format!("change reaches {services} services"))
    } else {
        let (base, what) = if files > 1 {
            (Level::Medium, format!("{files} files in one service"))
        } else {
            (Level::Low, "one file".to_string())
        };
        // Safety rule: when the graph is unsure around the seeds, go one level up.
        if found.unresolved_edges > 0 {
            (
                one_up(base),
                format!(
                    "{what}, raised one level because {} edge(s) near the seeds are unresolved",
                    found.unresolved_edges
                ),
            )
        } else if found.inferred_paths > 0 {
            (
                one_up(base),
                format!(
                    "{what}, raised one level because {} affected symbol(s) are only inferred",
                    found.inferred_paths
                ),
            )
        } else if found.truncated {
            (
                one_up(base),
                format!("{what}, raised one level because the impact was cut at the node limit"),
            )
        } else {
            (base, what)
        }
    };

    let mut resources: Vec<String> = found
        .endpoints
        .iter()
        .map(|r| format!("endpoint {}", r.node.name))
        .collect();
    resources.extend(found.tables.iter().map(|r| format!("table {}", r.node.name)));
    resources.extend(found.queues.iter().map(|r| format!("queue {}", r.node.name)));
    Ok(Route {
        level,
        reason,
        seeds,
        files: found.files,
        services: found.services,
        tests: found.tests.into_iter().map(|r| r.node.qualified_name).collect(),
        resources,
        unresolved_edges: found.unresolved_edges,
        inferred_paths: found.inferred_paths,
    })
}
