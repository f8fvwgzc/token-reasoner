//! Benchmark runner: does the evidence pack hold the files a task needed, how
//! many tokens does it cost against reading those files, and does the router
//! agree with a hand classification.
//!
//! The token baseline here is deliberately modest: the files the finished task
//! touched, read whole. It is what an agent pays at the very least without the
//! graph, before any searching. What a real session spends is measured by the
//! usage log, not here.

pub mod ab;

use anyhow::{Context, Result};
use lge_core::Level;
use lge_retrieve::budget::count;
use lge_retrieve::{evidence_pack, PackOptions};
use lge_router::{route, RouterConfig};
use lge_store::GraphStore;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// One `[[task]]` of a benchmark file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchTask {
    pub name: String,
    /// The task text as it would be given to the agent.
    pub task: String,
    /// Files the finished task touched or had to read, relative to the root.
    #[serde(default)]
    pub files: Vec<String>,
    /// Hand classification: `low`, `medium` or `top`.
    #[serde(default)]
    pub level: Option<String>,
}

#[derive(Deserialize)]
struct BenchFile {
    #[serde(default)]
    task: Vec<BenchTask>,
}

pub fn load_tasks(path: &Path) -> Result<Vec<BenchTask>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let file: BenchFile =
        toml::from_str(&text).with_context(|| format!("{} is not a valid benchmark file", path.display()))?;
    Ok(file.task)
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskResult {
    pub name: String,
    pub pack_tokens: usize,
    /// Tokens of the expected files read whole.
    pub files_tokens: usize,
    pub expected: usize,
    /// Expected files the pack names.
    pub found: usize,
    pub missing: Vec<String>,
    pub level_predicted: String,
    pub level_expected: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct BenchReport {
    pub budget: usize,
    pub results: Vec<TaskResult>,
    /// Share of expected files the packs name, over all tasks.
    pub recall: f64,
    /// Tasks whose pack names at least 80% of their files (the phase 3 exit bar).
    pub tasks_at_80: usize,
    /// Share of classified tasks where the router chose the hand-assigned level.
    pub router_agreement: Option<f64>,
    pub pack_tokens: usize,
    pub files_tokens: usize,
}

/// Run every task against the indexed graph. `root` is where the expected
/// files are read from, for the token baseline.
pub fn run(store: &dyn GraphStore, root: &Path, tasks: &[BenchTask], budget: usize) -> Result<BenchReport> {
    let mut report = BenchReport {
        budget,
        ..BenchReport::default()
    };
    let (mut expected, mut found, mut classified, mut agreed) = (0usize, 0usize, 0usize, 0usize);
    for t in tasks {
        let pack = evidence_pack(
            store,
            &t.task,
            &PackOptions {
                budget,
                ..PackOptions::default()
            },
        )?;
        let routed = route(store, &t.task, &RouterConfig::default())?;
        let missing: Vec<String> = t
            .files
            .iter()
            .filter(|f| !pack.text.contains(f.as_str()))
            .cloned()
            .collect();
        let files_tokens: usize = t
            .files
            .iter()
            .map(|f| std::fs::read_to_string(root.join(f)).map(|s| count(&s)).unwrap_or(0))
            .sum();
        let hit = t.files.len() - missing.len();
        expected += t.files.len();
        found += hit;
        if !t.files.is_empty() && hit * 5 >= t.files.len() * 4 {
            report.tasks_at_80 += 1;
        }
        if let Some(want) = t.level.as_deref().and_then(Level::parse) {
            classified += 1;
            if want == routed.level {
                agreed += 1;
            }
        }
        report.pack_tokens += pack.tokens;
        report.files_tokens += files_tokens;
        report.results.push(TaskResult {
            name: t.name.clone(),
            pack_tokens: pack.tokens,
            files_tokens,
            expected: t.files.len(),
            found: hit,
            missing,
            level_predicted: routed.level.as_str().to_string(),
            level_expected: t.level.clone(),
        });
    }
    report.recall = if expected == 0 {
        0.0
    } else {
        found as f64 / expected as f64
    };
    report.router_agreement = (classified > 0).then(|| agreed as f64 / classified as f64);
    Ok(report)
}

/// A fixed-width table for the terminal.
pub fn render(r: &BenchReport) -> String {
    let width = r
        .results
        .iter()
        .map(|t| t.name.chars().count())
        .max()
        .unwrap_or(4)
        .clamp(4, 40);
    let mut out = format!(
        "{:width$}  {:>6}  {:>6}  {:>7}  {:>7}  {:>7}\n",
        "task", "pack", "files", "found", "level", "expect"
    );
    for t in &r.results {
        let name: String = t.name.chars().take(width).collect();
        out.push_str(&format!(
            "{name:width$}  {:>6}  {:>6}  {:>7}  {:>7}  {:>7}\n",
            t.pack_tokens,
            t.files_tokens,
            format!("{}/{}", t.found, t.expected),
            t.level_predicted,
            t.level_expected.as_deref().unwrap_or("-")
        ));
        for m in &t.missing {
            out.push_str(&format!("{:width$}    missing {m}\n", ""));
        }
    }
    out.push_str(&format!(
        "\nfiles named by the packs: {:.0}% ({} of {} tasks at 80% or more), budget {} tokens\n",
        r.recall * 100.0,
        r.tasks_at_80,
        r.results.len(),
        r.budget
    ));
    out.push_str(&format!(
        "pack tokens {} against {} for reading the needed files whole\n",
        r.pack_tokens, r.files_tokens
    ));
    if let Some(a) = r.router_agreement {
        out.push_str(&format!(
            "router agrees with the hand classification on {:.0}% of classified tasks\n",
            a * 100.0
        ));
    }
    out
}
