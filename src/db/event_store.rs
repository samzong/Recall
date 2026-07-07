use std::collections::HashMap;

use anyhow::Result;
use chrono::Utc;
use rusqlite::OptionalExtension;

use super::store::{EventSessionStateMeta, Store};
use crate::types::{RawSessionEvent, SessionEventRecord};

impl Store {
    pub(crate) fn event_state_meta_map(
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

    pub(crate) fn persist_session_events_for_existing_session(
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

    pub(crate) fn list_session_events_for_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<SessionEventRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT event_seq, timestamp, kind, actor, name, status, target,
                    message_seq, summary, source_path, source_event_id, attrs_json, parser_version
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
                attrs_json: row.get(11)?,
                parser_version: row.get(12)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}

pub(crate) fn replace_session_events(
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
