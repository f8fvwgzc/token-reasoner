//! A lock file read whole, shown as its package list: one `name@version` per
//! line, sorted. Hashes, sources and dependency lists are left out, with the
//! command that finds any entry in the file.

use crate::shorten::{shell_word, worth_it};
use serde_json::Value;
use std::collections::BTreeSet;

/// `Cargo.lock`: the name and version of each `[[package]]` block.
fn cargo(text: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    // (name, version) of the block being read, `None` outside a block.
    let mut block: Option<(Option<String>, Option<String>)> = None;
    let finish = |block: &mut Option<(Option<String>, Option<String>)>, out: &mut Vec<String>| -> Option<()> {
        if let Some((name, version)) = block.take() {
            out.push(format!("{}@{}", name?, version?));
        }
        Some(())
    };
    for line in text.lines() {
        let line = line.trim_end();
        if line == "[[package]]" {
            finish(&mut block, &mut out)?;
            block = Some((None, None));
            continue;
        }
        if line.starts_with('[') && !line.starts_with("[ ") {
            // Another table (`[metadata]`) ends the block.
            finish(&mut block, &mut out)?;
            continue;
        }
        let Some((name, version)) = block.as_mut() else {
            continue;
        };
        let quoted = |key: &str| {
            line.strip_prefix(key)
                .and_then(|r| r.strip_prefix(" = \""))
                .and_then(|r| r.strip_suffix('"'))
                .map(str::to_string)
        };
        if let Some(v) = quoted("name") {
            *name = Some(v);
        } else if let Some(v) = quoted("version") {
            *version = Some(v);
        }
    }
    finish(&mut block, &mut out)?;
    (!out.is_empty()).then_some(out)
}

/// `package-lock.json` (and `npm-shrinkwrap.json`): version 2 and 3 list
/// `packages` by their `node_modules/<name>` path; version 1 nests
/// `dependencies`.
fn npm(text: &str) -> Option<Vec<String>> {
    let doc: Value = serde_json::from_str(text).ok()?;
    let lockfile_version = doc.get("lockfileVersion")?.as_u64()?;
    let mut out = Vec::new();
    if lockfile_version >= 2 {
        for (key, entry) in doc.get("packages")?.as_object()? {
            let Some(at) = key.rfind("node_modules/") else {
                // The root package and workspace folders.
                continue;
            };
            let name = &key[at + "node_modules/".len()..];
            // A link to a workspace folder has no version of its own.
            if let Some(version) = entry.get("version").and_then(Value::as_str) {
                out.push(format!("{name}@{version}"));
            }
        }
    } else {
        fn walk(deps: &serde_json::Map<String, Value>, out: &mut Vec<String>) {
            for (name, entry) in deps {
                if let Some(version) = entry.get("version").and_then(Value::as_str) {
                    out.push(format!("{name}@{version}"));
                }
                if let Some(nested) = entry.get("dependencies").and_then(Value::as_object) {
                    walk(nested, out);
                }
            }
        }
        walk(doc.get("dependencies")?.as_object()?, &mut out);
    }
    (!out.is_empty()).then_some(out)
}

/// `name@version` with the name possibly scoped (`@scope/name@1.0.0`).
fn split_at_version(key: &str) -> Option<(&str, &str)> {
    let at = key.char_indices().skip(1).find(|(_, c)| *c == '@')?.0;
    let (name, version) = (&key[..at], &key[at + 1..]);
    (!name.is_empty() && !version.is_empty()).then_some((name, version))
}

