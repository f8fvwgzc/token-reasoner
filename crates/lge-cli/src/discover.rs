//! `lge usage discover`: past Codex sessions replayed through lge's rules
//! (`lge_proxy::discover::replay`), printed as tables. Reads only; with
//! `--retain`, the commands that were corrected at least twice become
//! convention facts in the repository's graph, and nothing else is written.

use anyhow::Result;
use lge_memory::{Outcome, RetainInput};
use lge_proxy::discover::{family_of, DiscoverReport};
use lge_store::GraphStore;
use std::collections::HashSet;

/// A corrected command is kept as a fact only when the same correction was
/// seen at least this often: once may be chance.
pub const MIN_SEEN: u64 = 2;
/// Command families shown.
const FAMILIES_SHOWN: usize = 20;
/// The tag of the facts `--retain` stores.
pub const CORRECTION_TAG: &str = "cli-correction";

fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn share(part: u64, whole: u64) -> String {
    if whole == 0 {
        "-".to_string()
    } else {
        format!("{:.1}%", part as f64 * 100.0 / whole as f64)
    }
}

/// The report as tables.
pub fn render(r: &DiscoverReport) -> String {
    let mut out = format!(
        "Past Codex sessions of the last {} day(s): {} rollout file(s) read",
        r.days, r.files
    );
    if r.unreadable > 0 {
        out.push_str(&format!(", {} unreadable", r.unreadable));
    }
    out.push_str(&format!(
        ".\nTool outputs: {} ({} tokens). Every rule is replayed as if it were on, without a graph.\n",
        grouped(r.outputs),
        grouped(r.output_tokens)
    ));

    out.push_str(&format!(
        "\nCommand families by output tokens (top {FAMILIES_SHOWN}):\n{:32}  {:>8}  {:>11}  {:>12}  {:>11}\n",
        "family", "outputs", "tokens", "would remove", "no rule"
    ));
    for f in r.families.iter().take(FAMILIES_SHOWN) {
        let family: String = f.family.chars().take(32).collect();
        out.push_str(&format!(
            "{family:32}  {:>8}  {:>11}  {:>12}  {:>11}\n",
            grouped(f.outputs),
            grouped(f.output_tokens),
            grouped(f.by_rule.values().sum()),
            grouped(f.unrecognised_tokens)
        ));
    }
    if r.families.len() > FAMILIES_SHOWN {
        out.push_str(&format!(
            "... and {} more (--json lists every family)\n",
            r.families.len() - FAMILIES_SHOWN
        ));
    }

    out.push_str(&format!(
        "\nTokens each rule would remove:\n{:16}  {:>11}  {:>13}\n",
        "rule", "tokens", "of all output"
    ));
    let mut rules: Vec<(&String, &u64)> = r.by_rule.iter().collect();
    rules.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    for (rule, tokens) in &rules {
        out.push_str(&format!(
            "{rule:16}  {:>11}  {:>13}\n",
            grouped(**tokens),
            share(**tokens, r.output_tokens)
        ));
    }
    if rules.is_empty() {
        out.push_str("(none)\n");
    }
    out.push_str(&format!(
        "\nNo rule recognises: {} tokens ({} of all output).\n",
        grouped(r.unrecognised_tokens),
        share(r.unrecognised_tokens, r.output_tokens)
    ));
    out.push_str(&format!(
        "Code-mode scripts: {} ({} ran one command, {} several, {} not read).\n",
        grouped(r.scripts.scripts),
        grouped(r.scripts.single_exec),
        grouped(r.scripts.several_exec),
        grouped(r.scripts.unread)
    ));
    out.push_str(&format!(
        "Requests per session: {} session(s), median {}, max {}, {} in all.\n",
        grouped(r.requests.sessions),
        r.requests.median,
        grouped(r.requests.max),
        grouped(r.requests.total)
    ));

    out.push_str("\nCorrected commands (failed on their usage, then run another way within 3 calls)");
    match &r.corrections_from {
        Some(folder) => out.push_str(&format!(", in sessions under {folder}:\n")),
        None => out.push_str(":\n"),
    }
    if r.corrected.is_empty() {
        out.push_str("(none)\n");
    }
    for c in &r.corrected {
        out.push_str(&format!(
            "{:>4}x  `{}`, then `{}` ({})\n",
            c.count, c.wrong, c.right, c.class
        ));
    }
    out
}

