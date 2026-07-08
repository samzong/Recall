use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;
use tracing::debug;
use walkdir::WalkDir;

use crate::adapters::events;
use crate::adapters::file_scan::{self, FileScanEntry};
use crate::adapters::json_util::{jsonl_indexed, rfc3339_ms};
use crate::adapters::paths::resolve_home_dir;
use crate::adapters::{
    RawMessage, RawSession, ResumeCommand, SourceAdapter, SyncScanResult, SyncScanStats,
    first_timestamp,
};
use crate::db::store::Store;
use crate::types::{RawSessionEvent, RawUsageEvent, Role, TokenSource};

pub(crate) struct ClaudeCodeAdapter;

const USAGE_PARSER_VERSION: u32 = 5;
const EVENT_PARSER_VERSION: u32 = 2;

impl SourceAdapter for ClaudeCodeAdapter {
    fn id(&self) -> &str {
        "claude-code"
    }
    fn label(&self) -> &str {
        "CC"
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        Some(ResumeCommand {
            program: "claude".to_string(),
            args: vec!["--resume".to_string(), source_id.to_string()],
        })
    }

    fn usage_parser_version(&self) -> Option<u32> {
        Some(USAGE_PARSER_VERSION)
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let Some(claude_dir) = resolve_claude_dir()? else {
            return Ok(vec![]);
        };
        let mut indexes = load_session_indexes(&claude_dir);

        let mut sessions = Vec::new();
        let mut entries = collect_project_entries(&claude_dir, &mut indexes);
        entries.extend(collect_transcript_entries(&claude_dir));

        for entry in entries {
            let Some(mtime_ms) = file_scan::stat_mtime_ms(&entry.stat_target) else {
                continue;
            };
            if let Some(raw) = parse_claude_session_file(entry, mtime_ms, &indexes, true)? {
                sessions.push(raw);
            }
        }

        Ok(sessions)
    }

    fn scan_for_sync(
        &self,
        store: &Store,
        since_ts: Option<i64>,
        include_events: bool,
    ) -> anyhow::Result<Option<SyncScanResult>> {
        let Some(claude_dir) = resolve_claude_dir()? else {
            return Ok(Some(SyncScanResult { sessions: vec![], stats: SyncScanStats::default() }));
        };
        let result = scan_for_sync_impl(&claude_dir, store, since_ts, include_events)?;
        Ok(Some(result))
    }
}

struct SessionMeta {
    cwd: Option<String>,
    started_at: Option<i64>,
    entrypoint: Option<String>,
}

#[derive(Default)]
struct SessionIndexes {
    live: HashMap<String, SessionMeta>,
    project_summaries: HashMap<String, String>,
}

fn load_session_indexes(claude_dir: &Path) -> SessionIndexes {
    SessionIndexes { live: load_session_index(claude_dir), project_summaries: HashMap::new() }
}

fn resolve_claude_dir() -> anyhow::Result<Option<PathBuf>> {
    resolve_home_dir(".claude", "~/.claude not found, skipping Claude Code")
}

fn scan_for_sync_impl(
    claude_dir: &Path,
    store: &Store,
    since_ts: Option<i64>,
    include_events: bool,
) -> anyhow::Result<SyncScanResult> {
    let mut indexes = load_session_indexes(claude_dir);
    let mut entries = collect_project_entries(claude_dir, &mut indexes);
    entries.extend(collect_transcript_entries(claude_dir));

    file_scan::run_file_scan_with_options(
        store,
        "claude-code",
        since_ts,
        file_scan::FileScanOptions {
            usage_parser_version: Some(USAGE_PARSER_VERSION),
            event_parser_version: include_events.then_some(EVENT_PARSER_VERSION),
        },
        entries,
        |entry, mtime_ms| parse_claude_session_file(entry, mtime_ms, &indexes, include_events),
    )
}

