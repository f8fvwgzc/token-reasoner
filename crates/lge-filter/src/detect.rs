//! Finds the checks of a repository when it has no `lge.toml`.
//!
//! A check is suggested only when its command can actually start: the executable is on
//! `PATH` or in the project's virtual environment, or it is a script the project's
//! `package.json` defines. A missing linter is not a failed lint run.

use crate::{CheckKind, CheckSpec};
use std::fs;
use std::path::Path;

/// Directories that are never a project of their own.
const SKIPPED_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "dist",
    "build",
    "venv",
    "env",
    "vendor",
    "__pycache__",
];

/// Which languages already have a check at the repository root.
#[derive(Default, Clone, Copy)]
struct Covered {
    python: bool,
    node: bool,
    rust: bool,
}

/// The lint, type and test commands that fit `root`: Python (ruff or flake8, mypy,
/// pyright, pytest), Node (the `lint`, `typecheck` and `test` scripts, or `tsc`) and
/// Rust (clippy and `cargo test`).
///
/// The root is looked at first, then each directory directly under it, so a monorepo
/// of services gets one set of checks per service, named `service/check` and run inside
/// that service. A language the root already checks is not checked again per directory:
/// the root's command covers the tree.
pub fn detect_checks(root: &Path) -> Vec<CheckSpec> {
    detect_with(root, &on_path)
}

/// [`detect_checks`] with the `PATH` lookup passed in, so tests do not depend on what
/// is installed.
pub(crate) fn detect_with(root: &Path, has: &dyn Fn(&str) -> bool) -> Vec<CheckSpec> {
    let (mut specs, covered) = project_checks(root, root, Covered::default(), has);
    let mut dirs: Vec<String> = fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| !name.starts_with('.') && !SKIPPED_DIRS.contains(&name.as_str()))
        .collect();
    dirs.sort();
    for name in dirs {
        let (found, _) = project_checks(root, &root.join(&name), covered, has);
        specs.extend(found.into_iter().map(|mut spec| {
            spec.name = format!("{name}/{}", spec.name);
            spec
        }));
    }
    specs
}

/// The checks of one project directory, and which languages they cover.
fn project_checks(root: &Path, dir: &Path, skip: Covered, has: &dyn Fn(&str) -> bool) -> (Vec<CheckSpec>, Covered) {
    let mut specs = Vec::new();
    let mut add = |name: &str, kind: CheckKind, command: String| {
        specs.push(CheckSpec {
            name: name.to_string(),
            kind,
            command,
            cwd: dir.to_path_buf(),
            timeout_secs: kind.default_timeout_secs(),
        });
    };
    let mut covered = Covered::default();

    if !skip.python && is_python(dir) {
        covered.python = python_checks(root, dir, has, &mut add);
    }
    if !skip.node && dir.join("package.json").is_file() {
        covered.node = node_checks(root, dir, has, &mut add);
    }
    if !skip.rust && dir.join("Cargo.toml").is_file() && has("cargo") {
        if has("cargo-clippy") {
            add(
                "clippy",
                CheckKind::Lint,
                "cargo clippy --all-targets -- -D warnings".to_string(),
            );
        }
        add("cargo-test", CheckKind::Tests, "cargo test".to_string());
        covered.rust = true;
    }
    (specs, covered)
}

// ---------------------------------------------------------------------------
// Python
// ---------------------------------------------------------------------------

fn is_python(dir: &Path) -> bool {
    ["pyproject.toml", "setup.cfg", "setup.py", "pytest.ini"]
        .iter()
        .any(|f| dir.join(f).is_file())
        || file_names(dir)
            .iter()
            .any(|f| f.starts_with("requirements") && f.ends_with(".txt"))
        || has_python_tests(dir)
}

