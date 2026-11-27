use std::collections::{HashMap, HashSet};

use anyhow::Result;
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior};

use super::super::schema::USAGE_INDEX_JOB;
use super::super::store::Store;
use super::backfill::{
    backfill_usage_dedup_batch, backfill_usage_finalize_batch, backfill_usage_scan_batch,
};
use super::dirty::{flush_dirty_usage_tx, has_dirty_usage};
use super::{
    BUCKET_MILLIS, SessionUsage, UsageOwnership, UsageSlice, map_usage_event_row,
    push_source_filter,
};

use crate::types::UsageEventRecord;

impl Store {
    pub(crate) fn usage_index_backfill_pending(&self) -> Result<bool> {
        let pending: Option<bool> = self
            .conn
            .query_row(
                "SELECT phase != 'done' FROM background_job_state WHERE job = ?1",
                [USAGE_INDEX_JOB],
                |row| row.get(0),
            )
            .optional()?;
        Ok(pending.unwrap_or(false))
    }

    pub(crate) fn usage_index_ready(&self) -> Result<bool> {
        let phase: Option<String> = self
            .conn
            .query_row(
                "SELECT phase FROM background_job_state WHERE job = ?1",
                [USAGE_INDEX_JOB],
                |row| row.get(0),
            )
            .optional()?;
        if phase.as_deref() != Some("done") {
            return Ok(false);
        }
        if !has_dirty_usage(&self.conn)? {
            return Ok(true);
        }
        let query_only: bool = self.conn.query_row("PRAGMA query_only", [], |row| row.get(0))?;
        if query_only {
            return Ok(false);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        flush_dirty_usage_tx(&tx)?;
        tx.commit()?;
        Ok(!has_dirty_usage(&self.conn)?)
    }

    pub(crate) fn backfill_usage_index_batch(&self) -> Result<bool> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let state: Option<(String, Option<String>)> = tx
            .query_row(
                "SELECT phase, detail FROM background_job_state WHERE job = ?1",
                [USAGE_INDEX_JOB],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((phase, detail)) = state else {
            return Ok(false);
        };
        let more = match phase.as_str() {
            "scan" => backfill_usage_scan_batch(&tx, detail.as_deref())?,
            "dedup" => backfill_usage_dedup_batch(&tx, detail.as_deref())?,
            "finalize" => backfill_usage_finalize_batch(&tx)?,
            "done" => false,
            other => anyhow::bail!("Invalid usage index backfill phase: {other}"),
        };
        tx.commit()?;
        Ok(more)
    }

    pub(crate) fn fold_usage_slices_after<T>(
        &self,
        sources: Option<&[String]>,
        after_millis: Option<i64>,
        mut state: T,
        mut fold: impl FnMut(&mut T, UsageSlice),
    ) -> Result<T> {
        if !self.usage_index_ready()? {
            return self.fold_usage_events_after(sources, after_millis, state, |state, event| {
                fold(state, UsageSlice::from_event(event));
            });
        }

        match after_millis {
            Some(cutoff) if cutoff.rem_euclid(BUCKET_MILLIS) != 0 => {
                let b0 = cutoff.div_euclid(BUCKET_MILLIS) * BUCKET_MILLIS;
                let next_b = b0 + BUCKET_MILLIS;
                state = self.fold_counted_events_range(
                    sources,
                    cutoff,
                    next_b,
                    state,
                    |state, event| {
                        fold(state, UsageSlice::from_event(event));
                    },
                )?;
                self.fold_rollup_buckets(sources, Some(next_b), None, state, fold)
            }
            cutoff => self.fold_rollup_buckets(sources, cutoff, None, state, fold),
        }
    }

    pub(crate) fn fold_dashboard_usage_slices<T>(
        &self,
        sources: Option<&[String]>,
        cutoff: Option<i64>,
        mut state: T,
        mut fold: impl FnMut(&mut T, UsageSlice, bool),
    ) -> Result<T> {
        if !self.usage_index_ready()? {
            return self.fold_usage_events_after(sources, None, state, |state, event| {
                let in_window = cutoff.is_none_or(|c| event.timestamp >= c);
                fold(state, UsageSlice::from_event(event), in_window);
            });
        }

        match cutoff {
            Some(c) if c.rem_euclid(BUCKET_MILLIS) != 0 => {
                let b0 = c.div_euclid(BUCKET_MILLIS) * BUCKET_MILLIS;
                let next_b = b0 + BUCKET_MILLIS;
                state =
                    self.fold_rollup_buckets(sources, None, Some(b0), state, |state, slice| {
                        fold(state, slice, false);
                    })?;
                state =
                    self.fold_counted_events_range(sources, b0, next_b, state, |state, event| {
                        let in_window = event.timestamp >= c;
                        fold(state, UsageSlice::from_event(event), in_window);
                    })?;
                self.fold_rollup_buckets(sources, Some(next_b), None, state, |state, slice| {
                    fold(state, slice, true);
                })
            }
            cutoff => self.fold_rollup_buckets(sources, None, None, state, |state, slice| {
                let in_window = cutoff.is_none_or(|c| slice.timestamp >= c);
                fold(state, slice, in_window);
            }),
        }
    }

    fn fold_counted_events_range<T>(
        &self,
        sources: Option<&[String]>,
        start_inclusive: i64,
        end_exclusive: i64,
        mut state: T,
        mut fold: impl FnMut(&mut T, UsageEventRecord),
    ) -> Result<T> {
        let mut sql = String::from(
            "SELECT u.session_id, u.source, u.source_id, u.event_key, u.timestamp, u.model, u.provider,
                    u.input_tokens, u.output_tokens, u.cache_read_tokens, u.cache_write_tokens,
                    u.reasoning_tokens, u.token_source, u.parser_version, u.raw_usage_json
             FROM usage_events u
             JOIN usage_event_ownership o ON o.event_id = u.id
             WHERE o.ownership = 1 AND u.timestamp >= ?1 AND u.timestamp < ?2",
        );
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> =
            vec![Box::new(start_inclusive), Box::new(end_exclusive)];
        push_source_filter(&mut sql, &mut params, 3, "u.source", sources);

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(param_refs.as_slice(), map_usage_event_row)?;
        for row in rows {
            fold(&mut state, row?);
        }
        Ok(state)
    }

    fn fold_rollup_buckets<T>(
        &self,
        sources: Option<&[String]>,
        min_bucket_inclusive: Option<i64>,
        max_bucket_exclusive: Option<i64>,
        mut state: T,
        mut fold: impl FnMut(&mut T, UsageSlice),
    ) -> Result<T> {
        let mut sql = String::from(
            "SELECT session_id, source, bucket_ms, model, provider,
                    input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                    reasoning_tokens, token_source, events
             FROM usage_rollups
             WHERE 1 = 1",
        );
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let mut idx = 1;
        if let Some(min_b) = min_bucket_inclusive {
            sql.push_str(&format!(" AND bucket_ms >= ?{idx}"));
            params.push(Box::new(min_b));
            idx += 1;
        }
        if let Some(max_b) = max_bucket_exclusive {
            sql.push_str(&format!(" AND bucket_ms < ?{idx}"));
            params.push(Box::new(max_b));
            idx += 1;
        }
        push_source_filter(&mut sql, &mut params, idx, "source", sources);

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(param_refs.as_slice(), |row| {
            Ok(UsageSlice {
                session_id: row.get(0)?,
                source: row.get(1)?,
                timestamp: row.get(2)?,
                model: row.get(3)?,
                provider: row.get(4)?,
                input_tokens: row.get(5)?,
                output_tokens: row.get(6)?,
                cache_read_tokens: row.get(7)?,
                cache_write_tokens: row.get(8)?,
                reasoning_tokens: row.get(9)?,
                token_source: row.get(10)?,
                events: row.get::<_, i64>(11)? as usize,
            })
        })?;
        for row in rows {
            fold(&mut state, row?);
        }
        Ok(state)
    }

    pub(crate) fn session_usage(&self, session_id: &str) -> Result<SessionUsage> {
        if !self.usage_index_ready()? {
            return Ok(self.usage_by_session()?.remove(session_id).unwrap_or_default());
        }
        let mut usage = SessionUsage::default();
        let mut stmt = self.conn.prepare(
            "SELECT input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens
             FROM usage_rollups
             WHERE session_id = ?1",
        )?;
        let rows = stmt.query_map([session_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;
        for row in rows {
            let (input, output, cache_read, cache_write, reasoning) = row?;
            usage.tokens.add_amounts(input, output, cache_read, cache_write, reasoning);
        }
        usage.shared_with_other_session = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM usage_event_ownership WHERE session_id = ?1 AND ownership = 3)",
            [session_id],
            |row| row.get(0),
        )?;
        Ok(usage)
    }

    pub(crate) fn usage_by_session(&self) -> Result<HashMap<String, SessionUsage>> {
        if self.usage_index_ready()? {
            let mut totals: HashMap<String, SessionUsage> = HashMap::new();
            let mut stmt = self.conn.prepare(
                "SELECT session_id, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens
                 FROM usage_rollups",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            })?;
            for row in rows {
                let (sid, input, output, cache_read, cache_write, reasoning) = row?;
                let usage = totals.entry(sid).or_default();
                usage.tokens.add_amounts(input, output, cache_read, cache_write, reasoning);
            }
            let mut shared_stmt = self.conn.prepare(
                "SELECT DISTINCT session_id FROM usage_event_ownership WHERE ownership = 3",
            )?;
            let shared_rows = shared_stmt.query_map([], |row| row.get::<_, String>(0))?;
            for sid in shared_rows {
                totals.entry(sid?).or_default().shared_with_other_session = true;
            }
            return Ok(totals);
        }

        self.fold_usage_ownership(None, HashMap::new(), |totals, event, ownership| {
            let usage: &mut SessionUsage = totals.entry(event.session_id.clone()).or_default();
            match ownership {
                UsageOwnership::Counted => usage.tokens.add_amounts(
                    event.input_tokens,
                    event.output_tokens,
                    event.cache_read_tokens,
                    event.cache_write_tokens,
                    event.reasoning_tokens,
                ),
                UsageOwnership::OtherSession => usage.shared_with_other_session = true,
                UsageOwnership::Duplicate => {}
            }
        })
    }

    pub(crate) fn counted_usage_event_keys(
        &self,
        session_ids: &HashSet<&str>,
    ) -> Result<HashMap<String, HashSet<String>>> {
        if session_ids.is_empty() {
            return Ok(HashMap::new());
        }
        if self.usage_index_ready()? {
            let mut keys: HashMap<String, HashSet<String>> = HashMap::new();
            let ids: Vec<&str> = session_ids.iter().copied().collect();
            for chunk in ids.chunks(500) {
                let placeholders =
                    (1..=chunk.len()).map(|idx| format!("?{idx}")).collect::<Vec<_>>().join(", ");
                let sql = format!(
                    "SELECT session_id, event_key
                     FROM usage_event_ownership
                     WHERE ownership = 1 AND session_id IN ({placeholders})"
                );
                let mut stmt = self.conn.prepare(&sql)?;
                let params: Vec<&dyn rusqlite::types::ToSql> =
                    chunk.iter().map(|id| id as &dyn rusqlite::types::ToSql).collect();
                let rows = stmt.query_map(params.as_slice(), |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?;
                for row in rows {
                    let (sid, event_key) = row?;
                    keys.entry(sid).or_default().insert(event_key);
                }
            }
            return Ok(keys);
        }

        self.fold_usage_ownership(None, HashMap::new(), |keys, event, ownership| {
            if ownership == UsageOwnership::Counted
                && session_ids.contains(event.session_id.as_str())
            {
                keys.entry(event.session_id).or_default().insert(event.event_key);
            }
        })
    }
}
