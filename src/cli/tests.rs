use super::{
    Cli, Commands, ExtensionCommands, McpCommands, ShareCommands, Shell, SkillCommands, generate,
    insert_installed_help,
};
use crate::session;
use clap::{CommandFactory, Parser};

#[test]
fn export_accepts_default_jsonl_without_format_flag() {
    let cli = Cli::try_parse_from([
        "recall",
        "export",
        "--source",
        "grok",
        "--include",
        "metadata,messages",
    ])
    .unwrap();
    match cli.command {
        Some(Commands::Export { source, include, .. }) => {
            assert_eq!(source.as_deref(), Some("grok"));
            assert_eq!(include.as_deref(), Some("metadata,messages"));
        }
        _ => panic!("expected export command"),
    }
}

#[test]
fn export_rejects_removed_jsonl_flag() {
    assert!(Cli::try_parse_from(["recall", "export", "--jsonl"]).is_err());
}

#[test]
fn time_filter_validates_values_for_every_command() {
    let commands: &[&[&str]] = &[
        &["recall", "search", "query", "--time", "definitely-invalid"],
        &["recall", "session", "list", "--time", "definitely-invalid"],
        &["recall", "usage", "--time", "definitely-invalid"],
        &["recall", "export", "--time", "definitely-invalid"],
    ];

    for args in commands {
        let Err(error) = Cli::try_parse_from(*args) else {
            panic!("accepted invalid --time for {}", args[1]);
        };
        assert_eq!(error.exit_code(), 2);
    }

    for value in ["today", "7d", "week", "30d", "month", "all", "WEEK"] {
        assert!(Cli::try_parse_from(["recall", "usage", "--time", value]).is_ok());
    }
}

#[test]
fn usage_card_owns_period_and_excludes_report_flags() {
    let cli = Cli::try_parse_from(["recall", "usage", "--card", "--period", "year"]).unwrap();
    match cli.command {
        Some(Commands::Usage { card, period, json, .. }) => {
            assert!(card);
            assert!(!json);
            assert_eq!(period, crate::wrapped::WrappedPeriod::Year);
        }
        _ => panic!("expected usage command"),
    }

    assert!(Cli::try_parse_from(["recall", "usage", "--period", "year"]).is_err());
    assert!(Cli::try_parse_from(["recall", "usage", "--card", "--time", "7d"]).is_err());
    assert!(Cli::try_parse_from(["recall", "usage", "--card", "--json"]).is_err());
    assert!(Cli::try_parse_from(["recall", "usage", "--card", "--source", "codex"]).is_err());
}

#[test]
fn info_accepts_json_format() {
    let cli = Cli::try_parse_from(["recall", "info", "--format", "json"]).unwrap();
    match cli.command {
        Some(Commands::Info { format }) => {
            assert_eq!(format, crate::info::InfoFormat::Json);
        }
        _ => panic!("expected info command"),
    }
}

#[test]
fn search_accepts_json_format() {
    let cli = Cli::try_parse_from(["recall", "search", "extension", "--format", "json"]).unwrap();
    match cli.command {
        Some(Commands::Search { query, format, .. }) => {
            assert_eq!(query, "extension");
            assert_eq!(format, crate::query::SearchFormat::Json);
        }
        _ => panic!("expected search command"),
    }
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
        Some(Commands::Share { command: ShareCommands::Init { project_name, publish_dir } }) => {
            assert_eq!(project_name.as_deref(), Some("recall-share"));
            assert_eq!(publish_dir.unwrap().to_string_lossy(), "/tmp/recall-share");
        }
        _ => panic!("expected share init command"),
    }
}

#[test]
fn share_list_and_unpublish_parse() {
    let list = Cli::try_parse_from(["recall", "share", "list", "--format", "json"]).unwrap();
    match list.command {
        Some(Commands::Share { command: ShareCommands::List { format } }) => {
            assert_eq!(format, crate::share::ShareFormat::Json);
        }
        _ => panic!("expected share list command"),
    }

    let unpublish = Cli::try_parse_from([
        "recall",
        "share",
        "unpublish",
        "https://recall-share-test.pages.dev/abc-123",
        "--dry-run",
        "--yes",
        "--format",
        "json",
    ])
    .unwrap();
    match unpublish.command {
        Some(Commands::Share {
            command: ShareCommands::Unpublish { id, dry_run, yes, format },
        }) => {
            assert_eq!(id, "https://recall-share-test.pages.dev/abc-123");
            assert!(dry_run);
            assert!(yes);
            assert_eq!(format, crate::share::ShareFormat::Json);
        }
        _ => panic!("expected share unpublish command"),
    }

    let alias = Cli::try_parse_from(["recall", "share", "rm", "abc-123"]).unwrap();
    match alias.command {
        Some(Commands::Share { command: ShareCommands::Unpublish { id, dry_run, yes, .. } }) => {
            assert_eq!(id, "abc-123");
            assert!(!dry_run);
            assert!(!yes);
        }
        _ => panic!("expected share rm alias"),
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
            command: session::SessionCommands::Share { selector, tldr_file, .. },
        }) => {
            assert_eq!(selector.id.as_deref(), Some("session-1"));
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
    assert!(compact_help.contains("extension Manage Recall extensions"));
    assert!(compact_help.contains("session Operate on indexed sessions"));
    assert!(compact_help.contains("completions Generate shell completion script"));
    assert!(compact_help.contains("mcp Serve and inspect the read-only MCP server"));
}

