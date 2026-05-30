use std::collections::HashMap;

use anyhow::Result;
use chrono::Utc;
use rusqlite::Connection;
use rusqlite::OptionalExtension;

use crate::db::search::TimeRange;
use crate::types::{
    BackgroundJobStatus, Message, RawSessionEvent, RawUsageEvent, Role, SemanticProgress,
    SemanticSessionJob, Session, SessionEventRecord, SessionUsageEventRecord, UsageEventRecord,
};
use crate::utils::f32_slice_to_bytes;

pub struct Store {
    pub conn: Connection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectDirectory {
    pub directory: String,
    pub sessions: u64,
    pub last_seen: i64,
}

#[derive(Debug, Clone, Copy)]
pub struct UsageSessionStateMeta {
    pub parser_version: u32,
    pub source_updated_at: Option<i64>,
}

#[derive(Debug, Clone, Copy)]
pub struct EventSessionStateMeta {
    pub parser_version: u32,
    pub source_updated_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct SkillAuditEventRow {
    pub session_id: String,
    pub source: String,
    pub timestamp: Option<i64>,
    pub name: Option<String>,
    pub target: Option<String>,
    pub attrs_json: Option<String>,
}

impl Store {
    pub fn open() -> Result<Self> {
        let data_dir = dirs::data_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot determine data directory"))?
            .join("recall");
        std::fs::create_dir_all(&data_dir)?;
        let db_path = data_dir.join("recall.db");
        let conn = Connection::open(&db_path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA busy_timeout=5000;
             PRAGMA foreign_keys=ON;",
        )?;
        crate::db::schema::init(&conn)?;
        Ok(Store { conn })
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON;")?;
        crate::db::schema::init(&conn)?;
        Ok(Store { conn })
    }

    pub fn session_meta(
        &self,
        source: &str,
        source_id: &str,
    ) -> Result<Option<(Option<i64>, u32)>> {
        let mut stmt = self.conn.prepare(
            "SELECT updated_at, message_count FROM sessions WHERE source = ?1 AND source_id = ?2",
        )?;
        let mut rows = stmt.query(rusqlite::params![source, source_id])?;
        match rows.next()? {
            Some(row) => Ok(Some((row.get(0)?, row.get(1)?))),
            None => Ok(None),
        }
    }

    pub fn session_meta_map(&self, source: &str) -> Result<HashMap<String, (Option<i64>, u32)>> {
        let mut stmt = self.conn.prepare(
            "SELECT source_id, updated_at, message_count FROM sessions WHERE source = ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![source], |row| {
            Ok((row.get::<_, String>(0)?, (row.get(1)?, row.get(2)?)))
        })?;
        rows.collect::<Result<HashMap<_, _>, _>>().map_err(Into::into)
    }

    pub fn usage_state_meta_map(
        &self,
        source: &str,
    ) -> Result<HashMap<String, UsageSessionStateMeta>> {
        let mut stmt = self.conn.prepare(
            "SELECT source_id, parser_version, source_updated_at
             FROM usage_session_state
             WHERE source = ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![source], |row| {
            Ok((
                row.get::<_, String>(0)?,
                UsageSessionStateMeta {
                    parser_version: row.get(1)?,
                    source_updated_at: row.get(2)?,
                },
            ))
        })?;
        rows.collect::<Result<HashMap<_, _>, _>>().map_err(Into::into)
    }

    pub fn event_state_meta_map(
        &self,
        source: &str,
    ) -> Result<HashMap<String, EventSessionStateMeta>> {
        let mut stmt = self.conn.prepare(
            "SELECT source_id, parser_version, source_updated_at
             FROM event_session_state
             WHERE source = ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![source], |row| {
            Ok((
                row.get::<_, String>(0)?,
                EventSessionStateMeta {
                    parser_version: row.get(1)?,
                    source_updated_at: row.get(2)?,
                },
            ))
        })?;
        rows.collect::<Result<HashMap<_, _>, _>>().map_err(Into::into)
    }