fn load_session_index(claude_dir: &Path) -> HashMap<String, SessionMeta> {
    let sessions_dir = claude_dir.join("sessions");
    let mut index = HashMap::new();
    if !sessions_dir.exists() {
        return index;
    }

    let entries = match fs::read_dir(&sessions_dir) {
        Ok(e) => e,
        Err(e) => {
            debug!("cannot read ~/.claude/sessions: {e}");
            return index;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let v: Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(session_id) = v.get("sessionId").and_then(|s| s.as_str()) {
            let meta = SessionMeta {
                cwd: v.get("cwd").and_then(|s| s.as_str()).map(|s| s.to_string()),
                started_at: v.get("startedAt").and_then(|s| s.as_i64()),
                entrypoint: v.get("entrypoint").and_then(|s| s.as_str()).map(|s| s.to_string()),
            };
            index.insert(session_id.to_string(), meta);
        }
    }
    index
}

fn collect_project_entries(claude_dir: &Path, indexes: &mut SessionIndexes) -> Vec<FileScanEntry> {
    let projects_dir = claude_dir.join("projects");
    if !projects_dir.exists() {
        return vec![];
    }

    let mut entries = Vec::new();

    let project_dirs = match fs::read_dir(&projects_dir) {
        Ok(e) => e,
        Err(e) => {
            debug!("cannot read ~/.claude/projects: {e}");
            return vec![];
        }
    };

    for project_entry in project_dirs.flatten() {
        let project_path = project_entry.path();
        if !project_path.is_dir() {
            continue;
        }

        let dir_name = project_entry.file_name().to_string_lossy().to_string();
        let directory_hint = project_key_to_path(&dir_name);
        merge_project_session_summaries(&project_path, &mut indexes.project_summaries);

        for file_entry in WalkDir::new(&project_path).into_iter().filter_map(|e| e.ok()) {
            let file_path = file_entry.path();
            if file_path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            if !file_path.is_file() {
                continue;
            }
            let session_id = match file_path.file_stem().and_then(|s| s.to_str()) {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => continue,
            };

            let meta_cwd = indexes.live.get(&session_id).and_then(|m| m.cwd.clone());
            let directory = meta_cwd.or_else(|| Some(directory_hint.clone()));

            entries.push(FileScanEntry {
                session_id,
                stat_target: file_path.to_path_buf(),
                directory,
            });
        }
    }

    entries
}

fn merge_project_session_summaries(
    project_path: &Path,
    project_summaries: &mut HashMap<String, String>,
) {
    let index_path = project_path.join("sessions-index.json");
    let content = match fs::read_to_string(&index_path) {
        Ok(content) => content,
        Err(_) => return,
    };
    let v: Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(err) => {
            debug!("failed to parse {}: {err}", index_path.display());
            return;
        }
    };
    let Some(entries) = v.get("entries").and_then(|entries| entries.as_array()) else {
        return;
    };
    for entry in entries {
        let Some(session_id) = entry.get("sessionId").and_then(|id| id.as_str()) else {
            continue;
        };
        let Some(summary) = entry
            .get("summary")
            .and_then(|summary| summary.as_str())
            .map(str::trim)
            .filter(|summary| !summary.is_empty())
        else {
            continue;
        };
        project_summaries.insert(session_id.to_string(), summary.to_string());
    }
}

fn collect_transcript_entries(claude_dir: &Path) -> Vec<FileScanEntry> {
    let transcripts_dir = claude_dir.join("transcripts");
    if !transcripts_dir.exists() {
        return vec![];
    }

    let mut entries = Vec::new();

    for entry in WalkDir::new(&transcripts_dir).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        if !path.is_file() {
            continue;
        }
        let session_id = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => continue,
        };

        entries.push(FileScanEntry {
            session_id,
            stat_target: path.to_path_buf(),
            directory: None,
        });
    }

    entries
}

fn parse_claude_session_file(
    entry: FileScanEntry,
    mtime_ms: i64,
    indexes: &SessionIndexes,
    include_events: bool,
) -> anyhow::Result<Option<RawSession>> {
    let parsed = match parse_conversation_jsonl(&entry.stat_target, mtime_ms, include_events) {
        Ok(parsed) => parsed,
        Err(e) => {
            debug!("failed to parse {}: {e}", entry.stat_target.display());
            return Ok(None);
        }
    };

    if parsed.messages.is_empty() && parsed.usage_events.is_empty() {
        return Ok(None);
    }

    let meta = indexes.live.get(&entry.session_id);
    let started_at = first_timestamp(
        meta.and_then(|m| m.started_at),
        &parsed.messages,
        &parsed.usage_events,
        &[],
    )
    .unwrap_or(0);
    let directory =
        meta.and_then(|m| m.cwd.clone()).or_else(|| parsed.cwd.clone()).or(entry.directory);
    let entrypoint = meta.and_then(|m| m.entrypoint.clone());
    let source_file_path = entry.stat_target.to_str().map(|s| s.to_string());
    let duration_minutes = match (parsed.first_ts, parsed.last_ts) {
        (Some(first), Some(last)) if last >= first => Some(((last - first) / 60_000) as u32),
        _ => None,
    };
    let summary =
        parsed.summary.or_else(|| indexes.project_summaries.get(&entry.session_id).cloned());
    Ok(Some(RawSession {
        source_id: entry.session_id,
        directory,
        started_at,
        updated_at: Some(mtime_ms),
        entrypoint,
        messages: parsed.messages,
        usage_events: parsed.usage_events,
        usage_parser_version: Some(USAGE_PARSER_VERSION),
        events: parsed.events,
        event_parser_version: include_events.then_some(EVENT_PARSER_VERSION),
        source_file_path,
        custom_title: parsed.custom_title,
        summary,
        duration_minutes,
    }))
}

