//! Replay past Codex sessions through every rule, to see what lge would have
//! done with them.
//!
//! Each tool call of the rollout files of the last days is paired with its
//! output, its command and exit status are read the way the proxy reads them
//! ([`crate::responses`]), and the pair goes through
//! [`crate::session::shadow_decide`]: every rule as if it were `on`, without a
//! graph and without the conversation. Only aggregates are kept, in memory;
//! nothing is written.

use crate::command::{normalise, script_plan, ScriptPlan};
use crate::responses::{command_of, is_script_header_text, unframe, Framed, SHELL_TOOLS};
use crate::session::shadow_decide;
use crate::shorten::RunStatus;
use anyhow::Result;
use lge_retrieve::budget::count;
use lge_usage::RolloutItem;
use regex::Regex;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::LazyLock;

/// Later calls in which a corrected command is looked for.
const CORRECTION_WINDOW: usize = 3;
/// Characters of a command kept in a corrected pair.
const COMMAND_CHARS: usize = 200;

/// What the rules would have done over the sessions of the last days.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct DiscoverReport {
    pub days: i64,
    /// Rollout files read, and those that could not be read.
    pub files: usize,
    pub unreadable: usize,
    /// Tool outputs paired with their call, and their tokens.
    pub outputs: u64,
    pub output_tokens: u64,
    /// Rule id -> the tokens it would remove.
    pub by_rule: BTreeMap<String, u64>,
    /// Tokens of the outputs no rule would change.
    pub unrecognised_tokens: u64,
    /// Per command family (`program sub`), largest output first.
    pub families: Vec<Family>,
    pub scripts: ScriptStats,
    pub requests: RequestStats,
    /// Commands that failed on their usage and were corrected soon after,
    /// most frequent first.
    pub corrected: Vec<Correction>,
    /// The folder the corrected commands were limited to, when they were.
    pub corrections_from: Option<String>,
}

/// One command family: `program sub` for a shell command (`cargo test`,
/// `rg`), `(tool) name` for another tool, `(script)` for code-mode output
/// judged by content.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Family {
    pub family: String,
    pub outputs: u64,
    pub output_tokens: u64,
    pub by_rule: BTreeMap<String, u64>,
    pub unrecognised_tokens: u64,
}

/// Code-mode scripts: how many, how many ran a single exec call, how many
/// several, and how many could not be read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ScriptStats {
    pub scripts: u64,
    pub single_exec: u64,
    pub several_exec: u64,
    pub unread: u64,
}

/// Model requests per session.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RequestStats {
    pub sessions: u64,
    pub total: u64,
    pub median: f64,
    pub max: u64,
}

/// A command that failed on its usage, the command that worked after it, and
/// what the failure said.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Correction {
    pub wrong: String,
    pub right: String,
    pub class: String,
    pub count: u64,
}

/// What a program prints when it is called with options it does not take.
/// Read only on the program's own complaint lines (see [`usage_class`]).
static USAGE_ERROR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)unexpected argument|unrecognized (?:option|argument)|unknown (?:option|flag)|invalid option|requires a value|missing required argument",
    )
    .expect("valid regex")
});

/// The shell could not run the program: `bash: line 1: foo: command not
/// found`, `sh: ./run.sh: No such file or directory`, `zsh: command not
/// found: foo`.
static NOT_FOUND: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^-?(?:ba|z|da|k)?sh(?:: line \d+)?: (?:(\S+): (command not found|No such file or directory)|(command not found|no such file or directory): (\S+))\s*$",
    )
    .expect("valid regex")
});

/// One text a call printed: the command it is judged by (empty: by content),
/// the family it is counted in, the text and its status.
struct Unit {
    command: String,
    family: String,
    text: String,
    status: RunStatus,
}

/// A shell call, for the corrected-command check.
struct ShellRun {
    program: String,
    /// The program as written (`./run.sh`, `cargo`).
    program_word: String,
    command: String,
    status: RunStatus,
    text: String,
}

/// Replay every rollout file under `codex_home` written in the last `days`
/// days. Reads only; keeps aggregates only.
pub fn replay(codex_home: &Path, days: i64) -> Result<DiscoverReport> {
    replay_for(codex_home, days, None)
}