/// `pnpm-lock.yaml`: the keys of the `packages:` section, indented two spaces,
/// as `/name@version:`, `name@version:` or quoted; version 5 files write
/// `/name/version:`. A `(...)` peer suffix (`_...` in version 5) is dropped.
fn pnpm(text: &str) -> Option<Vec<String>> {
    let version_line = text.lines().find_map(|l| l.strip_prefix("lockfileVersion:"))?;
    let major: u32 = version_line
        .trim()
        .trim_matches(['\'', '"'])
        .split('.')
        .next()?
        .parse()
        .ok()?;
    let mut out = Vec::new();
    let mut in_packages = false;
    for line in text.lines() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        if !line.starts_with(' ') {
            in_packages = line.trim_end() == "packages:";
            continue;
        }
        if !in_packages || line.starts_with("   ") {
            continue;
        }
        let key = line[2..].trim_end().strip_suffix(':')?;
        let key = key
            .strip_prefix('\'')
            .and_then(|k| k.strip_suffix('\''))
            .or_else(|| key.strip_prefix('"').and_then(|k| k.strip_suffix('"')))
            .unwrap_or(key);
        let key = key.strip_prefix('/').unwrap_or(key);
        let entry = if major >= 6 {
            let key = key.split('(').next()?;
            let (name, version) = split_at_version(key)?;
            format!("{name}@{version}")
        } else {
            let key = key.split('_').next()?;
            let cut = key.rfind('/')?;
            let (name, version) = (&key[..cut], &key[cut + 1..]);
            if name.is_empty() || version.is_empty() {
                return None;
            }
            format!("{name}@{version}")
        };
        out.push(entry);
    }
    (!out.is_empty()).then_some(out)
}

/// `yarn.lock` version 1: the name from an entry's first selector, the version
/// from its `version "x"` line. A Berry lock file (`__metadata:`) is not read.
fn yarn(text: &str) -> Option<Vec<String>> {
    if !text.lines().take(5).any(|l| l.trim() == "# yarn lockfile v1") || text.contains("\n__metadata:") {
        return None;
    }
    let mut out = Vec::new();
    let mut pending: Option<String> = None;
    for line in text.lines() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        if !line.starts_with(' ') {
            if pending.is_some() {
                return None;
            }
            let selectors = line.trim_end().strip_suffix(':')?;
            let first = selectors.split(", ").next()?.trim_matches('"');
            pending = Some(split_at_version(first)?.0.to_string());
            continue;
        }
        if let Some(version) = line.trim().strip_prefix("version \"").and_then(|v| v.strip_suffix('"')) {
            let name = pending.take()?;
            out.push(format!("{name}@{version}"));
        }
    }
    if pending.is_some() {
        return None;
    }
    (!out.is_empty()).then_some(out)
}

