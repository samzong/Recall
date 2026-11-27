use std::collections::HashMap;

use anyhow::Result;
use rusqlite::OptionalExtension;

use super::super::store::{ParserStateMeta, Store};
use super::dirty::flush_dirty_usage_tx;
use super::{
    PersistedUsage, UsageConflict, UsageDedup, UsageOwnership, map_usage_event_row,
    normalize_persisted_usage, replace_usage_events,
};
use crate::types::{RawUsageEvent, SessionUsageEventRecord, UsageEventRecord};

impl Store {
    pub(crate) fn usage_state_meta_map(
        &self,
        source: &str,
    ) -> Result<HashMap<String, ParserStateMeta>> {
        self.parser_state_map(
            "SELECT source_id, parser_version, source_updated_at
             FROM usage_session_state
             WHERE source = ?1 AND session_id IN (SELECT session_id FROM native_bindings)",
            source,
        )
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
                "SELECT session_id FROM native_bindings WHERE source = ?1 AND source_id = ?2",
                rusqlite::params![source, source_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;

        let Some(session_id) = session_id else {
            return Ok(false);
        };

        let tx = self.conn.unchecked_transaction()?;
        replace_usage_events(
            &tx,
            &session_id,
            source,
            source_id,
            usage_events,
            Some(usage_parser_version),
            source_updated_at,
            UsageConflict::Reject,
        )?;
        flush_dirty_usage_tx(&tx)?;
        tx.commit()?;
        Ok(true)
    }

    pub(crate) fn fold_usage_events_after<T>(
        &self,
        sources: Option<&[String]>,
        after_millis: Option<i64>,
        state: T,
        mut fold: impl FnMut(&mut T, UsageEventRecord),
    ) -> Result<T> {
        self.fold_usage_ownership(sources, state, |state, event, ownership| {
            if ownership == UsageOwnership::Counted
                && after_millis.is_none_or(|cutoff| event.timestamp >= cutoff)
            {
                fold(state, event);
            }
        })
    }

    pub(super) fn fold_usage_ownership<T>(
        &self,
        sources: Option<&[String]>,
        mut state: T,
        mut fold: impl FnMut(&mut T, UsageEventRecord, UsageOwnership),
    ) -> Result<T> {
        let mut sql = String::from(
            "SELECT session_id, source, source_id, event_key, timestamp, model, provider,
                    input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                    reasoning_tokens, token_source, parser_version, raw_usage_json
             FROM usage_events
             WHERE 1 = 1",
        );

        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let mut param_idx = 1;

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

        sql.push_str(
            " ORDER BY timestamp ASC, source ASC, event_seq ASC, session_id ASC, event_key ASC",
        );

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(param_refs.as_slice(), map_usage_event_row)?;

        let mut dedup = UsageDedup::default();
        for row in rows {
            let event = row?;
            let ownership = dedup.assign(&event);
            fold(&mut state, event, ownership);
        }
        Ok(state)
    }

    #[cfg(any(test, feature = "bench"))]
    pub(crate) fn for_each_deduped_usage_event(
        events: &[UsageEventRecord],
        mut visit: impl FnMut(&UsageEventRecord),
    ) {
        let mut dedup = UsageDedup::default();
        for event in events {
            if dedup.assign(event) == UsageOwnership::Counted {
                visit(event);
            }
        }
    }

    pub(crate) fn list_usage_events_for_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<SessionUsageEventRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT event_key, event_seq, message_seq, timestamp, model, provider,
                    input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                    reasoning_tokens, token_source, parser_version, source_path, raw_usage_json,
                    source
             FROM usage_events
             WHERE session_id = ?1
             ORDER BY event_seq ASC, event_key ASC",
        )?;
        let rows = stmt.query_map(rusqlite::params![session_id], |row| {
            let raw_usage_json = row.get::<_, Option<String>>(14)?;
            let usage = normalize_persisted_usage(
                &row.get::<_, String>(15)?,
                PersistedUsage {
                    input_tokens: row.get(6)?,
                    output_tokens: row.get(7)?,
                    cache_read_tokens: row.get(8)?,
                    cache_write_tokens: row.get(9)?,
                    reasoning_tokens: row.get(10)?,
                    parser_version: row.get(12)?,
                },
                raw_usage_json.as_deref(),
            );
            Ok(SessionUsageEventRecord {
                event_key: row.get(0)?,
                event_seq: row.get(1)?,
                message_seq: row.get(2)?,
                timestamp: row.get(3)?,
                model: row.get(4)?,
                provider: row.get(5)?,
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cache_read_tokens: usage.cache_read_tokens,
                cache_write_tokens: usage.cache_write_tokens,
                reasoning_tokens: usage.reasoning_tokens,
                token_source: row.get(11)?,
                parser_version: usage.parser_version,
                source_path: row.get(13)?,
                raw_usage_json,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}
