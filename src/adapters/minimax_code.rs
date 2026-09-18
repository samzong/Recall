use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::Context;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use tracing::{debug, warn};
use walkdir::WalkDir;

use crate::adapters::AdapterSyncContext;
use crate::adapters::events::{
    EventContext, shell_file_evidence, tool_call_event, tool_result_event,
};
use crate::adapters::file_scan::{self, FileScanEntry};
use crate::adapters::json_util::{json_i64, jsonl_indexed};
use crate::adapters::paths;
use crate::adapters::usage::usage_count;
use crate::adapters::{
    RawMessage, RawSession, ResumeCommand, SourceAdapter, SourceObservation, SyncScanResult,
    SyncScanStats, first_timestamp, last_timestamp,
};
use crate::types::{FileEvidence, FileOperation, RawSessionEvent, RawUsageEvent, Role};

pub(crate) struct MinimaxCodeAdapter;

const USAGE_PARSER_VERSION: u32 = 1;
const EVENT_PARSER_VERSION: u32 = 1;
const METADATA_PARSER_VERSION: u32 = 1;
const RUNTIME_DB: &str = "runtime-state.sqlite";

fn minimax_scan_options(include_events: bool) -> file_scan::FileScanOptions {
    file_scan::FileScanOptions {
        usage_parser_version: Some(USAGE_PARSER_VERSION),
        event_parser_version: include_events.then_some(EVENT_PARSER_VERSION),
        metadata_parser_version: Some(METADATA_PARSER_VERSION),
    }
}

impl SourceAdapter for MinimaxCodeAdapter {
    fn id(&self) -> &str {
        "minimax-code"
    }

    fn label(&self) -> &str {
        "MX"
    }

    fn usage_parser_version(&self) -> Option<u32> {
        Some(USAGE_PARSER_VERSION)
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        Some(ResumeCommand::new("mcode", &["--session", source_id]))
    }

    fn start_command(&self, prompt: String) -> Option<ResumeCommand> {
        Some(crate::adapters::prompt_start("mcode", prompt))
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let Some(entries) = minimax_session_entries() else {
            return Ok(vec![]);
        };
        Ok(parse_stable_sessions(entries, true)?.sessions)
    }

    fn scan_for_sync(
        &self,
        context: &AdapterSyncContext,
        since_ts: Option<i64>,
        include_events: bool,
    ) -> anyhow::Result<Option<SyncScanResult>> {
        let Some(entries) = minimax_session_entries() else {
            return Ok(Some(SyncScanResult::default()));
        };
        let (moved, unchanged) = split_moved_sessions(context, entries);
        let mut result = rescan_moved_sessions(moved, include_events)?;
        result.absorb(file_scan::run_file_scan_with_options_and_snapshot(
            context,
            since_ts,
            minimax_scan_options(include_events),
            unchanged,
            minimax_session_snapshot,
            |entry, mtime_ms| parse_minimax_session(entry, mtime_ms, include_events),
        )?);
        Ok(Some(result))
    }
}

fn minimax_session_entries() -> Option<Vec<FileScanEntry>> {
    let data_dir = resolve_minimax_data_dir()?;
    let sessions_dir = paths::existing_dir(data_dir.join("v2").join("sessions"))?;
    let workspaces = read_workspace_dirs(&data_dir.join("v2").join("sqlite").join(RUNTIME_DB));
    Some(collect_session_entries(&sessions_dir, &workspaces))
}

fn resolve_minimax_data_dir() -> Option<PathBuf> {
    for name in ["MINIMAX_DATA_DIR", "MAVIS_DATA_DIR"] {
        if let Some(dir) = paths::env_path_dir(name) {
            let existing = paths::existing_dir(dir);
            if existing.is_none() {
                debug!("{name} not found, skipping MiniMax Code");
            }
            return existing;
        }
    }
    let home = dirs::home_dir()?;
    [".minimax", ".mavis"].into_iter().find_map(|relative| paths::existing_dir(home.join(relative)))
}

fn split_moved_sessions(
    context: &AdapterSyncContext,
    entries: Vec<FileScanEntry>,
) -> (Vec<FileScanEntry>, Vec<FileScanEntry>) {
    let indexed: HashMap<&str, Option<&str>> = context
        .session_paths()
        .map(|path| (path.source_id.as_str(), path.directory.as_deref()))
        .collect();
    entries.into_iter().partition(|entry| {
        let Some(directory) = entry.directory.as_deref() else {
            return false;
        };
        indexed.get(entry.session_id.as_str()).is_some_and(|indexed| *indexed != Some(directory))
    })
}

