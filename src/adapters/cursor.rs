use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use tracing::debug;
use walkdir::WalkDir;

use crate::adapters::events;
use crate::adapters::{
    RawMessage, RawSession, ResumeCommand, SourceAdapter, SyncScanResult, SyncScanStats,
};
use crate::db::store::{EventSessionStateMeta, Store, UsageSessionStateMeta};
use crate::types::{RawSessionEvent, RawUsageEvent, Role, TokenSource};

pub struct CursorAdapter;

const USAGE_PARSER_VERSION: u32 = 2;
const EVENT_PARSER_VERSION: u32 = 1;

#[derive(Debug, Clone)]
struct ComposerMeta {
    name: Option<String>,
    unified_mode: Option<String>,
    directory: Option<String>,
    created_at: Option<i64>,
    last_updated_at: Option<i64>,
}

struct ParsedComposerSession {
    messages: Vec<RawMessage>,
    usage_events: Vec<RawUsageEvent>,
    events: Vec<RawSessionEvent>,
    started_at: i64,
    updated_at: Option<i64>,
    entrypoint: Option<String>,
    directory: Option<String>,
}

impl SourceAdapter for CursorAdapter {
    fn id(&self) -> &str {
        "cursor"
    }

    fn label(&self) -> &str {
        "CUR"
    }

    fn resume_command(&self, _source_id: &str) -> Option<ResumeCommand> {
        None
    }

    fn usage_parser_version(&self) -> Option<u32> {
        Some(USAGE_PARSER_VERSION)
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        scan_cursor_sessions(None, true)
    }

    fn scan_for_sync(
        &self,
        store: &Store,
        since_ts: Option<i64>,
        include_events: bool,
    ) -> anyhow::Result<Option<SyncScanResult>> {
        let Some(conn) = open_global_db()? else {
            return Ok(Some(SyncScanResult { sessions: vec![], stats: SyncScanStats::default() }));
        };

        let existing = store.session_meta_map(self.id())?;
        let usage_state = store.usage_state_meta_map(self.id())?;
        let event_state =
            if include_events { store.event_state_meta_map(self.id())? } else { HashMap::new() };
        let global_mtime = global_db_mtime();
        let composer_ids = discover_composer_ids(&conn)?;
        let transcript_paths = collect_agent_transcript_paths();
        let mut sessions = Vec::new();
        let mut stats = SyncScanStats::default();

        for composer_id in composer_ids {
            let meta = load_composer_meta(&conn, &composer_id);
            let updated_at = meta.last_updated_at.or(meta.created_at);
            if let Some(cutoff) = since_ts
                && updated_at.is_some_and(|ts| ts < cutoff)
            {
                stats.filtered_sessions += 1;
                continue;
            }

            let source_updated_at = updated_at.or(global_mtime);
            if let Some((old_updated_at, _)) = existing.get(&composer_id)
                && *old_updated_at == source_updated_at
                && session_state_is_current(
                    usage_state.get(&composer_id).copied(),
                    event_state.get(&composer_id).copied(),
                    source_updated_at,
                    include_events,
                )
            {
                stats.skipped_sessions += 1;
                continue;
            }

            if let Some(raw) =
                build_raw_session(&conn, &composer_id, &meta, &transcript_paths, include_events)?
            {
                sessions.push(raw);
            }
        }

        Ok(Some(SyncScanResult { sessions, stats }))
    }
}

fn scan_cursor_sessions(
    since_ts: Option<i64>,
    include_events: bool,
) -> anyhow::Result<Vec<RawSession>> {
    let Some(conn) = open_global_db()? else {
        return scan_transcript_only_sessions(since_ts, include_events);
    };

    let transcript_paths = collect_agent_transcript_paths();
    let mut sessions = Vec::new();
    for composer_id in discover_composer_ids(&conn)? {
        let meta = load_composer_meta(&conn, &composer_id);
        if let Some(cutoff) = since_ts {
            let updated_at = meta.last_updated_at.or(meta.created_at).unwrap_or(0);
            if updated_at < cutoff {
                continue;
            }
        }
        if let Some(raw) =
            build_raw_session(&conn, &composer_id, &meta, &transcript_paths, include_events)?
        {
            sessions.push(raw);
        }
    }
    Ok(sessions)
}

fn scan_transcript_only_sessions(
    since_ts: Option<i64>,
    include_events: bool,
) -> anyhow::Result<Vec<RawSession>> {
    let Some(projects_dir) = resolve_projects_dir()? else {
        return Ok(vec![]);
    };
    let cwd_map = build_agent_cwd_map(resolve_global_state_db_path().as_deref());
    let mut sessions = Vec::new();
    for (session_id, path) in collect_agent_transcript_paths_from_dir(&projects_dir) {
        let Some(mtime_ms) = stat_mtime_ms(&path) else {
            continue;
        };
        if let Some(cutoff) = since_ts
            && mtime_ms < cutoff
        {
            continue;
        }
        let mut raw = match parse_agent_transcript(&path, include_events)? {
            Some(raw) => raw,
            None => continue,
        };
        raw.source_id = session_id;
        raw.directory = cwd_map.get(&raw.source_id).cloned();
        raw.started_at = stat_birth_ms(&path).unwrap_or(mtime_ms);
        raw.updated_at = Some(mtime_ms);
        sessions.push(raw);
    }
    Ok(sessions)
}

