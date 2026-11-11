use super::*;
use crate::db::schema;
use crate::db::search::TimeRange;
use crate::export::{ExportIncludes, ExportOptions, write_jsonl};
use crate::project_scope::ProjectScope;
use crate::types::{EvidenceVisibility, ParentRelation, ThreadRole};

fn setup() -> Store {
    schema::register_sqlite_vec();
    Store::open_in_memory().unwrap()
}

fn full_session(source: &str, source_id: &str, title: &str) -> Session {
    Session {
        id: format!("local-{source_id}"),
        source: source.to_string(),
        source_id: source_id.to_string(),
        title: title.to_string(),
        directory: Some("/tmp/project".to_string()),
        repo_remote: Some("github.com/samzong/Recall".to_string()),
        repo_slug: Some("samzong/Recall".to_string()),
        repo_name: Some("Recall".to_string()),
        started_at: 1_000,
        updated_at: Some(2_000),
        message_count: 2,
        entrypoint: Some("cli".to_string()),
        custom_title: Some("Custom".to_string()),
        summary: Some("A summary".to_string()),
        duration_minutes: Some(7),
        source_file_path: Some("/home/origin/.codex/sessions/a.jsonl".to_string()),
        is_import: false,
        locations: Vec::new(),
        alternative_versions: 0,
    }
}

fn full_messages(session_id: &str) -> Vec<Message> {
    vec![
        Message {
            session_id: session_id.to_string(),
            role: Role::User,
            content: "zebraquery".to_string(),
            timestamp: Some(1_000),
            seq: 0,
        },
        Message {
            session_id: session_id.to_string(),
            role: Role::Assistant,
            content: "assistant reply".to_string(),
            timestamp: Some(1_500),
            seq: 2,
        },
    ]
}

fn full_usage_event() -> RawUsageEvent {
    RawUsageEvent {
        event_key: "uk1".to_string(),
        event_seq: 0,
        message_seq: Some(2),
        timestamp: 1_500,
        model: "gpt-test".to_string(),
        provider: "openai".to_string(),
        input_tokens: 10,
        output_tokens: 20,
        cache_read_tokens: 3,
        cache_write_tokens: 2,
        reasoning_tokens: 1,
        token_source: TokenSource::Observed,
        parser_version: 4,
        source_path: Some("/home/origin/raw.jsonl".to_string()),
        raw_usage_json: Some("{\"input_tokens\":10}".to_string()),
    }
}

fn full_event() -> RawSessionEvent {
    RawSessionEvent {
        command_evidence_status: Some(crate::types::CommandEvidenceStatus::Unsupported),
        files: vec![
            crate::types::FileEvidence {
                path: "src/alpha.rs".into(),
                operation: crate::types::FileOperation::Write,
                kind: crate::types::FileEvidenceKind::Call,
                cwd: Some("/workspace/project".into()),
                target: Some(crate::types::FileTarget {
                    absolute_path: "/workspace/project/src/alpha.rs".into(),
                    repo_root: Some("/workspace/project".into()),
                    repo_relative_path: Some("src/alpha.rs".into()),
                    repo_remote: Some("github.com/fixture/project".into()),
                }),
            },
            crate::types::FileEvidence {
                path: "src/β.rs".into(),
                operation: crate::types::FileOperation::Delete,
                kind: crate::types::FileEvidenceKind::Call,
                cwd: None,
                target: None,
            },
        ],
        event_seq: 0,
        timestamp: Some(1_200),
        kind: "file_write".to_string(),
        actor: "assistant".to_string(),
        name: Some("apply_patch".to_string()),
        status: Some("ok".to_string()),
        target: Some("src/alpha.rs".to_string()),
        message_seq: Some(0),
        summary: Some("updated alpha and removed beta".to_string()),
        source_path: Some("/home/origin/raw.jsonl".to_string()),
        source_event_id: Some("ev-1".to_string()),
        tool_call_id: Some("call-1".to_string()),
        is_meta: Some(false),
        visibility: Some(EvidenceVisibility::Visible),
        attrs_json: Some("{\"exit_code\":0}".to_string()),
        parser_version: 5,
    }
}

