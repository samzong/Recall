use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use tracing::debug;
use walkdir::WalkDir;

use crate::adapters::events::{EventContext, tool_call_event, tool_result_event};
use crate::adapters::file_scan::{self, FileScanEntry, FileScanOptions};
use crate::adapters::json_util::rfc3339_ms;
use crate::adapters::paths;
use crate::adapters::usage::usage_count;
use crate::adapters::{
    AdapterSyncContext, RawMessage, RawSession, ResumeCommand, SourceAdapter, SyncScanResult,
};
use crate::types::{
    EvidenceVisibility, FileEvidence, FileEvidenceKind, FileOperation, RawSessionEvent,
    RawUsageEvent, Role,
};

pub(crate) struct GeminiAdapter;

const USAGE_PARSER_VERSION: u32 = 4;
const METADATA_PARSER_VERSION: u32 = 1;
const EVENT_PARSER_VERSION: u32 = 1;

impl SourceAdapter for GeminiAdapter {
    fn id(&self) -> &str {
        "gemini-cli"
    }
    fn label(&self) -> &str {
        "GEM"
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        Some(ResumeCommand {
            program: "gemini".to_string(),
            args: vec!["--resume".to_string(), source_id.to_string()],
        })
    }

    fn start_command(&self, prompt: String) -> Option<ResumeCommand> {
        Some(crate::adapters::prompt_start("gemini", prompt))
    }

    fn usage_parser_version(&self) -> Option<u32> {
        Some(USAGE_PARSER_VERSION)
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let Some(root) = resolve_gemini_root()? else {
            return Ok(vec![]);
        };

        let mut sessions = Vec::new();
        for path in collect_gemini_session_files(&root) {
            match parse_gemini_session_file(&path, true) {
                Ok(Some(session)) => sessions.push(session),
                Ok(None) => {}
                Err(e) => {
                    debug!("failed to parse gemini session {}: {e}", path.display());
                }
            }
        }

        Ok(sessions)
    }

    fn scan_for_sync(
        &self,
        context: &AdapterSyncContext,
        since_ts: Option<i64>,
        include_events: bool,
    ) -> anyhow::Result<Option<SyncScanResult>> {
        let Some(root) = resolve_gemini_root()? else {
            return Ok(Some(SyncScanResult::default()));
        };
        Ok(Some(scan_gemini_for_sync(&root, context, since_ts, include_events)?))
    }
}

struct GeminiDocument {
    value: Value,
    message_lines: Vec<usize>,
    inactive: Vec<(Value, usize)>,
}

fn scan_gemini_for_sync(
    root: &Path,
    context: &AdapterSyncContext,
    since_ts: Option<i64>,
    include_events: bool,
) -> anyhow::Result<SyncScanResult> {
    let mut combined = SyncScanResult::default();
    for path in collect_gemini_session_files(root) {
        let Some(snapshot) = file_scan::file_metadata_snapshot(&path) else {
            continue;
        };
        let doc = match read_gemini_document(&path) {
            Ok(doc) => doc,
            Err(error) => {
                debug!("failed to parse gemini session {}: {error}", path.display());
                continue;
            }
        };
        let fallback = path.file_stem().and_then(|stem| stem.to_str()).unwrap_or("unknown");
        let session_id =
            doc.value.get("sessionId").and_then(Value::as_str).unwrap_or(fallback).to_string();
        combined.absorb(file_scan::run_file_scan_with_options_and_snapshot(
            context,
            since_ts,
            FileScanOptions {
                usage_parser_version: Some(USAGE_PARSER_VERSION),
                event_parser_version: include_events.then_some(EVENT_PARSER_VERSION),
                metadata_parser_version: include_events.then_some(METADATA_PARSER_VERSION),
            },
            [FileScanEntry { session_id, stat_target: path.clone(), directory: None }],
            |entry| {
                let current = file_scan::file_metadata_snapshot(&entry.stat_target)?;
                if current != snapshot {
                    return None;
                }
                Some(file_scan::FileScanSnapshot::new(current.mtime_ms()?, current))
            },
            |entry, mtime_ms| {
                let mut session = parse_gemini_session_value(
                    &doc,
                    &entry.session_id,
                    Some(entry.stat_target.display().to_string()),
                    include_events,
                )?;
                if let Some(session) = &mut session {
                    session.updated_at = Some(mtime_ms);
                }
                Ok(session)
            },
        )?);
    }
    Ok(combined)
}