#[test]
fn mcp_parses_optional_db_path() {
    let cli = Cli::try_parse_from(["recall", "mcp"]).unwrap();
    match cli.command {
        Some(Commands::Mcp { db, command }) => {
            assert!(db.is_none());
            assert!(command.is_none());
        }
        _ => panic!("expected mcp command"),
    }

    let cli = Cli::try_parse_from(["recall", "mcp", "--db", "/tmp/recall-fixture.db"]).unwrap();
    match cli.command {
        Some(Commands::Mcp { db, command }) => {
            assert_eq!(db.unwrap().to_string_lossy(), "/tmp/recall-fixture.db");
            assert!(command.is_none());
        }
        _ => panic!("expected mcp command"),
    }
}

#[test]
fn mcp_install_parses_host_flags() {
    let cli = Cli::try_parse_from(["recall", "mcp", "install"]).unwrap();
    match cli.command {
        Some(Commands::Mcp {
            db,
            command: Some(McpCommands::Install { agents, dry_run, bin }),
        }) => {
            assert!(db.is_none());
            assert!(agents.is_empty());
            assert!(!dry_run);
            assert!(bin.is_none());
        }
        _ => panic!("expected mcp install command"),
    }

    let cli = Cli::try_parse_from([
        "recall",
        "mcp",
        "install",
        "--agent",
        "claude",
        "--agent",
        "codex",
        "--dry-run",
        "--bin",
        "/tmp/recall",
    ])
    .unwrap();
    match cli.command {
        Some(Commands::Mcp {
            command: Some(McpCommands::Install { agents, dry_run, bin }),
            ..
        }) => {
            assert_eq!(agents, ["claude", "codex"]);
            assert!(dry_run);
            assert_eq!(bin.unwrap().to_string_lossy(), "/tmp/recall");
        }
        _ => panic!("expected mcp install command"),
    }
}

#[test]
fn mcp_uninstall_parses() {
    let cli = Cli::try_parse_from(["recall", "mcp", "uninstall", "--agent", "claude"]).unwrap();
    match cli.command {
        Some(Commands::Mcp {
            command: Some(McpCommands::Uninstall { agents, dry_run }), ..
        }) => {
            assert_eq!(agents, ["claude"]);
            assert!(!dry_run);
        }
        _ => panic!("expected mcp uninstall command"),
    }
}

#[test]
fn mcp_install_rejects_db_flag() {
    assert!(Cli::try_parse_from(["recall", "mcp", "install", "--db", "/tmp/x.db"]).is_err());
}

#[test]
fn mcp_capabilities_parses_formats() {
    let cli = Cli::try_parse_from(["recall", "mcp", "capabilities"]).unwrap();
    match cli.command {
        Some(Commands::Mcp { command: Some(McpCommands::Capabilities { format }), .. }) => {
            assert_eq!(format, crate::mcp::McpCapabilitiesFormat::Text)
        }
        _ => panic!("expected mcp capabilities command"),
    }

    let cli = Cli::try_parse_from(["recall", "mcp", "capabilities", "--format", "json"]).unwrap();
    match cli.command {
        Some(Commands::Mcp { command: Some(McpCommands::Capabilities { format }), .. }) => {
            assert_eq!(format, crate::mcp::McpCapabilitiesFormat::Json)
        }
        _ => panic!("expected mcp capabilities command"),
    }
}

#[test]
fn mcp_help_lists_subcommands() {
    let mut command = Cli::command();
    let help = command.find_subcommand_mut("mcp").unwrap().render_long_help().to_string();
    assert!(help.contains("capabilities"));
    assert!(help.contains("install"));
    assert!(help.contains("uninstall"));
    assert!(help.contains("--db"));
}

#[test]
fn root_help_inserts_installed_extensions_before_options() {
    let mut help = "Commands:\n  search\n\nOptions:\n  -h\n".to_string();
    insert_installed_help(&mut help, "\nExtensions:\n  probe\n");

    let commands = help.find("Commands:").unwrap();
    let installed = help.find("Extensions:").unwrap();
    let options = help.find("Options:").unwrap();
    assert!(commands < installed);
    assert!(installed < options);
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
    assert!(script.contains("usage"));
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
fn extension_list_parses() {
    let cli = Cli::try_parse_from(["recall", "extension", "list"]).unwrap();
    match cli.command {
        Some(Commands::Extension { command: ExtensionCommands::List { available } }) => {
            assert!(!available);
        }
        _ => panic!("expected extension list command"),
    }
}

#[test]
fn extension_list_accepts_ext_alias() {
    let cli = Cli::try_parse_from(["recall", "ext", "list"]).unwrap();
    match cli.command {
        Some(Commands::Extension { command: ExtensionCommands::List { available } }) => {
            assert!(!available);
        }
        _ => panic!("expected extension list command"),
    }
}

#[test]
fn extension_manager_commands_parse() {
    let cli = Cli::try_parse_from(["recall", "ext", "list", "--available"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(Commands::Extension { command: ExtensionCommands::List { available: true } })
    ));

    let cli = Cli::try_parse_from(["recall", "ext", "install", "probe"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(Commands::Extension {
            command: ExtensionCommands::Install { name }
        }) if name == "probe"
    ));

    let cli = Cli::try_parse_from(["recall", "ext", "remove", "probe"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(Commands::Extension {
            command: ExtensionCommands::Remove { name }
        }) if name == "probe"
    ));

    let cli = Cli::try_parse_from(["recall", "ext", "upgrade", "probe"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(Commands::Extension {
            command: ExtensionCommands::Upgrade { name: Some(name) }
        }) if name == "probe"
    ));
}

#[test]
fn unknown_subcommand_parses_as_external_extension() {
    let cli = Cli::try_parse_from(["recall", "reflect", "--limit", "3"]).unwrap();
    match cli.command {
        Some(Commands::External(args)) => {
            assert_eq!(args, ["reflect", "--limit", "3"]);
        }
        _ => panic!("expected external command"),
    }
}
