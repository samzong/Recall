mod args;
mod catalog;
mod claude_catalog;
mod config;
mod debug;
mod launch;
mod opencode;
mod pi;
mod update;

use anyhow::Result;

use args::{Command, parse, rewrite_argv0};
pub use config::Paths;
use launch::{EnvLookup, plan};

pub fn run(raw_args: impl IntoIterator<Item = String>) -> Result<()> {
    run_with(raw_args.into_iter().collect(), &Paths::user()?, &EnvLookup::real())
}

pub fn run_with(raw_args: Vec<String>, paths: &Paths, env: &EnvLookup) -> Result<()> {
    let args = rewrite_argv0(raw_args.clone());
    let command = parse(&args)?;
    if matches!(command, Command::Launch(_)) {
        update::maybe_before_launch(paths, env, &raw_args)?;
    }
    match command {
        Command::Help => {
            print!("{}", help_text());
            Ok(())
        }
        Command::Version => {
            println!("rx {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Command::Config(command) => config::run(command, paths),
        Command::Debug { subcommand, gateway } => debug::run(subcommand, gateway, paths, env),
        Command::Update { yes } => update::run(yes, paths),
        Command::Launch(request) => {
            let plan = plan(&request, paths, env)?;
            if let Some(note) = &plan.stderr_note {
                eprintln!("{note}");
            }
            launch::exec(&plan)
        }
    }
}

pub fn help_text() -> &'static str {
    "\
rx — launch agent harnesses through a configured API gateway

Usage:
  rx <harness> [args...]
  rx config set gateway <openrouter|tokener>
  rx config set key <openrouter|tokener> <key>
  rx config get [name]
  rx update [--yes]

Options:
  --gateway <openrouter|tokener>   select gateway for this launch
  -h, --help                       show this help
  -V, --version                    show version

Environment:
  RX_NO_UPDATE=1                   skip launch-time update checks

Developer: rx debug --help

With no gateway configured, rx execs the harness unchanged.
Gateway flags are stripped; every other argument is passed through.
"
}

#[cfg(test)]
mod tests;