/// True when the directory has pytest-style test files, directly or in a `tests` or
/// `test` directory (two levels down at most). A `tests` directory alone is not a
/// sign of Python: Rust and Node projects have one too.
fn has_python_tests(dir: &Path) -> bool {
    fn is_test_file(name: &str) -> bool {
        name.ends_with(".py") && (name.starts_with("test_") || name.ends_with("_test.py") || name == "conftest.py")
    }
    fn any_test(dir: &Path, depth: usize) -> bool {
        file_names(dir).iter().any(|f| is_test_file(f))
            || (depth > 0 && dir_names(dir).iter().any(|d| any_test(&dir.join(d), depth - 1)))
    }
    file_names(dir).iter().any(|f| is_test_file(f)) || ["tests", "test"].iter().any(|d| any_test(&dir.join(d), 1))
}

/// Adds the Python checks; returns whether any was added.
fn python_checks(
    root: &Path,
    dir: &Path,
    has: &dyn Fn(&str) -> bool,
    add: &mut dyn FnMut(&str, CheckKind, String),
) -> bool {
    let read = |name: &str| fs::read_to_string(dir.join(name)).unwrap_or_default();
    let pyproject: toml::Table = toml::from_str(&read("pyproject.toml")).unwrap_or_default();
    let tool = |name: &str| pyproject.get("tool").and_then(|t| t.get(name));
    let setup_cfg = read("setup.cfg");
    let tox_ini = read("tox.ini");
    let exists = |name: &str| dir.join(name).is_file();
    let mut added = false;

    // Lint: ruff whenever it can run; flake8 only where the project configures it.
    let flake8_configured =
        exists(".flake8") || ini_section(&setup_cfg, "flake8").is_some() || ini_section(&tox_ini, "flake8").is_some();
    if let Some(ruff) = find_tool(root, dir, "ruff", has) {
        add("ruff", CheckKind::Lint, format!("{ruff} check ."));
        added = true;
    } else if let Some(flake8) = find_tool(root, dir, "flake8", has).filter(|_| flake8_configured) {
        add("flake8", CheckKind::Lint, format!("{flake8} ."));
        added = true;
    }

    // Types: only the checkers the project configures. An unconfigured mypy run on a
    // code base that never used it reports hundreds of errors that are not regressions.
    let mypy_ini = [read("mypy.ini"), read(".mypy.ini")].concat();
    let mypy_section = ini_section(&mypy_ini, "mypy").or(ini_section(&setup_cfg, "mypy"));
    if exists("mypy.ini") || exists(".mypy.ini") || mypy_section.is_some() || tool("mypy").is_some() {
        if let Some(mypy) = find_tool(root, dir, "mypy", has) {
            // With `files`, `packages` or `modules` configured mypy knows what to check;
            // without, it needs a target.
            let targets = ["files", "packages", "modules"];
            let has_targets = targets.iter().any(|key| {
                tool("mypy").is_some_and(|t| t.get(key).is_some())
                    || mypy_section.as_deref().is_some_and(|s| ini_has_key(s, key))
            });
            add(
                "mypy",
                CheckKind::Types,
                if has_targets { mypy } else { format!("{mypy} .") },
            );
            added = true;
        }
    }
    if exists("pyrightconfig.json") || tool("pyright").is_some() {
        if let Some(pyright) = find_tool(root, dir, "pyright", has) {
            add("pyright", CheckKind::Types, pyright);
            added = true;
        }
    }

    // Tests: pytest, when there is something for it to collect. On a project without
    // tests it exits with status 5, which would read as a failure.
    let pytest_configured = exists("pytest.ini")
        || tool("pytest").is_some()
        || ini_section(&setup_cfg, "tool:pytest").is_some()
        || ini_section(&tox_ini, "pytest").is_some();
    if pytest_configured || has_python_tests(dir) {
        if let Some(pytest) = find_tool(root, dir, "pytest", has) {
            add("pytest", CheckKind::Tests, pytest);
            added = true;
        }
    }
    added
}

/// How to start `name` from `dir`: the project's virtual environment first (that is
/// where its pinned version lives), then the repository's, then `PATH`.
fn find_tool(root: &Path, dir: &Path, name: &str, has: &dyn Fn(&str) -> bool) -> Option<String> {
    for venv in [".venv", "venv"] {
        if dir.join(venv).join("bin").join(name).is_file() {
            return Some(format!("{venv}/bin/{name}"));
        }
    }
    if dir != root {
        for venv in [".venv", "venv"] {
            if root.join(venv).join("bin").join(name).is_file() {
                return Some(format!("../{venv}/bin/{name}"));
            }
        }
    }
    has(name).then(|| name.to_string())
}

