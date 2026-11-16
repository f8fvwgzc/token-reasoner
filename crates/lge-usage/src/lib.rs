//! Usage log: reads Codex token usage per task and reports it.
//!
//! * [`parse_codex_jsonl`] turns a `codex exec --json` event stream or a
//!   `rollout-*.jsonl` session file into a [`ParsedSession`].
//! * [`new_record`] and [`apply_session`] build the `UsageRecord` of one attempt.
//! * [`report`] aggregates records per routed level and [`render_report`] prints the
//!   result. Cost appears only when [`load_prices`] found a `[prices]` table. Dry
//!   runs are counted apart: no session ran for them.
//!
//! This crate has no database access: callers persist records through `GraphStore`.
//!
//! # Token classes
//!
//! The four counts keep Codex's own accounting, checked against the rollout files
//! of codex-cli 0.160.0 (`total_tokens == input_tokens + output_tokens` on every
//! reading):
//!
//! * `cached_input_tokens` is the part of `input_tokens` that was read from the
//!   prompt cache, not an addition to it.
//! * `reasoning_tokens` is the part of `output_tokens` spent on reasoning, not an
//!   addition to it. That is why there is no separate reasoning price: reasoning
//!   is billed as output.
//!
//! So the total of a session is input plus output, and adding the cached or
//! reasoning count on top would count those tokens twice.
//!
//! # One attempt
//!
//! ```no_run
//! use lge_usage::{apply_session, codex_home, find_rollout, new_record, parse_codex_jsonl, parse_rollout_file, task_id};
//!
//! # fn attempt(stdout: &str, started_at: i64, finished_at: i64) -> lge_core::UsageRecord {
//! let task = "rename the helper";
//! let id = task_id(task, started_at);
//! let mut rec = new_record(task, &id, 1, lge_core::Level::Low, "one file", "codex", started_at);
//!
//! // `stdout` is what `codex exec --json` printed. The stream gives the session
//! // id, the usage and the last message; the rollout file adds model and effort.
//! let mut session = parse_codex_jsonl(stdout);
//! if let Some(path) = find_rollout(&codex_home(), &session.session_id) {
//!     if let Ok(rollout) = parse_rollout_file(&path) {
//!         session = session.merged_with(&rollout);
//!     }
//! }
//! apply_session(&mut rec, &session, finished_at, lge_core::outcome::SUCCESS);
//! # rec
//! # }
//! ```

mod codex;
mod prices;
mod record;
mod report;

pub use codex::{
    codex_home, find_rollout, parse_codex_jsonl, parse_rollout_file, rollout_cwd, rollout_files, rollout_items,
    rollout_requests, ParsedSession, RolloutItem, TokenUsage,
};
pub use prices::{load_prices, Prices, CONFIG_FILE};
pub use record::{apply_session, new_record, task_id};
pub use report::{
    bootstrap_interval, bootstrap_median_diff, compare_arms, render_report, report, ArmComparison, ArmStats,
    LevelStats, MetricDiff, Report, ARM_METRICS, HOLDOUT_ARM, MIN_ARM_SESSIONS, PROXY_ARM,
};
