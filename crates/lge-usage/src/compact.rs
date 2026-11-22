//! A what-if for Codex's automatic compaction: what a session's observed work
//! would have cost with a different `model_auto_compact_token_limit`.
//!
//! An estimate, never a measured saving. It replays the session's own
//! requests (output tokens as they were, growth of the history as it was)
//! against a model of Codex's rule: after a response whose total (input plus
//! output) reaches the limit, the next request is preceded by a request for a
//! summary over the whole history, and the history restarts at a size the
//! session itself shows (or a rule gives). Codex then works from a summary, a
//! behaviour change this cannot model. The model is first run at the session's
//! own limit and set beside the measured totals, so its error is visible, and
//! every change is reported against that run, not against the measured totals.
//! Exact on a clean synthetic series, within 5% on a noisy one; a session whose
//! own baseline is off by more than 10% is marked low confidence.
//!
//! Pure functions over [`RolloutSeries`]; the only file read is `config.toml`,
//! and nothing is ever written.
use crate::codex::{ReqKind, ReqUsage, RolloutSeries};
use serde::Serialize;
use std::path::Path;

/// Weights of the cost model: uncached input 1, cached input 0.1, output 8.
pub const CACHED_WEIGHT: f64 = 0.1;
pub const OUTPUT_WEIGHT: f64 = 8.0;
/// A session is long from this many requests and this mean input per request.
pub const LONG_SESSION_REQUESTS: u64 = 50;
pub const LONG_SESSION_MEAN_INPUT: i64 = 150_000;
/// Fewer work requests than this make the model low confidence.
pub const MIN_REQUESTS_FOR_CONFIDENCE: usize = 200;
/// The summary's size when the session never compacted (an assumption, flagged).
pub const SUMMARY_PRIOR_TOKENS: i64 = 4_000;
/// Retained user messages after a compaction: remote (OpenAI, Azure) and local.
pub const RETAINED_REMOTE_TOKENS: i64 = 64_000;
pub const RETAINED_LOCAL_TOKENS: i64 = 20_000;
/// The usable window is this share of the model's window.
const USABLE_SHARE: f64 = 0.95;
/// The automatic limit is this share of the model's window.
const AUTO_SHARE: f64 = 0.9;
/// The window assumed when a rollout never said (a 272,000-token model).
const DEFAULT_USABLE_WINDOW: i64 = 258_400;

/// What `config.toml` says about compaction.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct CompactConfig {
    pub limit: Option<i64>,
    /// `model_auto_compact_token_limit_scope = "body_after_prefix"`.
    pub body_after_prefix: bool,
}

/// Reads `model_auto_compact_token_limit` and its scope from
/// `<home>/config.toml`, read-only; a missing or unreadable file gives none.
pub fn read_config(codex_home: &Path) -> CompactConfig {
    let Ok(text) = std::fs::read_to_string(codex_home.join("config.toml")) else {
        return CompactConfig::default();
    };
    let Ok(table) = text.parse::<toml::Table>() else {
        return CompactConfig::default();
    };
    CompactConfig {
        limit: table
            .get("model_auto_compact_token_limit")
            .and_then(toml::Value::as_integer),
        body_after_prefix: table
            .get("model_auto_compact_token_limit_scope")
            .and_then(toml::Value::as_str)
            == Some("body_after_prefix"),
    }
}

/// Where the limit used came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitSource {
    Config,
    /// The smallest total just before a compaction: approximate, within about
    /// one growth step, and possibly low by the unreported tokens Codex adds.
    Observed,
    Default,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ParamSource {
    Observed,
    Estimate,
    Rule,
    Prior,
}

/// Tokens of one replay.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Run {
    pub compactions: u64,
    /// Work requests plus compaction requests.
    pub requests: u64,
    pub input: i64,
    pub cached: i64,
    pub output: i64,
    /// Of the above, what the compaction requests read and wrote.
    pub compaction_input: i64,
    pub compaction_output: i64,
    pub cost: f64,
    pub mean_input: i64,
    pub max_input: i64,
}

