use super::*;
use crate::codex::rollout_series;
use serde_json::json;

#[derive(Clone, Copy, PartialEq)]
enum Shape {
    /// OpenAI provider: a `token_usage_record` for every response, the compaction's
    /// named by the `compacted` line; no `token_count` for the compaction request.
    V2,
    /// An older Codex: `token_count` events only; the compaction request has one.
    Local,
}

struct Spec {
    limit: i64,
    start: i64,
    tail: i64,
    summary: i64,
    post: i64,
    static_cached: i64,
    work: usize,
    /// Growth per request, output per request, as functions of a counter.
    growth: Box<dyn Fn(usize) -> i64>,
    output: Box<dyn Fn(usize) -> i64>,
    cold_every: Option<usize>,
    shape: Shape,
}

fn clean(shape: Shape, work: usize) -> Spec {
    Spec {
        limit: 100_000,
        start: 30_000,
        tail: 2_000,
        summary: 3_000,
        post: 30_000,
        static_cached: 15_000,
        work,
        growth: Box::new(|_| 5_000),
        output: Box::new(|_| 500),
        cold_every: None,
        shape,
    }
}

/// A small deterministic generator for the noisy cases.
fn noise(seed: u64, n: usize, low: i64, high: i64) -> Vec<i64> {
    let mut x = seed;
    (0..n)
        .map(|_| {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            low + ((x >> 33) as i64 % (high - low + 1))
        })
        .collect()
}

/// A rollout the way Codex writes one, with the compactions the spec's own rule makes.
/// Returns the text and the number of compactions it holds.
fn synthetic_rollout(spec: &Spec) -> (String, usize) {
    let mut lines: Vec<String> = Vec::new();
    let mut push = |v: serde_json::Value| lines.push(v.to_string());
    push(json!({ "type": "session_meta", "payload": { "id": "sess-1", "source": "cli", "model_provider": "openai" } }));
    push(json!({ "type": "turn_context", "payload": { "model": "gpt-test" } }));
    push(json!({ "type": "event_msg", "payload": { "type": "task_started" } }));
    let mut n = 0;
    let usage = |lines: &mut Vec<String>, id: &str, input: i64, cached: i64, output: i64, count: bool, record: bool| {
        let u = json!({ "input_tokens": input, "cached_input_tokens": cached, "output_tokens": output, "reasoning_output_tokens": 0, "total_tokens": input + output });
        if record {
            lines.push(
                json!({ "type": "token_usage_record", "payload": { "response_id": id, "usage": u } }).to_string(),
            );
        }
        if count {
            lines.push(json!({ "type": "event_msg", "payload": { "type": "token_count", "info": { "last_token_usage": u, "total_token_usage": u, "model_context_window": 258_400 } } }).to_string());
        }
    };
    let mut s = spec.start;
    let mut post = false;
    let mut compactions = 0;
    let mut tail = vec![];
    for k in 0..spec.work {
        let cold = spec.cold_every.is_some_and(|e| k > 0 && k % e == 0) && !post;
        let cached = if k == 0 || cold {
            0
        } else if post {
            spec.static_cached
        } else {
            s - spec.tail
        };
        let out = (spec.output)(k);
        n += 1;
        let v2 = spec.shape == Shape::V2;
        usage(&mut lines, &format!("r{n}"), s, cached, out, true, v2);
        tail.push(s + out);
        if k + 1 == spec.work {
            break;
        }
        if s + out >= spec.limit {
            let hist = s + out;
            compactions += 1;
            let id = format!("c{compactions}");
            usage(&mut lines, &id, hist, hist - spec.tail, spec.summary, !v2, v2);
            let payload = if v2 {
                json!({ "compaction_response_id": id, "message": "", "replacement_history": [] })
            } else {
                json!({ "message": "" })
            };
            lines.push(json!({ "type": "compacted", "payload": payload }).to_string());
            lines.push(json!({ "type": "event_msg", "payload": { "type": "token_count", "info": { "last_token_usage": { "input_tokens": 0, "cached_input_tokens": 0, "output_tokens": 0, "total_tokens": spec.post }, "model_context_window": 258_400 } } }).to_string());
            s = spec.post + spec.tail;
            post = true;
        } else {
            s += (spec.growth)(k);
            post = false;
        }
    }
    (lines.join("\n") + "\n", compactions)
}

