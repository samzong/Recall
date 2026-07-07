use anyhow::Result;
use chrono::Utc;
use rusqlite::OptionalExtension;

use super::project_store::apply_scope_filters;
use super::store::Store;
use crate::db::search::{RepoFilter, TimeRange};
use crate::types::{BackgroundJobStatus, SemanticProgress, SemanticSessionJob};
use crate::utils::f32_slice_to_bytes;

impl Store {
    pub(crate) fn upsert_embeddings(&self, items: &[(i64, &[f32])]) -> Result<()> {
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

    pub(crate) fn embeddable_messages(&self, session_id: &str) -> Result<Vec<(i64, String)>> {
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

    pub(crate) fn pending_embeddable_messages(
        &self,
        session_id: &str,
    ) -> Result<Vec<(i64, String)>> {
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

    pub(crate) fn embedded_message_count(&self, session_id: &str) -> Result<u64> {
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

    pub(crate) fn claim_next_session_embedding_job(&self) -> Result<Option<SemanticSessionJob>> {
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

    pub(crate) fn update_session_embedding_progress(
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

    pub(crate) fn complete_session_embedding(&self, session_id: &str) -> Result<()> {
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

    pub(crate) fn fail_session_embedding(&self, session_id: &str, error: &str) -> Result<()> {
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

    pub(crate) fn set_background_job_state(
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

    pub(crate) fn clear_background_job_state(&self, job: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM background_job_state WHERE job = ?1", rusqlite::params![job])?;
        Ok(())
    }

    pub(crate) fn background_job_status(&self, job: &str) -> Result<BackgroundJobStatus> {
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

    pub(crate) fn semantic_progress(&self) -> Result<SemanticProgress> {
        self.semantic_progress_for_scope(None, TimeRange::All)
    }

    pub(crate) fn semantic_progress_for_scope(
        &self,
        sources: Option<&[String]>,
        time_range: TimeRange,
    ) -> Result<SemanticProgress> {
        self.semantic_progress_for_search_scope(sources, time_range, None, None)
    }

    pub(crate) fn semantic_progress_for_search_scope(
        &self,
        sources: Option<&[String]>,
        time_range: TimeRange,
        directory: Option<&str>,
        repo: Option<&RepoFilter>,
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
        apply_scope_filters(
            &mut sql,
            &mut params,
            &mut param_idx,
            sources,
            time_range,
            directory,
            repo,
        );
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
            repo,
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
}
