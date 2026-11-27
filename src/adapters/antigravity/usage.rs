use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};

use crate::adapters::paths::file_uri_to_path;
use crate::types::RawUsageEvent;

pub(super) const USAGE_PARSER_VERSION: u32 = 1;

const INHERITED_ORPHAN_SEQ_OFFSET: u32 = 1 << 30;

pub(super) struct ParsedAntigravityDb {
    pub(super) usage_events: Vec<RawUsageEvent>,
    pub(super) created_at_ms: Option<i64>,
    pub(super) workspace_dir: Option<String>,
}

struct StepTimestamps {
    by_response_id: HashMap<String, i64>,
    by_step_idx: HashMap<i64, i64>,
}

struct CandidateRow {
    idx: Option<i64>,
    event_key: String,
    response_id: Option<String>,
    last_step_index: Option<i64>,
    inline_ts: Option<i64>,
    inherited_orphan: bool,
    model_raw: String,
    display_label: Option<String>,
    provider: String,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    reasoning_tokens: i64,
    raw_usage_json: String,
}

pub(super) fn parse_antigravity_db(
    db_path: &Path,
    session_id: &str,
    live_cascade_ids: &HashSet<String>,
    fallback_timestamp_ms: Option<i64>,
) -> anyhow::Result<ParsedAntigravityDb> {
    if fs::metadata(db_path)?.len() == 0 {
        anyhow::bail!("Antigravity usage database is empty");
    }

    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(Duration::from_secs(3))?;
    let tx = conn.unchecked_transaction()?;

    let (created_at_ms, workspace_dir) = if table_exists(&tx, "trajectory_metadata_blob")? {
        read_trajectory_meta(&tx)?
    } else {
        (None, None)
    };

    if !table_exists(&tx, "gen_metadata")? {
        anyhow::bail!("Antigravity usage database has no gen_metadata table");
    }

    let rows: Vec<(Option<i64>, Vec<u8>)> = {
        let mut stmt =
            tx.prepare("SELECT idx, data FROM gen_metadata WHERE data IS NOT NULL ORDER BY idx")?;
        let mapped = stmt.query_map([], |row| Ok((row.get::<_, Option<i64>>(0)?, row.get(1)?)))?;
        mapped.collect::<rusqlite::Result<_>>()?
    };

    if rows.is_empty() {
        return Ok(ParsedAntigravityDb { usage_events: Vec::new(), created_at_ms, workspace_dir });
    }

    let label_models = collect_label_model_map(&rows);
    let mut seen_keys = HashSet::new();
    let mut candidates = Vec::new();

    for (pos, (idx, blob)) in rows.iter().enumerate() {
        let Some(chat_model) = message_field(blob, 1) else {
            continue;
        };
        let Some(usage) = message_field(chat_model, 4) else {
            continue;
        };

        let input_tokens = clamp_i64(varint_field(usage, 2).unwrap_or(0));
        let total_output = clamp_i64(varint_field(usage, 3).unwrap_or(0));
        let cache_write_tokens = clamp_i64(varint_field(usage, 4).unwrap_or(0));
        let cache_read_tokens = clamp_i64(varint_field(usage, 5).unwrap_or(0));
        let provider_enum = varint_field(usage, 6);
        let reasoning_tokens = clamp_i64(varint_field(usage, 9).unwrap_or(0));
        let response_output_tokens = clamp_i64(varint_field(usage, 10).unwrap_or(0));
        let output_tokens = if response_output_tokens > 0 {
            response_output_tokens
        } else {
            total_output.saturating_sub(reasoning_tokens)
        };

        if input_tokens == 0
            && output_tokens == 0
            && cache_read_tokens == 0
            && cache_write_tokens == 0
            && reasoning_tokens == 0
        {
            continue;
        }

        let kv = collect_kv_fields(chat_model);
        let origin_cascade = kv.get("cascade_id").map(String::as_str);
        let inherited_orphan = match origin_cascade {
            Some(origin) if origin != session_id => {
                if live_cascade_ids.contains(origin) {
                    continue;
                }
                true
            }
            _ => false,
        };

        let response_id = non_empty_string_field(usage, 11).map(str::to_string);
        let request_id = kv.get("request_id").filter(|id| !id.trim().is_empty()).cloned();
        let event_key = if let Some(id) = response_id.as_deref() {
            format!("response:{id}")
        } else if let Some(id) = request_id.as_deref() {
            format!("request:{id}")
        } else {
            format!("idx:{}", idx.unwrap_or(pos as i64))
        };

        if (response_id.is_some() || request_id.is_some()) && !seen_keys.insert(event_key.clone()) {
            continue;
        }

        let raw_response_model = non_empty_string_field(chat_model, 19);
        let display_label = non_empty_string_field(chat_model, 21).map(str::to_string);
        let model_raw =
            resolve_model_id(raw_response_model, display_label.as_deref(), &label_models);
        let used_claude = kv.get("used_claude").map(String::as_str) == Some("true");
        let provider = infer_provider(provider_enum, used_claude, &model_raw).to_string();
        let inline_ts = message_field(chat_model, 9)
            .and_then(|gen_time| message_field(gen_time, 4))
            .and_then(proto_timestamp_ms)
            .filter(|&ms| ms > 0);
        let last_step_index =
            kv.get("last_step_index").and_then(|value| value.trim().parse::<i64>().ok());

        let raw_usage_json = serde_json::json!({
            "idx": idx,
            "response_id": response_id,
            "request_id": request_id,
            "cascade_id": origin_cascade,
            "model_enum": varint_field(usage, 1),
            "provider_enum": provider_enum,
            "response_model": raw_response_model,
            "display_label": display_label,
            "input_tokens": input_tokens,
            "total_output_tokens": total_output,
            "response_output_tokens": response_output_tokens,
            "cache_write_tokens": cache_write_tokens,
            "cache_read_tokens": cache_read_tokens,
            "thinking_output_tokens": reasoning_tokens,
        })
        .to_string();

        candidates.push(CandidateRow {
            idx: *idx,
            event_key,
            response_id,
            last_step_index,
            inline_ts,
            inherited_orphan,
            model_raw,
            display_label,
            provider,
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_write_tokens,
            reasoning_tokens,
            raw_usage_json,
        });
    }

    if candidates.is_empty() {
        return Ok(ParsedAntigravityDb { usage_events: Vec::new(), created_at_ms, workspace_dir });
    }

    let step_timestamps =
        if candidates.iter().any(|row| row.inline_ts.is_none()) && table_exists(&tx, "steps")? {
            read_step_timestamps(&tx)?
        } else {
            StepTimestamps { by_response_id: HashMap::new(), by_step_idx: HashMap::new() }
        };

    let source_path = db_path.to_str().map(str::to_string);
    let mut usage_events = Vec::with_capacity(candidates.len());

    for (seq, candidate) in candidates.into_iter().enumerate() {
        let timestamp = candidate
            .inline_ts
            .or_else(|| {
                candidate
                    .response_id
                    .as_deref()
                    .and_then(|id| step_timestamps.by_response_id.get(id).copied())
            })
            .or_else(|| {
                candidate
                    .last_step_index
                    .and_then(|k| k.checked_add(1))
                    .and_then(|step_idx| step_timestamps.by_step_idx.get(&step_idx).copied())
            })
            .or(created_at_ms)
            .or(fallback_timestamp_ms)
            .unwrap_or(0);

        let base_seq =
            candidate.idx.and_then(|idx| u32::try_from(idx.max(0)).ok()).unwrap_or(seq as u32);
        let event_seq = if candidate.inherited_orphan {
            INHERITED_ORPHAN_SEQ_OFFSET.saturating_add(base_seq)
        } else {
            base_seq
        };

        let mut event = RawUsageEvent::observed(
            candidate.event_key,
            event_seq,
            timestamp,
            USAGE_PARSER_VERSION,
        );
        event.model = candidate.model_raw;
        event.provider = candidate.provider;
        event.input_tokens = candidate.input_tokens;
        event.output_tokens = candidate.output_tokens;
        event.cache_read_tokens = candidate.cache_read_tokens;
        event.cache_write_tokens = candidate.cache_write_tokens;
        event.reasoning_tokens = candidate.reasoning_tokens;
        event.source_path = source_path.clone();
        event.raw_usage_json = Some(candidate.raw_usage_json);
        let _ = candidate.display_label;
        usage_events.push(event);
    }

    Ok(ParsedAntigravityDb { usage_events, created_at_ms, workspace_dir })
}