fn persist_full(store: &Store, source: &str, source_id: &str, title: &str) {
    let session = full_session(source, source_id, title);
    let messages = full_messages(&session.id);
    let parents = vec![
        ParentLink {
            relation: ParentRelation::Spawn,
            source: "codex".to_string(),
            source_id: "parent-session".to_string(),
        },
        ParentLink {
            relation: ParentRelation::Fork,
            source: "codex".to_string(),
            source_id: "parent-session".to_string(),
        },
    ];
    let topology = SessionTopologyWrite {
        thread_role: Some(ThreadRole::Subagent),
        parents: &parents,
        parser_version: Some(1),
    };
    store
        .replace_session_with_usage_and_events_with_topology(
            source,
            source_id,
            &session,
            &messages,
            &[full_usage_event()],
            Some(4),
            &[full_event()],
            Some(5),
            &topology,
        )
        .unwrap();
}

fn export_all(store: &Store) -> String {
    let options = ExportOptions {
        session_ids: Vec::new(),
        sources: None,
        time_range: TimeRange::All,
        scope: ProjectScope::Global,
        thread_role: None,
        limit: None,
        includes: ExportIncludes::full(),
    };
    let mut out = Vec::new();
    write_jsonl(store, &options, &mut out).unwrap();
    String::from_utf8(out).unwrap()
}

fn count(store: &Store, sql: &str) -> i64 {
    store.conn.query_row(sql, [], |row| row.get(0)).unwrap()
}

#[test]
fn roundtrip_preserves_all_fields() {
    let a = setup();
    persist_full(&a, "codex", "src-1", "Roundtrip");
    let exported = export_all(&a);

    let b = setup();
    let summary = import_jsonl(&b, false, exported.as_bytes()).unwrap();
    assert_eq!(summary, ImportSummary { total: 1, imported: 1, skipped: 0 });

    let reexported = export_all(&b);
    let mut orig: serde_json::Value = serde_json::from_str(exported.trim()).unwrap();
    let mut copy: serde_json::Value = serde_json::from_str(reexported.trim()).unwrap();
    assert_eq!(orig["events"][0]["command_evidence_status"], "unsupported");
    orig["session"]["id"] = serde_json::Value::Null;
    copy["session"]["id"] = serde_json::Value::Null;
    assert_eq!(orig, copy, "export -> import -> export must be lossless");
    let mut legacy: serde_json::Value = serde_json::from_str(exported.trim()).unwrap();
    legacy["events"][0].as_object_mut().unwrap().remove("command_evidence_status");
    let legacy_store = setup();
    import_jsonl(&legacy_store, false, legacy.to_string().as_bytes()).unwrap();
    let restored: serde_json::Value =
        serde_json::from_str(export_all(&legacy_store).trim()).unwrap();
    assert_eq!(restored["events"][0]["command_evidence_status"], serde_json::Value::Null);
}

#[test]
fn event_replacement_preserves_files_on_failure_and_removes_old_associations() {
    let store = setup();
    persist_full(&store, "codex", "src-1", "Files");
    let before = export_all(&store);
    let first_id: i64 =
        store.conn.query_row("SELECT id FROM session_events", [], |row| row.get(0)).unwrap();
    let mut invalid_replacement = full_event();
    invalid_replacement.command_evidence_status =
        Some(crate::types::CommandEvidenceStatus::Complete);
    assert!(
        store
            .persist_session_events_for_existing_session(
                "codex",
                "src-1",
                &[invalid_replacement.clone(), invalid_replacement],
                6,
                None,
            )
            .is_err()
    );
    assert_eq!(export_all(&store), before);
    assert_eq!(count(&store, "SELECT COUNT(*) FROM event_files"), 2);
    let mut replacement = full_event();
    replacement.files.remove(0);
    replacement.command_evidence_status = Some(crate::types::CommandEvidenceStatus::LimitExceeded);
    store
        .persist_session_events_for_existing_session(
            "codex",
            "src-1",
            &[replacement.clone()],
            6,
            None,
        )
        .unwrap();
    let next_id: i64 =
        store.conn.query_row("SELECT id FROM session_events", [], |row| row.get(0)).unwrap();
    assert!(next_id > first_id);
    assert_eq!(count(&store, "SELECT COUNT(*) FROM event_files"), 1);
    let session_id: String =
        store.conn.query_row("SELECT id FROM sessions", [], |row| row.get(0)).unwrap();
    assert_eq!(
        store.list_session_events_for_session(&session_id).unwrap()[0].files,
        replacement.files
    );
    assert_eq!(
        store.list_session_events_for_session(&session_id).unwrap()[0].command_evidence_status,
        replacement.command_evidence_status
    );
    store.conn.execute("DELETE FROM sessions", []).unwrap();
    assert_eq!(count(&store, "SELECT COUNT(*) FROM event_files"), 0);
}

