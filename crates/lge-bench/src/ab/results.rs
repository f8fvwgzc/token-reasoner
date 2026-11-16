//! The results file of a run: one JSON line per session, appended and flushed
//! as soon as the session's check has run, so a stopped run can be resumed
//! (`lge ab --resume <work-dir>`) without spending its finished sessions again.

use super::{Arm, ArmRun, Variant};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

/// The file, in the work dir.
pub const RESULTS_FILE: &str = "results.jsonl";

/// One session and what it ran under.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResultLine {
    /// The session: task, run, arm, token tally, success, timestamps.
    #[serde(flatten)]
    pub run: ArmRun,
    /// The commit every copy is made from.
    pub base: String,
    /// The content hash of the task file.
    pub tasks_hash: String,
    /// The note appended to every prompt.
    pub note: Option<String>,
    /// `None`: the agent's own default.
    pub model: Option<String>,
    /// The sandbox passed to the agent; `None` with `approve_for_me` or the agent's default.
    pub sandbox: Option<String>,
    pub approve_for_me: bool,
    #[serde(default)]
    pub variant: Variant,
}

impl ResultLine {
    /// A result to keep: its check ran and nothing cut it short. Sessions that
    /// were interrupted or ended in an error before their check run again on
    /// a resume.
    pub fn finished(&self) -> bool {
        !self.run.interrupted && self.run.check_passed.is_some()
    }

    /// (task, run, arm).
    pub fn key(&self) -> (String, usize, Arm) {
        (self.run.task.clone(), self.run.run, self.run.arm)
    }
}

/// Append one line and flush it to the disk. A line is written whole with
/// one write, so a stop between sessions leaves a valid file; after a line
/// that was cut short (lge ab killed while writing), the new one starts on a
/// line of its own.
pub fn append(path: &Path, line: &ResultLine) -> Result<()> {
    let mut text = serde_json::to_string(line)?;
    text.push('\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    let mut last = [0u8; 1];
    let cut = file.metadata().map(|m| m.len() > 0).unwrap_or(false)
        && file.seek(SeekFrom::End(-1)).is_ok()
        && file.read_exact(&mut last).is_ok()
        && last[0] != b'\n';
    if cut {
        text.insert(0, '\n');
    }
    file.write_all(text.as_bytes())
        .and_then(|()| file.flush())
        .and_then(|()| file.sync_data())
        .with_context(|| format!("cannot write {}", path.display()))
}

/// The lines of a results file, in order, and the number of lines that could
/// not be read (a line cut by a second Ctrl-C, even inside a character). A
/// missing file has none. Each line is read on its own, so one bad line never
/// makes the others unreadable.
pub fn load(path: &Path) -> Result<(Vec<ResultLine>, usize)> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    let mut lines = Vec::new();
    let mut bad = 0;
    for line in bytes.split(|b| *b == b'\n').filter(|l| !l.trim_ascii().is_empty()) {
        match serde_json::from_slice::<ResultLine>(line) {
            Ok(l) => lines.push(l),
            Err(_) => bad += 1,
        }
    }
    Ok((lines, bad))
}

/// What a resumed run must share with the results it adds to.
#[derive(Debug, Clone, Copy)]
pub struct RunIdentity<'a> {
    pub base: &'a str,
    pub tasks_hash: &'a str,
    pub model: Option<&'a str>,
    pub variant: Variant,
    /// The note appended to every prompt (`None`: no note).
    pub note: Option<&'a str>,
    /// The sandbox passed to the agent (`None` with `approve_for_me`).
    pub sandbox: Option<&'a str>,
    pub approve_for_me: bool,
}

/// An error naming the first difference between the earlier lines and this
/// run: another base commit, task file, model, variant, prompt note, sandbox
/// or approval mode would mix two experiments in one report, which states
/// only this run's. `force` accepts them.
pub fn check_same_run(lines: &[ResultLine], now: &RunIdentity, force: bool) -> Result<()> {
    if force {
        return Ok(());
    }
    let shown = |m: Option<&str>| m.map_or_else(|| "the agent's default".to_string(), |m| format!("`{m}`"));
    for line in lines {
        if line.base != now.base {
            bail!(
                "the earlier results are from base commit {}, this run's is {}; pass --base {} to resume them, or --force to mix the two",
                line.base,
                now.base,
                line.base
            );
        }
        if line.tasks_hash != now.tasks_hash {
            bail!("the task file changed since the earlier results were written; --force mixes the two");
        }
        if line.model.as_deref() != now.model {
            bail!(
                "the earlier results used model {}, this run uses {}; pass the same --model, or --force to mix the two",
                shown(line.model.as_deref()),
                shown(now.model)
            );
        }
        if line.variant != now.variant {
            bail!(
                "the earlier results are of the variant `{}`, this run's is `{}`; pass the same --variant, or --force to mix the two",
                line.variant.as_str(),
                now.variant.as_str()
            );
        }
        if line.note.as_deref() != now.note {
            bail!(
                "the earlier results were run {}, this run {}; pass the same note options (--no-note), or --force to mix the two",
                if line.note.is_some() { "with a prompt note" } else { "without a prompt note" },
                match (now.note, line.note.is_some()) {
                    (Some(_), true) => "with another note",
                    (Some(_), false) => "with one",
                    (None, _) => "without one",
                }
            );
        }
        if line.approve_for_me != now.approve_for_me || line.sandbox.as_deref() != now.sandbox {
            let mode = |approve: bool, sandbox: Option<&str>| match (approve, sandbox) {
                (true, _) => "--approve-for-me".to_string(),
                (false, Some(s)) => format!("sandbox `{s}`"),
                (false, None) => "the agent's default sandbox".to_string(),
            };
            bail!(
                "the earlier results ran with {}, this run with {}; pass the same --sandbox and approval options, or --force to mix the two",
                mode(line.approve_for_me, line.sandbox.as_deref()),
                mode(now.approve_for_me, now.sandbox)
            );
        }
    }
    Ok(())
}