fn build_raw_session(
    conn: &Connection,
    composer_id: &str,
    meta: &ComposerMeta,
    transcript_paths: &HashMap<String, PathBuf>,
    include_events: bool,
) -> anyhow::Result<Option<RawSession>> {
    let parsed = match parse_composer_session(conn, composer_id, meta, include_events)? {
        Some(parsed)
            if !parsed.messages.is_empty()
                || !parsed.usage_events.is_empty()
                || !parsed.events.is_empty() =>
        {
            parsed
        }
        _ => {
            let Some(path) = transcript_paths.get(composer_id) else {
                return Ok(None);
            };
            let mtime_ms = stat_mtime_ms(path).unwrap_or(0);
            let raw = parse_agent_transcript(path, include_events)?
                .filter(|raw| !raw.messages.is_empty() || !raw.events.is_empty());
            let Some(mut raw) = raw else {
                return Ok(None);
            };
            raw.source_id = composer_id.to_string();
            raw.directory = meta.directory.clone().or(raw.directory);
            raw.started_at = stat_birth_ms(path).unwrap_or(mtime_ms);
            raw.updated_at = Some(mtime_ms);
            raw.entrypoint = meta.unified_mode.clone().or(raw.entrypoint);
            return Ok(Some(raw));
        }
    };

    let mut session = RawSession::search_only(
        composer_id.to_string(),
        parsed.directory.or(meta.directory.clone()),
        parsed.started_at,
        parsed.updated_at,
        parsed.entrypoint.or(meta.unified_mode.clone()),
        parsed.messages,
    )
    .with_usage(parsed.usage_events, USAGE_PARSER_VERSION);
    if include_events {
        session = session.with_events(parsed.events, EVENT_PARSER_VERSION);
    }
    Ok(Some(session))
}

fn parse_composer_session(
    conn: &Connection,
    composer_id: &str,
    meta: &ComposerMeta,
    include_events: bool,
) -> anyhow::Result<Option<ParsedComposerSession>> {
    let Some(raw_json) = read_disk_kv(conn, &format!("composerData:{composer_id}")) else {
        return Ok(None);
    };
    let data: Value = match serde_json::from_str(&raw_json) {
        Ok(value) => value,
        Err(err) => {
            debug!("failed to parse composerData for {composer_id}: {err}");
            return Ok(None);
        }
    };

    let headers = data
        .get("fullConversationHeadersOnly")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    let conversation_map = data.get("conversationMap").and_then(|value| value.as_object());

    let mut messages = Vec::new();
    let mut usage_events = Vec::new();
    let mut bubble_usage_events = Vec::new();
    let mut session_events = Vec::new();
    let source_path = format!("composer:{composer_id}");

    for (index, header) in headers.iter().enumerate() {
        let bubble_id = header.get("bubbleId").and_then(|value| value.as_str());
        let header_type = header.get("type").and_then(|value| value.as_i64());
        let role = bubble_role(header_type);
        let Some(role) = role else {
            continue;
        };

        let bubble = bubble_id.and_then(|bubble_id| load_bubble(conn, composer_id, bubble_id));
        let content = if let Some(bubble) = bubble.as_ref() {
            render_bubble_content(bubble, &role)
        } else if let (Some(bubble_id), Some(map)) = (bubble_id, conversation_map) {
            map.get(bubble_id).map(|value| render_legacy_bubble(value, &role)).unwrap_or_default()
        } else {
            String::new()
        };

        if content.is_empty() {
            continue;
        }

        let timestamp =
            bubble.as_ref().and_then(|value| json_i64(value.get("createdAt"))).or_else(|| {
                conversation_map
                    .and_then(|map| bubble_id.and_then(|id| map.get(id)))
                    .and_then(|value| json_i64(value.get("createdAt")))
            });

        messages.push(RawMessage { role: role.clone(), content, timestamp });

        if include_events
            && matches!(role, Role::Assistant)
            && let Some(bubble) = bubble.as_ref()
        {
            let message_seq = (messages.len() - 1) as u32;
            collect_bubble_tool_events(
                bubble,
                bubble_id.unwrap_or("unknown"),
                &source_path,
                timestamp,
                message_seq,
                &mut session_events,
            );
        }

        if let Some(bubble) = bubble.as_ref()
            && let Some(event) = extract_bubble_usage_event(
                composer_id,
                bubble_id.unwrap_or("unknown"),
                index as u32,
                timestamp.unwrap_or(0),
                bubble,
                &data,
            )
        {
            bubble_usage_events.push(event);
        }
    }

    if !bubble_usage_events.is_empty() {
        usage_events.extend(bubble_usage_events);
    } else if let Some(event) = extract_session_usage_event(composer_id, &data, meta) {
        usage_events.push(event);
    }

    if messages.is_empty() && usage_events.is_empty() && session_events.is_empty() {
        return Ok(None);
    }

    let started_at = json_i64(data.get("createdAt"))
        .or(meta.created_at)
        .or_else(|| messages.first().and_then(|msg| msg.timestamp))
        .unwrap_or(0);
    let updated_at = json_i64(data.get("lastUpdatedAt"))
        .or(json_i64(data.get("conversationCheckpointLastUpdatedAt")))
        .or(meta.last_updated_at)
        .or_else(|| messages.last().and_then(|msg| msg.timestamp));

    Ok(Some(ParsedComposerSession {
        messages,
        usage_events,
        events: session_events,
        started_at,
        updated_at,
        entrypoint: data
            .get("unifiedMode")
            .and_then(|value| value.as_str())
            .map(str::to_string)
            .or_else(|| meta.unified_mode.clone()),
        directory: meta.directory.clone(),
    }))
}

