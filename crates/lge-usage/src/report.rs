//! The report behind `lge usage report`: what each routed level cost and how
//! often it was enough.

use crate::codex::TokenUsage;
use crate::prices::Prices;
use crate::record::known_outcome;
use lge_core::{outcome, Level, UsageRecord};
use serde::Serialize;
use std::collections::BTreeMap;

/// Label of the row that covers every task.
const ALL: &str = "all";
/// Label of the row for records whose level is not `low`, `medium` or `top`.
const OTHER: &str = "other";
/// The `runner` of a record written by `--dry-run`: the prompt was printed and no
/// session ran.
const DRY_RUN: &str = "dry-run";

/// Usage aggregated per routed level and overall.
///
/// A task is the group of records with one `task_id`. It is counted, with all its
/// attempts and all their tokens, under the level of its first attempt: the level
/// the router chose. The row of a level therefore shows what that routing decision
/// ended up costing, retries at higher levels included, and the rows add up to
/// `overall`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    /// `low`, `medium`, `top`, always all three and in that order; then `other`,
    /// only when some record has a level outside the three.
    pub levels: Vec<LevelStats>,
    pub overall: LevelStats,
    /// Records of the `dry-run` runner. They are in no row: no session ran, and
    /// counted as tasks with zero tokens they would lower every average.
    pub dry_runs: usize,
    /// The prices the cost fields were computed with. Absent: no cost fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prices: Option<Prices>,
}

/// One row of the report.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct LevelStats {
    pub level: String,
    pub tasks: usize,
    /// Sessions run: one record each.
    pub attempts: usize,
    /// Attempts that were retries: flagged `escalated`, numbered above 1, or
    /// coming after another attempt of the same task.
    pub escalations: usize,
    /// Tasks whose only attempt succeeded.
    pub succeeded_first_attempt: usize,
    /// Tasks whose last attempt succeeded and was a retry.
    pub succeeded_after_escalation: usize,
    /// Tasks whose last attempt failed.
    pub failed: usize,
    /// Tasks whose last attempt has no known outcome (no checks ran, a dry run).
    pub unknown: usize,
    /// Succeeded tasks over tasks with a known outcome. Unknown outcomes are left
    /// out, so a batch of dry runs neither lowers nor raises the rate. `None` when
    /// no task has a known outcome.
    pub success_rate: Option<f64>,
    /// Tokens of every attempt, by class.
    #[serde(flatten)]
    pub tokens: TokenUsage,
    /// Input tokens that were not read from the cache.
    pub uncached_input_tokens: i64,
    /// Input plus output.
    pub total_tokens: i64,
    /// Cached input over input, 0 to 1; 0 without input.
    pub cached_share: f64,
    pub avg_tokens_per_task: f64,
    /// Tokens of the attempts that were followed by an escalation, by class: work
    /// that was paid for and then redone one level higher. This is the amount to
    /// count against the router's saving.
    pub retry: TokenUsage,
    /// Input plus output of `retry`.
    pub retry_tokens: i64,
    /// Cost of `tokens`. The three cost fields exist only when prices were given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avg_cost_per_task: Option<f64>,
    /// Cost of `retry`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_cost: Option<f64>,
}

impl LevelStats {
    fn named(level: &str) -> LevelStats {
        LevelStats {
            level: level.to_string(),
            ..LevelStats::default()
        }
    }

    /// Adds one task, given its attempts in order. The outcome of the task is the
    /// outcome of its last attempt.
    fn add_task(&mut self, attempts: &[&UsageRecord]) {
        let Some((last, superseded)) = attempts.split_last() else {
            return;
        };
        self.tasks += 1;
        self.attempts += attempts.len();
        // The same signs decide the escalation count and whether a success came
        // after an escalation, so the two columns cannot contradict each other.
        self.escalations += attempts
            .iter()
            .enumerate()
            .filter(|(i, rec)| *i > 0 || is_retry(rec))
            .count();
        let retried = !superseded.is_empty() || is_retry(last);
        match known_outcome(&last.outcome) {
            outcome::SUCCESS if retried => self.succeeded_after_escalation += 1,
            outcome::SUCCESS => self.succeeded_first_attempt += 1,
            outcome::FAILURE => self.failed += 1,
            _ => self.unknown += 1,
        }
        for rec in attempts {
            self.tokens += TokenUsage::of(rec);
        }
        for rec in superseded {
            self.retry += TokenUsage::of(rec);
        }
    }

    /// Fills the fields that are derived from the sums.
    fn finish(&mut self, prices: Option<&Prices>) {
        let succeeded = self.succeeded_first_attempt + self.succeeded_after_escalation;
        let known = succeeded + self.failed;
        self.success_rate = (known > 0).then(|| succeeded as f64 / known as f64);
        self.uncached_input_tokens = self.tokens.uncached_input_tokens();
        self.total_tokens = self.tokens.total();
        // A record that claims more cached tokens than input counts as fully
        // cached, so the share stays within 0 to 1.
        let cached = self.tokens.cached_input_tokens.min(self.tokens.input_tokens);
        self.cached_share = ratio(cached as f64, self.tokens.input_tokens as f64);
        self.avg_tokens_per_task = ratio(self.total_tokens as f64, self.tasks as f64);
        self.retry_tokens = self.retry.total();
        if let Some(prices) = prices {
            let cost = prices.cost(&self.tokens);
            self.cost = Some(cost);
            self.avg_cost_per_task = Some(ratio(cost, self.tasks as f64));
            self.retry_cost = Some(prices.cost(&self.retry));
        }
    }
}

