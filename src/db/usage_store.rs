mod backfill;
mod dirty;
mod queries;
mod rollups;
pub(super) use dirty::flush_dirty_usage_tx;

use std::collections::{HashMap, hash_map::Entry};

use anyhow::Result;
use chrono::Utc;

use crate::types::{RawUsageEvent, UsageEventRecord};
use crate::usage::TokenTotals;

const CODEX_DISJOINT_REASONING_VERSION: u32 = 5;
const CACHE_EXCLUSIVE_INPUT_VERSION: u32 = 2;
const FLEXIBLE_REASONING_VERSION: u32 = 2;
pub(super) const BUCKET_MILLIS: i64 = 900_000;
pub(super) const USAGE_SCAN_BATCH: i64 = 5_000;
pub(super) const USAGE_DEDUP_BATCH: usize = 2_000;
pub(super) const USAGE_FINALIZE_SESSION_BATCH: usize = 200;
pub(super) const USAGE_FINALIZE_GROUP_BATCH: usize = 500;

pub(super) struct PersistedUsage {
    pub(super) input_tokens: i64,
    pub(super) output_tokens: i64,
    pub(super) cache_read_tokens: i64,
    pub(super) cache_write_tokens: i64,
    pub(super) reasoning_tokens: i64,
    pub(super) parser_version: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct UsageSlice {
    pub(crate) session_id: String,
    pub(crate) source: String,
    pub(crate) timestamp: i64,
    pub(crate) model: String,
    pub(crate) provider: String,
    pub(crate) input_tokens: i64,
    pub(crate) output_tokens: i64,
    pub(crate) cache_read_tokens: i64,
    pub(crate) cache_write_tokens: i64,
    pub(crate) reasoning_tokens: i64,
    pub(crate) token_source: String,
    pub(crate) events: usize,
}

impl UsageSlice {
    pub(crate) fn from_event(event: UsageEventRecord) -> Self {
        Self {
            session_id: event.session_id,
            source: event.source,
            timestamp: event.timestamp,
            model: event.model,
            provider: event.provider,
            input_tokens: event.input_tokens,
            output_tokens: event.output_tokens,
            cache_read_tokens: event.cache_read_tokens,
            cache_write_tokens: event.cache_write_tokens,
            reasoning_tokens: event.reasoning_tokens,
            token_source: event.token_source,
            events: 1,
        }
    }

