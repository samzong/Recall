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

fn assert_usage_owner(store: &Store, owner: &str) {
    let totals = store.usage_by_session().unwrap();
    assert!(totals[owner].tokens.total_tokens > 0);
    assert!(totals.iter().all(|(id, usage)| id == owner || usage.tokens.total_tokens == 0));
    let indexed =
        build_usage_report(store, &UsageFilters { sources: None, time_range: TimeRange::All })
            .unwrap();
    store
        .conn
        .execute("UPDATE background_job_state SET phase = 'scan' WHERE job = 'usage_index'", [])
        .unwrap();
    let raw =
        build_usage_report(store, &UsageFilters { sources: None, time_range: TimeRange::All })
            .unwrap();
    store
        .conn
        .execute("UPDATE background_job_state SET phase = 'done' WHERE job = 'usage_index'", [])
        .unwrap();
    assert_eq!(indexed.summary.tokens, raw.summary.tokens);
    let counted = store
        .fold_usage_events_after(None, None, Vec::new(), |ids, event| ids.push(event.session_id))
        .unwrap();
    assert_eq!(counted, vec![owner.to_string()]);
}

#[test]
fn native_ownership_tracks_binding_lifecycle_and_rebuild() {
    let store = setup();
    let mut imported = make_session("aaa-import", "warp", "shared", "import");
    imported.is_import = true;
    let mut native = make_session("zzz-native", "warp", "shared", "native");
    native.message_count = 0;
    imported.message_count = 0;
    let event = make_usage_event("shared", 1_800_000, "model");
    store.persist_session_with_usage(&imported, &[], std::slice::from_ref(&event), None).unwrap();
    store.conn.execute("DELETE FROM native_bindings", []).unwrap();
    store.conn.execute("INSERT INTO session_sync(session_id, sync_id) VALUES ('aaa-import', '00000000-0000-4000-8000-000000000001')", []).unwrap();
    store.persist_session_with_usage(&native, &[], std::slice::from_ref(&event), Some(1)).unwrap();
    assert_usage_owner(&store, &native.id);
    store.prepare_remote(&ProjectScope::Global, &mut |_, _| {}).unwrap();
    assert_usage_owner(&store, &native.id);
    store.conn.execute("UPDATE session_sync SET sync_id = 'ffffffff-ffff-4fff-8fff-ffffffffffff' WHERE session_id = 'zzz-native'", []).unwrap();
    store.delete_session_data("warp", "shared").unwrap();
    assert_usage_owner(&store, &imported.id);
    store.persist_session_with_usage(&native, &[], &[], None).unwrap();
    assert_usage_owner(&store, &native.id);
    store
        .conn
        .execute("UPDATE native_bindings SET session_id = 'aaa-import', confirmed = 0", [])
        .unwrap();
    assert_usage_owner(&store, &imported.id);
    let raw = crate::adapters::RawSession::search_only(
        "shared",
        imported.directory.clone(),
        imported.started_at,
        imported.updated_at,
        None,
        vec![],
    );
    let store = crate::sync::persist_raw_session_for_conformance(store, "warp", raw).unwrap();
    assert!(!store.get_session_by_id(&imported.id).unwrap().unwrap().is_import);
    assert_eq!(count_rows(&store, "SELECT confirmed FROM native_bindings"), 1);
    assert_usage_owner(&store, &imported.id);
    store.conn.execute_batch("PRAGMA user_version = 21").unwrap();
    schema::init(&store.conn).unwrap();
    while store.backfill_usage_index_batch().unwrap() {}
    assert_usage_owner(&store, &imported.id);
    assert_eq!(count_rows(&store, "SELECT COUNT(*) FROM usage_events"), 2);
}

