use super::*;

#[test]
fn migrate_v8_adds_is_import_to_existing_v7_db() {
    register_sqlite_vec();
    let conn = Connection::open_in_memory().unwrap();
    for migrate in
        [migrate_v1, migrate_v2, migrate_v3, migrate_v4, migrate_v5, migrate_v6, migrate_v7]
    {
        migrate(&conn).unwrap();
    }
    conn.execute_batch(
        "INSERT INTO sessions (id, source, source_id, title, started_at)
         VALUES ('s1', 'codex', 'c1', 'existing', 0);",
    )
    .unwrap();

    init(&conn).unwrap();

    assert_eq!(schema_version(&conn).unwrap(), SCHEMA_VERSION);
    let is_import: i64 = conn
        .query_row("SELECT is_import FROM sessions WHERE id = 's1'", [], |row| row.get(0))
        .unwrap();
    assert_eq!(is_import, 0, "existing local sessions must default to is_import = 0");
}

#[test]
fn migrate_v10_purges_grok_usage_rows_from_existing_v9_db() {
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
    conn.execute_batch(
        "INSERT INTO sessions (id, source, source_id, title, started_at)
         VALUES
            ('grok-session', 'grok', 'grok-source', 'grok', 0),
            ('codex-session', 'codex', 'codex-source', 'codex', 0);
         INSERT INTO usage_events (
            session_id, source, source_id, event_key, event_seq, timestamp,
            model, provider, token_source, parser_version, created_at
         )
         VALUES
            ('grok-session', 'grok', 'grok-source', 'signals:context', 0, 1, 'grok-build', 'xai', 'observed', 3, 1),
            ('codex-session', 'codex', 'codex-source', 'usage', 0, 1, 'gpt', 'openai', 'observed', 1, 1);
         INSERT INTO usage_session_state (
            session_id, source, source_id, parser_version, source_updated_at, event_count, synced_at
         )
         VALUES
            ('grok-session', 'grok', 'grok-source', 3, 1, 1, 1),
            ('codex-session', 'codex', 'codex-source', 1, 1, 1, 1);",
    )
    .unwrap();

    assert_eq!(schema_version(&conn).unwrap(), 9);
    init(&conn).unwrap();

    assert_eq!(schema_version(&conn).unwrap(), SCHEMA_VERSION);
    let counts: (i64, i64, i64, i64) = conn
        .query_row(
            "SELECT
                (SELECT COUNT(*) FROM usage_events WHERE source = 'grok'),
                (SELECT COUNT(*) FROM usage_events WHERE source = 'codex'),
                (SELECT COUNT(*) FROM usage_session_state WHERE source = 'grok'),
                (SELECT COUNT(*) FROM usage_session_state WHERE source = 'codex')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(counts, (0, 1, 0, 1));
}

#[test]
fn migrate_v11_adds_topology_columns_and_table_to_existing_v10_db() {
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
    conn.execute_batch(
        "INSERT INTO sessions (id, source, source_id, title, started_at)
         VALUES ('s1', 'codex', 'c1', 'existing', 0);",
    )
    .unwrap();

    assert_eq!(schema_version(&conn).unwrap(), 10);
    init(&conn).unwrap();

    assert_eq!(schema_version(&conn).unwrap(), SCHEMA_VERSION);
    for col in ["thread_role", "metadata_parser_version"] {
        let sql = format!("SELECT {col} FROM sessions");
        conn.prepare(&sql)
            .unwrap_or_else(|e| panic!("column {col} missing after migrate_v11: {e}"));
    }
    let (role, version): (Option<String>, Option<i64>) = conn
        .query_row(
            "SELECT thread_role, metadata_parser_version FROM sessions WHERE id = 's1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(role, None, "existing sessions default to unknown role");
    assert_eq!(version, None);

    conn.execute(
        "INSERT INTO session_parent_links (session_id, relation, parent_source, parent_source_id)
         VALUES ('s1', 'spawn', 'codex', 'p1')",
        [],
    )
    .unwrap();
    conn.execute("DELETE FROM sessions WHERE id = 's1'", []).unwrap();
    let links: i64 =
        conn.query_row("SELECT COUNT(*) FROM session_parent_links", [], |row| row.get(0)).unwrap();
    assert_eq!(links, 0, "parent links cascade when their session is removed");
}
