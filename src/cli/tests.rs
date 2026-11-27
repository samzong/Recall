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

fn sorted_object_keys(value: &serde_json::Value) -> Vec<&str> {
    let mut keys: Vec<&str> = value.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    keys
}

fn assert_json_keys(value: &serde_json::Value, expected: &[&str]) {
    let mut expected_sorted = expected.to_vec();
    expected_sorted.sort_unstable();
    assert_eq!(sorted_object_keys(value), expected_sorted);
}

fn assert_token_totals_contract(tokens: &serde_json::Value) -> [i64; 6] {
    assert_json_keys(
        tokens,
        &[
            "cache_read_tokens",
            "cache_write_tokens",
            "input_tokens",
            "output_tokens",
            "reasoning_tokens",
            "total_tokens",
        ],
    );
    let buckets = [
        tokens["input_tokens"].as_i64().unwrap(),
        tokens["output_tokens"].as_i64().unwrap(),
        tokens["cache_read_tokens"].as_i64().unwrap(),
        tokens["cache_write_tokens"].as_i64().unwrap(),
        tokens["reasoning_tokens"].as_i64().unwrap(),
        tokens["total_tokens"].as_i64().unwrap(),
    ];
    for count in buckets {
        assert!(count >= 0, "negative token count in {tokens}");
    }
    assert_eq!(
        buckets[..5].iter().sum::<i64>(),
        buckets[5],
        "5-bucket conservation failed: {tokens}"
    );
    buckets
}

fn assert_bucket_slice_sum<'a>(
    entries: impl IntoIterator<Item = &'a serde_json::Value>,
    expected_keys: &[&str],
    expected_tokens: [i64; 6],
    expected_events: u64,
) {
    let mut sums = [0i64; 6];
    let mut events = 0u64;
    for entry in entries {
        assert_json_keys(entry, expected_keys);
        events += entry["events"].as_u64().unwrap();
        let entry_tokens = assert_token_totals_contract(&entry["tokens"]);
        for (acc, val) in sums.iter_mut().zip(entry_tokens) {
            *acc += val;
        }
    }
    assert_eq!(sums, expected_tokens);
    assert_eq!(events, expected_events);
}

fn assert_usage_report_contract(
    report: &crate::usage::UsageReport,
    expected_scope: Option<(&str, &str)>,
) {
    let raw = serde_json::to_string(report).unwrap();
    let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
    if let Some((kind, value)) = expected_scope {
        assert_json_keys(
            &json,
            &["by_model", "by_source", "daily", "monthly", "scope", "summary", "weekly"],
        );
        assert_json_keys(&json["scope"], &["directory", "kind", "repository", "value"]);
        assert_eq!(json["scope"]["kind"], kind);
        assert_eq!(json["scope"]["value"], value);
    } else {
        assert!(json.get("scope").is_none());
        assert_json_keys(
            &json,
            &["by_model", "by_source", "daily", "monthly", "summary", "weekly"],
        );
    }
    let summary = &json["summary"];
    assert_json_keys(
        summary,
        &["events", "models", "sessions", "sources", "token_source_events", "tokens"],
    );
    let summary_tokens = assert_token_totals_contract(&summary["tokens"]);
    let summary_events = summary["events"].as_u64().unwrap();
    let by_source = json["by_source"].as_array().unwrap();
    let by_model = json["by_model"].as_array().unwrap();
    assert_eq!(summary["sources"].as_u64().unwrap(), by_source.len() as u64);
    assert_eq!(summary["models"].as_u64().unwrap(), by_model.len() as u64);
    let token_source_sum: u64 = summary["token_source_events"]
        .as_object()
        .unwrap()
        .values()
        .map(|v| v.as_u64().unwrap())
        .sum();
    assert_eq!(token_source_sum, summary_events);
    assert_bucket_slice_sum(
        by_source,
        &["events", "sessions", "source", "tokens"],
        summary_tokens,
        summary_events,
    );
    assert_bucket_slice_sum(
        by_model,
        &["events", "model", "provider", "sessions", "source", "tokens"],
        summary_tokens,
        summary_events,
    );
    for period in ["daily", "weekly", "monthly"] {
        assert_bucket_slice_sum(
            json[period].as_array().unwrap(),
            &["events", "period", "sessions", "tokens"],
            summary_tokens,
            summary_events,
        );
    }
}

