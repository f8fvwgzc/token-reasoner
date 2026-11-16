//! `lge init`: what connects a repository to Codex, printed, and written on
//! request: the lge block of AGENTS.md, and the opt-in terse-replies skill
//! with its optional AGENTS.md section. The texts are in
//! [`lge_bench::agents`], shared with the A/B runner's variants.

use anyhow::{bail, Context, Result};
use lge_bench::agents::{
    agents_block, drop_terse_section, has_skill_marker, put_block, put_terse_section, TERSE_SECTION, TERSE_SKILL,
    TERSE_SKILL_NAME, TERSE_START,
};
use std::io::Write;
use std::path::{Path, PathBuf};

/// `--terse[=on|always|off]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum TerseMode {
    /// Write the terse-replies skill: Codex uses it when replies should be short.
    On,
    /// The skill, and with --write a short section in AGENTS.md so every reply is short.
    Always,
    /// Remove the skill and the section, where lge wrote them.
    Off,
}

pub struct InitOptions {
    /// Write the lge block (and with `--terse=always`, the section) into AGENTS.md.
    pub write: bool,
    /// Register the MCP server by running `codex mcp add`.
    pub register: bool,
    /// The block with the batch line.
    pub batch: bool,
    pub terse: Option<TerseMode>,
    /// The skill in the user's skills folder instead of the repository's.
    pub global: bool,
}

/// Where things go.
pub struct Places {
    pub repo: PathBuf,
    /// The user's home folder, for `--global`.
    pub home: Option<PathBuf>,
    /// The lge binary, for the MCP registration command.
    pub exe: String,
}

/// The folder of skill folders Codex reads: `<repo>/.agents/skills` for the
/// repository (Codex reads `.agents/skills` in every folder from the project
/// root down to where it runs), `~/.agents/skills` for the user (Codex's
/// current user location; `$CODEX_HOME/skills` is its deprecated one).
pub fn skills_dir(places: &Places, global: bool) -> Result<PathBuf> {
    let root = if global {
        places
            .home
            .clone()
            .context("--global needs the home folder, and HOME is not set")?
    } else {
        places.repo.clone()
    };
    Ok(root.join(".agents").join("skills"))
}

/// What a write did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Written,
    Unchanged,
    Removed,
    /// Nothing of lge's was there.
    Absent,
    /// There, but without lge's marker: left as it is.
    NotLge,
}

fn read(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

fn write(path: &Path, old: &str, new: &str) -> Result<Change> {
    if new == old {
        return Ok(Change::Unchanged);
    }
    std::fs::write(path, new).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(Change::Written)
}

/// Put `block` into AGENTS.md (see [`put_block`]).
pub fn write_block(agents: &Path, block: &str) -> Result<Change> {
    let old = read(agents)?.unwrap_or_default();
    write(agents, &old, &put_block(&old, block))
}

/// Put the terse-replies section into AGENTS.md, after the lge block.
pub fn write_terse_section(agents: &Path) -> Result<Change> {
    let old = read(agents)?.unwrap_or_default();
    write(agents, &old, &put_terse_section(&old))
}

/// Take the terse-replies section out of AGENTS.md.
pub fn remove_terse_section(agents: &Path) -> Result<Change> {
    let Some(old) = read(agents)? else {
        return Ok(Change::Absent);
    };
    if !old.contains(TERSE_START) {
        return Ok(Change::Absent);
    }
    write(agents, &old, &drop_terse_section(&old)).map(|c| match c {
        Change::Written => Change::Removed,
        other => other,
    })
}

/// Write the skill to `path`. A file there without lge's marker is not
/// overwritten: an error.
pub fn write_skill(path: &Path) -> Result<Change> {
    match read(path)? {
        Some(old) if old == TERSE_SKILL => Ok(Change::Unchanged),
        Some(old) if !has_skill_marker(&old) => bail!(
            "{} exists and was not written by lge; it is left as it is (move it away to let lge write its skill there)",
            path.display()
        ),
        _ => {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
            }
            std::fs::write(path, TERSE_SKILL).with_context(|| format!("cannot write {}", path.display()))?;
            Ok(Change::Written)
        }
    }
}

