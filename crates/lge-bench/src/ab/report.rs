//! What an A/B run found: per task and arm, then the change in total tokens
//! over the tasks both arms got right, and the verdict against the target.
//! Beside it, for reading the result rather than judging it: billed-equivalent
//! tokens with their own (secondary) verdict, requests, output and reasoning
//! per arm, a bootstrap interval clustered by task, the geometric mean of the
//! per-task ratios, which tasks varied too much between runs, and the context
//! per request from the daemon's traces.

use super::{Arm, ArmRun, Variant};
use anyhow::{bail, Result};
use lge_usage::TokenUsage;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The target: the median total tokens with lge's features on at least 25%
/// lower than with them off, with no drop in the success rate.
pub const TARGET_CHANGE: f64 = -0.25;

/// A task's arm is marked noisy when the total tokens of its passing runs
/// differ by more than this factor (largest over smallest): its median is
/// then a weak estimate and more runs are suggested.
pub const NOISE_SPREAD: f64 = 1.5;

/// Resamples of the clustered bootstrap interval.
pub const BOOTSTRAP_RESAMPLES: usize = 2_000;
/// Its seed: the same results always give the same interval.
pub const BOOTSTRAP_SEED: u64 = 0x5eed_ab01;

/// Weights of billed-equivalent tokens: what a token of each kind costs
/// against an uncached input token.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Weights {
    /// Uncached input.
    pub input: f64,
    /// Cached input.
    pub cached: f64,
    /// Output, reasoning included.
    pub output: f64,
}

/// The default weights: 1, 0.1 and 8, the ratios of the list prices of
/// current OpenAI models (cached input a tenth of input, output eight times).
pub const DEFAULT_WEIGHTS: Weights = Weights {
    input: 1.0,
    cached: 0.1,
    output: 8.0,
};

impl Default for Weights {
    fn default() -> Weights {
        DEFAULT_WEIGHTS
    }
}

impl Weights {
    /// `IN,CACHED,OUT`, three numbers of zero or more, at least one above zero.
    pub fn parse(text: &str) -> Result<Weights> {
        let parts: Vec<&str> = text.split(',').map(str::trim).collect();
        let numbers: Vec<f64> = parts.iter().filter_map(|p| p.parse::<f64>().ok()).collect();
        if parts.len() != 3 || numbers.len() != 3 || numbers.iter().any(|w| !w.is_finite() || *w < 0.0) {
            bail!("--weights takes three numbers of zero or more, IN,CACHED,OUT (such as 1,0.1,8); got `{text}`");
        }
        if numbers.iter().all(|w| *w == 0.0) {
            bail!("--weights: at least one weight must be above zero");
        }
        Ok(Weights {
            input: numbers[0],
            cached: numbers[1],
            output: numbers[2],
        })
    }

    /// Billed-equivalent tokens of a session: uncached input x `input` +
    /// cached input x `cached` + output (reasoning included) x `output`.
    pub fn billed(&self, u: &TokenUsage) -> f64 {
        let cached = u.cached_input_tokens.clamp(0, u.input_tokens.max(0));
        (u.input_tokens.max(0) - cached) as f64 * self.input
            + cached as f64 * self.cached
            + u.output_tokens.max(0) as f64 * self.output
    }
}

impl std::fmt::Display for Weights {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "uncached input x{}, cached input x{}, output x{}",
            self.input, self.cached, self.output
        )
    }
}

/// Runs and successes of one arm.
#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq)]
pub struct Tally {
    pub runs: usize,
    pub successes: usize,
    /// `successes / runs`; `None` without runs.
    pub rate: Option<f64>,
}

impl Tally {
    fn of<'a>(runs: impl Iterator<Item = &'a ArmRun>) -> Tally {
        let (mut n, mut ok) = (0, 0);
        for r in runs {
            n += 1;
            ok += usize::from(r.success);
        }
        Tally {
            runs: n,
            successes: ok,
            rate: (n > 0).then(|| ok as f64 / n as f64),
        }
    }
}

