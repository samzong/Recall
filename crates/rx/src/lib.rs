mod args;
mod catalog;
mod claude_catalog;
mod completions;
mod config;
mod dsh;
mod host;
mod install;
mod kimi;
mod launch;
mod opencode;
mod pi;
mod pick;
mod provider;
mod providers;
mod update;

use std::ffi::OsString;

use anyhow::Result;

use args::{Command, LaunchRequest, argv0_harness, parse, rewrite_argv0};
pub use config::Paths;
use launch::{EnvLookup, plan};

pub(crate) const RELEASE_VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn run(raw_args: impl IntoIterator<Item = impl Into<OsString>>) -> Result<()> {
    run_with(raw_args.into_iter().map(Into::into).collect(), &Paths::user()?, &EnvLookup::real())
}

pub fn run_with(raw_args: Vec<OsString>, paths: &Paths, env: &EnvLookup) -> Result<()> {
    let command = if raw_args.first().and_then(|argv0| argv0_harness(argv0)).is_some() {
        parse(&rewrite_argv0(raw_args.clone()))?
    } else {
        parse(&raw_args)?
    };
    match command {
        Command::Help => {
            print!("{}", help_text());
            Ok(())
        }
        Command::Version => {
            println!("rx {RELEASE_VERSION}");
            Ok(())
        }
        Command::Providers(command) => providers::run(command, paths, env),
        Command::Update(command) => update::run(command),
        Command::Completions(command) => completions::run(command, paths, env),
        Command::Host { passthrough } => host::run(passthrough, env),
        Command::PickHarness { provider } => {
            let Some(harness) = pick::harness(env)? else {
                return Ok(());
            };
            launch_request(
                LaunchRequest { harness, provider, passthrough: Vec::new() },
                paths,
                env,
                &raw_args,
            )
        }
        Command::Launch(request) => launch_request(request, paths, env, &raw_args),
    }
}

fn launch_request(
    request: LaunchRequest,
    paths: &Paths,
    env: &EnvLookup,
    raw_args: &[OsString],
) -> Result<()> {
    // Updating is incidental to launching: a broken state file, an
    // unwritable config dir, or a failed install must not stop the harness.
    if let Err(error) = update::maybe_before_launch(paths, env, raw_args) {
        eprintln!("[rx] update skipped: {error:#}");
    }
    let program = install::ensure(request.harness, env)?;
    let mut plan = plan(&request, paths, env)?;
    plan.program = program;
    if let Some(note) = &plan.stderr_note {
        eprintln!("{note}");
    }
    launch::exec(&plan)
}

pub fn help_text() -> &'static str {
    "\
rx — launch agent harnesses through a configured AI provider

Usage:
  rx
  rx --provider <provider>
  rx --provider none <harness> [args...]
  rx <harness> [args...]
  rx --provider <provider> <harness> [args...]
  rx providers <list|login|logout|use>
  rx providers models update [provider]
  rx update [--yes]
  rx completions <bash|zsh|fish>
  rx host [-- native harness args...]

A TTY `rx` with no harness opens a picker. Scripts must pass a harness.

Environment:
  RX_NO_UPDATE=1     skip launch-time update checks
  RX_NO_INSTALL=1    skip offering to install a missing harness
  RX_NO_YOLO=1       skip injecting max-permission flags into harnesses
"
}

#[cfg(test)]
mod tests;
