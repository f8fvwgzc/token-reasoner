//! Runs checks and turns each into a [`CheckResult`].

use crate::diff::relevant_diff_hunks;
use crate::parse::{filter_paths, FilterOptions};
use crate::render::rendered_len;
use crate::{CheckResult, CheckSpec, Report, MAX_DIFF_BYTES, MAX_FILTERED_BYTES, MAX_OUTPUT_BYTES};
use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long the pipe may stay open after the check itself has exited before whatever
/// it left running is killed.
const DRAIN_GRACE: Duration = Duration::from_millis(250);

/// Runs one check and filters its output.
///
/// The command runs through `sh -c` in `spec.cwd`, in its own process group, with
/// `CI=1` and `NO_COLOR=1` and no standard input. Standard output and standard error
/// share one pipe, so lines stay in the order they were written. When the timeout
/// passes, the whole group is killed and the result has `timed_out` set.
///
/// A check that fails is an `Ok` result with `passed == false`. `Err` means the check
/// could not be started at all (no such directory, no `sh`): a problem with the setup,
/// not with the code under test.
pub fn run_check(spec: &CheckSpec) -> Result<CheckResult> {
    run_with(spec, &spec.cwd, MAX_OUTPUT_BYTES)
}

/// Runs every check in order and builds the report: the results, with file paths made
/// relative to `root`, and the diff hunks of the files the failures name.
///
/// Checks run one after another: linters and test runners share caches and lock
/// files, and a stable order keeps the report stable.
pub fn run_all(root: &Path, specs: &[CheckSpec]) -> Result<Report> {
    let mut results = Vec::with_capacity(specs.len());
    for spec in specs {
        results.push(run_with(spec, root, MAX_OUTPUT_BYTES)?);
    }
    let targets = diff_targets(root, specs, &results);
    let diff_hunks = if targets.is_empty() {
        String::new()
    } else {
        relevant_diff_hunks(root, &targets, MAX_DIFF_BYTES)?
    };
    let mut report = Report {
        passed: results.iter().all(|r| r.passed),
        raw_bytes: results.iter().map(|r| r.raw_bytes).sum(),
        results,
        diff_hunks,
        filtered_bytes: 0,
    };
    report.filtered_bytes = rendered_len(&report);
    Ok(report)
}

/// [`run_check`] with the directory failure paths are made relative to, and the output cap.
pub(crate) fn run_with(spec: &CheckSpec, base: &Path, max_output: usize) -> Result<CheckResult> {
    let started = Instant::now();
    let captured = capture(spec, max_output)?;
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

    let passed = captured.exit_code == Some(0) && !captured.timed_out;
    let notice = if captured.timed_out {
        format!("timed out after {}s\n", spec.timeout_secs)
    } else {
        String::new()
    };
    let opts = FilterOptions {
        passed: Some(passed),
        max_bytes: MAX_FILTERED_BYTES.saturating_sub(notice.len()),
        keep_warnings: false,
    };
    // Lossy on purpose: a stray byte in a log must not cost the whole report.
    let raw = String::from_utf8_lossy(&captured.bytes);
    let (failures, filtered) = filter_paths(&spec.command, &raw, &opts, &|file| rebase(file, &spec.cwd, base));

    Ok(CheckResult {
        name: spec.name.clone(),
        kind: spec.kind,
        command: spec.command.clone(),
        exit_code: captured.exit_code,
        passed,
        timed_out: captured.timed_out,
        duration_ms,
        raw_bytes: captured.total_bytes,
        raw_lines: captured.total_lines,
        failures,
        filtered: notice + &filtered,
    })
}

// ---------------------------------------------------------------------------
// Process handling
// ---------------------------------------------------------------------------

struct Captured {
    bytes: Vec<u8>,
    total_bytes: usize,
    total_lines: usize,
    exit_code: Option<i32>,
    timed_out: bool,
}