fn assert_project_usage_report_contract(
    report: &crate::usage::ProjectUsageReport,
    expected_scope: Option<(&str, &str)>,
) {
    let raw = serde_json::to_string(report).unwrap();
    let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
    if let Some((kind, value)) = expected_scope {
        assert_json_keys(&json, &["projects", "scope", "summary"]);
        assert_json_keys(&json["scope"], &["directory", "kind", "repository", "value"]);
        assert_eq!(json["scope"]["kind"], kind);
        assert_eq!(json["scope"]["value"], value);
    } else {
        assert!(json.get("scope").is_none());
        assert_json_keys(&json, &["projects", "summary"]);
    }
    let summary = &json["summary"];
    assert_json_keys(
        summary,
        &["events", "indexed_sessions", "models", "projects", "sessions", "sources", "tokens"],
    );
    let summary_tokens = assert_token_totals_contract(&summary["tokens"]);
    let summary_events = summary["events"].as_u64().unwrap();
    let projects = json["projects"].as_array().unwrap();
    assert_eq!(summary["projects"].as_u64().unwrap(), projects.len() as u64);
    assert_bucket_slice_sum(
        projects,
        &[
            "by_model",
            "directory",
            "events",
            "indexed_sessions",
            "kind",
            "project",
            "repository",
            "sessions",
            "tokens",
            "top_model",
        ],
        summary_tokens,
        summary_events,
    );
    for entry in projects {
        let entry_tokens = assert_token_totals_contract(&entry["tokens"]);
        let entry_events = entry["events"].as_u64().unwrap();
        if entry["repository"].is_object() {
            assert_json_keys(&entry["repository"], &["name", "remote", "slug"]);
        }
        assert_bucket_slice_sum(
            entry["by_model"].as_array().unwrap(),
            &["events", "model", "provider", "sessions", "source", "tokens"],
            entry_tokens,
            entry_events,
        );
    }
}

fn seed_usage_contract_store() -> crate::db::store::Store {
    use crate::types::{RawUsageEvent, Session};
    use chrono::{Local, TimeZone};

    crate::db::schema::register_sqlite_vec();
    let store = crate::db::store::Store::open_in_memory().unwrap();
    let ts = |month: u32, day: u32, hour: u32| {
        Local.with_ymd_and_hms(2026, month, day, hour, 0, 0).single().unwrap().timestamp_millis()
    };
    let make_event =
        |key: &str, timestamp: i64, provider: &str, model: &str, buckets: [i64; 5]| RawUsageEvent {
            event_key: key.into(),
            timestamp,
            provider: provider.into(),
            model: model.into(),
            input_tokens: buckets[0],
            output_tokens: buckets[1],
            cache_read_tokens: buckets[2],
            cache_write_tokens: buckets[3],
            reasoning_tokens: buckets[4],
            ..crate::types::test_support::usage_event(key)
        };

    let s1 = Session {
        source: "claude-code".into(),
        directory: Some("/tmp/acme-app".into()),
        repo_remote: Some("github.com/acme/app".into()),
        repo_slug: Some("acme/app".into()),
        repo_name: Some("app".into()),
        started_at: ts(8, 24, 10),
        updated_at: Some(ts(8, 25, 11)),
        ..crate::types::test_support::session("usage-s1")
    };
    let s2 = Session {
        source: "codex".into(),
        directory: Some("/tmp/local-tool".into()),
        started_at: ts(8, 25, 14),
        updated_at: Some(ts(8, 25, 14)),
        ..crate::types::test_support::session("usage-s2")
    };
    let s3_zero = Session {
        source: "codex".into(),
        directory: Some("/tmp/local-tool".into()),
        started_at: ts(8, 26, 9),
        updated_at: Some(ts(8, 26, 12)),
        ..crate::types::test_support::session("usage-s3-zero")
    };
    let s4_unattributed = Session {
        source: "rx-agent".into(),
        directory: None,
        started_at: ts(9, 1, 16),
        updated_at: Some(ts(9, 1, 16)),
        ..crate::types::test_support::session("usage-s4")
    };

    store
        .persist_session_with_usage(
            &s1,
            &[],
            &[
                make_event(
                    "assistant:req-1:msg-1",
                    ts(8, 24, 10),
                    "anthropic",
                    "claude-sonnet",
                    [100, 40, 20, 10, 5],
                ),
                make_event(
                    "assistant:req-2:msg-2",
                    ts(8, 25, 11),
                    "anthropic",
                    "claude-sonnet",
                    [50, 15, 5, 3, 2],
                ),
            ],
            Some(1),
        )
        .unwrap();
    store
        .persist_session_with_usage(
            &s2,
            &[],
            &[RawUsageEvent {
                parser_version: 5,
                ..make_event("token_count:1", ts(8, 25, 14), "openai", "gpt-5", [30, 12, 8, 4, 6])
            }],
            Some(5),
        )
        .unwrap();
    store.persist_session_with_usage(&s3_zero, &[], &[], Some(1)).unwrap();
    store
        .persist_session_with_usage(
            &s4_unattributed,
            &[],
            &[make_event("msg-1", ts(9, 1, 16), "fixture", "fixture-model", [11, 7, 3, 2, 1])],
            Some(1),
        )
        .unwrap();
    store
}