fn discover_composer_ids(conn: &Connection) -> anyhow::Result<BTreeSet<String>> {
    let mut ids = BTreeSet::new();

    if let Some(raw) = read_item_value(conn, "composer.composerHeaders")
        && let Ok(value) = serde_json::from_str::<Value>(&raw)
        && let Some(items) = value.get("allComposers").and_then(|value| value.as_array())
    {
        for item in items {
            if let Some(id) = item.get("composerId").and_then(|value| value.as_str()) {
                ids.insert(id.to_string());
            }
        }
    }

    let mut stmt = conn.prepare("SELECT key FROM cursorDiskKV WHERE key LIKE 'composerData:%'")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    for row in rows {
        let key = row?;
        if let Some(id) = key.strip_prefix("composerData:") {
            ids.insert(id.to_string());
        }
    }

    if let Some(workspace_dir) = resolve_workspace_storage_dir() {
        for entry in fs::read_dir(workspace_dir)? {
            let entry = entry?;
            let db_path = entry.path().join("state.vscdb");
            if !db_path.exists() {
                continue;
            }
            if let Ok(workspace_conn) = Connection::open_with_flags(
                &db_path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            ) && let Some(raw) = read_item_value(&workspace_conn, "composer.composerData")
                && let Ok(value) = serde_json::from_str::<Value>(&raw)
            {
                collect_composer_ids_from_workspace_data(&value, &mut ids);
            }
        }
    }

    for (session_id, _) in collect_agent_transcript_paths() {
        ids.insert(session_id);
    }

    Ok(ids)
}

fn collect_composer_ids_from_workspace_data(value: &Value, ids: &mut BTreeSet<String>) {
    if let Some(items) = value.get("allComposers").and_then(|value| value.as_array()) {
        for item in items {
            if let Some(id) = item.get("composerId").and_then(|value| value.as_str()) {
                ids.insert(id.to_string());
            }
        }
    }
    for key in ["selectedComposerIds", "lastFocusedComposerIds"] {
        if let Some(items) = value.get(key).and_then(|value| value.as_array()) {
            for item in items {
                if let Some(id) = item.as_str() {
                    ids.insert(id.to_string());
                }
            }
        }
    }
}

fn load_composer_meta(conn: &Connection, composer_id: &str) -> ComposerMeta {
    let mut meta = ComposerMeta {
        name: None,
        unified_mode: None,
        directory: None,
        created_at: None,
        last_updated_at: None,
    };

    if let Some(raw) = read_item_value(conn, "composer.composerHeaders")
        && let Ok(value) = serde_json::from_str::<Value>(&raw)
        && let Some(items) = value.get("allComposers").and_then(|value| value.as_array())
    {
        for item in items {
            if item.get("composerId").and_then(|value| value.as_str()) == Some(composer_id) {
                meta.name = item.get("name").and_then(|value| value.as_str()).map(str::to_string);
                meta.unified_mode =
                    item.get("unifiedMode").and_then(|value| value.as_str()).map(str::to_string);
                meta.created_at = json_i64(item.get("createdAt"));
                meta.last_updated_at = json_i64(item.get("lastUpdatedAt"));
                meta.directory = workspace_path_from_identifier(item.get("workspaceIdentifier"));
                break;
            }
        }
    }

    if let Some(raw) = read_disk_kv(conn, &format!("composerData:{composer_id}"))
        && let Ok(data) = serde_json::from_str::<Value>(&raw)
    {
        if meta.name.is_none() {
            meta.name = data.get("name").and_then(|value| value.as_str()).map(str::to_string);
        }
        if meta.unified_mode.is_none() {
            meta.unified_mode =
                data.get("unifiedMode").and_then(|value| value.as_str()).map(str::to_string);
        }
        if meta.created_at.is_none() {
            meta.created_at = json_i64(data.get("createdAt"));
        }
        if meta.last_updated_at.is_none() {
            meta.last_updated_at = json_i64(data.get("lastUpdatedAt"))
                .or(json_i64(data.get("conversationCheckpointLastUpdatedAt")));
        }
    }

    if meta.directory.is_none()
        && let Some(path) =
            build_agent_cwd_map(resolve_global_state_db_path().as_deref()).get(composer_id)
    {
        meta.directory = Some(path.clone());
    }

    meta
}

fn workspace_path_from_identifier(value: Option<&Value>) -> Option<String> {
    let uri = value?.get("uri")?;
    uri.get("fsPath")
        .and_then(|value| value.as_str())
        .filter(|path| !path.trim().is_empty())
        .map(str::to_string)
}

fn load_bubble(conn: &Connection, composer_id: &str, bubble_id: &str) -> Option<Value> {
    let raw = read_disk_kv(conn, &format!("bubbleId:{composer_id}:{bubble_id}"))?;
    serde_json::from_str(&raw).ok()
}

fn render_bubble_content(bubble: &Value, role: &Role) -> String {
    let mut parts = Vec::new();
    if let Some(text) = non_empty_str(bubble.get("text").or_else(|| bubble.get("rawText"))) {
        let normalized = if matches!(role, Role::User) {
            strip_user_query_envelope(text).trim().to_string()
        } else {
            text.trim().to_string()
        };
        if !normalized.is_empty() {
            parts.push(normalized);
        }
    }

    if let Some(tool_data) = bubble.get("toolFormerData") {
        let name = tool_data.get("name").and_then(|value| value.as_str()).unwrap_or("tool");
        let args = tool_data
            .get("rawArgs")
            .or_else(|| tool_data.get("params"))
            .and_then(render_json_fragment)
            .unwrap_or_default();
        if !args.is_empty() {
            parts.push(format!("[tool:{name}] {args}"));
        } else {
            parts.push(format!("[tool:{name}]"));
        }
        if let Some(result) =
            tool_data.get("result").and_then(render_json_fragment).filter(|s| !s.is_empty())
        {
            parts.push(format!("[tool_result:{name}] {result}"));
        }
    }

    if let Some(blocks) = bubble.get("codeBlocks").and_then(|value| value.as_array()) {
        for block in blocks {
            if let Some(content) = block.get("content").and_then(|value| value.as_str()) {
                parts.push(format!("[code_block] {content}"));
            }
        }
    }

    parts.join("\n")
}

