use rusqlite::{Connection, Transaction, TransactionBehavior};

pub(crate) const USAGE_INDEX_JOB: &str = "usage_index";
pub(crate) const SKILL_USAGE_JOB: &str = "skill_usage";
pub(crate) const REVISION_COMPRESSION_JOB: &str = "revision_compression";

pub(super) fn migrate_v21(conn: &Connection) -> anyhow::Result<()> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    if super::read_schema_version(&tx)? >= 21 {
        return Ok(());
    }

    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS usage_event_ownership (
            event_id INTEGER PRIMARY KEY,
            session_id TEXT NOT NULL,
            dedup_group TEXT,
            timestamp INTEGER NOT NULL,
            event_seq INTEGER NOT NULL,
            event_key TEXT NOT NULL,
            ownership INTEGER NOT NULL CHECK (ownership IN (1, 2, 3))
         );
         CREATE INDEX IF NOT EXISTS idx_usage_ownership_session
            ON usage_event_ownership(session_id, ownership);
         CREATE INDEX IF NOT EXISTS idx_usage_ownership_group
            ON usage_event_ownership(dedup_group, timestamp, event_seq, session_id, event_key)
            WHERE dedup_group IS NOT NULL;

         CREATE TABLE IF NOT EXISTS usage_rollups (
            bucket_ms INTEGER NOT NULL,
            source TEXT NOT NULL,
            provider TEXT NOT NULL,
            model TEXT NOT NULL,
            token_source TEXT NOT NULL,
            session_id TEXT NOT NULL,
            events INTEGER NOT NULL CHECK (events > 0),
            input_tokens INTEGER NOT NULL,
            output_tokens INTEGER NOT NULL,
            cache_read_tokens INTEGER NOT NULL,
            cache_write_tokens INTEGER NOT NULL,
            reasoning_tokens INTEGER NOT NULL,
            PRIMARY KEY (bucket_ms, source, provider, model, token_source, session_id)
         ) WITHOUT ROWID;
         CREATE INDEX IF NOT EXISTS idx_usage_rollups_session
            ON usage_rollups(session_id);

         CREATE TABLE IF NOT EXISTS usage_dirty_sessions (
            session_id TEXT PRIMARY KEY
         ) WITHOUT ROWID;
         CREATE TABLE IF NOT EXISTS usage_dirty_groups (
            dedup_group TEXT PRIMARY KEY
         ) WITHOUT ROWID;

         CREATE TRIGGER IF NOT EXISTS usage_events_dirty_ai AFTER INSERT ON usage_events BEGIN
            INSERT INTO usage_dirty_sessions(session_id) VALUES (NEW.session_id)
            ON CONFLICT DO NOTHING;
         END;
         CREATE TRIGGER IF NOT EXISTS usage_events_dirty_au AFTER UPDATE ON usage_events BEGIN
            INSERT INTO usage_dirty_sessions(session_id) VALUES (OLD.session_id)
            ON CONFLICT DO NOTHING;
            INSERT INTO usage_dirty_sessions(session_id) VALUES (NEW.session_id)
            ON CONFLICT DO NOTHING;
            INSERT INTO usage_dirty_groups(dedup_group)
                SELECT dedup_group FROM usage_event_ownership
                WHERE event_id = OLD.id AND dedup_group IS NOT NULL
                ON CONFLICT DO NOTHING;
            DELETE FROM usage_event_ownership WHERE event_id = OLD.id;
         END;
         CREATE TRIGGER IF NOT EXISTS usage_events_dirty_ad AFTER DELETE ON usage_events BEGIN
            INSERT INTO usage_dirty_sessions(session_id) VALUES (OLD.session_id)
            ON CONFLICT DO NOTHING;
            INSERT INTO usage_dirty_groups(dedup_group)
                SELECT dedup_group FROM usage_event_ownership
                WHERE event_id = OLD.id AND dedup_group IS NOT NULL
                ON CONFLICT DO NOTHING;
            DELETE FROM usage_event_ownership WHERE event_id = OLD.id;
         END;

         CREATE TABLE IF NOT EXISTS session_skill_usage (
            session_id TEXT NOT NULL,
            source TEXT NOT NULL,
            raw_skill TEXT NOT NULL,
            signal TEXT NOT NULL CHECK (signal IN ('read_file', 'skill_tool', 'candidate_only')),
            max_timestamp INTEGER,
            has_null_timestamp INTEGER NOT NULL CHECK (has_null_timestamp IN (0, 1)),
            PRIMARY KEY (session_id, raw_skill, signal)
         ) WITHOUT ROWID;
         CREATE INDEX IF NOT EXISTS idx_session_skill_usage_source
            ON session_skill_usage(source, session_id);
         CREATE TABLE IF NOT EXISTS skill_dirty_sessions (
            session_id TEXT PRIMARY KEY
         ) WITHOUT ROWID;

         CREATE TRIGGER IF NOT EXISTS session_events_skill_ai AFTER INSERT ON session_events BEGIN
            INSERT INTO skill_dirty_sessions(session_id) VALUES (NEW.session_id)
            ON CONFLICT DO NOTHING;
         END;
         CREATE TRIGGER IF NOT EXISTS session_events_skill_au AFTER UPDATE ON session_events BEGIN
            INSERT INTO skill_dirty_sessions(session_id) VALUES (OLD.session_id)
            ON CONFLICT DO NOTHING;
            INSERT INTO skill_dirty_sessions(session_id) VALUES (NEW.session_id)
            ON CONFLICT DO NOTHING;
         END;
         CREATE TRIGGER IF NOT EXISTS session_events_skill_ad AFTER DELETE ON session_events BEGIN
            INSERT INTO skill_dirty_sessions(session_id) VALUES (OLD.session_id)
            ON CONFLICT DO NOTHING;
         END;",
    )?;

    let usage_end: i64 =
        tx.query_row("SELECT COALESCE(MAX(id), 0) FROM usage_events", [], |row| row.get(0))?;
    let (usage_phase, usage_detail) = if usage_end == 0 {
        ("done", String::new())
    } else {
        ("scan", serde_json::json!({"cursor": 0_i64, "end": usage_end}).to_string())
    };
    tx.execute(
        "INSERT INTO background_job_state(job, phase, detail, updated_at)
         VALUES (?1, ?2, NULLIF(?3, ''), unixepoch() * 1000)
         ON CONFLICT(job) DO UPDATE SET phase = excluded.phase, detail = excluded.detail,
                                       updated_at = excluded.updated_at",
        rusqlite::params![USAGE_INDEX_JOB, usage_phase, usage_detail],
    )?;

    let skill_end: i64 =
        tx.query_row("SELECT COALESCE(MAX(id), 0) FROM session_events", [], |row| row.get(0))?;
    let (skill_phase, skill_detail) = if skill_end == 0 {
        ("done", String::new())
    } else {
        ("scan", serde_json::json!({"cursor": 0_i64, "end": skill_end}).to_string())
    };
    tx.execute(
        "INSERT INTO background_job_state(job, phase, detail, updated_at)
         VALUES (?1, ?2, NULLIF(?3, ''), unixepoch() * 1000)
         ON CONFLICT(job) DO UPDATE SET phase = excluded.phase, detail = excluded.detail,
                                       updated_at = excluded.updated_at",
        rusqlite::params![SKILL_USAGE_JOB, skill_phase, skill_detail],
    )?;

    let rev_end: i64 =
        tx.query_row("SELECT COALESCE(MAX(rowid), 0) FROM sync_revisions", [], |row| row.get(0))?;
    let obj_end: i64 =
        tx.query_row("SELECT COALESCE(MAX(rowid), 0) FROM sync_objects", [], |row| row.get(0))?;
    let (rev_phase, rev_detail) = if rev_end > 0 || obj_end > 0 {
        (
            "backfill",
            serde_json::json!({
                "rev_cursor": 0_i64,
                "rev_end": rev_end,
                "obj_cursor": 0_i64,
                "obj_end": obj_end,
            })
            .to_string(),
        )
    } else {
        ("done", String::new())
    };
    tx.execute(
        "INSERT INTO background_job_state(job, phase, detail, updated_at)
         VALUES (?1, ?2, NULLIF(?3, ''), unixepoch() * 1000)
         ON CONFLICT(job) DO UPDATE SET phase = excluded.phase, detail = excluded.detail,
                                       updated_at = excluded.updated_at",
        rusqlite::params![REVISION_COMPRESSION_JOB, rev_phase, rev_detail],
    )?;

    tx.execute_batch("PRAGMA user_version = 21;")?;
    tx.commit()?;
    Ok(())
}