fn capture(spec: &CheckSpec, max_output: usize) -> Result<Captured> {
    let (mut reader, writer) = std::io::pipe().context("creating the output pipe")?;
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(&spec.command)
        .current_dir(&spec.cwd)
        .stdin(Stdio::null())
        .stdout(writer.try_clone().context("sharing the output pipe")?)
        .stderr(writer)
        .env("CI", "1")
        .env("NO_COLOR", "1")
        // These override NO_COLOR in some tools; the output is parsed, not looked at.
        .env_remove("FORCE_COLOR")
        .env_remove("CLICOLOR_FORCE");
    own_process_group(&mut command);
    let mut child = command
        .spawn()
        .with_context(|| format!("starting `{}` in {}", spec.command, spec.cwd.display()))?;
    // The command still holds the write ends of the pipe; the reader would never see
    // the end of the output while they are open.
    drop(command);

    let sink = Arc::new(Mutex::new(Sink::new(max_output)));
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let reader_sink = Arc::clone(&sink);
    let read_all = move || {
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => lock(&reader_sink).push(&buffer[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        let _ = done_tx.send(());
    };
    if let Err(e) = std::thread::Builder::new()
        .name("lge-filter-output".into())
        .spawn(read_all)
    {
        // Nobody would drain the pipe: the check could block on a full one forever.
        kill_group(&mut child);
        let _ = child.wait();
        return Err(e).context("starting the output reader");
    }

    // A timeout too large to add to the clock is no timeout.
    let deadline = Instant::now().checked_add(Duration::from_secs(spec.timeout_secs));
    let mut pause = Duration::from_millis(2);
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait().context("waiting for the check")? {
            break status;
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            timed_out = true;
            kill_group(&mut child);
            break child.wait().context("waiting for the killed check")?;
        }
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(50));
    };
    // The check is gone, but a process it left behind can keep the pipe open. Give the
    // reader a moment, then kill the group so reading ends, then stop waiting anyway.
    if done_rx.recv_timeout(DRAIN_GRACE).is_err() {
        kill_group(&mut child);
        let _ = done_rx.recv_timeout(Duration::from_secs(2));
    }

    let sink = std::mem::replace(&mut *lock(&sink), Sink::new(0));
    let (bytes, total_bytes, total_lines) = sink.finish();
    Ok(Captured {
        bytes,
        total_bytes,
        total_lines,
        exit_code: if timed_out { None } else { status.code() },
        timed_out,
    })
}

/// A poisoned lock only means the reader thread panicked; what it stored is still valid.
fn lock(sink: &Mutex<Sink>) -> std::sync::MutexGuard<'_, Sink> {
    sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(unix)]
fn own_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(not(unix))]
fn own_process_group(_command: &mut Command) {}

/// Kills the check and everything it started. The check leads its own process group
/// (see [`own_process_group`]), so the group id is its pid.
#[cfg(unix)]
fn kill_group(child: &mut Child) {
    if let Ok(pid) = libc::pid_t::try_from(child.id()) {
        if pid > 1 {
            // SAFETY: `killpg` takes two integers and touches no memory of this process.
            unsafe {
                libc::killpg(pid, libc::SIGKILL);
            }
        }
    }
    let _ = child.kill();
}

#[cfg(not(unix))]
fn kill_group(child: &mut Child) {
    let _ = child.kill();
}

/// Collects output up to a cap. Past the cap the first and the last half are kept:
/// the first error is at the top of a log and the verdict at the bottom.
struct Sink {
    cap: usize,
    head: Vec<u8>,
    tail: VecDeque<u8>,
    total: usize,
    newlines: usize,
    last: u8,
}

