use rusqlite::{Connection, OptionalExtension};

mod legacy;
use legacy::*;

const V12_SCHEMA_VERSION: i64 = 12;
pub(crate) const SCHEMA_VERSION: i64 = 17;

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
    if version < 15 {
        migrate_v15(conn)?;
    }
    if version < 16 {
        migrate_v16(conn)?;
    }
    if version < 17 {
        migrate_v17(conn)?;
    }
    Ok(())
}

fn migrate_v17(conn: &Connection) -> anyhow::Result<()> {
    let foreign_keys: bool = conn.query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
    conn.pragma_update(None, "foreign_keys", false)?;
    let result = (|| -> anyhow::Result<()> {
        let tx = conn.unchecked_transaction()?;
        if read_schema_version(&tx)? >= 17 {
            tx.commit()?;
            return Ok(());
        }
        tx.execute_batch(
            "CREATE TABLE sessions_v17 (
                id TEXT PRIMARY KEY,
                source TEXT NOT NULL,
                source_id TEXT NOT NULL,
                title TEXT NOT NULL,
                directory TEXT,
                started_at INTEGER NOT NULL,
                updated_at INTEGER,
                message_count INTEGER NOT NULL DEFAULT 0,
                entrypoint TEXT,
                custom_title TEXT,
                summary TEXT,
                duration_minutes INTEGER,
                source_file_path TEXT,
                is_import INTEGER NOT NULL DEFAULT 0,
                repo_remote TEXT,
                repo_slug TEXT,
                repo_name TEXT,
                thread_role TEXT CHECK (thread_role IN ('primary', 'subagent')),
                metadata_parser_version INTEGER
            );
            INSERT INTO sessions_v17
            SELECT id, source, source_id, title, directory, started_at, updated_at,
                   message_count, entrypoint, custom_title, summary, duration_minutes,
                   source_file_path, is_import, repo_remote, repo_slug, repo_name,
                   thread_role, metadata_parser_version FROM sessions;
            DROP TABLE sessions;
            ALTER TABLE sessions_v17 RENAME TO sessions;
            CREATE INDEX idx_sessions_source ON sessions(source, source_id);
            CREATE INDEX idx_sessions_started_at ON sessions(started_at);
            CREATE INDEX idx_sessions_directory ON sessions(directory);
            CREATE INDEX idx_sessions_repo_remote ON sessions(repo_remote);
            CREATE INDEX idx_sessions_repo_slug ON sessions(repo_slug);
            CREATE INDEX idx_sessions_repo_name ON sessions(repo_name);",
        )?;
        for table in ["usage_session_state", "event_session_state"] {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
                [table],
                |row| row.get(0),
            )?;
            if exists {
                tx.execute_batch(&format!(
                    "CREATE TABLE {table}_v17 (
                        session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
                        source TEXT NOT NULL,
                        source_id TEXT NOT NULL,
                        parser_version INTEGER NOT NULL,
                        source_updated_at INTEGER,
                        event_count INTEGER NOT NULL DEFAULT 0 CHECK (event_count >= 0),
                        synced_at INTEGER NOT NULL
                    );
                    INSERT INTO {table}_v17 SELECT * FROM {table};
                    DROP TABLE {table};
                    ALTER TABLE {table}_v17 RENAME TO {table};
                    CREATE INDEX idx_{table}_source ON {table}(source, source_id);"
                ))?;
            }
        }
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS native_bindings (
                source TEXT NOT NULL,
                source_id TEXT NOT NULL,
                session_id TEXT NOT NULL UNIQUE REFERENCES sessions(id) ON DELETE CASCADE,
                confirmed INTEGER NOT NULL CHECK (confirmed IN (0, 1)),
                PRIMARY KEY(source, source_id)
            );
            INSERT OR IGNORE INTO native_bindings
                SELECT source, source_id, id, NOT is_import FROM sessions;
            CREATE TABLE IF NOT EXISTS session_sync (
                session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
                sync_id TEXT NOT NULL UNIQUE,
                current_revision TEXT,
                alternative_versions INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS sync_aliases (
                sync_id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_sync_aliases_session ON sync_aliases(session_id);
            CREATE TABLE IF NOT EXISTS sync_revisions (
                digest TEXT PRIMARY KEY,
                sync_id TEXT NOT NULL,
                body BLOB NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_sync_revisions_identity ON sync_revisions(sync_id);
            CREATE TABLE IF NOT EXISTS sync_objects (
                digest TEXT PRIMARY KEY,
                body BLOB NOT NULL
            );
            CREATE TABLE IF NOT EXISTS hosts (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                revision INTEGER NOT NULL CHECK (revision > 0)
            );
            CREATE TABLE IF NOT EXISTS session_locations (
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                host_id TEXT NOT NULL REFERENCES hosts(id),
                directory TEXT,
                source_file_path TEXT,
                observed_at INTEGER NOT NULL,
                PRIMARY KEY(session_id, host_id)
            );
            PRAGMA user_version = 17;",
        )?;
        add_column_if_missing(
            &tx,
            "ALTER TABLE session_parent_links ADD COLUMN parent_sync_id TEXT",
        )?;
        let violation: Option<String> =
            tx.query_row("PRAGMA foreign_key_check", [], |row| row.get(0)).optional()?;
        anyhow::ensure!(violation.is_none(), "foreign key violation after identity migration");
        tx.commit()?;
        Ok(())
    })();
    let restored = conn.pragma_update(None, "foreign_keys", foreign_keys);
    result?;
    restored?;
    Ok(())
}

fn migrate_v16(conn: &Connection) -> anyhow::Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch("
        CREATE TABLE IF NOT EXISTS file_history_state (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            index_id TEXT NOT NULL
        );
        INSERT OR IGNORE INTO file_history_state VALUES (1, lower(hex(randomblob(16))));
        CREATE INDEX IF NOT EXISTS idx_event_files_remote_relative ON event_files(json_extract(evidence_json, '$.target.repo_remote'), json_extract(evidence_json, '$.target.repo_relative_path'), event_id);
        CREATE INDEX IF NOT EXISTS idx_event_files_root_relative ON event_files(json_extract(evidence_json, '$.target.repo_root'), json_extract(evidence_json, '$.target.repo_relative_path'), event_id);
        CREATE INDEX IF NOT EXISTS idx_event_files_absolute ON event_files(json_extract(evidence_json, '$.target.absolute_path'), event_id);
        ")?;
    if read_schema_version(&tx)? >= 15 {
        tx.execute_batch("PRAGMA user_version = 16;")?;
    }
    tx.commit()?;
    Ok(())
}

fn migrate_v15(conn: &Connection) -> anyhow::Result<()> {
    let tx = conn.unchecked_transaction()?;
    add_column_if_missing(
        &tx,
        "ALTER TABLE session_events ADD COLUMN command_evidence_status TEXT
         CHECK (command_evidence_status IN ('complete', 'unsupported', 'limit_exceeded')
                OR command_evidence_status IS NULL)",
    )?;
    if read_schema_version(&tx)? >= 14 {
        tx.execute_batch("PRAGMA user_version = 15;")?;
    }
    tx.commit()?;
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
mod legacy_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod trigram_tests;
