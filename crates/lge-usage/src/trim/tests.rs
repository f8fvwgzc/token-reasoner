use super::*;
use serde_json::json;

/// A synthetic rollout, written the way Codex writes one (usage records and
/// `token_count` events for every response).
struct Rollout {
    lines: Vec<String>,
    requests: usize,
    calls: usize,
}

impl Rollout {
    fn new(id: &str) -> Rollout {
        let mut r = Rollout {
            lines: Vec::new(),
            requests: 0,
            calls: 0,
        };
        r.push(json!({ "type": "session_meta", "payload": { "id": id, "source": "cli", "model_provider": "openai" } }));
        r.push(json!({ "type": "turn_context", "payload": { "model": "gpt-test" } }));
        r
    }

    fn push(&mut self, v: Value) {
        self.lines.push(v.to_string());
    }

    fn turn(&mut self) -> &mut Self {
        self.push(json!({ "type": "event_msg", "payload": { "type": "task_started" } }));
        self.push(json!({ "type": "response_item", "payload": { "type": "message", "role": "user", "content": [{ "type": "input_text", "text": "next step" }] } }));
        self
    }

    /// An `exec_command` call and its output of exactly `tokens` tokens (at four bytes each).
    fn exec(&mut self, cmd: &str, tokens: usize) -> &mut Self {
        self.calls += 1;
        let id = format!("call_{}", self.calls);
        let args = json!({ "cmd": cmd }).to_string();
        self.push(json!({ "type": "response_item", "payload": { "type": "function_call", "name": "exec_command", "arguments": args, "call_id": id } }));
        self.push(json!({ "type": "response_item", "payload": { "type": "function_call_output", "call_id": id, "output": "abcd".repeat(tokens) } }));
        self
    }

    fn req(&mut self, input: i64, cached: i64, output: i64) -> &mut Self {
        self.requests += 1;
        let u = json!({ "input_tokens": input, "cached_input_tokens": cached, "output_tokens": output, "reasoning_output_tokens": 0, "total_tokens": input + output });
        let id = format!("resp_{}", self.requests);
        self.push(json!({ "type": "token_usage_record", "payload": { "response_id": id, "usage": u } }));
        self.push(json!({ "type": "event_msg", "payload": { "type": "token_count", "info": { "last_token_usage": u, "total_token_usage": u, "model_context_window": 258_400 } } }));
        self
    }

    fn compacted(&mut self) -> &mut Self {
        self.push(json!({ "type": "compacted", "payload": { "message": "", "replacement_history": [] } }));
        self
    }

    fn text(&self) -> String {
        self.lines.iter().map(|l| format!("{l}\n")).collect()
    }

    fn timeline(&self) -> Timeline {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout-2026-01-01T00-00-00-sess.jsonl");
        std::fs::write(&path, self.text()).unwrap();
        read_timeline(&path, approx_count).unwrap()
    }
}

fn policy(threshold: i64, age_turns: u32, min_output: i64, min_gain: i64) -> Policy {
    Policy {
        threshold,
        age_turns,
        min_output,
        min_gain,
    }
}

/// Turn 1: a 5,000-token read and a 100-token listing; turn 2: nothing new;
/// turn 3: a fresh 6,000-token read and a request at 120,000 input.
fn base() -> Rollout {
    let mut r = Rollout::new("sess-base");
    r.turn().exec("cat big.rs", 5_000).exec("ls", 100).req(50_000, 0, 100);
    r.turn().req(80_000, 49_000, 100);
    r.turn().exec("cat new.rs", 6_000).req(120_000, 79_000, 100);
    r
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}

#[test]
fn trim_whatif_trims_at_the_threshold_and_stubs_only_the_eligible_outputs() {
    let tl = base().timeline();
    assert_eq!(tl.requests(), 3);
    // At 100k: the third request (120k) is the first to reach it. The old read is
    // trimmed; the small listing and the read of the current turn are not.
    let o = simulate(&tl, &policy(100_000, 1, 500, 1_000));
    assert_eq!((o.checkpoints, o.outputs_trimmed), (1, 1), "{o:?}");
    assert_eq!(o.tokens_trimmed, 5_000 - STUB_TOKENS);
    // Below every request's input: no checkpoint, nothing changes.
    let none = simulate(&tl, &policy(130_000, 1, 500, 1_000));
    assert_eq!((none.checkpoints, none.outputs_trimmed), (0, 0));
    assert!(close(none.net, 0.0) && close(none.trimmed_cost, none.actual_cost));
    // Exactly at a request's input counts: the second request (80k) checkpoints.
    let at = simulate(&tl, &policy(80_000, 1, 500, 1_000));
    assert_eq!((at.checkpoints, at.tokens_trimmed), (1, 5_000 - STUB_TOKENS));
    // Not enough to gain: no checkpoint.
    let small = simulate(&tl, &policy(100_000, 1, 500, 5_000));
    assert_eq!(small.checkpoints, 0);
}

