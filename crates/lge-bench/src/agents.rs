//! The text lge puts in a repository for Codex: the lge block of AGENTS.md,
//! the optional terse-replies section after it, and the terse-replies skill.
//!
//! `lge init` writes them and the A/B runner's variants write them into its
//! copies, so both use the same bytes. Every piece sits between lge's own
//! markers, and only text between those markers is ever replaced or removed.

/// Where the lge block starts and ends in AGENTS.md.
pub const BLOCK_START: &str = "<!-- lge:start -->";
pub const BLOCK_END: &str = "<!-- lge:end -->";
/// Where the terse-replies section starts and ends in AGENTS.md.
pub const TERSE_START: &str = "<!-- lge:terse:start -->";
pub const TERSE_END: &str = "<!-- lge:terse:end -->";

/// The lge block up to its last default line; the batch line, when asked
/// for, and the end marker follow.
macro_rules! block_head {
    () => {
        "<!-- lge:start -->
## Code graph (lge)

This repository is indexed by a local code graph, served by the `local_graph` MCP tools.

- Before the first grep, directory listing or file read for a code question, call `search_code_context` with the task or question. Put this rule in every sub-agent prompt that explores code.
- Before editing a symbol or file, call `analyze_change_impact`: it lists the affected code and the tests to run.
- Use `trace_execution_flow`, `find_dependencies` and `get_architecture_context` for how code connects, and `get_relevant_source` for exact lines.
- Read raw files only for line ranges these tools located.
- Edges marked inferred or unresolved are not verified: check them in the source.
- `recall_memory` returns decisions and conventions from earlier sessions. At the end of a task, store one with `retain_memory`.
- Tool outputs may be condensed by lge. A line starting with `lge:` says what was left out and how to get it back; treat everything else as the complete output. Running the same command again before changing anything prints it whole.
"
    };
}

/// The line `lge init --batch` adds. Its shape is the one the proxy can
/// attribute part by part (`lge_proxy::command::script_plan`): one
/// `tools.exec_command` call per command, each result printed once with
/// `text(NAME.output)`.
macro_rules! batch_line {
    () => {
        "- Batch related read-only commands (several file ranges, or a search and the reads it points to) into one exec script: one `tools.exec_command` call per command and one `text(result.output)` per result.\n"
    };
}

/// The rule Codex follows in this repository. Kept short: it is read in every session.
pub const AGENTS_BLOCK: &str = concat!(block_head!(), "<!-- lge:end -->\n");

/// [`AGENTS_BLOCK`] with the batch line (`lge init --batch`).
pub const AGENTS_BLOCK_BATCH: &str = concat!(block_head!(), batch_line!(), "<!-- lge:end -->\n");

/// The batch line on its own.
pub const BATCH_LINE: &str = batch_line!();

/// The lge block, with or without the batch line.
pub fn agents_block(batch: bool) -> &'static str {
    if batch {
        AGENTS_BLOCK_BATCH
    } else {
        AGENTS_BLOCK
    }
}

/// The section `lge init --terse=always --write` puts after the lge block:
/// the terse-replies rules for every session, without invoking the skill.
/// At most [`TERSE_SECTION_MAX_TOKENS`] tokens.
pub const TERSE_SECTION: &str = "<!-- lge:terse:start -->
## Replies

Answer first. No greeting, preamble, recap or closing offer, and no narration between routine tool calls. Use short plain words, no invented abbreviations, no arrows for words. Keep code, commands, paths, error text, numbers and every negation exact. Write full sentences for security warnings, destructive or irreversible steps, questions to the user, and anything written to files, commits or PRs. Reply in the user's language.
<!-- lge:terse:end -->
";

/// The most tokens [`TERSE_SECTION`] may cost: it is read in every request.
pub const TERSE_SECTION_MAX_TOKENS: usize = 120;

/// Name of the Codex skill `lge init --terse` writes, and of its folder.
pub const TERSE_SKILL_NAME: &str = "terse-replies";

/// The comment in the first lines of a skill file lge wrote. A file without
/// it is never overwritten or removed.
pub const SKILL_MARKER: &str = "lge:terse-replies";

/// Lines at the top of a skill file in which [`SKILL_MARKER`] is looked for.
const MARKER_LINES: usize = 5;

