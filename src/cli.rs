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
    #[command(about = "Show indexed source and background job status")]
    Info,
    #[command(about = "Scan configured AI coding session sources")]
    Sync {
        #[arg(long, help = "Reprocess every session, even if unchanged")]
        force: bool,
        #[arg(short, long, help = "Show per-source scan progress and settings")]
        verbose: bool,
        #[arg(long, help = "Sync only this source (id or label, e.g. cursor or CUR)")]
        source: Option<String>,
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
    #[command(about = "Search indexed coding sessions")]
    Search {
        #[arg(help = "Search query text")]
        query: String,
        #[arg(long, help = "Filter by source id or label")]
        source: Option<String>,
        #[arg(long, help = "Filter by time range")]
        time: Option<String>,
        #[arg(long, help = "Filter by project directory, including child paths")]
        project: Option<String>,
        #[arg(long, help = "Filter by repository identity")]
        repo: Option<String>,
    },
    #[command(about = "Show token usage reports")]
    Usage {
        #[arg(long, help = "Output usage report as JSON")]
        json: bool,
        #[arg(long, help = "Filter by source id or label")]
        source: Option<String>,
        #[arg(long, help = "Filter by time range")]
        time: Option<String>,
    },
    #[command(about = "Export session records as JSON Lines")]
    Export {
        #[arg(long, help = "Filter by source id or label")]
        source: Option<String>,
        #[arg(long, help = "Filter by time range")]
        time: Option<String>,
        #[arg(long, help = "Filter by project directory, including child paths")]
        project: Option<String>,
        #[arg(long, help = "Filter by repository identity")]
        repo: Option<String>,
        #[arg(
            long,
            default_value_t = 0,
            help = "Maximum sessions to export; 0 means all (default)"
        )]
        limit: usize,
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
    #[command(about = "Operate on indexed sessions")]
    Session {
        #[command(subcommand)]
        command: session::SessionCommands,
    },
    #[command(about = "Generate shell completion script")]
    Completions {
        #[arg(help = "Target shell")]
        shell: Shell,
    },
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

pub fn run() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Info) => recall::info::run()?,
        Some(Commands::Sync { force, verbose, source }) => {
            recall::sync::run_cli(force, verbose, source.as_deref())?
        }
        Some(Commands::BackgroundWorker { sync_first }) => {
            recall::sync::run_background_worker(sync_first)?
        }
        Some(Commands::BenchSemantic) => recall::bench::run_semantic()?,
        Some(Commands::BenchSearch { query }) => recall::bench::run_search(&query)?,
        Some(Commands::BenchEval { dataset, verbose }) => {
            recall::bench::run_eval(dataset.as_deref(), verbose)?
        }
        Some(Commands::BenchDumpSessions) => recall::bench::dump_sessions()?,
        Some(Commands::Search { query, source, time, project, repo }) => recall::query::run_search(
            &query,
            source.as_deref(),
            time.as_deref(),
            project.as_deref(),
            repo.as_deref(),
        )?,
        Some(Commands::Usage { json, source, time }) => {
            recall::usage::run_cli(json, source.as_deref(), time.as_deref())?
        }
        Some(Commands::Export { source, time, project, repo, limit }) => recall::export::run_cli(
            source.as_deref(),
            time.as_deref(),
            project.as_deref(),
            repo.as_deref(),
            limit,
        )?,
        Some(Commands::Import { file, dry_run }) => recall::import::run_cli(&file, dry_run)?,
        Some(Commands::Share { command: ShareCommands::Init { project_name, publish_dir } }) => {
            recall::share_init::run(project_name, publish_dir)?
        }
        Some(Commands::Skill {
            command: SkillCommands::Install { scope, agents, dry_run, yes },
        }) => run_skill_install(scope, agents, dry_run, yes)?,
        Some(Commands::Session { command }) => session::cmd_session(command)?,
        Some(Commands::Completions { shell }) => {
            generate(shell, &mut Cli::command(), "recall", &mut std::io::stdout());
        }
        None => recall::tui::runner::run(None)?,
    }

    Ok(())
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
mod tests {
    use super::{Cli, Commands, ShareCommands, Shell, SkillCommands, generate};
    use crate::session;
    use clap::{CommandFactory, Parser};
    use recall::adapters::{
        adapter_supports_usage_dashboard, all_adapters, source_supports_event_backfill,
    };

    #[test]
    fn export_accepts_default_jsonl_without_format_flag() {
        let cli = Cli::try_parse_from(["recall", "export", "--source", "grok"]).unwrap();
        match cli.command {
            Some(Commands::Export { source, .. }) => {
                assert_eq!(source.as_deref(), Some("grok"));
            }
            _ => panic!("expected export command"),
        }
    }

