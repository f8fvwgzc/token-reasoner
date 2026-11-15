//! Behaviour of `lge_resolve::resolve` on hand-built inputs: one or more tests
//! per rule and per provenance outcome, for Python, for TypeScript/JavaScript, and
//! for the languages the indexer reads through a grammar and a tags query.
//!
//! Every test resolves through `support::run`, which also checks the invariants
//! (confidence bands, justified static edges, sorted output, same output for
//! shuffled input) on that test's input.

mod derived;
mod facts;
mod invariants;
mod other;
mod python;
mod random;
mod review;
mod scenarios;
mod script;
mod support;
