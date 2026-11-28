use super::*;
use crate::types::{Message, RawUsageEvent, Role, Session};
use crate::usage::aggregate_usage_events;

fn aggregate_wrapped_events(
    events: &[UsageEventRecord],
    period: WrappedPeriod,
    now: DateTime<Local>,
) -> WrappedReport {
    let after = period.after_millis_at(now);
    let mut acc = WrappedAcc::default();
    Store::for_each_deduped_usage_event(events, |event| {
        if after.is_none_or(|cutoff| event.timestamp >= cutoff) {
            acc.add(event);
        }
    });
    acc.finish(period, None)
}

fn event(
    source: &str,
    session_id: &str,
    event_key: &str,
    timestamp: i64,
    model: &str,
    input: i64,
    output: i64,
) -> UsageEventRecord {
    UsageEventRecord {
        session_id: session_id.to_string(),
        source: source.to_string(),
        source_id: session_id.to_string(),
        event_key: event_key.to_string(),
        timestamp,
        model: model.to_string(),
        provider: "test".to_string(),
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: 0,
        token_source: "observed".to_string(),
    }
}

fn local_ts(year: i32, month: u32, day: u32, hour: u32) -> i64 {
    Local
        .with_ymd_and_hms(year, month, day, hour, 0, 0)
        .single()
        .expect("valid local time")
        .timestamp_millis()
}

fn fixture_now() -> DateTime<Local> {
    Local.with_ymd_and_hms(2026, 8, 26, 12, 0, 0).single().expect("valid local time")
}

fn populated_report() -> WrappedReport {
    let now = fixture_now();
    let events = vec![
        event(
            "claude-code",
            "s1",
            "a:1",
            local_ts(2026, 8, 20, 14),
            "claude-sonnet",
            800_000,
            200_000,
        ),
        event(
            "claude-code",
            "s1",
            "a:2",
            local_ts(2026, 8, 21, 14),
            "claude-sonnet",
            100_000,
            50_000,
        ),
        event("codex", "s2", "b:1", local_ts(2026, 8, 22, 9), "gpt-5.5", 80_000, 20_000),
        event("codex", "s3", "b:2", local_ts(2026, 8, 24, 9), "gpt-5.5", 10_000, 5_000),
    ];
    aggregate_wrapped_events(&events, WrappedPeriod::Month, now)
}

#[test]
fn populated_report_aggregates_highlights() {
    let report = populated_report();
    assert!(!report.empty);
    assert_eq!(report.sessions, 3);
    assert_eq!(report.active_days, 4);
    assert_eq!(report.longest_streak, 3);
    assert_eq!(report.tokens.input_tokens, 990_000);
    assert_eq!(report.tokens.output_tokens, 275_000);
    assert_eq!(report.tokens.total_tokens, 1_265_000);
    assert_eq!(report.top_model.as_ref().map(|m| m.model.as_str()), Some("claude-sonnet"));
    assert_eq!(report.top_source.as_ref().map(|s| s.source.as_str()), Some("claude-code"));
    assert_eq!(report.busiest_weekday.as_deref(), Some("thursday"));
    assert_eq!(report.busiest_hour, Some(14));
    assert_eq!(report.by_source.len(), 2);
    assert_eq!(report.by_source[0].source, "claude-code");
    assert_eq!(report.by_source[0].sessions, 1);
}

#[test]
fn period_cutoff_uses_local_calendar_days() {
    let now = fixture_now();
    let events = vec![
        event("codex", "before-week", "w0", local_ts(2026, 8, 19, 13), "gpt-5.5", 10, 1),
        event("codex", "s1", "k1", local_ts(2026, 8, 24, 12), "gpt-5.5", 10, 1),
        event("codex", "before-month", "m0", local_ts(2026, 7, 27, 13), "gpt-5.5", 10, 1),
        event("codex", "s2", "k2", local_ts(2026, 8, 10, 12), "gpt-5.5", 10, 1),
        event("codex", "s3", "k3", local_ts(2026, 4, 1, 12), "gpt-5.5", 10, 1),
        event("codex", "before-year", "y0", local_ts(2025, 8, 26, 13), "gpt-5.5", 10, 1),
        event("codex", "s4", "k4", local_ts(2025, 6, 1, 12), "gpt-5.5", 10, 1),
    ];

    assert_eq!(aggregate_wrapped_events(&events, WrappedPeriod::Week, now).sessions, 1);
    assert_eq!(aggregate_wrapped_events(&events, WrappedPeriod::Month, now).sessions, 3);
    assert_eq!(aggregate_wrapped_events(&events, WrappedPeriod::Year, now).sessions, 5);
    assert_eq!(aggregate_wrapped_events(&events, WrappedPeriod::All, now).sessions, 7);
}

