//! Index pipeline, traversal, ranking and evidence packs.

mod pipeline;

pub use pipeline::{index_repo, resolve_edges, unix_now, IndexOptions, IndexReport};