impl Run {
    fn add(&mut self, input: i64, cached: i64, output: i64, compaction: bool) {
        self.requests += 1;
        self.input += input;
        self.cached += cached;
        self.output += output;
        self.max_input = self.max_input.max(input);
        if compaction {
            self.compactions += 1;
            self.compaction_input += input;
            self.compaction_output += output;
        }
    }

    fn finish(mut self) -> Run {
        self.cost = cost(self.input, self.cached, self.output);
        self.mean_input = if self.requests > 0 {
            self.input / self.requests as i64
        } else {
            0
        };
        self
    }
}

/// Uncached-input equivalents: cached input at a tenth, output at eight times.
pub fn cost(input: i64, cached: i64, output: i64) -> f64 {
    let cached = cached.clamp(0, input.max(0));
    CACHED_WEIGHT * cached as f64 + (input - cached) as f64 + OUTPUT_WEIGHT * output as f64
}

/// How far the model at the session's own limit is from what was measured, in percent.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ErrorPct {
    pub input: f64,
    pub cached: f64,
    pub output: f64,
    pub cost: f64,
    pub compactions: f64,
}

/// What the session itself gave the model.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Params {
    pub growth_median: i64,
    pub tail_median: i64,
    pub static_cached: i64,
    pub summary_tokens: i64,
    pub summary_source: ParamSource,
    pub post_size: i64,
    pub post_source: ParamSource,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Candidate {
    pub limit: Option<i64>,
    /// The limit Codex would apply (capped at 90% of the model's window).
    pub applied: Option<i64>,
    pub capped: bool,
    pub run: Run,
    /// Percent change of the cost against the model at the session's own limit; none for a thrash.
    pub delta_pct: Option<f64>,
    pub thrash: bool,
    pub flags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Analysis {
    pub session_id: String,
    pub model: String,
    pub work_requests: usize,
    pub limit: i64,
    pub limit_source: LimitSource,
    pub window: i64,
    pub observed_compactions: usize,
    pub measured: Run,
    pub model_run: Run,
    pub error_pct: ErrorPct,
    pub params: Params,
    pub candidates: Vec<Candidate>,
    pub body_after_prefix_note: bool,
    pub confidence: &'static str,
}

fn median(values: &[i64]) -> i64 {
    if values.is_empty() {
        return 0;
    }
    let mut v = values.to_vec();
    v.sort_unstable();
    let mid = v.len() / 2;
    if v.len().is_multiple_of(2) {
        (v[mid - 1] + v[mid]) / 2
    } else {
        v[mid]
    }
}

/// The work of a session: its regular requests, with what the model needs to
/// know about each.
struct Work {
    input: Vec<i64>,
    cached: Vec<i64>,
    output: Vec<i64>,
    /// First request of a window (the first, or the first after a compaction).
    first_of_window: Vec<bool>,
    /// A request after an idle gap: most of the input was not cached.
    cold: Vec<bool>,
    /// Growth of the history from this request to the next, when both are in one window and warm.
    growth: Vec<Option<i64>>,
}

fn work_of(requests: &[&ReqUsage]) -> Work {
    let n = requests.len();
    let mut w = Work {
        input: requests.iter().map(|r| r.input).collect(),
        cached: requests.iter().map(|r| r.cached.clamp(0, r.input)).collect(),
        output: requests.iter().map(|r| r.output).collect(),
        first_of_window: vec![false; n],
        cold: vec![false; n],
        growth: vec![None; n],
    };
    for k in 0..n {
        w.first_of_window[k] = k == 0 || requests[k].window != requests[k - 1].window;
        let uncached = w.input[k] - w.cached[k];
        w.cold[k] = !w.first_of_window[k] && 2 * uncached > w.input[k];
    }
    for k in 0..n.saturating_sub(1) {
        if !w.first_of_window[k + 1] && !w.cold[k + 1] {
            w.growth[k] = Some((w.input[k + 1] - w.input[k]).max(0));
        }
    }
    w
}

/// Everything the replay needs, and what it replays.
struct Model {
    work: Work,
    g: i64,
    tail: i64,
    static_cached: i64,
    summary: i64,
    post: i64,
    window: i64,
    rework: i64,
}

impl Model {
    /// Replays the work at `limit` (the limit Codex applies, `None`: never).
    fn run(&self, limit: Option<i64>) -> (Run, bool) {
        let w = &self.work;
        let m = w.input.len();
        let mut run = Run::default();
        let mut thrash = false;
        if m == 0 {
            return (run, false);
        }
        let mut s = w.input[0];
        let mut post = false;
        let (mut close, mut since_compaction) = (0usize, usize::MAX);
        for k in 0..m {
            let observed_uncached = w.input[k] - w.cached[k];
            // What this request had to read fresh: as observed for a warm request, the whole
            // history less the static prefix after a compaction or an idle gap, and the
            // median tail for a request that began a window in the session but does not here.
            let cold_prefix = k > 0 && (w.cold[k] || post);
            let uncached = if k == 0 {
                observed_uncached.min(s)
            } else if cold_prefix {
                (s - self.static_cached).max(0)
            } else if w.first_of_window[k] {
                self.tail.min(s)
            } else {
                observed_uncached.min(s)
            };
            let cached = s - uncached;
            run.add(s, cached, w.output[k], false);
            let total = s + w.output[k];
            if k == m - 1 {
                break;
            }
            let hit = limit.is_some_and(|l| total >= l) || total >= self.window;
            if hit && !thrash {
                // The compaction request reads the whole history, the last response included.
                let compaction_uncached = if cold_prefix {
                    (s - self.static_cached).max(0)
                } else {
                    self.tail.min(s)
                };
                run.add(total, total - compaction_uncached, self.summary, true);
                close = if since_compaction < 2 { close + 1 } else { 0 };
                since_compaction = 0;
                if close >= 2 {
                    thrash = true;
                }
                // What Codex keeps, plus what the next request adds (its fresh tail).
                s = self.post + self.tail + self.rework;
                post = true;
            } else {
                since_compaction = since_compaction.saturating_add(1);
                s += w.growth[k].unwrap_or(self.g);
                post = false;
            }
        }
        if let Some(l) = limit {
            // The history restarts so close to the limit that every request compacts again.
            if self.post + self.rework + self.g >= l {
                thrash = true;
            }
        }
        (run.finish(), thrash)
    }
}

fn pct(model: f64, measured: f64) -> f64 {
    if measured == 0.0 {
        0.0
    } else {
        (model - measured) / measured * 100.0
    }
}

/// Options of an analysis.
#[derive(Debug, Clone)]
pub struct Options {
    /// Candidate limits to replay, besides the session's own and "never".
    pub limits: Vec<i64>,
    /// Tokens added to the first request after a compaction (what Codex has to re-read).
    pub rework_tokens: i64,
    pub config: CompactConfig,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            limits: vec![100_000, 150_000, 200_000],
            rework_tokens: 0,
            config: CompactConfig::default(),
        }
    }
}