#[test]
fn v22_normalizes_only_native_copilot_provenance_once() {
    let store = setup();
    for (id, native, version) in [
        ("native", true, 2),
        ("import-unconfirmed", false, 2),
        ("import-confirmed", false, 2),
        ("export-v3", false, 3),
    ] {
        let mut session = make_session(id, "copilot-cli", id, id);
        session.is_import = !native;
        let mut event = make_usage_event("usage:1", 1_800_000, "model");
        event.output_tokens = if version == 3 { 15 } else { 50 };
        event.reasoning_tokens = 35;
        event.parser_version = version;
        event.raw_usage_json = Some(r#"{"output_tokens":50}"#.into());
        let mut missing_raw = event.clone();
        missing_raw.event_key = "usage:2".into();
        missing_raw.raw_usage_json = None;
        store
            .persist_session_with_usage(&session, &[], &[event, missing_raw], native.then_some(2))
            .unwrap();
    }
    store.clear_import_marker("copilot-cli", "import-confirmed").unwrap();
    for _ in 0..2 {
        store.conn.execute_batch("PRAGMA user_version = 21").unwrap();
        schema::init(&store.conn).unwrap();
        let rows = store.list_usage_events_for_session("native").unwrap();
        assert_eq!(
            (rows[0].output_tokens, rows[0].reasoning_tokens, rows[0].parser_version),
            (15, 35, 3)
        );
        assert_eq!((rows[1].output_tokens, rows[1].parser_version), (50, 2));
        assert_eq!(store.usage_state_meta_map("copilot-cli").unwrap()["native"].parser_version, 2);
        for id in ["import-unconfirmed", "import-confirmed"] {
            let row = &store.list_usage_events_for_session(id).unwrap()[0];
            assert_eq!((row.output_tokens, row.reasoning_tokens, row.parser_version), (50, 35, 2));
        }
        assert_eq!(store.list_usage_events_for_session("export-v3").unwrap()[0].output_tokens, 15);
    }
    let mut exported = Vec::new();
    write_jsonl(
        &store,
        &ExportOptions {
            session_ids: vec![],
            sources: None,
            time_range: TimeRange::All,
            scope: ProjectScope::Global,
            thread_role: None,
            limit: None,
            includes: ExportIncludes::full(),
        },
        &mut exported,
    )
    .unwrap();
    let imported = setup();
    crate::import::import_jsonl(&imported, false, exported.as_slice()).unwrap();
    let session = imported.get_session_by_source_id("copilot-cli", "export-v3").unwrap().unwrap();
    assert_eq!(imported.list_usage_events_for_session(&session.id).unwrap()[0].output_tokens, 15);
}

#[test]
fn usage_saturates_and_v22_recovers_real_rollups_across_scan_batches() {
    let store = setup();
    store.insert_session(&make_session("large", "copilot-cli", "large", "large")).unwrap();
    store.conn.execute_batch(
        "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x < 5001)
         INSERT INTO usage_events(session_id,source,source_id,event_key,event_seq,timestamp,model,provider,input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,reasoning_tokens,token_source,parser_version,created_at)
         SELECT 'large','copilot-cli','large',CAST(x AS TEXT),x,unixepoch()*1000,'model','provider',9223372036854775807,9223372036854775807,9223372036854775807,9223372036854775807,9223372036854775807,'observed',3,0 FROM n;
         INSERT INTO usage_rollups VALUES(1800000,'copilot-cli','provider','model','observed','large',5001,1e30,1,1,1,1);
         PRAGMA user_version = 21;"
    ).unwrap();
    schema::init(&store.conn).unwrap();
    assert_eq!(count_rows(&store, "SELECT COUNT(*) FROM usage_rollups"), 0);
    let filters = UsageFilters { sources: None, time_range: TimeRange::All };
    let raw = build_usage_report(&store, &filters).unwrap();
    assert_eq!(raw.summary.tokens.input_tokens, i64::MAX);
    assert_eq!(raw.summary.tokens.output_tokens, i64::MAX);
    assert_eq!(raw.summary.tokens.cache_read_tokens, i64::MAX);
    assert_eq!(raw.summary.tokens.cache_write_tokens, i64::MAX);
    assert_eq!(raw.summary.tokens.reasoning_tokens, i64::MAX);
    assert!(raw.hourly_today_tokens.contains(&i64::MAX));
    assert_eq!(raw.summary.tokens.total_tokens, i64::MAX);
    while store.backfill_usage_index_batch().unwrap() {}
    let indexed = build_usage_report(&store, &filters).unwrap();
    assert_eq!(raw.summary.tokens, indexed.summary.tokens);
    let kind: String = store
        .conn
        .query_row("SELECT typeof(input_tokens) FROM usage_rollups", [], |row| row.get(0))
        .unwrap();
    assert_eq!(kind, "integer");
    let event = make_usage_event("next", 1_800_000, "other-model");
    store
        .persist_usage_events_for_existing_session("copilot-cli", "large", &[event], 3, None)
        .unwrap();
    assert_eq!(store.usage_by_session().unwrap()["large"].tokens.total_tokens, 21);
}

#[test]
fn usage_import_and_remote_reject_invalid_amounts_and_bucket_floor() {
    let store = setup();
    for (field, value) in [
        ("input_tokens", serde_json::json!(-1)),
        ("timestamp", serde_json::json!(i64::MIN)),
        ("output_tokens", serde_json::json!(1.5)),
        ("output_tokens", serde_json::json!(9223372036854775808_u64)),
    ] {
        let session = make_session("invalid", "copilot-cli", "invalid", "invalid");
        store
            .persist_session_with_usage(
                &session,
                &[],
                &[make_usage_event("1", 0, "model")],
                Some(3),
            )
            .unwrap();
        let mut record = store.remote_snapshot(session).unwrap();
        record["usage_events"][0][field] = value;
        let target = setup();
        assert!(
            crate::import::import_jsonl(&target, false, record.to_string().as_bytes()).is_err()
        );
        assert!(crate::import::decode_remote(record, "remote".into()).is_err());
        assert_eq!(count_rows(&target, "SELECT COUNT(*) FROM sessions"), 0);
    }
}

#[test]
fn remote_usage_snapshots_replace_nonempty_and_empty_usage() {
    use crate::db::remote_store::{Revision, digest};
    let source = setup();
    let target = setup();
    let mut session = make_session("source", "copilot-cli", "remote", "remote");
    session.message_count = 0;
    let mut parent = None;
    for (key, count) in [("old", 1), ("new", 1), ("empty", 0)] {
        let events = if count == 0 { vec![] } else { vec![make_usage_event(key, 0, "model")] };
        source
            .replace_session_with_usage_and_events(
                "copilot-cli",
                "remote",
                &session,
                &[],
                &events,
                Some(3),
                &[],
                None,
            )
            .unwrap();
        let revision = Revision {
            version: 1,
            sync_id: "00000000-0000-4000-8000-000000000002".into(),
            parent: parent.clone(),
            parent_sessions: vec![],
            record: source.remote_snapshot(session.clone()).unwrap(),
        };
        let body = serde_json::to_vec(&revision).unwrap();
        let hash = digest(&body);
        target.cache_remote(&format!("v1/revisions/{hash}.json"), &body).unwrap();
        target.merge_remote().unwrap();
        let id: String =
            target.conn.query_row("SELECT id FROM sessions", [], |row| row.get(0)).unwrap();
        let rows = target.list_usage_events_for_session(&id).unwrap();
        assert_eq!(rows.len(), count);
        if count != 0 {
            assert_eq!(rows[0].event_key, key);
        }
        assert_eq!(count_rows(&target, "SELECT COUNT(*) FROM usage_session_state"), 0);
        parent = Some(hash);
    }
}