/// True when the record says it is not a first attempt. A lone record can still
/// be a retry: its first attempt may be older than the window the caller listed.
fn is_retry(rec: &UsageRecord) -> bool {
    rec.escalated || rec.attempt > 1
}

/// `part / whole`, 0 when there is nothing to divide by.
fn ratio(part: f64, whole: f64) -> f64 {
    if whole > 0.0 {
        part / whole
    } else {
        0.0
    }
}

/// Aggregates usage records. `prices` decides whether the cost fields exist: no
/// price is built in, so without a price table there is no cost. Records of the
/// `dry-run` runner are counted in [`Report::dry_runs`] and nowhere else.
pub fn report(records: &[UsageRecord], prices: Option<&Prices>) -> Report {
    let mut rows = [
        LevelStats::named(Level::Low.as_str()),
        LevelStats::named(Level::Medium.as_str()),
        LevelStats::named(Level::Top.as_str()),
        LevelStats::named(OTHER),
    ];
    let mut overall = LevelStats::named(ALL);
    let (dry_runs, sessions): (Vec<&UsageRecord>, Vec<&UsageRecord>) = records.iter().partition(|rec| is_dry_run(rec));
    for attempts in tasks(&sessions) {
        rows[routed_row(&attempts)].add_task(&attempts);
        overall.add_task(&attempts);
    }
    let mut levels: Vec<LevelStats> = rows
        .into_iter()
        .filter(|row| row.level != OTHER || row.tasks > 0)
        .collect();
    for row in levels.iter_mut().chain(std::iter::once(&mut overall)) {
        row.finish(prices);
    }
    Report {
        levels,
        overall,
        dry_runs: dry_runs.len(),
        prices: prices.copied(),
    }
}

fn is_dry_run(rec: &UsageRecord) -> bool {
    rec.runner.trim().eq_ignore_ascii_case(DRY_RUN)
}

/// Index into the rows of [`report`] for a task: the level of its first attempt.
fn routed_row(attempts: &[&UsageRecord]) -> usize {
    match attempts.first().and_then(|first| Level::parse(&first.level)) {
        Some(Level::Low) => 0,
        Some(Level::Medium) => 1,
        Some(Level::Top) => 2,
        None => 3,
    }
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum TaskKey<'a> {
    Id(&'a str),
    /// A record without a task id cannot be an attempt of any other record, so it
    /// is a task of its own, keyed by its position.
    Lone(usize),
}

/// The attempts of each task, first attempt first.
fn tasks<'a>(records: &[&'a UsageRecord]) -> Vec<Vec<&'a UsageRecord>> {
    let mut groups: BTreeMap<TaskKey, Vec<&UsageRecord>> = BTreeMap::new();
    for (i, &rec) in records.iter().enumerate() {
        let key = if rec.task_id.is_empty() {
            TaskKey::Lone(i)
        } else {
            TaskKey::Id(&rec.task_id)
        };
        groups.entry(key).or_default().push(rec);
    }
    let mut tasks: Vec<Vec<&UsageRecord>> = groups.into_values().collect();
    for attempts in &mut tasks {
        attempts.sort_by(|a, b| (a.attempt, a.started_at, &a.id).cmp(&(b.attempt, b.started_at, &b.id)));
    }
    tasks
}

/// Renders the report as plain aligned tables: tasks, tokens and, when prices
/// were given, cost. The same report always gives the same text.
pub fn render_report(r: &Report) -> String {
    let rows: Vec<&LevelStats> = r.levels.iter().chain(std::iter::once(&r.overall)).collect();
    let all = &r.overall;
    let mut out = format!(
        "Usage report: tasks {}, attempts {}, escalations {}\n",
        grouped(all.tasks as i64),
        grouped(all.attempts as i64),
        grouped(all.escalations as i64)
    );
    if r.dry_runs > 0 {
        out.push_str(&format!(
            "Dry runs: {} records left out, no session ran\n",
            grouped(r.dry_runs as i64)
        ));
    }

    table(
        &mut out,
        "Tasks, by the level of the first attempt",
        &[
            "level",
            "tasks",
            "attempts",
            "escalations",
            "ok first",
            "ok escalated",
            "failed",
            "unknown",
            "success",
        ],
        rows.iter().map(|row| task_cells(row)).collect(),
    );
    table(
        &mut out,
        "Tokens (cached is part of input, reasoning is part of output, total = input + output)",
        &[
            "level",
            "input",
            "cached",
            "cached %",
            "output",
            "reasoning",
            "total",
            "avg/task",
            "retry",
        ],
        rows.iter().map(|row| token_cells(row)).collect(),
    );
    match &r.prices {
        Some(prices) => table(
            &mut out,
            &format!(
                "Cost (per million tokens: input {}, cached input {}, output {})",
                prices.input_per_mtok, prices.cached_input_per_mtok, prices.output_per_mtok
            ),
            &["level", "cost", "cost/task", "retry cost"],
            rows.iter().map(|row| cost_cells(row)).collect(),
        ),
        None => out.push_str("\nCost: not shown, no [prices] table in lge.toml\n"),
    }

    out.push_str(LEGEND);
    out
}

const LEGEND: &str = "
rows hold whole tasks: a retry at a higher level is counted in the row of the level the task started at
ok first: succeeded on the first attempt; ok escalated: succeeded after a retry one level higher
success: succeeded over succeeded plus failed; unknown outcomes are left out
retry: tokens of attempts that were followed by an escalation
";