    pub fn insert_session(&self, session: &Session) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO sessions (id, source, source_id, title, directory, started_at, updated_at, message_count, entrypoint)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                session.id,
                session.source,
                session.source_id,
                session.title,
                session.directory,
                session.started_at,
                session.updated_at,
                session.message_count,
                session.entrypoint,
            ],
        )?;
        Ok(())
    }

    pub fn insert_messages(&self, messages: &[Message]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO messages (session_id, role, content, timestamp, seq)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for msg in messages {
                stmt.execute(rusqlite::params![
                    msg.session_id,
                    msg.role.as_str(),
                    msg.content,
                    msg.timestamp,
                    msg.seq,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn persist_session(&self, session: &Session, messages: &[Message]) -> Result<()> {
        self.persist_session_with_usage(session, messages, &[], None)
    }

    pub fn persist_session_with_usage(
        &self,
        session: &Session,
        messages: &[Message],
        usage_events: &[RawUsageEvent],
        usage_parser_version: Option<u32>,
    ) -> Result<()> {
        self.persist_session_with_usage_and_events(
            session,
            messages,
            usage_events,
            usage_parser_version,
            &[],
            None,
        )
    }

    pub fn persist_session_with_usage_and_events(
        &self,
        session: &Session,
        messages: &[Message],
        usage_events: &[RawUsageEvent],
        usage_parser_version: Option<u32>,
        session_events: &[RawSessionEvent],
        event_parser_version: Option<u32>,
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        {
            tx.execute(
                "INSERT OR REPLACE INTO sessions (id, source, source_id, title, directory, started_at, updated_at, message_count, entrypoint)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                rusqlite::params![
                    session.id,
                    session.source,
                    session.source_id,
                    session.title,
                    session.directory,
                    session.started_at,
                    session.updated_at,
                    session.message_count,
                    session.entrypoint,
                ],
            )?;

            {
                let mut stmt = tx.prepare(
                    "INSERT INTO messages (session_id, role, content, timestamp, seq)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                )?;
                for msg in messages {
                    stmt.execute(rusqlite::params![
                        msg.session_id,
                        msg.role.as_str(),
                        msg.content,
                        msg.timestamp,
                        msg.seq,
                    ])?;
                }
            }

            {
                tx.execute(
                    "DELETE FROM usage_events WHERE session_id = ?1",
                    rusqlite::params![session.id],
                )?;
                let created_at = Utc::now().timestamp_millis();
                let mut stmt = tx.prepare(
                    "INSERT INTO usage_events (
                        session_id, source, source_id, event_key, event_seq, message_seq,
                        timestamp, model, provider, input_tokens, output_tokens,
                        cache_read_tokens, cache_write_tokens, reasoning_tokens,
                        token_source, parser_version, source_path, raw_usage_json, created_at
                     )
                     VALUES (
                        ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                        ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19
                     )
                     ON CONFLICT(session_id, event_key) DO UPDATE SET
                        event_seq = excluded.event_seq,
                        message_seq = excluded.message_seq,
                        timestamp = excluded.timestamp,
                        model = excluded.model,
                        provider = excluded.provider,
                        input_tokens = excluded.input_tokens,
                        output_tokens = excluded.output_tokens,
                        cache_read_tokens = excluded.cache_read_tokens,
                        cache_write_tokens = excluded.cache_write_tokens,
                        reasoning_tokens = excluded.reasoning_tokens,
                        token_source = excluded.token_source,
                        parser_version = excluded.parser_version,
                        source_path = excluded.source_path,
                        raw_usage_json = excluded.raw_usage_json",
                )?;
                for event in usage_events {
                    stmt.execute(rusqlite::params![
                        session.id,
                        session.source,
                        session.source_id,
                        event.event_key,
                        event.event_seq,
                        event.message_seq,
                        event.timestamp,
                        event.model,
                        event.provider,
                        event.input_tokens,
                        event.output_tokens,
                        event.cache_read_tokens,
                        event.cache_write_tokens,
                        event.reasoning_tokens,
                        event.token_source.as_str(),
                        event.parser_version,
                        event.source_path,
                        event.raw_usage_json,
                        created_at,
                    ])?;
                }
            }

            if let Some(parser_version) = usage_parser_version {
                let synced_at = Utc::now().timestamp_millis();
                tx.execute(
                    "INSERT INTO usage_session_state (
                        session_id, source, source_id, parser_version,
                        source_updated_at, event_count, synced_at
                     )
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(source, source_id) DO UPDATE SET
                        session_id = excluded.session_id,
                        parser_version = excluded.parser_version,
                        source_updated_at = excluded.source_updated_at,
                        event_count = excluded.event_count,
                        synced_at = excluded.synced_at",
                    rusqlite::params![
                        session.id,
                        session.source,
                        session.source_id,
                        parser_version,
                        session.updated_at,
                        usage_events.len() as u32,
                        synced_at,
                    ],
                )?;
            }

            replace_session_events(
                &tx,
                &session.id,
                &session.source,
                &session.source_id,
                session_events,
                event_parser_version,
                session.updated_at,
            )?;

            let units_total: i64 = tx.query_row(
                "SELECT COUNT(*) FROM messages
                 WHERE session_id = ?1 AND role = 'user' AND LENGTH(content) > 2",
                rusqlite::params![session.id],
                |row| row.get(0),
            )?;

            let now = Utc::now().timestamp_millis();
            if units_total == 0 {
                tx.execute(
                    "INSERT INTO session_embedding_state (session_id, status, units_total, units_done, finished_at, last_error)
                     VALUES (?1, 'done', 0, 0, ?2, NULL)
                     ON CONFLICT(session_id) DO UPDATE SET
                        status = 'done',
                        units_total = 0,
                        units_done = 0,
                        started_at = NULL,
                        finished_at = excluded.finished_at,
                        last_error = NULL",
                    rusqlite::params![session.id, now],
                )?;
            } else {
                tx.execute(
                    "INSERT INTO session_embedding_state (session_id, status, units_total, units_done, started_at, finished_at, last_error)
                     VALUES (?1, 'pending', ?2, 0, NULL, NULL, NULL)
                     ON CONFLICT(session_id) DO UPDATE SET
                        status = 'pending',
                        units_total = excluded.units_total,
                        units_done = 0,
                        started_at = NULL,
                        finished_at = NULL,
                        last_error = NULL",
                    rusqlite::params![session.id, units_total],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn persist_usage_events_for_existing_session(
        &self,
        source: &str,
        source_id: &str,
        usage_events: &[RawUsageEvent],
        usage_parser_version: u32,
        source_updated_at: Option<i64>,
    ) -> Result<bool> {
        let session_id = self
            .conn
            .query_row(
                "SELECT id FROM sessions WHERE source = ?1 AND source_id = ?2",
                rusqlite::params![source, source_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;

        let Some(session_id) = session_id else {
            return Ok(false);
        };

        let tx = self.conn.unchecked_transaction()?;
        {
            tx.execute(
                "DELETE FROM usage_events WHERE session_id = ?1",
                rusqlite::params![&session_id],
            )?;

            let created_at = Utc::now().timestamp_millis();
            let mut stmt = tx.prepare(
                "INSERT INTO usage_events (
                    session_id, source, source_id, event_key, event_seq, message_seq,
                    timestamp, model, provider, input_tokens, output_tokens,
                    cache_read_tokens, cache_write_tokens, reasoning_tokens,
                    token_source, parser_version, source_path, raw_usage_json, created_at
                 )
                 VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                    ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19
                 )",
            )?;
            for event in usage_events {
                stmt.execute(rusqlite::params![
                    &session_id,
                    source,
                    source_id,
                    event.event_key,
                    event.event_seq,
                    event.message_seq,
                    event.timestamp,
                    event.model,
                    event.provider,
                    event.input_tokens,
                    event.output_tokens,
                    event.cache_read_tokens,
                    event.cache_write_tokens,
                    event.reasoning_tokens,
                    event.token_source.as_str(),
                    event.parser_version,
                    event.source_path,
                    event.raw_usage_json,
                    created_at,
                ])?;
            }

            tx.execute(
                "INSERT INTO usage_session_state (
                    session_id, source, source_id, parser_version,
                    source_updated_at, event_count, synced_at
                 )
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(source, source_id) DO UPDATE SET
                    session_id = excluded.session_id,
                    parser_version = excluded.parser_version,
                    source_updated_at = excluded.source_updated_at,
                    event_count = excluded.event_count,
                    synced_at = excluded.synced_at",
                rusqlite::params![
                    &session_id,
                    source,
                    source_id,
                    usage_parser_version,
                    source_updated_at,
                    usage_events.len() as u32,
                    Utc::now().timestamp_millis(),
                ],
            )?;
        }
        tx.commit()?;
        Ok(true)
    }

    pub fn persist_session_events_for_existing_session(
        &self,
        source: &str,
        source_id: &str,
        session_events: &[RawSessionEvent],
        event_parser_version: u32,
        source_updated_at: Option<i64>,
    ) -> Result<bool> {
        let session_id = self
            .conn
            .query_row(
                "SELECT id FROM sessions WHERE source = ?1 AND source_id = ?2",
                rusqlite::params![source, source_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;

        let Some(session_id) = session_id else {
            return Ok(false);
        };

        let tx = self.conn.unchecked_transaction()?;
        {
            replace_session_events(
                &tx,
                &session_id,
                source,
                source_id,
                session_events,
                Some(event_parser_version),
                source_updated_at,
            )?;
        }
        tx.commit()?;
        Ok(true)
    }

    pub fn upsert_embeddings(&self, items: &[(i64, &[f32])]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut del = tx.prepare("DELETE FROM message_vec WHERE message_id = ?1")?;
            let mut ins =
                tx.prepare("INSERT INTO message_vec (message_id, embedding) VALUES (?1, ?2)")?;
            for &(message_id, embedding) in items {
                let blob = f32_slice_to_bytes(embedding);
                del.execute(rusqlite::params![message_id])?;
                ins.execute(rusqlite::params![message_id, blob])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn upsert_session_embedding_state(&self, session_id: &str, units_total: u64) -> Result<()> {
        let now = Utc::now().timestamp_millis();
        if units_total == 0 {
            self.conn.execute(
                "INSERT INTO session_embedding_state (session_id, status, units_total, units_done, finished_at, last_error)
                 VALUES (?1, 'done', 0, 0, ?2, NULL)
                 ON CONFLICT(session_id) DO UPDATE SET
                    status = 'done',
                    units_total = 0,
                    units_done = 0,
                    started_at = NULL,
                    finished_at = excluded.finished_at,
                    last_error = NULL",
                rusqlite::params![session_id, now],
            )?;
            return Ok(());
        }

        self.conn.execute(
            "INSERT INTO session_embedding_state (session_id, status, units_total, units_done, started_at, finished_at, last_error)
             VALUES (?1, 'pending', ?2, 0, NULL, NULL, NULL)
             ON CONFLICT(session_id) DO UPDATE SET
                status = 'pending',
                units_total = excluded.units_total,
                units_done = 0,
                started_at = NULL,
                finished_at = NULL,
                last_error = NULL",
            rusqlite::params![session_id, units_total as i64],
        )?;
        Ok(())
    }

    pub fn embeddable_messages(&self, session_id: &str) -> Result<Vec<(i64, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, content FROM messages
             WHERE session_id = ?1 AND role = 'user' AND LENGTH(content) > 2
             ORDER BY seq",
        )?;
        let rows = stmt.query_map(rusqlite::params![session_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn pending_embeddable_messages(&self, session_id: &str) -> Result<Vec<(i64, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT m.id, m.content
             FROM messages m
             LEFT JOIN message_vec mv ON mv.message_id = m.id
             WHERE m.session_id = ?1
               AND m.role = 'user'
               AND LENGTH(m.content) > 2
               AND mv.message_id IS NULL
             ORDER BY m.seq",
        )?;
        let rows = stmt.query_map(rusqlite::params![session_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn embeddable_message_count(&self, session_id: &str) -> Result<u64> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM messages
                 WHERE session_id = ?1 AND role = 'user' AND LENGTH(content) > 2",
                rusqlite::params![session_id],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub fn embedded_message_count(&self, session_id: &str) -> Result<u64> {
        self.conn
            .query_row(
                "SELECT COUNT(*)
                 FROM messages m
                 JOIN message_vec mv ON mv.message_id = m.id
                 WHERE m.session_id = ?1 AND m.role = 'user' AND LENGTH(m.content) > 2",
                rusqlite::params![session_id],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub fn has_pending_session_embeddings(&self) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM session_embedding_state WHERE status = 'pending'",
            [],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn claim_next_session_embedding_job(&self) -> Result<Option<SemanticSessionJob>> {
        let now = Utc::now().timestamp_millis();
        let session_id: Option<String> = self
            .conn
            .query_row(
                "UPDATE session_embedding_state
                 SET status = 'processing',
                     started_at = COALESCE(started_at, ?1),
                     finished_at = NULL,
                     last_error = NULL
                 WHERE session_id = (
                     SELECT st.session_id
                     FROM session_embedding_state st
                     JOIN sessions s ON s.id = st.session_id
                     WHERE st.status = 'pending'
                     ORDER BY COALESCE(s.updated_at, s.started_at) DESC
                     LIMIT 1
                 )
                 RETURNING session_id",
                rusqlite::params![now],
                |row| row.get(0),
            )
            .optional()?;

        let Some(session_id) = session_id else {
            return Ok(None);
        };

        let job = self.conn.query_row(
            "SELECT s.id, s.title, st.units_total
             FROM sessions s
             JOIN session_embedding_state st ON st.session_id = s.id
             WHERE s.id = ?1",
            rusqlite::params![session_id],
            |row| {
                Ok(SemanticSessionJob {
                    session_id: row.get(0)?,
                    title: row.get(1)?,
                    units_total: row.get(2)?,
                })
            },
        )?;
        Ok(Some(job))
    }

    pub fn update_session_embedding_progress(
        &self,
        session_id: &str,
        units_done: u64,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE session_embedding_state
             SET status = 'processing',
                 units_done = ?2
             WHERE session_id = ?1",
            rusqlite::params![session_id, units_done as i64],
        )?;
        Ok(())
    }

    pub fn complete_session_embedding(&self, session_id: &str) -> Result<()> {
        let now = Utc::now().timestamp_millis();
        self.conn.execute(
            "UPDATE session_embedding_state
             SET status = 'done',
                 units_done = units_total,
                 finished_at = ?2,
                 last_error = NULL
             WHERE session_id = ?1",
            rusqlite::params![session_id, now],
        )?;
        Ok(())
    }

    pub fn fail_session_embedding(&self, session_id: &str, error: &str) -> Result<()> {
        let now = Utc::now().timestamp_millis();
        self.conn.execute(
            "UPDATE session_embedding_state
             SET status = 'failed',
                 finished_at = ?2,
                 last_error = ?3
             WHERE session_id = ?1",
            rusqlite::params![session_id, now, error],
        )?;
        Ok(())
    }

    pub fn set_background_job_state(
        &self,
        job: &str,
        phase: &str,
        detail: Option<&str>,
    ) -> Result<()> {
        let now = Utc::now().timestamp_millis();
        self.conn.execute(
            "INSERT INTO background_job_state (job, phase, detail, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(job) DO UPDATE SET
                phase = excluded.phase,
                detail = excluded.detail,
                updated_at = excluded.updated_at",
            rusqlite::params![job, phase, detail, now],
        )?;
        Ok(())
    }

    pub fn clear_background_job_state(&self, job: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM background_job_state WHERE job = ?1", rusqlite::params![job])?;
        Ok(())
    }

    pub fn background_job_status(&self, job: &str) -> Result<BackgroundJobStatus> {
        let status = self
            .conn
            .query_row(
                "SELECT phase, detail FROM background_job_state WHERE job = ?1",
                rusqlite::params![job],
                |row| Ok(BackgroundJobStatus { phase: Some(row.get(0)?), detail: row.get(1)? }),
            )
            .optional()?;
        Ok(status.unwrap_or_default())
    }

    pub fn semantic_progress(&self) -> Result<SemanticProgress> {
        self.semantic_progress_for_scope(None, TimeRange::All)
    }

    pub fn semantic_progress_for_scope(
        &self,
        sources: Option<&[String]>,
        time_range: TimeRange,
    ) -> Result<SemanticProgress> {
        self.semantic_progress_for_search_scope(sources, time_range, None)
    }

    pub fn semantic_progress_for_search_scope(
        &self,
        sources: Option<&[String]>,
        time_range: TimeRange,
        directory: Option<&str>,
    ) -> Result<SemanticProgress> {
        let mut sql = String::from(
            "SELECT
                COUNT(*),
                SUM(CASE WHEN st.status = 'done' THEN 1 ELSE 0 END),
                SUM(CASE WHEN st.status = 'processing' THEN 1 ELSE 0 END),
                SUM(CASE WHEN st.status = 'failed' THEN 1 ELSE 0 END),
                SUM(CASE WHEN st.status = 'pending' THEN 1 ELSE 0 END)
             FROM session_embedding_state st
             JOIN sessions s ON s.id = st.session_id
             WHERE 1=1",
        );
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let mut param_idx = 1;
        apply_scope_filters(&mut sql, &mut params, &mut param_idx, sources, time_range, directory);
        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();

        let progress = self.conn.query_row(&sql, param_refs.as_slice(), |row| {
            Ok(SemanticProgress {
                total_sessions: row.get::<_, Option<i64>>(0)?.unwrap_or(0) as u64,
                done_sessions: row.get::<_, Option<i64>>(1)?.unwrap_or(0) as u64,
                processing_sessions: row.get::<_, Option<i64>>(2)?.unwrap_or(0) as u64,
                failed_sessions: row.get::<_, Option<i64>>(3)?.unwrap_or(0) as u64,
                pending_sessions: row.get::<_, Option<i64>>(4)?.unwrap_or(0) as u64,
                current_session_title: None,
            })
        })?;

        let mut current_sql = String::from(
            "SELECT s.title
             FROM session_embedding_state st
             JOIN sessions s ON s.id = st.session_id
             WHERE st.status = 'processing'",
        );
        let mut current_params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let mut current_param_idx = 1;
        apply_scope_filters(
            &mut current_sql,
            &mut current_params,
            &mut current_param_idx,
            sources,
            time_range,
            directory,
        );
        current_sql.push_str(" ORDER BY COALESCE(s.updated_at, s.started_at) DESC LIMIT 1");
        let current_param_refs: Vec<&dyn rusqlite::types::ToSql> =
            current_params.iter().map(|p| p.as_ref()).collect();

        let current_session_title = self
            .conn
            .query_row(&current_sql, current_param_refs.as_slice(), |row| row.get(0))
            .optional()?;

        Ok(SemanticProgress { current_session_title, ..progress })
    }

    pub fn delete_session_data(&self, source: &str, source_id: &str) -> Result<()> {
        let session_ids: Vec<String> = {
            let mut stmt = self
                .conn
                .prepare("SELECT id FROM sessions WHERE source = ?1 AND source_id = ?2")?;
            stmt.query_map(rusqlite::params![source, source_id], |row| row.get(0))?
                .filter_map(|r| r.ok())
                .collect()
        };
        for sid in &session_ids {
            self.conn.execute(
                "DELETE FROM message_vec WHERE message_id IN (SELECT id FROM messages WHERE session_id = ?1)",
                rusqlite::params![sid],
            )?;
        }
        self.conn.execute(
            "DELETE FROM sessions WHERE source = ?1 AND source_id = ?2",
            rusqlite::params![source, source_id],
        )?;
        Ok(())
    }

    pub fn list_usage_events(
        &self,
        sources: Option<&[String]>,
        time_range: TimeRange,
    ) -> Result<Vec<UsageEventRecord>> {
        let mut sql = String::from(
            "SELECT session_id, source, source_id, event_key, timestamp, model, provider,
                    input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                    reasoning_tokens, token_source
             FROM usage_events
             WHERE 1 = 1",
        );

        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let mut param_idx = 1;

        if let Some(cutoff) = time_range.millis_ago() {
            sql.push_str(&format!(" AND timestamp >= ?{param_idx}"));
            params.push(Box::new(cutoff));
            param_idx += 1;
        }

        if let Some(source_ids) = sources
            && !source_ids.is_empty()
        {
            sql.push_str(" AND source IN (");
            for (i, source_id) in source_ids.iter().enumerate() {
                if i > 0 {
                    sql.push_str(", ");
                }
                sql.push_str(&format!("?{param_idx}"));
                params.push(Box::new(source_id.clone()));
                param_idx += 1;
            }
            sql.push(')');
        }

        sql.push_str(" ORDER BY timestamp ASC, source ASC, event_seq ASC");

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(param_refs.as_slice(), |row| {
            Ok(UsageEventRecord {
                session_id: row.get(0)?,
                source: row.get(1)?,
                source_id: row.get(2)?,
                event_key: row.get(3)?,
                timestamp: row.get(4)?,
                model: row.get(5)?,
                provider: row.get(6)?,
                input_tokens: row.get(7)?,
                output_tokens: row.get(8)?,
                cache_read_tokens: row.get(9)?,
                cache_write_tokens: row.get(10)?,
                reasoning_tokens: row.get(11)?,
                token_source: row.get(12)?,
            })
        })?;

        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn list_skill_audit_events(
        &self,
        sources: Option<&[String]>,
        time_range: TimeRange,
    ) -> Result<Vec<SkillAuditEventRow>> {
        let mut sql = String::from(
            "SELECT e.session_id, e.source,
                    COALESCE(e.timestamp, s.updated_at, s.started_at) AS timestamp,
                    e.name, e.target, e.attrs_json
             FROM session_events e
             JOIN sessions s ON s.id = e.session_id
             WHERE (LOWER(e.name) IN ('skill', 'use_skill') OR e.target LIKE '%/skills/%/SKILL.md%'
                    OR e.target LIKE '%/skills/%/references/%')",
        );

        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let mut param_idx = 1;

        if let Some(cutoff) = time_range.millis_ago() {
            sql.push_str(&format!(
                " AND COALESCE(e.timestamp, s.updated_at, s.started_at) >= ?{param_idx}"
            ));
            params.push(Box::new(cutoff));
            param_idx += 1;
        }

        if let Some(source_ids) = sources
            && !source_ids.is_empty()
        {
            sql.push_str(" AND e.source IN (");
            for (i, source_id) in source_ids.iter().enumerate() {
                if i > 0 {
                    sql.push_str(", ");
                }
                sql.push_str(&format!("?{param_idx}"));
                params.push(Box::new(source_id.clone()));
                param_idx += 1;
            }
            sql.push(')');
        }

        sql.push_str(
            " ORDER BY COALESCE(e.timestamp, s.updated_at, s.started_at) ASC, e.session_id ASC, e.event_seq ASC",
        );

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(param_refs.as_slice(), |row| {
            Ok(SkillAuditEventRow {
                session_id: row.get(0)?,
                source: row.get(1)?,
                timestamp: row.get(2)?,
                name: row.get(3)?,
                target: row.get(4)?,
                attrs_json: row.get(5)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn list_sessions_by_ids(&self, session_ids: &[String]) -> Result<Vec<Session>> {
        if session_ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders =
            std::iter::repeat_n("?", session_ids.len()).collect::<Vec<_>>().join(", ");
        let sql = format!(
            "SELECT id, source, source_id, title, directory, started_at, updated_at, message_count, entrypoint
             FROM sessions
             WHERE id IN ({placeholders})
             ORDER BY started_at DESC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::types::ToSql> =
            session_ids.iter().map(|id| id as &dyn rusqlite::types::ToSql).collect();
        let rows = stmt.query_map(params.as_slice(), |row| {
            Ok(Session {
                id: row.get(0)?,
                source: row.get(1)?,
                source_id: row.get(2)?,
                title: row.get(3)?,
                directory: row.get(4)?,
                started_at: row.get(5)?,
                updated_at: row.get(6)?,
                message_count: row.get(7)?,
                entrypoint: row.get(8)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn get_messages(&self, session_id: &str) -> Result<Vec<Message>> {
        let mut stmt = self.conn.prepare(
            "SELECT role, content, timestamp, seq FROM messages WHERE session_id = ?1 ORDER BY seq",
        )?;
        let rows = stmt.query_map(rusqlite::params![session_id], |row| {
            let role_str: String = row.get(0)?;
            Ok(Message {
                session_id: session_id.to_string(),
                role: role_str.parse().unwrap_or(Role::User),
                content: row.get(1)?,
                timestamp: row.get(2)?,
                seq: row.get(3)?,
            })
        })?;
        let mut messages = Vec::new();
        for row in rows {
            messages.push(row?);
        }
        Ok(messages)
    }

    pub fn list_usage_events_for_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<SessionUsageEventRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT event_key, event_seq, message_seq, timestamp, model, provider,
                    input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                    reasoning_tokens, token_source
             FROM usage_events
             WHERE session_id = ?1
             ORDER BY event_seq ASC, event_key ASC",
        )?;
        let rows = stmt.query_map(rusqlite::params![session_id], |row| {
            Ok(SessionUsageEventRecord {
                event_key: row.get(0)?,
                event_seq: row.get(1)?,
                message_seq: row.get(2)?,
                timestamp: row.get(3)?,
                model: row.get(4)?,
                provider: row.get(5)?,
                input_tokens: row.get(6)?,
                output_tokens: row.get(7)?,
                cache_read_tokens: row.get(8)?,
                cache_write_tokens: row.get(9)?,
                reasoning_tokens: row.get(10)?,
                token_source: row.get(11)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn list_session_events_for_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<SessionEventRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT event_seq, timestamp, kind, actor, name, status, target,
                    message_seq, summary, source_path, source_event_id
             FROM session_events
             WHERE session_id = ?1
             ORDER BY event_seq ASC",
        )?;
        let rows = stmt.query_map(rusqlite::params![session_id], |row| {
            Ok(SessionEventRecord {
                event_seq: row.get(0)?,
                timestamp: row.get(1)?,
                kind: row.get(2)?,
                actor: row.get(3)?,
                name: row.get(4)?,
                status: row.get(5)?,
                target: row.get(6)?,
                message_seq: row.get(7)?,
                summary: row.get(8)?,
                source_path: row.get(9)?,
                source_event_id: row.get(10)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn stats(&self) -> Result<(u64, u64)> {
        self.stats_for_scope(None, TimeRange::All)
    }

    pub fn stats_for_scope(
        &self,
        sources: Option<&[String]>,
        time_range: TimeRange,
    ) -> Result<(u64, u64)> {
        self.stats_for_search_scope(sources, time_range, None)
    }

    pub fn stats_for_search_scope(
        &self,
        sources: Option<&[String]>,
        time_range: TimeRange,
        directory: Option<&str>,
    ) -> Result<(u64, u64)> {
        let mut session_sql = String::from("SELECT COUNT(*) FROM sessions s WHERE 1=1");
        let mut session_params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let mut session_param_idx = 1;
        apply_scope_filters(
            &mut session_sql,
            &mut session_params,
            &mut session_param_idx,
            sources,
            time_range,
            directory,
        );
        let session_param_refs: Vec<&dyn rusqlite::types::ToSql> =
            session_params.iter().map(|p| p.as_ref()).collect();
        let session_count: u64 =
            self.conn.query_row(&session_sql, session_param_refs.as_slice(), |row| row.get(0))?;

        let mut message_sql = String::from(
            "SELECT COUNT(*)
             FROM messages m
             JOIN sessions s ON s.id = m.session_id
             WHERE 1=1",
        );
        let mut message_params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let mut message_param_idx = 1;
        apply_scope_filters(
            &mut message_sql,
            &mut message_params,
            &mut message_param_idx,
            sources,
            time_range,
            directory,
        );
        let message_param_refs: Vec<&dyn rusqlite::types::ToSql> =
            message_params.iter().map(|p| p.as_ref()).collect();
        let message_count: u64 =
            self.conn.query_row(&message_sql, message_param_refs.as_slice(), |row| row.get(0))?;
        Ok((session_count, message_count))
    }

    pub fn list_recent_sessions(&self, limit: usize) -> Result<Vec<Session>> {
        self.list_recent_sessions_for_search_scope(None, TimeRange::All, None, limit)
    }

    pub fn list_recent_sessions_for_search_scope(
        &self,
        sources: Option<&[String]>,
        time_range: TimeRange,
        directory: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Session>> {
        let mut sql = String::from(
            "SELECT id, source, source_id, title, directory, started_at, updated_at, message_count, entrypoint
             FROM sessions s
             WHERE 1=1",
        );
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let mut param_idx = 1;
        apply_scope_filters(&mut sql, &mut params, &mut param_idx, sources, time_range, directory);
        sql.push_str(&format!(" ORDER BY started_at DESC LIMIT ?{param_idx}"));
        params.push(Box::new(limit as i64));
        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(param_refs.as_slice(), |row| {
            Ok(Session {
                id: row.get(0)?,
                source: row.get(1)?,
                source_id: row.get(2)?,
                title: row.get(3)?,
                directory: row.get(4)?,
                started_at: row.get(5)?,
                updated_at: row.get(6)?,
                message_count: row.get(7)?,
                entrypoint: row.get(8)?,
            })
        })?;
        let mut sessions = Vec::new();
        for row in rows {
            sessions.push(row?);
        }
        Ok(sessions)
    }

    pub fn list_export_sessions(
        &self,
        sources: Option<&[String]>,
        time_range: TimeRange,
        directory: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Vec<Session>> {
        let mut sql = String::from(
            "SELECT id, source, source_id, title, directory, started_at, updated_at, message_count, entrypoint
             FROM sessions s
             WHERE 1=1",
        );
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let mut param_idx = 1;
        apply_scope_filters(&mut sql, &mut params, &mut param_idx, sources, time_range, directory);
        sql.push_str(" ORDER BY started_at DESC, source ASC, source_id ASC");
        if let Some(limit) = limit {
            sql.push_str(&format!(" LIMIT ?{param_idx}"));
            params.push(Box::new(limit as i64));
        }
        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(param_refs.as_slice(), |row| {
            Ok(Session {
                id: row.get(0)?,
                source: row.get(1)?,
                source_id: row.get(2)?,
                title: row.get(3)?,
                directory: row.get(4)?,
                started_at: row.get(5)?,
                updated_at: row.get(6)?,
                message_count: row.get(7)?,
                entrypoint: row.get(8)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn list_project_directories(&self) -> Result<Vec<ProjectDirectory>> {
        let mut stmt = self.conn.prepare(
            "SELECT directory, COUNT(*) AS sessions, MAX(COALESCE(updated_at, started_at)) AS last_seen
             FROM sessions
             WHERE directory IS NOT NULL AND directory != ''
             GROUP BY directory
             ORDER BY last_seen DESC, sessions DESC, directory ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ProjectDirectory {
                directory: row.get(0)?,
                sessions: row.get::<_, i64>(1)? as u64,
                last_seen: row.get(2)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}

fn replace_session_events(
    tx: &rusqlite::Transaction<'_>,
    session_id: &str,
    source: &str,
    source_id: &str,
    session_events: &[RawSessionEvent],
    event_parser_version: Option<u32>,
    source_updated_at: Option<i64>,
) -> Result<()> {
    tx.execute("DELETE FROM session_events WHERE session_id = ?1", rusqlite::params![session_id])?;

    let created_at = Utc::now().timestamp_millis();
    let mut stmt = tx.prepare(
        "INSERT INTO session_events (
            session_id, source, source_id, event_seq, timestamp,
            kind, actor, name, status, target, message_seq, summary,
            source_path, source_event_id, attrs_json, parser_version, created_at
         )
         VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
            ?12, ?13, ?14, ?15, ?16, ?17
         )",
    )?;
    for event in session_events {
        stmt.execute(rusqlite::params![
            session_id,
            source,
            source_id,
            event.event_seq,
            event.timestamp,
            event.kind,
            event.actor,
            event.name,
            event.status,
            event.target,
            event.message_seq,
            event.summary,
            event.source_path,
            event.source_event_id,
            event.attrs_json,
            event.parser_version,
            created_at,
        ])?;
    }

    if let Some(parser_version) = event_parser_version {
        tx.execute(
            "INSERT INTO event_session_state (
                session_id, source, source_id, parser_version,
                source_updated_at, event_count, synced_at
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(source, source_id) DO UPDATE SET
                session_id = excluded.session_id,
                parser_version = excluded.parser_version,
                source_updated_at = excluded.source_updated_at,
                event_count = excluded.event_count,
                synced_at = excluded.synced_at",
            rusqlite::params![
                session_id,
                source,
                source_id,
                parser_version,
                source_updated_at,
                session_events.len() as u32,
                Utc::now().timestamp_millis(),
            ],
        )?;
    }

    Ok(())
}

fn apply_scope_filters(
    sql: &mut String,
    params: &mut Vec<Box<dyn rusqlite::types::ToSql>>,
    param_idx: &mut usize,
    sources: Option<&[String]>,
    time_range: TimeRange,
    directory: Option<&str>,
) {
    if let Some(sources) = sources
        && !sources.is_empty()
    {
        let placeholders: Vec<String> =
            (0..sources.len()).map(|offset| format!("?{}", *param_idx + offset)).collect();
        sql.push_str(&format!(" AND s.source IN ({})", placeholders.join(", ")));
        for source in sources {
            params.push(Box::new(source.clone()));
        }
        *param_idx += sources.len();
    }

    if let Some(min_ts) = time_range.millis_ago() {
        sql.push_str(&format!(" AND s.started_at >= ?{}", *param_idx));
        params.push(Box::new(min_ts));
        *param_idx += 1;
    }

    if let Some(dir) = directory {
        sql.push_str(&format!(
            " AND (s.directory = ?{} OR s.directory LIKE ?{})",
            *param_idx,
            *param_idx + 1
        ));
        params.push(Box::new(dir.to_string()));
        params.push(Box::new(directory_child_pattern(dir)));
        *param_idx += 2;
    }
}

fn directory_child_pattern(dir: &str) -> String {
    if dir.ends_with('/') { format!("{dir}%") } else { format!("{dir}/%") }
}