/// The body of `[name]` in an ini file, if the section exists.
fn ini_section(text: &str, name: &str) -> Option<String> {
    let header = format!("[{name}]");
    let mut lines = text.lines().skip_while(|l| l.trim() != header);
    lines.next()?;
    Some(
        lines
            .take_while(|l| !l.trim_start().starts_with('['))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

fn ini_has_key(section: &str, key: &str) -> bool {
    section.lines().any(|l| {
        l.split_once(['=', ':'])
            .is_some_and(|(k, _)| !l.starts_with(char::is_whitespace) && k.trim() == key)
    })
}

// ---------------------------------------------------------------------------
// Node
// ---------------------------------------------------------------------------

/// Adds the Node checks; returns whether any was added.
fn node_checks(
    root: &Path,
    dir: &Path,
    has: &dyn Fn(&str) -> bool,
    add: &mut dyn FnMut(&str, CheckKind, String),
) -> bool {
    let package: serde_json::Value = fs::read_to_string(dir.join("package.json"))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    let script = |name: &str| {
        package
            .get("scripts")
            .and_then(|s| s.get(name))
            .and_then(|s| s.as_str())
            .filter(|s| !s.trim().is_empty())
    };
    let manager = package_manager(root, dir, &package, has);
    let mut added = false;

    if let (Some(pm), Some(_)) = (manager, script("lint")) {
        add(&format!("{pm}:lint"), CheckKind::Lint, format!("{pm} run lint"));
        added = true;
    }
    let typecheck = ["typecheck", "type-check"]
        .into_iter()
        .find(|name| script(name).is_some());
    if let (Some(pm), Some(name)) = (manager, typecheck) {
        add(&format!("{pm}:{name}"), CheckKind::Types, format!("{pm} run {name}"));
        added = true;
    } else if dir.join("tsconfig.json").is_file() {
        // No script: run the compiler directly, but only one that is already installed.
        // `npx tsc` without a local TypeScript would download an unrelated package.
        if dir.join("node_modules/.bin/tsc").is_file() && has("npx") {
            add("tsc", CheckKind::Types, "npx tsc --noEmit".to_string());
            added = true;
        } else if has("tsc") {
            add("tsc", CheckKind::Types, "tsc --noEmit".to_string());
            added = true;
        }
    }
    // `npm init` writes a `test` script that only fails; that is not a test suite.
    let test = script("test").filter(|s| !s.contains("no test specified"));
    if let (Some(pm), Some(_)) = (manager, test) {
        add(&format!("{pm}:test"), CheckKind::Tests, format!("{pm} run test"));
        added = true;
    }
    added
}

/// The package manager the project uses: the `packageManager` field, else the lockfile
/// (in the project or, for a workspace member, at the root), else npm. `None` when
/// neither it nor npm is installed.
fn package_manager(
    root: &Path,
    dir: &Path,
    package: &serde_json::Value,
    has: &dyn Fn(&str) -> bool,
) -> Option<&'static str> {
    const LOCKFILES: &[(&str, &str)] = &[
        ("pnpm-lock.yaml", "pnpm"),
        ("yarn.lock", "yarn"),
        ("bun.lockb", "bun"),
        ("bun.lock", "bun"),
        ("package-lock.json", "npm"),
        ("npm-shrinkwrap.json", "npm"),
    ];
    let declared = package.get("packageManager").and_then(|v| v.as_str()).and_then(|v| {
        ["pnpm", "yarn", "bun", "npm"]
            .into_iter()
            .find(|pm| v.starts_with(&format!("{pm}@")))
    });
    let locked = [dir, root].iter().find_map(|d| {
        LOCKFILES
            .iter()
            .find(|(file, _)| d.join(file).is_file())
            .map(|(_, pm)| *pm)
    });
    let wanted = declared.or(locked).unwrap_or("npm");
    if has(wanted) {
        Some(wanted)
    } else {
        has("npm").then_some("npm")
    }
}

// ---------------------------------------------------------------------------
// File system helpers
// ---------------------------------------------------------------------------

fn entries(dir: &Path, want_dirs: bool) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().is_dir() == want_dirs)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}