#[test]
fn trim_whatif_respects_the_age_and_size_rules() {
    let mut r = Rollout::new("sess-age");
    r.turn()
        .exec("cat old_big.rs", 3_000)
        .exec("cat old_mid.rs", 1_000)
        .req(60_000, 0, 100);
    r.turn().exec("cat recent_big.rs", 3_000).req(70_000, 59_000, 100);
    r.turn().req(110_000, 69_000, 100);
    let tl = r.timeline();
    // K=2, S=2000: only the big output of turn 1 (two turns back).
    let strict = simulate(&tl, &policy(100_000, 2, 2_000, 1_000));
    assert_eq!(
        (strict.outputs_trimmed, strict.tokens_trimmed),
        (1, 3_000 - STUB_TOKENS)
    );
    // K=2, S=500: the medium one of turn 1 joins; turn 2 is still too recent.
    let sized = simulate(&tl, &policy(100_000, 2, 500, 1_000));
    assert_eq!(
        (sized.outputs_trimmed, sized.tokens_trimmed),
        (2, 4_000 - 2 * STUB_TOKENS)
    );
    // K=1, S=500: all three.
    let loose = simulate(&tl, &policy(100_000, 1, 500, 1_000));
    assert_eq!(
        (loose.outputs_trimmed, loose.tokens_trimmed),
        (3, 7_000 - 3 * STUB_TOKENS)
    );
    // K=3: nothing is old enough.
    assert_eq!(simulate(&tl, &policy(100_000, 3, 500, 1_000)).checkpoints, 0);
}

#[test]
fn trim_whatif_checkpoint_cost_and_per_request_saving_add_up() {
    let mut r = base();
    r.req(125_000, 119_000, 200).req(130_000, 124_000, 200);
    let tl = r.timeline();
    let o = simulate(&tl, &policy(100_000, 1, 500, 1_000));
    let d = (5_000 - STUB_TOKENS) as f64;
    // The request after the checkpoint pays its shortened cached prefix uncached.
    assert!(close(o.checkpoint_cost, 0.9 * (119_000.0 - d)), "{o:?}");
    // Each request after it saves the trimmed tokens at the cached price.
    assert!(close(o.saving, 2.0 * CACHED_WEIGHT * d), "{o:?}");
    assert!(close(o.recovery_cost, 0.0));
    assert!(close(o.net, o.saving - o.checkpoint_cost - o.recovery_cost));
    assert!(close(o.trimmed_cost, o.actual_cost - o.net));
    let actual = cost(50_000, 0, 100)
        + cost(80_000, 49_000, 100)
        + cost(120_000, 79_000, 100)
        + cost(125_000, 119_000, 200)
        + cost(130_000, 124_000, 200);
    assert!(close(o.actual_cost, actual));
    assert!(close(o.net_pct, o.net / actual * 100.0));
    // Many later requests pay the miss back.
    for k in 1..=2_000 {
        r.req(130_000 + k, 129_000 + k, 200);
    }
    let long = simulate(&r.timeline(), &policy(100_000, 1, 500, 1_000));
    assert!(long.net > 0.0 && long.checkpoints == 1, "{long:?}");
}

#[test]
fn trim_whatif_the_checkpoint_miss_starts_at_the_first_trimmed_output() {
    // The same request series as `base`, but the 5,000-token read comes after the
    // first request (50,000 input, 100 output): 50,100 tokens sit before it.
    let mut r = Rollout::new("sess-late");
    r.turn().exec("ls", 100).req(50_000, 0, 100);
    r.exec("cat big.rs", 5_000);
    r.turn().req(80_000, 49_000, 100);
    r.turn().exec("cat new.rs", 6_000).req(120_000, 79_000, 100);
    r.req(125_000, 119_000, 200);
    let o = simulate(&r.timeline(), &policy(100_000, 1, 500, 1_000));
    let d = (5_000 - STUB_TOKENS) as f64;
    assert_eq!(o.checkpoints, 1, "{o:?}");
    // Only the cached tokens after the first trimmed output are read again.
    assert!(close(o.checkpoint_cost, 0.9 * (119_000.0 - d - 50_100.0)), "{o:?}");
    assert!(close(o.saving, CACHED_WEIGHT * d), "{o:?}");
    // In `base` the read is the first thing in the history: the whole prefix is missed.
    let mut early = base();
    early.req(125_000, 119_000, 200);
    let e = simulate(&early.timeline(), &policy(100_000, 1, 500, 1_000));
    assert!(close(e.checkpoint_cost, 0.9 * (119_000.0 - d)), "{e:?}");
    assert!(o.checkpoint_cost < e.checkpoint_cost);
}

#[test]
fn trim_whatif_a_compaction_resets_the_history() {
    let p = policy(100_000, 1, 500, 1_000);
    for marked in [true, false] {
        let mut r = base();
        if marked {
            r.compacted();
        }
        // After the compaction (or a drop below half the input), the old outputs are gone.
        r.req(40_000, 0, 100).req(45_000, 39_000, 100);
        r.turn().turn().req(150_000, 44_000, 100);
        let o = simulate(&r.timeline(), &p);
        assert_eq!(o.checkpoints, 1, "marked {marked}: {o:?}");
        assert!(
            close(o.saving, 0.0) && close(o.checkpoint_cost, 0.0),
            "marked {marked}: {o:?}"
        );
        assert!(close(o.trimmed_cost, o.actual_cost));
    }
}

