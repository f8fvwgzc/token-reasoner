//! The middle layer: a local proxy between a coding agent and its model
//! provider that uses the code graph to send fewer tokens for the same work.

pub mod command;
pub mod config_hint;
pub mod delta;
pub mod discover;
pub mod fold;
pub mod graphs;
pub mod guardian;
pub mod lockfile;
pub mod noise;
pub mod responses;
pub mod rules;
pub mod server;
pub mod session;
pub mod shorten;
