use rusqlite::Connection;

const SCHEMA_VERSION: i64 = 4;

#[allow(clippy::missing_transmute_annotations)]
pub fn register_sqlite_vec() {
    unsafe {
        rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
            sqlite_vec::sqlite3_vec_init as *const (),
        )));
    }
}

pub fn init(conn: &Connection) -> anyhow::Result<()> {
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

pub fn schema_version(conn: &Connection) -> anyhow::Result<i64> {
    conn.query_row("PRAGMA user_version", [], |row| row.get(0)).map_err(Into::into)
}

pub const fn current_schema_version() -> i64 {
    SCHEMA_VERSION
}
