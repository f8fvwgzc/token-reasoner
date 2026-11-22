//! What changed recently: git commits, merged pull requests included, as
//! facts in the long-term memory. A fact is linked to the files the commit
//! touched, so asking about that code recalls its recent history; the model
//! does not have to run `git log` and read diffs to find out.

use anyhow::Result;
use lge_core::{file_id, memory_kind, MemoryRec};
use lge_store::GraphStore;
use std::collections::HashSet;

/// One commit, as the indexer reads it from git.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Commit {
    pub sha: String,
    /// Unix seconds.
    pub time: i64,
    /// First line of the message.
    pub subject: String,
    /// Paths changed, relative to the repository root.
    pub files: Vec<String>,
    /// True for a merge commit (how a merged pull request usually lands).
    pub merge: bool,
}

/// Files named in one fact at most; the rest are counted.
const MAX_FILES_NAMED: usize = 8;

/// A commit that changes more than this many files under the root is a mass
/// commit (an initial import, a rename of a tree, a formatter run). It says
/// nothing about any one file, so it is stored without links and tagged
/// `mass-commit`; recall also never admits a fact with more links than this.
pub const MASS_FILES: usize = 200;

/// The key a commit's fact is stored under.
pub fn commit_key(sha: &str) -> String {
    format!("commit:{}", &sha[..sha.len().min(12)])
}

fn text_of(c: &Commit) -> String {
    let short = &c.sha[..c.sha.len().min(7)];
    let what = if c.merge { "Merged" } else { "Commit" };
    let mut text = format!(
        "{what} {short} on {}: {}",
        crate::recall::date(c.time),
        c.subject.trim()
    );
    if !c.files.is_empty() {
        let named: Vec<&str> = c.files.iter().take(MAX_FILES_NAMED).map(String::as_str).collect();
        text.push_str(&format!(" (changed {}", named.join(", ")));
        if c.files.len() > MAX_FILES_NAMED {
            text.push_str(&format!(" and {} more", c.files.len() - MAX_FILES_NAMED));
        }
        text.push(')');
    }
    text.chars().take(crate::MAX_FACT_CHARS).collect()
}

/// Store the commits that are not in memory yet. `commits` is newest first;
/// the walk stops at the first commit already stored, so a repeated call costs
/// one lookup. Returns how many were stored.
pub fn ingest_commits(store: &dyn GraphStore, commits: &[Commit], now: i64) -> Result<usize> {
    let indexed: HashSet<String> = store.file_hashes()?.into_keys().collect();
    let mut stored = 0;
    for c in commits {
        if c.sha.is_empty() || c.subject.trim().is_empty() {
            continue;
        }
        let key = commit_key(&c.sha);
        if !store.memories_by_key(&key)?.is_empty() {
            break;
        }
        let mut tags = vec!["commit".to_string()];
        if c.merge {
            tags.push("merge".to_string());
        }
        let mass = c.files.len() > MASS_FILES;
        let links: Vec<String> = if mass {
            tags.push("mass-commit".to_string());
            Vec::new()
        } else {
            c.files
                .iter()
                .filter(|f| indexed.contains(*f))
                .map(|f| file_id(f))
                .collect()
        };
        store.put_memory(&MemoryRec {
            id: lge_core::stable_id(&["memory", &key]),
            kind: memory_kind::EXPERIENCE.to_string(),
            text: text_of(c),
            key,
            tags,
            source: "git".to_string(),
            created_at: now,
            valid_at: c.time,
            invalid_at: 0,
            expired_at: 0,
            proof_count: 1,
            links,
        })?;
        stored += 1;
    }
    Ok(stored)
}

/// Commits read per call at most. The walk stops earlier at the first commit
/// already in memory, so this bounds only the first run on a long history.
pub const HISTORY_DEPTH: usize = 200;

/// Read the repository's recent history from git and store what is new.
pub fn ingest_repo_history(store: &dyn GraphStore, root: &std::path::Path, now: i64) -> Result<usize> {
    // Nothing new since the last call: one lookup, no walk.
    match lge_index::git::head_commit(root) {
        Some(head) if !store.memories_by_key(&commit_key(&head))?.is_empty() => return Ok(0),
        None => return Ok(0),
        _ => {}
    }
    let commits: Vec<Commit> = lge_index::git::recent_commits(root, HISTORY_DEPTH)?
        .into_iter()
        .map(|c| Commit {
            sha: c.sha,
            time: c.time,
            subject: c.subject,
            files: c.files,
            merge: c.merge,
        })
        .collect();
    ingest_commits(store, &commits, now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_commit_reads_as_one_short_fact() {
        let c = Commit {
            sha: "381f402ee3c8a1b2".into(),
            time: 1_794_441_600,
            subject: "Reject unknown discount codes  ".into(),
            files: vec!["pricing.py".into(), "tests/test_pricing.py".into()],
            merge: false,
        };
        assert_eq!(
            text_of(&c),
            "Commit 381f402 on 2026-11-12: Reject unknown discount codes (changed pricing.py, tests/test_pricing.py)"
        );
        let many = Commit {
            merge: true,
            files: (0..11).map(|i| format!("f{i}.py")).collect(),
            ..c
        };
        let text = text_of(&many);
        assert!(
            text.starts_with("Merged 381f402 on 2026-11-12:") && text.ends_with("f7.py and 3 more)"),
            "{text}"
        );
        assert_eq!(commit_key("381f402ee3c8a1b2"), "commit:381f402ee3c8");
        assert_eq!(commit_key("abc"), "commit:abc");
    }
}
