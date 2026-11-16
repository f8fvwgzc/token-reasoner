//! A long list of paths, one per line (`rg --files`, `git ls-files`, `fd`,
//! `find`), with the folder written once for each run of files in it.
//!
//! Lossless: [`unfold_paths`] gives the list back byte for byte, and a fold
//! is only sent when it does. The order of the lines never changes.

use crate::shorten::worth_it;

/// A list with fewer simple path lines than this is left as it is.
const MIN_PATHS: usize = 40;
/// A folded line goes on with its folder repeated on a new line before it
/// reaches this many characters.
const MAX_LINE: usize = 200;
/// The first line of a folded list: how to read it.
pub const HEADER: &str = "lge: paths grouped by folder; a line \"dir/ a b\" stands for dir/a and dir/b.";
/// Lines that are messages of the listing tool, not paths. A list with any of
/// them is left as it is.
const MESSAGES: [&str; 6] = ["find: ", "fd: ", "rg: ", "grep: ", "fatal: ", "error: "];

/// The folder (up to and including the last `/`) and the name of a simple
/// path line: not empty, no whitespace, quote, backslash, colon or control
/// character, not ending in `/`, with a name after its last `/`.
fn simple(line: &str) -> Option<(&str, &str)> {
    if line.is_empty() || line.ends_with('/') {
        return None;
    }
    if line
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '\'' | '"' | '\\' | ':'))
    {
        return None;
    }
    let cut = line.rfind('/')?;
    Some(line.split_at(cut + 1))
}

/// Fold a list of paths: after [`HEADER`], each run of two or more
/// consecutive simple lines in the same folder becomes `dir/ name name ...`
/// (continued on a new line that repeats `dir/ ` before 200 characters); other
/// lines stay as they are. `None` when the list has fewer than 40 simple
/// lines, has a message of the tool or a line with `/ ` in it, does not unfold
/// to exactly the input, or is not worth sending.
pub fn fold_paths(text: &str) -> Option<String> {
    let folded = fold(text)?;
    (unfold_paths(&folded) == text && worth_it(text, &folded)).then_some(folded)
}

/// [`fold_paths`] without the checks on the result.
fn fold(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.split('\n').collect();
    // `split` gives an empty last piece for a final line break.
    let lines = match lines.split_last() {
        Some((&"", rest)) => rest,
        _ => &lines[..],
    };
    if lines
        .iter()
        .any(|l| l.contains("/ ") || MESSAGES.iter().any(|m| l.starts_with(m)))
    {
        return None;
    }
    let parsed: Vec<Option<(&str, &str)>> = lines.iter().map(|l| simple(l)).collect();
    if parsed.iter().flatten().count() < MIN_PATHS {
        return None;
    }
    let mut out: Vec<String> = vec![HEADER.to_string()];
    let mut i = 0;
    while i < lines.len() {
        let Some((dir, _)) = parsed[i] else {
            out.push(lines[i].to_string());
            i += 1;
            continue;
        };
        let mut j = i + 1;
        while j < lines.len() && parsed[j].is_some_and(|(d, _)| d == dir) {
            j += 1;
        }
        if j - i < 2 {
            out.push(lines[i].to_string());
            i = j;
            continue;
        }
        let mut line = dir.to_string();
        let mut chars = dir.chars().count();
        let mut names = 0;
        for (_, name) in parsed[i..j].iter().flatten() {
            let cost = 1 + name.chars().count();
            if names > 0 && chars + cost >= MAX_LINE {
                out.push(std::mem::replace(&mut line, dir.to_string()));
                chars = dir.chars().count();
                names = 0;
            }
            line.push(' ');
            line.push_str(name);
            chars += cost;
            names += 1;
        }
        out.push(line);
        i = j;
    }
    let mut folded = out.join("\n");
    if text.ends_with('\n') {
        folded.push('\n');
    }
    Some(folded)
}