/// Analyses one session, or `None` when it has fewer than two work requests.
pub fn analyse(series: &RolloutSeries, opts: &Options) -> Option<Analysis> {
    let work_refs: Vec<&ReqUsage> = series.requests.iter().filter(|r| r.kind == ReqKind::Regular).collect();
    if work_refs.len() < 2 {
        return None;
    }
    let work = work_of(&work_refs);
    let m = work_refs.len();
    // What was measured: every request, the compaction requests included.
    let mut measured = Run::default();
    for r in &series.requests {
        measured.add(
            r.input,
            r.cached.clamp(0, r.input),
            r.output,
            r.kind == ReqKind::Compaction,
        );
    }
    measured.compactions = series.compactions.len() as u64;
    let measured = measured.finish();

    let window = series.context_window.unwrap_or(DEFAULT_USABLE_WINDOW);
    let raw_window = (window as f64 / USABLE_SHARE).round() as i64;
    let auto_cap = (raw_window as f64 * AUTO_SHARE).round() as i64;

    let growths: Vec<i64> = work.growth.iter().flatten().copied().collect();
    let g = median(&growths);
    let warm_tails: Vec<i64> = (0..m)
        .filter(|&k| !work.first_of_window[k] && !work.cold[k])
        .map(|k| work.input[k] - work.cached[k])
        .collect();
    let tail = median(&warm_tails);
    // The cached prefix of the first request after a compaction.
    let after: Vec<usize> = (1..m).filter(|&k| work.first_of_window[k]).collect();
    let static_cached = if after.is_empty() {
        work.cached.iter().copied().filter(|c| *c > 0).min().unwrap_or(0)
    } else {
        median(&after.iter().map(|&k| work.cached[k]).collect::<Vec<_>>())
    };
    let summaries: Vec<i64> = series
        .requests
        .iter()
        .filter(|r| r.kind == ReqKind::Compaction)
        .map(|r| r.output)
        .collect();
    let (summary, summary_source) = if summaries.is_empty() {
        (SUMMARY_PRIOR_TOKENS, ParamSource::Prior)
    } else {
        (median(&summaries), ParamSource::Observed)
    };
    let observed_post: Vec<i64> = after.iter().map(|&k| (work.input[k] - tail).max(0)).collect();
    let estimates: Vec<i64> = series.compactions.iter().filter_map(|c| c.post_estimate).collect();
    let (post, post_source) = if !observed_post.is_empty() {
        (median(&observed_post), ParamSource::Observed)
    } else if !estimates.is_empty() {
        (median(&estimates), ParamSource::Estimate)
    } else {
        let remote = matches!(series.provider.as_str(), "" | "openai" | "azure");
        let budget = if remote {
            RETAINED_REMOTE_TOKENS
        } else {
            RETAINED_LOCAL_TOKENS
        };
        (
            work.input[0] + series.user_tokens.min(budget) + summary,
            ParamSource::Rule,
        )
    };

    // The limit the session ran at.
    let (limit, limit_source) = match opts.config.limit {
        Some(l) => (
            if opts.config.body_after_prefix {
                l
            } else {
                l.min(auto_cap)
            },
            LimitSource::Config,
        ),
        None => {
            let totals: Vec<i64> = (1..m)
                .filter(|&k| work.first_of_window[k])
                .map(|k| work_refs[k - 1].total)
                .collect();
            match totals.iter().min() {
                Some(t) => (*t, LimitSource::Observed),
                None => (auto_cap, LimitSource::Default),
            }
        }
    };
    let model = Model {
        work,
        g,
        tail,
        static_cached,
        summary,
        post,
        window,
        rework: opts.rework_tokens.max(0),
    };
    let (baseline, _) = model.run(Some(limit));
    let error_pct = ErrorPct {
        input: pct(baseline.input as f64, measured.input as f64),
        cached: pct(baseline.cached as f64, measured.cached as f64),
        output: pct(baseline.output as f64, measured.output as f64),
        cost: pct(baseline.cost, measured.cost),
        compactions: pct(baseline.compactions as f64, measured.compactions as f64),
    };
    let candidate = |limit: Option<i64>, own: bool| -> Candidate {
        let applied = limit.map(|l| {
            if opts.config.body_after_prefix {
                l
            } else {
                l.min(auto_cap)
            }
        });
        let capped = limit.zip(applied).is_some_and(|(l, a)| a < l);
        let (run, thrash) = model.run(applied);
        let mut flags = Vec::new();
        if thrash {
            flags.push("thrash".to_string());
        }
        if capped {
            flags.push("capped".to_string());
        }
        if summary_source == ParamSource::Prior || post_source == ParamSource::Rule {
            flags.push("prior".to_string());
        }
        if own {
            flags.push("session limit".to_string());
        }
        let delta = (!thrash && baseline.cost > 0.0).then(|| pct(run.cost, baseline.cost));
        Candidate {
            limit,
            applied,
            capped,
            run,
            delta_pct: delta,
            thrash,
            flags,
        }
    };
    let mut candidates: Vec<Candidate> = opts.limits.iter().map(|l| candidate(Some(*l), false)).collect();
    candidates.push(candidate(Some(limit), true));
    candidates.push(candidate(None, false));
    let low = error_pct.cost.abs() > 10.0
        || (limit_source == LimitSource::Default && series.compactions.is_empty())
        || m < MIN_REQUESTS_FOR_CONFIDENCE;
    Some(Analysis {
        session_id: series.session_id.clone(),
        model: series.model.clone(),
        work_requests: m,
        limit,
        limit_source,
        window,
        observed_compactions: series.compactions.len(),
        measured,
        model_run: baseline,
        error_pct,
        params: Params {
            growth_median: g,
            tail_median: tail,
            static_cached,
            summary_tokens: summary,
            summary_source,
            post_size: post,
            post_source,
        },
        candidates,
        body_after_prefix_note: opts.config.body_after_prefix,
        confidence: if low { "low" } else { "ok" },
    })
}