#[test]
fn wrapped_totals_match_usage_report_dedup() {
    let events = vec![
        event("codex", "session-a", "token_count:1", 1_800_000_000_000, "gpt-5.5", 8, 3),
        event("codex", "session-b", "token_count:9", 1_800_000_000_000, "gpt-5.5", 8, 3),
    ];
    let usage = aggregate_usage_events(&events);
    let wrapped = aggregate_wrapped_events(&events, WrappedPeriod::All, fixture_now());
    assert_eq!(wrapped.tokens, usage.summary.tokens);
    assert_eq!(wrapped.sessions, usage.summary.sessions);
}

fn make_session(id: &str, source: &str) -> Session {
    Session {
        source: source.into(),
        directory: Some("/tmp/test".into()),
        started_at: local_ts(2026, 8, 20, 14),
        message_count: 1,
        ..crate::types::test_support::session(id)
    }
}

fn make_usage(key: &str, timestamp: i64, model: &str, input: i64, output: i64) -> RawUsageEvent {
    RawUsageEvent {
        message_seq: Some(1),
        timestamp,
        model: model.into(),
        input_tokens: input,
        output_tokens: output,
        ..crate::types::test_support::usage_event(key)
    }
}

#[test]
fn busiest_slot_uses_actual_weekday_hour_pairs() {
    let now = fixture_now();
    let events = vec![
        event("codex", "s1", "k1", local_ts(2026, 8, 24, 14), "gpt-5.5", 100, 0),
        event("codex", "s2", "k2", local_ts(2026, 8, 25, 9), "gpt-5.5", 60, 0),
        event("codex", "s3", "k3", local_ts(2026, 8, 26, 9), "gpt-5.5", 60, 0),
    ];
    let report = aggregate_wrapped_events(&events, WrappedPeriod::Month, now);
    assert_eq!(report.busiest_weekday.as_deref(), Some("monday"));
    assert_eq!(report.busiest_hour, Some(14));
    let card = render_card(&report, false);
    assert!(card.contains("Monday at 14:00"));
    assert!(!card.contains("Monday at 09:00"));
}

#[test]
fn card_sanitizes_model_and_source_labels_report_stays_verbatim() {
    let now = fixture_now();
    let model = "gpt-5\u{1b}]52;c;ZW1iZWQ=\u{07}\n\u{1b}[31m";
    let source = "claude-code\u{1b}[32m\ninjected";
    let events = vec![event(source, "s1", "k1", local_ts(2026, 8, 24, 14), model, 10, 0)];
    let report = aggregate_wrapped_events(&events, WrappedPeriod::All, now);
    assert_eq!(report.top_model.as_ref().map(|m| m.model.as_str()), Some(model));
    assert_eq!(report.top_source.as_ref().map(|s| s.source.as_str()), Some(source));
    let card = render_card(&report, false);
    assert!(card.contains("gpt-5"));
    assert!(card.contains("Claude Codeinjected"));
    assert!(!card.contains('\u{1b}'));
    assert!(!card.contains('\u{07}'));
    assert!(!card.contains("]52;"));
    assert!(!card.contains("[31m"));
    assert!(!card.contains("[32m"));
    for line in card.lines() {
        assert_eq!(visible_width(line), INNER + 2, "{line}");
    }
}