fn table_exists(conn: &Connection, table: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        [table],
        |row| row.get(0),
    )
}

fn read_trajectory_meta(conn: &Connection) -> rusqlite::Result<(Option<i64>, Option<String>)> {
    let mut stmt = conn.prepare("SELECT data FROM trajectory_metadata_blob LIMIT 1")?;
    let mut rows = stmt.query([])?;
    let Some(row) = rows.next()? else {
        return Ok((None, None));
    };
    let blob: Vec<u8> = row.get(0)?;
    let created_at_ms = message_field(&blob, 2).and_then(proto_timestamp_ms).filter(|&ms| ms > 0);
    let workspace_dir = extract_workspace_from_trajectory_blob(&blob);
    Ok((created_at_ms, workspace_dir))
}

fn extract_workspace_from_trajectory_blob(blob: &[u8]) -> Option<String> {
    if let Some(folder) = message_field(blob, 1) {
        for field in [1, 2] {
            if let Some(uri) = non_empty_string_field(folder, field)
                && let Some(path) = file_uri_to_path(uri)
                && !path.is_empty()
            {
                return Some(path);
            }
        }
    }
    let mut reader = ProtoReader::new(blob);
    while let Some((field, wire)) = reader.next_field() {
        if field == 7
            && let Wire::Len(bytes) = wire
            && let Ok(uri) = std::str::from_utf8(bytes)
            && let Some(path) = file_uri_to_path(uri.trim())
            && !path.is_empty()
        {
            return Some(path);
        }
    }
    None
}

