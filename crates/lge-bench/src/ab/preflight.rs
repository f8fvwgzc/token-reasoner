//! Before any session: each task's setup and check, run on a fresh copy of
//! the base commit exactly as they run after a session. A check that passes
//! there (the task is already done, or the check tests something else) or
//! that the shell cannot run (quoting, a missing tool) would make every
//! session of the task worthless, so the run stops before spending a token.

use super::{shell, slug, AbTask, Interrupted, Shell, Worktree};
use anyhow::Result;
use serde::Serialize;
use std::path::Path;

/// One task's preflight.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PreflightRow {
    pub task: String,
    /// `ok`, `none`, or how it failed (`exit 2`).
    pub setup: String,
    /// How the check ended on the base commit (`exit 1`), or `-` when it did not run.
    pub check: String,
    /// Why the task cannot be measured; `None` when the check failed as expected.
    pub problem: Option<String>,
}

/// Every task's preflight.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Preflight {
    pub base: String,
    pub rows: Vec<PreflightRow>,
}

impl Preflight {
    /// No task has a problem.
    pub fn ok(&self) -> bool {
        self.rows.iter().all(|r| r.problem.is_none())
    }

    /// A table: task, setup, check, and the finding.
    pub fn render(&self) -> String {
        let width = self
            .rows
            .iter()
            .map(|r| r.task.chars().count())
            .max()
            .unwrap_or(4)
            .clamp(4, 40);
        let mut out = format!(
            "preflight on base commit {}: setup, then the check, on a fresh copy\n{:width$}  {:>8}  {:>10}  finding\n",
            self.base, "task", "setup", "check"
        );
        for r in &self.rows {
            let task: String = r.task.chars().take(width).collect();
            let finding = r.problem.as_deref().unwrap_or("fails as expected");
            out.push_str(&format!("{task:width$}  {:>8}  {:>10}  {finding}\n", r.setup, r.check));
        }
        out
    }

    /// The error a run stops with: each bad task and why.
    pub fn failure(&self) -> String {
        let bad: Vec<String> = self
            .rows
            .iter()
            .filter_map(|r| r.problem.as_ref().map(|p| format!("- {}: {p}", r.task)))
            .collect();
        format!(
            "preflight: {} task(s) cannot be measured, so no session was started:\n{}\n\n{}\
             fix the task file and run again; --no-preflight skips this check",
            bad.len(),
            bad.join("\n"),
            self.render()
        )
    }
}

/// Why a task cannot be measured, from its setup and its check on the base
/// commit; `None` when the check fails there as a good check should.
pub(super) fn problem(setup: Option<&Shell>, check: Option<&Shell>) -> Option<String> {
    if let Some(s) = setup.filter(|s| !s.ok) {
        return Some(format!("the setup failed ({}): {}", s.code, last_line(&s.tail)));
    }
    let check = check?;
    if check.ok {
        return Some(
            "the check passes on the base commit: the task is already done there, or the check does not test it"
                .to_string(),
        );
    }
    // The check runs a script the task itself must create: it fails on the base
    // commit as a good check should.
    if check.exit == Some(127) && missing_script(&check.tail) {
        return None;
    }
    if let Some(line) = &check.complaint {
        return Some(format!(
            "the shell could not run the check ({}): {line}; check its quoting and that every tool it calls is installed",
            check.code
        ));
    }
    match check.exit {
        Some(127) => Some(format!(
            "the check could not run (exit 127, a command was not found): {}",
            last_line(&check.tail)
        )),
        Some(126) => Some(format!(
            "the check could not run (exit 126, a command is not executable): {}",
            last_line(&check.tail)
        )),
        _ => None,
    }
}

/// Shells the not-found lines are read from.
const SHELLS: [&str; 6] = ["sh:", "bash:", "dash:", "zsh:", "/bin/sh:", "/bin/bash:"];

/// Endings of a script's name: a name with one is a file, not a tool.
const SCRIPT_ENDINGS: [&str; 8] = [".sh", ".bash", ".py", ".rb", ".pl", ".js", ".mjs", ".ts"];