fn series_of(spec: &Spec) -> (RolloutSeries, usize) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rollout-2026-01-01T00-00-00-sess-1.jsonl");
    let (text, n) = synthetic_rollout(spec);
    std::fs::write(&path, text).unwrap();
    (rollout_series(&path).unwrap(), n)
}

fn within(model: f64, measured: f64, percent: f64) -> bool {
    measured == 0.0 || ((model - measured) / measured).abs() * 100.0 <= percent
}

fn close_to_measured(a: &Analysis, percent: f64) {
    assert_eq!(a.model_run.compactions, a.measured.compactions, "{a:#?}");
    assert_eq!(a.model_run.requests, a.measured.requests);
    for (name, m, e) in [
        ("input", a.model_run.input as f64, a.measured.input as f64),
        ("cached", a.model_run.cached as f64, a.measured.cached as f64),
        ("output", a.model_run.output as f64, a.measured.output as f64),
        ("cost", a.model_run.cost, a.measured.cost),
    ] {
        assert!(within(m, e, percent), "{name}: model {m} against measured {e}");
    }
}

#[test]
fn compact_whatif_at_the_actual_limit_reproduces_a_synthetic_session_within_one_percent() {
    for shape in [Shape::V2, Shape::Local] {
        let spec = clean(shape, 200);
        let (series, expected) = series_of(&spec);
        assert!(expected > 5);
        assert_eq!(series.compactions.len(), expected);
        assert_eq!(
            series.requests.iter().filter(|r| r.kind == ReqKind::Regular).count(),
            200
        );
        assert_eq!(
            series.requests.iter().filter(|r| r.kind == ReqKind::Compaction).count(),
            expected
        );
        let a = analyse(&series, &Options::default()).unwrap();
        // The limit read from the series: within one growth step, and said to be observed.
        assert_eq!(a.limit_source, LimitSource::Observed);
        assert!((spec.limit..=spec.limit + 5_500).contains(&a.limit), "{}", a.limit);
        close_to_measured(&a, 1.0);
        // At the exact limit, the same.
        let exact = Options {
            config: CompactConfig {
                limit: Some(spec.limit),
                body_after_prefix: false,
            },
            ..Options::default()
        };
        let b = analyse(&series, &exact).unwrap();
        assert_eq!(b.limit_source, LimitSource::Config);
        close_to_measured(&b, 1.0);
        assert!(within(a.model_run.cost, b.model_run.cost, 1.0));
        assert_eq!(a.params.post_size, spec.post);
        assert_eq!(a.params.summary_tokens, spec.summary);
        assert_eq!(a.params.static_cached, spec.static_cached);
    }
}

#[test]
fn compact_whatif_at_the_actual_limit_reproduces_a_noisy_synthetic_session_within_five_percent() {
    for shape in [Shape::V2, Shape::Local] {
        let growth = noise(7, 400, 2_000, 9_000);
        let output = noise(11, 400, 200, 1_500);
        let spec = Spec {
            growth: Box::new(move |k| growth[k]),
            output: Box::new(move |k| output[k]),
            cold_every: Some(40),
            ..clean(shape, 400)
        };
        let (series, expected) = series_of(&spec);
        let a = analyse(&series, &Options::default()).unwrap();
        assert!(
            a.model_run.compactions.abs_diff(expected as u64) <= 1,
            "{} against {expected}",
            a.model_run.compactions
        );
        assert_eq!(
            a.model_run.output - a.model_run.compaction_output,
            a.measured.output - a.measured.compaction_output,
            "work output is exact"
        );
        assert!(within(a.model_run.input as f64, a.measured.input as f64, 5.0), "{a:#?}");
        assert!(within(a.model_run.cost, a.measured.cost, 5.0), "{a:#?}");
    }
}

