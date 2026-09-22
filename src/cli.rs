pub(crate) mod session_args;

use std::ffi::{OsStr, OsString};
use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::{Shell, generate};

use crate::session;

#[derive(Parser)]
#[command(name = "recall", version, about = "Search and recall AI coding sessions")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    #[command(about = "Configure and synchronize a remote session library")]
    Remote {
        #[command(subcommand)]
        command: crate::remote::RemoteCommands,
    },
    #[command(about = "Search indexed coding sessions")]
    Search {
        #[arg(help = "Search query text")]
        query: String,
        #[arg(long, help = "Filter by source id or label")]
        source: Option<String>,
        #[arg(long, value_parser = crate::query::parse_time_range_arg, help = "Filter by time range")]
        time: Option<String>,
        #[arg(long, help = "Filter by project directory, including child paths")]
        project: Option<String>,
        #[arg(long, help = "Filter by repository identity")]
        repo: Option<String>,
        #[arg(long, value_enum, default_value_t = crate::query::SearchFormat::Text)]
        format: crate::query::SearchFormat,
        #[arg(long, help = "Return keyword matches with message sequence anchors")]
        messages: bool,
        #[arg(long, requires = "messages", help = "Search within one Recall session")]
        session_id: Option<String>,
        #[arg(long, requires = "messages", value_parser = clap::value_parser!(u32).range(1..=50), help = "Maximum message matches (default 10, maximum 50)")]
        limit: Option<u32>,
    },
    #[command(about = "Operate on indexed sessions")]
    Session {
        #[command(subcommand)]
        command: session::SessionCommands,
    },
    #[command(about = "Scan configured AI coding session sources")]
    Sync {
        #[arg(
            long,
            conflicts_with_all = ["backfill_events", "dry_run", "force", "project"],
            requires = "source",
            help = "Sync only this native session id"
        )]
        session: Option<String>,
        #[arg(
            long,
            value_enum,
            requires = "session",
            default_value_t = crate::sync::SessionSyncFormat::Text,
            help = "Output format for --session"
        )]
        format: crate::sync::SessionSyncFormat,
        #[arg(
            long,
            help = "Backfill native file evidence without rebuilding existing discussions"
        )]
        backfill_events: bool,
        #[arg(
            long,
            requires = "backfill_events",
            help = "Preview event backfill without changing the index"
        )]
        dry_run: bool,
        #[arg(long, help = "Reprocess every session, even if unchanged")]
        force: bool,
        #[arg(short, long, help = "Show per-source scan progress and settings")]
        verbose: bool,
        #[arg(long, help = "Sync only this source (id or label, e.g. cursor or CUR)")]
        source: Option<String>,
        #[arg(
            long,
            help = "Scope selector: a path, owner/repo, a remote URL, or all; defaults to the current directory's project"
        )]
        project: Option<String>,
    },
    #[command(about = "Show indexed source and background job status")]
    Info {
        #[arg(long, value_enum, default_value_t = crate::info::InfoFormat::Text)]
        format: crate::info::InfoFormat,
    },
    #[command(about = "Show token usage reports")]
    Usage {
        #[arg(long, help = "Output usage report as JSON", conflicts_with = "card")]
        json: bool,
        #[arg(long, help = "Filter by source id or label", conflicts_with = "card")]
        source: Option<String>,
        #[arg(
            long,
            value_parser = crate::query::parse_time_range_arg,
            help = "Filter by time range",
            conflicts_with = "card"
        )]
        time: Option<String>,
        #[arg(long, help = "Print a shareable usage stats card")]
        card: bool,
        #[arg(
            long,
            value_enum,
            default_value_t = crate::wrapped::WrappedPeriod::Week,
            requires = "card",
            help = "Card time window"
        )]
        period: crate::wrapped::WrappedPeriod,
    },
    #[command(about = "Export session records as JSON Lines")]
    Export {
        #[arg(long, help = "Filter by source id or label")]
        source: Option<String>,
        #[arg(long, value_parser = crate::query::parse_time_range_arg, help = "Filter by time range")]
        time: Option<String>,
        #[arg(long, help = "Filter by project directory, including child paths")]
        project: Option<String>,
        #[arg(long, help = "Filter by repository identity")]
        repo: Option<String>,
        #[arg(long, value_enum, help = "Filter by topology thread role")]
        thread_role: Option<crate::db::search::ThreadRoleFilter>,
        #[arg(
            long,
            default_value_t = 0,
            help = "Maximum sessions to export; 0 means all (default)"
        )]
        limit: usize,
        #[arg(
            long,
            help = "Comma-separated JSONL fields; messages is required: metadata,messages,usage,events"
        )]
        include: Option<String>,
    },
    #[command(about = "Import session records from JSON Lines")]
    Import {
        #[arg(help = "Input file path, or - for stdin")]
        file: String,
        #[arg(long, help = "Parse and report without writing")]
        dry_run: bool,
    },
    #[command(about = "Share session pages")]
    Share {
        #[command(subcommand)]
        command: ShareCommands,
    },
    #[command(about = "Manage bundled Agent Skill")]
    Skill {
        #[command(subcommand)]
        command: SkillCommands,
    },
    #[command(about = "Manage Recall extensions", visible_alias = "ext")]
    Extension {
        #[command(subcommand)]
        command: ExtensionCommands,
    },
    #[command(about = "Generate shell completion script")]
    Completions {
        #[arg(help = "Target shell")]
        shell: Shell,
    },
    #[command(about = "Serve and inspect the read-only MCP server")]
    Mcp {
        #[arg(long, help = "Read this Recall database instead of the default index")]
        db: Option<PathBuf>,
        #[command(subcommand)]
        command: Option<McpCommands>,
    },
    #[command(hide = true, name = "__background-worker")]
    BackgroundWorker {
        #[arg(long)]
        sync_first: bool,
    },
    #[command(hide = true, name = "__bench-semantic")]
    BenchSemantic,
    #[command(hide = true, name = "__bench-search")]
    BenchSearch { query: String },
    #[command(hide = true, name = "__bench-eval")]
    BenchEval {
        #[arg(long)]
        dataset: Option<String>,
        #[arg(short, long)]
        verbose: bool,
    },
    #[command(hide = true, name = "__bench-dump-sessions")]
    BenchDumpSessions,
    #[command(external_subcommand)]
    External(Vec<OsString>),
}

