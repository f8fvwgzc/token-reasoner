//! `lge.toml`: the checks a repository wants run, when auto-detection is not enough.
//!
//! ```toml
//! [[check]]
//! name = "api tests"
//! kind = "tests"            # lint | types | tests
//! command = "pytest -q"
//! cwd = "services/api"      # optional, relative to the repository root
//! timeout_secs = 600        # optional
//! ```
//!
//! Other crates keep their own tables in the same file, so anything that is not a
//! `[[check]]` is ignored here.

use crate::{CheckKind, CheckSpec, CONFIG_FILE};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
struct ConfigFile {
    check: Option<Vec<ConfigCheck>>,
}

/// A misspelled key is an error, not a silently ignored setting.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigCheck {
    name: String,
    kind: CheckKind,
    command: String,
    cwd: Option<PathBuf>,
    timeout_secs: Option<u64>,
}

/// Reads the `[[check]]` tables of `lge.toml` at `root`.
///
/// `Ok(None)` means there is nothing configured (no file, or a file without checks) and
/// the caller should fall back to [`crate::detect_checks`]. A file that is there but
/// wrong is an error: running the wrong checks silently would be worse.
pub fn load_config(root: &Path) -> Result<Option<Vec<CheckSpec>>> {
    let path = root.join(CONFIG_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let file: ConfigFile = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    let Some(checks) = file.check else {
        return Ok(None);
    };

    let mut specs: Vec<CheckSpec> = Vec::with_capacity(checks.len());
    for check in checks {
        let name = check.name.trim().to_string();
        if name.is_empty() {
            bail!("{}: a check has an empty name", path.display());
        }
        if check.command.trim().is_empty() {
            bail!("{}: check `{name}` has an empty command", path.display());
        }
        if check.timeout_secs == Some(0) {
            bail!("{}: check `{name}` has timeout_secs = 0", path.display());
        }
        if specs.iter().any(|s| s.name == name) {
            bail!("{}: two checks are named `{name}`", path.display());
        }
        specs.push(CheckSpec {
            name,
            kind: check.kind,
            command: check.command,
            cwd: check.cwd.map_or_else(|| root.to_path_buf(), |dir| root.join(dir)),
            timeout_secs: check.timeout_secs.unwrap_or(check.kind.default_timeout_secs()),
        });
    }
    Ok(Some(specs))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, text: &str) {
        std::fs::write(dir.join(CONFIG_FILE), text).unwrap();
    }

    #[test]
    fn no_file_means_no_configuration() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_config(dir.path()).unwrap(), None);
    }

    #[test]
    fn a_file_without_checks_means_no_configuration() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "[prices]\ninput = 1.25\n\n[router]\nversion = 1\n");
        assert_eq!(load_config(dir.path()).unwrap(), None);
    }

    #[test]
    fn reads_checks_with_defaults() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            r#"
[prices]
input = 1.25

[[check]]
name = "lint"
kind = "lint"
command = "ruff check ."

[[check]]
name = "api tests"
kind = "tests"
command = "pytest -q"
cwd = "services/api"
timeout_secs = 42

[[check]]
name = "types"
kind = "typecheck"
command = "npx tsc --noEmit"
cwd = "web"
"#,
        );
        let specs = load_config(dir.path()).unwrap().unwrap();
        assert_eq!(
            specs,
            [
                CheckSpec {
                    name: "lint".into(),
                    kind: CheckKind::Lint,
                    command: "ruff check .".into(),
                    cwd: dir.path().to_path_buf(),
                    timeout_secs: 300,
                },
                CheckSpec {
                    name: "api tests".into(),
                    kind: CheckKind::Tests,
                    command: "pytest -q".into(),
                    cwd: dir.path().join("services/api"),
                    timeout_secs: 42,
                },
                CheckSpec {
                    name: "types".into(),
                    kind: CheckKind::Types,
                    command: "npx tsc --noEmit".into(),
                    cwd: dir.path().join("web"),
                    timeout_secs: 300,
                },
            ]
        );
    }

    #[test]
    fn an_empty_list_is_a_choice_to_run_nothing() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "check = []\n");
        assert_eq!(load_config(dir.path()).unwrap(), Some(Vec::new()));
    }

    #[test]
    fn mistakes_are_errors_that_name_the_file() {
        let dir = tempfile::tempdir().unwrap();
        for (text, expected) in [
            ("[[check]]\nname = \"a\"\nkind = \"lint\"\n", "command"),
            ("[[check]]\nname = \"a\"\nkind = \"speed\"\ncommand = \"x\"\n", "speed"),
            ("[[check]]\nname = \"a\"\nkind = \"lint\"\ncommand = \"x\"\ntimeout = 3\n", "timeout"),
            ("[[check]]\nname = \" \"\nkind = \"lint\"\ncommand = \"x\"\n", "empty name"),
            ("[[check]]\nname = \"a\"\nkind = \"lint\"\ncommand = \"\"\n", "empty command"),
            ("[[check]]\nname = \"a\"\nkind = \"lint\"\ncommand = \"x\"\ntimeout_secs = 0\n", "timeout_secs = 0"),
            (
                "[[check]]\nname = \"a\"\nkind = \"lint\"\ncommand = \"x\"\n[[check]]\nname = \"a\"\nkind = \"tests\"\ncommand = \"y\"\n",
                "two checks are named `a`",
            ),
            ("[[check]\nname = 1\n", "lge.toml"),
        ] {
            write(dir.path(), text);
            let error = format!("{:#}", load_config(dir.path()).unwrap_err());
            assert!(error.contains("lge.toml"), "{error}");
            assert!(error.contains(expected), "expected `{expected}` in: {error}");
        }
    }
}