/// Remove the skill at `path` when lge wrote it, and its folder when that is
/// then empty.
pub fn remove_skill(path: &Path) -> Result<Change> {
    match read(path)? {
        None => Ok(Change::Absent),
        Some(old) if !has_skill_marker(&old) => Ok(Change::NotLge),
        Some(_) => {
            std::fs::remove_file(path).with_context(|| format!("cannot remove {}", path.display()))?;
            if let Some(dir) = path.parent() {
                let _ = std::fs::remove_dir(dir);
            }
            Ok(Change::Removed)
        }
    }
}

/// Print, and on request apply, what connects the repository to Codex.
pub fn run(places: &Places, opts: &InitOptions, out: &mut dyn Write) -> Result<()> {
    writeln!(
        out,
        "A. The proxy: every Codex session goes through lge, nothing else to remember.\n"
    )?;
    writeln!(
        out,
        "   1. Start the daemon:  lge serve      (or, in Docker:  docker compose up -d)"
    )?;
    writeln!(
        out,
        "   2. Add one line to ~/.codex/config.toml (the CLI and the desktop app read the same file;"
    )?;
    writeln!(out, "      your ChatGPT or API-key login stays as it is):\n")?;
    writeln!(out, "        openai_base_url = \"http://127.0.0.1:8787/v1\"\n")?;
    writeln!(
        out,
        "   3. Open http://127.0.0.1:8787 to add folders and see tokens per session."
    )?;
    writeln!(
        out,
        "      Graph tools for the agent:  codex mcp add local_graph --url http://127.0.0.1:8787/mcp\n"
    )?;

    let command = ["mcp", "add", "local_graph", "--", &places.exe, "mcp"].map(str::to_string);
    writeln!(
        out,
        "B. Without the daemon: the graph tools only, started by Codex in the repository it works in.\n"
    )?;
    writeln!(out, "   lge index")?;
    writeln!(out, "   codex {}\n", command.join(" "))?;
    if opts.register {
        let status = std::process::Command::new("codex")
            .args(&command)
            .status()
            .context("cannot run `codex`; is the Codex CLI on PATH?")?;
        if !status.success() {
            bail!("`codex mcp add` failed");
        }
        writeln!(out, "   registered.\n")?;
    }

    let agents = places.repo.join("AGENTS.md");
    let block = agents_block(opts.batch);
    writeln!(
        out,
        "C. The rule that makes the agent ask the graph first belongs in {}:\n",
        agents.display()
    )?;
    writeln!(out, "{block}")?;
    if opts.write {
        match write_block(&agents, block)? {
            Change::Unchanged => writeln!(out, "   {} already has this block.", agents.display())?,
            _ => writeln!(out, "   written to {}.", agents.display())?,
        }
    }

    let Some(mode) = opts.terse else {
        return Ok(());
    };
    let skill = skills_dir(places, opts.global)?.join(TERSE_SKILL_NAME).join("SKILL.md");
    writeln!(
        out,
        "\nD. Terse replies: the Codex skill `{TERSE_SKILL_NAME}` ({}).\n",
        if opts.global {
            "for every repository"
        } else {
            "for this repository"
        }
    )?;
    match mode {
        TerseMode::On | TerseMode::Always => {
            match write_skill(&skill)? {
                Change::Unchanged => writeln!(out, "   {} is already up to date.", skill.display())?,
                _ => writeln!(out, "   skill written to {}.", skill.display())?,
            }
            if mode == TerseMode::Always {
                writeln!(
                    out,
                    "\n   So that every reply is short without invoking the skill, this section goes after the lge block:\n"
                )?;
                writeln!(out, "{TERSE_SECTION}")?;
                if opts.write {
                    match write_terse_section(&agents)? {
                        Change::Unchanged => writeln!(out, "   {} already has this section.", agents.display())?,
                        _ => writeln!(out, "   section written to {}.", agents.display())?,
                    }
                } else {
                    writeln!(out, "   add --write to put it in {}.", agents.display())?;
                }
            } else if read(&agents)?.is_some_and(|t| t.contains(TERSE_START)) {
                writeln!(
                    out,
                    "   {} still has the terse-replies section; --terse=off removes it.",
                    agents.display()
                )?;
            }
        }
        TerseMode::Off => {
            match remove_skill(&skill)? {
                Change::Removed => writeln!(out, "   skill removed from {}.", skill.display())?,
                Change::NotLge => writeln!(out, "   {} was not written by lge: left as it is.", skill.display())?,
                _ => writeln!(out, "   no skill of lge's at {}.", skill.display())?,
            }
            match remove_terse_section(&agents)? {
                Change::Removed => writeln!(out, "   section removed from {}.", agents.display())?,
                _ => writeln!(out, "   no terse-replies section in {}.", agents.display())?,
            }
        }
    }
    writeln!(
        out,
        "\n   Codex's own `model_verbosity` setting (low, medium or high, in ~/.codex/config.toml) also changes\n   \
         how long its replies are. Setting it is your choice; lge never sets it."
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lge_bench::agents::{AGENTS_BLOCK, AGENTS_BLOCK_BATCH, BATCH_LINE, SKILL_MARKER, TERSE_END};

    fn places(dir: &Path) -> Places {
        let repo = dir.join("repo");
        let home = dir.join("home");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        Places {
            repo,
            home: Some(home),
            exe: "/usr/local/bin/lge".to_string(),
        }
    }

    fn opts(write: bool, terse: Option<TerseMode>) -> InitOptions {
        InitOptions {
            write,
            register: false,
            batch: false,
            terse,
            global: false,
        }
    }

    fn init(p: &Places, o: &InitOptions) -> String {
        let mut out = Vec::new();
        run(p, o, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn agents(p: &Places) -> String {
        std::fs::read_to_string(p.repo.join("AGENTS.md")).unwrap_or_default()
    }

    fn skill(p: &Places) -> PathBuf {
        p.repo.join(".agents/skills/terse-replies/SKILL.md")
    }

    #[test]
    fn write_puts_the_block_in_an_empty_file_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let p = places(dir.path());
        std::fs::write(p.repo.join("AGENTS.md"), "").unwrap();
        let printed = init(&p, &opts(true, None));
        assert_eq!(agents(&p), AGENTS_BLOCK);
        assert!(printed.contains("written to"), "{printed}");
        assert!(printed.contains(AGENTS_BLOCK), "the block is printed");
        assert!(
            !printed.contains("D. Terse"),
            "nothing about terse replies unless asked"
        );
        let again = init(&p, &opts(true, None));
        assert_eq!(agents(&p), AGENTS_BLOCK);
        assert!(again.contains("already has this block"), "{again}");
        // Without --write nothing is written.
        let dir = tempfile::tempdir().unwrap();
        let p = places(dir.path());
        init(&p, &opts(false, None));
        assert!(!p.repo.join("AGENTS.md").exists());
    }

    #[test]
    fn an_older_block_is_replaced_in_place_and_other_text_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let p = places(dir.path());
        let old = "# House rules\n\nUse tabs.\n\n<!-- lge:start -->\n## Code graph (lge)\n\n- old line\n<!-- lge:end -->\n\n## Tests\n\nRun `make test`.\n";
        std::fs::write(p.repo.join("AGENTS.md"), old).unwrap();
        init(&p, &opts(true, None));
        let new = agents(&p);
        assert_eq!(
            new,
            format!(
                "# House rules\n\nUse tabs.\n\n{}\n\n## Tests\n\nRun `make test`.\n",
                AGENTS_BLOCK.trim_end()
            )
        );
        // The only difference to the v0.0.1 block is the condensed-output line.
        assert!(new.contains("- Tool outputs may be condensed by lge."));
        assert!(!new.contains(BATCH_LINE.trim_end()), "the batch line is opt-in");
        // --batch adds it, and leaving it out again removes it.
        let mut batch = opts(true, None);
        batch.batch = true;
        init(&p, &batch);
        assert!(agents(&p).contains(AGENTS_BLOCK_BATCH.trim_end()));
        init(&p, &opts(true, None));
        assert_eq!(agents(&p), new);
    }

    #[test]
    fn terse_on_always_and_off_write_and_remove_idempotently() {
        let dir = tempfile::tempdir().unwrap();
        let p = places(dir.path());
        std::fs::write(p.repo.join("AGENTS.md"), "# Mine\n").unwrap();

        // on: the skill only.
        let printed = init(&p, &opts(false, Some(TerseMode::On)));
        assert_eq!(std::fs::read_to_string(skill(&p)).unwrap(), TERSE_SKILL);
        assert!(printed.contains("skill written to"), "{printed}");
        assert!(printed.contains("model_verbosity") && printed.contains("lge never sets it"));
        assert_eq!(agents(&p), "# Mine\n", "on leaves AGENTS.md alone");
        assert!(init(&p, &opts(false, Some(TerseMode::On))).contains("already up to date"));

        // always without --write: printed, not written.
        let printed = init(&p, &opts(false, Some(TerseMode::Always)));
        assert!(printed.contains(TERSE_SECTION) && printed.contains("add --write"));
        assert_eq!(agents(&p), "# Mine\n");

        // always with --write: the block, then the section after it.
        init(&p, &opts(true, Some(TerseMode::Always)));
        let with = agents(&p);
        assert_eq!(
            with,
            format!("# Mine\n\n{}\n\n{TERSE_SECTION}", AGENTS_BLOCK.trim_end())
        );
        init(&p, &opts(true, Some(TerseMode::Always)));
        assert_eq!(agents(&p), with, "idempotent");
        // --write without --terse keeps the section.
        init(&p, &opts(true, None));
        assert_eq!(agents(&p), with);
        assert!(init(&p, &opts(false, Some(TerseMode::On))).contains("still has the terse-replies section"));

        // off: both go, and the folder lge made.
        let printed = init(&p, &opts(false, Some(TerseMode::Off)));
        assert!(
            printed.contains("skill removed") && printed.contains("section removed"),
            "{printed}"
        );
        assert!(!skill(&p).exists() && !skill(&p).parent().unwrap().exists());
        assert_eq!(agents(&p), format!("# Mine\n\n{AGENTS_BLOCK}"));
        assert!(!agents(&p).contains(TERSE_END));
        let again = init(&p, &opts(false, Some(TerseMode::Off)));
        assert!(again.contains("no skill of lge's") && again.contains("no terse-replies section"));
    }

    #[test]
    fn a_skill_lge_did_not_write_is_never_overwritten_or_removed() {
        let dir = tempfile::tempdir().unwrap();
        let p = places(dir.path());
        let mine = "---\nname: terse-replies\ndescription: My own.\n---\n\nBe brief.\n";
        std::fs::create_dir_all(skill(&p).parent().unwrap()).unwrap();
        std::fs::write(skill(&p), mine).unwrap();
        let mut out = Vec::new();
        let err = run(&p, &opts(false, Some(TerseMode::On)), &mut out).unwrap_err();
        assert!(format!("{err:#}").contains("was not written by lge"), "{err:#}");
        assert_eq!(std::fs::read_to_string(skill(&p)).unwrap(), mine);
        let printed = init(&p, &opts(false, Some(TerseMode::Off)));
        assert!(printed.contains("left as it is"), "{printed}");
        assert_eq!(std::fs::read_to_string(skill(&p)).unwrap(), mine);
        // An older skill of lge's (it carries the marker) is replaced.
        std::fs::write(skill(&p), format!("---\n# {SKILL_MARKER}\nname: x\n---\nold\n")).unwrap();
        init(&p, &opts(false, Some(TerseMode::On)));
        assert_eq!(std::fs::read_to_string(skill(&p)).unwrap(), TERSE_SKILL);
    }

    #[test]
    fn global_puts_the_skill_in_the_users_folder() {
        let dir = tempfile::tempdir().unwrap();
        let p = places(dir.path());
        let mut o = opts(false, Some(TerseMode::On));
        o.global = true;
        let printed = init(&p, &o);
        let global = p.home.as_ref().unwrap().join(".agents/skills/terse-replies/SKILL.md");
        assert_eq!(std::fs::read_to_string(&global).unwrap(), TERSE_SKILL);
        assert!(!skill(&p).exists());
        assert!(printed.contains("for every repository"));
        o.terse = Some(TerseMode::Off);
        init(&p, &o);
        assert!(!global.exists());
    }
}