/// One arm of one task, over its runs in pairs where both arms ran. The
/// medians are over the successful runs whose usage was read; `None` without one.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct ArmSummary {
    pub runs: usize,
    pub successes: usize,
    /// Median total tokens (input plus output).
    pub median_total: Option<f64>,
    /// Median billed-equivalent tokens.
    pub median_billed: Option<f64>,
    /// Median model requests, as the daemon counted them.
    pub median_requests: Option<f64>,
    /// Median output tokens, reasoning included.
    pub median_output: Option<f64>,
    /// Median reasoning tokens: a part of the output, shown on its own.
    pub median_reasoning: Option<f64>,
    /// Largest over smallest total tokens of those runs; `None` with fewer than two.
    pub spread: Option<f64>,
    /// `spread` is above [`NOISE_SPREAD`].
    pub noisy: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TaskSummary {
    pub name: String,
    pub off: ArmSummary,
    pub on: ArmSummary,
    /// `(on - off) / off` of the median totals; `None` unless both arms
    /// succeeded at least once.
    pub change: Option<f64>,
    /// The same of the median billed-equivalent tokens.
    pub change_billed: Option<f64>,
}

/// One arm over every task compared: medians over its successful runs in
/// those tasks.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct ArmTotals {
    /// Runs the medians are taken over.
    pub sessions: usize,
    pub median_requests: Option<f64>,
    pub median_output: Option<f64>,
    pub median_reasoning: Option<f64>,
    pub median_total: Option<f64>,
    pub median_billed: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct PerArm {
    pub off: ArmTotals,
    pub on: ArmTotals,
}