/// A session is long when it has at least [`LONG_SESSION_REQUESTS`] work
/// requests and a mean input per request above [`LONG_SESSION_MEAN_INPUT`].
pub fn is_long(series: &RolloutSeries) -> bool {
    let work: Vec<&ReqUsage> = series.requests.iter().filter(|r| r.kind == ReqKind::Regular).collect();
    !work.is_empty()
        && work.len() as u64 >= LONG_SESSION_REQUESTS
        && work.iter().map(|r| r.input).sum::<i64>() / work.len() as i64 > LONG_SESSION_MEAN_INPUT
}

/// The caveat the page and the command print with every figure.
pub const CAVEAT: &str = "An estimate, not a measurement: the work is replayed with the output tokens and the growth of each request as they were, and Codex would work from a summary after each compaction, which this cannot model. Changes are against the model at your current limit, so the model's own error cancels to first order. Exact on a clean synthetic series, within 5% on a noisy one; a limit inferred from the compactions seen is approximate (within about one growth step, possibly low by the tokens Codex adds that the last response did not report). lge never changes your configuration: set model_auto_compact_token_limit in ~/.codex/config.toml yourself if you decide to.";

/// What the command and the page show: the analysis of every long session of
/// the last days, with a total over them.
#[derive(Debug, Clone, Serialize)]
pub struct WhatIf {
    pub generated_at: i64,
    pub days: i64,
    pub weights: serde_json::Value,
    pub sessions: Vec<Analysis>,
    pub total: Option<Total>,
    /// Rollouts read, and how many of them were long.
    pub rollouts_read: usize,
    pub explain: Vec<Explain>,
    pub caveats: Vec<&'static str>,
}

