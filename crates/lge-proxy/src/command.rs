//! What a shell command was for, read from its text. The proxy only shortens
//! the output of commands it understands; everything else passes untouched.

/// The purpose of a command, as far as the proxy can tell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Purpose {
    /// The whole of one file: `cat path`.
    ReadWhole { path: String },
    /// A line range of one file: `sed -n '10,40p' path`, `head -n 50 path`.
    /// `from_top` is true when the range starts at line 1: reading a file
    /// from the top is looking around, not fetching known lines.
    ReadRange {
        path: String,
        start: usize,
        end: usize,
        from_top: bool,
    },
    /// `rg`, `grep`, `git grep`, `ag`.
    Search,
    /// A lint, type-check, build or test run; the tool as `lge_filter` names it.
    Check { tool: String },
    /// Anything else, including every command the proxy cannot read with certainty.
    Other,
}

/// Characters that make a command more than one simple command. With any of
/// them present the output is not simply the output of the first word.
const SHELL_OPERATORS: [&str; 9] = ["|", "&&", "||", ";", ">", "<", "$(", "`", "\n"];

/// Split a simple command into words, honouring single and double quotes.
/// `None` when a quote is left open.
fn words(command: &str) -> Option<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for c in command.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                started = true;
            }
            None if c.is_whitespace() => {
                if started || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            None => cur.push(c),
        }
    }
    if quote.is_some() {
        return None;
    }
    if started || !cur.is_empty() {
        out.push(cur);
    }
    Some(out)
}

/// `10,40p` -> (10, 40); `5p` -> (5, 5).
fn sed_range(script: &str) -> Option<(usize, usize)> {
    let body = script.strip_suffix('p')?;
    match body.split_once(',') {
        Some((a, b)) => Some((a.parse().ok()?, b.parse().ok()?)),
        None => body.parse().ok().map(|n| (n, n)),
    }
}

/// Read the purpose of a command line. A leading `cd dir &&` is allowed and
/// ignored; any other shell operator makes the command `Other`.
pub fn purpose(command: &str) -> Purpose {
    let mut command = command.trim();
    if let Some(rest) = command.strip_prefix("cd ") {
        match rest.split_once("&&") {
            Some((dir, tail)) if !SHELL_OPERATORS.iter().any(|op| dir.contains(op)) => command = tail.trim(),
            _ => return Purpose::Other,
        }
    }
    // A search pattern may contain `|`; look at the first word before judging operators.
    let first = command.split_whitespace().next().unwrap_or("");
    let program = first.rsplit('/').next().unwrap_or(first);
    if matches!(program, "rg" | "grep" | "ag" | "egrep" | "fgrep") || command.starts_with("git grep ") {
        return Purpose::Search;
    }
    let tool = lge_filter::tool_of(command);
    if matches!(
        tool.as_str(),
        "pytest"
            | "py.test"
            | "cargo"
            | "jest"
            | "vitest"
            | "tsc"
            | "eslint"
            | "pyright"
            | "basedpyright"
            | "mypy"
            | "ruff"
            | "flake8"
    ) {
        return Purpose::Check { tool };
    }
    if SHELL_OPERATORS.iter().any(|op| command.contains(op)) {
        return Purpose::Other;
    }
    let Some(w) = words(command) else { return Purpose::Other };
    let args: Vec<&str> = w.iter().skip(1).map(String::as_str).collect();
    match (program, args.as_slice()) {
        ("cat", [path]) if !path.starts_with('-') => Purpose::ReadWhole { path: path.to_string() },
        ("sed", ["-n", script, path]) => match sed_range(script) {
            Some((start, end)) if start >= 1 && end >= start => Purpose::ReadRange {
                path: path.to_string(),
                start,
                end,
                from_top: start == 1,
            },
            _ => Purpose::Other,
        },
        ("head", ["-n", n, path]) | ("head", [path, "-n", n]) => match n.parse::<usize>() {
            Ok(n) if n >= 1 => Purpose::ReadRange {
                path: path.to_string(),
                start: 1,
                end: n,
                from_top: true,
            },
            _ => Purpose::Other,
        },
        _ => Purpose::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_reads_are_recognised_only_in_their_plain_forms() {
        assert_eq!(
            purpose("cat src/app.py"),
            Purpose::ReadWhole {
                path: "src/app.py".into()
            }
        );
        assert_eq!(
            purpose("cd svc && cat 'my file.py'"),
            Purpose::ReadWhole {
                path: "my file.py".into()
            }
        );
        assert_eq!(
            purpose("sed -n '10,40p' a/b.ts"),
            Purpose::ReadRange {
                path: "a/b.ts".into(),
                start: 10,
                end: 40,
                from_top: false
            }
        );
        assert_eq!(
            purpose("sed -n 1,200p a.py"),
            Purpose::ReadRange {
                path: "a.py".into(),
                start: 1,
                end: 200,
                from_top: true
            }
        );
        assert_eq!(
            purpose("head -n 80 a.py"),
            Purpose::ReadRange {
                path: "a.py".into(),
                start: 1,
                end: 80,
                from_top: true
            }
        );
        for other in [
            "cat a.py b.py",
            "cat -n a.py",
            "cat a.py | head",
            "cat a.py > out",
            "sed -n '/def/p' a.py",
            "sed -i 's/a/b/' a.py",
            "nl -ba a.py",
            "cat 'unterminated",
            "cd a; cat b",
            "python -c 'print(1)'",
            "",
        ] {
            assert_eq!(purpose(other), Purpose::Other, "{other}");
        }
    }

    #[test]
    fn searches_and_checks_are_recognised_with_pipes_and_flags() {
        for search in [
            "rg -n \"create|update\" -S src",
            "grep -rn foo .",
            "git grep -n handler",
            "/opt/homebrew/bin/rg foo | head -50",
        ] {
            assert_eq!(purpose(search), Purpose::Search, "{search}");
        }
        assert_eq!(
            purpose("pytest -q tests/test_a.py"),
            Purpose::Check { tool: "pytest".into() }
        );
        assert_eq!(
            purpose("cd web && npx tsc --noEmit"),
            Purpose::Check { tool: "tsc".into() }
        );
        assert_eq!(
            purpose("cargo test -p lge-core 2>&1 | tail -20"),
            Purpose::Check { tool: "cargo".into() }
        );
        assert_eq!(
            purpose("npm run build"),
            Purpose::Other,
            "an unknown runner is left alone"
        );
        assert_eq!(purpose("ls -la"), Purpose::Other);
    }
}