#[test]
fn trim_whatif_a_later_call_with_the_same_command_or_file_is_a_recovery_and_costs_the_output() {
    let p = policy(100_000, 1, 500, 1_000);
    let mut r = base();
    r.exec("ls -la", 200).req(125_000, 119_000, 200);
    let none = simulate(&r.timeline(), &p);
    assert_eq!(none.recoveries, 0);
    for again in ["cat big.rs", "sed -n 1,80p big.rs"] {
        let mut r = base();
        r.exec(again, 5_000).req(125_000, 119_000, 200);
        let o = simulate(&r.timeline(), &p);
        assert_eq!(o.recoveries, 1, "{again}: {o:?}");
        assert!(close(o.recovery_share, 1.0));
        assert!(close(o.recovery_cost, 5_000.0), "{again}: {o:?}");
        assert!(close(o.net, o.saving - o.checkpoint_cost - o.recovery_cost));
        assert!(close(o.trimmed_cost - none.trimmed_cost, 5_000.0), "{again}");
    }
}

#[test]
fn trim_whatif_reads_commands_from_code_mode_scripts_and_simple_reads() {
    let script = r#"const r = await tools.exec_command({cmd:"cat big.rs",max_output_tokens:6000}); text(r.output)"#;
    assert_eq!(script_commands(script), vec!["cat big.rs".to_string()]);
    assert_eq!(
        script_commands(r#"exec_command({ "cmd": 'rg -n \'x\' src' })"#),
        vec!["rg -n 'x' src".to_string()]
    );
    assert!(script_commands("const cmdline = 1; mycmd: 2").is_empty());
    assert_eq!(file_read("sed -n 1,120p src/a.rs").as_deref(), Some("src/a.rs"));
    assert_eq!(file_read("head -n 50 a.rs").as_deref(), Some("a.rs"));
    assert_eq!(file_read("cat a.rs | wc -l"), None);
    assert_eq!(file_read("cat a.rs b.rs"), None);
}

fn long_session(id: &str, requests: usize) -> String {
    let mut r = Rollout::new(id);
    let mut input = 160_000;
    for k in 0..requests {
        if k % 5 == 0 {
            r.turn().exec(&format!("cat file_{k}.rs"), 3_000);
        }
        r.req(input, if k == 0 { 0 } else { input - 3_000 }, 300);
        input += 3_000;
    }
    r.text()
}

fn home_with(sessions: &[(&str, String)]) -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    let day = home.path().join("sessions/2026/01/01");
    std::fs::create_dir_all(&day).unwrap();
    for (id, text) in sessions {
        std::fs::write(day.join(format!("rollout-2026-01-01T00-00-00-{id}.jsonl")), text).unwrap();
    }
    home
}

#[test]
fn trim_whatif_skips_a_short_session_and_leaves_the_home_untouched() {
    let short = long_session("sess-short", 10);
    let home = home_with(&[
        ("sess-short", short.clone()),
        ("sess-long", long_session("sess-long", 60)),
    ]);
    let w = whatif(home.path(), 100_000, None, &Options::default());
    assert_eq!(w.rollouts_read, 2);
    assert_eq!(w.sessions.len(), 1);
    assert_eq!(w.sessions[0].session_id, "sess-long");
    let only_short = whatif(home.path(), 100_000, Some("sess-short"), &Options::default());
    assert!(only_short.sessions.is_empty() && only_short.best.is_none());
    assert!(render(&only_short).contains("no session with enough requests"));
    let file = home
        .path()
        .join("sessions/2026/01/01/rollout-2026-01-01T00-00-00-sess-short.jsonl");
    assert_eq!(std::fs::read_to_string(file).unwrap(), short);
}

#[test]
fn trim_whatif_json_lists_every_policy_the_best_one_and_the_caveat() {
    let home = home_with(&[("sess-long", long_session("sess-long", 120))]);
    let w = whatif(home.path(), 100_000, None, &Options::default());
    let v = serde_json::to_value(&w).unwrap();
    assert_eq!(v["policies"].as_array().unwrap().len(), 8);
    assert_eq!(v["totals"].as_array().unwrap().len(), 8);
    assert_eq!(v["sessions"][0]["outcomes"].as_array().unwrap().len(), 8);
    assert_eq!(v["sessions"][0]["requests"], 120);
    let best = v["best"].as_u64().unwrap() as usize;
    let nets: Vec<f64> = v["totals"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["net"].as_f64().unwrap())
        .collect();
    assert!(nets.iter().all(|n| *n <= nets[best]));
    assert!(v["totals"][best]["checkpoints"].as_u64().unwrap() >= 1);
    assert!(v["caveats"][0].as_str().unwrap().contains("not a measured saving"));
    assert_eq!(v["weights"]["output"], 8.0);
    let text = render(&w);
    assert!(
        text.contains("per session, best policy") && text.contains("TOTAL"),
        "{text}"
    );
    assert!(text.contains("Warning: This is an estimate from a replay"), "{text}");
}