#[test]
fn import_marks_is_import_and_writes_no_state_rows() {
    let a = setup();
    persist_full(&a, "codex", "src-1", "Marked");
    let exported = export_all(&a);

    let b = setup();
    import_jsonl(&b, false, exported.as_bytes()).unwrap();

    assert_eq!(count(&b, "SELECT COUNT(*) FROM sessions WHERE is_import = 1"), 1);
    assert_eq!(count(&b, "SELECT COUNT(*) FROM usage_session_state"), 0);
    assert_eq!(count(&b, "SELECT COUNT(*) FROM event_session_state"), 0);
    assert_eq!(count(&b, "SELECT COUNT(*) FROM usage_events"), 1);
    assert_eq!(count(&b, "SELECT COUNT(*) FROM session_events"), 1);
}

#[test]
fn import_is_idempotent() {
    let a = setup();
    persist_full(&a, "codex", "src-1", "Idem");
    persist_full(&a, "grok", "src-2", "Idem2");
    let exported = export_all(&a);

    let b = setup();
    let first = import_jsonl(&b, false, exported.as_bytes()).unwrap();
    assert_eq!(first, ImportSummary { total: 2, imported: 2, skipped: 0 });

    let second = import_jsonl(&b, false, exported.as_bytes()).unwrap();
    assert_eq!(second, ImportSummary { total: 2, imported: 0, skipped: 2 });
    assert_eq!(count(&b, "SELECT COUNT(*) FROM sessions"), 2);
    assert_eq!(count(&b, "SELECT COUNT(*) FROM messages"), 4);
    assert_eq!(count(&b, "SELECT COUNT(*) FROM usage_events"), 2);
}