#[derive(Subcommand)]
enum ShareCommands {
    #[command(about = "Initialize Cloudflare Pages sharing")]
    Init {
        #[arg(long, help = "Cloudflare Pages project name")]
        project_name: Option<String>,
        #[arg(long, help = "Local directory used for generated share pages")]
        publish_dir: Option<PathBuf>,
    },
    #[command(about = "List published share pages")]
    List {
        #[arg(long, value_enum, default_value_t = crate::share::ShareFormat::Text)]
        format: crate::share::ShareFormat,
    },
    #[command(about = "Remove a published share page and redeploy", visible_alias = "rm")]
    Unpublish {
        #[arg(help = "Share id from `recall share list`, or the published URL")]
        id: String,
        #[arg(long, help = "Show the target page without deleting or deploying")]
        dry_run: bool,
        #[arg(long, help = "Unpublish without confirmation")]
        yes: bool,
        #[arg(long, value_enum, default_value_t = crate::share::ShareFormat::Text)]
        format: crate::share::ShareFormat,
    },
}

const MCP_AGENT_HELP: &str = "Target host: claude, codex, cursor-agent, agy, or grok. Repeat for multiple hosts. Use '*' for all.";

#[derive(Subcommand)]
enum McpCommands {
    #[command(about = "Print the Recall MCP server capabilities and tools")]
    Capabilities {
        #[arg(long, value_enum, default_value_t = crate::mcp::McpCapabilitiesFormat::Text)]
        format: crate::mcp::McpCapabilitiesFormat,
    },
    #[command(about = "Register the Recall MCP server with local agent hosts")]
    Install {
        #[arg(
            long = "agent",
            help = MCP_AGENT_HELP
        )]
        agents: Vec<String>,
        #[arg(long, help = "Print host commands without running them")]
        dry_run: bool,
        #[arg(long, help = "Recall binary to register instead of PATH `recall`")]
        bin: Option<PathBuf>,
    },
    #[command(about = "Unregister the Recall MCP server from local agent hosts")]
    Uninstall {
        #[arg(
            long = "agent",
            help = MCP_AGENT_HELP
        )]
        agents: Vec<String>,
        #[arg(long, help = "Print host commands without running them")]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum SkillCommands {
    #[command(about = "Install Recall bundled Agent Skill")]
    Install {
        #[arg(long, help = "Install scope: user or project")]
        scope: Option<String>,
        #[arg(
            long = "agent",
            help = "Target agent id. Repeat for multiple agents. Use '*' for all."
        )]
        agents: Vec<String>,
        #[arg(long, help = "Show install plan without writing")]
        dry_run: bool,
        #[arg(short, long, help = "Skip prompts and accept policy-selected targets")]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum ExtensionCommands {
    #[command(about = "List installed official extensions")]
    List {
        #[arg(long, help = "List official extensions available to install")]
        available: bool,
    },
    #[command(about = "Install an official extension")]
    Install {
        #[arg(help = "Extension name")]
        name: String,
    },
    #[command(about = "Remove an installed official extension")]
    Remove {
        #[arg(help = "Extension name")]
        name: String,
    },
    #[command(about = "Upgrade installed official extensions")]
    Upgrade {
        #[arg(help = "Extension name; upgrades all installed extensions when omitted")]
        name: Option<String>,
    },
}