fn collect_bubble_tool_events(
    bubble: &Value,
    bubble_id: &str,
    source_path: &str,
    timestamp: Option<i64>,
    message_seq: u32,
    events_out: &mut Vec<RawSessionEvent>,
) {
    let Some(tool_data) = bubble.get("toolFormerData") else {
        return;
    };
    let name = tool_data.get("name").and_then(|value| value.as_str()).unwrap_or("tool").to_string();
    let source_event_id = Some(bubble_id.to_string());

    if let Some(params) = tool_data.get("params") {
        events_out.push(events::tool_call_event(
            events::EventContext {
                event_seq: events_out.len() as u32,
                timestamp,
                source_path: Some(source_path.to_string()),
                source_event_id: source_event_id.clone(),
                message_seq: Some(message_seq),
                parser_version: EVENT_PARSER_VERSION,
            },
            name.clone(),
            Some(params),
        ));
    } else if let Some(raw_args) = tool_data.get("rawArgs") {
        match raw_args {
            Value::String(text) => {
                events_out.push(events::tool_call_event_from_text(
                    events::EventContext {
                        event_seq: events_out.len() as u32,
                        timestamp,
                        source_path: Some(source_path.to_string()),
                        source_event_id: source_event_id.clone(),
                        message_seq: Some(message_seq),
                        parser_version: EVENT_PARSER_VERSION,
                    },
                    name.clone(),
                    Some(text.as_str()),
                ));
            }
            other => {
                events_out.push(events::tool_call_event(
                    events::EventContext {
                        event_seq: events_out.len() as u32,
                        timestamp,
                        source_path: Some(source_path.to_string()),
                        source_event_id: source_event_id.clone(),
                        message_seq: Some(message_seq),
                        parser_version: EVENT_PARSER_VERSION,
                    },
                    name.clone(),
                    Some(other),
                ));
            }
        }
    } else {
        events_out.push(events::tool_call_event(
            events::EventContext {
                event_seq: events_out.len() as u32,
                timestamp,
                source_path: Some(source_path.to_string()),
                source_event_id: source_event_id.clone(),
                message_seq: Some(message_seq),
                parser_version: EVENT_PARSER_VERSION,
            },
            name.clone(),
            None,
        ));
    }

    if let Some(result) = tool_data.get("result") {
        let summary = render_json_fragment(result).filter(|text| !text.is_empty());
        events_out.push(events::tool_result_event(
            events::EventContext {
                event_seq: events_out.len() as u32,
                timestamp,
                source_path: Some(source_path.to_string()),
                source_event_id,
                message_seq: Some(message_seq),
                parser_version: EVENT_PARSER_VERSION,
            },
            Some(name),
            summary,
        ));
    }
}

fn render_legacy_bubble(value: &Value, role: &Role) -> String {
    if let Some(text) = non_empty_str(value.get("text")) {
        let normalized = if matches!(role, Role::User) {
            strip_user_query_envelope(text).trim().to_string()
        } else {
            text.trim().to_string()
        };
        if !normalized.is_empty() {
            return normalized;
        }
    }
    render_bubble_content(value, role)
}

fn extract_bubble_usage_event(
    composer_id: &str,
    bubble_id: &str,
    event_seq: u32,
    timestamp: i64,
    bubble: &Value,
    composer_data: &Value,
) -> Option<RawUsageEvent> {
    let token_count = bubble.get("tokenCount")?;
    let input_tokens =
        token_count.get("inputTokens").and_then(|value| json_i64(Some(value))).unwrap_or(0).max(0);
    let output_tokens =
        token_count.get("outputTokens").and_then(|value| json_i64(Some(value))).unwrap_or(0).max(0);
    if input_tokens == 0 && output_tokens == 0 {
        return None;
    }

    let model = model_from_composer(composer_data);
    Some(RawUsageEvent {
        event_key: format!("bubble:{bubble_id}"),
        event_seq,
        message_seq: Some(event_seq),
        timestamp,
        model: model.clone(),
        provider: infer_cursor_provider(&model),
        input_tokens,
        output_tokens,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: 0,
        token_source: TokenSource::Observed,
        parser_version: USAGE_PARSER_VERSION,
        source_path: Some(format!("composer:{composer_id}")),
        raw_usage_json: Some(token_count.to_string()),
    })
}

fn extract_session_usage_event(
    composer_id: &str,
    composer_data: &Value,
    meta: &ComposerMeta,
) -> Option<RawUsageEvent> {
    if let Some(breakdown) = composer_data.get("promptTokenBreakdown") {
        let total_used = json_i64(breakdown.get("totalUsedTokens")).unwrap_or(0).max(0);
        if total_used == 0 {
            return None;
        }
        let (input_tokens, cache_read_tokens) = map_context_breakdown(breakdown, total_used);
        return Some(build_session_usage_event(
            composer_id,
            composer_data,
            meta,
            input_tokens,
            cache_read_tokens,
            breakdown,
        ));
    }

    let total_used = json_i64(composer_data.get("contextTokensUsed")).unwrap_or(0).max(0);
    if total_used == 0 {
        return None;
    }
    Some(build_session_usage_event(composer_id, composer_data, meta, total_used, 0, &Value::Null))
}