fn read_step_timestamps(conn: &Connection) -> rusqlite::Result<StepTimestamps> {
    let mut timestamps =
        StepTimestamps { by_response_id: HashMap::new(), by_step_idx: HashMap::new() };
    let mut stmt = conn
        .prepare("SELECT idx, metadata FROM steps WHERE step_type = 15 AND metadata IS NOT NULL")?;
    let rows =
        stmt.query_map([], |row| Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, Vec<u8>>(1)?)))?;
    for row in rows {
        let (idx, blob) = row?;
        let Some(ts_ms) = message_field(&blob, 1).and_then(proto_timestamp_ms).filter(|&ms| ms > 0)
        else {
            continue;
        };
        if let Some(step_idx) = idx {
            timestamps.by_step_idx.insert(step_idx, ts_ms);
        }
        if let Some(resp_id) = message_field(&blob, 9)
            .and_then(|chat_model| non_empty_string_field(chat_model, 11))
            .or_else(|| non_empty_string_field(&blob, 35))
        {
            timestamps.by_response_id.insert(resp_id.trim().to_string(), ts_ms);
        }
    }
    Ok(timestamps)
}

fn is_concrete_response_model(model: &str) -> bool {
    let trimmed = model.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("gemini-default") {
        return false;
    }
    !trimmed.to_ascii_lowercase().starts_with("model_placeholder_")
}

fn collect_label_model_map(rows: &[(Option<i64>, Vec<u8>)]) -> HashMap<String, String> {
    let mut map: HashMap<String, Option<String>> = HashMap::new();
    for (_, blob) in rows {
        let Some(chat_model) = message_field(blob, 1) else {
            continue;
        };
        let Some(label) = non_empty_string_field(chat_model, 21) else {
            continue;
        };
        let Some(model) =
            non_empty_string_field(chat_model, 19).filter(|m| is_concrete_response_model(m))
        else {
            continue;
        };
        map.entry(label.to_string())
            .and_modify(|existing| {
                if let Some(prev) = existing
                    && prev != model
                {
                    *existing = None;
                }
            })
            .or_insert_with(|| Some(model.to_string()));
    }
    map.into_iter().filter_map(|(k, v)| Some((k, v?))).collect()
}