/// [`replay`], with the corrected commands taken only from sessions that
/// worked inside `repo` (their recorded folder is `repo` or below it), when
/// given: what one repository's agents got wrong, not every repository's.
/// The other tables cover every session.
pub fn replay_for(codex_home: &Path, days: i64, repo: Option<&Path>) -> Result<DiscoverReport> {
    let since = lge_retrieve::unix_now() - days * 86_400;
    let mut report = DiscoverReport {
        days,
        corrections_from: repo.map(|r| r.to_string_lossy().to_string()),
        ..DiscoverReport::default()
    };
    let mut families: BTreeMap<String, Family> = BTreeMap::new();
    let mut corrections: HashMap<(String, String, String), u64> = HashMap::new();
    let mut per_session: Vec<u64> = Vec::new();
    for path in lge_usage::rollout_files(codex_home, since) {
        let Ok(items) = lge_usage::rollout_items(&path) else {
            report.unreadable += 1;
            continue;
        };
        report.files += 1;
        per_session.push(lge_usage::rollout_requests(&path).unwrap_or(0));
        let (units, runs) = read_session(&items, &mut report.scripts);
        for unit in units {
            add_unit(&mut report, &mut families, unit);
        }
        let here =
            repo.is_none_or(|repo| lge_usage::rollout_cwd(&path).is_some_and(|cwd| Path::new(&cwd).starts_with(repo)));
        if here {
            for (wrong, right, class) in corrected_pairs(&runs) {
                *corrections.entry((wrong, right, class)).or_insert(0) += 1;
            }
        }
    }
    report.families = families.into_values().collect();
    report
        .families
        .sort_by(|a, b| b.output_tokens.cmp(&a.output_tokens).then(a.family.cmp(&b.family)));
    report.corrected = corrections
        .into_iter()
        .map(|((wrong, right, class), count)| Correction {
            wrong,
            right,
            class,
            count,
        })
        .collect();
    report.corrected.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then(a.wrong.cmp(&b.wrong))
            .then(a.right.cmp(&b.right))
    });
    per_session.sort_unstable();
    report.requests = RequestStats {
        sessions: per_session.len() as u64,
        total: per_session.iter().sum(),
        median: match per_session.len() {
            0 => 0.0,
            n if n.is_multiple_of(2) => (per_session[n / 2 - 1] + per_session[n / 2]) as f64 / 2.0,
            n => per_session[n / 2] as f64,
        },
        max: per_session.last().copied().unwrap_or(0),
    };
    Ok(report)
}

fn add_unit(report: &mut DiscoverReport, families: &mut BTreeMap<String, Family>, unit: Unit) {
    let tokens = count(&unit.text) as u64;
    let fired = shadow_decide(&unit.command, &unit.text, unit.status);
    let family = families.entry(unit.family.clone()).or_insert_with(|| Family {
        family: unit.family.clone(),
        ..Family::default()
    });
    report.outputs += 1;
    report.output_tokens += tokens;
    family.outputs += 1;
    family.output_tokens += tokens;
    if fired.is_empty() {
        report.unrecognised_tokens += tokens;
        family.unrecognised_tokens += tokens;
    }
    for (rule, before, after) in fired {
        let removed = before.saturating_sub(after) as u64;
        *report.by_rule.entry(rule.as_str().to_string()).or_insert(0) += removed;
        *family.by_rule.entry(rule.as_str().to_string()).or_insert(0) += removed;
    }
}

/// Programs whose first word is a subcommand (`cargo test`, `git diff`).
/// For any other program the first word is an argument (a pattern, a path)
/// and is not part of the family.
const WITH_SUBCOMMANDS: [&str; 32] = [
    "cargo",
    "git",
    "go",
    "npm",
    "pnpm",
    "yarn",
    "bun",
    "deno",
    "uv",
    "poetry",
    "pip",
    "pip3",
    "docker",
    "kubectl",
    "gh",
    "rustup",
    "dotnet",
    "terraform",
    "helm",
    "brew",
    "bundle",
    "mix",
    "swift",
    "conda",
    "nx",
    "turbo",
    "mvn",
    "mvnw",
    "gradle",
    "gradlew",
    "just",
    "golangci-lint",
];

/// The family of a shell command: its program, and its subcommand when the
/// program has subcommands.
pub fn family_of(command: &str) -> String {
    let Some(n) = normalise(command) else {
        return "(unparsed command)".to_string();
    };
    let word = |s: &str| {
        s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':'))
    };
    match n.sub {
        Some(sub) if WITH_SUBCOMMANDS.contains(&n.program.as_str()) && word(&sub) => format!("{} {sub}", n.program),
        _ => n.program,
    }
}