#[test]
fn compact_whatif_on_a_session_that_never_compacted_matches_its_measured_totals_exactly() {
    for shape in [Shape::V2, Shape::Local] {
        let spec = Spec {
            limit: 10_000_000,
            start: 20_000,
            growth: Box::new(|_| 1_000),
            ..clean(shape, 150)
        };
        let (series, expected) = series_of(&spec);
        assert_eq!(expected, 0);
        let a = analyse(&series, &Options::default()).unwrap();
        assert_eq!(a.limit_source, LimitSource::Default);
        assert_eq!(a.limit, 244_800, "90% of the model's window (258,400 is 95% of it)");
        assert_eq!(a.model_run.compactions, 0);
        assert_eq!(
            (a.model_run.input, a.model_run.cached, a.model_run.output),
            (a.measured.input, a.measured.cached, a.measured.output)
        );
        assert!((a.model_run.cost - a.measured.cost).abs() < 1e-6);
        assert_eq!(a.confidence, "low", "the limit is assumed, not seen");
        // A limit above the largest total gives the uncompacted cost.
        let none = a.candidates.last().unwrap();
        assert_eq!(none.limit, None);
        let above = analyse(
            &series,
            &Options {
                limits: vec![300_000, 400_000],
                ..Options::default()
            },
        )
        .unwrap();
        assert_eq!(above.candidates[0].run, above.candidates[1].run);
        assert_eq!(above.candidates[0].run.compactions, 0);
        assert!((above.candidates[0].run.cost - a.measured.cost).abs() < 1e-6);
        assert!(above.candidates[0].capped, "capped at 90% of the window: said so");
    }
}

#[test]
fn compact_whatif_candidate_limits_never_change_the_output_tokens_it_replays() {
    let (series, _) = series_of(&clean(Shape::V2, 200));
    let a = analyse(
        &series,
        &Options {
            limits: vec![60_000, 100_000, 150_000, 200_000],
            ..Options::default()
        },
    )
    .unwrap();
    let work_output = a.measured.output - a.measured.compaction_output;
    for c in &a.candidates {
        assert_eq!(c.run.output - c.run.compaction_output, work_output, "{:?}", c.limit);
    }
}

#[test]
fn compact_whatif_reads_a_remote_compaction_that_has_only_a_usage_record_and_no_token_count() {
    let (series, expected) = series_of(&clean(Shape::V2, 60));
    assert_eq!(series.usage_records, 60 + expected);
    let marked: Vec<_> = series.compactions.iter().map(|c| c.request).collect();
    assert!(marked.iter().all(Option::is_some), "{marked:?}");
    for mark in &series.compactions {
        let r = &series.requests[mark.request.unwrap()];
        assert_eq!((r.kind, r.output), (ReqKind::Compaction, 3_000));
        assert!(r.response_id.as_deref().unwrap().starts_with('c'));
    }
    assert!(series.compactions.iter().all(|c| c.post_estimate == Some(30_000)));
}

#[test]
fn compact_whatif_a_limit_below_post_compaction_size_is_flagged_thrash_and_reports_no_saving() {
    let (series, _) = series_of(&clean(Shape::V2, 200));
    let a = analyse(
        &series,
        &Options {
            limits: vec![25_000, 150_000],
            ..Options::default()
        },
    )
    .unwrap();
    let tiny = &a.candidates[0];
    assert!(
        tiny.thrash && tiny.delta_pct.is_none() && tiny.flags.contains(&"thrash".to_string()),
        "{tiny:?}"
    );
    assert!(!a.candidates[1].thrash && a.candidates[1].delta_pct.is_some());
}

#[test]
fn compact_whatif_the_first_request_after_a_compaction_pays_for_everything_but_the_static_prefix() {
    let (series, _) = series_of(&clean(Shape::V2, 40));
    let a = analyse(&series, &Options::default()).unwrap();
    // One window of 14 requests ends in a compaction; the next request's cached part is the static prefix.
    let after: Vec<_> = series
        .requests
        .iter()
        .filter(|r| r.kind == ReqKind::Regular && r.cached == 15_000)
        .collect();
    assert!(!after.is_empty());
    let model = analyse(&series, &Options::default()).unwrap().model_run;
    assert_eq!(
        (model.input, model.compactions, model.requests),
        (a.measured.input, a.measured.compactions, a.measured.requests),
        "{a:#?}"
    );
    assert_eq!(model.cached, a.measured.cached, "the replay pays the same");
}

