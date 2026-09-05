use rusqlite::{Connection, OptionalExtension};

const V12_SCHEMA_VERSION: i64 = 12;
const SCHEMA_VERSION: i64 = 14;

#[allow(clippy::missing_transmute_annotations)]
pub(crate) fn register_sqlite_vec() {
    unsafe {
        rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
            sqlite_vec::sqlite3_vec_init as *const (),
        )));
    }
}

pub(crate) fn init(conn: &Connection) -> anyhow::Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < 1 {
        migrate_v1(conn)?;
    }
    if version < 2 {
        migrate_v2(conn)?;
    }
    if version < 3 {
        migrate_v3(conn)?;
    }
    if version < 4 {
        migrate_v4(conn)?;
    }
    if version < 5 {
        migrate_v5(conn)?;
    }
    if version < 6 {
        migrate_v6(conn)?;
    }
    if version < 7 {
        migrate_v7(conn)?;
    }
    if version < 8 {
        migrate_v8(conn)?;
    }
    if version < 9 {
        migrate_v9(conn)?;
    }
    if version < 10 {
        migrate_v10(conn)?;
    }
    if version < 11 {
        migrate_v11(conn)?;
    }
    if version < V12_SCHEMA_VERSION || fts_needs_trigram_rebuild(conn)? {
        migrate_v12(conn)?;
    }
    let version = read_schema_version(conn)?;
    if version < 13 {
        migrate_v13(conn)?;
    }
    if version < 14 {
        migrate_v14(conn)?;
    }
    Ok(())
}

fn migrate_v14(conn: &Connection) -> anyhow::Result<()> {
    let version = read_schema_version(conn)?;
    let version_update = if version >= 13 { "PRAGMA user_version = 14;" } else { "" };
    conn.execute_batch(&format!(
        "BEGIN IMMEDIATE;
         CREATE TABLE IF NOT EXISTS event_files (
             event_id INTEGER NOT NULL REFERENCES session_events(id) ON DELETE CASCADE,
             position INTEGER NOT NULL,
             path TEXT NOT NULL,
             evidence_json TEXT NOT NULL,
             PRIMARY KEY(event_id, position)
         );
         CREATE INDEX IF NOT EXISTS idx_event_files_path ON event_files(path);
         {version_update}
         COMMIT;",
    ))?;
    Ok(())
}

fn migrate_v1(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS sessions (
            id TEXT PRIMARY KEY,
            source TEXT NOT NULL,
            source_id TEXT NOT NULL,
            title TEXT NOT NULL,
            directory TEXT,
            started_at INTEGER NOT NULL,
            updated_at INTEGER,
            message_count INTEGER NOT NULL DEFAULT 0,
            entrypoint TEXT,
            UNIQUE(source, source_id)
        );

        CREATE INDEX IF NOT EXISTS idx_sessions_source ON sessions(source);
        CREATE INDEX IF NOT EXISTS idx_sessions_started_at ON sessions(started_at);
        CREATE INDEX IF NOT EXISTS idx_sessions_directory ON sessions(directory);

        CREATE TABLE IF NOT EXISTS messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
            role TEXT NOT NULL,
            content TEXT NOT NULL,
            timestamp INTEGER,
            seq INTEGER NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id, seq);

        CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
            content,
            content=messages,
            content_rowid=id,
            tokenize='unicode61'
        );

        CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
            INSERT INTO messages_fts(rowid, content) VALUES (new.id, new.content);
        END;

        CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
            INSERT INTO messages_fts(messages_fts, rowid, content) VALUES('delete', old.id, old.content);
        END;

        CREATE VIRTUAL TABLE IF NOT EXISTS message_vec USING vec0(
            message_id INTEGER PRIMARY KEY,
            embedding float[384]
        );

        CREATE TABLE IF NOT EXISTS session_embedding_state (
            session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
            status TEXT NOT NULL,
            units_total INTEGER NOT NULL DEFAULT 0,
            units_done INTEGER NOT NULL DEFAULT 0,
            started_at INTEGER,
            finished_at INTEGER,
            last_error TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_session_embedding_status
            ON session_embedding_state(status);

        CREATE TABLE IF NOT EXISTS background_job_state (
            job TEXT PRIMARY KEY,
            phase TEXT NOT NULL,
            detail TEXT,
            updated_at INTEGER NOT NULL
        );

        PRAGMA user_version = 1;
        ",
    )?;
    Ok(())
}

