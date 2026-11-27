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

#[test]
fn usage_cli_flags_enforce_conflicts_and_requirements() {
    assert!(Cli::try_parse_from(["recall", "usage", "--projects", "--json"]).is_ok());
    assert!(Cli::try_parse_from(["recall", "usage", "--project", "acme/app", "--json"]).is_ok());
    assert!(
        Cli::try_parse_from(["recall", "usage", "--card", "--project", ".", "--period", "all"])
            .is_ok()
    );

    for invalid in [
        &["recall", "usage", "--projects", "--project", "acme/app"][..],
        &["recall", "usage", "--projects", "--card"][..],
        &["recall", "usage", "--json", "--card"][..],
        &["recall", "usage", "--source", "codex", "--card"][..],
        &["recall", "usage", "--time", "7d", "--card"][..],
        &["recall", "usage", "--period", "all"][..],
    ] {
        let Err(error) = Cli::try_parse_from(invalid) else {
            panic!("accepted invalid usage args: {invalid:?}");
        };
        assert_eq!(error.exit_code(), 2);
    }
}

#[test]
fn usage_json_omits_scope_by_default_and_includes_when_scoped() {
    crate::db::schema::register_sqlite_vec();
    let store = crate::db::store::Store::open_in_memory().unwrap();
    let global = crate::usage::build_usage_report(
        &store,
        &crate::usage::UsageFilters {
            sources: None,
            time_range: crate::db::search::TimeRange::All,
        },
    )
    .unwrap();
    let global_json = serde_json::to_value(&global).unwrap();
    assert!(global_json.get("scope").is_none());

    let mut scoped = global.clone();
    scoped.scope =
        Some(crate::project_scope::ProjectScope::Directory("/tmp/demo".into()).envelope());
    let scoped_json = serde_json::to_value(&scoped).unwrap();
    assert_eq!(scoped_json["scope"]["kind"], "directory");
    assert_eq!(scoped_json["scope"]["value"], "/tmp/demo");
}

#[test]
fn cli_project_selector_normalizes_dot_tilde_and_preserves_symlinks() {
    crate::db::schema::register_sqlite_vec();
    let store = crate::db::store::Store::open_in_memory().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let real_dir = temp.path().join("real-proj");
    std::fs::create_dir_all(&real_dir).unwrap();
    #[cfg(unix)]
    let link_dir = temp.path().join("link-proj");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&real_dir, &link_dir).unwrap();

    #[cfg(unix)]
    {
        let link_str = format!("{}/", link_dir.to_string_lossy());
        assert_eq!(
            store.resolve_cli_project_selector(&link_str).unwrap(),
            crate::project_scope::ProjectScope::Directory(link_dir.to_string_lossy().into_owned())
        );
    }

    let prev = std::env::current_dir().unwrap();
    let non_git = temp.path().join("plain-dir");
    std::fs::create_dir_all(&non_git).unwrap();
    std::env::set_current_dir(&non_git).unwrap();
    let dot_scope = store.resolve_cli_project_selector(".").unwrap();
    let rel_scope = store.resolve_cli_project_selector("../real-proj/").unwrap();
    std::env::set_current_dir(&prev).unwrap();

    let expected_non_git = std::fs::canonicalize(&non_git).unwrap_or(non_git);
    let expected_real = std::fs::canonicalize(&real_dir).unwrap_or(real_dir);
    assert!(matches!(
        dot_scope,
        crate::project_scope::ProjectScope::Directory(ref dir)
            if std::path::Path::new(dir).file_name() == expected_non_git.file_name()
    ));
    assert!(matches!(
        rel_scope,
        crate::project_scope::ProjectScope::Directory(ref dir)
            if std::path::Path::new(dir).file_name() == expected_real.file_name()
    ));

    let tilde = store.resolve_cli_project_selector("~/sub/../final/").unwrap();
    let home = dirs::home_dir().unwrap();
    assert_eq!(
        tilde,
        crate::project_scope::ProjectScope::Directory(
            home.join("final").to_string_lossy().into_owned()
        )
    );
}