    #[test]
    fn export_rejects_removed_jsonl_flag() {
        assert!(Cli::try_parse_from(["recall", "export", "--jsonl"]).is_err());
    }

    #[test]
    fn share_init_accepts_project_and_publish_dir() {
        let cli = Cli::try_parse_from([
            "recall",
            "share",
            "init",
            "--project-name",
            "recall-share",
            "--publish-dir",
            "/tmp/recall-share",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Share {
                command: ShareCommands::Init { project_name, publish_dir },
            }) => {
                assert_eq!(project_name.as_deref(), Some("recall-share"));
                assert_eq!(publish_dir.unwrap().to_string_lossy(), "/tmp/recall-share");
            }
            _ => panic!("expected share init command"),
        }
    }

    #[test]
    fn session_share_accepts_tldr_file() {
        let cli = Cli::try_parse_from([
            "recall",
            "session",
            "share",
            "--id",
            "session-1",
            "--tldr-file",
            "/tmp/recall-tldr.md",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Session {
                command: session::SessionCommands::Share { id, tldr_file, .. },
            }) => {
                assert_eq!(id.as_deref(), Some("session-1"));
                assert_eq!(tldr_file.unwrap().to_string_lossy(), "/tmp/recall-tldr.md");
            }
            _ => panic!("expected session share command"),
        }
    }

    #[test]
    fn top_level_help_describes_public_commands() {
        let mut command = Cli::command();
        let help = command.render_long_help().to_string();
        let compact_help = help.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(!help.contains("--jsonl"));
        assert!(compact_help.contains("info Show indexed source and background job status"));
        assert!(compact_help.contains("sync Scan configured AI coding session sources"));
        assert!(compact_help.contains("search Search indexed coding sessions"));
        assert!(compact_help.contains("usage Show token usage reports"));
        assert!(compact_help.contains("export Export session records as JSON Lines"));
        assert!(compact_help.contains("import Import session records from JSON Lines"));
        assert!(compact_help.contains("share Share session pages"));
        assert!(compact_help.contains("skill Manage bundled Agent Skill"));
        assert!(compact_help.contains("session Operate on indexed sessions"));
        assert!(compact_help.contains("completions Generate shell completion script"));
    }

    #[test]
    fn completions_generates_zsh_script() {
        assert!(matches!(
            Cli::try_parse_from(["recall", "completions", "zsh"]).unwrap().command,
            Some(Commands::Completions { shell: Shell::Zsh })
        ));

        let mut output = Vec::new();
        generate(Shell::Zsh, &mut Cli::command(), "recall", &mut output);
        let script = String::from_utf8(output).unwrap();
        assert!(script.contains("#compdef recall"));
        assert!(script.contains("search"));
    }

    #[test]
    fn public_subcommand_help_describes_arguments_and_options() {
        for subcommand in ["search", "usage", "export", "import"] {
            let mut command = Cli::command();
            let command = command.find_subcommand_mut(subcommand).unwrap();
            let help = command.render_long_help().to_string();
            assert!(!help.contains("<SOURCE>    "), "{subcommand} source help missing");
            assert!(!help.contains("<TIME>        "), "{subcommand} time help missing");
            assert!(!help.contains("<QUERY>  "), "{subcommand} query help missing");
            assert!(!help.contains("<FILE>  \n"), "{subcommand} file help missing");
        }
    }

    #[test]
    fn skill_install_accepts_kitup_flags() {
        let cli = Cli::try_parse_from([
            "recall",
            "skill",
            "install",
            "--scope",
            "project",
            "--agent",
            "codex,claude-code",
            "--dry-run",
            "--yes",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Skill {
                command: SkillCommands::Install { scope, agents, dry_run, yes },
            }) => {
                assert_eq!(scope.as_deref(), Some("project"));
                assert_eq!(agents, ["codex,claude-code"]);
                assert!(dry_run);
                assert!(yes);
            }
            _ => panic!("expected skill install command"),
        }
    }

    #[test]
    fn dashboard_sync_skips_sources_without_usage_or_events() {
        for adapter in all_adapters() {
            let id = adapter.id();
            if matches!(id, "cline" | "antigravity-cli" | "kiro-cli") {
                assert!(
                    !adapter_supports_usage_dashboard(adapter.as_ref(), true),
                    "{id} should be skipped during dashboard sync"
                );
            }
            if source_supports_event_backfill(id) {
                assert!(adapter_supports_usage_dashboard(adapter.as_ref(), true));
            }
        }
    }
}
