use std::process::ExitCode;

use anyhow::{Result, bail};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};

use crate::backfill::{BackfillOptions, run};
use crate::scope::scope_failure;
use crate::session::RoleSet;

mod backfill;
mod export;
mod report;
mod scope;
mod server;
mod session;

#[derive(Parser)]
#[command(
    name = "recall-powercontext",
    version,
    about = "Backfill Recall sessions into PowerContext Content Sources"
)]
struct Cli {
    #[arg(long = "recall-extension-manifest", hide = true)]
    recall_extension_manifest: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    Backfill(BackfillArgs),
}

#[derive(Parser)]
struct BackfillArgs {
    #[arg(long, default_value = "http://127.0.0.1:8000")]
    server_url: String,
    #[arg(long)]
    token: Option<String>,
    #[arg(long, help = "Recall export window: today, 7d, week, 30d, month, or all")]
    time: Option<String>,
    #[arg(long, default_value = "user")]
    roles: String,
    #[arg(long)]
    stdin: bool,
    #[arg(long)]
    dry_run: bool,
    #[arg(long, value_enum, default_value = "text")]
    format: OutputFormat,
}

#[derive(Clone, Copy, ValueEnum)]
enum OutputFormat {
    Text,
    Json,
}

fn main() -> ExitCode {
    match try_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error:#}");
            if scope_failure(&error) { ExitCode::from(2) } else { ExitCode::FAILURE }
        }
    }
}

fn try_main() -> Result<()> {
    let cli = Cli::parse();
    if cli.recall_extension_manifest {
        println!("{}", manifest_json());
        return Ok(());
    }
    let Some(Command::Backfill(args)) = cli.command else {
        Cli::command().print_help()?;
        eprintln!();
        std::process::exit(2);
    };
    let format = args.format;
    let roles = RoleSet::parse(&args.roles)?;
    let token =
        args.token.or_else(|| std::env::var("POWERCONTEXT_TOKEN").ok().filter(|v| !v.is_empty()));
    let report = run(BackfillOptions {
        cwd: std::env::current_dir()?,
        time: args.time.filter(|value| !matches!(value.trim(), "" | "all")),
        server_url: args.server_url,
        token,
        roles,
        stdin: args.stdin,
        dry_run: args.dry_run,
    })?;
    let failed = report.totals.failed;
    match format {
        OutputFormat::Text => eprintln!("{report}"),
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&report)?),
    }
    if failed > 0 {
        let noun = if failed == 1 { "source" } else { "sources" };
        bail!("backfill failed for {failed} {noun}");
    }
    Ok(())
}

fn manifest_json() -> Value {
    json!({
        "name": "powercontext",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": 2,
        "min_recall": "0.4.0"
    })
}
