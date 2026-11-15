//! Git state of the indexed root: the commit an index run belongs to, and the
//! files that differ from that commit.

use anyhow::{Context, Result};
use git2::{ErrorCode, Repository, Status, StatusOptions};
use std::path::Path;

/// The repository that contains `root`, or `None` when there is none.
fn discover(root: &Path) -> Result<Option<Repository>> {
    match Repository::discover(root) {
        Ok(repo) => Ok(Some(repo)),
        Err(e) if e.code() == ErrorCode::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("open git repository at {}", root.display())),
    }
}

/// Id of the commit `HEAD` points at. `None` outside a git repository and in a
/// repository that has no commit yet.
pub fn head_commit(root: &Path) -> Option<String> {
    let repo = discover(root).ok()??;
    let commit = repo.head().ok()?.peel_to_commit().ok()?;
    Some(commit.id().to_string())
}

/// Paths under `root` that differ from `HEAD`: modified, added, deleted and
/// untracked files, staged or not. Ignored files are left out. Paths are relative
/// to `root` with forward slashes and sorted; `root` may be a subdirectory of the
/// repository. Outside a git repository the result is empty.
pub fn changed_files(root: &Path) -> Result<Vec<String>> {
    let Some(repo) = discover(root)? else {
        return Ok(Vec::new());
    };
    // A bare repository has no working files to compare.
    let Some(workdir) = repo.workdir() else {
        return Ok(Vec::new());
    };
    let prefix = root_prefix(root, workdir)?;

    let mut opts = StatusOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(false)
        .exclude_submodules(true);
    let statuses = repo.statuses(Some(&mut opts)).context("git status")?;

    let mut out = Vec::new();
    for entry in statuses.iter() {
        if !entry.status().intersects(changed()) {
            continue;
        }
        // Git reports paths relative to the working directory, with forward slashes.
        let path = String::from_utf8_lossy(entry.path_bytes());
        if let Some(rel) = path.strip_prefix(&prefix) {
            out.push(rel.to_string());
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// Every status that means "the file is not what `HEAD` has".
fn changed() -> Status {
    Status::INDEX_NEW
        | Status::INDEX_MODIFIED
        | Status::INDEX_DELETED
        | Status::INDEX_RENAMED
        | Status::INDEX_TYPECHANGE
        | Status::WT_NEW
        | Status::WT_MODIFIED
        | Status::WT_DELETED
        | Status::WT_RENAMED
        | Status::WT_TYPECHANGE
        | Status::CONFLICTED
}

/// `root` relative to the working directory, as the prefix git paths under it
/// start with: `""` for the working directory itself, `"services/api/"` below it.
fn root_prefix(root: &Path, workdir: &Path) -> Result<String> {
    // Both are made canonical so a symlinked temp directory compares equal.
    let root = root.canonicalize().context("root path")?;
    let workdir = workdir.canonicalize().context("git working directory")?;
    let rel = root
        .strip_prefix(&workdir)
        .context("root is outside the git working directory")?;
    let mut prefix = rel.to_string_lossy().replace('\\', "/");
    if !prefix.is_empty() {
        prefix.push('/');
    }
    Ok(prefix)
}

/// One commit of the repository's history.
#[derive(Debug, Clone, PartialEq)]
pub struct CommitInfo {
    pub sha: String,
    /// Unix seconds.
    pub time: i64,
    /// First line of the message.
    pub subject: String,
    /// Paths it changed under `root`, relative to `root`.
    pub files: Vec<String>,
    /// More than one parent: a merge, usually a merged pull request.
    pub merge: bool,
}

/// The newest `limit` commits reachable from HEAD, newest first, with the
/// files each changed (against its first parent). Commits that change nothing
/// under `root` are left out. Empty outside a repository or before the first commit.
pub fn recent_commits(root: &Path, limit: usize) -> Result<Vec<CommitInfo>> {
    let Some(repo) = discover(root)? else {
        return Ok(Vec::new());
    };
    let Some(workdir) = repo.workdir() else {
        return Ok(Vec::new());
    };
    let prefix = root_prefix(root, workdir)?;
    let mut walk = repo.revwalk()?;
    if walk.push_head().is_err() {
        return Ok(Vec::new());
    }
    walk.set_sorting(git2::Sort::TIME)?;
    let mut out = Vec::new();
    for oid in walk.take(limit * 4) {
        if out.len() >= limit {
            break;
        }
        let commit = repo.find_commit(oid?)?;
        let tree = commit.tree()?;
        let parent_tree = commit.parent(0).ok().map(|p| p.tree()).transpose()?;
        let diff = repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), None)?;
        let mut files: Vec<String> = diff
            .deltas()
            .filter_map(|d| {
                d.new_file()
                    .path()
                    .or_else(|| d.old_file().path())
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
            })
            .filter_map(|p| p.strip_prefix(&prefix).map(str::to_string))
            .collect();
        files.sort();
        files.dedup();
        if files.is_empty() {
            continue;
        }
        out.push(CommitInfo {
            sha: commit.id().to_string(),
            time: commit.time().seconds(),
            subject: commit
                .summary_bytes()
                .map(|b| String::from_utf8_lossy(b).to_string())
                .unwrap_or_default(),
            files,
            merge: commit.parent_count() > 1,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(root: &Path, rel: &str, text: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    /// Stages everything and commits it.
    fn commit_all(repo: &Repository, message: &str) -> git2::Oid {
        let mut index = repo.index().unwrap();
        index.add_all(["*"], git2::IndexAddOption::DEFAULT, None).unwrap();
        // add_all does not drop files that were deleted from the working directory.
        index.update_all(["*"], None).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
        let parents: Vec<&git2::Commit> = parent.iter().collect();
        repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &parents).unwrap()
    }

    #[test]
    fn outside_a_repository_there_is_no_commit_and_no_change() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.py", "x = 1\n");
        assert_eq!(head_commit(dir.path()), None);
        assert_eq!(changed_files(dir.path()).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn a_repository_without_commits_has_no_head_but_lists_its_files() {
        let dir = tempfile::tempdir().unwrap();
        Repository::init(dir.path()).unwrap();
        write(dir.path(), "a.py", "x = 1\n");
        assert_eq!(head_commit(dir.path()), None);
        assert_eq!(changed_files(dir.path()).unwrap(), vec!["a.py"]);
    }

    #[test]
    fn changed_files_lists_modified_added_deleted_and_untracked() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let repo = Repository::init(root).unwrap();
        write(root, ".gitignore", "*.log\nignored_dir/\n");
        write(root, "api/kept.py", "x = 1\n");
        write(root, "api/modified.py", "x = 1\n");
        write(root, "api/deleted.py", "x = 1\n");
        write(root, "web/staged_change.ts", "let x = 1;\n");
        let first = commit_all(&repo, "first");

        assert_eq!(head_commit(root), Some(first.to_string()));
        assert_eq!(changed_files(root).unwrap(), Vec::<String>::new());

        write(root, "api/modified.py", "x = 2\n");
        fs::remove_file(root.join("api/deleted.py")).unwrap();
        write(root, "api/untracked.py", "y = 1\n");
        write(root, "api/new/deep/untracked.py", "y = 1\n");
        write(root, "api/debug.log", "noise\n");
        write(root, "ignored_dir/skipped.py", "y = 1\n");
        // Staged, not committed: one new file and one edit.
        write(root, "web/added.ts", "let y = 1;\n");
        write(root, "web/staged_change.ts", "let x = 2;\n");
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("web/added.ts")).unwrap();
        index.add_path(Path::new("web/staged_change.ts")).unwrap();
        index.write().unwrap();

        assert_eq!(
            changed_files(root).unwrap(),
            vec![
                "api/deleted.py",
                "api/modified.py",
                "api/new/deep/untracked.py",
                "api/untracked.py",
                "web/added.ts",
                "web/staged_change.ts",
            ]
        );
        // Nothing was committed, so HEAD has not moved.
        assert_eq!(head_commit(root), Some(first.to_string()));

        // A subdirectory as root: only its files, relative to it.
        let api = root.join("api");
        assert_eq!(head_commit(&api), Some(first.to_string()));
        assert_eq!(
            changed_files(&api).unwrap(),
            vec!["deleted.py", "modified.py", "new/deep/untracked.py", "untracked.py"]
        );
        assert_eq!(
            changed_files(&root.join("web")).unwrap(),
            vec!["added.ts", "staged_change.ts"]
        );

        // Committing everything clears the list and moves HEAD.
        let second = commit_all(&repo, "second");
        assert_ne!(first, second);
        assert_eq!(head_commit(root), Some(second.to_string()));
        assert_eq!(changed_files(root).unwrap(), Vec::<String>::new());
    }
}