fn resolve_gemini_root() -> anyhow::Result<Option<PathBuf>> {
    if let Some(dir) = paths::env_path_dir("GEMINI_HOME") {
        if dir.is_dir() {
            return Ok(Some(dir));
        }
        debug!("GEMINI_HOME not found, skipping Gemini CLI");
        return Ok(None);
    }
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home dir"))?;
    let gemini_tmp = home.join(".gemini/tmp");
    if !gemini_tmp.exists() {
        debug!("~/.gemini/tmp not found, skipping Gemini CLI");
        return Ok(None);
    }
    Ok(Some(gemini_tmp))
}

fn collect_gemini_session_files(root: &Path) -> Vec<PathBuf> {
    let mut by_stem: std::collections::HashMap<(PathBuf, String), PathBuf> =
        std::collections::HashMap::new();
    for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.parent().is_none_or(|parent| parent.file_name().is_none_or(|name| name != "chats"))
        {
            continue;
        }
        let ext = path.extension().and_then(|ext| ext.to_str()).map(str::to_ascii_lowercase);
        if ext.as_deref() != Some("json") && ext.as_deref() != Some("jsonl") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let key = (path.parent().unwrap_or(root).to_path_buf(), stem.to_string());
        match by_stem.get(&key) {
            Some(existing)
                if existing.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
                    && ext.as_deref() == Some("json") =>
            {
                continue;
            }
            _ => {}
        }
        by_stem.insert(key, path.to_path_buf());
    }
    by_stem.into_values().collect()
}

fn parse_gemini_session_file(
    path: &Path,
    include_events: bool,
) -> anyhow::Result<Option<RawSession>> {
    let snapshot = file_scan::file_metadata_snapshot(path);
    let doc = read_gemini_document(path)?;
    let fallback_id = path.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown");
    let mut raw = parse_gemini_session_value(
        &doc,
        fallback_id,
        Some(path.display().to_string()),
        include_events,
    )?;
    if snapshot.is_none() || file_scan::file_metadata_snapshot(path) != snapshot {
        return Ok(None);
    }
    if let Some(raw) = &mut raw {
        raw.updated_at = snapshot.and_then(|snapshot| snapshot.mtime_ms());
    }
    Ok(raw)
}

fn read_gemini_document(path: &Path) -> anyhow::Result<GeminiDocument> {
    let reader = BufReader::new(fs::File::open(path)?);
    if path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
    {
        replay_gemini_jsonl(reader)
    } else {
        Ok(GeminiDocument {
            value: serde_json::from_reader(reader)?,
            message_lines: Vec::new(),
            inactive: Vec::new(),
        })
    }
}

fn replay_gemini_jsonl(reader: impl BufRead) -> anyhow::Result<GeminiDocument> {
    let mut session = Map::new();
    let mut messages: Vec<(Value, usize)> = Vec::new();
    let mut inactive = Vec::new();
    for (line_index, line) in reader.lines().enumerate() {
        let line = line?;
        let trimmed = line.trim().trim_start_matches('\u{feff}');
        if trimmed.is_empty() {
            continue;
        }
        let Ok(Value::Object(mut object)) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if let Some(rewind) = object.get("$rewindTo").and_then(Value::as_str) {
            let index = messages
                .iter()
                .position(|(message, _)| message.get("id").and_then(Value::as_str) == Some(rewind))
                .unwrap_or(0);
            inactive.extend(messages.drain(index..));
            continue;
        }
        let fields = match object.remove("$set") {
            Some(Value::Object(fields)) => fields,
            Some(_) => continue,
            None if object.get("kind").and_then(Value::as_str) == Some("main")
                || object.get("sessionId").and_then(Value::as_str).is_some() =>
            {
                object
            }
            None => {
                if object.get("type").and_then(Value::as_str).is_some() {
                    if let Some(id) = object.get("id") {
                        inactive.retain(|(message, _)| message.get("id") != Some(id));
                    }
                    upsert_gemini_message(&mut messages, Value::Object(object), line_index);
                }
                continue;
            }
        };
        for (key, value) in fields {
            if key == "messages" {
                if let Value::Array(snapshot) = value {
                    let replaced = std::mem::take(&mut messages);
                    for message in snapshot {
                        upsert_gemini_message(&mut messages, message, line_index);
                    }
                    inactive.retain(|(old, _)| {
                        old.get("id").is_none_or(|id| {
                            !messages.iter().any(|(message, _)| message.get("id") == Some(id))
                        })
                    });
                    inactive.extend(replaced.into_iter().filter(|(old, _)| {
                        old.get("id").is_none_or(|id| {
                            !messages.iter().any(|(message, _)| message.get("id") == Some(id))
                        })
                    }));
                }
            } else {
                session.insert(key, value);
            }
        }
    }
    let (values, message_lines): (Vec<_>, Vec<_>) = messages.into_iter().unzip();
    session.insert("messages".to_string(), Value::Array(values));
    Ok(GeminiDocument { value: Value::Object(session), message_lines, inactive })
}

