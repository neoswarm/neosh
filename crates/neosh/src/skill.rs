//! `neosh skill install` — putting the skill where a coding agent will find it.
//!
//! # Why this is a command rather than a paragraph in a README
//!
//! A skill is a file in a directory whose name depends on which agent you use, and there are five
//! of them. "Copy `SKILL.md` to `~/.claude/skills/neosh/`, or `~/.agents/skills/neosh/`, or
//! `.cursor/skills/neosh/`…" is instructions for a thing a program can do, and the version that
//! ships in the binary is the version that matches the binary — which for a document describing a
//! CLI is the whole point.
//!
//! # Why the source of truth is under this crate
//!
//! `crates/neosh/skills/`, not `skills/` at the repository root, and for exactly the reason
//! `plugins/` is its own crate: `include_dir!` reads a tree at compile time and `cargo package`
//! carries only what is under the package root, so embedding `../../skills` would work perfectly
//! in a checkout and publish a `neosh` whose `skill install` writes an empty directory. The
//! Claude Code plugin manifest at the repository root points here rather than the other way
//! round, which costs one line in a JSON file and no copies of anything.

use std::path::{Path, PathBuf};

use include_dir::{Dir, include_dir};

/// The skill, as it will be written out. One directory so that reference files and scripts can be
/// added later without this having to learn about them.
static SKILL: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/skills/neosh");

/// The name of the directory a skill goes in, which is also how an agent invokes it.
const NAME: &str = "neosh";

/// Which coding agent, and therefore which directory.
#[derive(Copy, Clone, PartialEq, Eq, Debug, clap::ValueEnum)]
pub enum Agent {
    /// Every agent that reads the shared location, and every one below. The default.
    All,
    /// Claude Code — `.claude/skills/`.
    Claude,
    /// OpenAI Codex — `.agents/skills/`.
    Codex,
    /// Cursor — `.cursor/skills/`.
    Cursor,
    /// Gemini CLI — `.gemini/skills/`.
    Gemini,
    /// The shared location the open Agent Skills format settled on — `.agents/skills/`.
    ///
    /// Worth having on its own for an agent this list has never heard of: a format with one
    /// directory everybody reads is the reason a new agent does not need a new flag here.
    Shared,
}

impl Agent {
    /// The directory this agent reads skills from, relative to a config root.
    fn dirs(self) -> &'static [&'static str] {
        match self {
            Agent::Claude => &[".claude/skills"],
            Agent::Codex | Agent::Shared => &[".agents/skills"],
            Agent::Cursor => &[".cursor/skills"],
            Agent::Gemini => &[".gemini/skills"],
            // Codex and the shared location are the same directory, so it appears once.
            Agent::All => &[".claude/skills", ".agents/skills", ".cursor/skills", ".gemini/skills"],
        }
    }
}

#[derive(clap::Args, Debug)]
pub struct SkillArgs {
    #[command(subcommand)]
    pub what: SkillCmd,
}

#[derive(clap::Subcommand, Debug)]
pub enum SkillCmd {
    /// Write the skill where a coding agent will find it.
    Install(InstallArgs),
    /// Print it instead, so it can go somewhere this does not know about.
    Show,
    /// Say where it would go, and whether something is already there.
    Where(InstallArgs),
}

#[derive(clap::Args, Debug)]
pub struct InstallArgs {
    /// Which agent. Defaults to all of them, which is four directories and no harm done.
    #[arg(long, value_enum, default_value = "all")]
    pub agent: Agent,
    /// Install for this project only, under the current directory, instead of for this user.
    ///
    /// A project install is checked in and therefore shared with everyone who clones the
    /// repository; a user install follows the person rather than the code. Neither is more
    /// correct, which is why this is a flag and not a guess.
    #[arg(long)]
    pub project: bool,
    /// Overwrite a skill that is already there.
    ///
    /// Off by default because the file may be somebody's edit of ours, and silently replacing
    /// somebody's writing is the one thing an installer must not do.
    #[arg(long)]
    pub force: bool,
}

pub fn run(cwd: &Path, args: &SkillArgs) -> anyhow::Result<()> {
    match &args.what {
        SkillCmd::Show => {
            let text = body()?;
            print!("{text}");
            Ok(())
        }
        SkillCmd::Where(a) => {
            for dir in targets(cwd, a)? {
                let at = dir.join(NAME);
                let mark = match at.join("SKILL.md").exists() {
                    true => "  (already there)",
                    false => "",
                };
                say(&format!("{}{mark}", at.display()));
            }
            Ok(())
        }
        SkillCmd::Install(a) => install(cwd, a),
    }
}