fn map_context_breakdown(breakdown: &Value, total_used: i64) -> (i64, i64) {
    let mut conversation_tokens = 0;
    let mut prompt_tokens = 0;
    if let Some(categories) = breakdown.get("categories").and_then(|value| value.as_array()) {
        for category in categories {
            let id = category.get("id").and_then(|value| value.as_str()).unwrap_or("");
            let estimated = json_i64(category.get("estimatedTokens")).unwrap_or(0).max(0);
            match id {
                "conversation" | "summarized_conversation" => conversation_tokens += estimated,
                _ => prompt_tokens += estimated,
            }
        }
    }
    let categorized = conversation_tokens + prompt_tokens;
    if categorized < total_used {
        prompt_tokens += total_used - categorized;
    }
    (prompt_tokens, conversation_tokens)
}

fn build_session_usage_event(
    composer_id: &str,
    composer_data: &Value,
    meta: &ComposerMeta,
    input_tokens: i64,
    cache_read_tokens: i64,
    breakdown: &Value,
) -> RawUsageEvent {
    let model = model_from_composer(composer_data);
    let timestamp = meta
        .last_updated_at
        .or(meta.created_at)
        .or_else(|| json_i64(composer_data.get("lastUpdatedAt")))
        .unwrap_or(0);
    RawUsageEvent {
        event_key: "session:prompt-token-breakdown".to_string(),
        event_seq: 0,
        message_seq: None,
        timestamp,
        model: model.clone(),
        provider: infer_cursor_provider(&model),
        input_tokens,
        output_tokens: 0,
        cache_read_tokens,
        cache_write_tokens: 0,
        reasoning_tokens: 0,
        token_source: TokenSource::Derived,
        parser_version: USAGE_PARSER_VERSION,
        source_path: Some(format!("composer:{composer_id}")),
        raw_usage_json: if breakdown.is_null() { None } else { Some(breakdown.to_string()) },
    }
}

fn model_from_composer(composer_data: &Value) -> String {
    composer_data
        .get("modelConfig")
        .and_then(|value| value.get("modelName"))
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "unknown".to_string())
}

fn infer_cursor_provider(model: &str) -> String {
    let lower = model.to_lowercase();
    if lower.starts_with("claude") {
        "anthropic".to_string()
    } else if lower.starts_with("gpt") || lower.starts_with("o1") || lower.starts_with("o3") {
        "openai".to_string()
    } else if lower.starts_with("gemini") {
        "google".to_string()
    } else {
        "cursor".to_string()
    }
}

fn bubble_role(header_type: Option<i64>) -> Option<Role> {
    match header_type {
        Some(1) => Some(Role::User),
        Some(2) => Some(Role::Assistant),
        _ => None,
    }
}

fn parse_agent_transcript(path: &Path, include_events: bool) -> anyhow::Result<Option<RawSession>> {
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut messages = Vec::new();
    let mut session_events = Vec::new();
    let source_path = path.display().to_string();

    for (line_index, line) in reader.lines().enumerate() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let role = match v.get("role").and_then(|r| r.as_str()) {
            Some("user") => Role::User,
            Some("assistant") => Role::Assistant,
            _ => continue,
        };
        let content_array =
            v.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_array());
        let Some(items) = content_array else {
            continue;
        };
        let is_user = matches!(role, Role::User);
        if include_events && matches!(role, Role::Assistant) {
            let message_seq = if messages.is_empty() { None } else { Some(messages.len() as u32) };
            collect_transcript_content_events(
                items,
                &source_path,
                line_index,
                message_seq,
                &mut session_events,
            );
        }
        let content = render_transcript_content_items(items, is_user);
        if content.is_empty() {
            continue;
        }
        messages.push(RawMessage { role, content, timestamp: None });
    }

    if messages.is_empty() && session_events.is_empty() {
        return Ok(None);
    }

    let mut session =
        RawSession::search_only(String::new(), None, 0, None, Some("agent".to_string()), messages);
    if include_events {
        session = session.with_events(session_events, EVENT_PARSER_VERSION);
    }
    Ok(Some(session))
}

fn collect_transcript_content_events(
    items: &[Value],
    source_path: &str,
    line_index: usize,
    message_seq: Option<u32>,
    events_out: &mut Vec<RawSessionEvent>,
) {
    for (item_index, item) in items.iter().enumerate() {
        if item.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
            continue;
        }
        let name = item.get("name").and_then(|n| n.as_str()).unwrap_or("tool").to_string();
        events_out.push(events::tool_call_event(
            events::EventContext {
                event_seq: events_out.len() as u32,
                timestamp: None,
                source_path: Some(source_path.to_string()),
                source_event_id: Some(format!("{line_index}:{item_index}")),
                message_seq,
                parser_version: EVENT_PARSER_VERSION,
            },
            name,
            item.get("input"),
        ));
    }
}

fn render_transcript_content_items(items: &[Value], is_user: bool) -> String {
    let mut parts = Vec::new();
    for item in items {
        match item.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                let Some(text) = item.get("text").and_then(|t| t.as_str()) else {
                    continue;
                };
                let normalized = if is_user { strip_user_query_envelope(text) } else { text };
                let trimmed = normalized.trim();
                if !trimmed.is_empty() {
                    parts.push(trimmed.to_string());
                }
            }
            Some("tool_use") => {
                let name = item.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                let rendered_input = match item.get("input") {
                    Some(Value::String(s)) => s.clone(),
                    Some(v) => serde_json::to_string(v).unwrap_or_default(),
                    None => String::new(),
                };
                parts.push(format!("[tool_use:{name}] {rendered_input}"));
            }
            _ => {}
        }
    }
    parts.join("\n")
}