/// The name a shell's not-found line gives, in the forms sh, dash, bash and
/// zsh print: `sh: x: command not found`, `sh: 1: x: not found`, `bash: line
/// 1: x: No such file or directory`, `zsh:1: no such file or directory: x`.
fn not_found_name(line: &str) -> Option<&str> {
    let l = line.trim();
    if !SHELLS.iter().any(|p| l.starts_with(p)) {
        return None;
    }
    let lower = l.to_ascii_lowercase();
    for lead in ["no such file or directory: ", "command not found: "] {
        if let Some(at) = lower.find(lead) {
            return Some(l[at + lead.len()..].trim());
        }
    }
    for end in [": No such file or directory", ": command not found", ": not found"] {
        if let Some(rest) = l.strip_suffix(end) {
            return rest.rsplit(": ").next().map(str::trim);
        }
    }
    None
}

/// The shell says a program it was given as a file is not there: a path
/// (`./scripts/check.sh`) or a script's name (`sh check.sh`). On the base
/// commit that is a script the task itself must write, not a missing tool.
fn missing_script(text: &str) -> bool {
    text.lines()
        .filter_map(not_found_name)
        .any(|name| name.contains('/') || SCRIPT_ENDINGS.iter().any(|e| name.ends_with(e)))
}

fn last_line(text: &str) -> String {
    text.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .chars()
        .take(200)
        .collect()
}

/// Run every task's setup and check on its own fresh copy of `base`, in the
/// folder `prefix` of it, one after another. The copies are removed as each
/// task is done.
pub(super) fn run(
    repo: &Path,
    base: &str,
    prefix: &str,
    work_dir: &Path,
    tasks: &[(usize, &AbTask)],
    stop: &dyn Fn() -> bool,
    log: &dyn Fn(&str),
) -> Result<Preflight> {
    let mut out = Preflight {
        base: base.to_string(),
        rows: Vec::new(),
    };
    for &(i, task) in tasks {
        if stop() {
            return Err(Interrupted.into());
        }
        log(&format!("preflight: {}", task.name));
        let dir = work_dir.join(format!("lge-ab-preflight-{:02}-{}", i + 1, slug(&task.name)));
        let copy = Worktree::add(repo, &dir, base)?;
        let cwd = match prefix.trim_end_matches('/') {
            "" => copy.path().to_path_buf(),
            inner => copy.path().join(inner),
        };
        let setup = match task.setup.as_deref().filter(|s| !s.trim().is_empty()) {
            Some(s) => Some(shell(s, &cwd)?),
            None => None,
        };
        let check = match &setup {
            Some(s) if !s.ok => None,
            _ => Some(shell(&task.check, &cwd)?),
        };
        if stop() {
            return Err(Interrupted.into());
        }
        out.rows.push(PreflightRow {
            task: task.name.clone(),
            setup: match &setup {
                None => "none".to_string(),
                Some(s) if s.ok => "ok".to_string(),
                Some(s) => s.code.clone(),
            },
            check: check.as_ref().map_or_else(|| "-".to_string(), |c| c.code.clone()),
            problem: problem(setup.as_ref(), check.as_ref()),
        });
        drop(copy);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checked(command: &str) -> Option<String> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("scripts")).unwrap();
        let check = shell(command, dir.path()).unwrap();
        problem(None, Some(&check))
    }

    #[test]
    fn a_check_that_runs_a_script_the_task_must_write_fails_as_it_should() {
        for command in [
            "sh scripts/check_links.sh --all",
            "./scripts/check_links.sh",
            "bash check_links.sh",
        ] {
            assert_eq!(checked(command), None, "{command}");
        }
        // A tool that is not installed is still a problem.
        let missing = checked("no-such-tool-for-lge --check").unwrap_or_default();
        assert!(missing.contains("could not run"), "{missing}");
        for (line, name) in [
            ("sh: scripts/x.sh: No such file or directory", Some("scripts/x.sh")),
            ("sh: 1: ./x.sh: not found", Some("./x.sh")),
            ("bash: line 1: pytest: command not found", Some("pytest")),
            ("zsh:1: no such file or directory: ./x", Some("./x")),
            ("FileNotFoundError: No such file or directory", None),
        ] {
            assert_eq!(not_found_name(line), name, "{line}");
        }
    }

    #[test]
    fn a_failing_program_that_prints_shell_words_is_an_expected_failure() {
        let check = "echo 'bash: line 1: shellcheck: command not found'; exit 1";
        assert_eq!(checked(check), None);
        // The shell's own complaint about the check still stops the run.
        let quoting = checked("echo 'unclosed").unwrap_or_default();
        assert!(quoting.contains("the shell could not run the check"), "{quoting}");
    }
}