/// The list a [`fold_paths`] output stands for: the header dropped, and each
/// line with a `/ ` split there into its folder and the space-separated names
/// after it. Other lines stay as they are.
pub fn unfold_paths(text: &str) -> String {
    let body = match text.strip_prefix(HEADER) {
        Some(rest) if rest.is_empty() => rest,
        Some(rest) => rest.strip_prefix('\n').unwrap_or(text),
        None => text,
    };
    let mut out = String::with_capacity(body.len() * 2);
    for segment in body.split_inclusive('\n') {
        let (line, end) = match segment.strip_suffix('\n') {
            Some(line) => (line, "\n"),
            None => (segment, ""),
        };
        match line.split_once("/ ") {
            Some((dir, names)) => {
                for (k, name) in names.split(' ').enumerate() {
                    if k > 0 {
                        out.push('\n');
                    }
                    out.push_str(dir);
                    out.push('/');
                    out.push_str(name);
                }
            }
            None => out.push_str(line),
        }
        out.push_str(end);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use lge_retrieve::budget::count;

    /// A small deterministic generator, so the property test needs no crate.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> usize {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) as usize
        }

        fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
            items[self.next() % items.len()]
        }
    }

    fn monorepo() -> String {
        let mut text = String::new();
        for package in ["billing", "orders", "search", "users", "web"] {
            for folder in ["src", "src/handlers", "src/models", "tests"] {
                for name in ["mod.rs", "create.rs", "update.rs", "delete.rs", "list.rs", "types.rs"] {
                    text.push_str(&format!("packages/{package}/{folder}/{name}\n"));
                }
            }
        }
        text
    }

    #[test]
    fn a_folded_list_unfolds_to_exactly_the_input() {
        let roots = ["", "./", "/srv/app/", "packages/web/", "src/ünïcode/", "a/b/c/d/e/f/"];
        let folders = ["src", "src/lib", "tests", "docs", "with space", "x", "é/ü", ""];
        let names = [
            "mod.rs",
            "index.ts",
            "README.md",
            "a",
            "日本語.txt",
            "name with spaces.py",
            "q'uote",
            "x:y",
            "trailing/",
            "",
            "café.rs",
            "mod.rs",
            "lib.rs",
        ];
        let mut rng = Lcg(7);
        let (mut folded, mut sent) = (0, 0);
        for case in 0..400 {
            let runs = 10 + rng.next() % 60;
            let mut text = String::new();
            for _ in 0..runs {
                // A run of files in one folder, as listings print them; the
                // same names come back in many folders.
                let dir = format!("{}{}", rng.pick(&roots), rng.pick(&folders));
                for _ in 0..1 + rng.next() % 8 {
                    text.push_str(&format!("{dir}/{}\n", rng.pick(&names)));
                }
            }
            if case % 3 == 0 {
                text.pop();
            }
            if let Some(out) = fold(&text) {
                assert_eq!(unfold_paths(&out), text, "case {case}");
                assert!(out.starts_with(HEADER));
                folded += 1;
            }
            if let Some(out) = fold_paths(&text) {
                assert_eq!(unfold_paths(&out), text, "case {case}");
                sent += 1;
            }
        }
        assert!(folded > 300 && sent > 0, "{folded} folded, {sent} worth sending");
    }

    #[test]
    fn a_monorepo_list_is_much_smaller_and_keeps_its_order() {
        let text = monorepo();
        let out = fold_paths(&text).expect("folded");
        assert!(count(&out) * 10 <= count(&text) * 7, "{out}");
        assert!(out.starts_with(&format!(
            "{HEADER}\npackages/billing/src/ mod.rs create.rs update.rs delete.rs list.rs types.rs\npackages/billing/src/handlers/ mod.rs"
        )));
        assert_eq!(unfold_paths(&out), text);
        // The folders come in the order the input had them.
        let at = |s: &str| out.find(s).unwrap();
        assert!(at("packages/orders/tests/ ") < at("packages/search/src/ "));
    }

    #[test]
    fn long_runs_go_on_with_the_folder_repeated() {
        let text: String = (0..120)
            .map(|i| format!("packages/service/src/generated/f{i:03}.rs\n"))
            .collect();
        let out = fold_paths(&text).unwrap();
        for line in out.lines().skip(1) {
            assert!(line.starts_with("packages/service/src/generated/ "), "{line}");
            assert!(line.chars().count() < MAX_LINE, "{line}");
        }
        assert!(out.lines().count() > 2);
        assert_eq!(unfold_paths(&out), text);
    }

    #[test]
    fn lists_that_are_not_plain_paths_are_left_alone() {
        let text = monorepo();
        assert_eq!(
            fold_paths(&text.lines().take(30).map(|l| format!("{l}\n")).collect::<String>()),
            None
        );
        for message in [
            "find: './private': Permission denied",
            "fd: error",
            "rg: ./x: No such file",
            "grep: x: Is a directory",
            "fatal: not a git repository",
            "error: bad",
        ] {
            assert_eq!(fold_paths(&format!("{text}{message}\n")), None, "{message}");
        }
        assert_eq!(fold_paths(&format!("{text}odd/ name\n")), None);
        // Lines that are not simple paths stay, in place.
        let mixed = format!("Listing:\n{text}two words here\n");
        let out = fold_paths(&mixed).unwrap();
        assert!(out.starts_with(&format!("{HEADER}\nListing:\n")));
        assert!(out.ends_with("\ntwo words here\n"));
        assert_eq!(unfold_paths(&out), mixed);
        // Names without a folder are not simple: nothing to fold.
        let flat: String = (0..100).map(|i| format!("file_{i}.rs\n")).collect();
        assert_eq!(fold_paths(&flat), None);
    }
}