/// The text and status of a plain or JSON frame.
fn plain(raw: &str) -> (String, RunStatus) {
    match unframe(raw, false) {
        Framed::Plain { text, status, .. } | Framed::Json { text, status, .. } => (text, status),
        Framed::JsonLines { .. } => (raw.to_string(), RunStatus::default()),
    }
}

/// One code-mode text: each exec result printed as a JSON line on its own,
/// or the whole text.
fn script_texts(raw: &str, outer: RunStatus) -> Vec<(String, RunStatus)> {
    match unframe(raw, true) {
        Framed::JsonLines { lines, .. } => lines
            .into_iter()
            .filter_map(|(_, exec)| exec.map(|e| (e.output, e.status)))
            .collect(),
        Framed::Plain { text, status, .. } | Framed::Json { text, status, .. } => vec![(
            text,
            RunStatus {
                exit: status.exit.or(outer.exit),
                running: status.running || outer.running,
                masked: false,
            },
        )],
    }
}

/// The units and shell runs of one session's items, in order.
fn read_session(items: &[RolloutItem], scripts: &mut ScriptStats) -> (Vec<Unit>, Vec<Option<ShellRun>>) {
    let calls: HashMap<&str, &RolloutItem> = items
        .iter()
        .filter(|i| i.kind.ends_with("_call"))
        .map(|i| (i.call_id.as_str(), i))
        .collect();
    let mut units = Vec::new();
    // One entry per paired call: the shell run, or None for any other call.
    let mut runs = Vec::new();
    for output in items.iter().filter(|i| i.kind.ends_with("_output")) {
        let Some(call) = calls.get(output.call_id.as_str()) else {
            continue;
        };
        let shell = match call.kind.as_str() {
            "function_call" if SHELL_TOOLS.contains(&call.name.as_str()) => {
                serde_json::from_str(&call.arguments).ok().as_ref().and_then(command_of)
            }
            "local_shell_call" => serde_json::from_str(&call.arguments).ok().as_ref().and_then(command_of),
            _ => None,
        };
        if let Some(ran) = shell {
            let family = family_of(&ran.command);
            let mut first: Option<(String, RunStatus)> = None;
            for raw in &output.outputs {
                let (text, status) = plain(raw);
                first.get_or_insert_with(|| (text.clone(), status));
                units.push(Unit {
                    command: ran.command.clone(),
                    family: family.clone(),
                    text,
                    status,
                });
            }
            runs.push(first.map(|(text, status)| {
                ShellRun {
                    program: normalise(&ran.command).map(|n| n.program).unwrap_or_default(),
                    program_word: normalise(&ran.command)
                        .and_then(|n| n.command.split_whitespace().next().map(str::to_string))
                        .unwrap_or_default(),
                    command: normalise(&ran.command).map_or_else(|| ran.command.clone(), |n| n.command),
                    status,
                    text,
                }
            }));
            continue;
        }
        runs.push(None);
        if call.kind == "custom_tool_call" && call.name == "exec" {
            scripts.scripts += 1;
            let plan = script_plan(&call.input);
            match plan.as_ref().map(|p| p.commands.len()) {
                Some(1) => scripts.single_exec += 1,
                Some(_) => scripts.several_exec += 1,
                None => scripts.unread += 1,
            }
            units.extend(script_units(plan.as_ref(), &output.outputs));
            continue;
        }
        let family = format!("(tool) {}", call.name);
        for raw in &output.outputs {
            let (text, status) = plain(raw);
            units.push(Unit {
                command: String::new(),
                family: family.clone(),
                text,
                status,
            });
        }
    }
    (units, runs)
}

/// The units of a code-mode script's output: each printed part judged by its
/// command when the plan says whose it is (as the proxy does), else by its
/// content.
fn script_units(plan: Option<&ScriptPlan>, outputs: &[String]) -> Vec<Unit> {
    let (outer, texts): (RunStatus, &[String]) = match outputs.first() {
        Some(head) if is_script_header_text(head) => (
            RunStatus {
                exit: None,
                running: head.starts_with("Script running"),
                masked: false,
            },
            &outputs[1..],
        ),
        _ => (RunStatus::default(), outputs),
    };
    let owner: Option<Vec<usize>> = match plan {
        Some(p) if p.prints.is_none() && p.commands.len() == 1 => Some(vec![0; texts.len()]),
        Some(ScriptPlan {
            prints: Some(prints), ..
        }) if prints.len() == texts.len() => Some(prints.clone()),
        _ => None,
    };
    let mut units = Vec::new();
    for (k, raw) in texts.iter().enumerate() {
        let command = match (&owner, plan) {
            (Some(owner), Some(plan)) => plan.commands.get(owner[k]).map(|c| c.cmd.clone()),
            _ => None,
        };
        let family = command.as_deref().map_or_else(|| "(script)".to_string(), family_of);
        // A part of a several-command script prints one command's output;
        // whole exec results as JSON lines appear only in single-command
        // and unread scripts.
        let several = plan.is_some_and(|p| p.prints.is_some());
        let pieces = if several {
            vec![plain(raw)]
        } else {
            script_texts(raw, outer)
        };
        for (text, status) in pieces {
            units.push(Unit {
                command: command.clone().unwrap_or_default(),
                family: family.clone(),
                text,
                status,
            });
        }
    }
    units
}