#[test]
fn compact_whatif_the_compaction_request_costs_a_full_history_read_plus_the_summary_output() {
    let (series, _) = series_of(&clean(Shape::V2, 100));
    let a = analyse(&series, &Options::default()).unwrap();
    let n = a.model_run.compactions as i64;
    assert!(n > 0);
    assert_eq!(
        a.model_run.compaction_output,
        3_000 * n,
        "the summary is what the compactions write"
    );
    assert!(
        a.model_run.compaction_input >= 95_000 * n,
        "each reads the whole history: {}",
        a.model_run.compaction_input
    );
}

#[test]
fn compact_whatif_cold_restarts_stay_cold_at_every_limit() {
    let spec = Spec {
        cold_every: Some(10),
        ..clean(Shape::V2, 100)
    };
    let (series, _) = series_of(&spec);
    let a = analyse(&series, &Options::default()).unwrap();
    let uncached = |r: &Run| r.input - r.cached;
    // Every limit pays the cold prefix again on the same requests: more compaction cannot make them warm.
    for c in &a.candidates {
        assert!(
            uncached(&c.run) >= 9 * (30_000 - 15_000),
            "{:?} {}",
            c.limit,
            uncached(&c.run)
        );
    }
}

#[test]
fn compact_whatif_reads_the_limit_from_config_toml_without_writing_it() {
    let home = tempfile::tempdir().unwrap();
    let text = "model_auto_compact_token_limit = 120000\n[projects]\n";
    std::fs::write(home.path().join("config.toml"), text).unwrap();
    assert_eq!(
        read_config(home.path()),
        CompactConfig {
            limit: Some(120_000),
            body_after_prefix: false
        }
    );
    std::fs::write(
        home.path().join("config.toml"),
        "model_auto_compact_token_limit = 500000\nmodel_auto_compact_token_limit_scope = \"body_after_prefix\"\n",
    )
    .unwrap();
    let config = read_config(home.path());
    assert!(config.body_after_prefix);
    assert_eq!(read_config(&home.path().join("nowhere")), CompactConfig::default());
    let (series, _) = series_of(&clean(Shape::V2, 60));
    // Under the default scope a config limit is capped at 90% of the model's window; under
    // body_after_prefix it is used as it is.
    let total = analyse(
        &series,
        &Options {
            config: CompactConfig {
                limit: Some(500_000),
                body_after_prefix: false,
            },
            ..Options::default()
        },
    )
    .unwrap();
    assert_eq!(total.limit, 244_800);
    let body = analyse(
        &series,
        &Options {
            config,
            ..Options::default()
        },
    )
    .unwrap();
    assert_eq!(body.limit, 500_000);
    assert!(body.body_after_prefix_note);
    assert_eq!(
        std::fs::read_to_string(home.path().join("config.toml"))
            .unwrap()
            .lines()
            .count(),
        2,
        "only read"
    );
}

#[test]
fn compact_whatif_infers_the_actual_limit_within_one_growth_step_and_labels_it_approximate() {
    let (series, _) = series_of(&clean(Shape::V2, 100));
    let a = analyse(&series, &Options::default()).unwrap();
    assert_eq!(a.limit_source, LimitSource::Observed);
    assert!(a.limit.abs_diff(100_000) <= 5_500, "{}", a.limit);
}

#[test]
fn compact_whatif_does_not_cap_a_config_limit_at_ninety_percent_under_body_after_prefix_scope() {
    let (series, _) = series_of(&clean(Shape::V2, 60));
    let config = CompactConfig {
        limit: Some(400_000),
        body_after_prefix: true,
    };
    let a = analyse(
        &series,
        &Options {
            limits: vec![400_000],
            config,
            ..Options::default()
        },
    )
    .unwrap();
    assert_eq!(a.limit, 400_000);
    assert!(!a.candidates[0].capped);
}