fn task_cells(row: &LevelStats) -> Vec<String> {
    vec![
        row.level.clone(),
        grouped(row.tasks as i64),
        grouped(row.attempts as i64),
        grouped(row.escalations as i64),
        grouped(row.succeeded_first_attempt as i64),
        grouped(row.succeeded_after_escalation as i64),
        grouped(row.failed as i64),
        grouped(row.unknown as i64),
        row.success_rate.map_or_else(|| "-".to_string(), percent),
    ]
}

fn token_cells(row: &LevelStats) -> Vec<String> {
    vec![
        row.level.clone(),
        grouped(row.tokens.input_tokens),
        grouped(row.tokens.cached_input_tokens),
        percent(row.cached_share),
        grouped(row.tokens.output_tokens),
        grouped(row.tokens.reasoning_tokens),
        grouped(row.total_tokens),
        grouped(row.avg_tokens_per_task.round() as i64),
        grouped(row.retry_tokens),
    ]
}

fn cost_cells(row: &LevelStats) -> Vec<String> {
    vec![
        row.level.clone(),
        money(row.cost),
        money(row.avg_cost_per_task),
        money(row.retry_cost),
    ]
}

/// Appends a blank line, the title and the table. The first column is left
/// aligned and the others right aligned, each as wide as its widest cell.
fn table(out: &mut String, title: &str, header: &[&str], rows: Vec<Vec<String>>) {
    let mut widths: Vec<usize> = header.iter().map(|cell| cell.chars().count()).collect();
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    out.push('\n');
    out.push_str(title);
    out.push('\n');
    push_row(out, header.iter().copied(), &widths);
    for row in &rows {
        push_row(out, row.iter().map(String::as_str), &widths);
    }
}

fn push_row<'a>(out: &mut String, cells: impl Iterator<Item = &'a str>, widths: &[usize]) {
    let mut line = String::new();
    for (i, (cell, width)) in cells.zip(widths).enumerate() {
        if i == 0 {
            line.push_str(&format!("{cell:<width$}"));
        } else {
            line.push_str(&format!("  {cell:>width$}"));
        }
    }
    out.push_str(line.trim_end());
    out.push('\n');
}