impl Sink {
    fn new(cap: usize) -> Sink {
        Sink {
            cap,
            head: Vec::new(),
            tail: VecDeque::new(),
            total: 0,
            newlines: 0,
            last: b'\n',
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        let Some(&last) = chunk.last() else {
            return;
        };
        self.total += chunk.len();
        self.newlines += chunk.iter().filter(|&&b| b == b'\n').count();
        self.last = last;

        let head_cap = self.cap / 2;
        let tail_cap = self.cap - head_cap;
        let (first, rest) = chunk.split_at(head_cap.saturating_sub(self.head.len()).min(chunk.len()));
        self.head.extend_from_slice(first);
        // Only the last `tail_cap` bytes of what remains can survive.
        let rest = &rest[rest.len().saturating_sub(tail_cap)..];
        let overflow = (self.tail.len() + rest.len()).saturating_sub(tail_cap);
        self.tail.drain(..overflow);
        self.tail.extend(rest);
    }

    /// The kept bytes, the number of bytes seen and the number of lines seen.
    fn finish(self) -> (Vec<u8>, usize, usize) {
        let dropped = self.total - self.head.len() - self.tail.len();
        let lines = self.newlines + usize::from(self.last != b'\n');
        let mut bytes = self.head;
        if dropped > 0 {
            bytes.extend_from_slice(format!("\n[output truncated: {dropped} bytes dropped]\n").as_bytes());
        }
        bytes.extend(self.tail);
        (bytes, self.total, lines)
    }
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// Makes a path a tool reported relative to `base`. Tools print paths relative to the
/// directory they ran in, or absolute; a report for a whole repository needs them
/// relative to its root. A path outside `base` is returned absolute.
///
/// Some tools print paths relative to their project root instead (cargo in a
/// workspace member, pytest with its configuration further up). So a relative path
/// that does not exist under `cwd` but does under `base` is taken from `base`.
pub(crate) fn rebase(file: &str, cwd: &Path, base: &Path) -> String {
    if file.is_empty() {
        return String::new();
    }
    let path = Path::new(file);
    let absolute = normalize(&if path.is_absolute() {
        path.to_path_buf()
    } else if !cwd.join(path).exists() && base.join(path).exists() {
        base.join(path)
    } else {
        cwd.join(path)
    });
    // Temporary and home directories are often behind a symlink (`/tmp` on macOS), and
    // tools differ in which spelling they print: try both for both sides.
    let bases = [Some(base.to_path_buf()), base.canonicalize().ok()];
    let candidates = [Some(absolute.clone()), absolute.canonicalize().ok()];
    for candidate in candidates.iter().flatten() {
        for base in bases.iter().flatten() {
            if let Ok(relative) = candidate.strip_prefix(base) {
                return slashed(relative);
            }
        }
    }
    slashed(&absolute)
}

/// Removes `.` and resolves `..` without touching the file system.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    out
}

fn slashed(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    if text.is_empty() {
        ".".to_string()
    } else {
        text
    }
}

// ---------------------------------------------------------------------------
// Which files to diff
// ---------------------------------------------------------------------------

crate::parse::pattern!(
    MENTION,
    r#"(?:File "([^"\n]+)", line (\d+))|(?:([A-Za-z0-9_@.\-/\[\]()]+\.[A-Za-z0-9]{1,6})[:(](\d+))"#
);

/// The files to look for in the diff, as `path` or `path:line`: the file of every
/// failure, then every existing file its excerpt mentions (the frames of a traceback
/// are where the change that broke the test most likely is). Unparsed output of a
/// failed check is searched the same way.
fn diff_targets(root: &Path, specs: &[CheckSpec], results: &[CheckResult]) -> Vec<String> {
    let mut targets: Vec<String> = Vec::new();
    let mut add = |target: String| {
        if !targets.contains(&target) {
            targets.push(target);
        }
    };
    let failed = || specs.iter().zip(results).filter(|(_, r)| !r.passed);
    for (_, result) in failed() {
        for failure in &result.failures {
            if failure.file.is_empty() || Path::new(&failure.file).is_absolute() {
                continue;
            }
            add(match failure.line {
                Some(line) => format!("{}:{line}", failure.file),
                None => failure.file.clone(),
            });
        }
    }
    for (spec, result) in failed() {
        let texts: Vec<&str> = if result.failures.is_empty() {
            vec![&result.filtered]
        } else {
            result.failures.iter().map(|f| f.excerpt.as_str()).collect()
        };
        for c in texts.iter().flat_map(|text| MENTION.captures_iter(text)) {
            let (path, line) = match (c.get(1), c.get(2), c.get(3), c.get(4)) {
                (Some(path), Some(line), _, _) | (_, _, Some(path), Some(line)) => (path.as_str(), line.as_str()),
                _ => continue,
            };
            // Excerpts keep the paths as the tool printed them: relative to where it ran.
            // The pattern also takes brackets (`app/[id]/page.tsx`), so `[Makefile:12]`
            // is tried again without the one in front.
            let existing = [path, path.trim_start_matches(['[', '('])]
                .into_iter()
                .map(|p| rebase(p, &spec.cwd, root))
                .find(|p| !Path::new(p).is_absolute() && root.join(p).is_file());
            if let Some(relative) = existing {
                add(format!("{relative}:{line}"));
            }
        }
    }
    targets
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CheckKind;

    fn spec(command: &str, cwd: &Path) -> CheckSpec {
        CheckSpec {
            name: "check".into(),
            kind: CheckKind::Tests,
            command: command.into(),
            cwd: cwd.to_path_buf(),
            timeout_secs: 20,
        }
    }

    #[test]
    fn sink_keeps_everything_under_the_cap() {
        let mut sink = Sink::new(10);
        sink.push(b"abc\n");
        sink.push(b"");
        sink.push(b"def");
        assert_eq!(sink.finish(), (b"abc\ndef".to_vec(), 7, 2));
        assert_eq!(Sink::new(10).finish(), (Vec::new(), 0, 0));
    }

    #[test]
    fn sink_keeps_both_ends_over_the_cap() {
        let mut sink = Sink::new(10);
        for chunk in [&b"0123"[..], b"456789ab", b"c", b"defghijklmnop\n"] {
            sink.push(chunk);
        }
        let (bytes, total, lines) = sink.finish();
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "01234\n[output truncated: 17 bytes dropped]\nmnop\n"
        );
        assert_eq!((total, lines), (27, 1));
    }