fn rescan_moved_sessions(
    entries: Vec<FileScanEntry>,
    include_events: bool,
) -> anyhow::Result<SyncScanResult> {
    let candidates = entries.len() as u32;
    let parsed = parse_stable_sessions(entries, include_events)?;
    let observations = parsed
        .sessions
        .iter()
        .map(|session| SourceObservation {
            source_id: session.source_id.clone(),
            source_file_path: session.source_file_path.clone(),
        })
        .collect();
    Ok(SyncScanResult {
        stats: SyncScanStats {
            candidates,
            parsed: candidates - parsed.missing,
            rejected_before_parse: parsed.missing,
            unstable_sessions: parsed.unstable,
            ..SyncScanStats::default()
        },
        observations,
        sessions: parsed.sessions,
    })
}

struct ParsedSessions {
    sessions: Vec<RawSession>,
    unstable: u32,
    missing: u32,
}

fn parse_stable_sessions(
    entries: Vec<FileScanEntry>,
    include_events: bool,
) -> anyhow::Result<ParsedSessions> {
    let mut sessions = Vec::new();
    let mut unstable = 0;
    let mut missing = 0;
    for entry in entries {
        let Some(snapshot) = minimax_session_snapshot(&entry) else {
            missing += 1;
            continue;
        };
        let raw =
            parse_minimax_session(entry.clone(), snapshot.effective_mtime_ms(), include_events)?;
        if minimax_session_snapshot(&entry).as_ref() != Some(&snapshot) {
            warn!(
                "skipping unstable MiniMax Code session {}: source files changed while parsing ({})",
                entry.session_id,
                entry.stat_target.display()
            );
            unstable += 1;
            continue;
        }
        sessions.extend(raw);
    }
    Ok(ParsedSessions { sessions, unstable, missing })
}

fn read_workspace_dirs(db_path: &Path) -> HashMap<String, String> {
    if !db_path.is_file() {
        return HashMap::new();
    }
    match query_workspace_dirs(db_path) {
        Ok(workspaces) => workspaces,
        Err(error) => {
            warn!("failed to read MiniMax Code workspaces from {}: {error}", db_path.display());
            HashMap::new()
        }
    }
}

fn query_workspace_dirs(db_path: &Path) -> anyhow::Result<HashMap<String, String>> {
    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let mut statement = conn
        .prepare("SELECT session_id, workspace_dir, record_json FROM local_runtime_sessions")
        .or_else(|_| {
            conn.prepare("SELECT session_id, NULL, record_json FROM local_runtime_sessions")
        })?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?, row.get::<_, String>(2)?))
    })?;
    let mut workspaces = HashMap::new();
    for row in rows {
        let (session_id, workspace_dir, record_json) = row?;
        let directory =
            workspace_dir.filter(|directory| !directory.trim().is_empty()).or_else(|| {
                serde_json::from_str::<Value>(&record_json)
                    .ok()
                    .as_ref()
                    .and_then(|record| record.get("workspaceDir"))
                    .and_then(Value::as_str)
                    .filter(|directory| !directory.trim().is_empty())
                    .map(str::to_string)
            });
        if let Some(directory) = directory {
            workspaces.insert(session_id, directory.trim().to_string());
        }
    }
    Ok(workspaces)
}

fn collect_session_entries(
    sessions_dir: &Path,
    workspaces: &HashMap<String, String>,
) -> Vec<FileScanEntry> {
    let mut entries = Vec::new();
    for dir_entry in WalkDir::new(sessions_dir).max_depth(5).into_iter().filter_map(|e| e.ok()) {
        let path = dir_entry.path();
        if !path.is_file() || path.file_name().and_then(|n| n.to_str()) != Some("messages.jsonl") {
            continue;
        }
        let Some(session_dir) = path.parent() else {
            continue;
        };
        if !session_dir
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.contains("-session_"))
        {
            continue;
        }
        let session_id = match read_manifest(session_dir) {
            Ok(manifest) => manifest.session_id,
            Err(error) => {
                warn!("{error}");
                continue;
            }
        };
        entries.push(FileScanEntry {
            directory: workspaces.get(&session_id).cloned(),
            session_id,
            stat_target: path.to_path_buf(),
        });
    }
    entries
}