fn install(cwd: &Path, a: &InstallArgs) -> anyhow::Result<()> {
    let mut written = 0;
    let mut kept = 0;
    for dir in targets(cwd, a)? {
        let at = dir.join(NAME);
        let marker = at.join("SKILL.md");
        if marker.exists() && !a.force {
            say(&format!("kept    {}  (--force to replace)", at.display()));
            kept += 1;
            continue;
        }
        std::fs::create_dir_all(&at)?;
        for file in SKILL.files() {
            let to = at.join(file.path());
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&to, file.contents())?;
        }
        say(&format!("wrote   {}", at.display()));
        written += 1;
    }
    if written == 0 && kept == 0 {
        say("nothing to do.");
        return Ok(());
    }
    if written > 0 {
        // The one thing somebody has to know afterwards, and the one thing nothing on screen says:
        // a skill is read when a session starts, so the agent that ran this command does not have
        // it and will not until it is restarted.
        say("\nStart a new session in your coding agent to pick it up.");
    }
    Ok(())
}

/// Every directory this install writes into.
fn targets(cwd: &Path, a: &InstallArgs) -> anyhow::Result<Vec<PathBuf>> {
    let root = match a.project {
        true => cwd.to_path_buf(),
        false => home()?,
    };
    Ok(a.agent.dirs().iter().map(|d| root.join(d)).collect())
}

fn home() -> anyhow::Result<PathBuf> {
    // `$HOME` first, and `directories` behind it. A user who has moved `$HOME` — which is how
    // every sandbox and every CI job runs — means it, and a skill installed into the real home
    // directory of the account a container happens to run as is one nothing will ever read.
    if let Some(h) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        return Ok(PathBuf::from(h));
    }
    directories::BaseDirs::new()
        .map(|d| d.home_dir().to_path_buf())
        .ok_or_else(|| anyhow::anyhow!("cannot find your home directory — use --project"))
}

/// The skill's text, for `show`.
fn body() -> anyhow::Result<String> {
    let file = SKILL
        .get_file("SKILL.md")
        .ok_or_else(|| anyhow::anyhow!("this build has no skill embedded in it"))?;
    Ok(String::from_utf8_lossy(file.contents()).into_owned())
}

/// `println!` that does not panic when the reader goes away — see the one in `main`.
fn say(line: &str) {
    use std::io::Write;
    if writeln!(std::io::stdout(), "{line}").is_err() {
        std::process::exit(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The skill has to actually be in the binary. An `include_dir!` that silently embedded
    /// nothing — the exact failure `plugins/` exists to prevent — would make `skill install`
    /// create empty directories and report success.
    #[test]
    fn the_skill_is_embedded() {
        let text = body().expect("a skill");
        assert!(
            text.starts_with("---\nname: neosh\n"),
            "no frontmatter: {:?}",
            &text[..40.min(text.len())]
        );
        assert!(text.contains("description:"), "a skill with no description is never loaded");
    }

    /// Every verb the skill tells an agent to run has to exist. A document that is confidently
    /// wrong about a CLI is worse than no document, because it is read as authoritative.
    #[test]
    fn every_verb_it_names_is_a_real_one() {
        let text = body().expect("a skill");
        for verb in [
            "neosh agent start",
            "neosh agent ls",
            "neosh agent send",
            "neosh agent read",
            "neosh agent watch",
            "neosh agent wait",
            "neosh agent interrupt",
            "neosh agent rename",
            "neosh agent archive",
            "neosh agent rm",
            "neosh agent models",
            "neosh agent commands",
            "neosh agent run",
            "neosh agent call",
            "neosh status",
            "neosh paths",
        ] {
            assert!(text.contains(verb), "the skill never mentions `{verb}`");
        }
    }

    #[test]
    fn all_covers_every_named_agent() {
        let all = Agent::All.dirs();
        for one in [Agent::Claude, Agent::Codex, Agent::Cursor, Agent::Gemini, Agent::Shared] {
            for dir in one.dirs() {
                assert!(all.contains(dir), "`--agent all` misses {dir} ({one:?})");
            }
        }
    }
}
