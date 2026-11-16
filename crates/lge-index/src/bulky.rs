//! Folders in a repository that searches would wade through: build output,
//! dependencies, logs and archives kept in the tree without being ignored.
//!
//! `rg` and `grep` read every file that `.gitignore` does not exclude, so one
//! forgotten `artifacts/` folder can fill an agent's search output (and its
//! tokens) with junk. [`bulky_dirs`] finds such folders, up to two levels below
//! the root, so the user can add one line to the file [`ignore_file`] names:
//! `.gitignore` inside a git repository, `.ignore` outside one, where `rg`
//! does not read `.gitignore` at all.

use crate::{is_dependency_or_generated, is_generated, lang_of, SKIP_DIRS};
use std::collections::BTreeMap;
use std::path::Path;

/// Folder names that hold generated output, dependencies, logs or archives.
/// Matched without regard to ASCII case.
pub const BULKY_NAMES: &[&str] = &[
    "artifacts",
    "archive",
    "archives",
    "dist",
    "build",
    "out",
    "target",
    "coverage",
    "logs",
    "tmp",
    ".cache",
    "node_modules",
    "vendor",
];

/// When a folder counts as bulky by size, and how far the walk goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BulkyLimits {
    /// A folder over this many bytes, with no indexed source, is bulky.
    pub bytes: u64,
    /// A folder with more files than this, and no indexed source, is bulky.
    pub files: u64,
    /// Files counted in all, over the whole walk. Past it the counts are lower
    /// bounds and folders not reached yet are not counted.
    pub walk_cap: u64,
}

impl Default for BulkyLimits {
    fn default() -> Self {
        BulkyLimits {
            bytes: 50 * 1024 * 1024,
            files: 5_000,
            walk_cap: 200_000,
        }
    }
}

/// One folder that searches would read and probably should not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BulkyDir {
    /// Relative to the root, with forward slashes (`web/dist`).
    pub path: String,
    /// Bytes of the files counted in it (a lower bound when the walk stopped).
    pub bytes: u64,
    /// Files counted in it (a lower bound when the walk stopped).
    pub files: u64,
    /// Why it was picked, in words.
    pub reason: String,
}

impl BulkyDir {
    /// The line that, added to the root's `.gitignore`, hides the folder from
    /// searches and from the index. Glob characters in the path are escaped, so
    /// the line hides this folder and nothing else.
    pub fn ignore_line(&self) -> String {
        let mut line = String::with_capacity(self.path.len() + 2);
        line.push('/');
        for c in self.path.chars() {
            if matches!(c, '\\' | '*' | '?' | '[') {
                line.push('\\');
            }
            line.push(c);
        }
        line.push('/');
        line
    }
}

/// The file under `root` where an ignore line hides a folder from searches.
/// `rg` reads `.gitignore` only inside a git repository (the root or a folder
/// above it has `.git`, a folder or a worktree's file). Outside one it reads
/// `.ignore` but not `.gitignore`, so a line there would quiet lge's warning
/// and still leave the folder in every search. lge reads both.
pub fn ignore_file(root: &Path) -> &'static str {
    if root.ancestors().any(|dir| dir.join(".git").exists()) {
        ".gitignore"
    } else {
        ".ignore"
    }
}

/// [`bulky_dirs_with`] with the default limits: 50 MB or 5,000 files, and a
/// walk that stops after 200,000 files.
pub fn bulky_dirs(root: &Path) -> Vec<BulkyDir> {
    bulky_dirs_with(root, &BulkyLimits::default())
}