fn upsert_gemini_message(messages: &mut Vec<(Value, usize)>, message: Value, line_index: usize) {
    if let Some(id) = message.get("id")
        && let Some(existing) =
            messages.iter_mut().find(|(existing, _)| existing.get("id") == Some(id))
    {
        *existing = (message, line_index);
    } else {
        messages.push((message, line_index));
    }
}

#[cfg(test)]
pub(crate) fn parse_gemini_session(
    json: &str,
    fallback_id: &str,
) -> anyhow::Result<Option<RawSession>> {
    let doc: Value = serde_json::from_str(json)?;
    parse_gemini_session_value(
        &GeminiDocument { value: doc, message_lines: Vec::new(), inactive: Vec::new() },
        fallback_id,
        None,
        true,
    )
}

fn parse_gemini_session_value(
    document: &GeminiDocument,
    fallback_id: &str,
    source_path: Option<String>,
    include_events: bool,
) -> anyhow::Result<Option<RawSession>> {
    let doc = &document.value;
    let session_id =
        doc.get("sessionId").and_then(|s| s.as_str()).unwrap_or(fallback_id).to_string();

    let started_at = rfc3339_ms(doc.get("startTime")).unwrap_or(0);
    let updated_at = rfc3339_ms(doc.get("lastUpdated"));

    let messages_arr = match doc.get("messages").and_then(|m| m.as_array()) {
        Some(arr) => arr,
        None => return Ok(None),
    };

    let mut messages = Vec::new();
    let mut usage_events = Vec::new();
    let mut events = Vec::new();

    for (index, msg) in messages_arr.iter().enumerate() {
        let msg_type = msg.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let role = match msg_type {
            "user" => Role::User,
            "gemini" | "model" => Role::Assistant,
            _ => continue,
        };

        let timestamp = rfc3339_ms(msg.get("timestamp"));
        if include_events && matches!(role, Role::Assistant) {
            extract_gemini_events(
                msg,
                EventContext {
                    event_seq: events.len() as u32,
                    timestamp,
                    source_path: source_path.clone(),
                    source_event_id: Some(
                        document
                            .message_lines
                            .get(index)
                            .map(|line| format!("line:{line}"))
                            .unwrap_or_else(|| format!("messages:{index}")),
                    ),
                    message_seq: messages.len().checked_sub(1).map(|seq| seq as u32),
                    parser_version: EVENT_PARSER_VERSION,
                },
                None,
                &mut events,
            );
        }

        let content = gemini_text(msg.get("content"));
        let message_seq = if content.is_empty() {
            None
        } else {
            let seq = messages.len() as u32;
            messages.push(RawMessage { role, content, timestamp });
            Some(seq)
        };

        if matches!(msg_type, "gemini" | "model")
            && let Some(event) = extract_gemini_usage_event(
                msg,
                index as u32,
                message_seq,
                timestamp.unwrap_or(started_at),
                source_path.as_deref(),
            )
        {
            usage_events.push(event);
        }
    }

    if include_events {
        for (msg, line) in &document.inactive {
            extract_gemini_events(
                msg,
                EventContext {
                    event_seq: events.len() as u32,
                    timestamp: rfc3339_ms(msg.get("timestamp")),
                    source_path: source_path.clone(),
                    source_event_id: Some(format!("line:{line}")),
                    message_seq: None,
                    parser_version: EVENT_PARSER_VERSION,
                },
                Some(EvidenceVisibility::Inactive),
                &mut events,
            );
        }
    }
    if messages.is_empty() && usage_events.is_empty() && events.is_empty() {
        return Ok(None);
    }

    let mut session =
        RawSession::search_only(session_id, None, started_at, updated_at, None, messages);
    session.source_file_path = source_path;
    session = session.with_usage(usage_events, USAGE_PARSER_VERSION);
    session.metadata_parser_version = Some(METADATA_PARSER_VERSION);
    session.refresh_session_on_metadata_backfill = true;
    if include_events {
        session = session.with_events(events, EVENT_PARSER_VERSION);
    }
    Ok(Some(session))
}