fn resolve_model_id(
    raw_response_model: Option<&str>,
    display_label: Option<&str>,
    label_models: &HashMap<String, String>,
) -> String {
    if let Some(model) = raw_response_model.filter(|m| is_concrete_response_model(m)) {
        return model.trim().to_string();
    }
    if let Some(label) = display_label
        && let Some(recovered) = label_models.get(label)
    {
        return recovered.clone();
    }
    let mut concrete_models = label_models.values();
    if let Some(model) = concrete_models.next()
        && concrete_models.all(|other| other == model)
    {
        return model.clone();
    }
    "unknown".to_string()
}

fn infer_provider(provider_enum: Option<u64>, used_claude: bool, model: &str) -> &'static str {
    let lower = model.trim().to_ascii_lowercase();
    if provider_enum == Some(26) || used_claude || lower.starts_with("claude") {
        return "anthropic";
    }
    if lower.starts_with("gpt")
        || lower.starts_with("o1")
        || lower.starts_with("o3")
        || lower.starts_with("o4")
        || lower.starts_with("model_openai_")
    {
        return "openai";
    }
    if provider_enum == Some(24) || lower.starts_with("gemini") {
        return "google";
    }
    "antigravity"
}

fn collect_kv_fields(chat_model: &[u8]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut reader = ProtoReader::new(chat_model);
    while let Some((field, wire)) = reader.next_field() {
        if field == 20
            && let Wire::Len(entry) = wire
            && let Some(key) = non_empty_string_field(entry, 1)
            && let Some(val) = string_field(entry, 2)
        {
            map.insert(key.trim().to_string(), val.trim().to_string());
        }
    }
    map
}

fn clamp_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn proto_timestamp_ms(ts: &[u8]) -> Option<i64> {
    let seconds = i64::try_from(varint_field(ts, 1)?).ok()?;
    let nanos = i64::try_from(varint_field(ts, 2).unwrap_or(0)).ok()?;
    if !(0..=999_999_999).contains(&nanos) {
        return None;
    }
    seconds.checked_mul(1000)?.checked_add(nanos / 1_000_000)
}

enum Wire<'a> {
    Varint(u64),
    Len(&'a [u8]),
    Fixed64,
    Fixed32,
}

struct ProtoReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> ProtoReader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn read_varint(&mut self) -> Option<u64> {
        let mut result: u64 = 0;
        let mut shift = 0u32;
        loop {
            let byte = *self.buf.get(self.pos)?;
            self.pos += 1;
            result |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(result);
            }
            shift += 7;
            if shift >= 64 {
                return None;
            }
        }
    }

    fn next_field(&mut self) -> Option<(u64, Wire<'a>)> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let tag = self.read_varint()?;
        let field = tag >> 3;
        let wire = match tag & 0x7 {
            0 => Wire::Varint(self.read_varint()?),
            1 => {
                self.pos = self.pos.checked_add(8).filter(|&p| p <= self.buf.len())?;
                Wire::Fixed64
            }
            2 => {
                let len = usize::try_from(self.read_varint()?).ok()?;
                let end = self.pos.checked_add(len).filter(|&p| p <= self.buf.len())?;
                let bytes = &self.buf[self.pos..end];
                self.pos = end;
                Wire::Len(bytes)
            }
            5 => {
                self.pos = self.pos.checked_add(4).filter(|&p| p <= self.buf.len())?;
                Wire::Fixed32
            }
            _ => return None,
        };
        Some((field, wire))
    }
}

fn message_field(buf: &[u8], field: u64) -> Option<&[u8]> {
    let mut reader = ProtoReader::new(buf);
    while let Some((found, wire)) = reader.next_field() {
        if found == field
            && let Wire::Len(bytes) = wire
        {
            return Some(bytes);
        }
    }
    None
}

fn varint_field(buf: &[u8], field: u64) -> Option<u64> {
    let mut reader = ProtoReader::new(buf);
    while let Some((found, wire)) = reader.next_field() {
        if found == field
            && let Wire::Varint(value) = wire
        {
            return Some(value);
        }
    }
    None
}

fn string_field(buf: &[u8], field: u64) -> Option<&str> {
    message_field(buf, field).and_then(|bytes| std::str::from_utf8(bytes).ok())
}

fn non_empty_string_field(buf: &[u8], field: u64) -> Option<&str> {
    string_field(buf, field).map(str::trim).filter(|text| !text.is_empty())
}
