use super::*;

#[test]
fn usage_reports_assign_normalized_duplicates_before_filtering_without_losing_raw_events() {
    let store = setup();
    for (id, source, timestamp, output, reasoning, version) in [
        ("codex-a-owner", "codex", 10, 10, 4, 4),
        ("codex-b-copy", "codex", 10, 6, 4, 5),
        ("claude-owner", "claude-code", 1, 3, 0, 1),
        ("claude-copy", "claude-code", 20, 3, 0, 1),
    ] {
        let mut event = crate::types::test_support::usage_event("assistant:shared");
        event.timestamp = timestamp;
        event.output_tokens = output;
        event.reasoning_tokens = reasoning;
        event.parser_version = version;
        store
            .persist_session_with_usage(
                &make_session(id, source, id, id),
                &[],
                &[event],
                Some(version),
            )
            .unwrap();
    }
    let totals = store.usage_by_session().unwrap();
    assert_eq!(totals["codex-a-owner"].tokens.total_tokens, 10);
    assert_eq!(totals["claude-owner"].tokens.total_tokens, 3);
    assert_eq!(totals["codex-b-copy"].tokens.total_tokens, 0);
    assert!(totals["codex-b-copy"].shared_with_other_session);
    assert_eq!(totals["claude-copy"].tokens.total_tokens, 0);
    assert!(totals["claude-copy"].shared_with_other_session);
    let report =
        build_usage_report(&store, &UsageFilters { sources: None, time_range: TimeRange::All })
            .unwrap();
    assert_eq!(report.summary.events, 2);
    assert_eq!(report.summary.tokens.total_tokens, 13);
    assert_eq!(
        report.summary.tokens.total_tokens,
        totals.values().map(|usage| usage.tokens.total_tokens).sum::<i64>()
    );
    let filtered = store
        .fold_usage_events_after(Some(&["claude-code".to_string()]), Some(10), 0, |count, _| {
            *count += 1
        })
        .unwrap();
    assert_eq!(filtered, 0);
    for id in ["codex-a-owner", "codex-b-copy", "claude-owner", "claude-copy"] {
        assert_eq!(store.list_usage_events_for_session(id).unwrap().len(), 1);
    }
}

#[test]
fn usage_ownership_survives_repersisting_the_owner() {
    let store = setup();
    for source in ["codex", "claude-code", "warp", "qoder"] {
        let sessions = ["a", "b"].map(|suffix| {
            let id = format!("{source}-{suffix}");
            make_session(&id, source, &id, &id)
        });
        let mut event = crate::types::test_support::usage_event("assistant:shared");
        event.input_tokens = 10;
        event.parser_version = 5;
        for session in &sessions {
            store.persist_session_with_usage(session, &[], &[event.clone()], Some(5)).unwrap();
        }
        let before = store.usage_by_session().unwrap();
        let owner =
            sessions.iter().find(|session| before[&session.id].tokens.total_tokens > 0).unwrap();
        store.persist_session_with_usage(owner, &[], &[event], Some(5)).unwrap();
        let after = store.usage_by_session().unwrap();
        for session in &sessions {
            assert_eq!(before[&session.id].tokens, after[&session.id].tokens, "{}", session.id);
            assert_eq!(
                before[&session.id].shared_with_other_session,
                after[&session.id].shared_with_other_session
            );
        }
    }
}