    pub(crate) fn total_tokens(&self) -> i64 {
        self.input_tokens
            + self.output_tokens
            + self.cache_read_tokens
            + self.cache_write_tokens
            + self.reasoning_tokens
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SessionUsage {
    pub(crate) tokens: TokenTotals,
    pub(crate) shared_with_other_session: bool,
}

#[derive(PartialEq)]
pub(super) enum UsageOwnership {
    Counted,
    Duplicate,
    OtherSession,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn dedup_group_key_from_parts(
    source: &str,
    event_key: &str,
    timestamp: i64,
    provider: &str,
    model: &str,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    reasoning_tokens: i64,
) -> Option<String> {
    let key = match source {
        "codex" => format!(
            "codex:token_count:{timestamp}:{provider}:{model}:{input_tokens}:{output_tokens}:{cache_read_tokens}:{cache_write_tokens}:{reasoning_tokens}"
        ),
        "claude-code" if event_key.starts_with("assistant:") && !event_key.contains(":line:") => {
            event_key.to_string()
        }
        "warp" => event_key.to_string(),
        "qoder"
            if event_key.starts_with("assistant:") && !event_key.starts_with("assistant:line:") =>
        {
            event_key.to_string()
        }
        "antigravity" | "antigravity-cli"
            if event_key.starts_with("response:") || event_key.starts_with("request:") =>
        {
            event_key.to_string()
        }
        _ => return None,
    };
    Some(format!("{source}\0{key}"))
}

#[derive(Default)]
pub(super) struct UsageDedup {
    owners: HashMap<String, String>,
}

impl UsageDedup {
    pub(super) fn assign(&mut self, event: &UsageEventRecord) -> UsageOwnership {
        let Some(group) = dedup_group_key_from_parts(
            &event.source,
            &event.event_key,
            event.timestamp,
            &event.provider,
            &event.model,
            event.input_tokens,
            event.output_tokens,
            event.cache_read_tokens,
            event.cache_write_tokens,
            event.reasoning_tokens,
        ) else {
            return UsageOwnership::Counted;
        };
        match self.owners.entry(group) {
            Entry::Vacant(entry) => {
                entry.insert(event.session_id.clone());
                UsageOwnership::Counted
            }
            Entry::Occupied(entry) if entry.get() == &event.session_id => UsageOwnership::Duplicate,
            Entry::Occupied(_) => UsageOwnership::OtherSession,
        }
    }
}

pub(super) struct OwnershipSourceRow {
    pub(super) event_id: i64,
    pub(super) source: String,
    pub(super) event_seq: i64,
    pub(super) timestamp: i64,
    pub(super) event_key: String,
    pub(super) model: String,
    pub(super) provider: String,
    pub(super) input_tokens: i64,
    pub(super) output_tokens: i64,
    pub(super) cache_read_tokens: i64,
    pub(super) cache_write_tokens: i64,
    pub(super) reasoning_tokens: i64,
    pub(super) parser_version: u32,
    pub(super) raw_usage_json: Option<String>,
}

pub(super) struct UsageScanRow {
    pub(super) event_id: i64,
    pub(super) session_id: String,
    pub(super) source: String,
    pub(super) event_seq: i64,
    pub(super) timestamp: i64,
    pub(super) event_key: String,
    pub(super) model: String,
    pub(super) provider: String,
    pub(super) input_tokens: i64,
    pub(super) output_tokens: i64,
    pub(super) cache_read_tokens: i64,
    pub(super) cache_write_tokens: i64,
    pub(super) reasoning_tokens: i64,
    pub(super) token_source: String,
    pub(super) parser_version: u32,
    pub(super) raw_usage_json: Option<String>,
}

pub(super) fn normalize_persisted_usage(
    source: &str,
    mut usage: PersistedUsage,
    raw_usage_json: Option<&str>,
) -> PersistedUsage {
    match source {
        "codex" if usage.parser_version < CODEX_DISJOINT_REASONING_VERSION => {
            split_inclusive_output(&mut usage);
            usage.parser_version = CODEX_DISJOINT_REASONING_VERSION;
        }
        "gemini-cli" | "qwen-code" if usage.parser_version < CACHE_EXCLUSIVE_INPUT_VERSION => {
            usage.input_tokens = usage.input_tokens.saturating_sub(usage.cache_read_tokens).max(0);
            usage.parser_version = CACHE_EXCLUSIVE_INPUT_VERSION;
        }
        "pi" | "omp" if usage.parser_version < FLEXIBLE_REASONING_VERSION => {
            if persisted_usage_total(raw_usage_json)
                == Some(
                    usage
                        .input_tokens
                        .saturating_add(usage.output_tokens)
                        .saturating_add(usage.cache_read_tokens)
                        .saturating_add(usage.cache_write_tokens),
                )
            {
                split_inclusive_output(&mut usage);
            }
            usage.parser_version = FLEXIBLE_REASONING_VERSION;
        }
        _ => {}
    }

    usage
}

fn split_inclusive_output(usage: &mut PersistedUsage) {
    let reasoning_tokens = usage.reasoning_tokens.min(usage.output_tokens);
    usage.output_tokens -= reasoning_tokens;
    usage.reasoning_tokens = reasoning_tokens;
}

fn persisted_usage_total(raw_usage_json: Option<&str>) -> Option<i64> {
    let usage = serde_json::from_str::<serde_json::Value>(raw_usage_json?).ok()?;
    ["totalTokens", "total_tokens"]
        .iter()
        .find_map(|key| usage.get(*key).and_then(serde_json::Value::as_i64))
        .filter(|total| *total >= 0)
}

pub(super) enum UsageConflict {
    Reject,
    Update,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn replace_usage_events(
    tx: &rusqlite::Transaction<'_>,
    session_id: &str,
    source: &str,
    source_id: &str,
    usage_events: &[RawUsageEvent],
    usage_parser_version: Option<u32>,
    source_updated_at: Option<i64>,
    conflict: UsageConflict,
) -> Result<()> {
    {
        tx.execute(
            "DELETE FROM usage_events WHERE session_id = ?1",
            rusqlite::params![session_id],
        )?;
        let created_at = Utc::now().timestamp_millis();
        let conflict = match conflict {
            UsageConflict::Reject => "",
            UsageConflict::Update => {
                "
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
                raw_usage_json = excluded.raw_usage_json"
            }
        };
        let mut stmt = tx.prepare(&format!(
            "INSERT INTO usage_events (
                session_id, source, source_id, event_key, event_seq, message_seq,
                timestamp, model, provider, input_tokens, output_tokens,
                cache_read_tokens, cache_write_tokens, reasoning_tokens,
                token_source, parser_version, source_path, raw_usage_json, created_at
             )
             VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19
             ){conflict}",
        ))?;
        for event in usage_events {
            stmt.execute(rusqlite::params![
                session_id,
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
    }

    if let Some(parser_version) = usage_parser_version {
        let synced_at = Utc::now().timestamp_millis();
        tx.execute(
            "INSERT INTO usage_session_state (
                session_id, source, source_id, parser_version,
                source_updated_at, event_count, synced_at
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(session_id) DO UPDATE SET
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
                usage_events.len() as u32,
                synced_at,
            ],
        )?;
    }

    Ok(())
}

pub(super) fn push_source_filter(
    sql: &mut String,
    params: &mut Vec<Box<dyn rusqlite::types::ToSql>>,
    start_idx: usize,
    column: &str,
    sources: Option<&[String]>,
) {
    if let Some(source_ids) = sources
        && !source_ids.is_empty()
    {
        sql.push_str(&format!(" AND {column} IN ("));
        for (i, source_id) in source_ids.iter().enumerate() {
            if i > 0 {
                sql.push_str(", ");
            }
            sql.push_str(&format!("?{}", start_idx + i));
            params.push(Box::new(source_id.clone()));
        }
        sql.push(')');
    }
}

pub(super) fn map_usage_event_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<UsageEventRecord> {
    let source = row.get::<_, String>(1)?;
    let raw_usage_json = row.get::<_, Option<String>>(14)?;
    let usage = normalize_persisted_usage(
        &source,
        PersistedUsage {
            input_tokens: row.get(7)?,
            output_tokens: row.get(8)?,
            cache_read_tokens: row.get(9)?,
            cache_write_tokens: row.get(10)?,
            reasoning_tokens: row.get(11)?,
            parser_version: row.get(13)?,
        },
        raw_usage_json.as_deref(),
    );
    Ok(UsageEventRecord {
        session_id: row.get(0)?,
        source,
        source_id: row.get(2)?,
        event_key: row.get(3)?,
        timestamp: row.get(4)?,
        model: row.get(5)?,
        provider: row.get(6)?,
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_tokens: usage.cache_read_tokens,
        cache_write_tokens: usage.cache_write_tokens,
        reasoning_tokens: usage.reasoning_tokens,
        token_source: row.get(12)?,
    })
}
