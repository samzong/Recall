use super::add_column_if_missing;
use rusqlite::Connection;

pub(super) fn migrate_v1(conn: &Connection) -> anyhow::Result<()> {
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

pub(super) fn migrate_v2(conn: &Connection) -> anyhow::Result<()> {
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

pub(super) fn migrate_v3(conn: &Connection) -> anyhow::Result<()> {
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

pub(super) fn migrate_v4(conn: &Connection) -> anyhow::Result<()> {
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

pub(super) fn migrate_v5(conn: &Connection) -> anyhow::Result<()> {
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

pub(super) fn migrate_v6(conn: &Connection) -> anyhow::Result<()> {
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

pub(super) fn migrate_v7(conn: &Connection) -> anyhow::Result<()> {
    add_column_if_missing(conn, "ALTER TABLE sessions ADD COLUMN source_file_path TEXT")?;
    conn.execute_batch("PRAGMA user_version = 7;")?;
    Ok(())
}

pub(super) fn migrate_v8(conn: &Connection) -> anyhow::Result<()> {
    add_column_if_missing(
        conn,
        "ALTER TABLE sessions ADD COLUMN is_import INTEGER NOT NULL DEFAULT 0",
    )?;
    conn.execute_batch("PRAGMA user_version = 8;")?;
    Ok(())
}

pub(super) fn migrate_v9(conn: &Connection) -> anyhow::Result<()> {
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

pub(super) fn migrate_v10(conn: &Connection) -> anyhow::Result<()> {
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

pub(super) fn migrate_v11(conn: &Connection) -> anyhow::Result<()> {
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
