use super::{Cli, Commands, recall_skill_bundle};
use clap::Parser;

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
fn unknown_subcommand_parses_as_external_extension() {
    let cli = Cli::try_parse_from(["recall", "reflect", "--limit", "3"]).unwrap();
    match cli.command {
        Some(Commands::External(args)) => {
            assert_eq!(args, ["reflect", "--limit", "3"]);
        }
        _ => panic!("expected external command"),
    }
}

#[test]
fn embedded_recall_skill_documents_mcp_signatures_and_inline_budgets() {
    let kitup::SkillBundle::Files(files) = recall_skill_bundle() else {
        panic!("expected embedded skill files bundle");
    };
    let skill_file =
        files.iter().find(|file| file.path == "SKILL.md").expect("embedded SKILL.md must exist");
    let skill = std::str::from_utf8(&skill_file.contents).expect("embedded SKILL.md must be UTF-8");

    for required in [
        "call_mcp_tool",
        "ServerName: \"recall\"",
        "mcp/recall/*.json",
        "mcp/recall/<tool>.json",
        "search_messages",
        "search_sessions",
        "list_recent_sessions",
        "get_session",
        "file_history",
        "query: string",
        "project?: string",
        "source?: string",
        "limit?: 1..50",
        "invocation_nonce?: string",
        "session_id?: string",
        "session_id: string",
        "around_seq?: u32",
        "before?: u32",
        "after?: u32",
        "from_seq?: u32",
        "to_seq?: u32",
        "max_chars?: 1..6000",
        "cursor?: string",
        "max_messages?: u32",
        "tail?: bool",
        "include_events?: bool",
        "event_ref?: string",
        "evidence_part?: \"payload\"|\"before\"|\"after\"",
        "max_bytes?: 1024..65536",
        "path: string",
        "target_project?: string",
        "include_command_candidates?: bool",
        "kind?: string",
        "8,800",
        r#"{"target_project":"owner/repo","path":"src/main.rs","include_command_candidates":true}"#,
    ] {
        assert!(skill.contains(required), "embedded SKILL.md missing `{required}`");
    }

    for stale in [
        r#"{"target_project":"owner/repo","path":"src/main.rs","include_command_candidates":true,"limit":20}"#,
        "defaults to 16,384",
    ] {
        assert!(!skill.contains(stale), "embedded SKILL.md still contains stale `{stale}`");
    }
}