struct ParsedConversation {
    messages: Vec<RawMessage>,
    usage_events: Vec<RawUsageEvent>,
    events: Vec<RawSessionEvent>,
    cwd: Option<String>,
    custom_title: Option<String>,
    summary: Option<String>,
    first_ts: Option<i64>,
    last_ts: Option<i64>,
}

fn parse_conversation_jsonl(
    path: &Path,
    fallback_timestamp: i64,
    include_events: bool,
) -> anyhow::Result<ParsedConversation> {
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut messages = Vec::new();
    let mut usage_events: Vec<RawUsageEvent> = Vec::new();
    let mut events = Vec::new();
    let mut usage_index: HashMap<String, usize> = HashMap::new();
    let mut cwd: Option<String> = None;
    let mut custom_title: Option<String> = None;
    let mut summary: Option<String> = None;
    let mut first_ts: Option<i64> = None;
    let mut last_ts: Option<i64> = None;
    let source_path = path.to_string_lossy().to_string();

    for item in jsonl_indexed(reader.lines()) {
        let (line_index, v) = item?;

        if cwd.is_none()
            && let Some(c) = v.get("cwd").and_then(|s| s.as_str())
            && !c.is_empty()
        {
            cwd = Some(c.to_string());
        }

        let msg_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");

        if msg_type == "custom-title"
            && let Some(title) = v.get("customTitle").and_then(|t| t.as_str())
        {
            let trimmed = title.trim();
            if !trimmed.is_empty() {
                custom_title = Some(trimmed.to_string());
            }
            continue;
        }
        if msg_type == "summary"
            && summary.is_none()
            && let Some(s) = v.get("summary").and_then(|t| t.as_str())
        {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                summary = Some(trimmed.to_string());
            }
            continue;
        }

        match msg_type {
            "user" | "assistant" => {}
            _ => continue,
        }

        let is_machinery = v.get("isCompactSummary").and_then(|b| b.as_bool()).unwrap_or(false)
            || v.get("isSidechain").and_then(|b| b.as_bool()).unwrap_or(false)
            || v.get("isMeta").and_then(|b| b.as_bool()).unwrap_or(false);

        let role = if msg_type == "user" { Role::User } else { Role::Assistant };

        let message = match v.get("message") {
            Some(m) => m,
            None => continue,
        };

        let text = extract_content(message.get("content"));
        let timestamp = rfc3339_ms(v.get("timestamp"));

        let message_seq =
            if !is_machinery && !text.is_empty() { Some(messages.len() as u32) } else { None };

        if role == Role::Assistant
            && let Some(event) = extract_claude_usage_event(
                &v,
                message,
                timestamp.unwrap_or(fallback_timestamp),
                line_index as u32,
                message_seq,
                &source_path,
            )
        {
            if let Some(existing_index) = usage_index.get(&event.event_key).copied() {
                merge_claude_usage_event(&mut usage_events[existing_index], event);
            } else {
                usage_index.insert(event.event_key.clone(), usage_events.len());
                usage_events.push(event);
            }
        }

        if is_machinery {
            continue;
        }

        if let Some(ts) = timestamp {
            if first_ts.is_none_or(|f| ts < f) {
                first_ts = Some(ts);
            }
            if last_ts.is_none_or(|l| ts > l) {
                last_ts = Some(ts);
            }
        }

        if include_events {
            collect_claude_content_events(
                message.get("content"),
                role.clone(),
                timestamp,
                &source_path,
                line_index,
                message_seq,
                &mut events,
            );
        }

        if !text.is_empty() {
            messages.push(RawMessage { role, content: text, timestamp });
        }
    }

    Ok(ParsedConversation {
        messages,
        usage_events,
        events,
        cwd,
        custom_title,
        summary,
        first_ts,
        last_ts,
    })
}

