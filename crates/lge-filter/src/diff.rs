//! The part of the working-tree diff that belongs to the failures.
//!
//! A failing test rarely needs the whole diff next to it: the hunks of the files the
//! failures name are enough, and the ones closest to the failing lines matter most.

use crate::format::clip_chars;
use crate::parse::pattern;
use crate::run::rebase;
use anyhow::{bail, Result};
use std::path::Path;
use std::process::{Command, Output, Stdio};

/// A hunk longer than this is cut down to a window around the failing line. New files
/// and rewrites would otherwise use the whole budget on one hunk.
const MAX_HUNK_LINES: usize = 60;
/// Longest diff line kept, in characters.
const MAX_LINE_CHARS: usize = 300;
/// Untracked files turned into new-file hunks, at most.
const MAX_NEW_FILES: usize = 20;
/// Files passed to git, at most: a lint run can name thousands, a command line cannot.
const MAX_TARGETS: usize = 500;

pattern!(HUNK_HEADER, r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@(.*)$");

/// One file named by a failure, with the failing lines known for it.
struct Target {
    path: String,
    lines: Vec<u32>,
}

struct FileDiff {
    path: String,
    /// The `--- a/path` and `+++ b/path` lines.
    old: String,
    new: String,
    hunks: Vec<Hunk>,
}

#[derive(Clone)]
struct Hunk {
    old_start: u32,
    new_start: u32,
    /// What git prints after the second `@@`: the enclosing function, usually.
    section: String,
    /// Body lines with their ` `, `+`, `-` or `\` prefix.
    lines: Vec<String>,
}

/// Returns the `git diff` hunks (working tree against `HEAD`, staged or not) of the
/// files in `files`, at most `max_bytes` long.
///
/// * Each entry of `files` is a path relative to `root`, optionally followed by
///   `:line` (`shop/pricing.py:8`). The same path may appear with several lines.
/// * A named file git does not track yet is shown as a new file.
/// * Hunks are chosen nearest to a failing line first, then in the order the files
///   were named; a hunk that does not fit is left out and counted in a closing line.
///   Chosen hunks are printed file by file, in file order, so the result reads as a diff.
/// * A long hunk is cut to a window around the failing line.
///
/// Outside a git repository, or without the `git` program, the result is empty.
pub fn relevant_diff_hunks(root: &Path, files: &[String], max_bytes: usize) -> Result<String> {
    let mut targets = targets(root, files);
    targets.truncate(MAX_TARGETS);
    if targets.is_empty() || max_bytes == 0 {
        return Ok(String::new());
    }
    let inside = git(root, &["rev-parse", "--is-inside-work-tree"], &[]);
    if !inside.is_some_and(|o| o.status.success() && o.stdout.starts_with(b"true")) {
        return Ok(String::new());
    }
    let paths: Vec<&str> = targets.iter().map(|t| t.path.as_str()).collect();

    // A repository without a commit has no HEAD to compare with: only the index.
    let has_head = git(root, &["rev-parse", "--verify", "--quiet", "HEAD"], &[]).is_some_and(|o| o.status.success());
    let against = if has_head { "HEAD" } else { "--cached" };
    let mut text = git_text(root, &[DIFF_ARGS, &[against]].concat(), &paths, &[0])?;

    let untracked = git_text(
        root,
        &["ls-files", "--others", "--exclude-standard", "-z"],
        &paths,
        &[0],
    )?;
    for path in untracked.split('\0').filter(|p| !p.is_empty()).take(MAX_NEW_FILES) {
        // `--no-index` compares two files outside the index; it exits 1 when they differ.
        text.push_str(&git_text(
            root,
            &[DIFF_ARGS, &["--no-index"]].concat(),
            &["/dev/null", path],
            &[0, 1],
        )?);
    }

    let mut diffs = parse_diff(&text);
    for diff in &mut diffs {
        let wanted = wanted_lines(&targets, &diff.path);
        for hunk in &mut diff.hunks {
            *hunk = hunk.window(wanted, MAX_HUNK_LINES);
        }
    }
    Ok(select(&diffs, &targets, max_bytes))
}

const DIFF_ARGS: &[&str] = &[
    "diff",
    "--no-color",
    "--no-ext-diff",
    "--no-textconv",
    "--relative",
    "--src-prefix=a/",
    "--dst-prefix=b/",
    "--inter-hunk-context=0",
    "-U3",
];

/// Runs git in `root`. `None` when git cannot be started. Paths are passed literally
/// (no globbing), and the index is not refreshed, so this never waits on a lock.
fn git(root: &Path, args: &[&str], paths: &[&str]) -> Option<Output> {
    let mut command = Command::new("git");
    command
        .arg("--literal-pathspecs")
        .arg("-C")
        .arg(root)
        .args(["-c", "core.quotepath=off"])
        .args(args);
    if !paths.is_empty() {
        command.arg("--").args(paths);
    }
    command
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .output()
        .ok()
}

/// Standard output of a git command that must succeed with one of the `ok` exit codes.
fn git_text(root: &Path, args: &[&str], paths: &[&str], ok: &[i32]) -> Result<String> {
    let Some(output) = git(root, args, paths) else {
        return Ok(String::new());
    };
    if !output.status.code().is_some_and(|code| ok.contains(&code)) {
        bail!(
            "git {} failed in {}: {}",
            args.first().copied().unwrap_or(""),
            root.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Reads the `path` and `path:line` entries, merging the lines of repeated paths.
/// Paths outside `root` cannot be in its diff and are dropped.
fn targets(root: &Path, files: &[String]) -> Vec<Target> {
    let mut targets: Vec<Target> = Vec::new();
    for entry in files {
        let (path, line) = match entry.rsplit_once(':') {
            Some((path, line)) if !line.is_empty() && line.bytes().all(|b| b.is_ascii_digit()) => {
                (path, line.parse::<u32>().ok())
            }
            _ => (entry.as_str(), None),
        };
        let path = rebase(path.trim(), root, root);
        if path.is_empty() || path == "." || Path::new(&path).is_absolute() || path.starts_with("../") {
            continue;
        }
        let index = match targets.iter().position(|t| t.path == path) {
            Some(index) => index,
            None => {
                targets.push(Target {
                    path,
                    lines: Vec::new(),
                });
                targets.len() - 1
            }
        };
        targets[index].lines.extend(line);
    }
    targets
}

fn wanted_lines<'a>(targets: &'a [Target], path: &str) -> &'a [u32] {
    targets
        .iter()
        .find(|t| t.path == path)
        .map_or(&[][..], |t| t.lines.as_slice())
}

// ---------------------------------------------------------------------------
// Reading a unified diff
// ---------------------------------------------------------------------------

fn parse_diff(text: &str) -> Vec<FileDiff> {
    let mut diffs: Vec<FileDiff> = Vec::new();
    // Lines the current hunk still expects on the old and the new side. While either
    // is above zero a line is hunk content, whatever it starts with: a removed SQL
    // comment looks like `--- comment`.
    let (mut old_left, mut new_left) = (0u32, 0u32);

    for line in text.lines() {
        let in_hunk = old_left > 0 || new_left > 0;
        if !in_hunk && line.starts_with("diff --git ") {
            diffs.push(FileDiff {
                path: String::new(),
                old: String::new(),
                new: String::new(),
                hunks: Vec::new(),
            });
            continue;
        }
        let Some(diff) = diffs.last_mut() else {
            continue;
        };
        if in_hunk || line.starts_with('\\') {
            let Some(hunk) = diff.hunks.last_mut() else {
                continue;
            };
            match line.as_bytes().first() {
                Some(b'+') => new_left = new_left.saturating_sub(1),
                Some(b'-') => old_left = old_left.saturating_sub(1),
                Some(b'\\') => {}
                _ => {
                    old_left = old_left.saturating_sub(1);
                    new_left = new_left.saturating_sub(1);
                }
            }
            // An empty line is a context line that lost its leading space.
            let line = if line.is_empty() { " " } else { line };
            hunk.lines.push(clip_chars(line, MAX_LINE_CHARS));
        } else if let Some(c) = HUNK_HEADER.captures(line) {
            let number = |i: usize, default: u32| c.get(i).and_then(|m| m.as_str().parse().ok()).unwrap_or(default);
            old_left = number(2, 1);
            new_left = number(4, 1);
            diff.hunks.push(Hunk {
                old_start: number(1, 0),
                new_start: number(3, 0),
                section: c.get(5).map_or("", |m| m.as_str()).to_string(),
                lines: Vec::new(),
            });
        } else if diff.hunks.is_empty() {
            // git puts a tab after a path that contains spaces.
            if let Some(old) = line.strip_prefix("--- ") {
                diff.old = line.trim_end_matches('\t').to_string();
                if diff.path.is_empty() {
                    diff.path = strip_side(old);
                }
            } else if let Some(new) = line.strip_prefix("+++ ") {
                diff.new = line.trim_end_matches('\t').to_string();
                if new.trim_end_matches('\t') != "/dev/null" {
                    diff.path = strip_side(new);
                }
            }
        }
    }
    diffs.retain(|d| !d.hunks.is_empty() && !d.path.is_empty());
    diffs
}

/// `a/shop/pricing.py` -> `shop/pricing.py`.
fn strip_side(side: &str) -> String {
    let side = side.trim_end_matches('\t').trim_matches('"');
    side.strip_prefix("a/")
        .or_else(|| side.strip_prefix("b/"))
        .unwrap_or(side)
        .to_string()
}

impl Hunk {
    fn counts(&self) -> (u32, u32) {
        let count = |sides: &[u8]| {
            let n = self.lines.iter().filter(|l| sides.contains(&l.as_bytes()[0])).count();
            u32::try_from(n).unwrap_or(u32::MAX)
        };
        (count(b" -"), count(b" +"))
    }

    fn render(&self) -> String {
        let (old_len, new_len) = self.counts();
        let mut out = format!(
            "@@ -{},{old_len} +{},{new_len} @@{}\n",
            self.old_start, self.new_start, self.section
        );
        for line in &self.lines {
            out.push_str(line);
            out.push('\n');
        }
        out
    }

    /// The line numbers this hunk covers in the new file.
    fn new_range(&self) -> (u32, u32) {
        let (_, new_len) = self.counts();
        (self.new_start, self.new_start + new_len.saturating_sub(1))
    }

    /// Distance from the nearest wanted line; zero when one is inside the hunk.
    /// Unknown (no wanted line) sorts last.
    fn distance(&self, wanted: &[u32]) -> u32 {
        let (first, last) = self.new_range();
        wanted
            .iter()
            .map(|&w| if w < first { first - w } else { w.saturating_sub(last) })
            .min()
            .unwrap_or(u32::MAX)
    }

    /// At most `max` body lines of this hunk, centred on the wanted line nearest to it
    /// (the top of the hunk when there is none), with the header numbers recomputed.
    fn window(&self, wanted: &[u32], max: usize) -> Hunk {
        if self.lines.len() <= max {
            return self.clone();
        }
        // The old and new line number each body line sits at.
        let mut positions = Vec::with_capacity(self.lines.len());
        let (mut old, mut new) = (self.old_start, self.new_start);
        for line in &self.lines {
            positions.push((old, new));
            match line.as_bytes()[0] {
                b'+' => new += 1,
                b'-' => old += 1,
                b'\\' => {}
                _ => {
                    old += 1;
                    new += 1;
                }
            }
        }
        let centre = positions
            .iter()
            .enumerate()
            .min_by_key(|(i, (_, new))| (wanted.iter().map(|w| w.abs_diff(*new)).min().unwrap_or(u32::MAX), *i))
            .map_or(0, |(i, _)| i);
        let start = centre.saturating_sub(max / 2).min(self.lines.len() - max);
        let (old_start, new_start) = positions[start];
        Hunk {
            old_start,
            new_start,
            section: self.section.clone(),
            lines: self.lines[start..start + max].to_vec(),
        }
    }
}

// ---------------------------------------------------------------------------
// Choosing what fits
// ---------------------------------------------------------------------------

fn omitted_line(count: usize) -> String {
    format!("... {count} more hunk{} not shown\n", if count == 1 { "" } else { "s" })
}

/// Picks hunks by distance until `max_bytes` is used and prints them file by file.
fn select(diffs: &[FileDiff], targets: &[Target], max_bytes: usize) -> String {
    // (distance, order the file was named in, file, hunk)
    let mut ranked: Vec<(u32, usize, usize, usize)> = Vec::new();
    for (f, diff) in diffs.iter().enumerate() {
        let named = targets.iter().position(|t| t.path == diff.path).unwrap_or(usize::MAX);
        let wanted = wanted_lines(targets, &diff.path);
        for (h, hunk) in diff.hunks.iter().enumerate() {
            ranked.push((hunk.distance(wanted), named, f, h));
        }
    }
    ranked.sort();

    let all = pick(diffs, &ranked, max_bytes);
    if all.omitted == 0 {
        return all.text;
    }
    // Not everything fits: choose again with room for the line that says so.
    let reserve = omitted_line(ranked.len()).len();
    let mut some = pick(diffs, &ranked, max_bytes.saturating_sub(reserve));
    if some.text.is_empty() {
        // Not even the nearest hunk fits whole: show as much of it as there is room for.
        if let Some(&(_, _, f, h)) = ranked.first() {
            let wanted = wanted_lines(targets, &diffs[f].path);
            let budget = max_bytes.saturating_sub(reserve);
            let cut = (1..diffs[f].hunks[h].lines.len())
                .rev()
                .map(|size| file_text(&diffs[f], &[diffs[f].hunks[h].window(wanted, size)]))
                .find(|text| text.len() <= budget);
            if let Some(text) = cut {
                some.text = text;
                some.omitted -= 1;
            }
        }
    }
    let line = omitted_line(some.omitted);
    if some.omitted > 0 && some.text.len() + line.len() <= max_bytes {
        some.text.push_str(&line);
    }
    some.text
}

struct Picked {
    text: String,
    omitted: usize,
}

fn pick(diffs: &[FileDiff], ranked: &[(u32, usize, usize, usize)], budget: usize) -> Picked {
    let mut chosen: Vec<Vec<usize>> = vec![Vec::new(); diffs.len()];
    let mut file_order: Vec<usize> = Vec::new();
    let (mut used, mut omitted) = (0, 0);
    for &(_, _, f, h) in ranked {
        let header = if chosen[f].is_empty() {
            diffs[f].old.len() + diffs[f].new.len() + 2
        } else {
            0
        };
        let cost = header + diffs[f].hunks[h].render().len();
        if used + cost > budget {
            omitted += 1;
            continue;
        }
        used += cost;
        if chosen[f].is_empty() {
            file_order.push(f);
        }
        chosen[f].push(h);
    }
    let mut text = String::new();
    for f in file_order {
        chosen[f].sort_unstable();
        let hunks: Vec<Hunk> = chosen[f].iter().map(|&h| diffs[f].hunks[h].clone()).collect();
        text.push_str(&file_text(&diffs[f], &hunks));
    }
    Picked { text, omitted }
}

fn file_text(diff: &FileDiff, hunks: &[Hunk]) -> String {
    let mut out = format!("{}\n{}\n", diff.old, diff.new);
    for hunk in hunks {
        out.push_str(&hunk.render());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A repository with one commit holding `files`.
    fn repo(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        run_git(dir.path(), &["init", "--quiet"]);
        for (path, text) in files {
            write(dir.path(), path, text);
        }
        if !files.is_empty() {
            run_git(dir.path(), &["add", "--all"]);
            run_git(dir.path(), &["commit", "--quiet", "--message", "first"]);
        }
        dir
    }

    fn run_git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            // The developer's own configuration (hooks, templates) must not reach the test.
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    fn write(root: &Path, rel: &str, text: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// Indented, so git finds no "function" line to print after the hunk header.
    fn numbered(count: usize) -> String {
        (1..=count).map(|i| format!("  line {i}\n")).collect()
    }

    fn names(files: &[&str]) -> Vec<String> {
        files.iter().map(|f| f.to_string()).collect()
    }

    #[test]
    fn outside_a_repository_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.py", "x = 1\n");
        assert_eq!(relevant_diff_hunks(dir.path(), &names(&["a.py:1"]), 8_000).unwrap(), "");
        assert_eq!(
            relevant_diff_hunks(&dir.path().join("missing"), &names(&["a.py"]), 8_000).unwrap(),
            ""
        );
    }

    #[test]
    fn only_the_named_files_are_shown() {
        let dir = repo(&[
            ("shop/pricing.py", "def rate():\n    return 1\n"),
            ("shop/cart.py", "total = 0\n"),
        ]);
        write(dir.path(), "shop/pricing.py", "def rate():\n    return 2\n");
        write(dir.path(), "shop/cart.py", "total = 1\n");
        let diff = relevant_diff_hunks(dir.path(), &names(&["shop/pricing.py:2"]), 8_000).unwrap();
        assert_eq!(
            diff,
            "--- a/shop/pricing.py\n+++ b/shop/pricing.py\n@@ -1,2 +1,2 @@\n def rate():\n-    return 1\n+    return 2\n"
        );
        // Nothing named, nothing changed, or no room: nothing shown.
        assert_eq!(relevant_diff_hunks(dir.path(), &[], 8_000).unwrap(), "");
        assert_eq!(
            relevant_diff_hunks(dir.path(), &names(&["README.md", "/etc/hosts:3", "../x.py"]), 8_000).unwrap(),
            ""
        );
        assert_eq!(
            relevant_diff_hunks(dir.path(), &names(&["shop/pricing.py"]), 0).unwrap(),
            ""
        );
    }

    #[test]
    fn staged_changes_count_too() {
        let dir = repo(&[("a.py", "x = 1\n")]);
        write(dir.path(), "a.py", "x = 2\n");
        run_git(dir.path(), &["add", "a.py"]);
        let diff = relevant_diff_hunks(dir.path(), &names(&["./a.py"]), 8_000).unwrap();
        assert_eq!(diff, "--- a/a.py\n+++ b/a.py\n@@ -1,1 +1,1 @@\n-x = 1\n+x = 2\n");
    }

    #[test]
    fn untracked_files_named_in_failures_are_new_file_hunks() {
        let dir = repo(&[("a.py", "x = 1\n")]);
        write(dir.path(), "tests/test_new.py", "def test_new():\n    assert False\n");
        write(dir.path(), "tests/test_other.py", "def test_other():\n    pass\n");
        let diff = relevant_diff_hunks(dir.path(), &names(&["tests/test_new.py:2", "a.py:1"]), 8_000).unwrap();
        assert_eq!(
            diff,
            "--- /dev/null\n+++ b/tests/test_new.py\n@@ -0,0 +1,2 @@\n+def test_new():\n+    assert False\n"
        );
    }

    #[test]
    fn a_repository_without_commits_shows_named_files_as_new() {
        let dir = repo(&[]);
        write(dir.path(), "a.py", "x = 1\n");
        write(dir.path(), "b.py", "y = 1\n");
        run_git(dir.path(), &["add", "b.py"]);
        let diff = relevant_diff_hunks(dir.path(), &names(&["a.py", "b.py"]), 8_000).unwrap();
        assert_eq!(
            diff,
            "--- /dev/null\n+++ b/a.py\n@@ -0,0 +1,1 @@\n+x = 1\n--- /dev/null\n+++ b/b.py\n@@ -0,0 +1,1 @@\n+y = 1\n"
        );
    }

    #[test]
    fn hunks_nearest_to_the_failing_line_come_first_when_room_is_short() {
        let dir = repo(&[("big.py", &numbered(300)), ("other.py", "a = 1\n")]);
        let changed = numbered(300)
            .replace("line 10\n", "line ten\n")
            .replace("line 150\n", "line one-fifty\n")
            .replace("line 290\n", "line two-ninety\n");
        write(dir.path(), "big.py", &changed);
        write(dir.path(), "other.py", "a = 2\n");

        // Everything fits: file order of naming, hunks in file order.
        let all = relevant_diff_hunks(dir.path(), &names(&["big.py:151", "other.py"]), 8_000).unwrap();
        let headers: Vec<&str> = all
            .lines()
            .filter(|l| l.starts_with("@@") || l.starts_with("+++"))
            .collect();
        assert_eq!(
            headers,
            [
                "+++ b/big.py",
                "@@ -7,7 +7,7 @@",
                "@@ -147,7 +147,7 @@",
                "@@ -287,7 +287,7 @@",
                "+++ b/other.py",
                "@@ -1,1 +1,1 @@"
            ]
        );

        // Room for one hunk: the one around line 150, and a count of the rest.
        let one = relevant_diff_hunks(dir.path(), &names(&["big.py:151", "other.py"]), 200).unwrap();
        assert!(one.len() <= 200);
        assert!(one.starts_with("--- a/big.py\n+++ b/big.py\n@@ -147,7 +147,7 @@\n   line 147\n"));
        assert!(one.contains("-  line 150\n+  line one-fifty\n"));
        assert!(one.ends_with("... 3 more hunks not shown\n"));

        // Room for two: the next nearest is line 290 (139 lines away), not line 10 (141).
        let two = relevant_diff_hunks(dir.path(), &names(&["big.py:151"]), 330).unwrap();
        let headers: Vec<&str> = two.lines().filter(|l| l.starts_with("@@")).collect();
        assert_eq!(headers, ["@@ -147,7 +147,7 @@", "@@ -287,7 +287,7 @@"]);
        assert!(two.ends_with("... 1 more hunk not shown\n"));

        // Two failing lines in one file: both of their hunks win over the third.
        let both = relevant_diff_hunks(dir.path(), &names(&["big.py:12", "big.py:288"]), 330).unwrap();
        let headers: Vec<&str> = both.lines().filter(|l| l.starts_with("@@")).collect();
        assert_eq!(headers, ["@@ -7,7 +7,7 @@", "@@ -287,7 +287,7 @@"]);
    }

    #[test]
    fn a_long_hunk_is_cut_to_a_window_around_the_failing_line() {
        let dir = repo(&[("a.py", "x = 1\n")]);
        write(dir.path(), "generated.py", &numbered(400));
        let diff = relevant_diff_hunks(dir.path(), &names(&["generated.py:250"]), 8_000).unwrap();
        let lines: Vec<&str> = diff.lines().collect();
        assert_eq!(
            &lines[..4],
            [
                "--- /dev/null",
                "+++ b/generated.py",
                "@@ -0,0 +220,60 @@",
                "+  line 220"
            ]
        );
        assert_eq!(lines.len(), 3 + MAX_HUNK_LINES);
        assert_eq!(lines[lines.len() - 1], "+  line 279");

        // No failing line known: the top of the file.
        let top = relevant_diff_hunks(dir.path(), &names(&["generated.py"]), 8_000).unwrap();
        assert!(top.contains("@@ -0,0 +1,60 @@\n+  line 1\n"));
        assert!(top.ends_with("+  line 60\n"));
    }

    #[test]
    fn the_cap_holds_even_when_no_hunk_fits_whole() {
        let dir = repo(&[("a.py", &numbered(50))]);
        write(
            dir.path(),
            "a.py",
            &numbered(50).replace("line 25\n", "line twenty-five\n"),
        );
        write(dir.path(), "new.py", &numbered(500));
        let files = names(&["a.py:25", "new.py:400"]);
        for max in [0, 10, 40, 80, 120, 140, 200, 500, 2_000, 20_000] {
            let diff = relevant_diff_hunks(dir.path(), &files, max).unwrap();
            assert!(diff.len() <= max, "max {max}: {} bytes", diff.len());
        }
        // A budget smaller than the nearest hunk still shows the lines around the failure.
        let tight = relevant_diff_hunks(dir.path(), &files, 140).unwrap();
        assert!(tight.starts_with("--- a/a.py\n+++ b/a.py\n@@ "));
        assert!(tight.contains("-  line 25\n+  line twenty-five\n"));
        assert!(tight.ends_with("... 1 more hunk not shown\n"));
    }

    #[test]
    fn a_root_inside_the_repository_gives_paths_relative_to_it() {
        let dir = repo(&[("services/api/app.py", "x = 1\n"), ("web/app.ts", "let x = 1;\n")]);
        write(dir.path(), "services/api/app.py", "x = 2\n");
        write(dir.path(), "web/app.ts", "let x = 2;\n");
        let root = dir.path().join("services/api");
        let diff = relevant_diff_hunks(&root, &names(&["app.py:1"]), 8_000).unwrap();
        assert_eq!(diff, "--- a/app.py\n+++ b/app.py\n@@ -1,1 +1,1 @@\n-x = 1\n+x = 2\n");
    }

    #[test]
    fn hunk_content_that_looks_like_a_header_stays_content() {
        let text = "diff --git a/q.sql b/q.sql\nindex 1..2 100644\n--- a/q.sql\n+++ b/q.sql\n@@ -1,3 +1,3 @@ -- schema\n select 1;\n--- old comment\n+++ new comment\n select 2;\n\\ No newline at end of file\ndiff --git a/r.sql b/r.sql\n--- a/r.sql\n+++ /dev/null\n@@ -1 +0,0 @@\n-drop table r;\n";
        let diffs = parse_diff(text);
        assert_eq!(diffs.len(), 2);
        assert_eq!(diffs[0].path, "q.sql");
        assert_eq!(
            diffs[0].hunks[0].render(),
            "@@ -1,3 +1,3 @@ -- schema\n select 1;\n--- old comment\n+++ new comment\n select 2;\n\\ No newline at end of file\n"
        );
        // A deleted file keeps its old name.
        assert_eq!(
            (diffs[1].path.as_str(), diffs[1].new.as_str()),
            ("r.sql", "+++ /dev/null")
        );
        assert_eq!(diffs[1].hunks[0].render(), "@@ -1,1 +0,0 @@\n-drop table r;\n");
    }

    #[test]
    fn targets_merge_lines_and_drop_what_cannot_be_in_the_diff() {
        let root = Path::new("/repo");
        let got: Vec<(String, Vec<u32>)> = targets(
            root,
            &names(&[
                "a.py:3",
                "./a.py:9",
                "b.py",
                "a.py",
                "/repo/c.py:1",
                "/etc/hosts",
                "../d.py:2",
                "",
                "e.py:x",
            ]),
        )
        .into_iter()
        .map(|t| (t.path, t.lines))
        .collect();
        assert_eq!(
            got,
            [
                ("a.py".to_string(), vec![3, 9]),
                ("b.py".to_string(), vec![]),
                ("c.py".to_string(), vec![1]),
                ("e.py:x".to_string(), vec![]),
            ]
        );
    }

    #[test]
    fn window_recomputes_the_header() {
        let hunk = Hunk {
            old_start: 10,
            new_start: 10,
            section: " fn f()".into(),
            lines: [" a", "-b", "+B", " c", "+d", " e", "-f", " g"]
                .iter()
                .map(|l| l.to_string())
                .collect(),
        };
        assert_eq!(
            hunk.render(),
            "@@ -10,6 +10,6 @@ fn f()\n a\n-b\n+B\n c\n+d\n e\n-f\n g\n"
        );
        assert_eq!(hunk.new_range(), (10, 15));
        assert_eq!(
            (
                hunk.distance(&[12]),
                hunk.distance(&[4]),
                hunk.distance(&[20, 40]),
                hunk.distance(&[])
            ),
            (0, 6, 5, u32::MAX)
        );
        // New line 13 is `+d`: a window of four around it.
        assert_eq!(
            hunk.window(&[13], 4).render(),
            "@@ -12,2 +11,4 @@ fn f()\n+B\n c\n+d\n e\n"
        );
        assert_eq!(hunk.window(&[], 3).render(), "@@ -10,2 +10,2 @@ fn f()\n a\n-b\n+B\n");
        assert_eq!(hunk.window(&[99], 2).render(), "@@ -13,2 +14,1 @@ fn f()\n e\n-f\n");
        assert_eq!(hunk.window(&[13], 8).render(), hunk.render());
    }
}