fn extract_gemini_events(
    message: &Value,
    context: EventContext,
    visibility: Option<EvidenceVisibility>,
    events: &mut Vec<RawSessionEvent>,
) {
    if !matches!(message.get("type").and_then(Value::as_str), Some("gemini" | "model")) {
        return;
    }
    let Some(calls) = message.get("toolCalls").and_then(Value::as_array) else {
        return;
    };
    for (index, call) in calls.iter().enumerate() {
        let Some(name) = call.get("name").and_then(Value::as_str) else {
            continue;
        };
        let call_id = call.get("id").and_then(Value::as_str).filter(|id| !id.is_empty());
        let context_for = |event_seq, kind| EventContext {
            event_seq,
            timestamp: rfc3339_ms(call.get("timestamp")).or(context.timestamp),
            source_path: context.source_path.clone(),
            source_event_id: context.source_event_id.as_ref().map(|origin| {
                format!("{origin}:toolCalls:{index}:{}:{kind}", call_id.unwrap_or("tool"))
            }),
            message_seq: context.message_seq,
            parser_version: EVENT_PARSER_VERSION,
        };
        let args = call.get("args");
        let mut event =
            tool_call_event(context_for(events.len() as u32, "call"), name.to_string(), args);
        event.kind = "tool_call".to_string();
        event.target = None;
        let operation = match name {
            "read_file" => Some(FileOperation::Read),
            "replace" | "write_file" => Some(FileOperation::Write),
            _ => None,
        };
        if let Some(operation) = operation
            && let Some(path) = args
                .and_then(|args| {
                    args.get("file_path").or_else(|| {
                        (name == "read_file").then(|| args.get("absolute_path")).flatten()
                    })
                })
                .and_then(Value::as_str)
                .filter(|path| !path.trim().is_empty())
        {
            event.kind = if operation == FileOperation::Read { "file_read" } else { "file_write" }
                .to_string();
            event.target = Some(path.to_string());
            event.files.push(FileEvidence {
                path: path.to_string(),
                operation,
                kind: FileEvidenceKind::Call,
                cwd: None,
                target: None,
            });
        } else if name == "run_shell_command" {
            event.kind = "command".to_string();
            event.target = args
                .and_then(|args| args.get("command"))
                .and_then(Value::as_str)
                .map(str::to_string);
            if let Some(command) = event.target.as_deref() {
                let cwd = args
                    .and_then(|args| args.get("dir_path"))
                    .and_then(Value::as_str)
                    .filter(|path| Path::new(path).is_absolute());
                let (files, status) = crate::adapters::events::shell_file_evidence(command, cwd);
                event.files = files;
                event.command_evidence_status = Some(status);
            }
        }
        event.tool_call_id = call_id.map(str::to_string);
        event.status = call
            .get("status")
            .and_then(Value::as_str)
            .filter(|status| {
                matches!(
                    *status,
                    "validating"
                        | "scheduled"
                        | "executing"
                        | "awaiting_approval"
                        | "success"
                        | "error"
                        | "cancelled"
                )
            })
            .map(str::to_string);
        event.visibility = visibility;
        event.attrs_json = Some(message.to_string());
        let status = event.status.clone();
        events.push(event);
        if call.get("result").is_some_and(|result| !result.is_null())
            || matches!(status.as_deref(), Some("success" | "error" | "cancelled"))
        {
            let mut result = tool_result_event(
                context_for(events.len() as u32, "result"),
                Some(name.to_string()),
                call.get("result").map(|result| match result {
                    Value::String(text) => text.to_string(),
                    value => value.to_string(),
                }),
            );
            result.tool_call_id = call_id.map(str::to_string);
            result.status = status;
            result.visibility = visibility;
            result.attrs_json = Some(message.to_string());
            events.push(result);
        }
    }
}