    #[test]
    fn rebase_makes_paths_relative_to_the_base() {
        let root = Path::new("/repo");
        let cwd = Path::new("/repo/api");
        assert_eq!(rebase("tests/test_a.py", cwd, root), "api/tests/test_a.py");
        assert_eq!(rebase("./tests/../shop/a.py", cwd, root), "api/shop/a.py");
        assert_eq!(rebase("/repo/api/shop/a.py", cwd, root), "api/shop/a.py");
        assert_eq!(rebase("../web/src/a.ts", cwd, root), "web/src/a.ts");
        assert_eq!(rebase("tests/test_a.py", cwd, cwd), "tests/test_a.py");
        assert_eq!(rebase("/usr/lib/python3/x.py", cwd, root), "/usr/lib/python3/x.py");
        assert_eq!(rebase("../../other/a.py", cwd, root), "/other/a.py");
        assert_eq!(rebase("", cwd, root), "");
        assert_eq!(rebase(".", root, root), ".");
    }

    #[test]
    fn rebase_accepts_paths_already_relative_to_the_base() {
        let dir = tempfile::tempdir().unwrap();
        let service = dir.path().join("crates/shop");
        std::fs::create_dir_all(service.join("src")).unwrap();
        std::fs::write(service.join("src/lib.rs"), "").unwrap();
        // cargo run inside a workspace member prints workspace-relative paths for
        // compiler errors and member-relative ones for panics.
        assert_eq!(
            rebase("crates/shop/src/lib.rs", &service, dir.path()),
            "crates/shop/src/lib.rs"
        );
        assert_eq!(rebase("src/lib.rs", &service, dir.path()), "crates/shop/src/lib.rs");
        // Neither exists: the tool's own directory is the best guess.
        assert_eq!(rebase("src/gone.rs", &service, dir.path()), "crates/shop/src/gone.rs");
    }

