pub(crate) mod models;

use anyhow::{Result, bail};

use crate::config::Paths;
use crate::launch::EnvLookup;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Subcommand {
    Help,
    Models,
}

pub(crate) fn parse(args: &[String]) -> Result<Subcommand> {
    match args.first().map(String::as_str) {
        None | Some("-h") | Some("--help") => Ok(Subcommand::Help),
        Some("models") => {
            if args.len() != 1 {
                bail!("usage: rx debug models [--gateway <openrouter|tokener>]");
            }
            Ok(Subcommand::Models)
        }
        Some(name) => bail!("unknown debug command: {name}\n\n{}", help_text()),
    }
}

pub(crate) fn run(
    subcommand: Subcommand,
    gateway: Option<String>,
    paths: &Paths,
    env: &EnvLookup,
) -> Result<()> {
    match subcommand {
        Subcommand::Help => {
            print!("{}", help_text());
            Ok(())
        }
        Subcommand::Models => models::run(gateway, paths, env),
    }
}

pub(crate) fn help_text() -> &'static str {
    "\
rx debug — developer diagnostics (unstable; not for everyday use)

Usage:
  rx debug models [--gateway <openrouter|tokener>]

Subcommands:
  models    probe gateway /models and /v1/models catalog endpoints

Options:
  --gateway <openrouter|tokener>   select gateway (same flag as launch commands)
  -h, --help                       show this help
"
}