fn migrate_v2(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS usage_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
            source TEXT NOT NULL,
            source_id TEXT NOT NULL,
            event_key TEXT NOT NULL,
            event_seq INTEGER NOT NULL,
            message_seq INTEGER,
            timestamp INTEGER NOT NULL,
            model TEXT NOT NULL DEFAULT 'unknown',
            provider TEXT NOT NULL DEFAULT 'unknown',
            input_tokens INTEGER NOT NULL DEFAULT 0 CHECK (input_tokens >= 0),
            output_tokens INTEGER NOT NULL DEFAULT 0 CHECK (output_tokens >= 0),
            cache_read_tokens INTEGER NOT NULL DEFAULT 0 CHECK (cache_read_tokens >= 0),
            cache_write_tokens INTEGER NOT NULL DEFAULT 0 CHECK (cache_write_tokens >= 0),
            reasoning_tokens INTEGER NOT NULL DEFAULT 0 CHECK (reasoning_tokens >= 0),
            token_source TEXT NOT NULL
                CHECK (token_source IN ('observed', 'derived', 'estimated')),
            parser_version INTEGER NOT NULL DEFAULT 1,
            source_path TEXT,
            raw_usage_json TEXT,
            created_at INTEGER NOT NULL,
            UNIQUE(session_id, event_key)
        );

        CREATE INDEX IF NOT EXISTS idx_usage_events_session
            ON usage_events(session_id);
        CREATE INDEX IF NOT EXISTS idx_usage_events_time
            ON usage_events(timestamp);
        CREATE INDEX IF NOT EXISTS idx_usage_events_source_time
            ON usage_events(source, timestamp);
        CREATE INDEX IF NOT EXISTS idx_usage_events_model_time
            ON usage_events(model, timestamp);

        PRAGMA user_version = 2;
        ",
    )?;
    Ok(())
}

fn migrate_v3(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS usage_session_state (
            session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
            source TEXT NOT NULL,
            source_id TEXT NOT NULL,
            parser_version INTEGER NOT NULL,
            source_updated_at INTEGER,
            event_count INTEGER NOT NULL DEFAULT 0 CHECK (event_count >= 0),
            synced_at INTEGER NOT NULL,
            UNIQUE(source, source_id)
        );

        CREATE INDEX IF NOT EXISTS idx_usage_session_state_source
            ON usage_session_state(source, source_id);

        PRAGMA user_version = 3;
        ",
    )?;
    Ok(())
}

fn migrate_v4(conn: &Connection) -> anyhow::Result<()> {
    let has_column = |name: &str| -> anyhow::Result<bool> {
        let mut stmt = conn.prepare("PRAGMA table_info(usage_events)")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
        for row in rows {
            if row? == name {
                return Ok(true);
            }
        }
        Ok(false)
    };

    if has_column("cost_usd")? {
        conn.execute_batch("ALTER TABLE usage_events DROP COLUMN cost_usd;")?;
    }
    if has_column("cost_source")? {
        conn.execute_batch("ALTER TABLE usage_events DROP COLUMN cost_source;")?;
    }
    conn.execute_batch("PRAGMA user_version = 4;")?;
    Ok(())
}

fn migrate_v5(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS session_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
            source TEXT NOT NULL,
            source_id TEXT NOT NULL,
            event_seq INTEGER NOT NULL,
            timestamp INTEGER,
            kind TEXT NOT NULL,
            actor TEXT NOT NULL,
            name TEXT,
            status TEXT,
            target TEXT,
            message_seq INTEGER,
            summary TEXT,
            source_path TEXT,
            source_event_id TEXT,
            attrs_json TEXT,
            parser_version INTEGER NOT NULL DEFAULT 1,
            created_at INTEGER NOT NULL,
            UNIQUE(session_id, event_seq)
        );

        CREATE INDEX IF NOT EXISTS idx_session_events_session
            ON session_events(session_id, event_seq);
        CREATE INDEX IF NOT EXISTS idx_session_events_kind_time
            ON session_events(kind, timestamp);
        CREATE INDEX IF NOT EXISTS idx_session_events_source_time
            ON session_events(source, timestamp);
        CREATE INDEX IF NOT EXISTS idx_session_events_target
            ON session_events(target);

        CREATE TABLE IF NOT EXISTS event_session_state (
            session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
            source TEXT NOT NULL,
            source_id TEXT NOT NULL,
            parser_version INTEGER NOT NULL,
            source_updated_at INTEGER,
            event_count INTEGER NOT NULL DEFAULT 0 CHECK (event_count >= 0),
            synced_at INTEGER NOT NULL,
            UNIQUE(source, source_id)
        );

        CREATE INDEX IF NOT EXISTS idx_event_session_state_source
            ON event_session_state(source, source_id);

        PRAGMA user_version = 5;
        ",
    )?;
    Ok(())
}