fn strip_user_query_envelope(text: &str) -> &str {
    const OPEN: &str = "<user_query>";
    const CLOSE: &str = "</user_query>";
    let trimmed = text.trim();
    if let Some(inner) = trimmed.strip_prefix(OPEN).and_then(|s| s.strip_suffix(CLOSE)) {
        inner
    } else {
        text
    }
}

fn collect_agent_transcript_paths() -> HashMap<String, PathBuf> {
    let Some(projects_dir) = resolve_projects_dir().ok().flatten() else {
        return HashMap::new();
    };
    collect_agent_transcript_paths_from_dir(&projects_dir).into_iter().collect()
}

fn collect_agent_transcript_paths_from_dir(projects_dir: &Path) -> Vec<(String, PathBuf)> {
    let mut entries = Vec::new();
    for walk_entry in WalkDir::new(projects_dir).into_iter().filter_map(|e| e.ok()) {
        let path = walk_entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if uuid::Uuid::try_parse(stem).is_err() {
            continue;
        }
        let parent_name = path.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str());
        if parent_name != Some(stem) {
            continue;
        }
        let grandparent_name = path
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str());
        if grandparent_name != Some("agent-transcripts") {
            continue;
        }
        entries.push((stem.to_string(), path.to_path_buf()));
    }
    entries
}

fn resolve_projects_dir() -> anyhow::Result<Option<PathBuf>> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home dir"))?;
    let dir = home.join(".cursor").join("projects");
    if !dir.exists() {
        debug!("~/.cursor/projects not found, skipping Cursor");
        return Ok(None);
    }
    Ok(Some(dir))
}

fn resolve_global_state_db_path() -> Option<PathBuf> {
    let db = dirs::config_dir()?.join("Cursor/User/globalStorage/state.vscdb");
    if db.exists() { Some(db) } else { None }
}

fn resolve_workspace_storage_dir() -> Option<PathBuf> {
    let dir = dirs::config_dir()?.join("Cursor/User/workspaceStorage");
    if dir.exists() { Some(dir) } else { None }
}

fn open_global_db() -> anyhow::Result<Option<Connection>> {
    let Some(path) = resolve_global_state_db_path() else {
        debug!("Cursor global state DB not found, skipping composer sessions");
        return Ok(None);
    };
    Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map(Some)
    .map_err(Into::into)
}

fn global_db_mtime() -> Option<i64> {
    resolve_global_state_db_path().and_then(|path| stat_mtime_ms(&path))
}

fn usage_state_is_current(
    state: Option<UsageSessionStateMeta>,
    source_updated_at: Option<i64>,
) -> bool {
    let Some(state) = state else {
        return false;
    };
    state.parser_version >= USAGE_PARSER_VERSION && state.source_updated_at == source_updated_at
}

fn event_state_is_current(
    state: Option<EventSessionStateMeta>,
    source_updated_at: Option<i64>,
) -> bool {
    let Some(state) = state else {
        return false;
    };
    state.parser_version >= EVENT_PARSER_VERSION && state.source_updated_at == source_updated_at
}

fn session_state_is_current(
    usage_state: Option<UsageSessionStateMeta>,
    event_state: Option<EventSessionStateMeta>,
    source_updated_at: Option<i64>,
    include_events: bool,
) -> bool {
    if !usage_state_is_current(usage_state, source_updated_at) {
        return false;
    }
    if include_events && !event_state_is_current(event_state, source_updated_at) {
        return false;
    }
    true
}

fn build_agent_cwd_map(db_path: Option<&Path>) -> HashMap<String, String> {
    let Some(db_path) = db_path else {
        return HashMap::new();
    };
    match read_agent_cwd_map(db_path) {
        Ok(map) => map,
        Err(err) => {
            debug!("cursor cwd map unavailable from {}: {err}", db_path.display());
            HashMap::new()
        }
    }
}

fn read_agent_cwd_map(db_path: &Path) -> anyhow::Result<HashMap<String, String>> {
    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let mut project_to_path: HashMap<String, String> = HashMap::new();
    if let Some(projects_json) = read_item_value(&conn, "glass.localAgentProjects.v1")
        && let Ok(projects) = serde_json::from_str::<Value>(&projects_json)
        && let Some(arr) = projects.as_array()
    {
        for item in arr {
            let project_id = match item.get("id").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let fs_path = item
                .get("workspace")
                .and_then(|w| w.get("uri"))
                .and_then(|u| u.get("fsPath"))
                .and_then(|p| p.as_str());
            if let Some(fs_path) = fs_path {
                project_to_path.insert(project_id, fs_path.to_string());
            }
        }
    }

    let mut session_to_path: HashMap<String, String> = HashMap::new();
    if let Some(membership_json) = read_item_value(&conn, "glass.localAgentProjectMembership.v1")
        && let Ok(membership) = serde_json::from_str::<Value>(&membership_json)
        && let Some(obj) = membership.as_object()
    {
        for (session_id, project_val) in obj {
            let Some(project_id) = project_val.as_str() else {
                continue;
            };
            if let Some(path) = project_to_path.get(project_id) {
                session_to_path.insert(session_id.clone(), path.clone());
            }
        }
    }
    Ok(session_to_path)
}

fn read_item_value(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM ItemTable WHERE key = ?1", [key], |row| {
        row.get::<_, String>(0)
    })
    .ok()
}