#[test]
fn v21_usage_index_matches_legacy_and_updates_incrementally() {
    use crate::usage::{UsageTarget, build_dashboard_bundle_at};
    use crate::wrapped::{WrappedPeriod, build_wrapped_report_at};
    use chrono::TimeZone;

    let store = setup();
    let now = chrono::Local.with_ymd_and_hms(2026, 6, 15, 14, 7, 30).unwrap();
    let cutoff = TimeRange::Month.cutoff_millis_at(now).unwrap();

    let codex_v4 = RawUsageEvent {
        timestamp: cutoff + 60_000,
        model: "gpt-5".to_string(),
        provider: "openai".to_string(),
        input_tokens: 100,
        output_tokens: 12,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: 5,
        raw_usage_json: None,
        parser_version: 4,
        ..crate::types::test_support::usage_event("token_count:10")
    };
    let codex_v5_dup = RawUsageEvent {
        event_key: "token_count:20".to_string(),
        output_tokens: 7,
        parser_version: 5,
        ..codex_v4.clone()
    };
    let cc_ev = RawUsageEvent {
        timestamp: cutoff - 60_000,
        model: "claude-sonnet".to_string(),
        provider: "anthropic".to_string(),
        input_tokens: 40,
        output_tokens: 15,
        cache_read_tokens: 5,
        cache_write_tokens: 2,
        reasoning_tokens: 0,
        raw_usage_json: None,
        parser_version: 1,
        ..crate::types::test_support::usage_event("assistant:cc-shared")
    };
    let warp_ev = RawUsageEvent {
        timestamp: now.timestamp_millis() - 120_000,
        model: "claude-opus".to_string(),
        provider: "anthropic".to_string(),
        input_tokens: 20,
        output_tokens: 8,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: 3,
        raw_usage_json: None,
        parser_version: 1,
        ..crate::types::test_support::usage_event("assistant:w-shared")
    };
    let qoder_ev = RawUsageEvent {
        timestamp: now.timestamp_millis() - 60_000,
        model: "qoder-1".to_string(),
        provider: "qoder".to_string(),
        input_tokens: 11,
        output_tokens: 4,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: 0,
        raw_usage_json: None,
        parser_version: 1,
        ..crate::types::test_support::usage_event("assistant:line:7")
    };
    let mut pi_ev = RawUsageEvent {
        timestamp: cutoff + 120_000,
        model: "pi-model".to_string(),
        provider: "pi".to_string(),
        input_tokens: 4,
        output_tokens: 6,
        cache_read_tokens: 2,
        cache_write_tokens: 1,
        reasoning_tokens: 0,
        raw_usage_json: Some(r#"{"input":4,"output":6,"cacheRead":2,"cacheWrite":1}"#.to_string()),
        parser_version: 1,
        ..crate::types::test_support::usage_event("assistant:pi-1")
    };

    store
        .persist_session_with_usage(
            &make_session("s-codex-a", "codex", "s-codex-a", "ca"),
            &[],
            &[codex_v4],
            Some(4),
        )
        .unwrap();
    store
        .persist_session_with_usage(
            &make_session("s-codex-b", "codex", "s-codex-b", "cb"),
            &[],
            &[codex_v5_dup],
            Some(5),
        )
        .unwrap();
    store
        .persist_session_with_usage(
            &make_session("s-cc-1", "claude-code", "s-cc-1", "cc1"),
            &[],
            std::slice::from_ref(&cc_ev),
            Some(1),
        )
        .unwrap();
    store
        .persist_session_with_usage(
            &make_session("s-cc-2", "claude-code", "s-cc-2", "cc2"),
            &[],
            &[cc_ev],
            Some(1),
        )
        .unwrap();
    store
        .persist_session_with_usage(
            &make_session("s-w-1", "warp", "w1", "w1"),
            &[],
            std::slice::from_ref(&warp_ev),
            Some(1),
        )
        .unwrap();
    store
        .persist_session_with_usage(
            &make_session("s-w-2", "warp", "w2", "w2"),
            &[],
            &[warp_ev],
            Some(1),
        )
        .unwrap();
    store
        .persist_session_with_usage(
            &make_session("s-qoder", "qoder", "q1", "q1"),
            &[],
            &[qoder_ev],
            Some(1),
        )
        .unwrap();
    store
        .persist_session_with_usage(
            &make_session("s-pi", "pi", "p1", "p1"),
            &[],
            &[pi_ev.clone()],
            Some(1),
        )
        .unwrap();

    store
        .conn
        .execute_batch(
            "DELETE FROM usage_event_ownership;
             DELETE FROM usage_rollups;
             UPDATE background_job_state
             SET phase = 'scan', detail = '{\"cursor\":0,\"end\":999999}'
             WHERE job = 'usage_index';",
        )
        .unwrap();
    assert!(!store.usage_index_ready().unwrap());

    let global = UsageTarget::Scope(ProjectScope::Global);
    let legacy_bundle =
        build_dashboard_bundle_at(&store, None, TimeRange::Month, &global, now).unwrap();
    let legacy_all =
        build_usage_report(&store, &UsageFilters { sources: None, time_range: TimeRange::All })
            .unwrap();
    let legacy_wrapped_all =
        build_wrapped_report_at(&store, WrappedPeriod::All, None, now).unwrap();
    let legacy_wrapped_week =
        build_wrapped_report_at(&store, WrappedPeriod::Week, None, now).unwrap();
    let legacy_by_session = store.usage_by_session().unwrap();
    let legacy_s_w1 = store.session_usage("s-w-1").unwrap();
    let legacy_s_w2 = store.session_usage("s-w-2").unwrap();
    let key_set: std::collections::HashSet<&str> =
        ["s-codex-a", "s-codex-b", "s-cc-1", "s-cc-2", "s-w-1", "s-w-2"].into_iter().collect();
    let legacy_keys = store.counted_usage_event_keys(&key_set).unwrap();
    let legacy_slice_sum = store
        .fold_usage_slices_after(None, Some(cutoff), 0_i64, |acc, slice| {
            *acc += slice.input_tokens
                + slice.output_tokens
                + slice.cache_read_tokens
                + slice.cache_write_tokens
                + slice.reasoning_tokens;
        })
        .unwrap();

    while store.backfill_usage_index_batch().unwrap() {}
    assert!(store.usage_index_ready().unwrap());

    let fast_bundle =
        build_dashboard_bundle_at(&store, None, TimeRange::Month, &global, now).unwrap();
    let fast_all =
        build_usage_report(&store, &UsageFilters { sources: None, time_range: TimeRange::All })
            .unwrap();
    let fast_wrapped_all = build_wrapped_report_at(&store, WrappedPeriod::All, None, now).unwrap();
    let fast_wrapped_week =
        build_wrapped_report_at(&store, WrappedPeriod::Week, None, now).unwrap();
    let fast_by_session = store.usage_by_session().unwrap();
    let fast_s_w1 = store.session_usage("s-w-1").unwrap();
    let fast_s_w2 = store.session_usage("s-w-2").unwrap();
    let fast_keys = store.counted_usage_event_keys(&key_set).unwrap();
    let fast_slice_sum = store
        .fold_usage_slices_after(None, Some(cutoff), 0_i64, |acc, slice| {
            *acc += slice.input_tokens
                + slice.output_tokens
                + slice.cache_read_tokens
                + slice.cache_write_tokens
                + slice.reasoning_tokens;
        })
        .unwrap();

    assert_eq!(
        serde_json::to_value(&fast_bundle.current_report).unwrap(),
        serde_json::to_value(&legacy_bundle.current_report).unwrap()
    );
    assert_eq!(
        serde_json::to_value(&fast_bundle.all_time_report).unwrap(),
        serde_json::to_value(&legacy_bundle.all_time_report).unwrap()
    );
    assert_eq!(
        serde_json::to_value(&fast_bundle.projects_report).unwrap(),
        serde_json::to_value(&legacy_bundle.projects_report).unwrap()
    );
    assert_eq!(
        serde_json::to_value(&fast_all).unwrap(),
        serde_json::to_value(&legacy_all).unwrap()
    );
    assert_eq!(fast_wrapped_all, legacy_wrapped_all);
    assert_eq!(fast_wrapped_week, legacy_wrapped_week);
    assert_eq!(fast_s_w1.tokens, legacy_s_w1.tokens);
    assert_eq!(fast_s_w1.shared_with_other_session, legacy_s_w1.shared_with_other_session);
    assert_eq!(fast_s_w2.tokens, legacy_s_w2.tokens);
    assert_eq!(fast_s_w2.shared_with_other_session, legacy_s_w2.shared_with_other_session);
    assert_eq!(fast_by_session.len(), legacy_by_session.len());
    for (sid, legacy_usage) in &legacy_by_session {
        assert_eq!(fast_by_session[sid].tokens, legacy_usage.tokens, "{sid}");
        assert_eq!(
            fast_by_session[sid].shared_with_other_session, legacy_usage.shared_with_other_session,
            "{sid}"
        );
    }
    assert_eq!(fast_keys, legacy_keys);
    assert_eq!(fast_slice_sum, legacy_slice_sum);

    store.conn.execute_batch("DELETE FROM sync_pending_snapshots;").unwrap();
    store.delete_session_data("warp", "w1").unwrap();
    let promoted = store.session_usage("s-w-2").unwrap();
    assert_eq!(promoted.tokens.total_tokens, 31);
    assert!(!promoted.shared_with_other_session);
    let queued_w2: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM sync_pending_snapshots WHERE session_id = 's-w-2'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(queued_w2, 0);

    pi_ev.input_tokens = 14;
    store
        .persist_session_with_usage(&make_session("s-pi", "pi", "p1", "p1"), &[], &[pi_ev], Some(1))
        .unwrap();
    let updated_pi = store.session_usage("s-pi").unwrap();
    assert_eq!(updated_pi.tokens.input_tokens, 14);
}
