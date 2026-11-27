use super::*;

fn installed(ids: &[&str]) -> HashSet<String> {
    ids.iter().map(|id| (*id).to_string()).collect()
}

#[test]
fn extract_skill_from_skill_tool_attrs() {
    let mut event = SkillAuditEventRow {
        session_id: "s1".to_string(),
        timestamp: Some(1),
        name: Some("Skill".to_string()),
        target: None,
        attrs_json: Some(r#"{"skill":"commit"}"#.to_string()),
    };
    let (id, signal) = extract_skill_from_event(&event, &installed(&["commit"])).unwrap();
    assert_eq!(id, "commit");
    assert_eq!(signal, SkillSignal::SkillTool);
    for attrs in [
        r#"{"type":"tool_use","name":"Skill","input":{"skill":"commit"}}"#,
        r#"{"type":"tool-call","toolName":"Skill","args":{"skill":"commit"}}"#,
        r#"{"type":"function_call","name":"Skill","arguments":"{\"skill\":\"commit\"}"}"#,
        r#"{"type":"custom_tool_call","name":"Skill","input":{"skill":"commit"}}"#,
    ] {
        event.attrs_json = Some(attrs.into());
        assert_eq!(extract_skill_from_event(&event, &installed(&["commit"])).unwrap().0, "commit");
    }
    event.attrs_json =
        Some(r#"{"type":"tool-result","toolName":"Skill","result":{"name":"result"}}"#.into());
    assert!(extract_skill_from_event(&event, &installed(&["commit"])).is_none());
}

#[test]
fn extract_skill_from_opencode_native_skill_tool() {
    let event = SkillAuditEventRow {
        session_id: "s1".to_string(),
        timestamp: Some(1),
        name: Some("skill".to_string()),
        target: None,
        attrs_json: Some(
            r#"{"part_id":"part","message_id":"message","part":{"type":"tool","state":{"input":{"name":"commit"}}}}"#.to_string(),
        ),
    };
    let (id, signal) = extract_skill_from_event(&event, &installed(&["commit"])).unwrap();
    assert_eq!(id, "commit");
    assert_eq!(signal, SkillSignal::SkillTool);
}

#[test]
fn list_skill_audit_events_includes_command_wrapped_skill_md_reads() {
    use crate::db::schema;
    use crate::db::search::TimeRange;
    use crate::db::store::Store;
    use crate::types::{RawSessionEvent, Session};

    schema::register_sqlite_vec();
    let store = Store::open_in_memory().unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    let session = Session {
        id: "session-1".to_string(),
        source: "codex".to_string(),
        source_id: "codex-1".to_string(),
        title: "Codex session".to_string(),
        directory: None,
        repo_remote: None,
        repo_slug: None,
        repo_name: None,
        started_at: now,
        updated_at: Some(now),
        message_count: 0,
        entrypoint: None,
        custom_title: None,
        summary: None,
        duration_minutes: None,
        source_file_path: None,
        is_import: false,
        locations: Vec::new(),
        alternative_versions: 0,
    };
    let event = RawSessionEvent {
        command_evidence_status: None,
        files: Vec::new(),
        event_seq: 0,
        timestamp: Some(now),
        kind: "command".to_string(),
        actor: "assistant".to_string(),
        name: Some("bash".to_string()),
        status: None,
        target: Some(r#"cat "/Users/x/.agents/skills/pre-ship/SKILL.md" && echo ok"#.to_string()),
        message_seq: None,
        summary: None,
        source_path: None,
        source_event_id: None,
        tool_call_id: None,
        is_meta: None,
        visibility: None,
        attrs_json: None,
        parser_version: 1,
    };
    let result = RawSessionEvent {
        event_seq: 1,
        kind: "tool_result".into(),
        actor: "tool".into(),
        name: Some("Skill".into()),
        target: None,
        attrs_json: Some(r#"{"skill":"commit"}"#.into()),
        ..event.clone()
    };
    store
        .persist_session_with_usage_and_events(&session, &[], &[], None, &[event, result], Some(1))
        .unwrap();

    let events = store.list_skill_audit_events(None, TimeRange::All).unwrap();
    assert_eq!(events.len(), 1);
    let (id, signal) = extract_skill_from_event(&events[0], &installed(&["pre-ship"])).unwrap();
    assert_eq!(id, "pre-ship");
    assert_eq!(signal, SkillSignal::ReadSkillFile);
}

#[test]
fn list_skill_audit_events_uses_session_timestamp_when_event_timestamp_missing() {
    use crate::db::schema;
    use crate::db::search::TimeRange;
    use crate::db::store::Store;
    use crate::types::{RawSessionEvent, Session};

    schema::register_sqlite_vec();
    let store = Store::open_in_memory().unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    let session = Session {
        id: "session-1".to_string(),
        source: "cursor".to_string(),
        source_id: "cursor-1".to_string(),
        title: "Cursor session".to_string(),
        directory: None,
        repo_remote: None,
        repo_slug: None,
        repo_name: None,
        started_at: now,
        updated_at: Some(now),
        message_count: 0,
        entrypoint: None,
        custom_title: None,
        summary: None,
        duration_minutes: None,
        source_file_path: None,
        is_import: false,
        locations: Vec::new(),
        alternative_versions: 0,
    };
    let event = RawSessionEvent {
        command_evidence_status: None,
        files: Vec::new(),
        event_seq: 0,
        timestamp: None,
        kind: "tool_call".to_string(),
        actor: "assistant".to_string(),
        name: Some("Skill".to_string()),
        status: None,
        target: None,
        message_seq: None,
        summary: None,
        source_path: None,
        source_event_id: None,
        tool_call_id: None,
        is_meta: None,
        visibility: None,
        attrs_json: Some(r#"{"skill":"commit"}"#.to_string()),
        parser_version: 1,
    };
    store
        .persist_session_with_usage_and_events(&session, &[], &[], None, &[event], Some(1))
        .unwrap();

    assert_eq!(store.list_skill_audit_events(None, TimeRange::All).unwrap().len(), 1);
    let ranged = store.list_skill_audit_events(None, TimeRange::Month).unwrap();
    assert_eq!(ranged.len(), 1);
    assert_eq!(ranged[0].timestamp, Some(now));
}

#[test]
fn skill_usage_index_matches_legacy_and_handles_null_timestamps_and_candidates() {
    use crate::db::schema;
    use crate::db::search::TimeRange;
    use crate::db::store::Store;
    use crate::types::{RawSessionEvent, Session};

    schema::register_sqlite_vec();
    let store = Store::open_in_memory().unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    let old = now - 40 * 86_400_000;

    let make_sess = |id: &str, source: &str, updated: i64| Session {
        id: id.to_string(),
        source: source.to_string(),
        source_id: id.to_string(),
        title: id.to_string(),
        directory: Some("/tmp/proj".to_string()),
        repo_remote: None,
        repo_slug: None,
        repo_name: None,
        started_at: old,
        updated_at: Some(updated),
        message_count: 0,
        entrypoint: None,
        custom_title: None,
        summary: None,
        duration_minutes: None,
        source_file_path: None,
        is_import: false,
        locations: Vec::new(),
        alternative_versions: 0,
    };
    let make_ev = |seq: u32,
                   ts: Option<i64>,
                   kind: &str,
                   name: &str,
                   target: Option<&str>,
                   attrs: Option<&str>| RawSessionEvent {
        command_evidence_status: None,
        files: Vec::new(),
        event_seq: seq,
        timestamp: ts,
        kind: kind.to_string(),
        actor: "assistant".to_string(),
        name: Some(name.to_string()),
        status: None,
        target: target.map(str::to_string),
        message_seq: None,
        summary: None,
        source_path: None,
        source_event_id: None,
        tool_call_id: None,
        is_meta: None,
        visibility: None,
        attrs_json: attrs.map(str::to_string),
        parser_version: 1,
    };

    store
        .persist_session_with_usage_and_events(
            &make_sess("s1", "claude-code", now),
            &[],
            &[],
            None,
            &[
                make_ev(
                    0,
                    Some(now - 60_000),
                    "tool_call",
                    "Skill",
                    None,
                    Some(r#"{"skill":"commit"}"#),
                ),
                make_ev(
                    1,
                    None,
                    "command",
                    "bash",
                    Some(r#"cat "/Users/x/.agents/skills/ship/SKILL.md""#),
                    None,
                ),
            ],
            Some(1),
        )
        .unwrap();
    store
        .persist_session_with_usage_and_events(
            &make_sess("s2", "cursor", old),
            &[],
            &[],
            None,
            &[
                make_ev(0, None, "tool_call", "Skill", None, Some(r#"{"skill":"commit"}"#)),
                make_ev(1, Some(old), "tool_call", "Skill", None, Some(r#"{"skill":"land"}"#)),
            ],
            Some(1),
        )
        .unwrap();
    store
        .persist_session_with_usage_and_events(
            &make_sess("s3", "codex", now),
            &[],
            &[],
            None,
            &[make_ev(
                0,
                Some(now - 30_000),
                "tool_call",
                "Skill",
                None,
                Some(r#"{"skill":"unlisted"}"#),
            )],
            Some(1),
        )
        .unwrap();

    let skills = vec![
        InstalledSkill { id: "commit".into(), install_path: "/skills/commit".into() },
        InstalledSkill { id: "ship".into(), install_path: "/skills/ship".into() },
        InstalledSkill { id: "land".into(), install_path: "/skills/land".into() },
    ];
    let month_filters = SkillAuditFilters {
        sources: None,
        time_range: TimeRange::Month,
        target: UsageTarget::Scope(ProjectScope::Global),
    };
    let all_filters = SkillAuditFilters {
        sources: None,
        time_range: TimeRange::All,
        target: UsageTarget::Scope(ProjectScope::Global),
    };

    store
        .conn
        .execute_batch(
            "DELETE FROM session_skill_usage;
             UPDATE background_job_state
             SET phase = 'scan', detail = '{\"cursor\":0,\"end\":999999}'
             WHERE job = 'skill_usage';",
        )
        .unwrap();
    assert!(!store.skill_usage_index_ready().unwrap());
    let legacy_month =
        build_skill_audit_report_with_installed(&store, &month_filters, skills.clone()).unwrap();
    let legacy_all =
        build_skill_audit_report_with_installed(&store, &all_filters, skills.clone()).unwrap();

    while store.backfill_skill_usage_batch().unwrap() {}
    assert!(store.skill_usage_index_ready().unwrap());
    let indexed_month =
        build_skill_audit_report_with_installed(&store, &month_filters, skills.clone()).unwrap();
    let indexed_all =
        build_skill_audit_report_with_installed(&store, &all_filters, skills.clone()).unwrap();

    assert_eq!(
        serde_json::to_value(&indexed_month).unwrap(),
        serde_json::to_value(&legacy_month).unwrap()
    );
    assert_eq!(
        serde_json::to_value(&indexed_all).unwrap(),
        serde_json::to_value(&legacy_all).unwrap()
    );
    assert!(indexed_all.coverage_note.is_none());

    store
        .conn
        .execute("UPDATE sessions SET updated_at = ?1 WHERE id = 's2'", [now - 1000])
        .unwrap();
    let after_touch =
        build_skill_audit_report_with_installed(&store, &month_filters, skills.clone()).unwrap();
    let commit_entry = after_touch
        .core
        .iter()
        .chain(after_touch.occasional.iter())
        .find(|e| e.id == "commit")
        .unwrap();
    assert_eq!(commit_entry.invocations, 2);

    store
        .persist_session_with_usage_and_events(
            &make_sess("s4", "codex", now),
            &[],
            &[],
            None,
            &[make_ev(0, Some(now), "tool_call", "Skill", None, Some(r#"{"skill":"land"}"#))],
            Some(1),
        )
        .unwrap();
    let after_inc =
        build_skill_audit_report_with_installed(&store, &month_filters, skills).unwrap();
    let land_entry =
        after_inc.occasional.iter().chain(after_inc.core.iter()).find(|e| e.id == "land").unwrap();
    assert_eq!(land_entry.invocations, 1);
}