/// Folders one or two levels below `root` that `.gitignore` and `.ignore` do
/// not exclude and that look like generated output or archives: a name from
/// [`BULKY_NAMES`] (with at least one file in it), or more than
/// `limits.bytes` / `limits.files` with no file the index would read. A folder
/// inside one already reported is not reported again. Largest first.
///
/// Hidden folders are left out, as `rg` leaves them out, except `.cache`. Never
/// fails: a folder that cannot be read counts as empty.
pub fn bulky_dirs_with(root: &Path, limits: &BulkyLimits) -> Vec<BulkyDir> {
    let Ok(root) = root.canonicalize() else {
        return Vec::new();
    };
    let (top, children) = folders(&root, limits.walk_cap);

    // Count the folders whose name already says what they are first: they are
    // reported even when the walk stops before the others are reached.
    let mut order: Vec<&String> = top.iter().collect();
    order.sort_by_key(|name| (!is_bulky_name(name), name.as_str()));
    let mut counts: BTreeMap<String, Count> = BTreeMap::new();
    let mut seen = 0u64;
    for name in order {
        if seen >= limits.walk_cap {
            break;
        }
        count_folder(&root, name, limits.walk_cap, &mut seen, &mut counts);
    }

    let mut out = Vec::new();
    for name in &top {
        let parent = counts.get(name).copied();
        if let Some(found) = judge(name, name, parent, limits) {
            out.push(found);
            continue;
        }
        // A folder that holds no file has no entry of its own; it was still
        // counted when its parent was counted to the end.
        let parent_whole = parent.is_some_and(|c| !c.cut);
        for child in children.get(name).into_iter().flatten() {
            let path = format!("{name}/{child}");
            let count = counts.get(&path).copied().or_else(|| parent_whole.then(Count::default));
            if let Some(found) = judge(&path, child, count, limits) {
                out.push(found);
            }
        }
    }
    out.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));
    out
}

/// Files, bytes and indexed source files counted under one folder.
#[derive(Debug, Default, Clone, Copy)]
struct Count {
    files: u64,
    bytes: u64,
    source: u64,
    /// The walk stopped inside this folder: the numbers are lower bounds.
    cut: bool,
}

fn is_bulky_name(name: &str) -> bool {
    BULKY_NAMES.iter().any(|n| n.eq_ignore_ascii_case(name))
}

/// What a folder of that name usually holds.
fn kind_of(name: &str) -> &'static str {
    match name.to_ascii_lowercase().as_str() {
        "node_modules" | "vendor" => "dependencies",
        "artifacts" | "archive" | "archives" => "archives or build artifacts",
        "coverage" => "coverage reports",
        "logs" => "logs",
        "tmp" | ".cache" => "temporary files",
        _ => "build output",
    }
}

/// A walker with the index's rules: `.gitignore` (inside a git repository or
/// not), `.ignore`, the ignore files of parent folders, never `.git`.
fn walker(dir: &Path, max_depth: Option<usize>, hidden: bool) -> ignore::Walk {
    let mut walk = ignore::WalkBuilder::new(dir);
    walk.require_git(false)
        .hidden(hidden)
        .max_depth(max_depth)
        .filter_entry(|e| e.file_name() != ".git");
    walk.build()
}

/// The folders one level below `root` that are not ignored, and for each the
/// folders one level below it. Hidden folders only when their name is in
/// [`BULKY_NAMES`]. Sorted by name. Each folder's listing looks at no more than
/// `cap` entries, so a folder of millions of files is not listed to the end.
fn folders(root: &Path, cap: u64) -> (Vec<String>, BTreeMap<String, Vec<String>>) {
    let top = subfolders(root, cap);
    let children = top
        .iter()
        .map(|name| (name.clone(), subfolders(&root.join(name), cap)))
        .filter(|(_, list)| !list.is_empty())
        .collect();
    (top, children)
}

/// The folders directly in `dir` that the walker does not ignore, sorted.
fn subfolders(dir: &Path, cap: u64) -> Vec<String> {
    let mut out = Vec::new();
    for entry in walker(dir, Some(1), false)
        .flatten()
        .take(usize::try_from(cap).unwrap_or(usize::MAX))
    {
        if entry.depth() != 1 || !entry.file_type().is_some_and(|t| t.is_dir()) {
            continue;
        }
        // A name that is not UTF-8, or that holds a line break, cannot be
        // written as a `.gitignore` line: such a folder is not reported.
        let Some(name) = entry.file_name().to_str().filter(|n| !n.contains(['\n', '\r'])) else {
            continue;
        };
        if name.starts_with('.') && !is_bulky_name(name) {
            continue;
        }
        out.push(name.to_string());
    }
    out.sort();
    out
}

/// Count the files under `root/name`, adding each to the folder and to the
/// folder one level below it that holds it. Stops when `seen` reaches `cap`.
fn count_folder(root: &Path, name: &str, cap: u64, seen: &mut u64, counts: &mut BTreeMap<String, Count>) {
    // The entry says the folder was counted, even when it holds no file.
    counts.entry(name.to_string()).or_default();
    for entry in walker(&root.join(name), None, true).flatten() {
        // Symbolic links are not followed: no loops, nothing counted twice.
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if *seen >= cap {
            counts.entry(name.to_string()).or_default().cut = true;
            if let Some(child) = child_of(&rel) {
                counts.entry(child).or_default().cut = true;
            }
            return;
        }
        *seen += 1;
        let bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
        let source = u64::from(indexed(&rel, bytes));
        for key in std::iter::once(name.to_string()).chain(child_of(&rel)) {
            let c = counts.entry(key).or_default();
            c.files += 1;
            c.bytes += bytes;
            c.source += source;
        }
    }
}

