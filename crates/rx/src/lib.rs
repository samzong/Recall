mod args;
mod catalog;
mod claude_catalog;
mod config;
mod debug;
mod install;
mod launch;
mod opencode;
mod pi;
mod update;

use anyhow::Result;

use args::{Command, argv0_harness, parse, rewrite_argv0};
pub use config::Paths;
use launch::{EnvLookup, plan};

pub fn run(raw_args: impl IntoIterator<Item = String>) -> Result<()> {
    run_with(raw_args.into_iter().collect(), &Paths::user()?, &EnvLookup::real())
}

pub fn run_with(raw_args: Vec<String>, paths: &Paths, env: &EnvLookup) -> Result<()> {
    let command = if raw_args.first().and_then(|argv0| argv0_harness(argv0)).is_some() {
        parse(&rewrite_argv0(raw_args.clone()))?
    } else {
        parse(&raw_args)?
    };
    if matches!(command, Command::Launch(_)) {
        // Updating is incidental to launching: a broken state file, an
        // unwritable config dir, or a failed install must not stop the harness.
        if let Err(error) = update::maybe_before_launch(paths, env, &raw_args) {
            eprintln!("[rx] update skipped: {error:#}");
        }
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
            let program = install::ensure(request.harness, env)?;
            let mut plan = plan(&request, paths, env)?;
            plan.program = program.to_string_lossy().into_owned();
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
  rx --gateway <profile> <harness> [args...]
  rx config set gateway <profile>
  rx config set key <profile> <key>
  rx config get [name]
  rx update [--yes]
  rx debug --help

Environment:
  RX_NO_UPDATE=1     skip launch-time update checks
  RX_NO_INSTALL=1    skip offering to install a missing harness
"
}

#[cfg(test)]
mod tests;