pub(super) fn migrate_v22(conn: &Connection) -> anyhow::Result<()> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    if super::read_schema_version(&tx)? >= 22 {
        return Ok(());
    }
    tx.execute_batch(
        "UPDATE usage_events
         SET output_tokens = output_tokens - reasoning_tokens, parser_version = 3
         WHERE source = 'copilot-cli'
           AND parser_version < 3
           AND reasoning_tokens > 0
           AND output_tokens >= reasoning_tokens
           AND EXISTS (
               SELECT 1 FROM usage_session_state uss
               WHERE uss.session_id = usage_events.session_id
                 AND uss.source = 'copilot-cli'
                 AND uss.parser_version < 3
           )
           AND json_valid(raw_usage_json)
           AND json_extract(raw_usage_json, '$.output_tokens') = output_tokens;

         CREATE TRIGGER IF NOT EXISTS native_bindings_usage_ai AFTER INSERT ON native_bindings BEGIN
            INSERT INTO usage_dirty_groups(dedup_group)
                SELECT dedup_group FROM usage_event_ownership
                WHERE session_id = NEW.session_id AND dedup_group IS NOT NULL
                ON CONFLICT DO NOTHING;
         END;
         CREATE TRIGGER IF NOT EXISTS native_bindings_usage_au AFTER UPDATE ON native_bindings BEGIN
            INSERT INTO usage_dirty_groups(dedup_group)
                SELECT dedup_group FROM usage_event_ownership
                WHERE session_id = OLD.session_id AND dedup_group IS NOT NULL
                ON CONFLICT DO NOTHING;
            INSERT INTO usage_dirty_groups(dedup_group)
                SELECT dedup_group FROM usage_event_ownership
                WHERE session_id = NEW.session_id AND dedup_group IS NOT NULL
                ON CONFLICT DO NOTHING;
         END;
         CREATE TRIGGER IF NOT EXISTS native_bindings_usage_ad AFTER DELETE ON native_bindings BEGIN
            INSERT INTO usage_dirty_groups(dedup_group)
                SELECT dedup_group FROM usage_event_ownership
                WHERE session_id = OLD.session_id AND dedup_group IS NOT NULL
                ON CONFLICT DO NOTHING;
         END;
         DELETE FROM usage_event_ownership;
         DELETE FROM usage_rollups;
         DELETE FROM usage_dirty_sessions;
         DELETE FROM usage_dirty_groups;",
    )?;
    let end: i64 =
        tx.query_row("SELECT COALESCE(MAX(id), 0) FROM usage_events", [], |row| row.get(0))?;
    let detail = (end != 0).then(|| serde_json::json!({"cursor": 0_i64, "end": end}).to_string());
    tx.execute(
        "INSERT INTO background_job_state(job, phase, detail, updated_at)
         VALUES (?1, ?2, ?3, unixepoch() * 1000)
         ON CONFLICT(job) DO UPDATE SET phase = excluded.phase, detail = excluded.detail,
                                       updated_at = excluded.updated_at",
        rusqlite::params![USAGE_INDEX_JOB, if end == 0 { "done" } else { "scan" }, detail],
    )?;
    tx.execute_batch("PRAGMA user_version = 22;")?;
    tx.commit()?;
    Ok(())
}