fn migrate_v6(conn: &Connection) -> anyhow::Result<()> {
    for stmt in [
        "ALTER TABLE sessions ADD COLUMN custom_title TEXT",
        "ALTER TABLE sessions ADD COLUMN summary TEXT",
        "ALTER TABLE sessions ADD COLUMN duration_minutes INTEGER",
    ] {
        add_column_if_missing(conn, stmt)?;
    }
    conn.execute_batch("PRAGMA user_version = 6;")?;
    Ok(())
}

fn migrate_v7(conn: &Connection) -> anyhow::Result<()> {
    add_column_if_missing(conn, "ALTER TABLE sessions ADD COLUMN source_file_path TEXT")?;
    conn.execute_batch("PRAGMA user_version = 7;")?;
    Ok(())
}

fn migrate_v8(conn: &Connection) -> anyhow::Result<()> {
    add_column_if_missing(
        conn,
        "ALTER TABLE sessions ADD COLUMN is_import INTEGER NOT NULL DEFAULT 0",
    )?;
    conn.execute_batch("PRAGMA user_version = 8;")?;
    Ok(())
}

fn migrate_v9(conn: &Connection) -> anyhow::Result<()> {
    for stmt in [
        "ALTER TABLE sessions ADD COLUMN repo_remote TEXT",
        "ALTER TABLE sessions ADD COLUMN repo_slug TEXT",
        "ALTER TABLE sessions ADD COLUMN repo_name TEXT",
    ] {
        add_column_if_missing(conn, stmt)?;
    }
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_sessions_repo_remote ON sessions(repo_remote);
         CREATE INDEX IF NOT EXISTS idx_sessions_repo_slug ON sessions(repo_slug);
         CREATE INDEX IF NOT EXISTS idx_sessions_repo_name ON sessions(repo_name);
         PRAGMA user_version = 9;",
    )?;
    Ok(())
}

fn migrate_v10(conn: &Connection) -> anyhow::Result<()> {
    let table_exists = |name: &str| -> anyhow::Result<bool> {
        let exists: i64 = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
            [name],
            |row| row.get(0),
        )?;
        Ok(exists != 0)
    };
    if table_exists("usage_events")? {
        conn.execute("DELETE FROM usage_events WHERE source = 'grok'", [])?;
    }
    if table_exists("usage_session_state")? {
        conn.execute("DELETE FROM usage_session_state WHERE source = 'grok'", [])?;
    }
    conn.execute_batch("PRAGMA user_version = 10;")?;
    Ok(())
}

fn migrate_v11(conn: &Connection) -> anyhow::Result<()> {
    add_column_if_missing(
        conn,
        "ALTER TABLE sessions ADD COLUMN thread_role TEXT
             CHECK (thread_role IN ('primary', 'subagent'))",
    )?;
    add_column_if_missing(conn, "ALTER TABLE sessions ADD COLUMN metadata_parser_version INTEGER")?;
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS session_parent_links (
            session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
            relation TEXT NOT NULL CHECK (relation IN ('spawn', 'fork', 'resume')),
            parent_source TEXT NOT NULL,
            parent_source_id TEXT NOT NULL,
            PRIMARY KEY (session_id, relation, parent_source, parent_source_id)
        );

        CREATE INDEX IF NOT EXISTS idx_session_parent_links_parent
            ON session_parent_links(parent_source, parent_source_id);

        PRAGMA user_version = 11;
        ",
    )?;
    Ok(())
}