/// The latest line of each (task, run, arm), in the order those were written.
pub fn latest(lines: Vec<ResultLine>) -> Vec<ResultLine> {
    let mut out: Vec<ResultLine> = Vec::new();
    for line in lines {
        out.retain(|l| l.key() != line.key());
        out.push(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::{TraceOutput, TraceSummary};
    use super::*;

    fn line(task: &str, run: usize, arm: Arm) -> ResultLine {
        let mut r = ArmRun::new(task, run, arm, 1);
        r.check_passed = Some(true);
        r.success = true;
        r.total_tokens = 1_000;
        r.trace = Some(TraceSummary {
            requests: 2,
            measured: 2,
            mean_context: Some(500.0),
            max_context: Some(700),
            lge_added_tokens: 1,
            lge_removed_tokens: 2,
            largest_outputs: vec![TraceOutput {
                request: 1,
                command: "rg -n x".to_string(),
                tokens: 10,
                shortened: true,
            }],
        });
        ResultLine {
            run: r,
            base: "abc".to_string(),
            tasks_hash: "h".to_string(),
            note: None,
            model: Some("m".to_string()),
            sandbox: Some("workspace-write".to_string()),
            approve_for_me: false,
            variant: Variant::Batch,
        }
    }

    #[test]
    fn lines_round_trip_and_a_cut_line_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(RESULTS_FILE);
        assert_eq!(load(&path).unwrap(), (Vec::new(), 0), "no file, no lines");
        let a = line("a", 1, Arm::Off);
        append(&path, &a).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"task\":\"a\",")
            .unwrap();
        let b = line("a", 1, Arm::On);
        append(&path, &b).unwrap();
        let (lines, bad) = load(&path).unwrap();
        assert_eq!(bad, 1);
        assert_eq!(lines, [a.clone(), b.clone()]);
        assert!(lines.iter().all(ResultLine::finished));
    }

    #[test]
    fn the_latest_line_of_a_session_wins_and_only_checked_sessions_are_finished() {
        let mut cut = line("a", 1, Arm::Off);
        cut.run.interrupted = true;
        cut.run.check_passed = None;
        assert!(!cut.finished());
        let mut errored = line("a", 1, Arm::On);
        errored.run.check_passed = None;
        errored.run.error = Some("the agent crashed".to_string());
        assert!(!errored.finished());
        let done = line("a", 1, Arm::Off);
        let kept = latest(vec![cut, errored.clone(), done.clone()]);
        assert_eq!(kept, [errored, done]);
    }

    #[test]
    fn another_base_task_file_model_or_variant_is_refused_unless_forced() {
        let lines = [line("a", 1, Arm::Off)];
        let same = RunIdentity {
            base: "abc",
            tasks_hash: "h",
            model: Some("m"),
            variant: Variant::Batch,
            note: None,
            sandbox: Some("workspace-write"),
            approve_for_me: false,
        };
        assert!(check_same_run(&lines, &same, false).is_ok());
        for (other, why) in [
            (RunIdentity { base: "def", ..same }, "base commit abc"),
            (
                RunIdentity {
                    tasks_hash: "x",
                    ..same
                },
                "task file changed",
            ),
            (RunIdentity { model: None, ..same }, "used model `m`"),
            (
                RunIdentity {
                    variant: Variant::Terse,
                    ..same
                },
                "variant `batch`",
            ),
            (
                RunIdentity {
                    note: Some("Run the checks before you finish."),
                    ..same
                },
                "without a prompt note",
            ),
            (
                RunIdentity {
                    sandbox: None,
                    approve_for_me: true,
                    ..same
                },
                "sandbox `workspace-write`",
            ),
            (
                RunIdentity {
                    sandbox: Some("danger-full-access"),
                    ..same
                },
                "this run with sandbox `danger-full-access`",
            ),
        ] {
            let err = check_same_run(&lines, &other, false).unwrap_err();
            assert!(format!("{err:#}").contains(why), "{err:#}");
            assert!(check_same_run(&lines, &other, true).is_ok());
        }
    }

    #[test]
    fn a_line_cut_inside_a_character_leaves_the_others_readable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(RESULTS_FILE);
        let a = line("a", 1, Arm::Off);
        append(&path, &a).unwrap();
        let mut cut = line("a", 1, Arm::On);
        cut.run.check_tail = "✕ totals — expected 3".to_string();
        let text = serde_json::to_string(&cut).unwrap();
        let at = text.find('—').unwrap() + 1;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&text.as_bytes()[..at])
            .unwrap();
        let c = line("b", 1, Arm::Off);
        append(&path, &c).unwrap();
        let (lines, bad) = load(&path).unwrap();
        assert_eq!(bad, 1);
        assert_eq!(lines, [a, c]);
    }
}