pub(super) fn migrate_v23(conn: &Connection) -> anyhow::Result<()> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    if super::read_schema_version(&tx)? >= 23 {
        return Ok(());
    }
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS codex_excluded_usage (
            source_id TEXT NOT NULL,
            timestamp INTEGER NOT NULL,
            total_input_tokens INTEGER NOT NULL,
            total_cached_tokens INTEGER NOT NULL,
            total_cache_write_tokens INTEGER NOT NULL,
            total_output_tokens INTEGER NOT NULL,
            total_reasoning_tokens INTEGER NOT NULL,
            last_input_tokens INTEGER NOT NULL,
            last_cached_tokens INTEGER NOT NULL,
            last_cache_write_tokens INTEGER NOT NULL,
            last_output_tokens INTEGER NOT NULL,
            last_reasoning_tokens INTEGER NOT NULL,
            PRIMARY KEY (
                source_id,
                timestamp,
                total_input_tokens,
                total_cached_tokens,
                total_cache_write_tokens,
                total_output_tokens,
                total_reasoning_tokens,
                last_input_tokens,
                last_cached_tokens,
                last_cache_write_tokens,
                last_output_tokens,
                last_reasoning_tokens
            )
         ) WITHOUT ROWID;
         CREATE INDEX IF NOT EXISTS idx_usage_events_codex_source
            ON usage_events(source_id, session_id)
            WHERE source = 'codex';
         CREATE TRIGGER IF NOT EXISTS codex_excluded_usage_native_ad AFTER DELETE ON native_bindings
         WHEN OLD.source = 'codex'
         BEGIN
            DELETE FROM codex_excluded_usage WHERE source_id = OLD.source_id;
            INSERT INTO usage_dirty_sessions(session_id)
                SELECT DISTINCT session_id FROM usage_events WHERE source = 'codex' AND source_id = OLD.source_id
                ON CONFLICT DO NOTHING;
         END;
         DELETE FROM usage_event_ownership;
         DELETE FROM usage_rollups;
         DELETE FROM usage_dirty_sessions;
         DELETE FROM usage_dirty_groups;",
    )?;
    let end: i64 =
        tx.query_row("SELECT COALESCE(MAX(id), 0) FROM usage_events", [], |row| row.get(0))?;
    let detail = (end != 0).then(|| serde_json::json!({"cursor": 0_i64, "end": end}).to_string());
    tx.execute(
        "INSERT INTO background_job_state(job, phase, detail, updated_at)
         VALUES (?1, ?2, ?3, unixepoch() * 1000)
         ON CONFLICT(job) DO UPDATE SET phase = excluded.phase, detail = excluded.detail,
                                       updated_at = excluded.updated_at",
        rusqlite::params![USAGE_INDEX_JOB, if end == 0 { "done" } else { "scan" }, detail],
    )?;
    tx.execute_batch("PRAGMA user_version = 23;")?;
    tx.commit()?;
    Ok(())
}
