//! Line diffs between two outputs: a command run again, or a file read again.
//!
//! [`rerun_delta`] and [`read_delta`] say what a second output would cost if
//! only what changed since the first were sent. In v0.0.2 they are measured,
//! never sent. The diff is lge's own Myers O(ND) implementation; a result is
//! only given when applying it to the first output gives the second exactly.

use crate::shorten::worth_it;

/// One step of a line diff, with 0-based line indexes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Line `a` of the old text is line `b` of the new one.
    Equal { a: usize, b: usize },
    /// Line `a` of the old text is gone.
    Delete { a: usize },
    /// Line `b` of the new text is new.
    Insert { b: usize },
}

/// Edits the diff looks for at most; past that, the differing middle is
/// given as deleted and inserted whole. Memory grows with its square.
const MAX_EDITS: usize = 1000;
/// Unchanged lines shown around each change.
const CONTEXT: usize = 2;

/// The shortest edit script between the middles of two texts, or `None` when
/// it needs more than [`MAX_EDITS`] edits.
fn myers(a: &[&str], b: &[&str]) -> Option<Vec<Op>> {
    let (n, m) = (a.len() as isize, b.len() as isize);
    if n == 0 && m == 0 {
        return Some(Vec::new());
    }
    let max = (n + m).min(MAX_EDITS as isize);
    let offset = max + 1;
    let at = |k: isize| (k + offset) as usize;
    let mut v = vec![0isize; (2 * max + 3) as usize];
    // trace[d]: the furthest x on each diagonal k in -d..=d before step d.
    let mut trace: Vec<Vec<isize>> = Vec::new();
    let mut end = None;
    'search: for d in 0..=max {
        trace.push((-d..=d).map(|k| v[at(k)]).collect());
        let mut k = -d;
        while k <= d {
            let mut x = if k == -d || (k != d && v[at(k - 1)] < v[at(k + 1)]) {
                v[at(k + 1)]
            } else {
                v[at(k - 1)] + 1
            };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[at(k)] = x;
            if x >= n && y >= m {
                end = Some(d);
                break 'search;
            }
            k += 2;
        }
    }
    let end = end?;
    let mut ops = Vec::new();
    let (mut x, mut y) = (n, m);
    for d in (0..=end).rev() {
        if d == 0 {
            while x > 0 && y > 0 {
                x -= 1;
                y -= 1;
                ops.push(Op::Equal {
                    a: x as usize,
                    b: y as usize,
                });
            }
            break;
        }
        let snapshot = &trace[d as usize];
        let get = |k: isize| snapshot[(k + d) as usize];
        let k = x - y;
        let down = k == -d || (k != d && get(k - 1) < get(k + 1));
        let prev_k = if down { k + 1 } else { k - 1 };
        let prev_x = get(prev_k);
        let prev_y = prev_x - prev_k;
        let snake_start = if down { prev_x } else { prev_x + 1 };
        while x > snake_start {
            x -= 1;
            y -= 1;
            ops.push(Op::Equal {
                a: x as usize,
                b: y as usize,
            });
        }
        ops.push(if down {
            Op::Insert { b: prev_y as usize }
        } else {
            Op::Delete { a: prev_x as usize }
        });
        x = prev_x;
        y = prev_y;
    }
    ops.reverse();
    Some(ops)
}

/// The edit script that turns `a` into `b`: common lines first and last are
/// matched directly, the middle by Myers' algorithm.
pub fn line_diff(a: &[&str], b: &[&str]) -> Vec<Op> {
    let prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (a_mid, b_mid) = (&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]);
    let mut ops: Vec<Op> = (0..prefix).map(|i| Op::Equal { a: i, b: i }).collect();
    match myers(a_mid, b_mid) {
        Some(middle) => ops.extend(middle.into_iter().map(|op| match op {
            Op::Equal { a, b } => Op::Equal {
                a: a + prefix,
                b: b + prefix,
            },
            Op::Delete { a } => Op::Delete { a: a + prefix },
            Op::Insert { b } => Op::Insert { b: b + prefix },
        })),
        None => {
            ops.extend((0..a_mid.len()).map(|i| Op::Delete { a: prefix + i }));
            ops.extend((0..b_mid.len()).map(|j| Op::Insert { b: prefix + j }));
        }
    }
    ops.extend((0..suffix).map(|k| Op::Equal {
        a: a.len() - suffix + k,
        b: b.len() - suffix + k,
    }));
    ops
}