#[derive(Debug, PartialEq, Eq)]
struct MinimaxSessionSnapshot {
    manifest: file_scan::FileMetadataSnapshot,
    messages: file_scan::FileMetadataSnapshot,
}

fn minimax_session_snapshot(
    entry: &FileScanEntry,
) -> Option<file_scan::FileScanSnapshot<MinimaxSessionSnapshot>> {
    let session_dir = entry.stat_target.parent()?;
    let manifest = file_scan::file_metadata_snapshot(&session_dir.join("manifest.json"))?;
    let messages = file_scan::file_metadata_snapshot(&entry.stat_target)?;
    let effective_mtime_ms = manifest.mtime_ms()?.max(messages.mtime_ms()?);
    Some(file_scan::FileScanSnapshot::new(
        effective_mtime_ms,
        MinimaxSessionSnapshot { manifest, messages },
    ))
}

struct MinimaxManifest {
    session_id: String,
    created_at_ms: Option<i64>,
}

fn read_manifest(session_dir: &Path) -> anyhow::Result<MinimaxManifest> {
    let path = session_dir.join("manifest.json");
    let content =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let manifest: Value = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    let session_id = manifest
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .with_context(|| format!("{} has no sessionId", path.display()))?;
    Ok(MinimaxManifest { session_id, created_at_ms: json_i64(manifest.get("createdAtMs")) })
}

fn parse_minimax_session(
    entry: FileScanEntry,
    mtime_ms: i64,
    include_events: bool,
) -> anyhow::Result<Option<RawSession>> {
    match parse_minimax_session_impl(&entry, mtime_ms, include_events) {
        Ok(raw) => Ok(raw),
        Err(error) => {
            warn!("failed to parse {}: {error}", entry.stat_target.display());
            Ok(None)
        }
    }
}

fn parse_minimax_session_impl(
    entry: &FileScanEntry,
    mtime_ms: i64,
    include_events: bool,
) -> anyhow::Result<Option<RawSession>> {
    let session_dir = entry
        .stat_target
        .parent()
        .with_context(|| format!("{} has no session directory", entry.stat_target.display()))?;
    let manifest = read_manifest(session_dir)?;
    let file = fs::File::open(&entry.stat_target)
        .with_context(|| format!("failed to read {}", entry.stat_target.display()))?;
    parse_minimax_messages(
        manifest,
        entry.directory.clone(),
        BufReader::new(file).lines(),
        mtime_ms,
        entry.stat_target.to_str().map(str::to_string),
        include_events,
    )
}

fn parse_minimax_messages(
    manifest: MinimaxManifest,
    directory: Option<String>,
    lines: impl Iterator<Item = std::io::Result<String>>,
    mtime_ms: i64,
    source_path: Option<String>,
    include_events: bool,
) -> anyhow::Result<Option<RawSession>> {
    let mut messages: Vec<RawMessage> = Vec::new();
    let mut events: Vec<RawSessionEvent> = Vec::new();
    let mut usage_events: Vec<RawUsageEvent> = Vec::new();

    for item in jsonl_indexed(lines) {
        let (line_index, record) = item?;
        let Some(message) = record.get("message") else {
            continue;
        };
        let timestamp = json_i64(message.get("timestamp"));

        match message.get("role").and_then(Value::as_str).unwrap_or_default() {
            role @ ("user" | "assistant") => {
                let content = message.get("content");
                let text = if role == "user" {
                    user_text(message, content)
                } else {
                    visible_text(content)
                };
                if !text.is_empty() {
                    let role = if role == "user" { Role::User } else { Role::Assistant };
                    messages.push(RawMessage { role, content: text, timestamp });
                }
                if role != "assistant" {
                    continue;
                }
                let message_seq = messages.len().checked_sub(1).map(|seq| seq as u32);
                if let Some(event) = usage_event(
                    message,
                    line_index,
                    usage_events.len() as u32,
                    message_seq,
                    timestamp.unwrap_or(mtime_ms),
                    source_path.as_deref(),
                ) {
                    usage_events.push(event);
                }
                if !include_events {
                    continue;
                }
                let parts = content.and_then(Value::as_array).into_iter().flatten();
                for (part_index, part) in parts.enumerate() {
                    if part.get("type").and_then(Value::as_str) != Some("toolCall") {
                        continue;
                    }
                    if let Some(event) = tool_call_from_part(
                        part,
                        EventContext {
                            event_seq: events.len() as u32,
                            timestamp,
                            source_path: source_path.clone(),
                            source_event_id: Some(format!("minimax:{line_index}:{part_index}")),
                            message_seq,
                            parser_version: EVENT_PARSER_VERSION,
                        },
                        directory.as_deref(),
                    ) {
                        events.push(event);
                    }
                }
            }
            "toolResult" if include_events => {
                events.push(tool_result_from_message(
                    message,
                    EventContext {
                        event_seq: events.len() as u32,
                        timestamp,
                        source_path: source_path.clone(),
                        source_event_id: Some(format!("minimax:{line_index}")),
                        message_seq: messages.len().checked_sub(1).map(|seq| seq as u32),
                        parser_version: EVENT_PARSER_VERSION,
                    },
                ));
            }
            _ => {}
        }
    }

    if messages.is_empty() && usage_events.is_empty() && events.is_empty() {
        return Ok(None);
    }

    let first = first_timestamp(None, &messages, &usage_events, &events);
    let started_at = manifest.created_at_ms.or(first).unwrap_or(0);
    let duration_minutes = match (first, last_timestamp(None, &messages, &usage_events, &events)) {
        (Some(first), Some(last)) if last >= first => Some(((last - first) / 60_000) as u32),
        _ => None,
    };

    let mut session = RawSession::search_only(
        manifest.session_id,
        directory,
        started_at,
        Some(mtime_ms),
        None,
        messages,
    );
    session.source_file_path = source_path;
    session.duration_minutes = duration_minutes;
    session.metadata_parser_version = Some(METADATA_PARSER_VERSION);
    session = session.with_usage(usage_events, USAGE_PARSER_VERSION);
    if include_events {
        session = session.with_events(events, EVENT_PARSER_VERSION);
    }
    Ok(Some(session))
}