/// The terse-replies skill. Codex reads the YAML front matter (it must start
/// on the first line); the marker is a YAML comment inside it.
pub const TERSE_SKILL: &str = "---
name: terse-replies
description: Reply in as few words as the facts need, keeping code, commands, paths, numbers and negations exact; use when the user wants short answers.
# lge:terse-replies (written by `lge init --terse`; `lge init --terse=off` removes it)
---

# Terse replies

Say the same facts in fewer words.

- Give the answer first. No greeting, preamble, recap or closing offer.
- Do not narrate routine tool calls. Before a run of several steps, write one line on what you are about to do; after it, one line with the result.
- Use short, plain words. Do not invent abbreviations, and do not put arrows or symbols in place of words.
- Keep code, commands, paths, identifiers, error text and numbers exactly as they are.
- Never drop a negation or a limit: not, never, no, only, except.
- If the short form is not actually shorter, or could be read two ways, write the plain sentence.
- Write full sentences for security warnings, for confirmations before a destructive or irreversible step, for questions to the user, and for anything written to files, commits, pull requests, issues or docs.
- Reply in the user's language.

## Ultra

Only when the user asks for ultra-short replies: sentence fragments are fine, and each fact is said once. Every rule above still holds.
";

/// True when `text`, a skill file, carries lge's marker in its first lines.
pub fn has_skill_marker(text: &str) -> bool {
    text.lines().take(MARKER_LINES).any(|l| l.contains(SKILL_MARKER))
}

/// Byte range of the first `start .. end` pair (the end marker included), when
/// the end comes after the start.
fn marked(text: &str, start: &str, end: &str) -> Option<(usize, usize)> {
    let s = text.find(start)?;
    let e = s + text[s..].find(end)? + end.len();
    Some((s, e))
}

/// `old` (the text of AGENTS.md, "" when missing) with `block` in place of
/// an earlier lge block, or appended after a blank line when there is none.
/// Everything outside the markers is kept byte for byte, and applying the
/// same block twice changes nothing.
pub fn put_block(old: &str, block: &str) -> String {
    match marked(old, BLOCK_START, BLOCK_END) {
        Some((s, e)) => format!("{}{}{}", &old[..s], block.trim_end(), &old[e..]),
        None if old.trim().is_empty() => block.to_string(),
        None => format!("{}\n\n{block}", old.trim_end()),
    }
}

/// `old` with [`TERSE_SECTION`]: replaced in place when it is there, else
/// right after the lge block (after a blank line), else at the end.
pub fn put_terse_section(old: &str) -> String {
    let section = TERSE_SECTION.trim_end();
    if let Some((s, e)) = marked(old, TERSE_START, TERSE_END) {
        return format!("{}{section}{}", &old[..s], &old[e..]);
    }
    match marked(old, BLOCK_START, BLOCK_END) {
        Some((_, e)) => format!("{}\n\n{section}{}", &old[..e], &old[e..]),
        None if old.trim().is_empty() => TERSE_SECTION.to_string(),
        None => format!("{}\n\n{TERSE_SECTION}", old.trim_end()),
    }
}