fn read_disk_kv(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM cursorDiskKV WHERE key = ?1", [key], |row| {
        row.get::<_, String>(0)
    })
    .ok()
}

fn stat_birth_ms(path: &Path) -> Option<i64> {
    let meta = fs::metadata(path).ok()?;
    let created = meta.created().ok()?;
    let duration = created.duration_since(UNIX_EPOCH).ok()?;
    Some(duration.as_millis() as i64)
}

fn stat_mtime_ms(path: &Path) -> Option<i64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    Some(modified.duration_since(UNIX_EPOCH).ok()?.as_millis() as i64)
}

fn non_empty_str(value: Option<&Value>) -> Option<&str> {
    value.and_then(|value| value.as_str()).filter(|value| !value.trim().is_empty())
}

fn render_json_fragment(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        _ => serde_json::to_string(value).ok(),
    }
}

fn json_i64(value: Option<&Value>) -> Option<i64> {
    value.and_then(|value| {
        value
            .as_i64()
            .or_else(|| value.as_u64().map(|value| value as i64))
            .or_else(|| value.as_f64().map(|value| value as i64))
    })
}
#[cfg(test)]
mod tests {
    use std::io::Write;

    use rusqlite::Connection;

    use super::*;
    use crate::types::TokenSource;

    fn temp_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "recall-cursor-test-{}-{}",
            label,
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_jsonl(path: &Path, lines: &[&str]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut f = fs::File::create(path).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
    }

    fn seed_global_db(root: &Path, composer_id: &str, bubble_id: &str) -> Connection {
        let db_path = root.join("state.vscdb");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )
        .unwrap();

        let headers = serde_json::json!({
            "allComposers": [{
                "composerId": composer_id,
                "name": "Usage review",
                "unifiedMode": "chat",
                "createdAt": 1_700_000_000_000_i64,
                "lastUpdatedAt": 1_700_000_100_000_i64,
                "workspaceIdentifier": {
                    "uri": { "fsPath": "/Users/x/project" }
                }
            }]
        });
        conn.execute(
            "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
            ["composer.composerHeaders", &headers.to_string()],
        )
        .unwrap();

        let composer_data = serde_json::json!({
            "composerId": composer_id,
            "createdAt": 1_700_000_000_000_i64,
            "lastUpdatedAt": 1_700_000_100_000_i64,
            "unifiedMode": "chat",
            "modelConfig": { "modelName": "claude-sonnet-4" },
            "promptTokenBreakdown": {
                "totalUsedTokens": 1200,
                "categories": [
                    { "id": "conversation", "estimatedTokens": 300 }
                ]
            },
            "fullConversationHeadersOnly": [
                { "bubbleId": bubble_id, "type": 1 },
            ]
        });
        conn.execute(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
            [format!("composerData:{composer_id}"), composer_data.to_string()],
        )
        .unwrap();

        let bubble = serde_json::json!({
            "type": 1,
            "text": "<user_query>\nhello cursor\n</user_query>",
            "createdAt": 1_700_000_000_000_i64,
            "tokenCount": { "inputTokens": 0, "outputTokens": 0 }
        });
        conn.execute(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
            [format!("bubbleId:{composer_id}:{bubble_id}"), bubble.to_string()],
        )
        .unwrap();