/// The facts `--retain` stores: one per corrected pair seen at least
/// [`MIN_SEEN`] times, keyed by the wrong command's program and subcommand.
/// When two corrections share a key, only the most frequent is kept (the
/// report lists them most frequent first), since a newer fact with the same
/// key replaces the older one.
pub fn corrections(r: &DiscoverReport) -> Vec<RetainInput> {
    let mut keys = HashSet::new();
    r.corrected
        .iter()
        .filter(|c| c.count >= MIN_SEEN)
        .filter_map(|c| {
            let key = format!("{CORRECTION_TAG}:{}", family_of(&c.wrong));
            keys.insert(key.clone()).then(|| RetainInput {
                text: format!(
                    "Use `{}` instead of `{}` ({}; seen {} times).",
                    c.right, c.wrong, c.class, c.count
                ),
                kind: lge_core::memory_kind::CONVENTION.to_string(),
                key,
                tags: vec![CORRECTION_TAG.to_string()],
                source: "lge usage discover".to_string(),
                valid_at: None,
            })
        })
        .collect()
}

/// Store [`corrections`] in `store`. Returns each fact's key and what the
/// store did with it.
pub fn retain_corrections(store: &dyn GraphStore, r: &DiscoverReport, now: i64) -> Result<Vec<(String, Outcome)>> {
    corrections(r)
        .into_iter()
        .map(|input| Ok((input.key.clone(), lge_memory::retain(store, &input, now)?.outcome)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lge_proxy::discover::replay;
    use lge_retrieve::budget::count;
    use serde_json::{json, Value};
    use std::path::Path;

    const USAGE: &str = "error: unexpected argument '--nocapturee' found\n\nUsage: cargo test [OPTIONS] [TESTNAME]\n";

    fn framed(exit: i32, text: &str) -> String {
        format!("Chunk ID: 1a2b\nWall time: 1.0 seconds\nProcess exited with code {exit}\nOriginal token count: 10\nOutput:\n{text}")
    }

    /// A rollout with shell calls: (command, exit, output).
    fn rollout(home: &Path, name: &str, calls: &[(&str, i32, String)]) {
        rollout_in(home, name, "/work/shop", calls);
    }

    /// [`rollout`] of a session that worked in `cwd`.
    fn rollout_in(home: &Path, name: &str, cwd: &str, calls: &[(&str, i32, String)]) {
        let mut lines: Vec<Value> = vec![
            json!({ "timestamp": "2026-11-10T10:00:00.000Z", "type": "session_meta", "payload": { "id": name, "cwd": cwd } }),
        ];
        for (i, (cmd, exit, output)) in calls.iter().enumerate() {
            let id = format!("call_{i}");
            lines.push(json!({ "timestamp": "2026-11-10T10:00:01.000Z", "type": "response_item", "payload":
                { "type": "function_call", "name": "exec_command", "call_id": id, "arguments": json!({ "cmd": cmd }).to_string() } }));
            lines.push(
                json!({ "timestamp": "2026-11-10T10:00:01.000Z", "type": "response_item", "payload":
                { "type": "function_call_output", "call_id": id, "output": framed(*exit, output) } }),
            );
            lines.push(json!({ "timestamp": "2026-11-10T10:00:02.000Z", "type": "event_msg", "payload": {
                "type": "token_count",
                "info": { "total_token_usage": { "input_tokens": 10, "output_tokens": 1 }, "last_token_usage": { "input_tokens": 10, "output_tokens": 1 } }
            } }));
        }
        let dir = home.join("sessions/2026/11/10");
        std::fs::create_dir_all(&dir).unwrap();
        let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(dir.join(format!("rollout-2026-11-10T10-00-00-{name}.jsonl")), text).unwrap();
    }

    fn codex_home() -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        let readme = "line of a file\n".repeat(20);
        // The same correction in two sessions, another one once.
        for name in ["a", "b"] {
            rollout(
                home.path(),
                name,
                &[
                    ("cargo test --nocapturee", 2, USAGE.to_string()),
                    (
                        "cargo test -- --nocapture",
                        0,
                        "test result: ok. 1 passed; 0 failed\n".to_string(),
                    ),
                    ("cat README.md", 0, readme.clone()),
                ],
            );
        }
        rollout(
            home.path(),
            "c",
            &[
                (
                    "git lgo",
                    1,
                    "git: 'lgo' is not a git command. unknown option\n".to_string(),
                ),
                ("git log --oneline -1", 0, "abc123 first\n".to_string()),
            ],
        );
        home
    }

    #[test]
    fn discover_prints_the_totals_of_the_replay() {
        let home = codex_home();
        let report = replay(home.path(), 30).unwrap();
        assert_eq!((report.files, report.outputs), (3, 8));
        let text = render(&report);
        assert!(text.contains("3 rollout file(s) read"), "{text}");
        let tokens: u64 = [
            USAGE,
            "test result: ok. 1 passed; 0 failed\n",
            &"line of a file\n".repeat(20),
        ]
        .iter()
        .map(|t| count(t) as u64)
        .sum::<u64>()
            * 2
            + count("git: 'lgo' is not a git command. unknown option\n") as u64
            + count("abc123 first\n") as u64;
        assert_eq!(report.output_tokens, tokens);
        assert!(
            text.contains(&format!("Tool outputs: 8 ({} tokens)", grouped(tokens))),
            "{text}"
        );
        assert!(text.contains("cargo test"), "{text}");
        assert!(
            text.contains("2x  `cargo test --nocapturee`, then `cargo test -- --nocapture` (unexpected argument)"),
            "{text}"
        );
        assert!(text.contains("Requests per session: 3 session(s)"), "{text}");
    }

    #[test]
    fn the_json_output_is_stable() {
        let home = codex_home();
        let first = serde_json::to_string_pretty(&replay(home.path(), 30).unwrap()).unwrap();
        let second = serde_json::to_string_pretty(&replay(home.path(), 30).unwrap()).unwrap();
        assert_eq!(first, second);
        let value: Value = serde_json::from_str(&first).unwrap();
        let mut keys: Vec<&String> = value.as_object().unwrap().keys().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "by_rule",
                "corrected",
                "corrections_from",
                "days",
                "families",
                "files",
                "output_tokens",
                "outputs",
                "requests",
                "scripts",
                "unreadable",
                "unrecognised_tokens"
            ]
        );
    }

    #[test]
    fn retain_stores_only_corrections_seen_twice() {
        let home = codex_home();
        let report = replay(home.path(), 30).unwrap();
        let facts = corrections(&report);
        assert_eq!(facts.len(), 1, "{facts:?}");
        let fact = &facts[0];
        assert_eq!(fact.key, "cli-correction:cargo test");
        assert_eq!(fact.tags, ["cli-correction"]);
        assert_eq!(fact.kind, "convention");
        assert_eq!(
            fact.text,
            "Use `cargo test -- --nocapture` instead of `cargo test --nocapturee` (unexpected argument; seen 2 times)."
        );

        let dir = tempfile::tempdir().unwrap();
        let store = lge_store::Store::open(&dir.path().join("graph.lbug")).unwrap();
        let stored = retain_corrections(&store, &report, 1_700_000_000).unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].0, "cli-correction:cargo test");
        let kept = store.memories_by_key("cli-correction:cargo test").unwrap();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].tags, ["cli-correction"]);
        assert_eq!(kept[0].kind, "convention");
        assert!(store.memories_by_key("cli-correction:git lgo").unwrap().is_empty());
        assert!(store.memories_by_key("cli-correction:git").unwrap().is_empty());
        // Again: the same fact, asserted once more.
        let again = retain_corrections(&store, &report, 1_700_000_100).unwrap();
        assert!(matches!(again[0].1, Outcome::Repeated), "{again:?}");
    }

    /// A failing test that mentions a missing file, and a second read of
    /// another file, are not usage errors: no convention comes of them, however
    /// often the same session is replayed.
    #[test]
    fn test_failures_and_path_guesses_are_not_corrections() {
        let home = tempfile::tempdir().unwrap();
        let failing = "FAILED tests/test_load.py::test_load - FileNotFoundError: [Errno 2] No such file or directory: 'data/x.csv'\n\
                       ========================= 1 failed, 4 passed in 0.31s =========================\n";
        for name in ["a", "b"] {
            rollout(
                home.path(),
                name,
                &[
                    ("pytest", 1, failing.to_string()),
                    (
                        "pytest tests/test_other.py",
                        0,
                        "===== 3 passed in 0.10s =====\n".to_string(),
                    ),
                    (
                        "cat src/app/util.py",
                        1,
                        "cat: src/app/util.py: No such file or directory\n".to_string(),
                    ),
                    ("cat src/app/utils.py", 0, "def f():\n    pass\n".to_string()),
                    (
                        "sh scripts/check.sh",
                        127,
                        "sh: scripts/check.sh: No such file or directory\n".to_string(),
                    ),
                    ("sh scripts/check_all.sh", 0, "all good\n".to_string()),
                ],
            );
        }
        let report = replay(home.path(), 30).unwrap();
        assert!(report.corrected.is_empty(), "{:?}", report.corrected);
        assert!(corrections(&report).is_empty());
        // A program the shell could not find is one.
        let home = tempfile::tempdir().unwrap();
        for name in ["a", "b"] {
            rollout(
                home.path(),
                name,
                &[
                    (
                        "./run_tests.sh",
                        127,
                        "bash: line 1: ./run_tests.sh: No such file or directory\n".to_string(),
                    ),
                    ("./run_tests.sh --all", 0, "done\n".to_string()),
                ],
            );
        }
        let report = replay(home.path(), 30).unwrap();
        assert_eq!(report.corrected.len(), 1, "{:?}", report.corrected);
        assert_eq!(report.corrected[0].class, "no such file or directory");
    }

    /// `--retain` writes into one repository's graph: only the corrections of
    /// sessions that worked inside it count.
    #[test]
    fn retain_reads_only_the_repositorys_own_sessions() {
        let home = tempfile::tempdir().unwrap();
        for (name, cwd) in [
            ("a", "/work/shop/api"),
            ("b", "/work/shop"),
            ("c", "/work/other"),
            ("d", "/work/other"),
        ] {
            rollout_in(
                home.path(),
                name,
                cwd,
                &[
                    ("cargo test --nocapturee", 2, USAGE.to_string()),
                    (
                        "cargo test -- --nocapture",
                        0,
                        "test result: ok. 1 passed; 0 failed\n".to_string(),
                    ),
                ],
            );
        }
        let all = replay(home.path(), 30).unwrap();
        assert_eq!(all.corrected[0].count, 4);
        let here = lge_proxy::discover::replay_for(home.path(), 30, Some(Path::new("/work/shop"))).unwrap();
        assert_eq!(here.corrected[0].count, 2);
        assert_eq!(here.files, all.files, "the other tables still cover every session");
        assert!(
            render(&here).contains("in sessions under /work/shop:"),
            "{}",
            render(&here)
        );
        let elsewhere = lge_proxy::discover::replay_for(home.path(), 30, Some(Path::new("/work/none"))).unwrap();
        assert!(elsewhere.corrected.is_empty());
    }
}
