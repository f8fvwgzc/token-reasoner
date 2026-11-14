//! Long-term memory: facts the agent keeps between sessions, recalled under a
//! token budget, and markdown pages built from them.
//!
//! Modelled on hindsight (retain, recall, pages) and graphiti (supersede,
//! never delete), reimplemented on the graph store without a model in the
//! loop. See `docs/PRIOR_ART.md`.

pub mod dedup;
mod pages;
mod recall;
mod retain;

pub use pages::{export_pages, refresh_pages, search_pages, snippet, to_markdown, PageHit, MEMORY_PAGE_KIND, SEEDED, SNIPPET_CHARS};
pub use recall::{date, fact_line, recall, Embedder, Recall, RecallOptions, Recalled};
pub use retain::{entity_links, retain, Outcome, RetainInput, Retained, MAX_FACT_CHARS};