        conn
    }

    #[test]
    fn strip_user_query_envelope_strips_wrapper() {
        let text = "<user_query>\nhello world\n</user_query>";
        assert_eq!(strip_user_query_envelope(text).trim(), "hello world");
    }

    #[test]
    fn render_bubble_content_includes_tool_former_data() {
        let bubble = serde_json::json!({
            "text": "",
            "toolFormerData": {
                "name": "grep",
                "rawArgs": "{\"pattern\":\"usage\"}",
                "result": "{\"matches\":1}"
            }
        });
        let rendered = render_bubble_content(&bubble, &Role::Assistant);
        assert!(rendered.contains("[tool:grep]"));
        assert!(rendered.contains("usage"));
        assert!(rendered.contains("[tool_result:grep]"));
    }

    #[test]
    fn parse_composer_session_extracts_messages_and_usage() {
        let root = temp_root("composer");
        let composer_id = uuid::Uuid::new_v4().to_string();
        let bubble_id = uuid::Uuid::new_v4().to_string();
        let conn = seed_global_db(&root, &composer_id, &bubble_id);
        let meta = load_composer_meta(&conn, &composer_id);
        let parsed = parse_composer_session(&conn, &composer_id, &meta, false).unwrap().unwrap();
        assert_eq!(parsed.messages.len(), 1);
        assert_eq!(parsed.messages[0].content, "hello cursor");
        assert_eq!(parsed.usage_events.len(), 1);
        assert_eq!(parsed.usage_events[0].token_source, TokenSource::Derived);
        assert_eq!(parsed.usage_events[0].input_tokens, 900);
        assert_eq!(parsed.usage_events[0].cache_read_tokens, 300);
        assert_eq!(parsed.usage_events[0].output_tokens, 0);
        assert_eq!(parsed.directory.as_deref(), Some("/Users/x/project"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn parse_composer_session_prefers_bubble_usage_over_context_breakdown() {
        let root = temp_root("bubble-usage");
        let composer_id = uuid::Uuid::new_v4().to_string();
        let bubble_id = uuid::Uuid::new_v4().to_string();
        let conn = seed_global_db(&root, &composer_id, &bubble_id);
        let composer_data = serde_json::json!({
            "composerId": composer_id,
            "createdAt": 1_700_000_000_000_i64,
            "lastUpdatedAt": 1_700_000_100_000_i64,
            "unifiedMode": "chat",
            "modelConfig": { "modelName": "claude-sonnet-4" },
            "promptTokenBreakdown": {
                "totalUsedTokens": 1200,
                "categories": [{ "id": "conversation", "estimatedTokens": 300 }]
            },
            "fullConversationHeadersOnly": [
                { "bubbleId": bubble_id, "type": 2 },
            ]
        });
        conn.execute(
            "UPDATE cursorDiskKV SET value = ?1 WHERE key = ?2",
            rusqlite::params![composer_data.to_string(), format!("composerData:{composer_id}"),],
        )
        .unwrap();
        let bubble = serde_json::json!({
            "type": 2,
            "text": "assistant reply",
            "createdAt": 1_700_000_050_000_i64,
            "tokenCount": { "inputTokens": 12, "outputTokens": 34 }
        });
        conn.execute(
            "UPDATE cursorDiskKV SET value = ?1 WHERE key = ?2",
            rusqlite::params![bubble.to_string(), format!("bubbleId:{composer_id}:{bubble_id}"),],
        )
        .unwrap();

        let meta = load_composer_meta(&conn, &composer_id);
        let parsed = parse_composer_session(&conn, &composer_id, &meta, false).unwrap().unwrap();
        assert_eq!(parsed.usage_events.len(), 1);
        assert_eq!(parsed.usage_events[0].token_source, TokenSource::Observed);
        assert_eq!(parsed.usage_events[0].input_tokens, 12);
        assert_eq!(parsed.usage_events[0].output_tokens, 34);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn parse_composer_session_extracts_tool_events() {
        let root = temp_root("tool-events");
        let composer_id = uuid::Uuid::new_v4().to_string();
        let bubble_id = uuid::Uuid::new_v4().to_string();
        let conn = seed_global_db(&root, &composer_id, &bubble_id);
        let composer_data = serde_json::json!({
            "composerId": composer_id,
            "createdAt": 1_700_000_000_000_i64,
            "lastUpdatedAt": 1_700_000_100_000_i64,
            "unifiedMode": "chat",
            "modelConfig": { "modelName": "claude-sonnet-4" },
            "fullConversationHeadersOnly": [
                { "bubbleId": bubble_id, "type": 2 },
            ]
        });
        conn.execute(
            "UPDATE cursorDiskKV SET value = ?1 WHERE key = ?2",
            rusqlite::params![composer_data.to_string(), format!("composerData:{composer_id}"),],
        )
        .unwrap();
        let bubble = serde_json::json!({
            "type": 2,
            "text": "",
            "createdAt": 1_700_000_050_000_i64,
            "toolFormerData": {
                "name": "grep",
                "rawArgs": "{\"pattern\":\"usage\"}",
                "result": "{\"matches\":1}"
            }
        });
        conn.execute(
            "UPDATE cursorDiskKV SET value = ?1 WHERE key = ?2",
            rusqlite::params![bubble.to_string(), format!("bubbleId:{composer_id}:{bubble_id}"),],
        )
        .unwrap();

        let meta = load_composer_meta(&conn, &composer_id);
        let parsed = parse_composer_session(&conn, &composer_id, &meta, true).unwrap().unwrap();
        assert_eq!(parsed.events.len(), 2);
        assert_eq!(parsed.events[0].kind, "search");
        assert_eq!(parsed.events[0].name.as_deref(), Some("grep"));
        assert_eq!(parsed.events[1].kind, "tool_result");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn parse_agent_transcript_extracts_tool_use_events() {
        let root = temp_root("transcript-events");
        let uuid = uuid::Uuid::new_v4().to_string();
        let jsonl_path = root.join(format!("{uuid}.jsonl"));
        write_jsonl(
            &jsonl_path,
            &[
                r#"{"role":"user","message":{"content":[{"type":"text","text":"<user_query>\nhello\n</user_query>"}]}}"#,
                r#"{"role":"assistant","message":{"content":[{"type":"text","text":"searching"},{"type":"tool_use","name":"Glob","input":{"glob_pattern":"*.rs"}}]}}"#,
            ],
        );
        let raw = parse_agent_transcript(&jsonl_path, true).unwrap().unwrap();
        assert_eq!(raw.events.len(), 1);
        assert_eq!(raw.events[0].kind, "search");
        assert_eq!(raw.events[0].name.as_deref(), Some("Glob"));
        assert_eq!(raw.event_parser_version, Some(EVENT_PARSER_VERSION));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn parse_agent_transcript_happy_path() {
        let root = temp_root("parse");
        let uuid = uuid::Uuid::new_v4().to_string();
        let jsonl_path = root.join(format!("{uuid}.jsonl"));
        write_jsonl(
            &jsonl_path,
            &[
                r#"{"role":"user","message":{"content":[{"type":"text","text":"<user_query>\nhello\n</user_query>"}]}}"#,
                r#"{"role":"assistant","message":{"content":[{"type":"text","text":"hi"},{"type":"tool_use","name":"Glob","input":{"glob_pattern":"*.rs"}}]}}"#,
            ],
        );
        let raw = parse_agent_transcript(&jsonl_path, true).unwrap().unwrap();
        assert_eq!(raw.messages.len(), 2);
        assert_eq!(raw.messages[0].content, "hello");
        assert!(raw.messages[1].content.contains("[tool_use:Glob]"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn infer_cursor_provider_maps_models() {
        assert_eq!(infer_cursor_provider("claude-sonnet-4"), "anthropic");
        assert_eq!(infer_cursor_provider("composer-2.5"), "cursor");
        assert_eq!(infer_cursor_provider("gpt-4.1"), "openai");
    }
}