fn fts_needs_trigram_rebuild(conn: &Connection) -> anyhow::Result<bool> {
    let has_messages: i64 = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'messages')",
        [],
        |row| row.get(0),
    )?;
    if has_messages == 0 {
        return Ok(false);
    }
    let has_flag = has_trigram_message_flag(conn)?;
    let trigger_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type = 'trigger'
           AND name IN ('messages_trigram_ai', 'messages_trigram_ad')
           AND instr(sql, 'trigram_indexed IS NULL') > 0",
        [],
        |row| row.get(0),
    )?;
    Ok(!has_flag || trigger_count != 2 || !has_trigram_fts(conn)?)
}

pub(crate) fn has_trigram_message_flag(conn: &Connection) -> anyhow::Result<bool> {
    let has_flag: i64 = conn.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM pragma_table_info('messages') WHERE name = 'trigram_indexed'
        )",
        [],
        |row| row.get(0),
    )?;
    Ok(has_flag != 0)
}

pub(crate) fn has_trigram_fts(conn: &Connection) -> anyhow::Result<bool> {
    let fts_sql: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'messages_fts_trigram'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    Ok(fts_sql.is_some_and(|sql| sql.contains("trigram")))
}

fn migrate_v12(conn: &Connection) -> anyhow::Result<()> {
    let file_backed = conn.path().is_some_and(|path| !path.is_empty());
    migrate_v12_with_lock(conn, file_backed, crate::utils::try_acquire_worker_lock)
}

fn migrate_v12_with_lock<T>(
    conn: &Connection,
    file_backed: bool,
    acquire_lock: impl FnOnce() -> anyhow::Result<Option<T>>,
) -> anyhow::Result<()> {
    if !fts_needs_trigram_rebuild(conn)? {
        if read_schema_version(conn)? < V12_SCHEMA_VERSION {
            conn.execute_batch("PRAGMA user_version = 12;")?;
        }
        return Ok(());
    }

    let _lock = if file_backed {
        let Some(lock) = acquire_lock()? else {
            eprintln!(
                "Search index upgrade deferred: background indexing is running; will retry on next launch."
            );
            return Ok(());
        };
        Some(lock)
    } else {
        None
    };

    if !fts_needs_trigram_rebuild(conn)? {
        if read_schema_version(conn)? < V12_SCHEMA_VERSION {
            conn.execute_batch("PRAGMA user_version = 12;")?;
        }
        return Ok(());
    }

    eprintln!("Upgrading search index (one-time; may take a minute on large databases)...");
    let started = std::time::Instant::now();
    let version = read_schema_version(conn)?;
    let version_update =
        if version < V12_SCHEMA_VERSION { "PRAGMA user_version = 12;" } else { "" };
    add_column_if_missing(
        conn,
        "ALTER TABLE messages ADD COLUMN trigram_indexed INTEGER
         CHECK (trigram_indexed IN (0, 1))",
    )?;
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    if let Err(err) = rebuild_trigram_fts(conn, version_update) {
        let _ = conn.execute_batch("ROLLBACK;");
        return Err(err);
    }
    if file_backed {
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        eprintln!("Search index upgraded in {:.0?}.", started.elapsed());
    }
    Ok(())
}

fn migrate_v13(conn: &Connection) -> anyhow::Result<()> {
    let version = read_schema_version(conn)?;
    for stmt in [
        "ALTER TABLE session_events ADD COLUMN tool_call_id TEXT",
        "ALTER TABLE session_events ADD COLUMN is_meta INTEGER
         CHECK (is_meta IN (0, 1) OR is_meta IS NULL)",
        "ALTER TABLE session_events ADD COLUMN visibility TEXT
         CHECK (visibility IN ('visible', 'hidden', 'inactive') OR visibility IS NULL)",
    ] {
        add_column_if_missing(conn, stmt)?;
    }
    if version >= V12_SCHEMA_VERSION {
        conn.execute_batch("PRAGMA user_version = 13;")?;
    }
    Ok(())
}