/// Why `run` failed on its usage, read from the program's own complaint:
/// the shell saying the program itself was not found, or a usage error on a
/// line the program or its argument parser starts (`error:`, `usage:`,
/// `<program>:`, `x.py: error:`). A test or a log line that merely mentions
/// such words (`FileNotFoundError: No such file or directory`) is not one.
fn usage_class(run: &ShellRun) -> Option<String> {
    for line in run.text.lines().map(str::trim) {
        if let Some(c) = NOT_FOUND.captures(line) {
            let named = c.get(1).or(c.get(4)).map_or("", |m| m.as_str());
            let class = c.get(2).or(c.get(3)).map_or("", |m| m.as_str());
            // A missing argument (`sh scripts/x.sh`) is a path guess, not a usage error.
            if named == run.program_word || named.rsplit('/').next() == Some(run.program.as_str()) {
                return Some(class.to_ascii_lowercase());
            }
            continue;
        }
        let lower = line.to_ascii_lowercase();
        let complaint = lower.starts_with("error:")
            || lower.starts_with("usage:")
            || lower.starts_with(&format!("{}:", run.program.to_ascii_lowercase()))
            || lower.contains(": error: ");
        if complaint {
            if let Some(found) = USAGE_ERROR.find(line) {
                return Some(found.as_str().to_ascii_lowercase());
            }
        }
    }
    None
}

/// The words of `a` not in `b` and of `b` not in `a` are all paths: the
/// second command reads another file, it does not fix the first one's usage.
fn differs_only_in_paths(a: &str, b: &str) -> bool {
    let words = |s: &str| s.split_whitespace().map(str::to_string).collect::<Vec<_>>();
    let (mut only_a, mut only_b) = (words(a), words(b));
    only_a.retain(|w| match only_b.iter().position(|v| v == w) {
        Some(k) => {
            only_b.remove(k);
            false
        }
        None => true,
    });
    let pathy = |w: &String| !w.starts_with('-') && (w.contains('/') || w.contains('.'));
    only_a.iter().chain(only_b.iter()).all(pathy)
}

/// (wrong, right, class) for each shell run that failed on its usage and was
/// followed, within [`CORRECTION_WINDOW`] calls, by a run of the same program
/// that exited 0 with another command. A run whose output is a test, lint or
/// build run lge recognises failed on its findings, not its usage; a pair
/// whose commands differ only in paths is another file, not a fix.
fn corrected_pairs(runs: &[Option<ShellRun>]) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for (i, run) in runs.iter().enumerate() {
        let Some(wrong) = run else { continue };
        if wrong.program.is_empty() || matches!(wrong.status.exit, None | Some(0)) {
            continue;
        }
        let Some(class) = usage_class(wrong) else {
            continue;
        };
        // A test or lint run that closed with its tally, or whose failures point
        // at files, failed on its findings.
        let opts = lge_filter::FilterOptions::default();
        let check = lge_filter::filter_for_proxy(&lge_filter::tool_of(&wrong.command), &wrong.text, &opts);
        if check.closed || check.failures.iter().any(|f| !f.file.is_empty()) {
            continue;
        }
        let right = runs
            .iter()
            .skip(i + 1)
            .take(CORRECTION_WINDOW)
            .flatten()
            .find(|r| r.program == wrong.program && r.status.exit == Some(0) && r.command != wrong.command)
            .filter(|r| !differs_only_in_paths(&wrong.command, &r.command));
        if let Some(right) = right {
            out.push((
                wrong.command.chars().take(COMMAND_CHARS).collect(),
                right.command.chars().take(COMMAND_CHARS).collect(),
                class,
            ));
        }
    }
    out
}