    #[test]
    fn rebase_sees_through_a_symlinked_base() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().canonicalize().unwrap();
        std::fs::write(real.join("a.py"), "").unwrap();
        // The tool prints the resolved path, the caller holds the unresolved one, or the reverse.
        assert_eq!(
            rebase(&real.join("a.py").to_string_lossy(), dir.path(), dir.path()),
            "a.py"
        );
        assert_eq!(rebase(&dir.path().join("a.py").to_string_lossy(), &real, &real), "a.py");
    }

    #[test]
    fn diff_targets_take_failure_files_then_existing_files_from_excerpts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("shop")).unwrap();
        std::fs::write(dir.path().join("shop/pricing.py"), "").unwrap();
        std::fs::write(dir.path().join("build.log"), "").unwrap();
        let failure = |file: &str, line, excerpt: &str| crate::Failure {
            file: file.into(),
            line,
            name: "t".into(),
            message: String::new(),
            excerpt: excerpt.into(),
        };
        let result = |passed, failures, filtered: &str| CheckResult {
            name: "c".into(),
            kind: CheckKind::Tests,
            command: "c".into(),
            exit_code: Some(1),
            passed,
            timed_out: false,
            duration_ms: 0,
            raw_bytes: 0,
            raw_lines: 0,
            failures,
            filtered: filtered.into(),
        };
        let results = [
            result(
                false,
                vec![
                    failure(
                        "tests/test_orders.py",
                        Some(35),
                        "tests/test_orders.py:35: \nshop/pricing.py:8: KeyError\n  File \"shop/pricing.py\", line 9, in f\n/usr/lib/python3/x.py:1: in y\nmissing/gone.py:3: in z",
                    ),
                    failure("tests/test_orders.py", Some(35), ""),
                    failure("/abs/outside.py", Some(1), ""),
                    failure("", None, ""),
                    failure("README.md", None, ""),
                ],
                "",
            ),
            result(false, Vec::new(), "make: *** [build.log:12] Error 1\nsee build.log(40)\n"),
            result(true, vec![failure("ignored/passed.py", Some(1), "")], ""),
        ];
        let specs = [spec("c", dir.path()), spec("c", dir.path()), spec("c", dir.path())];
        assert_eq!(
            diff_targets(dir.path(), &specs, &results),
            [
                "tests/test_orders.py:35",
                "README.md",
                "shop/pricing.py:8",
                "shop/pricing.py:9",
                "build.log:12",
                "build.log:40",
            ]
        );
    }

    #[cfg(unix)]
    mod commands {
        use super::*;

        #[test]
        fn a_passing_command_is_one_line() {
            let dir = tempfile::tempdir().unwrap();
            let result = run_check(&spec(
                "echo building; echo 'error: only a log line'; echo done",
                dir.path(),
            ))
            .unwrap();
            assert!(result.passed && !result.timed_out);
            assert_eq!(result.exit_code, Some(0));
            assert_eq!(result.filtered, "ok");
            assert!(result.failures.is_empty());
            assert_eq!((result.raw_bytes, result.raw_lines), (37, 3));
            assert_eq!((result.name.as_str(), result.kind), ("check", CheckKind::Tests));
        }

        #[test]
        fn a_failing_command_keeps_its_output_and_exit_code() {
            let dir = tempfile::tempdir().unwrap();
            let result = run_check(&spec("echo 'boom: it broke'; exit 3", dir.path())).unwrap();
            assert!(!result.passed);
            assert_eq!(result.exit_code, Some(3));
            assert!(result.failures.is_empty());
            assert_eq!(result.filtered, "boom: it broke\n");
        }

        #[test]
        fn stdout_and_stderr_are_interleaved_in_order() {
            let dir = tempfile::tempdir().unwrap();
            let result = run_check(&spec(
                "echo one; echo two >&2; echo three; echo four >&2; exit 1",
                dir.path(),
            ))
            .unwrap();
            assert_eq!(result.filtered, "one\ntwo\nthree\nfour\n");
        }

        #[test]
        fn a_missing_program_is_a_failed_check_not_an_error() {
            let dir = tempfile::tempdir().unwrap();
            let result = run_check(&spec("no-such-tool-lge-filter --version", dir.path())).unwrap();
            assert_eq!(result.exit_code, Some(127));
            assert!(result.filtered.contains("no-such-tool-lge-filter"));
        }

        #[test]
        fn a_missing_directory_is_an_error() {
            let dir = tempfile::tempdir().unwrap();
            let error = run_check(&spec("echo hi", &dir.path().join("nope"))).unwrap_err();
            assert!(format!("{error:#}").contains("starting `echo hi`"));
        }

        #[test]
        fn environment_and_working_directory() {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("marker.txt"), "").unwrap();
            let result = run_check(&spec(
                "echo \"CI=$CI NO_COLOR=$NO_COLOR FORCE=${FORCE_COLOR:-unset}\"; ls; exit 1",
                dir.path(),
            ))
            .unwrap();
            assert_eq!(result.filtered, "CI=1 NO_COLOR=1 FORCE=unset\nmarker.txt\n");
        }

        #[test]
        fn standard_input_is_closed() {
            let dir = tempfile::tempdir().unwrap();
            let result = run_check(&spec("cat; echo after", dir.path())).unwrap();
            assert!(result.passed);
            assert!(result.duration_ms < 10_000);
        }

        #[test]
        fn timeout_kills_the_whole_process_group() {
            let dir = tempfile::tempdir().unwrap();
            let mut spec = spec(
                "echo started; sleep 30 & echo $! > child.pid; sleep 30; echo finished",
                dir.path(),
            );
            spec.timeout_secs = 1;
            let started = Instant::now();
            let result = run_check(&spec).unwrap();
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "took {:?}",
                started.elapsed()
            );
            assert!(result.timed_out && !result.passed);
            assert_eq!(result.exit_code, None);
            assert_eq!(result.filtered, "timed out after 1s\nstarted\n");
            assert!(result.duration_ms >= 1_000);

            // The background `sleep` was started by the check, not by this test: it must be gone too.
            let pid: libc::pid_t = std::fs::read_to_string(dir.path().join("child.pid"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            let gone = || unsafe { libc::kill(pid, 0) } != 0;
            let deadline = Instant::now() + Duration::from_secs(5);
            while !gone() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            assert!(gone(), "process {pid} survived the timeout");
        }

        #[test]
        fn a_timeout_beyond_the_clock_means_no_timeout() {
            let dir = tempfile::tempdir().unwrap();
            let mut spec = spec("echo fine", dir.path());
            spec.timeout_secs = u64::MAX;
            assert!(run_check(&spec).unwrap().passed);
        }

        #[test]
        fn a_background_process_left_behind_does_not_hang_the_check() {
            let dir = tempfile::tempdir().unwrap();
            let started = Instant::now();
            let result = run_check(&spec("sleep 30 & echo done", dir.path())).unwrap();
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "took {:?}",
                started.elapsed()
            );
            assert!(result.passed);
        }

        #[test]
        fn non_utf8_output_does_not_panic() {
            let dir = tempfile::tempdir().unwrap();
            let result = run_check(&spec(
                "printf 'caf\\351 \\377\\376 error: bad bytes\\n'; exit 1",
                dir.path(),
            ))
            .unwrap();
            assert_eq!(result.raw_bytes, 25);
            assert!(result.filtered.contains("error: bad bytes"));
            assert!(result.filtered.contains('\u{fffd}'));
        }

        #[test]
        fn output_over_the_cap_keeps_the_start_and_the_end() {
            let dir = tempfile::tempdir().unwrap();
            let command = "echo 'first line'; i=0; while [ $i -lt 2000 ]; do echo \"filler line $i\"; i=$((i+1)); done; echo 'error: the last line'; exit 1";
            let result = run_with(&spec(command, dir.path()), dir.path(), 4_000).unwrap();
            assert!(result.raw_bytes > 30_000);
            assert_eq!(result.raw_lines, 2_002);
            assert!(result.filtered.contains("[output truncated: "));
            assert!(result.filtered.ends_with("error: the last line\n"));
            assert!(result.filtered.len() <= MAX_FILTERED_BYTES);
        }

        #[test]
        fn parsed_failures_are_relative_to_the_base() {
            let root = tempfile::tempdir().unwrap();
            let service = root.path().join("api");
            std::fs::create_dir_all(&service).unwrap();
            // A fake linter: prints one ruff-style line with a relative and one with an absolute path.
            let command = "echo \"shop/orders.py:1:8: F401 unused\"; echo \"$(pwd -P)/shop/cart.py:2:1: E722 bare except\"; exit 1";
            let mut spec = spec(command, &service);
            spec.name = "ruff".into();
            spec.command = format!("ruff() {{ {command}; }}; ruff");

            let inside = run_check(&spec).unwrap();
            let files: Vec<&str> = inside.failures.iter().map(|f| f.file.as_str()).collect();
            assert_eq!(files, ["shop/cart.py", "shop/orders.py"]);

            let from_root = run_with(&spec, root.path(), MAX_OUTPUT_BYTES).unwrap();
            let files: Vec<&str> = from_root.failures.iter().map(|f| f.file.as_str()).collect();
            assert_eq!(files, ["api/shop/cart.py", "api/shop/orders.py"]);
            assert_eq!(
                from_root.filtered,
                "api/shop/cart.py\n  2: E722: bare except\napi/shop/orders.py\n  1: F401: unused\n"
            );
        }
    }
}