#[test]
fn store_backed_reports_use_indexed_session_counts() {
    crate::db::schema::register_sqlite_vec();
    let store = Store::open_in_memory().unwrap();
    let session = make_session("s1", "claude-code");
    store.insert_session(&make_session("s2", "claude-code")).unwrap();
    let mut old_session = make_session("s3", "codex");
    old_session.started_at = local_ts(2026, 8, 19, 13);
    old_session.updated_at = Some(local_ts(2026, 8, 25, 13));
    store.insert_session(&old_session).unwrap();
    let mut usage_only_session = make_session("s4", "codex");
    usage_only_session.started_at = local_ts(2026, 8, 1, 13);
    usage_only_session.updated_at = Some(local_ts(2026, 8, 1, 13));
    store
        .persist_session_with_usage(
            &usage_only_session,
            &[],
            &[make_usage("evt-2", local_ts(2026, 8, 25, 14), "gpt-5.5", 10, 1)],
            Some(4),
        )
        .unwrap();
    let messages = vec![Message {
        session_id: "s1".to_string(),
        role: Role::User,
        content: "hello".to_string(),
        timestamp: Some(local_ts(2026, 8, 20, 14)),
        seq: 0,
    }];
    store
        .persist_session_with_usage(
            &session,
            &messages,
            &[make_usage("evt-1", local_ts(2026, 8, 20, 14), "claude-sonnet", 100, 20)],
            Some(1),
        )
        .unwrap();

    let report = build_wrapped_report_at(&store, WrappedPeriod::All, None, fixture_now()).unwrap();
    assert!(!report.empty);
    assert_eq!(report.sessions, 4);
    assert_eq!(report.by_source[0].sessions, 2);
    assert_eq!(report.tokens.total_tokens, 131);
    assert_eq!(report.top_source.unwrap().source, "claude-code");

    let week = build_wrapped_report_at(&store, WrappedPeriod::Week, None, fixture_now()).unwrap();
    assert_eq!(week.sessions, 4);
    assert_eq!(week.by_source.iter().map(|row| row.sessions).sum::<usize>(), 4);
    let codex = week.by_source.iter().find(|row| row.source == "codex").unwrap();
    assert_eq!(codex.sessions, 2);
    assert_eq!(codex.tokens.total_tokens, 11);

    let week_cutoff_now =
        Local.with_ymd_and_hms(2026, 9, 20, 12, 0, 0).single().expect("valid local time");
    let empty =
        build_wrapped_report_at(&store, WrappedPeriod::Week, None, week_cutoff_now).unwrap();
    assert!(empty.empty);
}

