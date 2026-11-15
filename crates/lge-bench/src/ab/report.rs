//! What an A/B run found: per task and arm, then the change in total tokens
//! over the tasks both arms got right, and the verdict against the target.

use super::{Arm, ArmRun};
use serde::Serialize;
use std::collections::BTreeMap;

/// The target: the median total tokens with lge's features on at least 25%
/// lower than with them off, with no drop in the success rate.
pub const TARGET_CHANGE: f64 = -0.25;

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

/// One arm of one task, over its runs in pairs where both arms ran.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct ArmSummary {
    pub runs: usize,
    pub successes: usize,
    /// Median total tokens (input plus output) of the successful runs whose
    /// usage was read; `None` without one.
    pub median_total: Option<f64>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TaskSummary {
    pub name: String,
    pub off: ArmSummary,
    pub on: ArmSummary,
    /// `(on - off) / off` of the median totals; `None` unless both arms
    /// succeeded at least once.
    pub change: Option<f64>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Verdict {
    /// `None` when no task succeeded in both arms.
    pub met: Option<bool>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AbReport {
    /// Every session, in the order it ran.
    pub runs: Vec<ArmRun>,
    pub tasks: Vec<TaskSummary>,
    /// Tasks where both arms succeeded (with their usage read): the changes
    /// below are taken over these.
    pub compared: usize,
    pub median_change: Option<f64>,
    pub mean_change: Option<f64>,
    /// Over the pairs where both arms ran.
    pub success_off: Tally,
    pub success_on: Tally,
    pub target_change: f64,
    pub verdict: Verdict,
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

/// The report for `runs`, with tasks in the order of `names`. Only pairs where
/// both arms ran (see [`ran`]) count: the others are listed, never compared.
pub fn summarize(names: &[String], runs: Vec<ArmRun>) -> AbReport {
    let mut arms_of_pair: BTreeMap<(&str, usize), [bool; 2]> = BTreeMap::new();
    for r in runs.iter().filter(|r| ran(r)) {
        arms_of_pair.entry((r.task.as_str(), r.run)).or_default()[usize::from(r.arm == Arm::On)] = true;
    }
    let complete: Vec<&ArmRun> = runs
        .iter()
        .filter(|r| ran(r) && arms_of_pair.get(&(r.task.as_str(), r.run)) == Some(&[true, true]))
        .collect();
    let summary = |name: &str, arm: Arm| {
        let mine: Vec<&&ArmRun> = complete.iter().filter(|r| r.task == name && r.arm == arm).collect();
        let totals: Vec<f64> = mine
            .iter()
            .filter(|r| r.success && r.total_tokens > 0)
            .map(|r| r.total_tokens as f64)
            .collect();
        ArmSummary {
            runs: mine.len(),
            successes: mine.iter().filter(|r| r.success).count(),
            median_total: median(&totals),
        }
    };
    let tasks: Vec<TaskSummary> = names
        .iter()
        .map(|name| {
            let (off, on) = (summary(name, Arm::Off), summary(name, Arm::On));
            let change = match (off.median_total, on.median_total) {
                (Some(o), Some(n)) if o > 0.0 => Some((n - o) / o),
                _ => None,
            };
            TaskSummary {
                name: name.clone(),
                off,
                on,
                change,
            }
        })
        .collect();
    let changes: Vec<f64> = tasks.iter().filter_map(|t| t.change).collect();
    let success_off = Tally::of(complete.iter().copied().filter(|r| r.arm == Arm::Off));
    let success_on = Tally::of(complete.iter().copied().filter(|r| r.arm == Arm::On));
    let median_change = median(&changes);
    AbReport {
        not_proxied: runs.iter().filter(|r| r.proxied == Some(false)).count(),
        errored: runs
            .iter()
            .filter(|r| !r.interrupted && r.check_passed.is_none())
            .count(),
        unmeasured: complete.iter().filter(|r| r.success && r.total_tokens <= 0).count(),
        runs,
        compared: changes.len(),
        mean_change: mean(&changes),
        verdict: verdict(median_change, &success_off, &success_on),
        median_change,
        tasks,
        success_off,
        success_on,
        target_change: TARGET_CHANGE,
        interrupted: false,
        settings_restored: true,
    }
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

/// Met when the median change is at or below [`TARGET_CHANGE`] and the arm
/// with lge succeeded at least as often as the arm without.
pub fn verdict(median_change: Option<f64>, off: &Tally, on: &Tally) -> Verdict {
    let goal = format!(
        "at least {:.0}% lower median total tokens with no drop in success",
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
        misses.push(format!("median total tokens {}", describe(m)));
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
            "target met ({goal}): median total tokens {}, success {} with lge against {} without",
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

/// Plain tables for the terminal: every session, then each task, then the verdict.
pub fn render(r: &AbReport) -> String {
    let width = r
        .tasks
        .iter()
        .map(|t| t.name.chars().count())
        .max()
        .unwrap_or(4)
        .clamp(4, 32);
    let cut = |name: &str| name.chars().take(width).collect::<String>();
    let mut out = format!(
        "{:width$}  {:>3}  {:>3}  {:>11}  {:>11}  {:>9}  {:>9}  {:>11}  {:>5}  {:>5}  result\n",
        "task", "run", "arm", "input", "cached", "output", "reasoning", "total", "turns", "reqs"
    );
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
        "\n{:width$}  {:>13}  {:>13}  {:>8}  {:>6}  {:>6}\n",
        "task", "off median", "on median", "change", "off ok", "on ok"
    ));
    let median_text = |m: Option<f64>| m.map(|v| grouped(v.round() as i64)).unwrap_or_else(|| "-".to_string());
    for t in &r.tasks {
        out.push_str(&format!(
            "{:width$}  {:>13}  {:>13}  {:>8}  {:>6}  {:>6}\n",
            cut(&t.name),
            median_text(t.off.median_total),
            median_text(t.on.median_total),
            signed_percent(t.change),
            format!("{}/{}", t.off.successes, t.off.runs),
            format!("{}/{}", t.on.successes, t.on.runs),
        ));
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
    out.push_str(&format!(
        "success: without lge {}/{} ({}), with lge {}/{} ({})\n",
        r.success_off.successes,
        r.success_off.runs,
        percent(r.success_off.rate),
        r.success_on.successes,
        r.success_on.runs,
        percent(r.success_on.rate)
    ));
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
    out.push_str(&format!("verdict: {}\n", r.verdict.text));
    out
}