fn collect_claude_content_events(
    content: Option<&Value>,
    role: Role,
    timestamp: Option<i64>,
    source_path: &str,
    line_index: usize,
    message_seq: Option<u32>,
    events_out: &mut Vec<RawSessionEvent>,
) {
    let Some(Value::Array(arr)) = content else {
        return;
    };
    for (item_index, item) in arr.iter().enumerate() {
        match item.get("type").and_then(|t| t.as_str()) {
            Some("tool_use") if role == Role::Assistant => {
                let name = item.get("name").and_then(|n| n.as_str()).unwrap_or("tool").to_string();
                events_out.push(events::tool_call_event(
                    events::EventContext {
                        event_seq: events_out.len() as u32,
                        timestamp,
                        source_path: Some(source_path.to_string()),
                        source_event_id: Some(format!("{line_index}:{item_index}")),
                        message_seq,
                        parser_version: EVENT_PARSER_VERSION,
                    },
                    name,
                    item.get("input"),
                ));
            }
            Some("tool_result") => {
                let summary = item.get("content").map(|content| match content {
                    Value::String(text) => text.to_string(),
                    other => other.to_string(),
                });
                events_out.push(events::tool_result_event(
                    events::EventContext {
                        event_seq: events_out.len() as u32,
                        timestamp,
                        source_path: Some(source_path.to_string()),
                        source_event_id: Some(format!("{line_index}:{item_index}")),
                        message_seq,
                        parser_version: EVENT_PARSER_VERSION,
                    },
                    item.get("tool_use_id").and_then(|id| id.as_str()).map(String::from),
                    summary,
                ));
            }
            _ => {}
        }
    }
}

fn extract_claude_usage_event(
    row: &Value,
    message: &Value,
    timestamp: i64,
    event_seq: u32,
    message_seq: Option<u32>,
    source_path: &str,
) -> Option<RawUsageEvent> {
    let usage = message.get("usage")?;
    let input_tokens = usage.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0).max(0);
    let output_tokens = usage.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0).max(0);
    let cache_read_tokens =
        usage.get("cache_read_input_tokens").and_then(|v| v.as_i64()).unwrap_or(0).max(0);
    let cache_write_tokens =
        usage.get("cache_creation_input_tokens").and_then(|v| v.as_i64()).unwrap_or(0).max(0);

    let model = message.get("model").and_then(|v| v.as_str()).filter(|v| !v.trim().is_empty())?;

    let request_id = row.get("requestId").and_then(|v| v.as_str());
    let message_id = message.get("id").and_then(|v| v.as_str());
    let event_key = match (request_id, message_id) {
        (Some(request_id), Some(message_id)) => format!("assistant:{request_id}:{message_id}"),
        (Some(request_id), None) => format!("assistant:{request_id}:line:{event_seq}"),
        (None, Some(message_id)) => format!("assistant:{message_id}:line:{event_seq}"),
        (None, None) => format!("line:{event_seq}"),
    };

    Some(RawUsageEvent {
        event_key,
        event_seq,
        message_seq,
        timestamp,
        model: model.to_string(),
        provider: "anthropic".to_string(),
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_write_tokens,
        reasoning_tokens: 0,
        token_source: TokenSource::Observed,
        parser_version: USAGE_PARSER_VERSION,
        source_path: Some(source_path.to_string()),
        raw_usage_json: Some(usage.to_string()),
    })
}

fn merge_claude_usage_event(existing: &mut RawUsageEvent, next: RawUsageEvent) {
    existing.input_tokens = existing.input_tokens.max(next.input_tokens);
    existing.output_tokens = existing.output_tokens.max(next.output_tokens);
    existing.cache_read_tokens = existing.cache_read_tokens.max(next.cache_read_tokens);
    existing.cache_write_tokens = existing.cache_write_tokens.max(next.cache_write_tokens);
    existing.reasoning_tokens = existing.reasoning_tokens.max(next.reasoning_tokens);
    existing.timestamp = existing.timestamp.max(next.timestamp);
    existing.raw_usage_json = next.raw_usage_json;
}