/// One hunk of a unified diff. Starts are 1-based; for an empty side, the
/// start is the line after which the change goes (0 for the very top).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub a_start: usize,
    pub a_len: usize,
    pub b_start: usize,
    pub b_len: usize,
    /// Each line with its tag: `' '` unchanged, `'-'` removed, `'+'` added.
    pub lines: Vec<(char, String)>,
}

/// The changes from `a` to `b` as hunks with `context` unchanged lines around
/// each change; changes closer than twice that share a hunk.
pub fn unified(a: &[&str], b: &[&str], context: usize) -> Vec<Hunk> {
    let ops = line_diff(a, b);
    // Lines of each text consumed before each op.
    let mut before = Vec::with_capacity(ops.len() + 1);
    let (mut ai, mut bi) = (0usize, 0usize);
    for op in &ops {
        before.push((ai, bi));
        match op {
            Op::Equal { .. } => {
                ai += 1;
                bi += 1;
            }
            Op::Delete { .. } => ai += 1,
            Op::Insert { .. } => bi += 1,
        }
    }
    before.push((ai, bi));
    let changes: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, op)| !matches!(op, Op::Equal { .. }))
        .map(|(k, _)| k)
        .collect();
    let mut hunks = Vec::new();
    let mut i = 0;
    while i < changes.len() {
        let first = changes[i];
        let mut last = first;
        let mut j = i + 1;
        while j < changes.len() && changes[j] - last <= 2 * context + 1 {
            last = changes[j];
            j += 1;
        }
        let from = first.saturating_sub(context);
        let to = (last + context).min(ops.len() - 1);
        let mut lines = Vec::new();
        let (mut a_len, mut b_len) = (0, 0);
        for op in &ops[from..=to] {
            match *op {
                Op::Equal { a: x, .. } => {
                    lines.push((' ', a[x].to_string()));
                    a_len += 1;
                    b_len += 1;
                }
                Op::Delete { a: x } => {
                    lines.push(('-', a[x].to_string()));
                    a_len += 1;
                }
                Op::Insert { b: y } => {
                    lines.push(('+', b[y].to_string()));
                    b_len += 1;
                }
            }
        }
        let (a0, b0) = before[from];
        hunks.push(Hunk {
            a_start: if a_len > 0 { a0 + 1 } else { a0 },
            a_len,
            b_start: if b_len > 0 { b0 + 1 } else { b0 },
            b_len,
            lines,
        });
        i = j;
    }
    hunks
}

/// `a` with `hunks` applied: the new text, when the hunks are `a`'s.
pub fn apply(a: &[&str], hunks: &[Hunk]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(a.len());
    let mut next = 0usize;
    for h in hunks {
        let start = if h.a_len == 0 { h.a_start } else { h.a_start - 1 };
        let start = start.clamp(next, a.len());
        out.extend(a[next..start].iter().map(|l| l.to_string()));
        for (tag, line) in &h.lines {
            if *tag != '-' {
                out.push(line.clone());
            }
        }
        next = (start + h.a_len).min(a.len());
    }
    out.extend(a[next..].iter().map(|l| l.to_string()));
    out
}

/// The hunks as text, their line numbers moved by the given offsets (the
/// line before each text's first line).
fn render(out: &mut String, hunks: &[Hunk], a_offset: usize, b_offset: usize) {
    for h in hunks {
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            h.a_start + a_offset,
            h.a_len,
            h.b_start + b_offset,
            h.b_len
        ));
        for (tag, line) in &h.lines {
            out.push(*tag);
            out.push_str(line);
            out.push('\n');
        }
    }
}

/// What a re-run of a command would cost as a diff against run `run`'s
/// output `earlier`: a header and the changed lines with 2 lines of context.
/// `None` when nothing changed, when the hunks do not rebuild `now` exactly,
/// or when the diff is not worth sending. Measured only.
pub fn rerun_delta(earlier: &str, now: &str, run: usize, cmd: &str) -> Option<String> {
    let a: Vec<&str> = earlier.lines().collect();
    let b: Vec<&str> = now.lines().collect();
    if a == b {
        return None;
    }
    let hunks = unified(&a, &b, CONTEXT);
    if apply(&a, &hunks) != b {
        return None;
    }
    let mut out = format!(
        "lge: same command as run {run} of `{cmd}` earlier; only what changed since that run is shown ({} lines before, {} now):\n",
        a.len(),
        b.len()
    );
    render(&mut out, &hunks, 0, 0);
    worth_it(now, &out).then_some(out)
}

