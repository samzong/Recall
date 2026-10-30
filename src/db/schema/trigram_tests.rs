use super::*;
use crate::db::store::Store;
use crate::types::{Message, Role};

#[test]
fn init_adds_trigram_fts_even_when_user_version_is_ahead() {
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
    conn.execute_batch(
        "INSERT INTO sessions (id, source, source_id, title, started_at)
         VALUES ('s1', 'cursor', 'c1', 'usage audit', 0);
         PRAGMA user_version = 13;",
    )
    .unwrap();
    let content = "\u{6211}\u{611f}\u{89c9}\u{8fd9}\u{4e2a}\u{7edf}\u{8ba1}\u{7684}\u{4e0d}\u{51c6}\u{786e}\u{9700}\u{8981}\u{590d}\u{67e5}";
    let query = "\u{7edf}\u{8ba1}\u{7684}\u{4e0d}\u{51c6}\u{786e}";
    conn.execute(
        "INSERT INTO messages (session_id, role, content, seq) VALUES ('s1', 'user', ?1, 0)",
        [content],
    )
    .unwrap();

    let before: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH ?1",
            [format!("\"{query}\"")],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(before, 0, "unicode61 cannot match a mid-clause CJK substring");

    init(&conn).unwrap();

    let after: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM messages_fts_trigram WHERE messages_fts_trigram MATCH ?1",
            [format!("\"{query}\"")],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(after, 1, "trigram rebuild must re-index existing rows for substring match");

    init(&conn).unwrap();
    let stable: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM messages_fts_trigram WHERE messages_fts_trigram MATCH ?1",
            [format!("\"{query}\"")],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stable, 1, "ensure pass must be idempotent on an already-trigram index");

    conn.execute("DELETE FROM sessions WHERE id = 's1'", []).unwrap();
    let removed: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM messages_fts_trigram WHERE messages_fts_trigram MATCH ?1",
            [format!("\"{query}\"")],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(removed, 0, "trigram index must delete rows with their messages");
}

#[test]
fn init_recreates_missing_trigram_fts_and_reindexes_existing_messages() {
    register_sqlite_vec();
    let conn = Connection::open_in_memory().unwrap();
    init(&conn).unwrap();
    conn.execute_batch(
        "INSERT INTO sessions (id, source, source_id, title, started_at)
         VALUES ('s1', 'cursor', 'c1', 'usage audit', 0);
         INSERT INTO messages (session_id, role, content, seq)
         VALUES ('s1', 'assistant', 'context cache', 1);",
    )
    .unwrap();
    let content = "\u{6211}\u{611f}\u{89c9}\u{8fd9}\u{4e2a}\u{7edf}\u{8ba1}\u{7684}\u{4e0d}\u{51c6}\u{786e}\u{9700}\u{8981}\u{590d}\u{67e5}";
    let query = "\u{7edf}\u{8ba1}\u{7684}\u{4e0d}\u{51c6}\u{786e}";
    conn.execute(
        "INSERT INTO messages (session_id, role, content, seq) VALUES ('s1', 'user', ?1, 0)",
        [content],
    )
    .unwrap();
    let old_write_hits: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM messages_fts_trigram
             WHERE messages_fts_trigram MATCH ?1",
            [format!("\"{query}\"")],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(old_write_hits, 1, "pre-v12 inserts must remain trigram searchable");
    let old_latin_hits: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM messages_fts_trigram
             WHERE messages_fts_trigram MATCH 'context'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(old_latin_hits, 1, "pre-v12 Latin inserts must remain trigram searchable");
    conn.execute_batch("DROP TABLE messages_fts_trigram;").unwrap();

    init(&conn).unwrap();

    let hits: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM messages_fts_trigram WHERE messages_fts_trigram MATCH ?1",
            [format!("\"{query}\"")],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(hits, 1, "a missing FTS table must be recreated and reindexed on open");
    let latin_hits: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM messages_fts_trigram
             WHERE messages_fts_trigram MATCH 'context'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(latin_hits, 0, "rebuild must index only CJK legacy rows");
}

#[test]
fn migrate_v12_retries_after_worker_lock_deferral() {
    register_sqlite_vec();
    let db = tempfile::NamedTempFile::new().unwrap();
    let conn = Connection::open(db.path()).unwrap();
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

    migrate_v12_with_lock(&conn, true, || Ok::<Option<()>, anyhow::Error>(None)).unwrap();
    assert_eq!(schema_version(&conn).unwrap(), 11);
    assert!(!has_trigram_fts(&conn).unwrap());

    migrate_v13(&conn).unwrap();
    assert_eq!(schema_version(&conn).unwrap(), 11);
    let structured_columns: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('session_events')
             WHERE name IN ('tool_call_id', 'is_meta', 'visibility')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(structured_columns, 3);

    conn.execute_batch(
        "INSERT INTO sessions (id, source, source_id, title, started_at)
         VALUES ('s1', 'cursor', 'c1', 'usage audit', 0);",
    )
    .unwrap();
    let store = Store { trigram_message_flag: has_trigram_message_flag(&conn).unwrap(), conn };
    let content = "\u{6211}\u{611f}\u{89c9}\u{8fd9}\u{4e2a}\u{7edf}\u{8ba1}\u{7684}\u{4e0d}\u{51c6}\u{786e}\u{9700}\u{8981}\u{590d}\u{67e5}";
    let query = "\u{7edf}\u{8ba1}\u{7684}\u{4e0d}\u{51c6}\u{786e}";
    store
        .insert_messages(&[Message {
            session_id: "s1".to_string(),
            role: Role::User,
            content: content.to_string(),
            timestamp: None,
            seq: 0,
        }])
        .unwrap();

    migrate_v12_with_lock(&store.conn, true, || Ok::<Option<()>, anyhow::Error>(Some(()))).unwrap();
    assert_eq!(schema_version(&store.conn).unwrap(), V12_SCHEMA_VERSION);
    init(&store.conn).unwrap();
    assert_eq!(schema_version(&store.conn).unwrap(), SCHEMA_VERSION);
    assert!(has_trigram_fts(&store.conn).unwrap());
    let hits: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM messages_fts_trigram
             WHERE messages_fts_trigram MATCH ?1",
            [format!("\"{query}\"")],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(hits, 1, "messages written during deferral must join the rebuilt index");
}