/// Sums over the long sessions, candidates by position.
#[derive(Debug, Clone, Serialize)]
pub struct Total {
    pub measured: Run,
    pub model_run: Run,
    pub candidates: Vec<Candidate>,
}

/// What one rollout held (`--explain`): whether the reader matched.
#[derive(Debug, Clone, Serialize)]
pub struct Explain {
    pub session_id: String,
    pub requests: usize,
    pub compacted_lines: usize,
    pub context_compacted_events: usize,
    pub recompute_events: usize,
    pub usage_records: usize,
    pub compaction_requests: usize,
}

fn add_run(a: &mut Run, b: &Run) {
    a.compactions += b.compactions;
    a.requests += b.requests;
    a.input += b.input;
    a.cached += b.cached;
    a.output += b.output;
    a.compaction_input += b.compaction_input;
    a.compaction_output += b.compaction_output;
    a.cost += b.cost;
    a.max_input = a.max_input.max(b.max_input);
}

fn finish_mean(r: &mut Run) {
    r.mean_input = if r.requests > 0 { r.input / r.requests as i64 } else { 0 };
}

/// Reads the rollouts of the last `days` days under `codex_home` (read only),
/// keeps the long sessions (`session` narrows by id prefix) and analyses them.
pub fn whatif(codex_home: &Path, days: i64, session: Option<&str>, opts: &Options) -> WhatIf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let files = crate::codex::rollout_files(codex_home, now - days * 86_400);
    let mut sessions = Vec::new();
    let mut explain = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut read = 0;
    for f in &files {
        let Ok(series) = crate::codex::rollout_series(f) else {
            continue;
        };
        if series.background.is_some() || series.requests.is_empty() || !seen.insert(series.session_id.clone()) {
            continue;
        }
        read += 1;
        if session.is_some_and(|p| !series.session_id.starts_with(p)) || !is_long(&series) {
            continue;
        }
        explain.push(Explain {
            session_id: series.session_id.clone(),
            requests: series.requests.len(),
            compacted_lines: series.compacted_lines,
            context_compacted_events: series.context_compacted_events,
            recompute_events: series.recompute_events,
            usage_records: series.usage_records,
            compaction_requests: series.requests.iter().filter(|r| r.kind == ReqKind::Compaction).count(),
        });
        if let Some(a) = analyse(&series, opts) {
            sessions.push(a);
        }
    }
    let total = sessions.first().map(|first| {
        let mut t = Total {
            measured: Run::default(),
            model_run: Run::default(),
            candidates: first.candidates.clone(),
        };
        for c in &mut t.candidates {
            c.run = Run::default();
            c.flags.clear();
            c.delta_pct = None;
        }
        for a in &sessions {
            add_run(&mut t.measured, &a.measured);
            add_run(&mut t.model_run, &a.model_run);
            for (c, d) in t.candidates.iter_mut().zip(&a.candidates) {
                add_run(&mut c.run, &d.run);
                c.thrash |= d.thrash;
            }
        }
        finish_mean(&mut t.measured);
        finish_mean(&mut t.model_run);
        for c in &mut t.candidates {
            finish_mean(&mut c.run);
            c.delta_pct = (!c.thrash && t.model_run.cost > 0.0).then(|| pct(c.run.cost, t.model_run.cost));
        }
        t
    });
    WhatIf {
        generated_at: now,
        days,
        weights: serde_json::json!({"cached": CACHED_WEIGHT, "uncached": 1.0, "output": OUTPUT_WEIGHT}),
        sessions,
        total,
        rollouts_read: read,
        explain,
        caveats: vec![CAVEAT],
    }
}