fn gemini_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.trim().to_string(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| match item {
                Value::String(text) => Some(text.as_str()),
                Value::Object(object) => object.get("text").and_then(Value::as_str),
                _ => None,
            })
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn extract_gemini_usage_event(
    msg: &Value,
    event_seq: u32,
    message_seq: Option<u32>,
    timestamp: i64,
    source_path: Option<&str>,
) -> Option<RawUsageEvent> {
    let tokens = msg.get("tokens")?;
    let output_tokens = usage_count(tokens, &["output"]);
    let cache_read_tokens = usage_count(tokens, &["cached"]);
    let reasoning_tokens = usage_count(tokens, &["thoughts"]);
    let input_tokens = usage_count(tokens, &["input"]).saturating_sub(cache_read_tokens);
    if input_tokens == 0 && output_tokens == 0 && cache_read_tokens == 0 && reasoning_tokens == 0 {
        return None;
    }

    let model = msg
        .get("model")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("unknown")
        .to_string();
    let event_key = msg
        .get("id")
        .map(|value| format!("message:{value}"))
        .unwrap_or_else(|| format!("line:{event_seq}"));

    Some(RawUsageEvent {
        message_seq,
        model: model.clone(),
        provider: "google".to_string(),
        input_tokens,
        output_tokens,
        cache_read_tokens,
        reasoning_tokens,
        source_path: source_path.map(str::to_string),
        raw_usage_json: Some(tokens.to_string()),
        ..RawUsageEvent::observed(event_key, event_seq, timestamp, USAGE_PARSER_VERSION)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_gemini_session_extracts_usage_events() {
        let json = r#"{
            "sessionId": "abc-123",
            "startTime": "2025-11-13T13:48:00.000Z",
            "messages": [
                {"id": 0, "type": "user", "content": "hello", "timestamp": "2025-11-13T13:48:05.000Z"},
                {
                    "id": 1,
                    "type": "gemini",
                    "content": "hi there",
                    "timestamp": "2025-11-13T13:48:10.000Z",
                    "model": "gemini-2.5-pro",
                    "toolCalls": [
                        {"id":"write-1","name":"write_file","args":{"file_path":"/repo/a.rs","content":"new content"},"status":"success","timestamp":"2025-11-13T13:48:11.000Z","result":[{"text":"saved"}]},
                        {"id":"read-1","name":"read_file","args":{"absolute_path":"/repo/b.rs"},"result":[{"text":"read content"}]}
                    ],
                    "tokens": { "input": 100, "output": 20, "cached": 30, "thoughts": 5 }
                }
            ]
        }"#;

        let session = parse_gemini_session(json, "fallback").unwrap().unwrap();
        assert_eq!(session.usage_events.len(), 1);
        let event = &session.usage_events[0];
        assert_eq!(event.model, "gemini-2.5-pro");
        assert_eq!(event.provider, "google");
        assert_eq!(event.input_tokens, 70);
        assert_eq!(event.output_tokens, 20);
        assert_eq!(event.cache_read_tokens, 30);
        assert_eq!(event.reasoning_tokens, 5);
        assert_eq!(event.token_source, crate::types::TokenSource::Observed);
        assert_eq!(session.events.len(), 4);
        let call = &session.events[0];
        assert_eq!(call.tool_call_id.as_deref(), Some("write-1"));
        assert_eq!(call.message_seq, Some(0));
        assert_eq!(call.files[0].path, "/repo/a.rs");
        assert_eq!(call.files[0].operation, FileOperation::Write);
        assert_eq!(call.files[0].cwd, None);
        assert_eq!(call.timestamp, rfc3339_ms(Some(&Value::from("2025-11-13T13:48:11.000Z"))));
        let result = &session.events[1];
        assert_eq!(result.tool_call_id, call.tool_call_id);
        assert_eq!(result.status.as_deref(), Some("success"));
        assert!(result.files.is_empty());
        let attrs: Value = serde_json::from_str(result.attrs_json.as_deref().unwrap()).unwrap();
        assert_eq!(attrs.pointer("/toolCalls/0/args/content"), Some(&Value::from("new content")));
        assert_eq!(attrs.pointer("/toolCalls/0/result/0/text"), Some(&Value::from("saved")));
        assert_eq!(session.events[2].files[0].operation, FileOperation::Read);
        assert_eq!(session.events[3].status, None);
    }

    #[test]
    fn parse_gemini_session_file_sets_source_path() {
        let dir = tempfile::tempdir().unwrap();
        let chats = dir.path().join("chats");
        fs::create_dir_all(&chats).unwrap();
        let path = chats.join("session-prefix.json");
        std::fs::write(
            &path,
            r#"{"sessionId":"abc-123","messages":[{"type":"user","content":"hello"}]}"#,
        )
        .unwrap();

        let session = parse_gemini_session_file(&path, true).unwrap().unwrap();

        assert_eq!(session.source_file_path.as_deref(), path.to_str());
        crate::db::schema::register_sqlite_vec();
        let store = crate::db::store::Store::open_in_memory().unwrap();
        store.conn.execute(
            "INSERT INTO sessions (id, source, source_id, title, started_at, updated_at, message_count) VALUES ('stored', 'gemini-cli', 'abc-123', 'hello', 0, ?1, 1)",
            rusqlite::params![session.updated_at],
        ).unwrap();
        store
            .persist_usage_events_for_existing_session(
                "gemini-cli",
                "abc-123",
                &[],
                USAGE_PARSER_VERSION,
                session.updated_at,
            )
            .unwrap();
        let context = || AdapterSyncContext::from_store_for_test(&store, "gemini-cli").unwrap();
        let usage_only = scan_gemini_for_sync(dir.path(), &context(), None, false).unwrap();
        assert_eq!(usage_only.stats.skipped_sessions, 1);
        for previous_version in [None, Some(EVENT_PARSER_VERSION - 1)] {
            if let Some(version) = previous_version {
                store
                    .persist_session_events_for_existing_session(
                        "gemini-cli",
                        "abc-123",
                        &[],
                        version,
                        session.updated_at,
                    )
                    .unwrap();
            }
            let backfill = scan_gemini_for_sync(dir.path(), &context(), None, true).unwrap();
            assert_eq!(backfill.sessions.len(), 1);
            assert_eq!(backfill.sessions[0].source_id, "abc-123");
            assert_eq!(backfill.sessions[0].event_parser_version, Some(EVENT_PARSER_VERSION));
        }
        store
            .persist_session_events_for_existing_session(
                "gemini-cli",
                "abc-123",
                &[],
                EVENT_PARSER_VERSION,
                session.updated_at,
            )
            .unwrap();
        let refreshed = scan_gemini_for_sync(dir.path(), &context(), None, true).unwrap();
        assert_eq!(refreshed.sessions.len(), 1);
        assert!(refreshed.sessions[0].refresh_session_on_metadata_backfill);
        store
            .persist_topology_for_existing_session(
                "gemini-cli",
                "abc-123",
                &crate::db::store::SessionTopologyWrite {
                    thread_role: None,
                    parents: &[],
                    parser_version: Some(METADATA_PARSER_VERSION),
                },
            )
            .unwrap();
        let skipped = scan_gemini_for_sync(dir.path(), &context(), None, true).unwrap();
        assert_eq!(skipped.stats.skipped_sessions, 1);
    }

    #[test]
    fn parse_gemini_session_accepts_model_type_and_array_content() {
        let json = r#"{
            "sessionId": "model-1",
            "messages": [
                {"type":"user","content":[{"text":"hello"}]},
                {"type":"model","content":[{"text":"hi"}],"tokens":{"input":10,"output":2}}
            ]
        }"#;
        let session = parse_gemini_session(json, "fallback").unwrap().unwrap();
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].content, "hello");
        assert_eq!(session.messages[1].role, Role::Assistant);
        assert_eq!(session.messages[1].content, "hi");
        assert_eq!(session.usage_events.len(), 1);
        assert_eq!(session.usage_parser_version, Some(USAGE_PARSER_VERSION));
    }

    #[test]
    fn parse_gemini_jsonl_replays_set_patches_and_bare_messages() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-abc.jsonl");
        let records = [
            serde_json::json!({"kind":"main","sessionId":"abc"}),
            serde_json::json!({"$set":{"startTime":"2025-11-13T13:48:00.000Z"}}),
            serde_json::json!({"id":"user","type":"user","content":"hello"}),
            serde_json::json!({"id":"work","type":"gemini","content":"", "toolCalls":[{"id":"write-1","name":"write_file","args":{"file_path":"/repo/a.rs","content":"new"},"status":"executing"}]}),
            serde_json::json!({"id":"work","type":"gemini","content":"", "toolCalls":[{"id":"write-1","name":"write_file","args":{"file_path":"/repo/a.rs","content":"new"},"status":"success","result":[{"text":"saved"}]}]}),
            serde_json::json!({"id":"removed","type":"gemini","content":"", "toolCalls":[{"id":"write-2","name":"replace","args":{"file_path":"/repo/b.rs","old_string":"old","new_string":"new"},"status":"error","result":[{"text":"failed"}]}]}),
            serde_json::json!({"$rewindTo":"removed"}),
            serde_json::json!({"id":"current","type":"gemini","content":"there"}),
        ];
        fs::write(&path, records.iter().map(Value::to_string).collect::<Vec<_>>().join("\n"))
            .unwrap();
        let session = parse_gemini_session_file(&path, true).unwrap().unwrap();
        assert_eq!(session.source_id, "abc");
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].content, "hello");
        assert_eq!(session.messages[1].content, "there");
        assert_eq!(session.events.len(), 4);
        let active = &session.events[0];
        assert_eq!(active.source_event_id.as_deref(), Some("line:4:toolCalls:0:write-1:call"));
        assert_eq!(active.status.as_deref(), Some("success"));
        assert_eq!(active.message_seq, Some(0));
        assert_eq!(active.visibility, None);
        let inactive = &session.events[2];
        assert_eq!(inactive.source_event_id.as_deref(), Some("line:5:toolCalls:0:write-2:call"));
        assert_eq!(inactive.visibility, Some(EvidenceVisibility::Inactive));
        assert_eq!(inactive.message_seq, None);
        assert_eq!(inactive.files[0].path, "/repo/b.rs");
        assert_eq!(session.events[3].status.as_deref(), Some("error"));
        assert_eq!(
            session.started_at,
            rfc3339_ms(Some(&Value::from("2025-11-13T13:48:00.000Z"))).unwrap()
        );
        let mut wire = fs::read_to_string(&path).unwrap();
        wire.push_str("\n{\"$rewindTo\":\"unknown\"}\n");
        fs::write(&path, wire).unwrap();
        let rewound = parse_gemini_session_file(&path, true).unwrap().unwrap();
        assert!(rewound.messages.is_empty());
        assert_eq!(rewound.events.len(), 4);
        let mut invalid_utf8 = fs::read(&path).unwrap();
        invalid_utf8.extend_from_slice(b"\n\xff");
        fs::write(&path, invalid_utf8).unwrap();
        assert!(parse_gemini_session_file(&path, true).is_err());
        assert!(
            rewound
                .events
                .iter()
                .all(|event| event.visibility == Some(EvidenceVisibility::Inactive))
        );
    }

    #[test]
    fn collect_gemini_session_files_keeps_same_stem_in_different_projects() {
        let dir = tempfile::tempdir().unwrap();
        for project in ["proj-a", "proj-b"] {
            let chats = dir.path().join(project).join("chats");
            std::fs::create_dir_all(&chats).unwrap();
            std::fs::write(chats.join("wip.json"), "{}").unwrap();
        }
        let files = collect_gemini_session_files(dir.path());
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn collect_gemini_session_files_prefers_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let chats = dir.path().join("proj").join("chats");
        std::fs::create_dir_all(&chats).unwrap();
        std::fs::write(chats.join("session-1.json"), "{}").unwrap();
        std::fs::write(chats.join("session-1.jsonl"), "{}\n").unwrap();
        let files = collect_gemini_session_files(dir.path());
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].extension().and_then(|ext| ext.to_str()), Some("jsonl"));
    }
}
