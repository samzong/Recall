use super::*;
use crate::db::store::Store;

#[test]
fn remote_identity_migration_preserves_v16_data_and_a_restorable_backup() {
    register_sqlite_vec();
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("recall.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL;").unwrap();
    for migrate in [
        migrate_v1,
        migrate_v2,
        migrate_v3,
        migrate_v4,
        migrate_v5,
        migrate_v6,
        migrate_v7,
        migrate_v8,
        migrate_v9,
        migrate_v10,
        migrate_v11,
        migrate_v12,
        migrate_v13,
        migrate_v14,
        migrate_v15,
        migrate_v16,
    ] {
        migrate(&conn).unwrap();
    }
    conn.execute_batch(
        "INSERT INTO sessions(id, source, source_id, title, started_at) VALUES ('native', 'codex', 'raw', 'Original', 1);
         INSERT INTO sessions(id, source, source_id, title, started_at, is_import) VALUES ('legacy', 'codex', 'old', 'Imported', 1, 1);
         INSERT INTO messages(id, session_id, role, content, seq) VALUES (1, 'native', 'user', 'migrationpreserved', 0);
         INSERT INTO usage_events(session_id, source, source_id, event_key, event_seq, timestamp, input_tokens, token_source, created_at)
             VALUES ('native', 'codex', 'raw', 'u1', 0, 1, 37, 'observed', 1);
         INSERT INTO usage_session_state(session_id, source, source_id, parser_version, synced_at) VALUES ('native', 'codex', 'raw', 4, 1);
         INSERT INTO session_events(id, session_id, source, source_id, event_seq, kind, actor, attrs_json, created_at)
             VALUES (1, 'native', 'codex', 'raw', 0, 'tool_call', 'assistant', '{\"kept\":true}', 1);
         INSERT INTO event_session_state(session_id, source, source_id, parser_version, synced_at) VALUES ('native', 'codex', 'raw', 5, 1);
         INSERT INTO event_files(event_id, position, path, evidence_json) VALUES (1, 0, '/work/file', '{}');
         INSERT INTO session_parent_links VALUES ('native', 'fork', 'codex', 'old');",
    ).unwrap();
    conn.execute(
        "INSERT INTO message_vec(message_id, embedding) VALUES (1, ?1)",
        [serde_json::to_string(&vec![0.1_f32; 384]).unwrap()],
    )
    .unwrap();
    drop(conn);
    let store = Store::open_at(&path).unwrap();
    assert_eq!(schema_version(&store.conn).unwrap(), SCHEMA_VERSION);
    assert_eq!(store.get_messages("native").unwrap()[0].content, "migrationpreserved");
    assert_eq!(store.list_usage_events_for_session("native").unwrap()[0].input_tokens, 37);
    assert_eq!(store.session_topology("native").unwrap().parents[0].source_id, "old");
    for table in [
        "message_vec",
        "usage_session_state",
        "event_session_state",
        "session_events",
        "event_files",
    ] {
        assert_eq!(
            store
                .conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
    assert!(store.has_native_binding("native").unwrap());
    assert!(!store.has_native_binding("legacy").unwrap());
    assert!(store.get_session_by_id("native").unwrap().unwrap().locations.is_empty());
    assert!(store.get_session_by_id("legacy").unwrap().unwrap().locations.is_empty());
    assert_eq!(
        store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'migrationpreserved'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
    let backups = std::fs::read_dir(root.path())
        .unwrap()
        .filter_map(|entry| {
            let path = entry.unwrap().path();
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("recall-before-remote-")
                .then_some(path)
        })
        .collect::<Vec<_>>();
    assert_eq!(backups.len(), 1);
    let backup = Connection::open(&backups[0]).unwrap();
    let restored = root.path().join("restored.db");
    backup.backup(rusqlite::DatabaseName::Main, &restored, None).unwrap();
    let restored = Connection::open(restored).unwrap();
    assert_eq!(schema_version(&restored).unwrap(), 16);
    assert_eq!(
        restored
            .query_row("SELECT content FROM messages WHERE session_id = 'native'", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap(),
        "migrationpreserved"
    );
    assert!(restored.execute("INSERT INTO sessions(id, source, source_id, title, started_at) VALUES ('collision', 'codex', 'raw', 'Duplicate', 1)", []).is_err());
}

#[test]
fn v14_preserves_existing_events_and_is_idempotent() {
    register_sqlite_vec();
    let store = Store::open_in_memory().unwrap();
    store.conn.execute_batch(
        r#"ALTER TABLE session_events DROP COLUMN command_evidence_status;
         DROP TABLE event_files;
         PRAGMA user_version = 13;
         INSERT INTO sessions(id, source, source_id, title, started_at) VALUES ('history', 'codex', 'native', 'History', 1);
         INSERT INTO session_events(session_id, source, source_id, event_seq, kind, actor, attrs_json, created_at)
         VALUES ('history', 'codex', 'native', 0, 'tool_call', 'assistant', '{"input":"preserved"}', 1);"#,
    ).unwrap();
    let legacy = store.list_session_events_for_session("history").unwrap();
    assert_eq!(legacy.len(), 1);
    assert!(legacy[0].files.is_empty());
    assert_eq!(legacy[0].command_evidence_status, None);
    migrate_v14(&store.conn).unwrap();
    assert_eq!(
        store.list_session_events_for_session("history").unwrap()[0].command_evidence_status,
        None
    );
    init(&store.conn).unwrap();
    init(&store.conn).unwrap();
    let payload: String = store
        .conn
        .query_row(
            "SELECT attrs_json FROM session_events WHERE session_id = 'history'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(payload, r#"{"input":"preserved"}"#);
    assert_eq!(
        store.list_session_events_for_session("history").unwrap()[0].command_evidence_status,
        None
    );
    assert!(store.list_session_events_for_session("history").unwrap()[0].files.is_empty());
}

#[test]
fn migrate_v13_adds_structured_event_columns_to_existing_v12_db() {
    type EventRow = (
        i64,
        Option<i64>,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<String>,
        Option<String>,
        Option<String>,
        i64,
    );

    register_sqlite_vec();
    let conn = Connection::open_in_memory().unwrap();
    migrate_v1(&conn).unwrap();
    migrate_v2(&conn).unwrap();
    migrate_v3(&conn).unwrap();
    migrate_v4(&conn).unwrap();
    migrate_v5(&conn).unwrap();
    migrate_v6(&conn).unwrap();
    migrate_v7(&conn).unwrap();
    migrate_v8(&conn).unwrap();
    migrate_v9(&conn).unwrap();
    migrate_v10(&conn).unwrap();
    migrate_v11(&conn).unwrap();
    migrate_v12(&conn).unwrap();
    conn.execute_batch(
        "INSERT INTO sessions (id, source, source_id, title, started_at)
         VALUES ('s1', 'codex', 'source-1', 'existing', 1000);
         INSERT INTO session_events (
            session_id, source, source_id, event_seq, timestamp, kind, actor,
            name, status, target, message_seq, summary, source_path,
            source_event_id, attrs_json, parser_version, created_at
         ) VALUES (
            's1', 'codex', 'source-1', 7, 1234, 'file_write', 'assistant',
            'apply_patch', 'completed', 'src/lib.rs', 3, 'updated file',
            '/tmp/session.jsonl', 'line:7', '{\"path\":\"src/lib.rs\"}', 4, 2000
         );",
    )
    .unwrap();
    let before: EventRow = conn
        .query_row(
            "SELECT event_seq, timestamp, kind, actor, name, status, target,
                    message_seq, summary, source_path, source_event_id, parser_version
             FROM session_events WHERE session_id = 's1'",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                ))
            },
        )
        .unwrap();

    init(&conn).unwrap();

    assert_eq!(schema_version(&conn).unwrap(), SCHEMA_VERSION);
    let after: EventRow = conn
        .query_row(
            "SELECT event_seq, timestamp, kind, actor, name, status, target,
                    message_seq, summary, source_path, source_event_id, parser_version
             FROM session_events WHERE session_id = 's1'",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(after, before);
    let structured: (Option<String>, Option<bool>, Option<String>) = conn
        .query_row(
            "SELECT tool_call_id, is_meta, visibility FROM session_events WHERE session_id = 's1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(structured, (None, None, None));
    assert!(conn.execute("UPDATE session_events SET is_meta = 2", []).is_err());
    assert!(conn.execute("UPDATE session_events SET visibility = 'unknown'", []).is_err());

    init(&conn).unwrap();
    assert_eq!(schema_version(&conn).unwrap(), SCHEMA_VERSION);
}

#[test]
fn init_is_idempotent() {
    register_sqlite_vec();
    let conn = Connection::open_in_memory().unwrap();
    init(&conn).unwrap();
    let first = schema_version(&conn).unwrap();
    init(&conn).unwrap();
    assert_eq!(schema_version(&conn).unwrap(), first);
    assert_eq!(first, SCHEMA_VERSION);
}