/// A lock file read whole, as its sorted, de-duplicated package list, when
/// its basename is `Cargo.lock`, `package-lock.json`, `npm-shrinkwrap.json`,
/// `pnpm-lock.yaml` or `yarn.lock` and its content reads as that format.
/// `None` when it does not, or when the list is not worth sending.
pub fn lockfile_summary(path: &str, text: &str) -> Option<String> {
    let base = path.rsplit('/').next().unwrap_or(path);
    let (packages, needle) = match base {
        "Cargo.lock" => (cargo(text)?, "name = \"NAME\""),
        "package-lock.json" | "npm-shrinkwrap.json" => (npm(text)?, "\"node_modules/NAME\""),
        "pnpm-lock.yaml" => (pnpm(text)?, "NAME@"),
        "yarn.lock" => (yarn(text)?, "NAME@"),
        _ => return None,
    };
    let sorted: BTreeSet<String> = packages.into_iter().collect();
    let mut out = format!(
        "lge: {base} shown as its package list ({} packages, name@version, sorted); hashes, sources and dependency lists are left out. Find an entry with: rg -n -A4 '{needle}' {}\n",
        sorted.len(),
        shell_word(path)
    );
    for package in &sorted {
        out.push_str(package);
        out.push('\n');
    }
    worth_it(text, &out).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        let path = format!("{}/testdata/lockfile/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    fn listed(out: &str) -> Vec<&str> {
        out.lines().skip(1).collect()
    }

    #[test]
    fn a_cargo_lock_lists_every_package() {
        let text = fixture("Cargo.lock");
        let out = lockfile_summary("Cargo.lock", &text).expect("worth it");
        assert!(
            out.starts_with("lge: Cargo.lock shown as its package list (60 packages, name@version, sorted); hashes, sources and dependency lists are left out. Find an entry with: rg -n -A4 'name = \"NAME\"' Cargo.lock\n"),
            "{out}"
        );
        assert!(listed(&out).contains(&"hashbrown@0.17.1"), "{out}");
        let names = listed(&out);
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
    }

    #[test]
    fn npm_pnpm_and_yarn_lock_files_list_the_same_packages() {
        let npm = lockfile_summary("web/package-lock.json", &fixture("package-lock.json")).expect("npm");
        let pnpm = lockfile_summary("pnpm-lock.yaml", &fixture("pnpm-lock.yaml")).expect("pnpm");
        let yarn = lockfile_summary("yarn.lock", &fixture("yarn.lock")).expect("yarn");
        assert!(
            npm.starts_with("lge: package-lock.json shown as its package list (49 packages"),
            "{npm}"
        );
        assert!(
            npm.contains("rg -n -A4 '\"node_modules/NAME\"' web/package-lock.json\n"),
            "{npm}"
        );
        assert!(pnpm.contains("rg -n -A4 'NAME@' pnpm-lock.yaml\n"), "{pnpm}");
        for out in [&npm, &pnpm, &yarn] {
            assert!(listed(out).contains(&"request@2.88.2"), "{out}");
            assert!(listed(out).contains(&"left-pad@1.3.0"), "{out}");
        }
        // The same install, the same list (yarn resolves one range to a newer
        // version than npm and pnpm do).
        assert_eq!(listed(&npm), listed(&pnpm));
        let mut yarn_only: Vec<&str> = listed(&yarn)
            .into_iter()
            .filter(|p| !listed(&npm).contains(p))
            .collect();
        yarn_only.sort_unstable();
        assert_eq!(yarn_only, ["extsprintf@1.4.1"]);
    }

    #[test]
    fn older_formats_and_scoped_names_are_read() {
        let v1 = r#"{"name": "x", "lockfileVersion": 1, "dependencies": {"@babel/core": {"version": "7.1.0", "dependencies": {"debug": {"version": "4.1.0"}}}, "debug": {"version": "2.6.9"}}}"#;
        assert_eq!(npm(v1).unwrap(), ["@babel/core@7.1.0", "debug@4.1.0", "debug@2.6.9"]);
        let pnpm5 = "lockfileVersion: 5.4\n\npackages:\n\n  /@babel/core/7.1.0:\n    resolution: {integrity: x}\n\n  /react-dom/17.0.2_react@17.0.2:\n    dev: false\n";
        assert_eq!(pnpm(pnpm5).unwrap(), ["@babel/core@7.1.0", "react-dom@17.0.2"]);
        let pnpm6 = "lockfileVersion: '6.0'\n\npackages:\n\n  /@types/node@20.1.0:\n    dev: true\n\n  '/styled@5.0.0(react@18.0.0)':\n    dev: false\n";
        assert_eq!(pnpm(pnpm6).unwrap(), ["@types/node@20.1.0", "styled@5.0.0"]);
        let yarn1 = "# yarn lockfile v1\n\n\"@babel/core@^7.0.0\", \"@babel/core@^7.1.0\":\n  version \"7.1.2\"\n\nstring-width-cjs@npm:string-width@^4.2.0:\n  version \"4.2.3\"\n";
        assert_eq!(yarn(yarn1).unwrap(), ["@babel/core@7.1.2", "string-width-cjs@4.2.3"]);
    }

    #[test]
    fn unknown_or_broken_content_is_left_alone() {
        let text = fixture("package-lock.json");
        assert_eq!(lockfile_summary("notes.txt", &text), None);
        assert_eq!(lockfile_summary("Cargo.lock", &text), None, "JSON is not a Cargo.lock");
        assert_eq!(lockfile_summary("package-lock.json", "{\"name\": \"x\"}"), None);
        assert_eq!(lockfile_summary("package-lock.json", &text[..text.len() / 2]), None);
        assert_eq!(lockfile_summary("yarn.lock", "__metadata:\n  version: 6\n"), None);
        assert_eq!(
            lockfile_summary("pnpm-lock.yaml", "packages:\n  /a@1.0.0:\n"),
            None,
            "no lockfileVersion"
        );
        assert_eq!(
            lockfile_summary("pnpm-lock.yaml", "lockfileVersion: '9.0'\npackages:\n  odd line\n"),
            None
        );
        assert_eq!(cargo("[[package]]\nname = \"a\"\n"), None, "a block without a version");
        // A small lock file is not worth a summary.
        assert_eq!(
            lockfile_summary("Cargo.lock", "[[package]]\nname = \"a\"\nversion = \"1.0.0\"\n"),
            None
        );
    }
}