fn file_names(dir: &Path) -> Vec<String> {
    entries(dir, false)
}

fn dir_names(dir: &Path) -> Vec<String> {
    entries(dir, true)
}

/// True when `name` is an executable file in one of the `PATH` directories.
pub(crate) fn on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| is_executable(&dir.join(name)))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn write(root: &Path, rel: &str, text: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// (name, kind, command, cwd relative to the root) of every detected check.
    fn detect(root: &Path, installed: &[&str]) -> Vec<(String, CheckKind, String, PathBuf)> {
        detect_with(root, &|name| installed.contains(&name))
            .into_iter()
            .map(|s| {
                let cwd = s.cwd.strip_prefix(root).unwrap().to_path_buf();
                (s.name, s.kind, s.command, cwd)
            })
            .collect()
    }

    fn check(name: &str, kind: CheckKind, command: &str, cwd: &str) -> (String, CheckKind, String, PathBuf) {
        (name.to_string(), kind, command.to_string(), PathBuf::from(cwd))
    }

    const ALL: &[&str] = &[
        "ruff",
        "flake8",
        "mypy",
        "pyright",
        "pytest",
        "npm",
        "npx",
        "pnpm",
        "yarn",
        "bun",
        "tsc",
        "cargo",
        "cargo-clippy",
    ];

    #[test]
    fn empty_directory_has_no_checks() {
        let dir = tempfile::tempdir().unwrap();
        assert!(detect(dir.path(), ALL).is_empty());
        assert!(detect_checks(dir.path()).is_empty());
    }

    #[test]
    fn python_project_with_configured_tools() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "pyproject.toml",
            "[project]\nname = \"shop\"\n\n[tool.ruff]\nline-length = 100\n\n[tool.mypy]\nstrict = true\n\n[tool.pyright]\ninclude = [\"shop\"]\n",
        );
        write(dir.path(), "tests/test_cart.py", "def test_ok():\n    assert True\n");
        assert_eq!(
            detect(dir.path(), ALL),
            [
                check("ruff", CheckKind::Lint, "ruff check .", ""),
                check("mypy", CheckKind::Types, "mypy .", ""),
                check("pyright", CheckKind::Types, "pyright", ""),
                check("pytest", CheckKind::Tests, "pytest", ""),
            ]
        );
        // Nothing installed: nothing suggested, however much is configured.
        assert!(detect(dir.path(), &[]).is_empty());
        // Timeouts follow the kind.
        let specs = detect_with(dir.path(), &|name| ALL.contains(&name));
        assert_eq!(specs[0].timeout_secs, 300);
        assert_eq!(specs[3].timeout_secs, 900);
    }

    #[test]
    fn python_type_checkers_need_configuration_and_pytest_needs_tests() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "requirements-dev.txt", "pytest\nmypy\n");
        write(dir.path(), "shop/cart.py", "x = 1\n");
        // ruff runs without configuration; mypy, pyright and pytest have nothing to go on.
        assert_eq!(
            detect(dir.path(), ALL),
            [check("ruff", CheckKind::Lint, "ruff check .", "")]
        );

        write(
            dir.path(),
            "setup.cfg",
            "[flake8]\nmax-line-length = 100\n\n[mypy]\nfiles = shop\n\n[tool:pytest]\ntestpaths = tests\n",
        );
        assert_eq!(
            detect(dir.path(), &["flake8", "mypy", "pytest"]),
            [
                check("flake8", CheckKind::Lint, "flake8 .", ""),
                check("mypy", CheckKind::Types, "mypy", ""),
                check("pytest", CheckKind::Tests, "pytest", ""),
            ]
        );
    }

    #[test]
    fn a_virtual_environment_beats_path() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "pyproject.toml", "[tool.mypy]\nfiles = [\"shop\"]\n");
        write(dir.path(), "test_shop.py", "def test_ok(): pass\n");
        write(dir.path(), ".venv/bin/ruff", "");
        write(dir.path(), ".venv/bin/mypy", "");
        write(dir.path(), "venv/bin/pytest", "");
        assert_eq!(
            detect(dir.path(), &[]),
            [
                check("ruff", CheckKind::Lint, ".venv/bin/ruff check .", ""),
                check("mypy", CheckKind::Types, ".venv/bin/mypy", ""),
                check("pytest", CheckKind::Tests, "venv/bin/pytest", ""),
            ]
        );
    }

    #[test]
    fn a_tests_directory_without_python_is_not_a_python_project() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "tests/integration.rs", "#[test]\nfn ok() {}\n");
        write(dir.path(), "tests/fixtures/services/app.py", "x = 1\n");
        assert!(detect(dir.path(), ALL).is_empty());
        write(dir.path(), "tests/unit/test_app.py", "def test_ok(): pass\n");
        assert_eq!(
            detect(dir.path(), ALL),
            [
                check("ruff", CheckKind::Lint, "ruff check .", ""),
                check("pytest", CheckKind::Tests, "pytest", ""),
            ]
        );
    }

    #[test]
    fn node_scripts_through_the_lockfile_package_manager() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "package.json",
            r#"{"scripts": {"lint": "eslint .", "type-check": "tsc --noEmit", "test": "vitest run", "build": "vite build"}}"#,
        );
        write(dir.path(), "tsconfig.json", "{}");
        write(dir.path(), "pnpm-lock.yaml", "");
        assert_eq!(
            detect(dir.path(), ALL),
            [
                check("pnpm:lint", CheckKind::Lint, "pnpm run lint", ""),
                check("pnpm:type-check", CheckKind::Types, "pnpm run type-check", ""),
                check("pnpm:test", CheckKind::Tests, "pnpm run test", ""),
            ]
        );
        // pnpm is not installed: npm can still run the scripts.
        assert_eq!(
            detect(dir.path(), &["npm"])[0],
            check("npm:lint", CheckKind::Lint, "npm run lint", "")
        );
        // No package manager at all: only the compiler is left, if it is installed.
        assert_eq!(
            detect(dir.path(), &["tsc"]),
            [check("tsc", CheckKind::Types, "tsc --noEmit", "")]
        );
        assert!(detect(dir.path(), &[]).is_empty());
    }

    #[test]
    fn node_without_scripts_falls_back_to_tsc() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "package.json",
            r#"{"packageManager": "yarn@4.1.0", "scripts": {"test": "echo \"Error: no test specified\" && exit 1"}}"#,
        );
        // No tsconfig.json: nothing to check.
        assert!(detect(dir.path(), ALL).is_empty());
        write(dir.path(), "tsconfig.json", "{}");
        assert_eq!(
            detect(dir.path(), ALL),
            [check("tsc", CheckKind::Types, "tsc --noEmit", "")]
        );
        // A local TypeScript is preferred, through npx.
        write(dir.path(), "node_modules/.bin/tsc", "");
        assert_eq!(
            detect(dir.path(), ALL),
            [check("tsc", CheckKind::Types, "npx tsc --noEmit", "")]
        );
        // npx without a local TypeScript would download something: not suggested.
        fs::remove_file(dir.path().join("node_modules/.bin/tsc")).unwrap();
        assert!(detect(dir.path(), &["npx", "npm"]).is_empty());

        write(
            dir.path(),
            "package.json",
            r#"{"packageManager": "yarn@4.1.0", "scripts": {"test": "jest"}}"#,
        );
        assert_eq!(
            detect(dir.path(), ALL)[1],
            check("yarn:test", CheckKind::Tests, "yarn run test", "")
        );
    }

    #[test]
    fn rust_project() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Cargo.toml", "[package]\nname = \"x\"\n");
        assert_eq!(
            detect(dir.path(), ALL),
            [
                check(
                    "clippy",
                    CheckKind::Lint,
                    "cargo clippy --all-targets -- -D warnings",
                    ""
                ),
                check("cargo-test", CheckKind::Tests, "cargo test", ""),
            ]
        );
        assert_eq!(
            detect(dir.path(), &["cargo"]),
            [check("cargo-test", CheckKind::Tests, "cargo test", "")]
        );
        // The real PATH: whatever runs this test usually found cargo there.
        let real = detect_checks(dir.path());
        assert_eq!(
            real.iter().any(|s| s.command == "cargo test" && s.cwd == dir.path()),
            on_path("cargo")
        );
    }

    #[test]
    fn monorepo_gets_one_set_of_checks_per_service_directory() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "orders_api/pyproject.toml", "[tool.mypy]\n");
        write(dir.path(), "orders_api/tests/test_routes.py", "def test_ok(): pass\n");
        write(
            dir.path(),
            "web_gateway/package.json",
            r#"{"scripts": {"test": "jest", "lint": "eslint src"}}"#,
        );
        write(dir.path(), "web_gateway/yarn.lock", "");
        write(
            dir.path(),
            "notifier/package.json",
            r#"{"scripts": {"typecheck": "tsc --noEmit"}}"#,
        );
        write(dir.path(), "package-lock.json", "{}");
        write(dir.path(), ".venv/bin/pytest", "");
        // Not projects: hidden, vendored, or two levels down.
        write(dir.path(), ".cache/package.json", r#"{"scripts": {"test": "jest"}}"#);
        write(
            dir.path(),
            "node_modules/left-pad/package.json",
            r#"{"scripts": {"test": "tape"}}"#,
        );
        write(
            dir.path(),
            "node_modules/package.json",
            r#"{"scripts": {"test": "tape"}}"#,
        );
        write(dir.path(), "docs/examples/Cargo.toml", "[package]\n");
        assert_eq!(
            detect(dir.path(), &["ruff", "mypy", "npm", "yarn", "cargo"]),
            [
                check(
                    "notifier/npm:typecheck",
                    CheckKind::Types,
                    "npm run typecheck",
                    "notifier"
                ),
                check("orders_api/ruff", CheckKind::Lint, "ruff check .", "orders_api"),
                check("orders_api/mypy", CheckKind::Types, "mypy .", "orders_api"),
                check(
                    "orders_api/pytest",
                    CheckKind::Tests,
                    "../.venv/bin/pytest",
                    "orders_api"
                ),
                check("web_gateway/yarn:lint", CheckKind::Lint, "yarn run lint", "web_gateway"),
                check(
                    "web_gateway/yarn:test",
                    CheckKind::Tests,
                    "yarn run test",
                    "web_gateway"
                ),
            ]
        );
    }

    #[test]
    fn a_language_checked_at_the_root_is_not_checked_again_per_directory() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Cargo.toml", "[workspace]\nmembers = [\"core\"]\n");
        write(dir.path(), "core/Cargo.toml", "[package]\nname = \"core\"\n");
        write(dir.path(), "web/package.json", r#"{"scripts": {"test": "vitest run"}}"#);
        // A root package.json that defines no check does not hide the one in `web`.
        write(
            dir.path(),
            "package.json",
            r#"{"private": true, "devDependencies": {"prettier": "3"}}"#,
        );
        assert_eq!(
            detect(dir.path(), &["cargo", "npm"]),
            [
                check("cargo-test", CheckKind::Tests, "cargo test", ""),
                check("web/npm:test", CheckKind::Tests, "npm run test", "web"),
            ]
        );
    }

    #[test]
    fn ini_sections() {
        let text = "[metadata]\nname = shop\n\n[mypy]\nfiles = shop\n  tests\nstrict = True\n[mypy-yaml.*]\nignore_missing_imports = True\n";
        let section = ini_section(text, "mypy").unwrap();
        assert!(ini_has_key(&section, "files") && ini_has_key(&section, "strict"));
        assert!(!ini_has_key(&section, "ignore_missing_imports") && !ini_has_key(&section, "tests"));
        assert!(ini_section(text, "flake8").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn on_path_wants_an_executable_file() {
        assert!(on_path("sh"));
        assert!(!on_path("no-such-tool-lge-filter"));
        assert!(!on_path(""));
    }
}