/// [`rerun_delta`] for a file read again: `earlier` holds the file's lines
/// from line `earlier_start` on, `now` from `now_start` on, and hunks are
/// numbered in file lines. When every line of `now` equals the line with the
/// same number in `earlier`, one line says so. Measured only.
pub fn read_delta(earlier: &str, earlier_start: usize, now: &str, now_start: usize, cmd: &str) -> Option<String> {
    let a: Vec<&str> = earlier.lines().collect();
    let b: Vec<&str> = now.lines().collect();
    if b.is_empty() || earlier_start == 0 || now_start == 0 {
        return None;
    }
    let now_end = now_start + b.len() - 1;
    let covered = now_start >= earlier_start
        && b.iter()
            .enumerate()
            .all(|(k, line)| a.get(now_start - earlier_start + k) == Some(line));
    let out = if covered {
        format!(
            "lge: lines {now_start}-{now_end} are identical to the same lines in the output of `{cmd}` earlier; not repeated here.\n"
        )
    } else {
        // Compare with the lines of the earlier read this read covers, not with
        // the whole earlier read.
        if now_start < earlier_start {
            return None;
        }
        let skip = (now_start - earlier_start).min(a.len());
        let window = &a[skip..(skip + b.len()).min(a.len())];
        let first = earlier_start.max(now_start);
        let hunks = unified(window, &b, CONTEXT);
        if window.is_empty() || hunks.is_empty() || apply(window, &hunks) != b {
            return None;
        }
        let mut out = format!(
            "lge: lines {now_start}-{now_end} were read earlier by `{cmd}` (then lines {first}-{}); only what changed since then is shown, numbered in file lines:\n",
            first + window.len() - 1
        );
        render(&mut out, &hunks, first - 1, now_start - 1);
        out
    };
    worth_it(now, &out).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> usize {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) as usize
        }
    }

    /// A text and a version of it with random lines changed, removed and added.
    fn edited(rng: &mut Lcg, len: usize, edits: usize) -> (Vec<String>, Vec<String>) {
        let words = ["alpha", "beta", "gamma", "delta", "}", "", "    return x;"];
        let a: Vec<String> = (0..len).map(|_| words[rng.next() % words.len()].to_string()).collect();
        let mut b = a.clone();
        for _ in 0..edits {
            match rng.next() % 3 {
                0 if !b.is_empty() => {
                    let at = rng.next() % b.len();
                    b.remove(at);
                }
                1 => {
                    let at = rng.next() % (b.len() + 1);
                    b.insert(at, format!("new line {}", rng.next() % 100));
                }
                _ if !b.is_empty() => {
                    let at = rng.next() % b.len();
                    b[at] = format!("changed {}", rng.next() % 100);
                }
                _ => {}
            }
        }
        (a, b)
    }

    #[test]
    fn applying_the_diff_gives_the_new_text() {
        let mut rng = Lcg(42);
        for case in 0..500 {
            let len = rng.next() % 60;
            let edits = rng.next() % 12;
            let (a, b) = edited(&mut rng, len, edits);
            let a: Vec<&str> = a.iter().map(String::as_str).collect();
            let b: Vec<&str> = b.iter().map(String::as_str).collect();
            for context in [0, 2, 5] {
                let hunks = unified(&a, &b, context);
                assert_eq!(apply(&a, &hunks), b, "case {case}, context {context}");
            }
            // The script is minimal enough to be one: equal lines really are equal.
            for op in line_diff(&a, &b) {
                if let Op::Equal { a: x, b: y } = op {
                    assert_eq!(a[x], b[y]);
                }
            }
        }
    }

    #[test]
    fn a_large_difference_falls_back_to_whole_blocks_and_still_applies() {
        let a: Vec<String> = (0..1500).map(|i| format!("old {i}")).collect();
        let b: Vec<String> = (0..1500).map(|i| format!("new {i}")).collect();
        let a: Vec<&str> = a.iter().map(String::as_str).collect();
        let b: Vec<&str> = b.iter().map(String::as_str).collect();
        let hunks = unified(&a, &b, 2);
        assert_eq!(apply(&a, &hunks), b);
    }

    #[test]
    fn hunks_are_numbered_and_carry_their_context() {
        let a = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"];
        let b = ["a", "b", "c", "D", "e", "f", "g", "h", "i", "j", "k"];
        let hunks = unified(&a, &b, 2);
        assert_eq!(hunks.len(), 2);
        assert_eq!(
            (hunks[0].a_start, hunks[0].a_len, hunks[0].b_start, hunks[0].b_len),
            (2, 5, 2, 5)
        );
        let want: Vec<(char, String)> = [(' ', "b"), (' ', "c"), ('-', "d"), ('+', "D"), (' ', "e"), (' ', "f")]
            .iter()
            .map(|(t, l)| (*t, l.to_string()))
            .collect();
        assert_eq!(hunks[0].lines, want);
        assert_eq!((hunks[1].a_start, hunks[1].b_start, hunks[1].b_len), (9, 9, 3));
    }

    fn test_log(failing: usize) -> String {
        let mut text = String::from("running 300 tests\n");
        for i in 0..300 {
            let verdict = if i == failing { "FAILED" } else { "ok" };
            text.push_str(&format!("test orders::tests::case_number_{i} ... {verdict}\n"));
        }
        text.push_str("test result: FAILED. 299 passed; 1 failed\n");
        text
    }

    #[test]
    fn a_rerun_shows_only_what_changed() {
        let (earlier, now) = (test_log(17), test_log(250));
        let out = rerun_delta(&earlier, &now, 1, "cargo test").expect("worth it");
        assert!(out.starts_with("lge: same command as run 1 of `cargo test` earlier; only what changed since that run is shown (302 lines before, 302 now):\n@@ -17,5 +17,5 @@\n"), "{out}");
        assert!(out
            .contains("-test orders::tests::case_number_17 ... FAILED\n+test orders::tests::case_number_17 ... ok\n"));
        assert!(out.contains("+test orders::tests::case_number_250 ... FAILED\n"));
        assert_eq!(
            rerun_delta(&earlier, &earlier, 1, "cargo test"),
            None,
            "an identical run is the pointer's job"
        );
        // A completely different output is not worth a diff.
        let other: String = (0..300).map(|i| format!("line {i} of something else\n")).collect();
        assert_eq!(rerun_delta(&earlier, &other, 1, "cargo test"), None);
    }

    fn numbered(from: usize, to: usize, changed: Option<usize>) -> String {
        (from..=to)
            .map(|n| {
                let code = if Some(n) == changed {
                    "CHANGED"
                } else {
                    "let value = compute(input, other_argument);"
                };
                format!("{n:>6}\t    {code} // line {n}\n")
            })
            .collect()
    }

    #[test]
    fn a_read_inside_an_earlier_read_is_one_line() {
        let earlier = numbered(1, 400, None);
        let now = numbered(100, 300, None);
        assert_eq!(
            read_delta(&earlier, 1, &now, 100, "nl -ba src/a.rs").as_deref(),
            Some("lge: lines 100-300 are identical to the same lines in the output of `nl -ba src/a.rs` earlier; not repeated here.\n")
        );
        // A changed line is shown as a hunk in file line numbers.
        let changed = numbered(1, 400, Some(250));
        let out = read_delta(&earlier, 1, &changed, 1, "nl -ba src/a.rs").expect("worth it");
        assert!(out.contains("\n@@ -248,5 +248,5 @@\n"), "{out}");
        let shifted = numbered(101, 400, Some(250));
        let out =
            read_delta(&numbered(101, 400, None), 101, &shifted, 101, "sed -n '101,400p' a.rs").expect("worth it");
        assert!(out.contains("\n@@ -248,5 +248,5 @@\n"), "{out}");
        // A range that starts before the earlier one is not covered.
        assert!(
            read_delta(&numbered(100, 300, None), 100, &numbered(50, 300, None), 50, "x")
                .is_none_or(|o| !o.contains("identical"))
        );
    }

    #[test]
    fn a_narrower_re_read_after_an_edit_is_compared_with_its_own_lines_of_the_earlier_read() {
        let earlier = numbered(1, 400, None);
        let now = numbered(100, 160, Some(130));
        let out = read_delta(&earlier, 1, &now, 100, "nl -ba src/a.rs | sed -n '100,160p'").expect("worth it");
        assert!(out.contains("(then lines 100-160)"), "{out}");
        assert!(out.contains("\n@@ -128,5 +128,5 @@\n"), "{out}");
        assert!(out.contains("CHANGED"), "{out}");
        assert!(!out.contains("line 99 ") && !out.contains("line 200 "), "{out}");
    }
}