/// `1234567` as `1,234,567`.
fn grouped(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    if n < 0 {
        out.push('-');
    }
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// `0.4923` as `49.2%`.
fn percent(share: f64) -> String {
    format!("{:.1}%", share * 100.0)
}

/// Four decimals: one task often costs a fraction of a cent.
fn money(amount: Option<f64>) -> String {
    amount.map_or_else(|| "-".to_string(), |amount| format!("{amount:.4}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRICES: Prices = Prices {
        input_per_mtok: 2.0,
        cached_input_per_mtok: 0.5,
        output_per_mtok: 10.0,
    };

    /// Token counts are given in thousands, as (input, cached, reasoning, output).
    fn rec(task: &str, attempt: i64, level: &str, outcome: &str, tokens: (i64, i64, i64, i64)) -> UsageRecord {
        UsageRecord {
            id: format!("{task}/{attempt}"),
            task_id: task.to_string(),
            task: format!("task {task}"),
            attempt,
            level: level.to_string(),
            started_at: 1000 + attempt,
            input_tokens: tokens.0 * 1000,
            cached_input_tokens: tokens.1 * 1000,
            reasoning_tokens: tokens.2 * 1000,
            output_tokens: tokens.3 * 1000,
            outcome: outcome.to_string(),
            escalated: attempt > 1,
            ..UsageRecord::default()
        }
    }

    fn usage(input: i64, cached: i64, reasoning: i64, output: i64) -> TokenUsage {
        TokenUsage {
            input_tokens: input * 1000,
            cached_input_tokens: cached * 1000,
            reasoning_tokens: reasoning * 1000,
            output_tokens: output * 1000,
        }
    }

    /// Six tasks, deliberately out of order:
    /// A low, succeeds at once. B low, fails, succeeds at medium. C low, fails at
    /// low, medium and top. D medium, succeeds. E top, outcome unknown. F medium,
    /// fails with no retry.
    fn sample() -> Vec<UsageRecord> {
        vec![
            rec("C", 3, "top", "failure", (4000, 3000, 400, 500)),
            rec("E", 1, "top", "unknown", (8000, 6000, 800, 1000)),
            rec("B", 2, "medium", "success", (3000, 1500, 150, 300)),
            rec("A", 1, "low", "success", (1000, 400, 40, 100)),
            rec("C", 1, "low", "failure", (1000, 0, 10, 50)),
            rec("F", 1, "medium", "failure", (600, 0, 20, 40)),
            rec("B", 1, "low", "failure", (2000, 500, 60, 200)),
            rec("D", 1, "medium", "success", (5000, 2500, 250, 250)),
            rec("C", 2, "medium", "failure", (2000, 1000, 100, 100)),
        ]
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn levels_are_always_low_medium_top() {
        let r = report(&[], None);
        let names: Vec<&str> = r.levels.iter().map(|row| row.level.as_str()).collect();
        assert_eq!(names, ["low", "medium", "top"]);
        assert_eq!(r.overall, LevelStats::named("all"));
        assert_eq!(r.levels[0], LevelStats::named("low"));
        assert_eq!(r.prices, None);
    }

    #[test]
    fn counts_tasks_attempts_and_escalations_per_routed_level() {
        let r = report(&sample(), None);
        let counts = |row: &LevelStats| (row.tasks, row.attempts, row.escalations);
        assert_eq!(counts(&r.levels[0]), (3, 6, 3), "A, B and C were routed to low");
        assert_eq!(
            counts(&r.levels[1]),
            (2, 2, 0),
            "D and F; B's and C's medium retries belong to low"
        );
        assert_eq!(counts(&r.levels[2]), (1, 1, 0), "E; C's top retry belongs to low");
        assert_eq!(counts(&r.overall), (6, 9, 3));
    }

    #[test]
    fn task_outcome_is_that_of_the_last_attempt() {
        let r = report(&sample(), None);
        let outcomes = |row: &LevelStats| {
            (
                row.succeeded_first_attempt,
                row.succeeded_after_escalation,
                row.failed,
                row.unknown,
            )
        };
        assert_eq!(
            outcomes(&r.levels[0]),
            (1, 1, 1, 0),
            "A at once, B after escalation, C failed"
        );
        assert_eq!(outcomes(&r.levels[1]), (1, 0, 1, 0));
        assert_eq!(outcomes(&r.levels[2]), (0, 0, 0, 1));
        assert_eq!(outcomes(&r.overall), (2, 1, 2, 1));
        for row in r.levels.iter().chain([&r.overall]) {
            let (first, later, failed, unknown) = outcomes(row);
            assert_eq!(
                first + later + failed + unknown,
                row.tasks,
                "{}: every task has one outcome",
                row.level
            );
        }
    }

    #[test]
    fn success_rate_leaves_unknown_outcomes_out() {
        let r = report(&sample(), None);
        assert!(close(r.levels[0].success_rate.unwrap(), 2.0 / 3.0));
        assert!(close(r.levels[1].success_rate.unwrap(), 0.5));
        assert_eq!(r.levels[2].success_rate, None, "only an unknown outcome: no rate");
        assert!(
            close(r.overall.success_rate.unwrap(), 3.0 / 5.0),
            "E is not in the denominator"
        );
    }

    #[test]
    fn tokens_are_summed_by_class() {
        let r = report(&sample(), None);
        assert_eq!(r.levels[0].tokens, usage(13000, 6400, 760, 1250));
        assert_eq!(r.levels[1].tokens, usage(5600, 2500, 270, 290));
        assert_eq!(r.levels[2].tokens, usage(8000, 6000, 800, 1000));
        assert_eq!(r.overall.tokens, usage(26600, 14900, 1830, 2540));

        assert_eq!(
            r.levels[0].total_tokens, 14_250_000,
            "input + output; cached and reasoning are inside"
        );
        assert_eq!(r.levels[0].uncached_input_tokens, 6_600_000);
        assert_eq!(r.overall.total_tokens, 29_140_000);
        assert_eq!(r.overall.uncached_input_tokens, 11_700_000);
    }

    #[test]
    fn cached_share_is_cached_over_input() {
        let r = report(&sample(), None);
        assert!(close(r.levels[0].cached_share, 6400.0 / 13000.0));
        assert!(close(r.levels[1].cached_share, 2500.0 / 5600.0));
        assert!(close(r.levels[2].cached_share, 0.75));
        assert!(close(r.overall.cached_share, 14900.0 / 26600.0));
        assert_eq!(
            report(&[], None).overall.cached_share,
            0.0,
            "no input: no share, not NaN"
        );
    }

    #[test]
    fn average_tokens_per_task_counts_every_attempt_of_the_task() {
        let r = report(&sample(), None);
        assert!(close(r.levels[0].avg_tokens_per_task, 4_750_000.0));
        assert!(close(r.levels[1].avg_tokens_per_task, 2_945_000.0));
        assert!(close(r.levels[2].avg_tokens_per_task, 9_000_000.0));
        assert!(close(r.overall.avg_tokens_per_task, 29_140_000.0 / 6.0));
        assert_eq!(report(&[], None).overall.avg_tokens_per_task, 0.0);
    }

    #[test]
    fn retry_cost_is_the_tokens_of_attempts_that_were_escalated() {
        let r = report(&sample(), None);
        // B's low attempt, C's low attempt and C's medium attempt were redone.
        // C's last attempt failed too, but nothing came after it.
        assert_eq!(r.levels[0].retry, usage(5000, 1500, 170, 350));
        assert_eq!(r.levels[0].retry_tokens, 5_350_000);
        assert_eq!(
            r.levels[1].retry,
            TokenUsage::default(),
            "F failed but was never retried"
        );
        assert_eq!(r.levels[2].retry_tokens, 0);
        assert_eq!(r.overall.retry, usage(5000, 1500, 170, 350));
        assert_eq!(r.overall.retry_tokens, 5_350_000);
    }

    #[test]
    fn level_rows_add_up_to_the_overall_row() {
        let mut records = sample();
        records.push(rec("G", 1, "experimental", "success", (10, 5, 1, 2)));
        let r = report(&records, Some(&PRICES));
        let sum = |f: fn(&LevelStats) -> i64| r.levels.iter().map(f).sum::<i64>();
        assert_eq!(sum(|row| row.tasks as i64), r.overall.tasks as i64);
        assert_eq!(sum(|row| row.attempts as i64), r.overall.attempts as i64);
        assert_eq!(sum(|row| row.escalations as i64), r.overall.escalations as i64);
        assert_eq!(sum(|row| row.total_tokens), r.overall.total_tokens);
        assert_eq!(
            sum(|row| row.tokens.cached_input_tokens),
            r.overall.tokens.cached_input_tokens
        );
        assert_eq!(
            sum(|row| row.tokens.reasoning_tokens),
            r.overall.tokens.reasoning_tokens
        );
        assert_eq!(sum(|row| row.retry_tokens), r.overall.retry_tokens);
        let cost: f64 = r.levels.iter().filter_map(|row| row.cost).sum();
        assert!(close(cost, r.overall.cost.unwrap()));
    }

    #[test]
    fn cost_exists_only_with_prices() {
        let without = report(&sample(), None);
        for row in without.levels.iter().chain([&without.overall]) {
            assert_eq!(
                (row.cost, row.avg_cost_per_task, row.retry_cost),
                (None, None, None),
                "{}",
                row.level
            );
        }

        let with = report(&sample(), Some(&PRICES));
        assert_eq!(with.prices, Some(PRICES));
        let costs = |row: &LevelStats| {
            (
                row.cost.unwrap(),
                row.avg_cost_per_task.unwrap(),
                row.retry_cost.unwrap(),
            )
        };
        // low: 6.6M uncached at 2 + 6.4M cached at 0.5 + 1.25M output at 10.
        let (cost, per_task, retry) = costs(&with.levels[0]);
        assert!(
            close(cost, 28.9) && close(per_task, 28.9 / 3.0) && close(retry, 11.25),
            "{cost} {per_task} {retry}"
        );
        let (cost, per_task, retry) = costs(&with.levels[1]);
        assert!(
            close(cost, 10.35) && close(per_task, 5.175) && close(retry, 0.0),
            "{cost} {per_task} {retry}"
        );
        let (cost, per_task, retry) = costs(&with.levels[2]);
        assert!(
            close(cost, 17.0) && close(per_task, 17.0) && close(retry, 0.0),
            "{cost} {per_task} {retry}"
        );
        let (cost, per_task, retry) = costs(&with.overall);
        assert!(
            close(cost, 56.25) && close(per_task, 9.375) && close(retry, 11.25),
            "{cost} {per_task} {retry}"
        );

        let empty = report(&[], Some(&PRICES));
        assert_eq!(empty.overall.cost, Some(0.0));
        assert_eq!(empty.overall.avg_cost_per_task, Some(0.0), "no tasks: zero, not NaN");
    }

    #[test]
    fn everything_but_cost_is_the_same_with_and_without_prices() {
        let mut priced = report(&sample(), Some(&PRICES));
        priced.prices = None;
        for row in priced.levels.iter_mut().chain([&mut priced.overall]) {
            (row.cost, row.avg_cost_per_task, row.retry_cost) = (None, None, None);
        }
        assert_eq!(priced, report(&sample(), None));
    }

    #[test]
    fn record_order_does_not_change_the_report() {
        let mut reversed = sample();
        reversed.reverse();
        assert_eq!(report(&reversed, Some(&PRICES)), report(&sample(), Some(&PRICES)));
    }

    #[test]
    fn a_success_followed_by_a_failed_attempt_is_a_failure() {
        let records = [
            rec("T", 2, "medium", "failure", (20, 0, 0, 2)),
            rec("T", 1, "low", "success", (10, 0, 0, 1)),
        ];
        let r = report(&records, None);
        assert_eq!((r.levels[0].failed, r.levels[0].succeeded_first_attempt), (1, 0));
        assert_eq!(r.levels[0].retry, usage(10, 0, 0, 1));
    }

    #[test]
    fn a_lone_retry_record_counts_as_success_after_escalation() {
        // The first attempt is outside the listed window; only the retry is here.
        let r = report(&[rec("T", 2, "medium", "success", (20, 0, 0, 2))], None);
        let medium = &r.levels[1];
        assert_eq!((medium.tasks, medium.attempts, medium.escalations), (1, 1, 1));
        assert_eq!(
            (medium.succeeded_first_attempt, medium.succeeded_after_escalation),
            (0, 1)
        );
        assert_eq!(medium.retry_tokens, 0, "the attempt it replaced is not in the data");
    }

    #[test]
    fn a_retry_without_the_escalated_flag_is_still_an_escalation() {
        // Written by something that numbers attempts but does not set the flag.
        let mut numbered = rec("T", 2, "medium", "success", (20, 0, 0, 2));
        numbered.escalated = false;
        let r = report(&[rec("T", 1, "low", "failure", (10, 0, 0, 1)), numbered.clone()], None);
        let low = &r.levels[0];
        assert_eq!(
            (low.attempts, low.escalations, low.succeeded_after_escalation),
            (2, 1, 1)
        );
        assert_eq!(low.retry_tokens, 11_000);

        let alone = report(&[numbered], None);
        assert_eq!(
            (alone.levels[1].escalations, alone.levels[1].succeeded_after_escalation),
            (1, 1)
        );

        // Neither flag nor number: the second record of a task is still a retry.
        let mut unnumbered = [
            rec("U", 0, "low", "failure", (10, 0, 0, 1)),
            rec("U", 0, "medium", "success", (20, 0, 0, 2)),
        ];
        unnumbered[1].started_at = 2000;
        let r = report(&unnumbered, None);
        assert_eq!(
            (r.levels[0].escalations, r.levels[0].succeeded_after_escalation),
            (1, 1)
        );
        assert_eq!(r.levels[0].succeeded_first_attempt, 0);
    }

    #[test]
    fn unrecognized_outcome_and_level_are_kept_visible() {
        let records = [
            rec("T", 1, "low", "passed", (10, 0, 0, 1)),
            rec("U", 1, "", "success", (20, 0, 0, 2)),
            rec("V", 1, "HIGH", " Success ", (30, 0, 0, 3)),
        ];
        let r = report(&records, None);
        let names: Vec<&str> = r.levels.iter().map(|row| row.level.as_str()).collect();
        assert_eq!(names, ["low", "medium", "top", "other"]);
        assert_eq!(
            r.levels[0].unknown, 1,
            "an outcome outside the vocabulary is unknown, not success"
        );
        assert_eq!(
            r.levels[2].succeeded_first_attempt, 1,
            "`high` is the top level, as in Level::parse"
        );
        assert_eq!((r.levels[3].tasks, r.levels[3].total_tokens), (1, 22_000));
        assert_eq!(r.overall.tasks, 3);
    }

    #[test]
    fn records_without_a_task_id_are_separate_tasks() {
        let records = [
            rec("", 1, "low", "success", (10, 0, 0, 1)),
            rec("", 1, "low", "failure", (10, 0, 0, 1)),
        ];
        let r = report(&records, None);
        assert_eq!((r.levels[0].tasks, r.levels[0].attempts), (2, 2));
        assert_eq!((r.levels[0].succeeded_first_attempt, r.levels[0].failed), (1, 1));
        assert_eq!(r.levels[0].retry_tokens, 0);
    }

    /// Every table: each row as wide as its header.
    fn assert_aligned(text: &str, tables: usize) {
        let blocks: Vec<&str> = text.split("\n\n").skip(1).take(tables).collect();
        assert_eq!(blocks.len(), tables, "{text}");
        for block in blocks {
            let mut lines = block.lines().skip(1);
            let header = lines.next().unwrap();
            assert!(header.starts_with("level "), "{header:?}");
            for line in lines {
                assert_eq!(
                    line.chars().count(),
                    header.chars().count(),
                    "{line:?} against {header:?}"
                );
            }
        }
    }

    #[test]
    fn dry_runs_are_left_out_of_every_row_and_counted_apart() {
        let mut records = sample();
        for i in 0..4 {
            let mut dry = rec(&format!("dry{i}"), 1, "low", "unknown", (0, 0, 0, 0));
            dry.runner = if i == 0 {
                " Dry-Run ".to_string()
            } else {
                "dry-run".to_string()
            };
            records.insert(i * 2, dry);
        }
        let with_dry_runs = report(&records, Some(&PRICES));
        assert_eq!(with_dry_runs.dry_runs, 4);
        // No session ran: as tasks with zero tokens they would have lowered the
        // low row's average from 4.75M to about 2M and added four unknowns.
        let mut without = report(&sample(), Some(&PRICES));
        assert_eq!(without.dry_runs, 0);
        without.dry_runs = 4;
        assert_eq!(with_dry_runs, without);

        let text = render_report(&with_dry_runs);
        assert!(
            text.starts_with(
                "Usage report: tasks 6, attempts 9, escalations 3\nDry runs: 4 records left out, no session ran\n\n"
            ),
            "{text}"
        );
        assert_aligned(&text, 3);
        assert!(!render_report(&report(&sample(), None)).contains("Dry runs"));

        let only_dry = report(&records[..1], None);
        assert_eq!((only_dry.dry_runs, only_dry.overall.tasks), (1, 0));
        assert_eq!(serde_json::to_value(&only_dry).unwrap()["dry_runs"], 1);
    }

    #[test]
    fn a_session_that_used_no_tokens_is_still_an_attempt() {
        // A crashed session, not a dry run: it happened, so it counts.
        let records = [
            rec("A", 1, "low", "failure", (0, 0, 0, 0)),
            rec("A", 2, "medium", "unknown", (0, 0, 0, 0)),
            rec("B", 1, "top", "success", (0, 0, 0, 0)),
        ];
        let r = report(&records, Some(&PRICES));
        assert_eq!((r.overall.tasks, r.overall.attempts, r.overall.escalations), (2, 3, 1));
        assert_eq!((r.levels[0].unknown, r.levels[2].succeeded_first_attempt), (1, 1));
        assert_eq!(r.levels[0].success_rate, None);
        assert_eq!(r.overall.success_rate, Some(1.0));
        for row in r.levels.iter().chain([&r.overall]) {
            assert_eq!(
                (row.cached_share, row.avg_tokens_per_task, row.total_tokens),
                (0.0, 0.0, 0)
            );
            assert_eq!(
                (row.cost, row.avg_cost_per_task, row.retry_cost),
                (Some(0.0), Some(0.0), Some(0.0)),
                "{}",
                row.level
            );
        }
        let text = render_report(&r);
        assert!(!text.contains("NaN") && !text.contains("inf"), "{text}");
        assert_aligned(&text, 3);
    }

    #[test]
    fn a_task_escalated_twice_with_an_unfinished_last_attempt_is_unknown() {
        let records = [
            rec("C", 3, "top", "unknown", (4, 3, 1, 1)),
            rec("C", 1, "low", "failure", (1, 0, 0, 1)),
            rec("C", 2, "medium", "failure", (2, 1, 0, 1)),
        ];
        let r = report(&records, None);
        let low = &r.levels[0];
        assert_eq!((low.tasks, low.attempts, low.escalations), (1, 3, 2));
        assert_eq!(
            (
                low.succeeded_first_attempt,
                low.succeeded_after_escalation,
                low.failed,
                low.unknown
            ),
            (0, 0, 0, 1),
            "two failed attempts do not make the task failed while the third is open"
        );
        assert_eq!(low.success_rate, None);
        assert_eq!(low.tokens, usage(7, 4, 1, 3));
        assert_eq!(low.retry, usage(3, 1, 0, 2), "both superseded attempts, not the last");
        assert_eq!((r.levels[1].tasks, r.levels[2].tasks), (0, 0));
    }

    #[test]
    fn cached_share_stays_within_zero_to_one() {
        // More cached than input: a runner that reports the two apart, or a
        // damaged record. 900% would be nonsense.
        let r = report(&[rec("A", 1, "low", "success", (100, 900, 0, 10))], Some(&PRICES));
        assert_eq!(r.overall.cached_share, 1.0);
        assert_eq!(r.overall.uncached_input_tokens, 0);
        assert!(
            close(r.overall.cost.unwrap(), 0.15),
            "100k cached at 0.5 + 10k output at 10"
        );
        assert!(render_report(&r).contains("  100.0%  "));

        let no_input = report(&[rec("A", 1, "low", "success", (0, 900, 0, 0))], None);
        assert_eq!(no_input.overall.cached_share, 0.0);
    }

    #[test]
    fn negative_counts_in_a_record_are_not_subtracted() {
        let records = [
            rec("A", 1, "low", "success", (-100, -900, -5, -10)),
            rec("B", 1, "low", "success", (50, 10, 0, 5)),
        ];
        let r = report(&records, Some(&PRICES));
        assert_eq!(r.overall.tokens, usage(50, 10, 0, 5));
        assert_eq!(r.overall.uncached_input_tokens, 40_000);
        assert_eq!(r.overall.total_tokens, 55_000);
        assert!(close(r.overall.cached_share, 0.2));
        assert!(close(r.overall.avg_tokens_per_task, 27_500.0));
        let text = render_report(&r);
        let negative = text
            .split_whitespace()
            .find(|word| word.starts_with('-') && word.len() > 1);
        assert_eq!(negative, None, "{text}");
    }

    #[test]
    fn saturated_counts_render_aligned_and_do_not_panic() {
        let max = i64::MAX / 1000;
        let mut first = rec("A", 1, "low", "success", (max, max, max, max));
        first.started_at = i64::MAX;
        first.attempt = i64::MAX;
        let mut second = rec("A", 1, "medium", "failure", (max, 5, 5, max));
        second.attempt = i64::MIN;
        let r = report(
            &[first, second, rec("B", 1, "bogus", "success", (max, 0, 0, max))],
            Some(&PRICES),
        );
        assert_eq!(r.overall.total_tokens, i64::MAX);
        assert_eq!(r.overall.tokens.input_tokens, i64::MAX);
        assert_eq!(
            (r.levels[1].tasks, r.levels[1].attempts),
            (1, 2),
            "the lowest attempt number routes the task"
        );
        assert!(r.overall.cost.unwrap().is_finite());
        let text = render_report(&r);
        assert!(text.contains("9,223,372,036,854,775,807"), "{text}");
        assert_aligned(&text, 3);
    }

    #[test]
    fn free_text_of_a_record_never_reaches_the_tables() {
        let plain = sample();
        let mut wordy = sample();
        for (i, rec) in wordy.iter_mut().enumerate() {
            rec.task = format!(
                "{}\n\tsecond line | with a pipe, a wide \u{6f22}\u{5b57} and a tab {i}",
                "long task text ".repeat(400)
            );
            rec.reason = "reason\r\nwith a line break".repeat(50);
            rec.model = "a-model-name-that-is-much-wider-than-any-column".to_string();
            rec.session_id = format!("session-{i}");
            rec.files = vec!["src/a path with spaces.rs".to_string(); 30];
            rec.services = vec!["svc".to_string(); 30];
        }
        let text = render_report(&report(&wordy, Some(&PRICES)));
        assert_eq!(text, render_report(&report(&plain, Some(&PRICES))));
        assert_eq!(text, RENDERED_WITH_PRICES);
        assert_aligned(&text, 3);
    }

    #[test]
    fn same_report_renders_the_same_text_every_time() {
        let mut records = sample();
        records.push(rec("G", 1, "experimental", "success", (10, 5, 1, 2)));
        let first = render_report(&report(&records, Some(&PRICES)));
        for shift in 1..records.len() {
            records.rotate_left(shift);
            assert_eq!(
                render_report(&report(&records, Some(&PRICES))),
                first,
                "rotated by {shift}"
            );
        }
        records.reverse();
        assert_eq!(render_report(&report(&records, Some(&PRICES))), first);
        assert_aligned(&first, 3);
    }

    #[test]
    fn without_prices_no_amount_of_money_is_rendered_or_serialized() {
        let r = report(&sample(), None);
        let text = render_report(&r);
        // Every number in the tables is a count or a percentage: no decimal amount.
        let tables = text.split("\nCost:").next().unwrap();
        for word in tables.split_whitespace() {
            assert!(!word.contains('.') || word.ends_with('%'), "{word:?} in\n{text}");
        }
        assert_eq!(text.matches("Cost").count(), 1, "{text}");
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("cost") && !json.contains("price"), "{json}");
    }

    #[test]
    fn json_has_token_classes_inline_and_cost_only_with_prices() {
        let plain = serde_json::to_value(report(&sample(), None)).unwrap();
        let low = &plain["levels"][0];
        assert_eq!(low["level"], "low");
        assert_eq!(low["tasks"], 3);
        assert_eq!(low["input_tokens"], 13_000_000);
        assert_eq!(low["cached_input_tokens"], 6_400_000);
        assert_eq!(low["reasoning_tokens"], 760_000);
        assert_eq!(low["output_tokens"], 1_250_000);
        assert_eq!(low["total_tokens"], 14_250_000);
        assert_eq!(low["retry"]["input_tokens"], 5_000_000);
        assert_eq!(low["retry_tokens"], 5_350_000);
        assert_eq!(plain["levels"][2]["success_rate"], serde_json::Value::Null);
        for row in [low, &plain["overall"]] {
            let keys = row.as_object().unwrap();
            assert!(
                !keys.contains_key("cost") && !keys.contains_key("avg_cost_per_task"),
                "{row}"
            );
            assert!(!keys.contains_key("retry_cost"), "{row}");
        }
        assert!(!plain.as_object().unwrap().contains_key("prices"));

        let priced = serde_json::to_value(report(&sample(), Some(&PRICES))).unwrap();
        assert!((priced["overall"]["cost"].as_f64().unwrap() - 56.25).abs() < 1e-9);
        assert!((priced["overall"]["retry_cost"].as_f64().unwrap() - 11.25).abs() < 1e-9);
        assert_eq!(priced["prices"]["output_per_mtok"], 10.0);
    }

    const RENDERED_WITH_PRICES: &str = "\
Usage report: tasks 6, attempts 9, escalations 3

Tasks, by the level of the first attempt
level   tasks  attempts  escalations  ok first  ok escalated  failed  unknown  success
low         3         6            3         1             1       1        0    66.7%
medium      2         2            0         1             0       1        0    50.0%
top         1         1            0         0             0       0        1        -
all         6         9            3         2             1       2        1    60.0%

Tokens (cached is part of input, reasoning is part of output, total = input + output)
level        input      cached  cached %     output  reasoning       total   avg/task      retry
low     13,000,000   6,400,000     49.2%  1,250,000    760,000  14,250,000  4,750,000  5,350,000
medium   5,600,000   2,500,000     44.6%    290,000    270,000   5,890,000  2,945,000          0
top      8,000,000   6,000,000     75.0%  1,000,000    800,000   9,000,000  9,000,000          0
all     26,600,000  14,900,000     56.0%  2,540,000  1,830,000  29,140,000  4,856,667  5,350,000

Cost (per million tokens: input 2, cached input 0.5, output 10)
level      cost  cost/task  retry cost
low     28.9000     9.6333     11.2500
medium  10.3500     5.1750      0.0000
top     17.0000    17.0000      0.0000
all     56.2500     9.3750     11.2500

rows hold whole tasks: a retry at a higher level is counted in the row of the level the task started at
ok first: succeeded on the first attempt; ok escalated: succeeded after a retry one level higher
success: succeeded over succeeded plus failed; unknown outcomes are left out
retry: tokens of attempts that were followed by an escalation
";

    #[test]
    fn renders_aligned_tables_with_cost() {
        let text = render_report(&report(&sample(), Some(&PRICES)));
        assert_eq!(text, RENDERED_WITH_PRICES, "\n{text}");
    }

    #[test]
    fn renders_no_cost_table_without_prices() {
        let text = render_report(&report(&sample(), None));
        let (before_cost, _) = RENDERED_WITH_PRICES.split_once("\nCost (").unwrap();
        let expected = format!("{before_cost}\nCost: not shown, no [prices] table in lge.toml\n{LEGEND}");
        assert_eq!(text, expected, "\n{text}");
        assert!(!text.contains("cost/task"));
    }

    #[test]
    fn rendering_is_deterministic_and_has_no_trailing_spaces() {
        let mut shuffled = sample();
        shuffled.rotate_left(4);
        let text = render_report(&report(&sample(), Some(&PRICES)));
        assert_eq!(text, render_report(&report(&shuffled, Some(&PRICES))));
        for line in text.lines() {
            assert_eq!(line, line.trim_end(), "trailing space in {line:?}");
        }
    }

    #[test]
    fn table_rows_are_as_wide_as_their_header() {
        let mut records = sample();
        records.push(rec(
            "H",
            1,
            "low",
            "success",
            (987_654_321, 123_456_789, 1_000, 55_555_555),
        ));
        records.push(rec("I", 1, "mystery", "unknown", (1, 0, 0, 1)));
        let text = render_report(&report(&records, Some(&PRICES)));
        assert!(text.contains("\nother "), "{text}");
        for block in text.split("\n\n").skip(1).take(3) {
            let mut lines = block.lines().skip(1);
            let header = lines.next().unwrap();
            for line in lines {
                assert_eq!(
                    line.chars().count(),
                    header.chars().count(),
                    "{line:?} against {header:?}"
                );
            }
        }
    }

    #[test]
    fn renders_an_empty_report() {
        let text = render_report(&report(&[], None));
        assert!(
            text.starts_with("Usage report: tasks 0, attempts 0, escalations 0\n"),
            "{text}"
        );
        assert!(
            text.contains("\nall         0         0            0         0             0       0        0        -\n")
        );
        assert!(!text.contains("NaN"));
    }

    #[test]
    fn numbers_are_grouped_in_threes() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1_000), "1,000");
        assert_eq!(grouped(12_345_678), "12,345,678");
        assert_eq!(grouped(-1_234), "-1,234");
        assert_eq!(grouped(i64::MIN), "-9,223,372,036,854,775,808");
        assert_eq!(percent(0.4923), "49.2%");
        assert_eq!(money(Some(1.0)), "1.0000");
        assert_eq!(money(None), "-");
    }
}
