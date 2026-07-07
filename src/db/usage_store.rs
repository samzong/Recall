use std::collections::HashMap;

use anyhow::Result;
use chrono::Utc;
use rusqlite::OptionalExtension;

use super::store::{Store, UsageSessionStateMeta};
use crate::db::search::TimeRange;
use crate::types::{RawUsageEvent, SessionUsageEventRecord, UsageEventRecord};

impl Store {
    pub(crate) fn usage_state_meta_map(
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

    pub(crate) fn persist_usage_events_for_existing_session(
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

    pub(crate) fn list_usage_events(
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

    pub(crate) fn list_usage_events_for_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<SessionUsageEventRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT event_key, event_seq, message_seq, timestamp, model, provider,
                    input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                    reasoning_tokens, token_source, parser_version, source_path, raw_usage_json
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
                parser_version: row.get(12)?,
                source_path: row.get(13)?,
                raw_usage_json: row.get(14)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}
