use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use rusqlite::Transaction;

use super::super::schema::USAGE_INDEX_JOB;
use super::dirty::{
    RollupAccum, rebuild_session_ownership_tx, rebuild_session_rollups_tx, recompute_dedup_group_tx,
};
use super::{
    BUCKET_MILLIS, PersistedUsage, USAGE_DEDUP_BATCH, USAGE_FINALIZE_GROUP_BATCH,
    USAGE_FINALIZE_SESSION_BATCH, USAGE_SCAN_BATCH, UsageScanRow, dedup_group_key_from_parts,
    normalize_persisted_usage,
};

pub(super) fn backfill_usage_scan_batch(
    tx: &Transaction<'_>,
    detail: Option<&str>,
) -> Result<bool> {
    let parsed: serde_json::Value =
        detail.map(serde_json::from_str).transpose()?.unwrap_or_else(|| serde_json::json!({}));
    let cursor = parsed.get("cursor").and_then(serde_json::Value::as_i64).unwrap_or(0);
    let end = parsed.get("end").and_then(serde_json::Value::as_i64).unwrap_or(0);

    let rows: Vec<UsageScanRow> = {
        let mut stmt = tx.prepare(
            "SELECT id, session_id, source, event_seq, timestamp, event_key, model, provider,
                    input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                    reasoning_tokens, token_source, parser_version, raw_usage_json
             FROM usage_events
             WHERE id > ?1 AND id <= ?2
             ORDER BY id
             LIMIT ?3",
        )?;
        stmt.query_map(rusqlite::params![cursor, end, USAGE_SCAN_BATCH], |row| {
            Ok(UsageScanRow {
                event_id: row.get(0)?,
                session_id: row.get(1)?,
                source: row.get(2)?,
                event_seq: row.get(3)?,
                timestamp: row.get(4)?,
                event_key: row.get(5)?,
                model: row.get(6)?,
                provider: row.get(7)?,
                input_tokens: row.get(8)?,
                output_tokens: row.get(9)?,
                cache_read_tokens: row.get(10)?,
                cache_write_tokens: row.get(11)?,
                reasoning_tokens: row.get(12)?,
                token_source: row.get(13)?,
                parser_version: row.get(14)?,
                raw_usage_json: row.get(15)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?
    };

    if let Some(last_id) = rows.last().map(|r| r.event_id) {
        let mut rollups: BTreeMap<(i64, String, String, String, String, String), RollupAccum> =
            BTreeMap::new();
        {
            let mut own_stmt = tx.prepare(
                "INSERT OR IGNORE INTO usage_event_ownership(
                    event_id, session_id, dedup_group, timestamp, event_seq, event_key, ownership
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1)",
            )?;
            for row in &rows {
                let usage = normalize_persisted_usage(
                    &row.source,
                    PersistedUsage {
                        input_tokens: row.input_tokens,
                        output_tokens: row.output_tokens,
                        cache_read_tokens: row.cache_read_tokens,
                        cache_write_tokens: row.cache_write_tokens,
                        reasoning_tokens: row.reasoning_tokens,
                        parser_version: row.parser_version,
                    },
                    row.raw_usage_json.as_deref(),
                );
                let group = dedup_group_key_from_parts(
                    &row.source,
                    &row.event_key,
                    row.timestamp,
                    &row.provider,
                    &row.model,
                    usage.input_tokens,
                    usage.output_tokens,
                    usage.cache_read_tokens,
                    usage.cache_write_tokens,
                    usage.reasoning_tokens,
                );
                own_stmt.execute(rusqlite::params![
                    row.event_id,
                    &row.session_id,
                    group,
                    row.timestamp,
                    row.event_seq,
                    &row.event_key,
                ])?;
                let bucket_ms = row.timestamp.div_euclid(BUCKET_MILLIS) * BUCKET_MILLIS;
                let entry = rollups
                    .entry((
                        bucket_ms,
                        row.source.clone(),
                        row.provider.clone(),
                        row.model.clone(),
                        row.token_source.clone(),
                        row.session_id.clone(),
                    ))
                    .or_insert((0, 0, 0, 0, 0, 0));
                entry.0 += 1;
                entry.1 += usage.input_tokens;
                entry.2 += usage.output_tokens;
                entry.3 += usage.cache_read_tokens;
                entry.4 += usage.cache_write_tokens;
                entry.5 += usage.reasoning_tokens;
            }
        }
        {
            let mut rollup_stmt = tx.prepare(
                "INSERT INTO usage_rollups(
                    bucket_ms, source, provider, model, token_source, session_id,
                    events, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                 ON CONFLICT(bucket_ms, source, provider, model, token_source, session_id) DO UPDATE SET
                    events = usage_rollups.events + excluded.events,
                    input_tokens = usage_rollups.input_tokens + excluded.input_tokens,
                    output_tokens = usage_rollups.output_tokens + excluded.output_tokens,
                    cache_read_tokens = usage_rollups.cache_read_tokens + excluded.cache_read_tokens,
                    cache_write_tokens = usage_rollups.cache_write_tokens + excluded.cache_write_tokens,
                    reasoning_tokens = usage_rollups.reasoning_tokens + excluded.reasoning_tokens",
            )?;
            for ((bucket_ms, source, provider, model, token_source, session_id), sums) in rollups {
                rollup_stmt.execute(rusqlite::params![
                    bucket_ms,
                    source,
                    provider,
                    model,
                    token_source,
                    session_id,
                    sums.0,
                    sums.1,
                    sums.2,
                    sums.3,
                    sums.4,
                    sums.5,
                ])?;
            }
        }
        let next_detail = serde_json::json!({"cursor": last_id, "end": end}).to_string();
        tx.execute(
            "UPDATE background_job_state SET detail = ?2, updated_at = unixepoch() * 1000 WHERE job = ?1",
            rusqlite::params![USAGE_INDEX_JOB, next_detail],
        )?;
    } else {
        let next_detail = serde_json::json!({"group_cursor": ""}).to_string();
        tx.execute(
            "UPDATE background_job_state SET phase = 'dedup', detail = ?2, updated_at = unixepoch() * 1000 WHERE job = ?1",
            rusqlite::params![USAGE_INDEX_JOB, next_detail],
        )?;
    }
    Ok(true)
}

pub(super) fn backfill_usage_dedup_batch(
    tx: &Transaction<'_>,
    detail: Option<&str>,
) -> Result<bool> {
    let parsed: serde_json::Value =
        detail.map(serde_json::from_str).transpose()?.unwrap_or_else(|| serde_json::json!({}));
    let group_cursor = parsed.get("group_cursor").and_then(serde_json::Value::as_str).unwrap_or("");

    let groups: Vec<String> = {
        let mut stmt = tx.prepare(
            "SELECT dedup_group
             FROM usage_event_ownership
             WHERE dedup_group IS NOT NULL AND dedup_group > ?1
             GROUP BY dedup_group
             HAVING COUNT(*) > 1
             ORDER BY dedup_group
             LIMIT ?2",
        )?;
        stmt.query_map(rusqlite::params![group_cursor, USAGE_DEDUP_BATCH as i64], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };

    if let Some(last_group) = groups.last().cloned() {
        let mut affected_sessions = BTreeSet::new();
        for group in &groups {
            recompute_dedup_group_tx(tx, group, &mut affected_sessions)?;
        }
        if !affected_sessions.is_empty() {
            let mut dirty_stmt = tx.prepare(
                "INSERT INTO usage_dirty_sessions(session_id) VALUES (?1) ON CONFLICT DO NOTHING",
            )?;
            for sid in affected_sessions {
                dirty_stmt.execute([sid])?;
            }
        }
        let next_detail = serde_json::json!({"group_cursor": last_group}).to_string();
        tx.execute(
            "UPDATE background_job_state SET detail = ?2, updated_at = unixepoch() * 1000 WHERE job = ?1",
            rusqlite::params![USAGE_INDEX_JOB, next_detail],
        )?;
    } else {
        tx.execute(
            "UPDATE background_job_state SET phase = 'finalize', detail = NULL, updated_at = unixepoch() * 1000 WHERE job = ?1",
            [USAGE_INDEX_JOB],
        )?;
    }
    Ok(true)
}

pub(super) fn backfill_usage_finalize_batch(tx: &Transaction<'_>) -> Result<bool> {
    let dirty_sessions: Vec<String> = {
        let mut stmt = tx.prepare("SELECT session_id FROM usage_dirty_sessions LIMIT ?1")?;
        stmt.query_map([USAGE_FINALIZE_SESSION_BATCH as i64], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    if !dirty_sessions.is_empty() {
        let mut dirty_groups = BTreeSet::new();
        for sid in &dirty_sessions {
            rebuild_session_ownership_tx(tx, sid, &mut dirty_groups)?;
            rebuild_session_rollups_tx(tx, sid)?;
            tx.execute("DELETE FROM usage_dirty_sessions WHERE session_id = ?1", [sid])?;
        }
        if !dirty_groups.is_empty() {
            let mut group_stmt = tx.prepare(
                "INSERT INTO usage_dirty_groups(dedup_group) VALUES (?1) ON CONFLICT DO NOTHING",
            )?;
            for group in dirty_groups {
                group_stmt.execute([group])?;
            }
        }
        return Ok(true);
    }

    let dirty_groups: Vec<String> = {
        let mut stmt = tx.prepare("SELECT dedup_group FROM usage_dirty_groups LIMIT ?1")?;
        stmt.query_map([USAGE_FINALIZE_GROUP_BATCH as i64], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    if !dirty_groups.is_empty() {
        let mut affected_sessions = BTreeSet::new();
        for group in &dirty_groups {
            recompute_dedup_group_tx(tx, group, &mut affected_sessions)?;
            tx.execute("DELETE FROM usage_dirty_groups WHERE dedup_group = ?1", [group])?;
        }
        for sid in affected_sessions {
            rebuild_session_rollups_tx(tx, &sid)?;
        }
        return Ok(true);
    }

    tx.execute(
        "UPDATE background_job_state SET phase = 'done', detail = NULL, updated_at = unixepoch() * 1000 WHERE job = ?1",
        [USAGE_INDEX_JOB],
    )?;
    Ok(false)
}