/// `old` without the terse-replies section, with the blank lines written
/// before it; unchanged when there is none. Undoes [`put_terse_section`].
pub fn drop_terse_section(old: &str) -> String {
    match marked(old, TERSE_START, TERSE_END) {
        Some((s, e)) => {
            let before = old[..s].trim_end_matches(['\n', '\r']);
            let after = &old[e..];
            if before.is_empty() {
                after.trim_start_matches(['\n', '\r']).to_string()
            } else {
                format!("{before}{after}")
            }
        }
        None => old.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lge_retrieve::budget::count;

    #[test]
    fn token_counts_are_pinned() {
        // Every request pays for these: a change in their size is a decision.
        assert_eq!(count(AGENTS_BLOCK), 243, "AGENTS_BLOCK");
        assert_eq!(count(AGENTS_BLOCK_BATCH), 289, "AGENTS_BLOCK_BATCH");
        assert_eq!(count(TERSE_SECTION), 107, "TERSE_SECTION");
        assert_eq!(count(TERSE_SKILL), 300, "TERSE_SKILL");
        assert!(count(TERSE_SECTION) <= TERSE_SECTION_MAX_TOKENS);
    }

    #[test]
    fn the_blocks_differ_only_by_the_batch_line() {
        assert_eq!(AGENTS_BLOCK_BATCH.replace(BATCH_LINE, ""), AGENTS_BLOCK);
        for block in [AGENTS_BLOCK, AGENTS_BLOCK_BATCH] {
            assert!(block.starts_with(BLOCK_START) && block.trim_end().ends_with(BLOCK_END));
            assert_eq!(block.matches(BLOCK_START).count(), 1);
        }
        assert!(AGENTS_BLOCK.contains("A line starting with `lge:` says what was left out"));
        assert!(!AGENTS_BLOCK.contains("exec script"));
    }

    #[test]
    fn nothing_borrows_a_trademark() {
        for text in [AGENTS_BLOCK_BATCH, TERSE_SECTION, TERSE_SKILL] {
            assert!(!text.to_lowercase().contains("caveman"));
        }
    }

    #[test]
    fn the_skill_has_codex_front_matter_and_the_marker() {
        let mut lines = TERSE_SKILL.lines();
        assert_eq!(
            lines.next(),
            Some("---"),
            "Codex needs the front matter on the first line"
        );
        assert!(TERSE_SKILL.contains("\nname: terse-replies\n"));
        let description = TERSE_SKILL
            .lines()
            .find_map(|l| l.strip_prefix("description: "))
            .unwrap();
        assert_eq!(description.matches(". ").count(), 0, "one sentence: {description}");
        assert!(has_skill_marker(TERSE_SKILL));
        assert!(!has_skill_marker("---\nname: mine\ndescription: x\n---\n"));
        // The marker further down does not count.
        assert!(!has_skill_marker(&format!("a\nb\nc\nd\ne\nf {SKILL_MARKER}\n")));
        let close = TERSE_SKILL.lines().skip(1).position(|l| l == "---").unwrap();
        assert!(close <= MARKER_LINES, "the marker is inside the front matter");
    }

    #[test]
    fn the_block_goes_in_place_and_is_idempotent() {
        assert_eq!(put_block("", AGENTS_BLOCK), AGENTS_BLOCK);
        assert_eq!(put_block("  \n", AGENTS_BLOCK), AGENTS_BLOCK);
        let other = "# Notes\n\nKeep tests fast.\n";
        let added = put_block(other, AGENTS_BLOCK);
        assert_eq!(added, format!("# Notes\n\nKeep tests fast.\n\n{AGENTS_BLOCK}"));
        assert_eq!(put_block(&added, AGENTS_BLOCK), added);

        let old = "# Notes\n\n<!-- lge:start -->\nold rule\n<!-- lge:end -->\n\n## After\n\ntext\n";
        let new = put_block(old, AGENTS_BLOCK_BATCH);
        assert_eq!(
            new,
            format!("# Notes\n\n{}\n\n## After\n\ntext\n", AGENTS_BLOCK_BATCH.trim_end())
        );
        assert_eq!(put_block(&new, AGENTS_BLOCK_BATCH), new);
        // Back to the block without the batch line.
        assert_eq!(
            put_block(&new, AGENTS_BLOCK),
            format!("# Notes\n\n{}\n\n## After\n\ntext\n", AGENTS_BLOCK.trim_end())
        );
    }

    #[test]
    fn the_terse_section_follows_the_block_and_comes_off_again() {
        let file = format!("# Notes\n\n{AGENTS_BLOCK}\n## After\n");
        let with = put_terse_section(&file);
        let block_end = with.find(BLOCK_END).unwrap() + BLOCK_END.len();
        assert!(with[block_end..].starts_with(&format!("\n\n{TERSE_START}")), "{with}");
        assert!(with.ends_with("<!-- lge:terse:end -->\n\n## After\n"), "{with}");
        assert_eq!(put_terse_section(&with), with, "idempotent");
        assert_eq!(drop_terse_section(&with), file, "removed exactly");
        assert_eq!(drop_terse_section(&file), file, "nothing to remove");

        // A block that ends the file.
        let with = put_terse_section(AGENTS_BLOCK);
        assert_eq!(with, format!("{}\n\n{TERSE_SECTION}", AGENTS_BLOCK.trim_end()));
        assert_eq!(drop_terse_section(&with), AGENTS_BLOCK);

        // No block: at the end; alone in an empty file.
        assert_eq!(put_terse_section(""), TERSE_SECTION);
        assert_eq!(drop_terse_section(TERSE_SECTION), "");
        assert_eq!(put_terse_section("x\n"), format!("x\n\n{TERSE_SECTION}"));

        // Rewriting the block keeps the section where it is.
        let rewritten = put_block(&put_terse_section(&file), AGENTS_BLOCK_BATCH);
        assert!(rewritten.contains(TERSE_START) && rewritten.contains(BATCH_LINE.trim_end()));
    }
}
