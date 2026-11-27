use std::collections::BTreeMap;

use anyhow::Result;
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior};

use super::schema::SKILL_USAGE_JOB;
use super::store::{SkillAuditEventRow, Store};
use crate::db::search::TimeRange;
use crate::types::RawSessionEvent;

const SKILL_SCAN_BATCH: i64 = 5_000;
const SKILL_FINALIZE_BATCH: usize = 200;

pub(crate) struct IndexedSkillUsageRow {
    pub(crate) session_id: String,
    pub(crate) raw_skill: String,
    pub(crate) signal: String,
    pub(crate) max_timestamp: Option<i64>,
    pub(crate) has_null_timestamp: bool,
    pub(crate) session_ts: i64,
}

type SkillAggregateKey = (String, String, String, &'static str);
type SkillAggregateVal = (Option<i64>, i64);

struct SkillScanRow {
    id: i64,
    session_id: String,
    source: String,
    timestamp: Option<i64>,
    kind: String,
    name: Option<String>,
    target: Option<String>,
    attrs_json: Option<String>,
}

impl Store {
    pub(crate) fn skill_usage_backfill_pending(&self) -> Result<bool> {
        let pending: Option<bool> = self
            .conn
            .query_row(
                "SELECT phase != 'done' FROM background_job_state WHERE job = ?1",
                [SKILL_USAGE_JOB],
                |row| row.get(0),
            )
            .optional()?;
        Ok(pending.unwrap_or(false))
    }

    pub(crate) fn skill_usage_index_ready(&self) -> Result<bool> {
        let phase: Option<String> = self
            .conn
            .query_row(
                "SELECT phase FROM background_job_state WHERE job = ?1",
                [SKILL_USAGE_JOB],
                |row| row.get(0),
            )
            .optional()?;
        if phase.as_deref() != Some("done") {
            return Ok(false);
        }
        let has_dirty: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM skill_dirty_sessions)",
            [],
            |row| row.get(0),
        )?;
        if !has_dirty {
            return Ok(true);
        }
        let query_only: bool = self.conn.query_row("PRAGMA query_only", [], |row| row.get(0))?;
        if query_only {
            return Ok(false);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        flush_dirty_skill_tx(&tx)?;
        tx.commit()?;
        let remaining: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM skill_dirty_sessions)",
            [],
            |row| row.get(0),
        )?;
        Ok(!remaining)
    }

    pub(crate) fn backfill_skill_usage_batch(&self) -> Result<bool> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let state: Option<(String, Option<String>)> = tx
            .query_row(
                "SELECT phase, detail FROM background_job_state WHERE job = ?1",
                [SKILL_USAGE_JOB],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((phase, detail)) = state else {
            return Ok(false);
        };
        let more = match phase.as_str() {
            "scan" => backfill_skill_scan_batch(&tx, detail.as_deref())?,
            "finalize" => backfill_skill_finalize_batch(&tx)?,
            "done" => false,
            other => anyhow::bail!("Invalid skill usage backfill phase: {other}"),
        };
        tx.commit()?;
        Ok(more)
    }

    pub(crate) fn list_indexed_skill_usage(
        &self,
        sources: Option<&[String]>,
    ) -> Result<Vec<IndexedSkillUsageRow>> {
        let mut sql = String::from(
            "SELECT u.session_id, u.raw_skill, u.signal, u.max_timestamp,
                    u.has_null_timestamp, COALESCE(s.updated_at, s.started_at)
             FROM session_skill_usage u
             JOIN sessions s ON s.id = u.session_id
             WHERE 1 = 1",
        );
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        if let Some(source_ids) = sources
            && !source_ids.is_empty()
        {
            sql.push_str(" AND u.source IN (");
            for (i, source_id) in source_ids.iter().enumerate() {
                if i > 0 {
                    sql.push_str(", ");
                }
                sql.push_str(&format!("?{}", i + 1));
                params.push(Box::new(source_id.clone()));
            }
            sql.push(')');
        }
        sql.push_str(" ORDER BY u.session_id ASC, u.raw_skill ASC, u.signal ASC");

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(param_refs.as_slice(), |row| {
            Ok(IndexedSkillUsageRow {
                session_id: row.get(0)?,
                raw_skill: row.get(1)?,
                signal: row.get(2)?,
                max_timestamp: row.get(3)?,
                has_null_timestamp: row.get::<_, i64>(4)? != 0,
                session_ts: row.get(5)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub(crate) fn list_skill_audit_events(
        &self,
        sources: Option<&[String]>,
        time_range: TimeRange,
    ) -> Result<Vec<SkillAuditEventRow>> {
        let mut sql = String::from(
            "SELECT e.session_id,
                    COALESCE(e.timestamp, s.updated_at, s.started_at) AS timestamp,
                    e.name, e.target, e.attrs_json
             FROM session_events e
             JOIN sessions s ON s.id = e.session_id
             WHERE e.kind != 'tool_result' AND (LOWER(e.name) IN ('skill', 'use_skill') OR e.target LIKE '%/skills/%/SKILL.md%'
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
                timestamp: row.get(1)?,
                name: row.get(2)?,
                target: row.get(3)?,
                attrs_json: row.get(4)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}

pub(super) fn rebuild_session_skill_usage_from_raw_tx(
    tx: &Transaction<'_>,
    session_id: &str,
    source: &str,
    session_events: &[RawSessionEvent],
) -> Result<()> {
    let phase: Option<String> = tx
        .query_row(
            "SELECT phase FROM background_job_state WHERE job = ?1",
            [SKILL_USAGE_JOB],
            |row| row.get(0),
        )
        .optional()?;
    if phase.as_deref() != Some("done") {
        return Ok(());
    }

    tx.execute("DELETE FROM session_skill_usage WHERE session_id = ?1", [session_id])?;
    let mut aggs: BTreeMap<SkillAggregateKey, SkillAggregateVal> = BTreeMap::new();
    for event in session_events {
        if let Some((raw_skill, signal)) = crate::skill_audit::classify_skill_event(
            &event.kind,
            event.name.as_deref(),
            event.target.as_deref(),
            event.attrs_json.as_deref(),
        ) {
            record_skill_agg(
                &mut aggs,
                session_id.to_string(),
                source.to_string(),
                raw_skill,
                signal,
                event.timestamp,
            );
        }
    }
    upsert_skill_aggregates(tx, aggs)?;
    tx.execute("DELETE FROM skill_dirty_sessions WHERE session_id = ?1", [session_id])?;
    Ok(())
}

pub(super) fn flush_dirty_skill_tx(tx: &Transaction<'_>) -> Result<()> {
    let phase: Option<String> = tx
        .query_row(
            "SELECT phase FROM background_job_state WHERE job = ?1",
            [SKILL_USAGE_JOB],
            |row| row.get(0),
        )
        .optional()?;
    if phase.as_deref() != Some("done") {
        return Ok(());
    }

    let dirty_sessions: Vec<String> = {
        let mut stmt = tx.prepare("SELECT session_id FROM skill_dirty_sessions")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    if dirty_sessions.is_empty() {
        return Ok(());
    }
    for sid in &dirty_sessions {
        rebuild_session_skill_usage_from_db_tx(tx, sid)?;
    }
    tx.execute("DELETE FROM skill_dirty_sessions", [])?;
    Ok(())
}

fn rebuild_session_skill_usage_from_db_tx(tx: &Transaction<'_>, session_id: &str) -> Result<()> {
    tx.execute("DELETE FROM session_skill_usage WHERE session_id = ?1", [session_id])?;
    let mut aggs: BTreeMap<SkillAggregateKey, SkillAggregateVal> = BTreeMap::new();
    {
        let mut stmt = tx.prepare(
            "SELECT source, timestamp, kind, name, target,
                    CASE WHEN LOWER(name) IN ('skill', 'use_skill') THEN attrs_json END
             FROM session_events
             WHERE session_id = ?1",
        )?;
        let rows = stmt.query_map([session_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })?;
        for row in rows {
            let (source, timestamp, kind, name, target, attrs_json) = row?;
            if let Some((raw_skill, signal)) = crate::skill_audit::classify_skill_event(
                &kind,
                name.as_deref(),
                target.as_deref(),
                attrs_json.as_deref(),
            ) {
                record_skill_agg(
                    &mut aggs,
                    session_id.to_string(),
                    source,
                    raw_skill,
                    signal,
                    timestamp,
                );
            }
        }
    }
    upsert_skill_aggregates(tx, aggs)
}

fn backfill_skill_scan_batch(tx: &Transaction<'_>, detail: Option<&str>) -> Result<bool> {
    let parsed: serde_json::Value =
        detail.map(serde_json::from_str).transpose()?.unwrap_or_else(|| serde_json::json!({}));
    let cursor = parsed.get("cursor").and_then(serde_json::Value::as_i64).unwrap_or(0);
    let end = parsed.get("end").and_then(serde_json::Value::as_i64).unwrap_or(0);

    let rows: Vec<SkillScanRow> = {
        let mut stmt = tx.prepare(
            "SELECT id, session_id, source, timestamp, kind, name, target,
                    CASE WHEN LOWER(name) IN ('skill', 'use_skill') THEN attrs_json END
             FROM session_events
             WHERE id > ?1 AND id <= ?2
             ORDER BY id
             LIMIT ?3",
        )?;
        stmt.query_map(rusqlite::params![cursor, end, SKILL_SCAN_BATCH], |row| {
            Ok(SkillScanRow {
                id: row.get(0)?,
                session_id: row.get(1)?,
                source: row.get(2)?,
                timestamp: row.get(3)?,
                kind: row.get(4)?,
                name: row.get(5)?,
                target: row.get(6)?,
                attrs_json: row.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?
    };

    if let Some(last_id) = rows.last().map(|r| r.id) {
        let mut aggs: BTreeMap<SkillAggregateKey, SkillAggregateVal> = BTreeMap::new();
        for row in rows {
            if let Some((raw_skill, signal)) = crate::skill_audit::classify_skill_event(
                &row.kind,
                row.name.as_deref(),
                row.target.as_deref(),
                row.attrs_json.as_deref(),
            ) {
                record_skill_agg(
                    &mut aggs,
                    row.session_id,
                    row.source,
                    raw_skill,
                    signal,
                    row.timestamp,
                );
            }
        }
        upsert_skill_aggregates(tx, aggs)?;
        let next_detail = serde_json::json!({"cursor": last_id, "end": end}).to_string();
        tx.execute(
            "UPDATE background_job_state SET detail = ?2, updated_at = unixepoch() * 1000 WHERE job = ?1",
            rusqlite::params![SKILL_USAGE_JOB, next_detail],
        )?;
    } else {
        tx.execute(
            "UPDATE background_job_state SET phase = 'finalize', detail = NULL, updated_at = unixepoch() * 1000 WHERE job = ?1",
            [SKILL_USAGE_JOB],
        )?;
    }
    Ok(true)
}

fn backfill_skill_finalize_batch(tx: &Transaction<'_>) -> Result<bool> {
    let dirty_sessions: Vec<String> = {
        let mut stmt = tx.prepare("SELECT session_id FROM skill_dirty_sessions LIMIT ?1")?;
        stmt.query_map([SKILL_FINALIZE_BATCH as i64], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    if !dirty_sessions.is_empty() {
        for sid in &dirty_sessions {
            rebuild_session_skill_usage_from_db_tx(tx, sid)?;
            tx.execute("DELETE FROM skill_dirty_sessions WHERE session_id = ?1", [sid])?;
        }
        return Ok(true);
    }

    tx.execute(
        "UPDATE background_job_state SET phase = 'done', detail = NULL, updated_at = unixepoch() * 1000 WHERE job = ?1",
        [SKILL_USAGE_JOB],
    )?;
    Ok(false)
}

fn record_skill_agg(
    aggs: &mut BTreeMap<SkillAggregateKey, SkillAggregateVal>,
    session_id: String,
    source: String,
    raw_skill: String,
    signal: &'static str,
    timestamp: Option<i64>,
) {
    let entry = aggs.entry((session_id, source, raw_skill, signal)).or_insert((None, 0));
    match timestamp {
        Some(ts) => entry.0 = Some(entry.0.map_or(ts, |cur| cur.max(ts))),
        None => entry.1 = 1,
    }
}

fn upsert_skill_aggregates(
    tx: &Transaction<'_>,
    aggs: BTreeMap<SkillAggregateKey, SkillAggregateVal>,
) -> Result<()> {
    if aggs.is_empty() {
        return Ok(());
    }
    let mut stmt = tx.prepare(
        "INSERT INTO session_skill_usage(
            session_id, source, raw_skill, signal, max_timestamp, has_null_timestamp
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(session_id, raw_skill, signal) DO UPDATE SET
            max_timestamp = CASE
                WHEN session_skill_usage.max_timestamp IS NULL THEN excluded.max_timestamp
                WHEN excluded.max_timestamp IS NULL THEN session_skill_usage.max_timestamp
                ELSE MAX(session_skill_usage.max_timestamp, excluded.max_timestamp)
            END,
            has_null_timestamp = MAX(session_skill_usage.has_null_timestamp, excluded.has_null_timestamp)",
    )?;
    for ((session_id, source, raw_skill, signal), (max_ts, has_null)) in aggs {
        stmt.execute(rusqlite::params![session_id, source, raw_skill, signal, max_ts, has_null])?;
    }
    Ok(())
}
