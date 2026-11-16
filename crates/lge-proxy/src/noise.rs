//! Progress lines of installers, transfers and builds that ended with exit
//! code 0: lines that say work is going on (`Downloading ...`, `Receiving
//! objects: 45%`, a progress bar), not what came of it.
//!
//! Each entry of the table names the commands it is for and the lines it may
//! drop, with lge's own patterns. A line that mentions an error, a warning, a
//! failure, a deprecation, a vulnerability, a security matter, a denial, a
//! conflict, a fatal condition or a URL is always kept, and so is a message of
//! a git server. There is never a cap on lines: only lines a pattern names go.

use crate::command::Normalised;
use crate::shorten::worth_it;
use regex::Regex;
use std::sync::LazyLock;

/// Lines that are kept whatever a drop pattern says.
static KEEP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(error|warn|fail|deprecat|vulnerab|security|denied|conflict|fatal)|https?://")
        .expect("the pattern is a valid regex")
});

/// A line a git server sent (`remote: ...`). Kept unless it is one of git's
/// own progress counters, which [`GIT_REMOTE_PROGRESS`] names.
static REMOTE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^remote: ").expect("the pattern is a valid regex"));

/// git's progress counters as a server relays them.
static GIT_REMOTE_PROGRESS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^remote: (?:(?:Enumerating|Counting|Compressing|Writing|Receiving|Resolving) (?:objects|deltas):|Total \d+ \(delta)",
    )
    .expect("the pattern is a valid regex")
});

/// A line that is only a progress bar: bar characters, digits and units, with
/// at least one `%`, `#` or `=`.
static PROGRESS_BAR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\s#=>.\-|/\\\[\]0-9%:kMGB]*$").expect("the pattern is a valid regex"));