/// A 95% bootstrap interval of the median change in total tokens, clustered
/// by task: each resample draws the compared tasks with replacement, then
/// each drawn task's runs of each arm with replacement.
#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
pub struct Interval {
    pub low: f64,
    pub high: f64,
    pub resamples: usize,
    pub seed: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Verdict {
    /// `None` when no task succeeded in both arms.
    pub met: Option<bool>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AbReport {
    /// What the two arms differ in.
    pub variant: Variant,
    /// Every session, in the order it ran (sessions of an earlier run first
    /// when it was resumed).
    pub runs: Vec<ArmRun>,
    pub tasks: Vec<TaskSummary>,
    /// Tasks where both arms succeeded (with their usage read): the changes
    /// below are taken over these.
    pub compared: usize,
    pub median_change: Option<f64>,
    pub mean_change: Option<f64>,
    /// See [`Interval`]; `None` unless two or more tasks were compared over
    /// two or more runs.
    pub interval: Option<Interval>,
    /// `exp(mean(ln(on / off)))` of the compared tasks' median totals.
    pub geo_mean_ratio: Option<f64>,
    /// Over the pairs where both arms ran.
    pub success_off: Tally,
    pub success_on: Tally,
    pub target_change: f64,
    /// The verdict, on total tokens as docs/AB.md defines it.
    pub verdict: Verdict,
    /// The weights of billed-equivalent tokens.
    pub weights: Weights,
    /// Median and mean change in billed-equivalent tokens.
    pub median_change_billed: Option<f64>,
    pub mean_change_billed: Option<f64>,
    /// The same rule on billed-equivalent tokens: a second reading, never
    /// the verdict.
    pub verdict_billed: Verdict,
    pub per_arm: PerArm,
    /// Tasks' arms whose passing runs varied more than [`NOISE_SPREAD`].
    pub noisy: usize,
    /// Sessions whose requests never reached the daemon: the arm's settings
    /// did not apply to them.
    pub not_proxied: usize,
    /// Sessions that ended in an error before their check ran (the copy, the
    /// setup, the index or the agent could not run): their pairs are listed,
    /// never compared, and do not count for success.
    pub errored: usize,
    /// Successful sessions whose event stream reported no token usage (no
    /// `turn.completed`): they count for success but are left out of the
    /// medians, where they would read as a session that cost nothing.
    pub unmeasured: usize,
    /// Stopped early (Ctrl-C): the numbers cover the pairs that finished.
    pub interrupted: bool,
    /// False when the daemon's original settings could not be put back.
    pub settings_restored: bool,
    /// The full id of the commit every copy was made from; "" when unknown.
    pub base: String,
    /// The sessions ran with `--approve-for-me` in place of `-s workspace-write`.
    pub approve_for_me: bool,
    /// The note appended to every prompt; `None` when there was none.
    pub note: Option<String>,
    /// Where the results file and the traces are; "" when unknown.
    pub work_dir: String,
    /// Sessions read back from the results file of an earlier run.
    pub resumed: usize,
}

/// The median; the mean of the two middle values for an even count.
pub fn median(values: &[f64]) -> Option<f64> {
    let mut v: Vec<f64> = values.iter().copied().filter(|x| x.is_finite()).collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    let mid = v.len() / 2;
    Some(if v.len() % 2 == 1 {
        v[mid]
    } else {
        (v[mid - 1] + v[mid]) / 2.0
    })
}

fn mean(values: &[f64]) -> Option<f64> {
    (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
}

/// True when the session ran and its check gave an answer: not cut short by
/// Ctrl-C, and no error before the check (no copy, a failed setup, no index,
/// an agent that could not start).
fn ran(r: &ArmRun) -> bool {
    !r.interrupted && r.check_passed.is_some()
}

/// A session that counts in the medians: it succeeded and its usage was read.
fn measured(r: &ArmRun) -> bool {
    r.success && r.total_tokens > 0
}

/// `(on - off) / off`.
fn change_of(off: Option<f64>, on: Option<f64>) -> Option<f64> {
    match (off, on) {
        (Some(o), Some(n)) if o > 0.0 => Some((n - o) / o),
        _ => None,
    }
}

/// The report for `runs` with the default weights; see [`summarize_with`].
pub fn summarize(names: &[String], runs: Vec<ArmRun>) -> AbReport {
    summarize_with(names, runs, DEFAULT_WEIGHTS)
}

/// The report for `runs`, with tasks in the order of `names`. Only pairs where
/// both arms ran (their checks gave an answer, nothing cut them short) count:
/// the others are listed, never compared.
pub fn summarize_with(names: &[String], runs: Vec<ArmRun>, weights: Weights) -> AbReport {
    let mut arms_of_pair: BTreeMap<(&str, usize), [bool; 2]> = BTreeMap::new();
    for r in runs.iter().filter(|r| ran(r)) {
        arms_of_pair.entry((r.task.as_str(), r.run)).or_default()[usize::from(r.arm == Arm::On)] = true;
    }
    let complete: Vec<&ArmRun> = runs
        .iter()
        .filter(|r| ran(r) && arms_of_pair.get(&(r.task.as_str(), r.run)) == Some(&[true, true]))
        .collect();
    let counted = |name: &str, arm: Arm| -> Vec<&ArmRun> {
        complete
            .iter()
            .copied()
            .filter(|r| r.task == name && r.arm == arm && measured(r))
            .collect()
    };
    let summary = |name: &str, arm: Arm| {
        let mine: Vec<&&ArmRun> = complete.iter().filter(|r| r.task == name && r.arm == arm).collect();
        let good = counted(name, arm);
        let totals: Vec<f64> = good.iter().map(|r| r.total_tokens as f64).collect();
        let spread = (totals.len() >= 2).then(|| {
            let max = totals.iter().copied().fold(f64::MIN, f64::max);
            let min = totals.iter().copied().fold(f64::MAX, f64::min);
            max / min
        });
        ArmSummary {
            runs: mine.len(),
            successes: mine.iter().filter(|r| r.success).count(),
            median_total: median(&totals),
            median_billed: median(&good.iter().map(|r| weights.billed(&r.usage)).collect::<Vec<_>>()),
            median_requests: median(
                &good
                    .iter()
                    .filter_map(|r| r.requests)
                    .map(|n| n as f64)
                    .collect::<Vec<_>>(),
            ),
            median_output: median(&good.iter().map(|r| r.usage.output_tokens as f64).collect::<Vec<_>>()),
            median_reasoning: median(&good.iter().map(|r| r.usage.reasoning_tokens as f64).collect::<Vec<_>>()),
            spread,
            noisy: spread.is_some_and(|s| s > NOISE_SPREAD),
        }
    };
    let tasks: Vec<TaskSummary> = names
        .iter()
        .map(|name| {
            let (off, on) = (summary(name, Arm::Off), summary(name, Arm::On));
            TaskSummary {
                name: name.clone(),
                change: change_of(off.median_total, on.median_total),
                change_billed: change_of(off.median_billed, on.median_billed),
                off,
                on,
            }
        })
        .collect();
    let changes: Vec<f64> = tasks.iter().filter_map(|t| t.change).collect();
    let changes_billed: Vec<f64> = tasks.iter().filter_map(|t| t.change_billed).collect();
    let success_off = Tally::of(complete.iter().copied().filter(|r| r.arm == Arm::Off));
    let success_on = Tally::of(complete.iter().copied().filter(|r| r.arm == Arm::On));
    let median_change = median(&changes);
    let median_change_billed = median(&changes_billed);

    // The compared tasks: their measured runs per arm.
    let compared_runs: Vec<(Vec<&ArmRun>, Vec<&ArmRun>)> = tasks
        .iter()
        .filter(|t| t.change.is_some())
        .map(|t| (counted(&t.name, Arm::Off), counted(&t.name, Arm::On)))
        .collect();
    let totals_of = |runs: &[&ArmRun]| runs.iter().map(|r| r.total_tokens as f64).collect::<Vec<_>>();
    let clusters: Vec<(Vec<f64>, Vec<f64>)> = compared_runs
        .iter()
        .map(|(off, on)| (totals_of(off), totals_of(on)))
        .collect();
    let several_runs = complete.iter().map(|r| r.run).max().unwrap_or(0) >= 2;
    let interval = if clusters.len() >= 2 && several_runs {
        clustered_interval(&clusters, BOOTSTRAP_RESAMPLES, BOOTSTRAP_SEED).map(|(low, high)| Interval {
            low,
            high,
            resamples: BOOTSTRAP_RESAMPLES,
            seed: BOOTSTRAP_SEED,
        })
    } else {
        None
    };
    let logs: Vec<f64> = tasks
        .iter()
        .filter_map(|t| match (t.off.median_total, t.on.median_total) {
            (Some(o), Some(n)) if o > 0.0 && n > 0.0 => Some((n / o).ln()),
            _ => None,
        })
        .collect();
    let arm_totals = |arm: Arm| {
        let good: Vec<&ArmRun> = compared_runs
            .iter()
            .flat_map(|(off, on)| if arm == Arm::Off { off.iter() } else { on.iter() })
            .copied()
            .collect();
        let of = |f: &dyn Fn(&ArmRun) -> Option<f64>| median(&good.iter().filter_map(|r| f(r)).collect::<Vec<_>>());
        ArmTotals {
            sessions: good.len(),
            median_requests: of(&|r| r.requests.map(|n| n as f64)),
            median_output: of(&|r| Some(r.usage.output_tokens as f64)),
            median_reasoning: of(&|r| Some(r.usage.reasoning_tokens as f64)),
            median_total: of(&|r| Some(r.total_tokens as f64)),
            median_billed: of(&|r| Some(weights.billed(&r.usage))),
        }
    };
    let per_arm = PerArm {
        off: arm_totals(Arm::Off),
        on: arm_totals(Arm::On),
    };
    AbReport {
        variant: Variant::Features,
        not_proxied: runs.iter().filter(|r| r.proxied == Some(false)).count(),
        errored: runs
            .iter()
            .filter(|r| !r.interrupted && r.check_passed.is_none())
            .count(),
        unmeasured: complete.iter().filter(|r| r.success && r.total_tokens <= 0).count(),
        noisy: tasks
            .iter()
            .map(|t| usize::from(t.off.noisy) + usize::from(t.on.noisy))
            .sum(),
        runs,
        compared: changes.len(),
        mean_change: mean(&changes),
        verdict: verdict(median_change, &success_off, &success_on),
        verdict_billed: verdict_on(BILLED, median_change_billed, &success_off, &success_on),
        median_change,
        interval,
        geo_mean_ratio: mean(&logs).map(f64::exp),
        weights,
        median_change_billed,
        mean_change_billed: mean(&changes_billed),
        per_arm,
        tasks,
        success_off,
        success_on,
        target_change: TARGET_CHANGE,
        interrupted: false,
        settings_restored: true,
        base: String::new(),
        approve_for_me: false,
        note: None,
        work_dir: String::new(),
        resumed: 0,
    }
}

/// The 95% interval of the median over tasks of `(median(on) - median(off)) /
/// median(off)`, clustered by task: each of `resamples` rounds draws the
/// tasks with replacement, then, within each drawn task, its runs of each
/// arm with replacement. `clusters` holds each task's (off, on) totals;
/// built on [`lge_usage::bootstrap_interval`], so a seed gives one answer.
pub fn clustered_interval(clusters: &[(Vec<f64>, Vec<f64>)], resamples: usize, seed: u64) -> Option<(f64, f64)> {
    let usable: Vec<&(Vec<f64>, Vec<f64>)> = clusters
        .iter()
        .filter(|(off, on)| !off.is_empty() && !on.is_empty())
        .collect();
    if usable.is_empty() {
        return None;
    }
    lge_usage::bootstrap_interval(resamples, seed, |below| {
        let mut changes = Vec::with_capacity(usable.len());
        for _ in 0..usable.len() {
            let (off, on) = usable[below(usable.len())];
            let o = median_of_draw(off, below);
            let n = median_of_draw(on, below);
            if let Some(c) = change_of(o, n) {
                changes.push(c);
            }
        }
        median(&changes)
    })
}

/// The median of `values` drawn again with replacement, as many as there are.
fn median_of_draw(values: &[f64], below: &mut dyn FnMut(usize) -> usize) -> Option<f64> {
    let picked: Vec<f64> = (0..values.len()).map(|_| values[below(values.len())]).collect();
    median(&picked)
}

/// Decimals of a change in percent: one, or as many more (up to four) as it
/// takes for the rounded figure to fall on the same side of
/// [`TARGET_CHANGE`] as the change itself, so that "25.0% lower" is never
/// printed for a change that missed the target.
fn decimals(change: f64) -> usize {
    let met = change <= TARGET_CHANGE;
    (1..4)
        .find(|&d| {
            let scale = 10f64.powi(d as i32);
            ((change * 100.0 * scale).round() / scale / 100.0 <= TARGET_CHANGE) == met
        })
        .unwrap_or(4)
}

/// `-31.2%` as "31.2% lower".
fn describe(change: f64) -> String {
    let d = decimals(change);
    let p = format!("{:.d$}", (change * 100.0).abs());
    if p.trim_start_matches(['0', '.']).is_empty() {
        "unchanged".to_string()
    } else if change < 0.0 {
        format!("{p}% lower")
    } else {
        format!("{p}% higher")
    }
}

fn percent(rate: Option<f64>) -> String {
    rate.map(|r| format!("{:.0}%", r * 100.0))
        .unwrap_or_else(|| "-".to_string())
}

/// The measure of the verdict.
const TOTAL: &str = "total tokens";
/// The measure of the second reading.
const BILLED: &str = "billed-equivalent tokens";

/// Met when the median change is at or below [`TARGET_CHANGE`] and the arm
/// with lge succeeded at least as often as the arm without.
pub fn verdict(median_change: Option<f64>, off: &Tally, on: &Tally) -> Verdict {
    verdict_on(TOTAL, median_change, off, on)
}

/// [`verdict`] on another measure, named in the text.
fn verdict_on(measure: &str, median_change: Option<f64>, off: &Tally, on: &Tally) -> Verdict {
    let goal = format!(
        "at least {:.0}% lower median {measure} with no drop in success",
        -TARGET_CHANGE * 100.0
    );
    let Some(m) = median_change else {
        return Verdict {
            met: None,
            text: format!("no verdict: no task succeeded in both arms (target: {goal})"),
        };
    };
    let tokens_ok = m <= TARGET_CHANGE;
    let success_ok = on.rate.unwrap_or(0.0) >= off.rate.unwrap_or(0.0);
    let mut misses = Vec::new();
    if !tokens_ok {
        misses.push(format!("median {measure} {}", describe(m)));
    }
    if !success_ok {
        misses.push(format!(
            "success dropped from {} to {}",
            percent(off.rate),
            percent(on.rate)
        ));
    }
    let met = misses.is_empty();
    let text = if met {
        format!(
            "target met ({goal}): median {measure} {}, success {} with lge against {} without",
            describe(m),
            percent(on.rate),
            percent(off.rate)
        )
    } else {
        format!("target not met ({goal}): {}", misses.join("; "))
    };
    Verdict { met: Some(met), text }
}

fn grouped(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 {
        out.insert(0, '-');
    }
    out
}

fn signed_percent(change: Option<f64>) -> String {
    change
        .map(|c| format!("{:+.1}%", c * 100.0))
        .unwrap_or_else(|| "-".to_string())
}

/// [`signed_percent`] with as many decimals as the verdict needs.
fn target_percent(change: Option<f64>) -> String {
    change
        .map(|c| format!("{:+.d$}%", c * 100.0, d = decimals(c)))
        .unwrap_or_else(|| "-".to_string())
}

fn whole(value: Option<f64>) -> String {
    value
        .map(|v| grouped(v.round() as i64))
        .unwrap_or_else(|| "-".to_string())
}

/// What the variant compares, for the report's first lines.
fn variant_text(variant: Variant) -> &'static str {
    match variant {
        Variant::Features => "features: lge's graph context and output shortening off against on",
        Variant::Terse => {
            "terse: AGENTS.md without against with the terse-replies section; the daemon's settings as you set them in both arms"
        }
        Variant::Batch => {
            "batch: AGENTS.md lge block without against with the batch line; the daemon's settings as you set them in both arms"
        }
    }
}

/// Plain tables for the terminal: every session, then each task, each arm,
/// the context per request, then the summary and the verdicts.
pub fn render(r: &AbReport) -> String {
    let width = r
        .tasks
        .iter()
        .map(|t| t.name.chars().count())
        .max()
        .unwrap_or(4)
        .clamp(4, 32);
    let cut = |name: &str| name.chars().take(width).collect::<String>();
    let mut out = String::new();
    out.push_str(&format!("variant: {}\n", variant_text(r.variant)));
    if !r.base.is_empty() {
        out.push_str(&format!("base commit: {}\n", r.base));
    }
    match &r.note {
        Some(note) => out.push_str(&format!("note appended to every prompt: {note}\n")),
        None => out.push_str("note appended to every prompt: none\n"),
    }
    if r.approve_for_me {
        out.push_str(
            "sessions ran with --approve-for-me (workspace-write sandbox, approvals reviewed automatically)\n",
        );
    }
    if !r.work_dir.is_empty() {
        out.push_str(&format!("results and traces: {}\n", r.work_dir));
    }
    if r.resumed > 0 {
        out.push_str(&format!(
            "resumed: {} session(s) read back from an earlier run's results\n",
            r.resumed
        ));
    }
    out.push('\n');
    out.push_str(&format!(
        "{:width$}  {:>3}  {:>3}  {:>11}  {:>11}  {:>9}  {:>9}  {:>11}  {:>5}  {:>5}  result\n",
        "task", "run", "arm", "input", "cached", "output", "reasoning", "total", "turns", "reqs"
    ));
    for run in &r.runs {
        let result = match (&run.error, run.check_passed) {
            (Some(e), _) => format!("error: {e}"),
            (None, Some(true)) => "pass".to_string(),
            (None, Some(false)) => "fail".to_string(),
            (None, None) => "-".to_string(),
        };
        out.push_str(&format!(
            "{:width$}  {:>3}  {:>3}  {:>11}  {:>11}  {:>9}  {:>9}  {:>11}  {:>5}  {:>5}  {result}\n",
            cut(&run.task),
            run.run,
            run.arm.as_str(),
            grouped(run.usage.input_tokens),
            grouped(run.usage.cached_input_tokens),
            grouped(run.usage.output_tokens),
            grouped(run.usage.reasoning_tokens),
            grouped(run.total_tokens),
            run.turns,
            run.requests.map(|n| n.to_string()).unwrap_or_else(|| "-".to_string()),
        ));
    }
    out.push_str(&format!(
        "\n{:width$}  {:>13}  {:>13}  {:>8}  {:>6}  {:>6}  {:>11}  noise\n",
        "task", "off median", "on median", "change", "off ok", "on ok", "billed chg"
    ));
    for t in &r.tasks {
        let noise: Vec<String> = [(Arm::Off, &t.off), (Arm::On, &t.on)]
            .iter()
            .filter(|(_, s)| s.noisy)
            .map(|(arm, s)| format!("{arm} {:.1}x", s.spread.unwrap_or(0.0)))
            .collect();
        let row = format!(
            "{:width$}  {:>13}  {:>13}  {:>8}  {:>6}  {:>6}  {:>11}  {}",
            cut(&t.name),
            whole(t.off.median_total),
            whole(t.on.median_total),
            signed_percent(t.change),
            format!("{}/{}", t.off.successes, t.off.runs),
            format!("{}/{}", t.on.successes, t.on.runs),
            signed_percent(t.change_billed),
            noise.join(", ")
        );
        out.push_str(row.trim_end());
        out.push('\n');
    }
    out.push_str(&format!(
        "\nper arm, medians over the successful runs of the compared tasks (output includes reasoning):\n\
         {:3}  {:>8}  {:>8}  {:>9}  {:>9}  {:>11}  {:>11}\n",
        "arm", "sessions", "requests", "output", "reasoning", "total", "billed"
    ));
    for (arm, a) in [(Arm::Off, &r.per_arm.off), (Arm::On, &r.per_arm.on)] {
        out.push_str(&format!(
            "{:3}  {:>8}  {:>8}  {:>9}  {:>9}  {:>11}  {:>11}\n",
            arm.as_str(),
            a.sessions,
            whole(a.median_requests),
            whole(a.median_output),
            whole(a.median_reasoning),
            whole(a.median_total),
            whole(a.median_billed),
        ));
    }
    let traced: Vec<&ArmRun> = r.runs.iter().filter(|run| run.trace.is_some()).collect();
    if !traced.is_empty() {
        out.push_str(&format!(
            "\ncontext per request, from the daemon's traces (input tokens of each request, cached included):\n\
             {:width$}  {:>3}  {:>3}  {:>5}  {:>11}  {:>11}\n",
            "task", "run", "arm", "reqs", "mean", "max"
        ));
        for run in traced {
            let Some(t) = &run.trace else { continue };
            out.push_str(&format!(
                "{:width$}  {:>3}  {:>3}  {:>5}  {:>11}  {:>11}\n",
                cut(&run.task),
                run.run,
                run.arm.as_str(),
                t.requests,
                whole(t.mean_context),
                whole(t.max_context.map(|m| m as f64)),
            ));
            if !t.largest_outputs.is_empty() {
                let list: Vec<String> = t
                    .largest_outputs
                    .iter()
                    .map(|o| {
                        format!(
                            "{} tokens `{}`{}",
                            grouped(o.tokens as i64),
                            o.command,
                            if o.shortened { " (shortened by lge)" } else { "" }
                        )
                    })
                    .collect();
                out.push_str(&format!("    largest outputs: {}\n", list.join("; ")));
            }
        }
    }
    out.push_str(&format!(
        "\ncompared over {} of {} task(s) where both arms succeeded\n",
        r.compared,
        r.tasks.len()
    ));
    out.push_str(&format!(
        "total tokens with lge against without: median {}, mean {}\n",
        target_percent(r.median_change),
        signed_percent(r.mean_change)
    ));
    if let Some(i) = &r.interval {
        out.push_str(&format!(
            "95% interval of the median change (bootstrap: tasks, then runs within each task; {} resamples): {} to {}\n",
            i.resamples,
            signed_percent(Some(i.low)),
            signed_percent(Some(i.high))
        ));
    }
    if let Some(g) = r.geo_mean_ratio {
        out.push_str(&format!(
            "geometric mean of the tasks' on/off ratios: {g:.3} ({})\n",
            signed_percent(Some(g - 1.0))
        ));
    }
    out.push_str(&format!(
        "billed-equivalent tokens ({}): median {}, mean {}\n",
        r.weights,
        signed_percent(r.median_change_billed),
        signed_percent(r.mean_change_billed)
    ));
    out.push_str(&format!(
        "success: without lge {}/{} ({}), with lge {}/{} ({})\n",
        r.success_off.successes,
        r.success_off.runs,
        percent(r.success_off.rate),
        r.success_on.successes,
        r.success_on.runs,
        percent(r.success_on.rate)
    ));
    for t in &r.tasks {
        for (arm, s) in [(Arm::Off, &t.off), (Arm::On, &t.on)] {
            if s.noisy {
                out.push_str(&format!(
                    "noisy: {} with lge {arm}: total tokens of its passing runs differ {:.1}x (above {NOISE_SPREAD}x); \
                     its median is a weak estimate, add runs (a higher --runs with --resume adds them to these results)\n",
                    t.name,
                    s.spread.unwrap_or(0.0)
                ));
            }
        }
    }
    if r.errored > 0 {
        out.push_str(&format!(
            "warning: {} session(s) ended in an error before their check; their pairs are not compared\n",
            r.errored
        ));
    }
    if r.unmeasured > 0 {
        out.push_str(&format!(
            "warning: {} successful session(s) reported no token usage; they are left out of the medians\n",
            r.unmeasured
        ));
    }
    if r.not_proxied > 0 {
        out.push_str(&format!(
            "warning: {} session(s) did not go through the daemon, so the arm's settings did not apply to them; \
             is `openai_base_url` in ~/.codex/config.toml set to the daemon?\n",
            r.not_proxied
        ));
    }
    if r.interrupted {
        out.push_str("interrupted: the numbers cover the pairs that finished\n");
    }
    if !r.settings_restored {
        out.push_str("warning: the daemon's settings could not be restored; set them back on its page\n");
    }
    out.push_str(&format!(
        "second reading, on billed-equivalent tokens (not the verdict): {}\n",
        r.verdict_billed.text
    ));
    out.push_str(&format!("verdict: {}\n", r.verdict.text));
    out
}
