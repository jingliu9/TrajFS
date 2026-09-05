//! Agent skill export (docs/PLAN.md §13). The template is embedded so the skill always matches the binary.

use crate::config::Config;
use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use std::path::PathBuf;

pub const TEMPLATE: &str = include_str!("../../../../skills/traj/SKILL.md");

#[derive(Args, Debug)]
pub struct SkillArgs {
    #[command(subcommand)]
    pub which: Which,
}

#[derive(Subcommand, Debug)]
pub enum Which {
    /// Render the skill into the repo (.claude/skills/traj/SKILL.md and an AGENTS.md section)
    Export {
        /// Directory to write into (default: the repo root holding trajfs.toml)
        #[arg(long)]
        out: Option<PathBuf>,
        /// Print the rendered Markdown instead of writing files
        #[arg(long)]
        stdout: bool,
    },
    /// List the verbs the skill must mention (for tests)
    Verbs,
}

pub const VERBS: &[&str] = &[
    "init", "doctor", "pack", "watch", "ls", "tree", "find", "du", "stat", "cat", "extract",
    "edit", "grep", "sql", "derive", "verify", "commit", "skill", "hook", "bench", "mount",
    "umount",
];

pub fn render(cfg: Option<&Config>) -> String {
    let (data, stores) = match cfg {
        Some(c) => (
            c.data_root().display().to_string(),
            c.store_root().display().to_string(),
        ),
        None => ("<data_root>".to_string(), "<store_root>".to_string()),
    };
    let body = TEMPLATE
        .replace("{{data_root}}", &data)
        .replace("{{store_root}}", &stores);
    format!("{body}\n<!-- traj-skill-version: {} -->\n", crate::VERSION)
}

pub fn version_in(text: &str) -> Option<String> {
    let i = text.find("<!-- traj-skill-version: ")?;
    let rest = &text[i + "<!-- traj-skill-version: ".len()..];
    let j = rest.find(" -->")?;
    Some(rest[..j].to_string())
}

pub fn export_all(cfg: &Config) -> Result<()> {
    write_to(&cfg.dir, Some(cfg))
}

fn write_to(dir: &std::path::Path, cfg: Option<&Config>) -> Result<()> {
    let text = render(cfg);
    let skill_dir = dir.join(".claude").join("skills").join("traj");
    std::fs::create_dir_all(&skill_dir)?;
    std::fs::write(skill_dir.join("SKILL.md"), &text)?;
    // AGENTS.md: replace or append a delimited section
    let agents = dir.join("AGENTS.md");
    let start = "<!-- traj-skill:start -->";
    let end = "<!-- traj-skill:end -->";
    let body_no_fm = strip_frontmatter(&text);
    let section = format!("{start}\n{body_no_fm}\n{end}\n");
    let existing = std::fs::read_to_string(&agents).unwrap_or_default();
    let new = match (existing.find(start), existing.find(end)) {
        (Some(s), Some(e)) if e > s => format!(
            "{}{}{}",
            &existing[..s],
            section,
            &existing[e + end.len()..].trim_start_matches('\n')
        ),
        _ => {
            if existing.is_empty() {
                format!("# Agent instructions\n\n{section}")
            } else {
                format!("{}\n\n{section}", existing.trim_end())
            }
        }
    };
    std::fs::write(&agents, new)?;
    Ok(())
}

fn strip_frontmatter(text: &str) -> String {
    if let Some(rest) = text.strip_prefix("---\n") {
        if let Some(i) = rest.find("\n---\n") {
            return rest[i + 5..].to_string();
        }
    }
    text.to_string()
}

pub fn run(a: SkillArgs) -> Result<i32> {
    match a.which {
        Which::Export { out, stdout } => {
            let cfg = Config::try_find()?;
            if stdout {
                print!("{}", render(cfg.as_ref()));
                return Ok(0);
            }
            let dir = match (out, &cfg) {
                (Some(d), _) => d,
                (None, Some(c)) => c.dir.clone(),
                (None, None) => anyhow::bail!("no trajfs.toml found; pass --out <dir>"),
            };
            write_to(&dir, cfg.as_ref())
                .with_context(|| format!("write skill under {}", dir.display()))?;
            println!(
                "wrote {}/.claude/skills/traj/SKILL.md and {}/AGENTS.md (version {})",
                dir.display(),
                dir.display(),
                crate::VERSION
            );
            Ok(0)
        }
        Which::Verbs => {
            for v in VERBS {
                println!("{v}");
            }
            Ok(0)
        }
    }
}