fn extract_content(content: Option<&Value>) -> String {
    match content {
        None => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => {
            let mut parts = Vec::new();
            for item in arr {
                match item.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                            parts.push(text.to_string());
                        }
                    }
                    Some("tool_use") => {
                        let name = item.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                        if let Some(input) = item.get("input") {
                            parts.push(format!("[{name}] {input}"));
                        }
                    }
                    Some("tool_result") => {
                        if let Some(content) = item.get("content") {
                            match content {
                                Value::String(s) => parts.push(s.clone()),
                                Value::Array(inner) => {
                                    for block in inner {
                                        if block.get("type").and_then(|t| t.as_str())
                                            == Some("text")
                                            && let Some(text) =
                                                block.get("text").and_then(|t| t.as_str())
                                        {
                                            parts.push(text.to_string());
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
            parts.join("\n")
        }
        _ => String::new(),
    }
}

fn project_key_to_path(key: &str) -> String {
    let key = key.strip_prefix('-').unwrap_or(key);
    let mut result = String::with_capacity(key.len() + 1);
    result.push('/');
    let mut chars = key.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '-' {
            if chars.peek() == Some(&'-') {
                chars.next();
                result.push_str("/.");
            } else {
                result.push('/');
            }
        } else {
            result.push(c);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    use crate::db::{schema, store::Store};
    use crate::types::Session;

    fn setup_store() -> Store {
        schema::register_sqlite_vec();
        Store::open_in_memory().unwrap()
    }

    fn temp_claude_root(label: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("recall-cc-test-{}-{}", label, uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_user_jsonl(project_dir: &Path, session_id: &str, text: &str) -> PathBuf {
        fs::create_dir_all(project_dir).unwrap();
        let path = project_dir.join(format!("{session_id}.jsonl"));
        let line = serde_json::json!({
            "type": "user",
            "message": {"content": text},
            "timestamp": "2026-04-13T10:00:00Z"
        });
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "{line}").unwrap();
        path
    }

    #[test]
    fn parse_claude_session_file_extracts_structured_tool_events() {
        let root = temp_claude_root("events");
        let project = root.join("projects").join("-tmp-foo");
        fs::create_dir_all(&project).unwrap();
        let path = project.join("tool-session.jsonl");
        let assistant = serde_json::json!({
            "type": "assistant",
            "timestamp": "2026-04-13T10:00:00Z",
            "message": {
                "content": [
                    {"type": "tool_use", "id": "tool-1", "name": "Read", "input": {"path": "src/main.rs"}}
                ]
            }
        });
        let user = serde_json::json!({
            "type": "user",
            "timestamp": "2026-04-13T10:00:01Z",
            "message": {
                "content": [
                    {"type": "tool_result", "tool_use_id": "tool-1", "content": "file body"}
                ]
            }
        });
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "{assistant}").unwrap();
        writeln!(f, "{user}").unwrap();
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();

        let entry = FileScanEntry {
            session_id: "tool-session".to_string(),
            stat_target: path.clone(),
            directory: Some("/tmp/foo".to_string()),
        };
        let indexes = SessionIndexes::default();
        let raw = parse_claude_session_file(entry, mtime, &indexes, true).unwrap().unwrap();

        assert_eq!(raw.events.len(), 2);
        assert_eq!(raw.events[0].kind, "file_read");
        assert_eq!(raw.events[0].name.as_deref(), Some("Read"));
        assert_eq!(raw.events[0].target.as_deref(), Some("src/main.rs"));
        assert_eq!(raw.events[1].kind, "tool_result");

        let entry = FileScanEntry {
            session_id: "tool-session".to_string(),
            stat_target: path,
            directory: Some("/tmp/foo".to_string()),
        };
        let raw = parse_claude_session_file(entry, mtime, &indexes, false).unwrap().unwrap();

        assert!(raw.events.is_empty());
        assert_eq!(raw.event_parser_version, None);

        let _ = fs::remove_dir_all(&root);
    }

    fn write_usage_jsonl(project_dir: &Path, session_id: &str) -> PathBuf {
        fs::create_dir_all(project_dir).unwrap();
        let path = project_dir.join(format!("{session_id}.jsonl"));
        let first = serde_json::json!({
            "type": "assistant",
            "requestId": "req-1",
            "timestamp": "2026-04-13T10:00:00Z",
            "message": {
                "id": "msg-1",
                "model": "claude-sonnet-4-5",
                "content": [{"type": "text", "text": "partial"}],
                "usage": {
                    "input_tokens": 100,
                    "output_tokens": 20,
                    "cache_read_input_tokens": 50,
                    "cache_creation_input_tokens": 5
                }
            }
        });
        let second = serde_json::json!({
            "type": "assistant",
            "requestId": "req-1",
            "timestamp": "2026-04-13T10:00:02Z",
            "message": {
                "id": "msg-1",
                "model": "claude-sonnet-4-5",
                "content": [{"type": "text", "text": "complete"}],
                "usage": {
                    "input_tokens": 100,
                    "output_tokens": 30,
                    "cache_read_input_tokens": 50,
                    "cache_creation_input_tokens": 5
                }
            }
        });
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "{first}").unwrap();
        writeln!(f, "{second}").unwrap();
        path
    }

    fn write_usage_only_jsonl(project_dir: &Path, session_id: &str) -> PathBuf {
        fs::create_dir_all(project_dir).unwrap();
        let path = project_dir.join(format!("{session_id}.jsonl"));
        let line = serde_json::json!({
            "type": "assistant",
            "requestId": "req-usage-only",
            "timestamp": "2026-04-13T10:00:00Z",
            "message": {
                "id": "msg-usage-only",
                "model": "claude-opus-4-7",
                "content": [],
                "usage": {
                    "input_tokens": 10,
                    "output_tokens": 20,
                    "cache_read_input_tokens": 30,
                    "cache_creation_input_tokens": 40
                }
            }
        });
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "{line}").unwrap();
        path
    }

    fn make_existing_session(source_id: &str, updated_at: i64, message_count: u32) -> Session {
        Session {
            id: format!("internal-{source_id}"),
            source: "claude-code".to_string(),
            source_id: source_id.to_string(),
            title: "existing".to_string(),
            directory: None,
            repo_remote: None,
            repo_slug: None,
            repo_name: None,
            started_at: 0,
            updated_at: Some(updated_at),
            message_count,
            entrypoint: None,
            custom_title: None,
            summary: None,
            duration_minutes: None,
            source_file_path: None,
            is_import: false,
        }
    }

    #[test]
    fn parse_claude_session_file_sets_updated_at_to_mtime() {
        let root = temp_claude_root("parse");
        let project = root.join("projects").join("-tmp-foo");
        let path = write_user_jsonl(&project, "abc-123", "hello");
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();

        let entry = FileScanEntry {
            session_id: "abc-123".to_string(),
            stat_target: path.clone(),
            directory: Some("/tmp/foo".to_string()),
        };
        let indexes = SessionIndexes::default();
        let raw = parse_claude_session_file(entry, mtime, &indexes, true).unwrap().unwrap();

        assert_eq!(raw.source_id, "abc-123");
        assert_eq!(raw.updated_at, Some(mtime));
        assert_eq!(raw.directory.as_deref(), Some("/tmp/foo"));
        assert_eq!(raw.messages.len(), 1);
        assert_eq!(raw.messages[0].content, "hello");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_claude_session_file_uses_jsonl_cwd_between_session_index_and_entry_hint() {
        let root = temp_claude_root("cwd");
        let project = root.join("projects").join("-tmp-encoded");
        fs::create_dir_all(&project).unwrap();
        let path = project.join("cwd-session.jsonl");
        let line = serde_json::json!({
            "type": "user",
            "cwd": "/tmp/from-jsonl",
            "message": {"content": "hello"},
            "timestamp": "2026-04-13T10:00:00Z"
        });
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "{line}").unwrap();
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();

        let entry = FileScanEntry {
            session_id: "cwd-session".to_string(),
            stat_target: path.clone(),
            directory: Some("/tmp/from-entry".to_string()),
        };
        let indexes = SessionIndexes::default();
        let raw = parse_claude_session_file(entry, mtime, &indexes, true).unwrap().unwrap();
        assert_eq!(raw.directory.as_deref(), Some("/tmp/from-jsonl"));
        assert_eq!(raw.duration_minutes, Some(0));
        assert_eq!(raw.source_file_path.as_deref(), path.to_str());

        let entry = FileScanEntry {
            session_id: "cwd-session".to_string(),
            stat_target: path,
            directory: Some("/tmp/from-entry".to_string()),
        };
        let indexes = SessionIndexes {
            live: HashMap::from([(
                "cwd-session".to_string(),
                SessionMeta {
                    cwd: Some("/tmp/from-index".to_string()),
                    started_at: None,
                    entrypoint: None,
                },
            )]),
            project_summaries: HashMap::new(),
        };
        let raw = parse_claude_session_file(entry, mtime, &indexes, true).unwrap().unwrap();
        assert_eq!(raw.directory.as_deref(), Some("/tmp/from-index"));
        assert_eq!(raw.started_at, 1_776_074_400_000);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_claude_session_file_extracts_title_summary_and_duration() {
        let root = temp_claude_root("metadata");
        let project = root.join("projects").join("-tmp-meta");
        fs::create_dir_all(&project).unwrap();
        let path = project.join("meta-session.jsonl");
        let lines = [
            serde_json::json!({
                "type": "user",
                "message": {"content": "start"},
                "timestamp": "2026-04-13T10:00:00Z"
            }),
            serde_json::json!({
                "type": "custom-title",
                "customTitle": "First title"
            }),
            serde_json::json!({
                "type": "custom-title",
                "customTitle": "Final title"
            }),
            serde_json::json!({
                "type": "custom-title",
                "customTitle": "   "
            }),
            serde_json::json!({
                "type": "summary",
                "summary": "   "
            }),
            serde_json::json!({
                "type": "summary",
                "summary": "First summary"
            }),
            serde_json::json!({
                "type": "summary",
                "summary": "Second summary"
            }),
            serde_json::json!({
                "type": "assistant",
                "message": {"content": "done"},
                "timestamp": "2026-04-13T10:02:00Z"
            }),
        ];
        let mut f = fs::File::create(&path).unwrap();
        for line in lines {
            writeln!(f, "{line}").unwrap();
        }
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();

        let entry = FileScanEntry {
            session_id: "meta-session".to_string(),
            stat_target: path,
            directory: Some("/tmp/meta".to_string()),
        };
        let indexes = SessionIndexes::default();
        let raw = parse_claude_session_file(entry, mtime, &indexes, true).unwrap().unwrap();

        assert_eq!(raw.custom_title.as_deref(), Some("Final title"));
        assert_eq!(raw.summary.as_deref(), Some("First summary"));
        assert_eq!(raw.duration_minutes, Some(2));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_for_sync_uses_project_sessions_index_summary() {
        let root = temp_claude_root("project-summary");
        let project = root.join("projects").join("-tmp-index");
        let _path = write_user_jsonl(&project, "index-session", "hello");
        let index = serde_json::json!({
            "version": 1,
            "entries": [
                {
                    "sessionId": "index-session",
                    "summary": "Project index summary"
                }
            ]
        });
        fs::write(project.join("sessions-index.json"), index.to_string()).unwrap();

        let store = setup_store();
        let result = scan_for_sync_impl(&root, &store, None, true).unwrap();

        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].summary.as_deref(), Some("Project index summary"));
        assert_eq!(result.sessions[0].started_at, 1_776_074_400_000);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_claude_session_file_extracts_deduped_usage() {
        let root = temp_claude_root("usage");
        let project = root.join("projects").join("-tmp-foo");
        let path = write_usage_jsonl(&project, "usage-session");
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();

        let entry = FileScanEntry {
            session_id: "usage-session".to_string(),
            stat_target: path,
            directory: Some("/tmp/foo".to_string()),
        };
        let indexes = SessionIndexes::default();
        let raw = parse_claude_session_file(entry, mtime, &indexes, true).unwrap().unwrap();

        assert_eq!(raw.usage_events.len(), 1);
        let event = &raw.usage_events[0];
        assert_eq!(event.event_key, "assistant:req-1:msg-1");
        assert_eq!(event.model, "claude-sonnet-4-5");
        assert_eq!(event.provider, "anthropic");
        assert_eq!(event.input_tokens, 100);
        assert_eq!(event.output_tokens, 30);
        assert_eq!(event.cache_read_tokens, 50);
        assert_eq!(event.cache_write_tokens, 5);
        assert_eq!(event.token_source, TokenSource::Observed);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_claude_session_file_keeps_usage_without_searchable_messages() {
        let root = temp_claude_root("usage-only");
        let project = root.join("projects").join("-tmp-foo");
        let path = write_usage_only_jsonl(&project, "usage-only-session");
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();

        let entry = FileScanEntry {
            session_id: "usage-only-session".to_string(),
            stat_target: path,
            directory: Some("/tmp/foo".to_string()),
        };
        let indexes = SessionIndexes::default();
        let raw = parse_claude_session_file(entry, mtime, &indexes, true).unwrap().unwrap();

        assert!(raw.messages.is_empty());
        assert_eq!(raw.started_at, 1_776_074_400_000);
        assert_eq!(raw.usage_events.len(), 1);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_claude_session_file_keeps_zero_token_usage_events() {
        let root = temp_claude_root("zero-usage");
        let project = root.join("projects").join("-tmp-foo");
        fs::create_dir_all(&project).unwrap();
        let path = project.join("zero-session.jsonl");
        let line = serde_json::json!({
            "type": "assistant",
            "requestId": "req-zero",
            "timestamp": "2026-04-13T10:00:00Z",
            "message": {
                "id": "msg-zero",
                "model": "gpt-5.5",
                "content": [{"type": "text", "text": "zero"}],
                "usage": {
                    "input_tokens": 0,
                    "output_tokens": 0
                }
            }
        });
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "{line}").unwrap();
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();

        let entry = FileScanEntry {
            session_id: "zero-session".to_string(),
            stat_target: path,
            directory: Some("/tmp/foo".to_string()),
        };
        let indexes = SessionIndexes::default();
        let raw = parse_claude_session_file(entry, mtime, &indexes, true).unwrap().unwrap();

        assert_eq!(raw.usage_events.len(), 1);
        assert_eq!(raw.usage_events[0].model, "gpt-5.5");
        assert_eq!(raw.usage_events[0].input_tokens, 0);
        assert_eq!(raw.usage_events[0].output_tokens, 0);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn collect_project_entries_walks_nested_projects() {
        let root = temp_claude_root("collect");
        let p1 = root.join("projects").join("-tmp-foo");
        let p2 = root.join("projects").join("-tmp-bar");
        let nested = p1.join("parent-session").join("subagents");
        write_user_jsonl(&p1, "sess-1", "a");
        write_user_jsonl(&p2, "sess-2", "b");
        write_user_jsonl(&nested, "agent-a123", "nested");

        let mut indexes = SessionIndexes::default();
        let entries = collect_project_entries(&root, &mut indexes);
        assert_eq!(entries.len(), 3);
        let ids: Vec<_> = entries.iter().map(|e| e.session_id.clone()).collect();
        assert!(ids.contains(&"sess-1".to_string()));
        assert!(ids.contains(&"sess-2".to_string()));
        assert!(ids.contains(&"agent-a123".to_string()));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_for_sync_skips_unchanged_session() {
        let root = temp_claude_root("skip");
        let project = root.join("projects").join("-tmp-proj");
        let path = write_user_jsonl(&project, "sess-skip", "hello");
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();

        let store = setup_store();
        store.insert_session(&make_existing_session("sess-skip", mtime, 1)).unwrap();
        store
            .persist_usage_events_for_existing_session(
                "claude-code",
                "sess-skip",
                &[],
                USAGE_PARSER_VERSION,
                Some(mtime),
            )
            .unwrap();
        store
            .persist_session_events_for_existing_session(
                "claude-code",
                "sess-skip",
                &[],
                EVENT_PARSER_VERSION,
                Some(mtime),
            )
            .unwrap();

        let result = scan_for_sync_impl(&root, &store, None, true).unwrap();
        assert_eq!(result.sessions.len(), 0);
        assert_eq!(result.stats.skipped_sessions, 1);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_for_sync_reparses_when_mtime_diverges() {
        let root = temp_claude_root("mismatch");
        let project = root.join("projects").join("-tmp-proj");
        let path = write_user_jsonl(&project, "sess-stale", "hi");
        let actual_mtime = file_scan::stat_mtime_ms(&path).unwrap();

        let store = setup_store();
        store
            .insert_session(&make_existing_session("sess-stale", actual_mtime - 1_000, 1))
            .unwrap();

        let result = scan_for_sync_impl(&root, &store, None, true).unwrap();
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].source_id, "sess-stale");
        assert_eq!(result.sessions[0].updated_at, Some(actual_mtime));
        assert_eq!(result.stats.skipped_sessions, 0);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_for_sync_picks_up_new_session() {
        let root = temp_claude_root("new");
        let project = root.join("projects").join("-tmp-proj");
        write_user_jsonl(&project, "sess-fresh", "fresh");

        let store = setup_store();

        let result = scan_for_sync_impl(&root, &store, None, true).unwrap();
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].source_id, "sess-fresh");
        assert_eq!(result.stats.skipped_sessions, 0);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn project_key_to_path_decodes_dashes() {
        assert_eq!(project_key_to_path("-tmp-foo"), "/tmp/foo");
        assert_eq!(
            project_key_to_path("-Users-x-git-samzong-Recall"),
            "/Users/x/git/samzong/Recall"
        );
    }

    #[test]
    fn machinery_turn_keeps_usage_but_drops_message() {
        let dir = std::env::temp_dir().join(format!("recall-mach-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("sess.jsonl");
        let mut f = fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"{{"type":"user","message":{{"role":"user","content":"hi"}},"timestamp":"2026-05-20T10:00:00Z"}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","isSidechain":true,"message":{{"role":"assistant","content":"sub-agent work","usage":{{"input_tokens":100,"output_tokens":50}},"model":"claude-x"}},"timestamp":"2026-05-20T10:00:01Z"}}"#
        )
        .unwrap();

        let parsed = parse_conversation_jsonl(&path, 0, true).unwrap();

        assert!(
            parsed.messages.iter().all(|m| m.content != "sub-agent work"),
            "machinery turn must not be a stored message"
        );
        assert!(
            !parsed.usage_events.is_empty(),
            "usage event from the machinery turn must be preserved"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