/// `a/b` for a file at `a/b/...`; `None` for a file directly in `a`.
fn child_of(rel: &str) -> Option<String> {
    let mut parts = rel.splitn(3, '/');
    let (Some(first), Some(second), Some(_)) = (parts.next(), parts.next(), parts.next()) else {
        return None;
    };
    Some(format!("{first}/{second}"))
}

/// True when [`crate::scan`] would index the file at `rel`: no hidden or
/// skipped folder on the way, a known language, not generated, not a
/// dependency of another language.
fn indexed(rel: &str, bytes: u64) -> bool {
    let mut parts: Vec<&str> = rel.split('/').collect();
    let Some(file) = parts.pop() else { return false };
    if file.starts_with('.') || parts.iter().any(|d| d.starts_with('.') || SKIP_DIRS.contains(d)) {
        return false;
    }
    lang_of(rel).is_some_and(|lang| !is_generated(rel) && !is_dependency_or_generated(rel, lang, bytes))
}

fn thousands(n: u64) -> String {
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

/// The verdict on one folder; `None` when it looks like ordinary content.
fn judge(path: &str, name: &str, count: Option<Count>, limits: &BulkyLimits) -> Option<BulkyDir> {
    let counted = count.is_some();
    let c = count.unwrap_or_default();
    let mut reason = if is_bulky_name(name) && (c.files > 0 || !counted) {
        let mut r = format!("named `{name}`: usually {}", kind_of(name));
        if c.source > 0 {
            r.push_str(&format!(
                "; it also holds {} file(s) lge indexes, check them before ignoring it",
                thousands(c.source)
            ));
        }
        r
    } else if counted && !c.cut && c.source == 0 && (c.bytes > limits.bytes || c.files > limits.files) {
        // Only a folder counted to the end is known to hold no source.
        "large and holds no source the index reads".to_string()
    } else {
        return None;
    };
    if !counted {
        reason.push_str(&format!(
            "; not counted, the walk stopped at {} files",
            thousands(limits.walk_cap)
        ));
    } else if c.cut {
        reason.push_str(&format!(
            "; counting stopped at {} files in all",
            thousands(limits.walk_cap)
        ));
    }
    Some(BulkyDir {
        path: path.to_string(),
        bytes: c.bytes,
        files: c.files,
        reason,
    })
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

    fn paths(found: &[BulkyDir]) -> Vec<&str> {
        let mut p: Vec<&str> = found.iter().map(|d| d.path.as_str()).collect();
        p.sort();
        p
    }

    #[test]
    fn named_folders_one_and_two_levels_down_are_found_unless_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, ".gitignore", "/build/\nweb/coverage/\n");
        write(root, ".ignore", "logs/\n");
        write(root, "src/main.py", "x = 1\n");
        // Found: a named folder at the top, one inside a source folder, `.cache`.
        write(root, "artifacts/release-1.tar", "0123456789");
        write(root, "artifacts/old/release-0.tar", "01234");
        write(root, "web/dist/bundle.js", "var a;");
        write(root, ".cache/pip/x.whl", "x");
        write(root, "Tmp/scratch.txt", "x");
        // Not found: ignored by .gitignore or .ignore, hidden, empty, too deep.
        write(root, "build/out.o", "x");
        write(root, "web/coverage/index.html", "x");
        write(root, "logs/today.log", "x");
        write(root, ".venv/lib/site.py", "x");
        fs::create_dir_all(root.join("out")).unwrap();
        fs::create_dir_all(root.join("web/tmp")).unwrap();
        write(root, "a/b/node_modules/dep/index.js", "x");

        let found = bulky_dirs(root);
        assert_eq!(paths(&found), [".cache", "Tmp", "artifacts", "web/dist"], "{found:#?}");
        let artifacts = found.iter().find(|d| d.path == "artifacts").unwrap();
        // The nested `old/` folder is counted in, not reported apart.
        assert_eq!((artifacts.files, artifacts.bytes), (2, 15));
        assert_eq!(artifacts.ignore_line(), "/artifacts/");
        assert!(artifacts.reason.contains("archives"), "{}", artifacts.reason);
        let dist = found.iter().find(|d| d.path == "web/dist").unwrap();
        assert_eq!(dist.ignore_line(), "/web/dist/");
        assert!(dist.reason.contains("build output"), "{}", dist.reason);
        // Largest first.
        assert_eq!(found[0].path, "artifacts");
    }

    #[test]
    fn a_named_folder_with_source_in_it_says_so() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "tmp/try.py", "def f():\n    pass\n");
        let found = bulky_dirs(dir.path());
        assert_eq!(found.len(), 1);
        assert!(found[0].reason.contains("1 file(s) lge indexes"), "{}", found[0].reason);
    }

    #[test]
    fn large_folders_without_indexed_source_are_found() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let limits = BulkyLimits {
            bytes: 1_000,
            files: 5,
            walk_cap: 10_000,
        };
        for i in 0..6 {
            write(root, &format!("data/run{i}.csv"), "1,2,3\n");
            write(root, &format!("app/fixtures/case{i}.bin"), "x");
            // Many files, but source among them: not reported.
            write(root, &format!("lib/mod{i}.py"), "x = 1\n");
        }
        write(root, "app/main.py", "x = 1\n");
        write(root, "docs/big.pdf", &"x".repeat(2_000));
        // Few small files and no source: ordinary content.
        write(root, "assets/logo.png", "x");

        let found = bulky_dirs_with(root, &limits);
        assert_eq!(paths(&found), ["app/fixtures", "data", "docs"], "{found:#?}");
        let data = found.iter().find(|d| d.path == "data").unwrap();
        assert_eq!(data.files, 6);
        assert!(data.reason.contains("no source"), "{}", data.reason);
    }

    #[test]
    fn the_default_size_limit_is_fifty_megabytes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("dumps")).unwrap();
        // A sparse file: its length is what is counted, nothing is written.
        let file = fs::File::create(root.join("dumps/db.sql.gz")).unwrap();
        file.set_len(51 * 1024 * 1024).unwrap();
        fs::create_dir_all(root.join("small")).unwrap();
        fs::File::create(root.join("small/db.sql.gz"))
            .unwrap()
            .set_len(10 * 1024 * 1024)
            .unwrap();
        let found = bulky_dirs(root);
        assert_eq!(paths(&found), ["dumps"]);
        assert_eq!(found[0].bytes, 51 * 1024 * 1024);
    }

    #[test]
    fn the_walk_stops_at_its_cap_and_named_folders_come_first() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for i in 0..20 {
            write(root, &format!("aaa/data{i}.bin"), "x");
            write(root, &format!("node_modules/dep{i}/index.js"), "x");
        }
        write(root, "vendor/lib.js", "x");
        let limits = BulkyLimits {
            bytes: u64::MAX,
            files: 1_000,
            walk_cap: 10,
        };
        let found = bulky_dirs_with(root, &limits);
        assert_eq!(paths(&found), ["node_modules", "vendor"], "{found:#?}");
        let modules = found.iter().find(|d| d.path == "node_modules").unwrap();
        assert_eq!(modules.files, 10);
        assert!(modules.reason.contains("counting stopped"), "{}", modules.reason);
        let vendor = found.iter().find(|d| d.path == "vendor").unwrap();
        assert!(vendor.reason.contains("not counted"), "{}", vendor.reason);
    }

    #[test]
    fn a_missing_root_has_no_bulky_folders() {
        assert!(bulky_dirs(Path::new("/definitely/not/a/real/root")).is_empty());
    }

    // ---- adversarial ----------------------------------------------------------

    #[test]
    fn ignore_files_of_the_root_and_of_subfolders_apply_inside_a_counted_folder() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let limits = BulkyLimits {
            bytes: 1_000,
            files: 5,
            walk_cap: 10_000,
        };
        // Anchored at the root, with a nested `.ignore`: neither hidden part counts in `web`.
        write(root, ".gitignore", "/web/blobs/\n");
        write(root, "web/.ignore", "cache/\n");
        write(root, "web/index.html", "<p>x</p>");
        for i in 0..20 {
            write(root, &format!("web/blobs/b{i}.bin"), &"x".repeat(100));
            write(root, &format!("web/cache/c{i}.bin"), &"x".repeat(100));
        }
        write(root, "web/img/logo.png", "x");
        let found = bulky_dirs_with(root, &limits);
        assert!(found.is_empty(), "{found:#?}");
    }

    #[test]
    fn the_ignore_line_hides_a_folder_whose_name_has_glob_characters() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let limits = BulkyLimits {
            bytes: 10,
            files: 1_000,
            walk_cap: 10_000,
        };
        for name in ["data[old]", "a*b", "q?", "back\\slash"] {
            write(root, &format!("{name}/dump.bin"), &"x".repeat(100));
        }
        // Look-alikes the unescaped globs would match.
        write(root, "datao/keep.bin", &"x".repeat(100));
        write(root, "axxb/keep.bin", &"x".repeat(100));
        write(root, "qz/keep.bin", &"x".repeat(100));
        let found = bulky_dirs_with(root, &limits);
        assert_eq!(found.len(), 7, "{found:#?}");
        let lines: Vec<String> = found
            .iter()
            .filter(|d| !["datao", "axxb", "qz"].contains(&d.path.as_str()))
            .map(BulkyDir::ignore_line)
            .collect();
        fs::write(root.join(".gitignore"), lines.join("\n") + "\n").unwrap();
        assert_eq!(paths(&bulky_dirs_with(root, &limits)), ["axxb", "datao", "qz"]);
    }

    #[test]
    fn a_walk_cut_short_never_claims_a_folder_holds_no_source() {
        let limits = BulkyLimits {
            bytes: 1_000,
            files: 5,
            walk_cap: 10,
        };
        // Large and no source among the files counted, but the walk stopped
        // inside it: the source may be in the part not reached.
        let cut = Count {
            files: 10,
            bytes: 5_000,
            source: 0,
            cut: true,
        };
        assert_eq!(judge("app", "app", Some(cut), &limits), None);
        let whole = Count { cut: false, ..cut };
        assert!(judge("app", "app", Some(whole), &limits).is_some());
        // A name that says what the folder is still counts.
        let named = judge("dist", "dist", Some(cut), &limits).unwrap();
        assert!(named.reason.contains("counting stopped"), "{}", named.reason);
    }

    #[cfg(unix)]
    #[test]
    fn symbolic_links_are_not_followed_and_unreadable_folders_count_as_empty() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = dir.path();
        for i in 0..50 {
            write(outside.path(), &format!("big/f{i}.bin"), &"x".repeat(100));
        }
        let limits = BulkyLimits {
            bytes: 1_000,
            files: 10,
            walk_cap: 10_000,
        };
        // A link at the top, one inside a folder, a loop, a dangling link.
        symlink(outside.path().join("big"), root.join("linked")).unwrap();
        write(root, "src/main.py", "x = 1\n");
        symlink(outside.path().join("big"), root.join("src/linked")).unwrap();
        symlink(root, root.join("src/loop")).unwrap();
        symlink(root.join("missing"), root.join("dangling")).unwrap();
        // A named folder that cannot be read.
        write(root, "logs/secret.log", "x");
        fs::set_permissions(root.join("logs"), fs::Permissions::from_mode(0o000)).unwrap();
        let found = bulky_dirs_with(root, &limits);
        fs::set_permissions(root.join("logs"), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(found.iter().all(|d| !d.path.contains("linked")), "{found:#?}");
        assert!(found.iter().all(|d| d.bytes < 1_000), "{found:#?}");
    }

    #[test]
    fn thousands_are_grouped() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(200_000), "200,000");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn the_ignore_file_is_the_one_rg_reads_there() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("several repos");
        fs::create_dir_all(root.join("api")).unwrap();
        // Not a git repository: rg ignores .gitignore here.
        assert_eq!(ignore_file(&root), ".ignore");
        // A repository, or a folder inside one (a worktree has a .git file).
        fs::create_dir_all(root.join("api/.git")).unwrap();
        assert_eq!(ignore_file(&root.join("api")), ".gitignore");
        fs::create_dir_all(root.join("api/web")).unwrap();
        assert_eq!(ignore_file(&root.join("api/web")), ".gitignore");
        fs::create_dir_all(root.join("wt")).unwrap();
        fs::write(root.join("wt/.git"), "gitdir: /elsewhere\n").unwrap();
        assert_eq!(ignore_file(&root.join("wt")), ".gitignore");
    }
}