fn tool_call_from_part(
    part: &Value,
    context: EventContext,
    cwd: Option<&str>,
) -> Option<RawSessionEvent> {
    let name = part.get("name").and_then(Value::as_str).filter(|name| !name.trim().is_empty())?;
    let arguments = part.get("arguments");
    let mut event = tool_call_event(context, name.to_string(), arguments);
    event.tool_call_id =
        part.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()).map(str::to_string);

    match (event.kind.as_str(), event.target.clone()) {
        ("command", Some(command)) => {
            let shell_cwd = arguments
                .and_then(|arguments| arguments.get("cwd"))
                .and_then(Value::as_str)
                .filter(|cwd| Path::new(cwd).is_absolute())
                .or(cwd);
            let (files, status) = shell_file_evidence(&command, shell_cwd);
            event.files = files;
            event.command_evidence_status = Some(status);
        }
        ("file_read", Some(path)) => {
            event.files.push(FileEvidence::call(
                path,
                FileOperation::Read,
                cwd.map(str::to_string),
            ));
        }
        ("file_write", Some(path)) => {
            event.files.push(FileEvidence::call(
                path,
                FileOperation::Write,
                cwd.map(str::to_string),
            ));
        }
        _ => {}
    }
    Some(event)
}

fn tool_result_from_message(message: &Value, context: EventContext) -> RawSessionEvent {
    let name = message
        .get("toolName")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .map(str::to_string);
    let output = message.get("content").map(|content| match content {
        Value::String(text) => text.to_string(),
        Value::Array(_) => visible_text(Some(content)),
        other => other.to_string(),
    });
    let mut event = tool_result_event(context, name, output);
    event.tool_call_id = message
        .get("toolCallId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string);
    event.status = message
        .get("isError")
        .and_then(Value::as_bool)
        .map(|is_error| if is_error { "error" } else { "success" }.to_string());
    event
}

fn usage_event(
    message: &Value,
    line_index: usize,
    event_seq: u32,
    message_seq: Option<u32>,
    timestamp: i64,
    source_path: Option<&str>,
) -> Option<RawUsageEvent> {
    let usage = message.get("usage")?;
    let input_tokens = usage_count(usage, &["input"]);
    let output_tokens = usage_count(usage, &["output"]);
    let cache_read_tokens = usage_count(usage, &["cacheRead"]);
    let cache_write_tokens = usage_count(usage, &["cacheWrite"]);
    if input_tokens == 0 && output_tokens == 0 && cache_read_tokens == 0 && cache_write_tokens == 0
    {
        return None;
    }
    let event_key = message
        .get("responseId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("minimax:{line_index}"));
    Some(RawUsageEvent {
        message_seq,
        model: message
            .get("model")
            .and_then(Value::as_str)
            .filter(|model| !model.trim().is_empty())
            .unwrap_or("unknown")
            .to_string(),
        provider: message
            .get("provider")
            .and_then(Value::as_str)
            .filter(|provider| !provider.trim().is_empty())
            .unwrap_or("minimax")
            .to_string(),
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_write_tokens,
        source_path: source_path.map(str::to_string),
        raw_usage_json: Some(usage.to_string()),
        ..RawUsageEvent::observed(event_key, event_seq, timestamp, USAGE_PARSER_VERSION)
    })
}

fn user_text(message: &Value, content: Option<&Value>) -> String {
    let text = joined_text(content);
    let range = message.get("canonicalTextRange");
    let start = json_i64(range.and_then(|range| range.get("startOffset")));
    let end = json_i64(range.and_then(|range| range.get("endOffset")));
    let canonical = start.zip(end).filter(|(start, end)| (0..=*end).contains(start)).and_then(
        |(start, end)| Some(utf16_byte_offset(&text, start)?..utf16_byte_offset(&text, end)?),
    );
    match canonical {
        Some(range) => text[range].trim().to_string(),
        None => strip_harness_reminders(&text),
    }
}

fn utf16_byte_offset(text: &str, offset: i64) -> Option<usize> {
    let mut units = 0;
    for (byte_offset, character) in text.char_indices() {
        if units == offset {
            return Some(byte_offset);
        }
        units += character.len_utf16() as i64;
    }
    (units == offset).then_some(text.len())
}

fn visible_text(content: Option<&Value>) -> String {
    strip_harness_reminders(&joined_text(content))
}

fn joined_text(content: Option<&Value>) -> String {
    let Some(parts) = content.and_then(Value::as_array) else {
        return String::new();
    };
    parts
        .iter()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

fn strip_harness_reminders(text: &str) -> String {
    let mut remaining = text;
    let mut kept = String::with_capacity(text.len());
    while let Some(start) = remaining.find("<system-reminder>") {
        kept.push_str(&remaining[..start]);
        let tail = &remaining[start..];
        remaining = match tail.find("</system-reminder>") {
            Some(end) => &tail[end + "</system-reminder>".len()..],
            None => "",
        };
    }
    kept.push_str(remaining);
    kept.trim().to_string()
}

#[cfg(test)]
pub(crate) fn parse_minimax_transcript(
    session_id: &str,
    messages_jsonl: &str,
    directory: Option<String>,
) -> Option<RawSession> {
    let manifest = MinimaxManifest { session_id: session_id.to_string(), created_at_ms: None };
    let lines = messages_jsonl.lines().map(|line| Ok(line.to_string()));
    parse_minimax_messages(manifest, directory, lines, 0, None, true).unwrap()
}

#[cfg(test)]
pub(crate) fn parse_conformance_fixture(data_dir: &Path) -> anyhow::Result<Option<RawSession>> {
    let sessions_dir = data_dir.join("v2").join("sessions");
    let workspaces = read_workspace_dirs(&data_dir.join("v2").join("sqlite").join(RUNTIME_DB));
    let Some(entry) = collect_session_entries(&sessions_dir, &workspaces).into_iter().next() else {
        return Ok(None);
    };
    let Some(snapshot) = minimax_session_snapshot(&entry) else {
        return Ok(None);
    };
    parse_minimax_session_impl(&entry, snapshot.effective_mtime_ms(), true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::CommandEvidenceStatus;

    pub(crate) const TRANSCRIPT: &str = concat!(
        r#"{"message_id": "msg-user", "turn_id": "turn-1", "message": {"role": "user", "content": [{"type": "text", "text": "<system-reminder>\n<agent-context>\n  agent: Mavis\n</agent-context>\n</system-reminder>\n\nshare the current project"}], "timestamp": 1789751473094, "canonicalTextRange": {"startOffset": 86, "endOffset": 111}}}"#,
        "\n",
        r#"{"message_id": "msg-notice", "turn_id": "turn-1", "message": {"role": "user", "content": [{"type": "text", "text": "<background-task-finished>\nLocal background tasks have finished in this session.\n</background-task-finished>"}], "timestamp": 1789751473500, "canonicalTextRange": {"startOffset": 108, "endOffset": 108}}}"#,
        "\n",
        r#"{"message_id":"msg-assistant","turn_id":"turn-1","message":{"role":"assistant","content":[{"type":"thinking","thinking":"internal"},{"type":"text","text":"Reading the adapter."},{"type":"toolCall","id":"call-1","name":"read","arguments":{"path":"src/adapters/mod.rs"}},{"type":"toolCall","id":"call-2","name":"edit","arguments":{"file_path":"src/adapters/mod.rs","old_string":"a","new_string":"b"}},{"type":"toolCall","id":"call-3","name":"bash","arguments":{"command":"mv src/adapters/mod.rs src/adapters/registry.rs"}}],"api":"anthropic-messages","provider":"minimax","model":"MiniMax-M3","usage":{"input":15997,"output":375,"cacheRead":128,"cacheWrite":0,"totalTokens":16500},"stopReason":"tool_use","timestamp":1789751476000,"responseId":"resp-1"}}"#,
        "\n",
        r#"{"message_id":"msg-toolresult","turn_id":"turn-1","message":{"role":"toolResult","toolCallId":"call-1","toolName":"read","content":[{"type":"text","text":"mod.rs contents"}],"details":{"status":"completed"},"isError":false,"timestamp":1789751476614}}"#,
        "\n",
        r#"{"message_id":"msg-custom","turn_id":"turn-1","message":{"role":"custom","customType":"todo_cadence_reminder","content":"<system-reminder>todo</system-reminder>","timestamp":1789751477000}}"#,
        "\n",
    );

    fn parsed() -> RawSession {
        parse_minimax_transcript("mvs_test", TRANSCRIPT, Some("/repo".to_string()))
            .expect("session parsed")
    }

    #[test]
    fn keeps_canonical_user_text_and_drops_harness_notices() {
        let session = parsed();
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, Role::User);
        assert_eq!(session.messages[0].content, "share the current project");
        assert_eq!(session.messages[1].role, Role::Assistant);
        assert_eq!(session.messages[1].content, "Reading the adapter.");
    }

    #[test]
    fn slices_canonical_user_text_by_utf16_offsets() {
        let transcript = concat!(
            r#"{"message":{"role":"user","content":[{"type":"text","text":"\ud83d\ude42query"}],"timestamp":1,"canonicalTextRange":{"startOffset":2,"endOffset":7}}}"#,
            "\n",
        );
        let session = parse_minimax_transcript("mvs_utf16", transcript, None).expect("parsed");
        assert_eq!(session.messages[0].content, "query");
    }

    #[test]
    fn reads_usage_from_the_assistant_message() {
        let session = parsed();
        assert_eq!(session.usage_events.len(), 1);
        let usage = &session.usage_events[0];
        assert_eq!(usage.event_key, "resp-1");
        assert_eq!(usage.model, "MiniMax-M3");
        assert_eq!(usage.provider, "minimax");
        assert_eq!(usage.input_tokens, 15997);
        assert_eq!(usage.output_tokens, 375);
        assert_eq!(usage.cache_read_tokens, 128);
        assert_eq!(usage.cache_write_tokens, 0);
        assert_eq!(usage.message_seq, Some(1));
    }

    #[test]
    fn maps_lowercase_tools_to_file_and_command_evidence() {
        let session = parsed();
        let kinds: Vec<_> = session.events.iter().map(|event| event.kind.as_str()).collect();
        assert_eq!(kinds, vec!["file_read", "file_write", "command", "tool_result"]);

        let read = &session.events[0];
        assert_eq!(read.target.as_deref(), Some("src/adapters/mod.rs"));
        assert_eq!(read.tool_call_id.as_deref(), Some("call-1"));
        assert_eq!(read.files.len(), 1);
        assert_eq!(read.files[0].operation, FileOperation::Read);

        let edit = &session.events[1];
        assert_eq!(edit.target.as_deref(), Some("src/adapters/mod.rs"));
        assert_eq!(edit.files.len(), 1);
        assert_eq!(edit.files[0].operation, FileOperation::Write);

        let command = &session.events[2];
        assert_eq!(
            command.target.as_deref(),
            Some("mv src/adapters/mod.rs src/adapters/registry.rs")
        );
        assert_eq!(command.command_evidence_status, Some(CommandEvidenceStatus::Complete));
        assert_eq!(
            command
                .files
                .iter()
                .map(|file| (file.path.as_str(), file.operation.clone(), file.cwd.as_deref()))
                .collect::<Vec<_>>(),
            [
                ("src/adapters/mod.rs", FileOperation::MoveFrom, Some("/repo")),
                ("src/adapters/registry.rs", FileOperation::MoveTo, Some("/repo")),
            ]
        );
    }

    #[test]
    fn keeps_top_level_tool_results() {
        let session = parsed();
        let result = session.events.last().unwrap();
        assert_eq!(result.kind, "tool_result");
        assert_eq!(result.name.as_deref(), Some("read"));
        assert_eq!(result.tool_call_id.as_deref(), Some("call-1"));
        assert_eq!(result.status.as_deref(), Some("success"));
        assert_eq!(result.summary.as_deref(), Some("mod.rs contents"));
        assert_eq!(result.message_seq, Some(1));
    }

    #[test]
    fn drops_sessions_without_any_signal() {
        assert!(parse_minimax_transcript("mvs_empty", "", None).is_none());
    }

    #[test]
    fn reads_workspace_dir_from_the_runtime_database() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join(RUNTIME_DB);
        let conn = Connection::open(&db_path).unwrap();
        conn.execute(
            "CREATE TABLE local_runtime_sessions (session_id TEXT PRIMARY KEY, record_json TEXT NOT NULL, updated_at_ms INTEGER NOT NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO local_runtime_sessions VALUES (?1, ?2, 0)",
            rusqlite::params![
                "mvs_test",
                r#"{"sessionId":"mvs_test","workspaceDir":"/Users/x/git/Recall"}"#
            ],
        )
        .unwrap();
        drop(conn);

        let workspaces = read_workspace_dirs(&db_path);
        assert_eq!(workspaces.get("mvs_test").map(String::as_str), Some("/Users/x/git/Recall"));
        assert!(read_workspace_dirs(&dir.path().join("missing.sqlite")).is_empty());
    }

    #[test]
    fn workspace_change_bypasses_the_incremental_skip() {
        use crate::db::store::SessionPath;
        use std::collections::HashSet;

        let indexed = |source_id: &str, directory: Option<&str>| SessionPath {
            source_id: source_id.to_string(),
            directory: directory.map(str::to_string),
            source_file_path: None,
            repo_remote: None,
            repo_slug: None,
            repo_name: None,
        };
        let session_paths = ["moved", "same", "cleared"]
            .into_iter()
            .map(|source_id| {
                let directory = match source_id {
                    "moved" => Some("/old"),
                    "same" => Some("/repo"),
                    _ => None,
                };
                (source_id.to_string(), indexed(source_id, directory))
            })
            .collect();
        let context = AdapterSyncContext::new(
            "minimax-code".to_string(),
            HashMap::new(),
            session_paths,
            HashSet::new(),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
        );
        let entry = |session_id: &str, directory: Option<&str>| FileScanEntry {
            session_id: session_id.to_string(),
            stat_target: PathBuf::from("/data/messages.jsonl"),
            directory: directory.map(str::to_string),
        };
        let entries = vec![
            entry("moved", Some("/repo")),
            entry("same", Some("/repo")),
            entry("cleared", Some("/repo")),
            entry("unknown", Some("/repo")),
            entry("same", None),
        ];

        let (moved, unchanged) = split_moved_sessions(&context, entries);
        assert_eq!(
            moved.iter().map(|entry| entry.session_id.as_str()).collect::<Vec<_>>(),
            ["moved", "cleared"]
        );
        assert_eq!(
            unchanged.iter().map(|entry| entry.session_id.as_str()).collect::<Vec<_>>(),
            ["same", "unknown", "same"]
        );
    }

    #[test]
    fn collects_sessions_from_the_dated_layout() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir =
            dir.path().join("v2/sessions/2026/09/18/17-11-12-765-session_bXZzXzUwMTc5NDVk");
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(
            session_dir.join("manifest.json"),
            r#"{"schemaVersion":1,"sessionId":"mvs_test","createdAtMs":1789751472765}"#,
        )
        .unwrap();
        fs::write(session_dir.join("messages.jsonl"), TRANSCRIPT).unwrap();

        let session = parse_conformance_fixture(dir.path()).unwrap().expect("session parsed");
        assert_eq!(session.source_id, "mvs_test");
        assert_eq!(session.started_at, 1789751472765);
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.events.len(), 4);
        assert_eq!(session.usage_events.len(), 1);
        assert_eq!(session.directory, None);
    }
}