pub(crate) fn run() -> Result<()> {
    if print_root_help_if_requested()? {
        return Ok(());
    }

    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Info { format }) => crate::info::run(format)?,
        Some(Commands::Sync { session: Some(session), source, format, .. }) => {
            crate::sync::run_single_session(source.as_deref(), &session, format)?;
        }
        Some(Commands::Sync {
            force, verbose, source, project, backfill_events, dry_run, ..
        }) => {
            crate::sync::run_cli(
                force,
                verbose,
                source.as_deref(),
                project.as_deref(),
                backfill_events,
                dry_run,
            )?;
        }
        Some(Commands::BackgroundWorker { sync_first }) => {
            crate::sync::run_background_worker(sync_first)?
        }
        Some(Commands::BenchSemantic) => crate::bench::run_semantic()?,
        Some(Commands::BenchSearch { query }) => crate::bench::run_search(&query)?,
        Some(Commands::BenchEval { dataset, verbose }) => {
            crate::bench::run_eval(dataset.as_deref(), verbose)?
        }
        Some(Commands::BenchDumpSessions) => crate::bench::dump_sessions()?,
        Some(Commands::Search {
            query,
            source,
            time,
            project,
            repo,
            format,
            messages,
            session_id,
            limit,
        }) => {
            if messages {
                crate::query::run_message_search(
                    &query,
                    source.as_deref(),
                    time.as_deref(),
                    project.as_deref(),
                    repo.as_deref(),
                    session_id.as_deref(),
                    limit.unwrap_or(10) as usize,
                    format,
                )?;
            } else {
                crate::query::run_search(
                    &query,
                    source.as_deref(),
                    time.as_deref(),
                    project.as_deref(),
                    repo.as_deref(),
                    format,
                )?
            }
        }
        Some(Commands::Usage { json, source, time, card, period }) => {
            crate::usage::run_cli(json, source.as_deref(), time.as_deref(), card, period)?
        }
        Some(Commands::Export { source, time, project, repo, thread_role, limit, include }) => {
            crate::export::run_cli(
                source.as_deref(),
                time.as_deref(),
                project.as_deref(),
                repo.as_deref(),
                thread_role,
                limit,
                include.as_deref(),
            )?
        }
        Some(Commands::Import { file, dry_run }) => crate::import::run_cli(&file, dry_run)?,
        Some(Commands::Share { command }) => match command {
            ShareCommands::Init { project_name, publish_dir } => {
                crate::share_init::run(project_name, publish_dir)?
            }
            ShareCommands::List { format } => crate::share::run_list(format)?,
            ShareCommands::Unpublish { id, dry_run, yes, format } => {
                crate::share::run_unpublish(&id, dry_run, yes, format)?
            }
        },
        Some(Commands::Skill {
            command: SkillCommands::Install { scope, agents, dry_run, yes },
        }) => run_skill_install(scope, agents, dry_run, yes)?,
        Some(Commands::Extension { command: ExtensionCommands::List { available } }) => {
            crate::extension::run_list(available)?
        }
        Some(Commands::Extension { command: ExtensionCommands::Install { name } }) => {
            crate::extension::run_install(&name)?
        }
        Some(Commands::Extension { command: ExtensionCommands::Remove { name } }) => {
            crate::extension::run_remove(&name)?
        }
        Some(Commands::Extension { command: ExtensionCommands::Upgrade { name } }) => {
            crate::extension::run_upgrade(name)?
        }
        Some(Commands::Session { command }) => session::cmd_session(command)?,
        Some(Commands::Remote { command }) => crate::remote::run(command)?,
        Some(Commands::Completions { shell }) => {
            generate(shell, &mut Cli::command(), "recall", &mut std::io::stdout());
        }
        Some(Commands::Mcp { db: Some(_), command: Some(_) }) => {
            anyhow::bail!("--db is only valid when serving (`recall mcp`)");
        }
        Some(Commands::Mcp { db, command: None }) => crate::mcp::run(db)?,
        Some(Commands::Mcp { command: Some(McpCommands::Capabilities { format }), .. }) => {
            crate::mcp::run_capabilities(format)?
        }
        Some(Commands::Mcp {
            command: Some(McpCommands::Install { agents, dry_run, bin }),
            ..
        }) => crate::mcp_host::install(&agents, dry_run, bin)?,
        Some(Commands::Mcp {
            command: Some(McpCommands::Uninstall { agents, dry_run }), ..
        }) => crate::mcp_host::uninstall(&agents, dry_run)?,
        Some(Commands::External(args)) => {
            let status = crate::extension::run_external(args)?;
            if !status.success() {
                std::process::exit(status.code().unwrap_or(1));
            }
        }
        None => crate::tui::runner::run(None)?,
    }

    Ok(())
}

