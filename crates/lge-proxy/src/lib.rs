//! The middle layer: a local proxy between a coding agent and its model
//! provider that uses the code graph to send fewer tokens for the same work.

pub mod command;
pub mod responses;
pub mod server;
pub mod session;
pub mod shorten;
