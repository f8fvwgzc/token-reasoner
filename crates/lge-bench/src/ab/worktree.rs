//! A fresh copy of the repository at HEAD for one session: a detached git
//! worktree, removed again when the copy is dropped (also on error or panic).

use anyhow::{bail, Context, Result};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Run git in `repo`; its standard output, trimmed. A failure carries git's message.
fn git(repo: &Path, args: &[&OsStr]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("cannot run `git`; is it installed and on PATH?")?;
    if !out.status.success() {
        let words: Vec<String> = args.iter().map(|a| a.to_string_lossy().to_string()).collect();
        bail!(
            "`git {}` failed in {}: {}",
            words.join(" "),
            repo.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The commit HEAD points to; an error when `repo` is not in a git
/// repository or has no commit yet.
pub fn head(repo: &Path) -> Result<String> {
    git(
        repo,
        &[OsStr::new("rev-parse"), OsStr::new("--verify"), OsStr::new("HEAD")],
    )
    .with_context(|| format!("{} has no commit to copy (is it a git repository?)", repo.display()))
}

/// Where `repo` sits inside its git repository (`""` at the top, `sub/dir/`
/// below it), so the same folder can be found in a copy.
pub fn prefix(repo: &Path) -> Result<String> {
    git(repo, &[OsStr::new("rev-parse"), OsStr::new("--show-prefix")])
}

/// The top folder of the git repository that holds `repo`.
pub fn toplevel(repo: &Path) -> Result<PathBuf> {
    let top = git(repo, &[OsStr::new("rev-parse"), OsStr::new("--show-toplevel")])?;
    let top = PathBuf::from(top);
    Ok(top.canonicalize().unwrap_or(top))
}

/// One detached worktree. Dropping it removes the folder and git's record of it.
pub struct Worktree {
    repo: PathBuf,
    path: PathBuf,
}

impl Worktree {
    /// Check out HEAD of `repo` into `path`, a folder that must not hold
    /// anything else: what is left there from an earlier run is removed first.
    pub fn add(repo: &Path, path: &Path) -> Result<Worktree> {
        if path.exists() {
            remove(repo, path);
        }
        let added = git(
            repo,
            &[
                OsStr::new("worktree"),
                OsStr::new("add"),
                OsStr::new("--detach"),
                path.as_os_str(),
                OsStr::new("HEAD"),
            ],
        );
        if let Err(e) = added {
            // A checkout that failed half way (disk full, Ctrl-C) leaves no copy behind either.
            remove(repo, path);
            return Err(e);
        }
        let path = path
            .canonicalize()
            .with_context(|| format!("the copy {} was not created", path.display()))?;
        Ok(Worktree {
            repo: repo.to_path_buf(),
            path,
        })
    }

    /// The copy, as a canonical path: the path the agent and the daemon see.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Remove a worktree, whatever state the session left it in, and forget it.
fn remove(repo: &Path, path: &Path) {
    let _ = git(
        repo,
        &[
            OsStr::new("worktree"),
            OsStr::new("remove"),
            OsStr::new("--force"),
            path.as_os_str(),
        ],
    );
    if path.exists() {
        let _ = std::fs::remove_dir_all(path);
    }
    let _ = git(repo, &[OsStr::new("worktree"), OsStr::new("prune")]);
}

impl Drop for Worktree {
    fn drop(&mut self) {
        remove(&self.repo, &self.path);
    }
}