/// The drop patterns of each entry.
static DROPS: LazyLock<Vec<(&'static str, Vec<Regex>)>> = LazyLock::new(|| {
    let table: [(&str, &[&str]); 10] = [
        (
            "install",
            &[
                r"^\s*(?:(?:Downloading|Downloaded|Fetching|Fetched|Resolving|Resolved|Collecting|Using cached|Preparing|Building wheel|Unpacking|Already downloaded|Installing collected packages|==> (?:Downloading|Pouring|Fetching))\b|Progress:)",
            ],
        ),
        (
            "git-transfer",
            &[
                r"^(?:remote: )?(?:Enumerating|Counting|Compressing|Writing|Receiving|Resolving) (?:objects|deltas):",
                r"^(?:remote: )?Total \d+ \(delta",
                r"^Delta compression using",
            ],
        ),
        ("git-status", &[r#"^\s*\(use "git "#, r"^\s*\(create/copy files"]),
        (
            "curl",
            &[
                r"^\s*% Total\s+% Received",
                r"^\s+Dload\s+Upload",
                r"^\s*\d{1,3}\s+\S+\s+\d{1,3}\s+\S+\s+\d{1,3}\s+\S+\s+\S+\s+\S+\s+(?:--:--:--|\d+:\d\d:\d\d)",
            ],
        ),
        (
            "wget",
            &[
                r"^\s*\d+K[ .]+\d+%",
                r"^(?:Resolving|Connecting to|HTTP request sent|Length:|Saving to:) ",
            ],
        ),
        (
            "docker",
            &[
                r"^[0-9a-f]{12}: (?:Pulling fs layer|Waiting|Downloading|Verifying Checksum|Download complete|Extracting|Pull complete)",
                r"^#\d+ (?:sha256:|\d+\.\d+ |DONE |CACHED$|\[internal\])",
            ],
        ),
        (
            "make",
            &[
                r"^make(?:\[\d+\])?: (?:Entering|Leaving) directory",
                r"^make(?:\[\d+\])?: Nothing to be done",
            ],
        ),
        (
            "gradle",
            &[
                r"^> Task \S+(?: UP-TO-DATE| NO-SOURCE| FROM-CACHE| SKIPPED)?$",
                r"^Starting a Gradle Daemon",
                r"^<[=-]+> \d+% (?:EXECUTING|CONFIGURING|INITIALIZING)",
            ],
        ),
        (
            "terraform",
            &[r": Refreshing state\.\.\.", r"^(?:Acquiring|Releasing) state lock"],
        ),
        (
            "pre-commit",
            &[r"^\[INFO\] (?:Initializing environment|Installing environment|Once installed this environment)"],
        ),
    ];
    table
        .iter()
        .map(|(name, patterns)| {
            let compiled = patterns
                .iter()
                .map(|p| Regex::new(p).expect("the pattern is a valid regex"))
                .collect();
            (*name, compiled)
        })
        .collect()
});

/// The word after the subcommand: `install` in `uv pip install`.
fn third(n: &Normalised) -> &str {
    n.args
        .iter()
        .filter(|a| !a.starts_with('-'))
        .nth(1)
        .map_or("", String::as_str)
}

/// `git status` in its short or machine form.
fn short_status(n: &Normalised) -> bool {
    n.args.iter().any(|a| {
        a == "--short"
            || a.starts_with("--porcelain")
            || (a.starts_with('-') && !a.starts_with("--") && a.contains('s'))
    })
}

/// The entry of the table for a command, by its program and subcommand.
pub fn entry(n: &Normalised) -> Option<&'static str> {
    let sub = n.sub.as_deref().unwrap_or("");
    Some(match n.program.as_str() {
        "npm" | "pnpm" | "yarn" | "bun" if matches!(sub, "install" | "i" | "ci" | "add") => "install",
        "pip" | "pip3" if sub == "install" => "install",
        "uv" if (sub == "pip" && third(n) == "install") || matches!(sub, "sync" | "add") => "install",
        "poetry" if matches!(sub, "install" | "lock" | "update" | "add") => "install",
        "bundle" if matches!(sub, "install" | "update") => "install",
        "composer" if matches!(sub, "install" | "update" | "require") => "install",
        "brew" if matches!(sub, "install" | "upgrade") => "install",
        "cargo" if matches!(sub, "fetch" | "update" | "install") => "install",
        "git" if matches!(sub, "push" | "pull" | "fetch" | "clone") => "git-transfer",
        "git" if sub == "status" && !short_status(n) => "git-status",
        "curl" => "curl",
        "wget" => "wget",
        "docker"
            if matches!(sub, "build" | "pull")
                || (sub == "compose" && matches!(third(n), "build" | "pull"))
                || (sub == "buildx" && third(n) == "build") =>
        {
            "docker"
        }
        "make" => "make",
        "gradle" | "gradlew" => "gradle",
        "terraform" | "tofu" if sub == "plan" => "terraform",
        "pre-commit" => "pre-commit",
        _ => return None,
    })
}

/// True when the line must stay, whatever a drop pattern says.
fn must_keep(line: &str) -> bool {
    KEEP.is_match(line) || (REMOTE.is_match(line) && !GIT_REMOTE_PROGRESS.is_match(line))
}

/// The lines of `text` that `name`'s patterns keep, and how many were dropped.
fn drop_lines<'a>(name: &str, text: &'a str) -> (Vec<&'a str>, usize) {
    let drops = DROPS
        .iter()
        .find(|(entry, _)| *entry == name)
        .map_or(&[][..], |(_, d)| d.as_slice());
    let mut kept = Vec::new();
    let mut dropped = 0;
    let mut previous_gone_or_blank = false;
    for line in text.lines() {
        let blank = line.trim().is_empty();
        let gone = !must_keep(line)
            && (drops.iter().any(|r| r.is_match(line))
                || (name == "install" && PROGRESS_BAR.is_match(line) && line.contains(['%', '#', '=']))
                || (name == "git-status" && blank && previous_gone_or_blank));
        if gone {
            dropped += 1;
        } else {
            kept.push(line);
        }
        previous_gone_or_blank = gone || blank;
    }
    (kept, dropped)
}