#[test]
fn rollout_series_does_not_call_the_last_regular_request_a_compaction_when_no_usage_record_matches() {
    let mut lines = vec![json!({ "type": "session_meta", "payload": { "id": "s", "source": "cli" } })];
    for (n, input) in [(1, 20_000), (2, 25_000)] {
        lines.push(json!({ "type": "token_usage_record", "payload": { "response_id": format!("r{n}"), "usage": { "input_tokens": input, "cached_input_tokens": 0, "output_tokens": 10, "total_tokens": input + 10 } } }));
        lines.push(json!({ "type": "event_msg", "payload": { "type": "token_count", "info": { "last_token_usage": { "input_tokens": input, "cached_input_tokens": 0, "output_tokens": 10, "total_tokens": input + 10 } } } }));
    }
    // A compaction that names a response no record holds.
    lines.push(json!({ "type": "compacted", "payload": { "compaction_response_id": "missing" } }));
    let text: String = lines.iter().map(|l| l.to_string() + "\n").collect();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rollout-2026-01-01T00-00-00-s.jsonl");
    std::fs::write(&path, text).unwrap();
    let series = rollout_series(&path).unwrap();
    assert_eq!(series.requests.len(), 2);
    assert!(
        series.requests.iter().all(|r| r.kind == ReqKind::Regular),
        "no adjacency guess when records exist"
    );
    assert_eq!(series.compactions.len(), 1);
    assert_eq!(series.compactions[0].request, None);
}

#[test]
fn rollout_series_keeps_the_zero_input_recompute_event_as_the_post_compaction_estimate_not_a_request() {
    let (series, expected) = series_of(&clean(Shape::Local, 30));
    assert_eq!(series.recompute_events, expected);
    assert!(series.requests.iter().all(|r| r.input > 0));
    assert!(series.compactions.iter().all(|c| c.post_estimate == Some(30_000)));
}

#[test]
fn rollout_series_counts_a_compacted_line_and_a_context_compacted_event_of_one_compaction_once() {
    let usage = |n: u32| json!({ "type": "token_usage_record", "payload": { "response_id": format!("r{n}"), "usage": { "input_tokens": 1000, "cached_input_tokens": 0, "output_tokens": 10, "total_tokens": 1010 } } });
    let lines = [
        json!({ "type": "session_meta", "payload": { "id": "s", "source": "cli" } }),
        usage(1),
        json!({ "type": "compacted", "payload": { "compaction_response_id": "r1" } }),
        json!({ "type": "event_msg", "payload": { "type": "context_compacted" } }),
        usage(2),
        // A Legacy-mode compaction with only the event.
        json!({ "type": "event_msg", "payload": { "type": "context_compacted" } }),
    ];
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rollout-2026-01-01T00-00-00-s.jsonl");
    std::fs::write(&path, lines.iter().map(|l| l.to_string() + "\n").collect::<String>()).unwrap();
    let series = rollout_series(&path).unwrap();
    assert_eq!((series.compacted_lines, series.context_compacted_events), (1, 2));
    assert_eq!(series.compactions.len(), 2, "one for the pair, one for the lone event");
}

#[test]
fn rollout_series_skips_background_threads_and_tolerates_a_cut_last_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rollout-2026-01-01T00-00-00-g.jsonl");
    let text = format!(
        "{}\n{}\n",
        json!({ "type": "session_meta", "payload": { "id": "g", "source": { "subagent": { "other": "guardian" } } } }),
        json!({ "type": "token_usage_record", "payload": { "response_id": "r1", "usage": { "input_tokens": 5, "cached_input_tokens": 0, "output_tokens": 1, "total_tokens": 6 } } })
    );
    std::fs::write(&path, text).unwrap();
    let series = rollout_series(&path).unwrap();
    assert_eq!(series.background.as_deref(), Some("guardian"));
    assert!(series.requests.is_empty());
    let cut = dir
        .path()
        .join("rollout-2026-01-01T00-00-00-00000000-0000-0000-0000-000000000001.jsonl");
    let full = json!({ "type": "token_usage_record", "payload": { "response_id": "r1", "usage": { "input_tokens": 5, "cached_input_tokens": 0, "output_tokens": 1, "total_tokens": 6 } } });
    std::fs::write(&cut, format!("not json\n{full}\n{{\"type\":\"token_usa")).unwrap();
    let series = rollout_series(&cut).unwrap();
    assert_eq!(series.requests.len(), 1);
    assert_eq!(series.session_id, "00000000-0000-0000-0000-000000000001");
}