/// The text of the command.
pub fn render(w: &WhatIf, explain: bool) -> String {
    use std::fmt::Write;
    let mut o = String::new();
    if w.sessions.is_empty() {
        let _ = writeln!(o, "no session with enough requests (at least {LONG_SESSION_REQUESTS} requests and more than {LONG_SESSION_MEAN_INPUT} input tokens each on average) in the last {} days ({} rollouts read)", w.days, w.rollouts_read);
    }
    let row = |o: &mut String, name: &str, r: &Run, delta: Option<f64>, flags: &str| {
        let _ = writeln!(
            o,
            "  {:<22} compactions {:>4}  requests {:>6}  mean input {:>9}  input {:>13}  cost {:>10.0}k  {}  {}",
            name,
            r.compactions,
            r.requests,
            r.mean_input,
            r.input,
            r.cost / 1000.0,
            delta.map_or("             ".to_string(), |d| format!("{d:>+7.1}% vs model")),
            flags
        );
    };
    for a in &w.sessions {
        let _ = writeln!(
            o,
            "session {} model {} requests {} limit {} ({:?}) compactions seen {} confidence {}",
            a.session_id.chars().take(8).collect::<String>(),
            a.model,
            a.work_requests,
            a.limit,
            a.limit_source,
            a.observed_compactions,
            a.confidence
        );
        row(&mut o, "measured", &a.measured, None, "");
        row(
            &mut o,
            &format!("model at {}", a.limit),
            &a.model_run,
            None,
            &format!(
                "(model error: cost {:+.1}%, input {:+.1}%, compactions {:+.1}%)",
                a.error_pct.cost, a.error_pct.input, a.error_pct.compactions
            ),
        );
        for c in &a.candidates {
            let name = c.limit.map_or("never compact".to_string(), |l| format!("limit {l}"));
            row(&mut o, &name, &c.run, c.delta_pct, &c.flags.join(", "));
        }
    }
    if let Some(t) = &w.total {
        let _ = writeln!(o, "TOTAL over {} sessions", w.sessions.len());
        row(&mut o, "measured", &t.measured, None, "");
        row(&mut o, "model", &t.model_run, None, "");
        for c in &t.candidates {
            let name = c.limit.map_or("never compact".to_string(), |l| format!("limit {l}"));
            row(&mut o, &name, &c.run, c.delta_pct, if c.thrash { "thrash" } else { "" });
        }
    }
    if explain {
        for e in &w.explain {
            let _ = writeln!(o, "explain {}: {} requests, {} compacted lines, {} context_compacted events, {} recompute events, {} usage records, {} compaction requests", e.session_id, e.requests, e.compacted_lines, e.context_compacted_events, e.recompute_events, e.usage_records, e.compaction_requests);
        }
    }
    let _ = writeln!(o, "\n{CAVEAT}");
    o
}

#[cfg(test)]
mod tests;