#[test]
fn usage_report_locks_json_keys_conservation_and_scope() {
    use crate::db::search::TimeRange;
    use crate::usage::{
        UsageFilters, UsageTarget, build_usage_report, build_usage_report_for_target,
    };

    let store = seed_usage_contract_store();
    let global =
        build_usage_report(&store, &UsageFilters { sources: None, time_range: TimeRange::All })
            .unwrap();
    assert_usage_report_contract(&global, None);
    assert_eq!(global.summary.events, 4);
    assert_eq!(global.summary.sessions, 3);
    assert_eq!(global.summary.sources, 3);
    assert_eq!(global.summary.models, 3);
    assert_eq!(global.summary.tokens.total_tokens, 334);
    assert_eq!(global.monthly.len(), 2);

    let dir_scope = store.resolve_cli_project_selector("/tmp/local-tool").unwrap();
    let dir_envelope = dir_scope.envelope();
    let mut dir_report =
        build_usage_report_for_target(&store, None, TimeRange::All, &UsageTarget::Scope(dir_scope))
            .unwrap();
    dir_report.scope = Some(dir_envelope);
    assert_usage_report_contract(&dir_report, Some(("directory", "/tmp/local-tool")));
    let dir_json = serde_json::to_value(&dir_report).unwrap();
    assert_eq!(dir_json["scope"]["directory"], "/tmp/local-tool");
    assert!(dir_json["scope"]["repository"].is_null());
    assert_eq!(dir_report.summary.events, 1);
    assert_eq!(dir_report.summary.sessions, 1);
    assert_eq!(dir_report.summary.tokens.total_tokens, 60);

    let repo_scope = store.resolve_cli_project_selector("acme/app").unwrap();
    let repo_envelope = repo_scope.envelope();
    let mut repo_report = build_usage_report_for_target(
        &store,
        None,
        TimeRange::All,
        &UsageTarget::Scope(repo_scope),
    )
    .unwrap();
    repo_report.scope = Some(repo_envelope);
    assert_usage_report_contract(&repo_report, Some(("repository", "acme/app")));
    assert_eq!(repo_report.summary.events, 2);
    assert_eq!(repo_report.summary.sessions, 1);
    assert_eq!(repo_report.summary.tokens.total_tokens, 250);
}

#[test]
fn project_usage_report_locks_json_keys_conservation_and_scope() {
    use crate::db::search::TimeRange;
    use crate::project_scope::ProjectScope;
    use crate::usage::build_project_usage_report;

    let store = seed_usage_contract_store();
    let report = build_project_usage_report(&store, None, TimeRange::All).unwrap();
    assert_project_usage_report_contract(&report, None);
    assert_eq!(report.summary.projects, 3);
    assert_eq!(report.summary.events, 4);
    assert_eq!(report.summary.sessions, 3);
    assert_eq!(report.summary.indexed_sessions, 4);
    assert_eq!(report.summary.sources, 3);
    assert_eq!(report.summary.models, 3);
    assert_eq!(report.summary.tokens.total_tokens, 334);

    let json = serde_json::to_value(&report).unwrap();
    let projects = json["projects"].as_array().unwrap();
    let repo_entry = projects.iter().find(|p| p["kind"] == "repository").unwrap();
    assert_eq!(repo_entry["project"], "acme/app");
    assert!(repo_entry["directory"].is_null());
    assert_eq!(repo_entry["repository"]["slug"], "acme/app");

    let dir_entry = projects.iter().find(|p| p["kind"] == "directory").unwrap();
    assert_eq!(dir_entry["project"], "/tmp/local-tool");
    assert_eq!(dir_entry["directory"], "/tmp/local-tool");
    assert!(dir_entry["repository"].is_null());
    assert_eq!(dir_entry["sessions"], 1);
    assert_eq!(dir_entry["indexed_sessions"], 2);

    let mut scoped = report.clone();
    scoped.scope = Some(ProjectScope::Directory("/tmp/local-tool".into()).envelope());
    assert_project_usage_report_contract(&scoped, Some(("directory", "/tmp/local-tool")));
}