#[test]
fn store_backed_report_normalizes_legacy_usage_rows() {
    crate::db::schema::register_sqlite_vec();
    let store = Store::open_in_memory().unwrap();

    let mut codex = make_usage("codex", local_ts(2026, 8, 20, 14), "gpt-5.5", 0, 10);
    codex.reasoning_tokens = 4;
    codex.parser_version = 4;
    store
        .persist_session_with_usage(&make_session("codex", "codex"), &[], &[codex], Some(4))
        .unwrap();

    let mut gemini = make_usage("gemini", local_ts(2026, 8, 20, 14), "gemini-2.5-pro", 100, 20);
    gemini.cache_read_tokens = 30;
    gemini.reasoning_tokens = 5;
    store
        .persist_session_with_usage(&make_session("gemini", "gemini-cli"), &[], &[gemini], Some(1))
        .unwrap();

    let mut qwen = make_usage("qwen", local_ts(2026, 8, 20, 14), "qwen3", 100, 20);
    qwen.cache_read_tokens = 30;
    qwen.reasoning_tokens = 5;
    store
        .persist_session_with_usage(&make_session("qwen", "qwen-code"), &[], &[qwen], Some(1))
        .unwrap();

    let mut pi = make_usage("pi", local_ts(2026, 8, 20, 14), "deepseek", 10, 7);
    pi.cache_read_tokens = 2;
    pi.cache_write_tokens = 1;
    pi.reasoning_tokens = 4;
    pi.parser_version = 1;
    pi.raw_usage_json = Some(r#"{"totalTokens":20}"#.to_string());
    store.persist_session_with_usage(&make_session("pi", "pi"), &[], &[pi], Some(1)).unwrap();

    let report = build_wrapped_report_at(&store, WrappedPeriod::All, None, fixture_now()).unwrap();
    assert_eq!(report.tokens.input_tokens, 150);
    assert_eq!(report.tokens.output_tokens, 49);
    assert_eq!(report.tokens.cache_read_tokens, 62);
    assert_eq!(report.tokens.cache_write_tokens, 1);
    assert_eq!(report.tokens.reasoning_tokens, 18);
    assert_eq!(report.tokens.total_tokens, 280);

    let codex = store.list_usage_events_for_session("codex").unwrap();
    assert_eq!(codex[0].output_tokens, 6);
    assert_eq!(codex[0].reasoning_tokens, 4);
    assert_eq!(codex[0].parser_version, 7);

    let gemini = store.list_usage_events_for_session("gemini").unwrap();
    assert_eq!(gemini[0].input_tokens, 70);
    assert_eq!(gemini[0].cache_read_tokens, 30);
    assert_eq!(gemini[0].parser_version, 2);

    let qwen = store.list_usage_events_for_session("qwen").unwrap();
    assert_eq!(qwen[0].input_tokens, 70);
    assert_eq!(qwen[0].cache_read_tokens, 30);
    assert_eq!(qwen[0].parser_version, 2);

    let pi = store.list_usage_events_for_session("pi").unwrap();
    assert_eq!(pi[0].output_tokens, 3);
    assert_eq!(pi[0].reasoning_tokens, 4);
    assert_eq!(pi[0].parser_version, 2);
}

#[test]
fn wrapped_directory_label_folds_home_and_hides_usernames() {
    assert_eq!(shorten_wrapped_directory("/Users/alice/git/openclaw"), "~/git/openclaw");
    assert_eq!(shorten_wrapped_directory("/home/bob/work/a/b/app"), "~/b/app");
    assert_eq!(shorten_wrapped_directory(r"C:\Users\carol\src\tool\"), "~/src/tool");
    assert_eq!(shorten_wrapped_directory("/Users/alice"), "~");
    assert_eq!(shorten_wrapped_directory("/var/tmp/my-app"), "tmp/my-app");
    for path in ["/Users/alice/git/openclaw", "/home/bob/work/app", r"C:\Users\carol\src\tool"] {
        let label = shorten_wrapped_directory(path);
        assert!(!label.contains("alice") && !label.contains("bob") && !label.contains("carol"));
    }
}

#[test]
fn scoped_wrapped_card_renders_54_cols_and_empty_message() {
    crate::db::schema::register_sqlite_vec();
    let store = Store::open_in_memory().unwrap();
    let mut s1 = make_session("s1", "codex");
    s1.directory = Some("/Users/alice/git/openclaw".into());
    s1.repo_remote = Some("github.com/acme/openclaw".into());
    s1.repo_slug = Some("acme/openclaw".into());
    s1.repo_name = Some("openclaw".into());
    store
        .persist_session_with_usage(
            &s1,
            &[],
            &[make_usage("evt-1", local_ts(2026, 8, 24, 14), "gpt-5.5", 100, 20)],
            Some(1),
        )
        .unwrap();

    let mut s2 = make_session("s2", "codex");
    s2.directory = Some("/Users/alice/git/other".into());
    store
        .persist_session_with_usage(
            &s2,
            &[],
            &[make_usage("evt-2", local_ts(2026, 8, 24, 14), "gpt-5.5", 500, 100)],
            Some(1),
        )
        .unwrap();

    let report =
        build_wrapped_report_at(&store, WrappedPeriod::All, Some("acme/openclaw"), fixture_now())
            .unwrap();
    assert_eq!(report.project_label.as_deref(), Some("acme/openclaw"));
    assert_eq!(report.tokens.total_tokens, 120);
    let card = render_card(&report, false);
    assert!(card.contains("acme/openclaw"));
    for line in card.lines() {
        assert_eq!(visible_width(line), INNER + 2, "{line}");
    }

    let empty = build_wrapped_report_at(
        &store,
        WrappedPeriod::All,
        Some("/Users/alice/git/empty"),
        fixture_now(),
    )
    .unwrap();
    assert!(empty.empty);
    assert_eq!(empty.project_label.as_deref(), Some("~/git/empty"));
    let empty_card = render_card(&empty, false);
    assert!(empty_card.contains("No usage data for this project."));
    assert!(!empty_card.contains("alice"));
    for line in empty_card.lines() {
        assert_eq!(visible_width(line), INNER + 2, "{line}");
    }
}

#[test]
fn scoped_wrapped_runs_global_dedup_before_project_filter() {
    crate::db::schema::register_sqlite_vec();
    let store = Store::open_in_memory().unwrap();
    let mut s1 = make_session("s1", "claude-code");
    s1.directory = Some("/tmp/proj-a".into());
    s1.repo_remote = Some("github.com/acme/proj-a".into());
    s1.repo_slug = Some("acme/proj-a".into());
    s1.repo_name = Some("proj-a".into());

    let mut s2 = make_session("s2", "claude-code");
    s2.directory = Some("/tmp/proj-b".into());
    s2.repo_remote = Some("github.com/acme/proj-b".into());
    s2.repo_slug = Some("acme/proj-b".into());
    s2.repo_name = Some("proj-b".into());

    let shared_event =
        make_usage("assistant:req-1:msg-1", local_ts(2026, 8, 24, 14), "claude-sonnet", 100, 50);
    store
        .persist_session_with_usage(&s1, &[], std::slice::from_ref(&shared_event), Some(1))
        .unwrap();
    store.persist_session_with_usage(&s2, &[], &[shared_event], Some(1)).unwrap();

    let proj_a =
        build_wrapped_report_at(&store, WrappedPeriod::All, Some("acme/proj-a"), fixture_now())
            .unwrap();
    let proj_b =
        build_wrapped_report_at(&store, WrappedPeriod::All, Some("acme/proj-b"), fixture_now())
            .unwrap();
    assert_eq!(proj_a.tokens.total_tokens + proj_b.tokens.total_tokens, 150);
}