#[test]
fn skip_keeps_local_content_on_conflicting_import() {
    let a = setup();
    persist_full(&a, "codex", "src-1", "Remote version");
    let exported = export_all(&a);

    let b = setup();
    persist_full(&b, "codex", "src-1", "Local version");
    let summary = import_jsonl(&b, false, exported.as_bytes()).unwrap();
    assert_eq!(summary, ImportSummary { total: 1, imported: 0, skipped: 1 });

    let title: String = b
        .conn
        .query_row(
            "SELECT title FROM sessions WHERE source = 'codex' AND source_id = 'src-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(title, "Local version");
}

#[test]
fn dry_run_writes_nothing() {
    let a = setup();
    persist_full(&a, "codex", "src-1", "Dry");
    let exported = export_all(&a);

    let b = setup();
    let summary = import_jsonl(&b, true, exported.as_bytes()).unwrap();
    assert_eq!(summary, ImportSummary { total: 1, imported: 1, skipped: 0 });
    for table in
        ["sessions", "messages", "usage_events", "session_events", "session_embedding_state"]
    {
        assert_eq!(count(&b, &format!("SELECT COUNT(*) FROM {table}")), 0, "{table} dirty");
    }
}

#[test]
fn duplicate_keys_in_one_file_count_once_in_dry_run_and_real_run() {
    let a = setup();
    persist_full(&a, "codex", "src-1", "Dup");
    let line = export_all(&a);
    let doubled = format!("{line}{line}");

    let b = setup();
    let dry = import_jsonl(&b, true, doubled.as_bytes()).unwrap();
    let real = import_jsonl(&b, false, doubled.as_bytes()).unwrap();

    let expected = ImportSummary { total: 2, imported: 1, skipped: 1 };
    assert_eq!(dry, expected, "dry-run must predict exactly what a real import writes");
    assert_eq!(real, expected);
    assert_eq!(count(&b, "SELECT COUNT(*) FROM sessions"), 1);
}

#[test]
fn malformed_line_fails_with_line_number_keeping_prior_rows() {
    let a = setup();
    persist_full(&a, "codex", "src-1", "Good");
    let mut exported = export_all(&a);
    exported.push_str("not json\n");

    let b = setup();
    let err = import_jsonl(&b, false, exported.as_bytes()).unwrap_err();
    assert!(err.to_string().contains("line 2"), "unexpected error: {err}");
    assert_eq!(count(&b, "SELECT COUNT(*) FROM sessions"), 1);
}

#[test]
fn rejects_unsupported_schema_version_and_record_type() {
    let store = setup();
    let bad_version = r#"{"schema_version":99,"record_type":"session","session":{"source":"codex","source_id":"x","title":"t","started_at":0}}"#;
    let err = import_jsonl(&store, false, bad_version.as_bytes()).unwrap_err();
    assert!(err.to_string().contains("schema_version"), "unexpected error: {err}");

    let bad_type = r#"{"schema_version":3,"record_type":"snapshot","session":{"source":"codex","source_id":"x","title":"t","started_at":0}}"#;
    let err = import_jsonl(&store, false, bad_type.as_bytes()).unwrap_err();
    assert!(err.to_string().contains("record_type"), "unexpected error: {err}");

    let bad_visibility = r#"{"schema_version":6,"record_type":"session","session":{"source":"codex","source_id":"x","title":"t","started_at":0},"events":[{"event_seq":0,"kind":"tool","actor":"assistant","visibility":"unknown"}]}"#;
    let err = import_jsonl(&store, false, bad_visibility.as_bytes()).unwrap_err();
    assert!(err.to_string().contains("unknown variant"), "unexpected error: {err}");
}

#[test]
fn accepts_schema_version_2_without_v3_fields() {
    let store = setup();
    let v2 = r#"{"schema_version":2,"record_type":"session","session":{"source":"codex","source_id":"v2-1","title":"V2","started_at":100},"messages":[{"seq":0,"role":"user","timestamp":100,"content":"hello v2"}],"usage_events":[{"event_key":"k","event_seq":0,"message_seq":null,"timestamp":100,"model":"m","provider":"p","input_tokens":1,"output_tokens":2,"cache_read_tokens":0,"cache_write_tokens":0,"reasoning_tokens":0,"token_source":"observed"}],"events":[{"event_seq":0,"timestamp":null,"kind":"tool","actor":"assistant"}]}"#;
    let summary = import_jsonl(&store, false, v2.as_bytes()).unwrap();
    assert_eq!(summary, ImportSummary { total: 1, imported: 1, skipped: 0 });

    let parser_version: i64 = store
        .conn
        .query_row("SELECT parser_version FROM usage_events", [], |row| row.get(0))
        .unwrap();
    assert_eq!(parser_version, 0, "missing v3 fields must default to 0");
    let events = store
        .list_session_events_for_session(
            &store.get_session_by_source_id("codex", "v2-1").unwrap().unwrap().id,
        )
        .unwrap();
    assert_eq!(events[0].tool_call_id, None);
    assert_eq!(events[0].is_meta, None);
    assert_eq!(events[0].visibility, None);
}

#[test]
fn preserves_message_seq_and_fts() {
    let a = setup();
    persist_full(&a, "codex", "src-1", "Seq");
    let exported = export_all(&a);

    let b = setup();
    import_jsonl(&b, false, exported.as_bytes()).unwrap();

    let seqs: Vec<u32> = {
        let mut stmt = b.conn.prepare("SELECT seq FROM messages ORDER BY seq").unwrap();
        let rows = stmt.query_map([], |row| row.get(0)).unwrap();
        rows.collect::<Result<Vec<_>, _>>().unwrap()
    };
    assert_eq!(seqs, vec![0, 2], "non-contiguous seq must be preserved");

    let fts_hits =
        count(&b, "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'zebraquery'");
    assert_eq!(fts_hits, 1, "imported messages must be searchable via FTS");
}

#[test]
fn local_sync_refresh_clears_is_import() {
    let a = setup();
    persist_full(&a, "codex", "src-1", "Converge");
    let exported = export_all(&a);

    let b = setup();
    import_jsonl(&b, false, exported.as_bytes()).unwrap();
    assert_eq!(count(&b, "SELECT COUNT(*) FROM sessions WHERE is_import = 1"), 1);

    persist_full(&b, "codex", "src-1", "Now local");
    assert_eq!(count(&b, "SELECT COUNT(*) FROM sessions WHERE is_import = 1"), 0);
}