fn rebuild_trigram_fts(conn: &Connection, version_update: &str) -> anyhow::Result<()> {
    backfill_trigram_flags(conn)?;
    conn.execute_batch(&format!(
        "
        DROP TRIGGER IF EXISTS messages_trigram_ai;
        DROP TRIGGER IF EXISTS messages_trigram_ad;
        DROP TABLE IF EXISTS messages_fts_trigram;

        CREATE VIRTUAL TABLE messages_fts_trigram USING fts5(
            content,
            content=messages,
            content_rowid=id,
            tokenize='trigram remove_diacritics 1'
        );

        CREATE TRIGGER messages_trigram_ai AFTER INSERT ON messages
        WHEN new.trigram_indexed = 1
          OR new.trigram_indexed IS NULL BEGIN
            INSERT INTO messages_fts_trigram(rowid, content) VALUES (new.id, new.content);
        END;

        CREATE TRIGGER messages_trigram_ad AFTER DELETE ON messages
        WHEN old.trigram_indexed = 1
          OR old.trigram_indexed IS NULL BEGIN
            INSERT INTO messages_fts_trigram(messages_fts_trigram, rowid, content)
            VALUES('delete', old.id, old.content);
        END;

        INSERT INTO messages_fts_trigram(rowid, content)
        SELECT id, content FROM messages
        WHERE trigram_indexed = 1;

        {version_update}

        COMMIT;
        ",
    ))?;
    Ok(())
}

fn backfill_trigram_flags(conn: &Connection) -> anyhow::Result<()> {
    let flags: Vec<(i64, bool)> = {
        let mut stmt =
            conn.prepare("SELECT id, content FROM messages WHERE trigram_indexed IS NULL")?;
        let rows = stmt.query_map([], |row| {
            let id: i64 = row.get(0)?;
            let needs = crate::utils::text_needs_trigram(row.get_ref(1)?.as_str()?);
            Ok((id, needs))
        })?;
        rows.collect::<Result<_, _>>()?
    };
    let mut update = conn.prepare("UPDATE messages SET trigram_indexed = ?2 WHERE id = ?1")?;
    for (id, needs) in flags {
        update.execute(rusqlite::params![id, needs])?;
    }
    Ok(())
}

fn add_column_if_missing(conn: &Connection, stmt: &str) -> anyhow::Result<()> {
    if let Err(err) = conn.execute(stmt, []) {
        let msg = err.to_string();
        if !msg.contains("duplicate column name") {
            return Err(err.into());
        }
    }
    Ok(())
}

fn read_schema_version(conn: &Connection) -> anyhow::Result<i64> {
    conn.query_row("PRAGMA user_version", [], |row| row.get(0)).map_err(Into::into)
}

#[cfg(test)]
pub(crate) fn schema_version(conn: &Connection) -> anyhow::Result<i64> {
    read_schema_version(conn)
}