#[test]
fn rollout_series_falls_back_to_token_count_events_when_there_are_no_usage_records() {
    let (series, expected) = series_of(&clean(Shape::Local, 40));
    assert_eq!(series.usage_records, 0);
    assert_eq!(
        series.requests.iter().filter(|r| r.kind == ReqKind::Regular).count(),
        40
    );
    assert_eq!(series.compactions.len(), expected);
    assert!(
        series.compactions.iter().all(|c| c.request.is_some()),
        "the compaction request had a token_count"
    );
    assert_eq!(series.context_window, Some(258_400));
}

#[test]
fn a_session_is_long_from_fifty_work_requests_and_a_mean_input_above_150000() {
    let long = Spec {
        start: 160_000,
        limit: 10_000_000,
        growth: Box::new(|_| 100),
        ..clean(Shape::V2, 60)
    };
    assert!(is_long(&series_of(&long).0));
    assert!(
        !is_long(&series_of(&clean(Shape::V2, 60)).0),
        "mean input is below 150,000"
    );
    let few = Spec {
        start: 160_000,
        limit: 10_000_000,
        ..clean(Shape::V2, 10)
    };
    assert!(!is_long(&series_of(&few).0));
}

fn home_with_one_long_session() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    let day = home.path().join("sessions/2026/01/01");
    std::fs::create_dir_all(&day).unwrap();
    let mut spec = clean(Shape::V2, 120);
    spec.limit = 240_000;
    spec.start = 160_000;
    spec.post = 160_000;
    spec.static_cached = 15_000;
    spec.growth = Box::new(|_| 2_000);
    let (text, _) = synthetic_rollout(&spec);
    std::fs::write(day.join("rollout-2026-01-01T00-00-00-sess-1.jsonl"), text).unwrap();
    home
}

#[test]
fn compact_whatif_lists_a_long_session_with_a_baseline_row_and_one_row_per_candidate() {
    let home = home_with_one_long_session();
    let w = whatif(home.path(), 100_000, None, &Options::default());
    assert_eq!(w.sessions.len(), 1, "{:?}", w.explain);
    let a = &w.sessions[0];
    assert_eq!(a.candidates.len(), 3 + 2, "three limits, the session's own and never");
    let text = render(&w, true);
    assert!(text.contains("measured") && text.contains("model at "), "{text}");
    assert!(
        text.contains("limit 100000") && text.contains("never compact"),
        "{text}"
    );
    assert!(
        text.contains("explain sess-1") && text.contains("An estimate, not a measurement"),
        "{text}"
    );
    let json = serde_json::to_string(&w).unwrap();
    assert!(json.contains("\"measured\"") && json.contains("\"model_run\"") && json.contains("\"candidates\""));
    assert!(w.total.is_some());
}

#[test]
fn compact_whatif_reports_nothing_when_no_session_is_long_and_leaves_the_home_untouched() {
    let home = tempfile::tempdir().unwrap();
    let day = home.path().join("sessions/2026/01/01");
    std::fs::create_dir_all(&day).unwrap();
    let (text, _) = synthetic_rollout(&clean(Shape::V2, 30));
    let file = day.join("rollout-2026-01-01T00-00-00-sess-1.jsonl");
    std::fs::write(&file, &text).unwrap();
    let w = whatif(home.path(), 100_000, None, &Options::default());
    assert!(w.sessions.is_empty() && w.total.is_none());
    assert!(render(&w, false).contains("no session with enough requests"));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), text);
    assert!(!home.path().join("config.toml").exists());
}
