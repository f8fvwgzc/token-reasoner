//! Runs lint, type checks and tests locally and reduces the output to failures and relevant diff hunks.
//!
//! The model that wrote a change does not need the whole log of `pytest` or `tsc`: it
//! needs to know which checks failed, where, and what it changed nearby. This crate
//!
//! 1. finds the checks to run ([`load_config`], or [`detect_checks`] when there is no `lge.toml`),
//! 2. runs them ([`run_check`], [`run_all`]),
//! 3. reduces each output to its failures ([`filter_output`]): file, line, rule or test
//!    name, message and a short excerpt,
//! 4. adds the `git diff` hunks of the files those failures name ([`relevant_diff_hunks`]),
//! 5. and renders one compact text for the model ([`render`]).
//!
//! The one promise every step keeps: a failing test or error the tool reported is never
//! dropped. When output has to shrink, excerpts shrink first, then messages; names stay.
//! When a parser does not understand the output of a failed check, the generic filter
//! is used instead, so the model still sees why it failed.
//!
//! ```no_run
//! use lge_filter::{detect_checks, load_config, render, run_all};
//!
//! let root = std::path::Path::new(".");
//! let specs = match load_config(root)? {
//!     Some(specs) => specs,
//!     None => detect_checks(root),
//! };
//! let report = run_all(root, &specs)?;
//! print!("{}", render(&report));
//! # Ok::<(), anyhow::Error>(())
//! ```
//!
//! The crate is standalone: no database, no other `lge` crate. It needs `sh` and, for
//! the diff hunks, `git`; process groups and `/dev/null` make it Unix-only in practice.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

mod ansi;
mod config;
mod detect;
mod diff;
mod format;
mod parse;
mod render;
mod run;

pub use config::load_config;
pub use detect::detect_checks;
pub use diff::relevant_diff_hunks;
pub use parse::{filter_output, filter_with, tool_of, FilterOptions};
pub use render::render;
pub use run::{run_all, run_check};

/// Cap on the filtered text of one check. Excerpts and then messages are shortened to
/// fit; failure names are never dropped, so a run with thousands of failures can exceed it.
pub const MAX_FILTERED_BYTES: usize = 12_000;

/// Cap on the diff hunks [`run_all`] attaches to a report.
pub const MAX_DIFF_BYTES: usize = 8_000;

/// Cap on the output kept from one check. Past it, the first and last halves are kept:
/// the end of a log holds the summary, the start holds the first error.
pub const MAX_OUTPUT_BYTES: usize = 20 * 1024 * 1024;

/// Name of the configuration file read by [`load_config`].
pub const CONFIG_FILE: &str = "lge.toml";

/// What a check verifies. Decides the default timeout and the label in the report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckKind {
    Lint,
    #[serde(alias = "type", alias = "typecheck")]
    Types,
    #[serde(alias = "test")]
    Tests,
}

impl CheckKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CheckKind::Lint => "lint",
            CheckKind::Types => "types",
            CheckKind::Tests => "tests",
        }
    }

    /// Timeout used when a check does not set one. Test suites get longer than linters.
    pub fn default_timeout_secs(self) -> u64 {
        match self {
            CheckKind::Lint | CheckKind::Types => 300,
            CheckKind::Tests => 900,
        }
    }
}

/// One command to run. `command` goes through `sh -c` in `cwd`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckSpec {
    pub name: String,
    pub kind: CheckKind,
    pub command: String,
    pub cwd: PathBuf,
    pub timeout_secs: u64,
}

/// One failing test or one error.
///
/// * `file` is the path the tool reported (made relative to the repository root by
///   [`run_all`]); empty when the tool named none.
/// * `name` is what the model needs to find or rerun it: the test id
///   (`tests/test_cart.py::test_total[1-2]`, `cart::tests::totals_items`,
///   `Cart › totals items`) or the rule or error code (`F401`, `TS2322`, `E0308`,
///   `clippy::needless_return`, `no-unused-vars`).
/// * `message` is one line. `excerpt` is the few lines of traceback or source the tool
///   printed for it, possibly shortened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    pub file: String,
    pub line: Option<u32>,
    pub name: String,
    pub message: String,
    pub excerpt: String,
}

/// The outcome of one check. `passed` is the exit code's verdict (zero and not timed
/// out), never the parser's. `raw_bytes` and `raw_lines` measure everything the command
/// printed; `filtered` is what is left of it for the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckResult {
    pub name: String,
    pub kind: CheckKind,
    pub command: String,
    pub exit_code: Option<i32>,
    pub passed: bool,
    pub timed_out: bool,
    pub duration_ms: u64,
    pub raw_bytes: usize,
    pub raw_lines: usize,
    pub failures: Vec<Failure>,
    pub filtered: String,
}

/// All checks of one run, in the order they ran. `passed` is true when no check failed,
/// which includes the case of no checks at all. `filtered_bytes` is the size of the
/// text [`render`] returns for this report, so `raw_bytes - filtered_bytes` is what was
/// kept away from the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub results: Vec<CheckResult>,
    pub passed: bool,
    pub diff_hunks: String,
    pub raw_bytes: usize,
    pub filtered_bytes: usize,
}