pub(crate) const fn current_schema_version() -> i64 {
    SCHEMA_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::store::Store;
    use crate::types::{Message, Role};

    #[test]
    fn v14_preserves_existing_events_and_is_idempotent() {
        register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        store.conn.execute_batch(
            r#"DROP TABLE event_files;
             PRAGMA user_version = 13;
             INSERT INTO sessions(id, source, source_id, title, started_at) VALUES ('history', 'codex', 'native', 'History', 1);
             INSERT INTO session_events(session_id, source, source_id, event_seq, kind, actor, attrs_json, created_at)
             VALUES ('history', 'codex', 'native', 0, 'tool_call', 'assistant', '{"input":"preserved"}', 1);"#,
        ).unwrap();
        let legacy = store.list_session_events_for_session("history").unwrap();
        assert_eq!(legacy.len(), 1);
        assert!(legacy[0].files.is_empty());
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
        assert!(store.list_session_events_for_session("history").unwrap()[0].files.is_empty());
    }

    #[test]
    fn migrate_v6_adds_metadata_columns_to_existing_v5_db() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY, source TEXT NOT NULL, source_id TEXT NOT NULL,
                title TEXT NOT NULL, directory TEXT, started_at INTEGER NOT NULL,
                updated_at INTEGER, message_count INTEGER NOT NULL DEFAULT 0,
                entrypoint TEXT, UNIQUE(source, source_id)
            );
            PRAGMA user_version = 5;",
        )
        .unwrap();
        migrate_v5(&conn).unwrap();

        init(&conn).unwrap();

        assert_eq!(schema_version(&conn).unwrap(), SCHEMA_VERSION);
        for col in ["custom_title", "summary", "duration_minutes"] {
            let sql = format!("SELECT {col} FROM sessions");
            conn.prepare(&sql)
                .unwrap_or_else(|e| panic!("column {col} missing after migrate_v6: {e}"));
        }
    }

    #[test]
    fn migrate_v7_adds_source_file_path_to_existing_v6_db() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY, source TEXT NOT NULL, source_id TEXT NOT NULL,
                title TEXT NOT NULL, directory TEXT, started_at INTEGER NOT NULL,
                updated_at INTEGER, message_count INTEGER NOT NULL DEFAULT 0,
                entrypoint TEXT, custom_title TEXT, summary TEXT,
                duration_minutes INTEGER, UNIQUE(source, source_id)
            );
            PRAGMA user_version = 6;",
        )
        .unwrap();
        migrate_v5(&conn).unwrap();
        conn.execute_batch("PRAGMA user_version = 6;").unwrap();

        init(&conn).unwrap();

        assert_eq!(schema_version(&conn).unwrap(), SCHEMA_VERSION);
        conn.prepare("SELECT source_file_path FROM sessions")
            .unwrap_or_else(|e| panic!("column source_file_path missing after migrate_v7: {e}"));
    }

    #[test]
    fn migrate_v8_adds_is_import_to_existing_v7_db() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY, source TEXT NOT NULL, source_id TEXT NOT NULL,
                title TEXT NOT NULL, directory TEXT, started_at INTEGER NOT NULL,
                updated_at INTEGER, message_count INTEGER NOT NULL DEFAULT 0,
                entrypoint TEXT, custom_title TEXT, summary TEXT,
                duration_minutes INTEGER, source_file_path TEXT, UNIQUE(source, source_id)
            );
            INSERT INTO sessions (id, source, source_id, title, started_at)
            VALUES ('s1', 'codex', 'c1', 'existing', 0);
            PRAGMA user_version = 7;",
        )
        .unwrap();
        migrate_v5(&conn).unwrap();
        conn.execute_batch("PRAGMA user_version = 7;").unwrap();

        init(&conn).unwrap();

        assert_eq!(schema_version(&conn).unwrap(), SCHEMA_VERSION);
        let is_import: i64 = conn
            .query_row("SELECT is_import FROM sessions WHERE id = 's1'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(is_import, 0, "existing local sessions must default to is_import = 0");
    }

    #[test]
    fn migrate_v9_adds_repo_identity_to_existing_v8_db() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY, source TEXT NOT NULL, source_id TEXT NOT NULL,
                title TEXT NOT NULL, directory TEXT, started_at INTEGER NOT NULL,
                updated_at INTEGER, message_count INTEGER NOT NULL DEFAULT 0,
                entrypoint TEXT, custom_title TEXT, summary TEXT,
                duration_minutes INTEGER, source_file_path TEXT,
                is_import INTEGER NOT NULL DEFAULT 0, UNIQUE(source, source_id)
            );
            INSERT INTO sessions (id, source, source_id, title, started_at)
            VALUES ('s1', 'codex', 'c1', 'existing', 0);
            PRAGMA user_version = 8;",
        )
        .unwrap();
        migrate_v5(&conn).unwrap();
        conn.execute_batch("PRAGMA user_version = 8;").unwrap();

        init(&conn).unwrap();

        assert_eq!(schema_version(&conn).unwrap(), SCHEMA_VERSION);
        for col in ["repo_remote", "repo_slug", "repo_name"] {
            let sql = format!("SELECT {col} FROM sessions");
            conn.prepare(&sql)
                .unwrap_or_else(|e| panic!("column {col} missing after migrate_v9: {e}"));
        }
        let repo_remote: Option<String> = conn
            .query_row("SELECT repo_remote FROM sessions WHERE id = 's1'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(repo_remote, None);
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
        let links: i64 = conn
            .query_row("SELECT COUNT(*) FROM session_parent_links", [], |row| row.get(0))
            .unwrap();
        assert_eq!(links, 0, "parent links cascade when their session is removed");
    }

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

        migrate_v12_with_lock(&store.conn, true, || Ok::<Option<()>, anyhow::Error>(Some(())))
            .unwrap();
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
    fn migrate_v11_rejects_invalid_thread_role() {
        register_sqlite_vec();
        let conn = Connection::open_in_memory().unwrap();
        init(&conn).unwrap();
        let err = conn.execute(
            "INSERT INTO sessions (id, source, source_id, title, started_at, thread_role)
             VALUES ('s1', 'codex', 'c1', 't', 0, 'bogus')",
            [],
        );
        assert!(err.is_err(), "thread_role CHECK must reject values outside primary/subagent");
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
}