/// The output of a command that exited with code 0, without the progress
/// lines its entry names, and the entry's name. Call it only when the exit
/// code is known to be 0. `None` for a command with no entry, one whose exit
/// status may not be its own (`|| true`, a pipe), when nothing was dropped, or
/// when the saving is too small.
pub fn apply(n: &Normalised, text: &str) -> Option<(&'static str, String)> {
    if !n.single || n.exit_masked {
        return None;
    }
    let name = entry(n)?;
    let (kept, dropped) = drop_lines(name, text);
    if dropped == 0 {
        return None;
    }
    // Programs without subcommands are named alone: `wget`, not `wget out.html`.
    let label = match (&n.sub, name) {
        (Some(sub), "install" | "git-transfer" | "git-status" | "docker" | "terraform" | "pre-commit") => {
            format!("{} {sub}", n.program)
        }
        _ => n.program.clone(),
    };
    let mut out = kept.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(&format!(
        "lge: {dropped} progress lines of `{label}` left out (exit code 0).\n"
    ));
    worth_it(text, &out).then_some((name, out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::normalise;

    fn fixture(name: &str) -> String {
        let path = format!("{}/testdata/noise/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    fn entry_of(command: &str) -> Option<&'static str> {
        entry(&normalise(command).unwrap())
    }

    /// The kept text of `name`'s patterns, after the terminal clean-up the
    /// proxy runs first.
    fn kept(name: &str, text: &str) -> (String, usize) {
        let clean = lge_filter::normalize_terminal(text);
        let (kept, dropped) = drop_lines(name, &clean);
        (kept.join("\n"), dropped)
    }

    #[test]
    fn commands_find_their_entry() {
        for (command, name) in [
            ("npm install", "install"),
            ("npm ci", "install"),
            ("pnpm add react", "install"),
            ("yarn add left-pad", "install"),
            ("pip install -r requirements.txt", "install"),
            ("uv pip install requests", "install"),
            ("uv sync", "install"),
            ("poetry lock", "install"),
            ("brew upgrade", "install"),
            ("cargo fetch", "install"),
            ("git clone https://example.com/r.git", "git-transfer"),
            ("git -C r pull", "git-transfer"),
            ("git status", "git-status"),
            ("curl -o x https://example.com", "curl"),
            ("wget https://example.com", "wget"),
            ("docker pull alpine", "docker"),
            ("docker compose build", "docker"),
            ("docker buildx build .", "docker"),
            ("make -j8", "make"),
            ("./gradlew build", "gradle"),
            ("terraform plan", "terraform"),
            ("tofu plan", "terraform"),
            ("pre-commit run --all-files", "pre-commit"),
        ] {
            assert_eq!(entry_of(command), Some(name), "{command}");
        }
        for command in [
            "npm test",
            "pip list",
            "uv run pytest",
            "git status -s",
            "git status --porcelain",
            "git status -sb",
            "git log",
            "docker run alpine",
            "terraform apply",
        ] {
            assert_eq!(entry_of(command), None, "{command}");
        }
    }

    #[test]
    fn pip_keeps_its_errors_and_its_result() {
        let (out, dropped) = kept("install", &fixture("pip_install.txt"));
        assert_eq!(dropped, 16, "{out}");
        for line in [
            "ERROR: pip's dependency resolver",
            "botocore 1.31.1 requires urllib3<1.27,>=1.25.4, but you have urllib3 2.6.3 which is incompatible.",
            "Successfully installed certifi-2026.10.23",
            "[notice] A new release of pip is available",
        ] {
            assert!(out.contains(line), "{line}: {out}");
        }
        for gone in ["Collecting", "Downloading", "Installing collected packages"] {
            assert!(!out.contains(gone), "{gone}: {out}");
        }
    }

    #[test]
    fn npm_warnings_and_its_tally_stay() {
        let text = fixture("npm_install.txt");
        let (out, dropped) = kept("install", &text);
        assert_eq!(dropped, 0, "{out}");
        assert!(out.contains("npm warn deprecated request@2.88.2"));
        assert!(out.contains("added 49 packages, and audited 50 packages"));
        let n = normalise("npm install").unwrap();
        assert_eq!(apply(&n, &text), None, "nothing to drop");
    }

    #[test]
    fn a_git_clone_keeps_only_what_came_of_it() {
        let text = fixture("git_clone.txt");
        let (out, dropped) = kept("git-transfer", &text);
        assert_eq!(out, "Cloning into 'clone2'...");
        assert_eq!(dropped, 6);
        // Run as is, with every redraw of the counters on one line.
        let n = normalise("git clone --progress --no-local file:///r clone2").unwrap();
        let (name, short) = apply(&n, &text).expect("worth it");
        assert_eq!(name, "git-transfer");
        assert_eq!(
            short,
            "Cloning into 'clone2'...\nlge: 6 progress lines of `git clone` left out (exit code 0).\n"
        );
        // A server's own messages and ref updates stay.
        let push = "Enumerating objects: 5, done.\nCounting objects: 100% (5/5), done.\nWriting objects: 100% (3/3), 300 bytes | 300.00 KiB/s, done.\nTotal 3 (delta 2), reused 0 (delta 0), pack-reused 0\nremote: Resolving deltas: 100% (2/2), completed with 2 local objects.\nremote:\nremote: Create a pull request for 'fix' on GitHub by visiting:\nremote:      https://github.com/o/r/pull/new/fix\nremote:\nTo github.com:o/r.git\n * [new branch]      fix -> fix\n";
        let (out, dropped) = kept("git-transfer", push);
        assert_eq!(dropped, 5, "{out}");
        assert_eq!(
            out,
            "remote:\nremote: Create a pull request for 'fix' on GitHub by visiting:\nremote:      https://github.com/o/r/pull/new/fix\nremote:\nTo github.com:o/r.git\n * [new branch]      fix -> fix"
        );
    }

    #[test]
    fn git_status_loses_its_hints() {
        let (out, dropped) = kept("git-status", &fixture("git_status.txt"));
        assert_eq!(dropped, 3);
        assert_eq!(
            out,
            "On branch main\nYour branch is up to date with 'origin/main'.\n\nChanges not staged for commit:\n\tmodified:   README.md\n\nUntracked files:\n\tnewfile.txt\n\nno changes added to commit (use \"git add\" and/or \"git commit -a\")"
        );
        // A blank line left after a dropped one goes too.
        let (out, dropped) = kept(
            "git-status",
            "On branch main\nUntracked files:\n  (use \"git add <file>...\" to include in what will be committed)\n\n\tx.txt\n",
        );
        assert_eq!(
            (out.as_str(), dropped),
            ("On branch main\nUntracked files:\n\tx.txt", 2)
        );
    }

    #[test]
    fn curl_wget_and_make_lose_their_meters() {
        let (out, dropped) = kept("curl", &fixture("curl.txt"));
        assert_eq!((out.as_str(), dropped), ("", 3));
        let (out, dropped) = kept("wget", &fixture("wget.txt"));
        assert!(
            out.starts_with("--2026-11-16 11:52:44--  https://registry.npmjs.org/"),
            "{out}"
        );
        assert!(out.contains("HTTP request sent, awaiting response... 200 OK") || dropped > 0);
        assert!(out.contains("- ‘big.bin’ saved [5825770/5825770]"), "{out}");
        assert!(!out.contains(".......... .........."), "{out}");
        let (out, dropped) = kept("make", &fixture("make.txt"));
        assert_eq!(dropped, 6);
        assert_eq!(
            out,
            "/Library/Developer/CommandLineTools/usr/bin/make -C sub\nbuilt\n/Library/Developer/CommandLineTools/usr/bin/make -C sub\nbuilt"
        );
        let n = normalise("wget -O big.bin https://registry.npmjs.org/typescript/-/typescript-5.4.5.tgz").unwrap();
        let (_, short) = apply(&n, &fixture("wget.txt")).expect("the dots are most of it");
        assert!(
            short.ends_with("progress lines of `wget` left out (exit code 0).\n"),
            "{short}"
        );
    }

    #[test]
    fn docker_gradle_terraform_and_pre_commit_lose_their_progress() {
        let docker = "Using default tag: latest\nlatest: Pulling from library/alpine\n9824c27679d3: Pulling fs layer\n9824c27679d3: Verifying Checksum\n9824c27679d3: Download complete\n9824c27679d3: Pull complete\nDigest: sha256:abc\nStatus: Downloaded newer image for alpine:latest\n#5 [2/3] RUN npm ci\n#5 0.412 npm warn deprecated inflight@1.0.6\n#5 1.200 added 10 packages\n#5 DONE 1.3s\n#6 CACHED\n";
        let (out, dropped) = kept("docker", docker);
        assert_eq!(dropped, 7, "{out}");
        assert!(
            out.contains("#5 0.412 npm warn deprecated inflight@1.0.6"),
            "warnings beat drop: {out}"
        );
        assert!(out.contains("Status: Downloaded newer image for alpine:latest"));
        let gradle = "Starting a Gradle Daemon (subsequent builds will be faster)\n> Task :app:compileJava UP-TO-DATE\n> Task :app:test\n<=========----> 75% EXECUTING [2s]\nBUILD SUCCESSFUL in 4s\n";
        let (out, dropped) = kept("gradle", gradle);
        assert_eq!((out.as_str(), dropped), ("BUILD SUCCESSFUL in 4s", 4));
        let terraform = "Acquiring state lock. This may take a few moments...\naws_s3_bucket.a: Refreshing state... [id=a]\nNo changes. Your infrastructure matches the configuration.\nReleasing state lock. This may take a few moments...\n";
        let (out, dropped) = kept("terraform", terraform);
        assert_eq!(
            (out.as_str(), dropped),
            ("No changes. Your infrastructure matches the configuration.", 3)
        );
        let pre_commit = "[INFO] Initializing environment for https://github.com/psf/black.\n[INFO] Installing environment for local.\n[INFO] Once installed this environment will be reused.\nblack....................................................................Passed\n";
        let (out, dropped) = kept("pre-commit", pre_commit);
        assert_eq!(dropped, 2, "the line with a URL stays: {out}");
        assert!(out.ends_with("Passed"));
    }

    #[test]
    fn keep_words_beat_every_drop_pattern() {
        for line in [
            "Downloading failed for x",
            "Collecting ERROR",
            "Resolving deltas: denied",
            "Fetching https://example.com/x",
            "100%|#####| conflict",
            "Downloading deprecated-package",
        ] {
            let (out, dropped) = kept("install", line);
            assert_eq!((out.as_str(), dropped), (line, 0), "{line}");
        }
        // A progress bar alone goes; a number alone stays.
        assert_eq!(kept("install", "  45%|####      | 12/30 [00:01<00:02]").1, 0);
        assert_eq!(kept("install", "[=====>     ] 50%").1, 1);
        assert_eq!(kept("install", "12345").1, 0);
    }

    #[test]
    fn a_masked_exit_or_a_chain_is_never_cleaned() {
        let text = fixture("wget.txt");
        for command in [
            "wget https://e.com/x || true",
            "wget -O - https://e.com/x | cat",
            "cd a; wget https://e.com/x; ls",
        ] {
            assert_eq!(apply(&normalise(command).unwrap(), &text), None, "{command}");
        }
    }
}
