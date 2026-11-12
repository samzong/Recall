mod compact;
mod recall;
mod report;
mod settings;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::json;

#[derive(Parser)]
#[command(
    name = "recall-calibrate",
    version,
    about = "Calibrate agent settings from Recall history"
)]
struct Cli {
    #[arg(long = "recall-extension-manifest", hide = true)]
    recall_extension_manifest: bool,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    #[command(about = "Advise the context window at which each agent should compact")]
    Compact {
        #[arg(long, value_enum)]
        source: Option<Source>,
        #[arg(long, default_value = "7d")]
        time: String,
        #[arg(long, value_enum, default_value_t = Format::Text)]
        format: Format,
    },
}

#[derive(Clone, Copy, PartialEq, ValueEnum)]
enum Source {
    ClaudeCode,
    Codex,
}

#[derive(Clone, Copy, ValueEnum)]
enum Format {
    Text,
    Json,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.recall_extension_manifest {
        let manifest = json!({
            "name": "calibrate",
            "version": env!("CARGO_PKG_VERSION"),
            "protocol": 2,
            "min_recall": "0.7.1"
        });
        println!("{manifest}");
        return Ok(());
    }
    let Some(Commands::Compact { source, time, format }) = cli.command else {
        <Cli as clap::CommandFactory>::command().print_help()?;
        return Ok(());
    };
    let mut advice = vec![];
    let (sources, labels) = match source {
        Some(Source::ClaudeCode) => ("claude-code", "Claude Code"),
        Some(Source::Codex) => ("codex", "Codex"),
        None => ("claude-code,codex", "Claude Code and Codex"),
    };
    eprintln!("reading {labels} usage from Recall ({time})");
    let mut events = recall::export_usage(sources, &time)?;
    if source != Some(Source::Codex) {
        advice.extend(report::claude(&events.remove("claude-code").unwrap_or_default()));
    }
    if source != Some(Source::ClaudeCode) {
        advice.extend(report::codex(&events.remove("codex").unwrap_or_default()));
    }
    let report = report::Report { time, advice };
    match format {
        Format::Text => print!("{}", report::render_text(&report)),
        Format::Json => println!("{}", serde_json::to_string_pretty(&report)?),
    }
    Ok(())
}