#[test]
fn bundle_dedups_globally_partitions_non_native_and_unions_weekly_sessions() {
    use crate::db::search::TimeRange;
    use crate::types::{RawUsageEvent, Session};
    use crate::usage::{ProjectPartitionKey, UsageTarget, build_dashboard_bundle_at};
    use chrono::{Local, TimeZone};

    crate::db::schema::register_sqlite_vec();
    let store = crate::db::store::Store::open_in_memory().unwrap();
    let ts = |day: u32, hour: u32| {
        Local.with_ymd_and_hms(2026, 8, day, hour, 0, 0).single().unwrap().timestamp_millis()
    };
    let now = Local.with_ymd_and_hms(2026, 8, 26, 15, 0, 0).single().unwrap();

    let mut s1 = Session {
        source: "claude-code".into(),
        directory: Some("/tmp/repo-a".into()),
        repo_remote: Some("github.com/acme/repo-a".into()),
        repo_slug: Some("acme/repo-a".into()),
        repo_name: Some("repo-a".into()),
        started_at: ts(24, 10),
        updated_at: Some(ts(25, 10)),
        ..crate::types::test_support::session("s1")
    };
    s1.id = "s1".into();
    let mut s2 = Session {
        source: "claude-code".into(),
        directory: Some("/tmp/dir-only".into()),
        started_at: ts(24, 11),
        updated_at: Some(ts(24, 11)),
        is_import: true,
        ..crate::types::test_support::session("s2")
    };
    s2.id = "s2".into();
    let mut s3_zero = Session {
        source: "codex".into(),
        directory: Some("/tmp/dir-only/".into()),
        started_at: ts(26, 9),
        updated_at: Some(ts(26, 12)),
        ..crate::types::test_support::session("s3")
    };
    s3_zero.id = "s3".into();

    let shared = RawUsageEvent {
        timestamp: ts(24, 10),
        model: "claude-sonnet".into(),
        input_tokens: 100,
        output_tokens: 20,
        ..crate::types::test_support::usage_event("assistant:req-1:msg-1")
    };
    let day2 = RawUsageEvent {
        timestamp: ts(25, 10),
        model: "claude-sonnet".into(),
        input_tokens: 50,
        output_tokens: 10,
        ..crate::types::test_support::usage_event("assistant:req-2:msg-2")
    };
    store.persist_session_with_usage(&s1, &[], &[shared.clone(), day2], Some(1)).unwrap();
    store.conn.execute(
        "INSERT INTO sessions (id, source, source_id, title, directory, started_at, updated_at, is_import)
         VALUES ('s2', 'claude-code', 's2', 'imported', '/tmp/dir-only', ?1, ?1, 1)",
        rusqlite::params![ts(24, 11)],
    ).unwrap();
    store.conn.execute(
        "INSERT INTO usage_events (session_id, source, source_id, event_key, event_seq, timestamp, model, provider, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens, token_source, parser_version, created_at)
         VALUES ('s2', 'claude-code', 's2', 'assistant:req-1:msg-1', 0, ?1, 'claude-sonnet', 'anthropic', 100, 20, 0, 0, 0, 'observed', 1, ?1)",
        rusqlite::params![ts(24, 11)],
    ).unwrap();
    store.insert_session(&s3_zero).unwrap();

    let bundle = build_dashboard_bundle_at(
        &store,
        None,
        TimeRange::Week,
        &UsageTarget::Partition {
            key: ProjectPartitionKey::Directory("/tmp/dir-only".into()),
            label: "/tmp/dir-only".into(),
        },
        now,
    )
    .unwrap();

    assert_eq!(bundle.current_report.summary.tokens.total_tokens, 0);
    assert_eq!(bundle.current_report.daily_all_sessions.get("2026-08-24"), Some(&1));
    assert_eq!(bundle.current_report.daily_all_sessions.get("2026-08-26"), Some(&1));
    assert_eq!(bundle.current_report.weekly_all_sessions.get("2026-W35"), Some(&2));
    assert_eq!(bundle.current_report.hourly_today_sessions[12], 1);

    let global_bundle = build_dashboard_bundle_at(
        &store,
        None,
        TimeRange::All,
        &UsageTarget::Scope(crate::project_scope::ProjectScope::Global),
        now,
    )
    .unwrap();
    assert_eq!(global_bundle.current_report.daily_all_sessions.get("2026-08-24"), Some(&2));
    assert_eq!(global_bundle.current_report.daily_all_sessions.get("2026-08-25"), Some(&1));
    assert_eq!(global_bundle.current_report.daily_all_sessions.get("2026-08-26"), Some(&1));
    assert_eq!(global_bundle.current_report.weekly_all_sessions.get("2026-W35"), Some(&3));

    let dir_project = global_bundle
        .projects_report
        .projects
        .iter()
        .find(|p| p.key == ProjectPartitionKey::Directory("/tmp/dir-only".into()))
        .unwrap();
    assert_eq!(dir_project.indexed_sessions, 2);
    assert_eq!(
        store
            .session_ids_in_scope(&crate::project_scope::ProjectScope::Directory(
                "/tmp/dir-only".into()
            ))
            .unwrap()
            .len(),
        1
    );
}
