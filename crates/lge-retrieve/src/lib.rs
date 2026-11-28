//! Index pipeline, traversal, ranking and evidence packs.

pub mod algo;
pub mod budget;
mod pack;
mod pipeline;
mod render;
mod report;
mod seeds;
mod traverse;

pub use pack::{commit_line, evidence_pack, EvidencePack, PackOptions, SHOWN_EARLIER};
pub use pipeline::{index_repo, refresh_if_stale, resolve_edges, unix_now, Freshness, IndexOptions, IndexReport};
pub use render::{render_deps, render_impact, render_trace};
pub use report::{
    architecture, compute_metrics, overview, refresh_graph_views, repo_map, Community, ARCHITECTURE_SLUG,
    GRAPH_PAGE_KIND, OVERVIEW_SLUG,
};
pub use seeds::{find_seeds, name_candidates, Seed, SeedOptions};
pub use traverse::{
    dependencies, dependencies_of, dependents_of, impact, resolve_targets, trace, Deps, Impact, Reached, Trace, Walk,
    WalkOptions,
};
