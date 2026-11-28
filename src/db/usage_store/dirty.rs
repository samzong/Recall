use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use rusqlite::{OptionalExtension, Transaction};

use super::super::schema::USAGE_INDEX_JOB;
use super::{
    OwnershipSourceRow, PersistedUsage, codex_mirror_row_suppressed, dedup_group_key_from_parts,
    normalize_persisted_usage, validate_usage_amounts,
};
pub(super) fn has_dirty_usage(conn: &rusqlite::Connection) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM usage_dirty_sessions) OR EXISTS(SELECT 1 FROM usage_dirty_groups)",
        [],
        |row| row.get(0),
    )?)
}

pub(in super::super) fn flush_dirty_usage_tx(tx: &Transaction<'_>) -> Result<()> {
    let phase: Option<String> = tx
        .query_row(
            "SELECT phase FROM background_job_state WHERE job = ?1",
            [USAGE_INDEX_JOB],
            |row| row.get(0),
        )
        .optional()?;
    if phase.as_deref() != Some("done") {
        return Ok(());
    }

    let dirty_sessions: BTreeSet<String> = {
        let mut stmt = tx.prepare("SELECT session_id FROM usage_dirty_sessions")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut dirty_groups: BTreeSet<String> = {
        let mut stmt = tx.prepare("SELECT dedup_group FROM usage_dirty_groups")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    if dirty_sessions.is_empty() && dirty_groups.is_empty() {
        return Ok(());
    }

    for sid in &dirty_sessions {
        rebuild_session_ownership_tx(tx, sid, &mut dirty_groups)?;
    }

    let mut rollup_sessions = dirty_sessions;
    for group in &dirty_groups {
        recompute_dedup_group_tx(tx, group, &mut rollup_sessions)?;
    }
    for sid in &rollup_sessions {
        rebuild_session_rollups_tx(tx, sid)?;
    }

    tx.execute("DELETE FROM usage_dirty_sessions", [])?;
    tx.execute("DELETE FROM usage_dirty_groups", [])?;
    Ok(())
}

pub(super) fn rebuild_session_ownership_tx(
    tx: &Transaction<'_>,
    session_id: &str,
    dirty_groups: &mut BTreeSet<String>,
) -> Result<()> {
    {
        let mut old_groups = tx.prepare(
            "SELECT DISTINCT dedup_group FROM usage_event_ownership
             WHERE session_id = ?1 AND dedup_group IS NOT NULL",
        )?;
        let rows = old_groups.query_map([session_id], |row| row.get::<_, String>(0))?;
        for group in rows {
            dirty_groups.insert(group?);
        }
    }
    tx.execute("DELETE FROM usage_event_ownership WHERE session_id = ?1", [session_id])?;

    let rows: Vec<OwnershipSourceRow> = {
        let mut stmt = tx.prepare(
            "SELECT ue.id, ue.source, s.source_id, ue.event_seq, ue.timestamp, ue.event_key,
                    ue.model, ue.provider, ue.input_tokens, ue.output_tokens,
                    ue.cache_read_tokens, ue.cache_write_tokens, ue.reasoning_tokens,
                    ue.parser_version, ue.raw_usage_json
             FROM usage_events ue
             JOIN sessions s ON s.id = ue.session_id
             WHERE ue.session_id = ?1",
        )?;
        stmt.query_map([session_id], |row| {
            Ok(OwnershipSourceRow {
                event_id: row.get(0)?,
                source: row.get(1)?,
                source_id: row.get(2)?,
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
                parser_version: row.get(13)?,
                raw_usage_json: row.get(14)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?
    };

    let mut insert = tx.prepare(
        "INSERT INTO usage_event_ownership(
            event_id, session_id, dedup_group, timestamp, event_seq, event_key, ownership
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;
    for row in rows {
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
        validate_usage_amounts(
            row.timestamp,
            [
                row.input_tokens,
                row.output_tokens,
                row.cache_read_tokens,
                row.cache_write_tokens,
                row.reasoning_tokens,
            ],
        )?;
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
        if let Some(group_key) = group.as_ref() {
            dirty_groups.insert(group_key.clone());
        }
        let ownership = if codex_mirror_row_suppressed(
            tx,
            session_id,
            &row.source,
            &row.source_id,
            row.timestamp,
            row.raw_usage_json.as_deref(),
        )? {
            3
        } else {
            1
        };
        insert.execute(rusqlite::params![
            row.event_id,
            session_id,
            group,
            row.timestamp,
            row.event_seq,
            row.event_key,
            ownership,
        ])?;
    }
    Ok(())
}

pub(super) fn recompute_dedup_group_tx(
    tx: &Transaction<'_>,
    group: &str,
    affected_sessions: &mut BTreeSet<String>,
) -> Result<()> {
    struct GroupMember {
        event_id: i64,
        session_id: String,
        current: i64,
        source: String,
        source_id: String,
        timestamp: i64,
        raw_usage_json: Option<String>,
    }

    let rows: Vec<GroupMember> = {
        let mut stmt = tx.prepare(
            "SELECT o.event_id, ue.session_id, o.ownership, ue.source, s.source_id, ue.timestamp, ue.raw_usage_json
             FROM usage_event_ownership o
             JOIN usage_events ue ON ue.id = o.event_id
             JOIN sessions s ON s.id = ue.session_id
             LEFT JOIN session_sync ss ON ss.session_id = s.id
             WHERE o.dedup_group = ?1
             ORDER BY ue.timestamp ASC,
              ue.event_seq ASC,
              s.source_id ASC,
              (NOT EXISTS (SELECT 1 FROM native_bindings nb WHERE nb.session_id = s.id)) ASC,
              CASE WHEN EXISTS (SELECT 1 FROM native_bindings nb WHERE nb.session_id = s.id)
                   THEN '' ELSE COALESCE(ss.sync_id, '') END ASC,
              ue.event_key ASC,
              ue.session_id ASC",
        )?;
        stmt.query_map([group], |row| {
            Ok(GroupMember {
                event_id: row.get(0)?,
                session_id: row.get(1)?,
                current: row.get(2)?,
                source: row.get(3)?,
                source_id: row.get(4)?,
                timestamp: row.get(5)?,
                raw_usage_json: row.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?
    };
    let mut suppressed_flags = Vec::with_capacity(rows.len());
    let mut winner_sid: Option<String> = None;
    for row in &rows {
        let suppressed = codex_mirror_row_suppressed(
            tx,
            &row.session_id,
            &row.source,
            &row.source_id,
            row.timestamp,
            row.raw_usage_json.as_deref(),
        )?;
        if !suppressed && winner_sid.is_none() {
            winner_sid = Some(row.session_id.clone());
        }
        suppressed_flags.push(suppressed);
    }
    let mut seen_winner = false;
    let mut update =
        tx.prepare("UPDATE usage_event_ownership SET ownership = ?1 WHERE event_id = ?2")?;
    for (row, suppressed) in rows.into_iter().zip(suppressed_flags) {
        let target = if suppressed {
            3
        } else if !seen_winner {
            seen_winner = true;
            1
        } else if winner_sid.as_deref() == Some(row.session_id.as_str()) {
            2
        } else {
            3
        };
        if row.current != target {
            update.execute(rusqlite::params![target, row.event_id])?;
            if row.current == 1 || target == 1 {
                affected_sessions.insert(row.session_id);
            }
        }
    }
    Ok(())
}

pub(super) type RollupKey = (i64, String, String, String, String);
pub(super) type RollupAccum = (i64, i64, i64, i64, i64, i64);

pub(super) fn rebuild_session_rollups_tx(tx: &Transaction<'_>, session_id: &str) -> Result<()> {
    tx.execute("DELETE FROM usage_rollups WHERE session_id = ?1", [session_id])?;
    let mut buckets: BTreeMap<RollupKey, RollupAccum> = BTreeMap::new();
    {
        let mut stmt = tx.prepare(
            "SELECT u.source, u.timestamp, u.model, u.provider,
                    u.input_tokens, u.output_tokens, u.cache_read_tokens, u.cache_write_tokens,
                    u.reasoning_tokens, u.token_source, u.parser_version, u.raw_usage_json
             FROM usage_events u
             JOIN usage_event_ownership o ON o.event_id = u.id
             WHERE o.session_id = ?1 AND o.ownership = 1",
        )?;
        let rows = stmt.query_map([session_id], |row| {
            let source = row.get::<_, String>(0)?;
            let timestamp = row.get::<_, i64>(1)?;
            let model = row.get::<_, String>(2)?;
            let provider = row.get::<_, String>(3)?;
            let raw_usage_json = row.get::<_, Option<String>>(11)?;
            let usage = normalize_persisted_usage(
                &source,
                PersistedUsage {
                    input_tokens: row.get(4)?,
                    output_tokens: row.get(5)?,
                    cache_read_tokens: row.get(6)?,
                    cache_write_tokens: row.get(7)?,
                    reasoning_tokens: row.get(8)?,
                    parser_version: row.get(10)?,
                },
                raw_usage_json.as_deref(),
            );
            let token_source = row.get::<_, String>(9)?;
            Ok((source, timestamp, provider, model, token_source, usage))
        })?;
        for row in rows {
            let (source, timestamp, provider, model, token_source, usage) = row?;
            let bucket_ms = validate_usage_amounts(
                timestamp,
                [
                    usage.input_tokens,
                    usage.output_tokens,
                    usage.cache_read_tokens,
                    usage.cache_write_tokens,
                    usage.reasoning_tokens,
                ],
            )?;
            let entry = buckets
                .entry((bucket_ms, source, provider, model, token_source))
                .or_insert((0, 0, 0, 0, 0, 0));
            entry.0 = entry.0.saturating_add(1);
            entry.1 = entry.1.saturating_add(usage.input_tokens);
            entry.2 = entry.2.saturating_add(usage.output_tokens);
            entry.3 = entry.3.saturating_add(usage.cache_read_tokens);
            entry.4 = entry.4.saturating_add(usage.cache_write_tokens);
            entry.5 = entry.5.saturating_add(usage.reasoning_tokens);
        }
    }
    let mut insert = tx.prepare(
        "INSERT INTO usage_rollups(
            bucket_ms, source, provider, model, token_source, session_id,
            events, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
    )?;
    for ((bucket_ms, source, provider, model, token_source), sums) in buckets {
        insert.execute(rusqlite::params![
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
    Ok(())
}