fn print_root_help_if_requested() -> Result<bool> {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    if args.len() != 1 {
        return Ok(false);
    }
    let arg = args[0].as_os_str();
    if arg != OsStr::new("--help") && arg != OsStr::new("-h") && arg != OsStr::new("help") {
        return Ok(false);
    }

    print!("{}", render_root_help());
    Ok(true)
}

fn render_root_help() -> String {
    let mut command = Cli::command();
    let mut help = command.render_long_help().to_string();
    insert_installed_help(&mut help, &crate::extension::installed_help());
    help
}

fn insert_installed_help(help: &mut String, installed: &str) {
    if let Some(index) = help.find("\nOptions:") {
        help.insert_str(index, installed);
    } else {
        help.push_str(installed);
    }
}

fn run_skill_install(
    scope: Option<String>,
    agents: Vec<String>,
    dry_run: bool,
    yes: bool,
) -> Result<()> {
    let flags = kitup::parse_install_flags(kitup::InstallFlagValues {
        scope,
        scope_set: false,
        agents,
        yes,
        dry_run,
    });
    kitup::install_flag_error(&flags.errors)?;

    let report = kitup::run_bundled_skill_install(&kitup::InstallWorkflowOptions {
        install: kitup::InstallOptions {
            base: kitup::BaseOptions::default(),
            app_id: "recall".to_string(),
            skill_bundle: recall_skill_bundle(),
            scope: flags.scope,
            agents: flags.agents,
        },
        yes: flags.yes,
        dry_run: flags.dry_run,
        stdin_tty: std::io::stdin().is_terminal(),
        current_agent: None,
        default_scope: Some(kitup::Scope::User),
        scope_set: flags.scope_set,
        prompt_scope: true,
    })?;
    kitup::install_workflow_error(&report)?;
    Ok(())
}

fn recall_skill_bundle() -> kitup::SkillBundle {
    kitup::files_bundle(vec![
        kitup::SkillFile {
            path: "SKILL.md".to_string(),
            contents: include_bytes!("../skills/recall/SKILL.md").to_vec(),
            mode: None,
        },
        kitup::SkillFile {
            path: "agents/openai.yaml".to_string(),
            contents: include_bytes!("../skills/recall/agents/openai.yaml").to_vec(),
            mode: None,
        },
    ])
}

#[cfg(test)]
mod tests;
