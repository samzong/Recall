use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;
use tracing::debug;

use rusqlite::params;

use crate::adapters::AdapterSyncContext;
use crate::adapters::events;
use crate::adapters::file_scan::{self, FileScanEntry, FileScanOptions};
use crate::adapters::json_util::{jsonl_indexed, rfc3339_ms};
use crate::adapters::opencode;
use crate::adapters::paths::resolve_home_dir;
use crate::adapters::{
    RawMessage, RawSession, ResumeCommand, SourceAdapter, SyncScanResult, SyncScanStats,
    first_timestamp, last_timestamp,
};
use crate::types::{
    CommandEvidenceStatus, FileEvidence, FileEvidenceKind, FileOperation, RawSessionEvent,
    RawUsageEvent, Role,
};

pub(crate) struct CopilotAdapter;

const METADATA_PARSER_VERSION: u32 = 1;
const EVENT_PARSER_VERSION: u32 = 3;
const USAGE_PARSER_VERSION: u32 = 2;

impl SourceAdapter for CopilotAdapter {
    fn id(&self) -> &str {
        "copilot-cli"
    }
    fn label(&self) -> &str {
        "CPL"
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        Some(ResumeCommand {
            program: "copilot".to_string(),
            args: vec![format!("--resume={source_id}")],
        })
    }

    fn start_command(&self, prompt: String) -> Option<ResumeCommand> {
        Some(crate::adapters::prompt_start("copilot", prompt))
    }

    fn app_command(&self, source_id: &str) -> Option<ResumeCommand> {
        Some(open_url_command(copilot_session_url(source_id)))
    }

    fn usage_parser_version(&self) -> Option<u32> {
        Some(USAGE_PARSER_VERSION)
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let Some(sessions_dir) = resolve_copilot_dir()? else {
            return Ok(vec![]);
        };

        let usage = load_copilot_usage_index(resolve_session_store_db().as_deref());
        let mut sessions = Vec::new();
        for entry in collect_copilot_entries(&sessions_dir) {
            let Some(mtime_ms) = entry_mtime(&entry, &usage) else {
                continue;
            };
            if let Some(raw) = parse_copilot_session_for_entry(entry, mtime_ms, true, &usage)? {
                sessions.push(raw);
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
        let Some(sessions_dir) = resolve_copilot_dir()? else {
            return Ok(Some(SyncScanResult {
                sessions: vec![],
                stats: SyncScanStats::default(),
                observations: Vec::new(),
            }));
        };
        let result = scan_for_sync_impl(
            &sessions_dir,
            context,
            since_ts,
            include_events,
            resolve_session_store_db().as_deref(),
        )?;
        Ok(Some(result))
    }
}

fn copilot_session_url(source_id: &str) -> String {
    format!("ghapp://sessions/{source_id}")
}

#[cfg(target_os = "macos")]
fn open_url_command(url: String) -> ResumeCommand {
    ResumeCommand { program: "open".to_string(), args: vec![url] }
}

#[cfg(target_os = "windows")]
fn open_url_command(url: String) -> ResumeCommand {
    ResumeCommand {
        program: "cmd".to_string(),
        args: vec!["/C".to_string(), "start".to_string(), String::new(), url],
    }
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
fn open_url_command(url: String) -> ResumeCommand {
    ResumeCommand { program: "xdg-open".to_string(), args: vec![url] }
}

fn resolve_copilot_dir() -> anyhow::Result<Option<PathBuf>> {
    resolve_home_dir(
        ".copilot/session-state",
        "~/.copilot/session-state not found, skipping Copilot CLI",
    )
}

fn resolve_session_store_db() -> Option<PathBuf> {
    let path = dirs::home_dir()?.join(".copilot").join("session-store.db");
    path.is_file().then_some(path)
}

enum CopilotUsageIndex {
    Unavailable,
    Available { events: HashMap<String, Vec<RawUsageEvent>>, latest_ts: HashMap<String, i64> },
}

impl CopilotUsageIndex {
    fn is_available(&self) -> bool {
        matches!(self, Self::Available { .. })
    }

    fn latest_ts(&self, session_id: &str) -> i64 {
        match self {
            Self::Unavailable => 0,
            Self::Available { latest_ts, .. } => latest_ts.get(session_id).copied().unwrap_or(0),
        }
    }

    fn events_for(&self, session_id: &str) -> Option<Vec<RawUsageEvent>> {
        match self {
            Self::Unavailable => None,
            Self::Available { events, .. } => {
                Some(events.get(session_id).cloned().unwrap_or_default())
            }
        }
    }
}

fn load_copilot_usage_index(db_path: Option<&Path>) -> CopilotUsageIndex {
    let Some(path) = db_path else {
        return CopilotUsageIndex::Unavailable;
    };
    let conn = match opencode::open_readonly(path) {
        Ok(Some(conn)) => conn,
        Ok(None) => return CopilotUsageIndex::Unavailable,
        Err(error) => {
            debug!("failed to open {}: {error}", path.display());
            return CopilotUsageIndex::Unavailable;
        }
    };
    let mut stmt = match conn.prepare(
        "SELECT id, session_id, model, input_tokens, output_tokens,
                cache_read_tokens, cache_write_tokens, reasoning_tokens, created_at
         FROM assistant_usage_events
         ORDER BY session_id, id",
    ) {
        Ok(stmt) => stmt,
        Err(error) => {
            debug!("copilot session-store usage table unavailable: {error}");
            return CopilotUsageIndex::Unavailable;
        }
    };
    let rows = match stmt.query_map(params![], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<i64>>(3)?,
            row.get::<_, Option<i64>>(4)?,
            row.get::<_, Option<i64>>(5)?,
            row.get::<_, Option<i64>>(6)?,
            row.get::<_, Option<i64>>(7)?,
            row.get::<_, Option<String>>(8)?,
        ))
    }) {
        Ok(rows) => rows,
        Err(error) => {
            debug!("failed to read copilot usage events: {error}");
            return CopilotUsageIndex::Unavailable;
        }
    };

    let mut events = HashMap::new();
    let mut latest_ts = HashMap::new();
    for row in rows.flatten() {
        let (id, session_id, model, input, output, cache_read, cache_write, reasoning, created_at) =
            row;
        let inclusive_input = input.unwrap_or(0).max(0);
        let output_tokens = output.unwrap_or(0).max(0);
        let cache_read_tokens = cache_read.unwrap_or(0).max(0);
        let cache_write_tokens = cache_write.unwrap_or(0).max(0);
        let reasoning_tokens = reasoning.unwrap_or(0).max(0);
        let input_tokens =
            inclusive_input.saturating_sub(cache_read_tokens.saturating_add(cache_write_tokens));
        if input_tokens == 0
            && output_tokens == 0
            && cache_read_tokens == 0
            && cache_write_tokens == 0
            && reasoning_tokens == 0
        {
            continue;
        }
        let timestamp = created_at
            .as_deref()
            .and_then(|value| rfc3339_ms(Some(&Value::String(value.to_string()))))
            .unwrap_or(0);
        let event_seq = events.get(&session_id).map(Vec::len).unwrap_or(0) as u32;
        let model =
            model.filter(|value| !value.trim().is_empty()).unwrap_or_else(|| "unknown".to_string());
        let raw_usage_json = serde_json::json!({
            "id": id,
            "input_tokens": inclusive_input,
            "output_tokens": output_tokens,
            "cache_read_tokens": cache_read_tokens,
            "cache_write_tokens": cache_write_tokens,
            "reasoning_tokens": reasoning_tokens,
        })
        .to_string();
        let event = RawUsageEvent {
            model,
            provider: "github".to_string(),
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_write_tokens,
            reasoning_tokens,
            source_path: path.to_str().map(str::to_string),
            raw_usage_json: Some(raw_usage_json),
            ..RawUsageEvent::observed(
                format!("usage:{id}"),
                event_seq,
                timestamp,
                USAGE_PARSER_VERSION,
            )
        };
        latest_ts
            .entry(session_id.clone())
            .and_modify(|latest| {
                if timestamp > *latest {
                    *latest = timestamp;
                }
            })
            .or_insert(timestamp);
        events.entry(session_id).or_default().push(event);
    }
    CopilotUsageIndex::Available { events, latest_ts }
}

fn entry_mtime(entry: &FileScanEntry, usage: &CopilotUsageIndex) -> Option<i64> {
    let file_mtime = file_scan::stat_mtime_ms(&entry.stat_target)?;
    Some(file_mtime.max(usage.latest_ts(&entry.session_id)))
}

fn attach_usage(raw: RawSession, usage: &CopilotUsageIndex) -> RawSession {
    match usage.events_for(&raw.source_id) {
        Some(events) => raw.with_usage(events, USAGE_PARSER_VERSION),
        None => raw,
    }
}

fn scan_for_sync_impl(
    sessions_dir: &Path,
    context: &AdapterSyncContext,
    since_ts: Option<i64>,
    include_events: bool,
    usage_db: Option<&Path>,
) -> anyhow::Result<SyncScanResult> {
    let entries = collect_copilot_entries(sessions_dir);
    let usage = load_copilot_usage_index(usage_db);
    file_scan::run_file_scan_with_options_and_mtime(
        context,
        since_ts,
        FileScanOptions {
            usage_parser_version: usage.is_available().then_some(USAGE_PARSER_VERSION),
            event_parser_version: include_events.then_some(EVENT_PARSER_VERSION),
            metadata_parser_version: include_events.then_some(METADATA_PARSER_VERSION),
        },
        entries,
        |entry| entry_mtime(entry, &usage),
        |entry, mtime_ms| parse_copilot_session_for_entry(entry, mtime_ms, include_events, &usage),
    )
}

fn collect_copilot_entries(sessions_dir: &Path) -> Vec<FileScanEntry> {
    let mut entries = Vec::new();
    let read = match fs::read_dir(sessions_dir) {
        Ok(r) => r,
        Err(e) => {
            debug!("cannot read {}: {e}", sessions_dir.display());
            return entries;
        }
    };

    for dir_entry in read.flatten() {
        let session_dir = dir_entry.path();
        if !session_dir.is_dir() {
            continue;
        }
        let events_path = session_dir.join("events.jsonl");
        if !events_path.is_file() {
            continue;
        }
        let dir_name = match session_dir.file_name().and_then(|n| n.to_str()) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => continue,
        };
        let session_id = peek_copilot_session_id(&events_path).unwrap_or_else(|| dir_name.clone());

        entries.push(FileScanEntry { session_id, stat_target: events_path, directory: None });
    }
    entries
}

fn peek_copilot_session_id(events_path: &Path) -> Option<String> {
    let file = fs::File::open(events_path).ok()?;
    let reader = BufReader::new(file);
    for (idx, line) in reader.lines().enumerate() {
        if idx >= 16 {
            break;
        }
        let Ok(line) = line else { continue };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("type").and_then(|t| t.as_str()) == Some("session.start") {
            return v
                .get("data")
                .and_then(|d| d.get("sessionId"))
                .and_then(|s| s.as_str())
                .map(String::from);
        }
    }
    None
}

fn parse_copilot_session_for_entry(
    entry: FileScanEntry,
    mtime_ms: i64,
    include_events: bool,
    usage: &CopilotUsageIndex,
) -> anyhow::Result<Option<RawSession>> {
    let source_file_path = entry.stat_target.to_str().map(str::to_string);
    let file = match fs::File::open(&entry.stat_target) {
        Ok(f) => f,
        Err(e) => {
            debug!("failed to read {}: {e}", entry.stat_target.display());
            return Ok(None);
        }
    };
    let lines = BufReader::new(file).lines();
    let source_path = entry.stat_target.display().to_string();
    let mut raw = match parse_copilot_events_from_lines(
        lines,
        &entry.session_id,
        include_events,
        Some(source_path),
    ) {
        Ok(Some(raw)) => raw,
        Ok(None) => return Ok(None),
        Err(e) => {
            debug!("failed to parse copilot session {}: {e}", entry.stat_target.display());
            return Ok(None);
        }
    };
    raw.source_id = entry.session_id;
    raw.updated_at = Some(mtime_ms);
    raw.source_file_path = source_file_path;
    Ok(Some(attach_usage(raw, usage)))
}

#[cfg(test)]
pub(crate) fn parse_copilot_events(
    content: &str,
    fallback_id: &str,
) -> anyhow::Result<Option<RawSession>> {
    parse_copilot_events_from_lines(
        content.lines().map(|s| io::Result::Ok(s.to_string())),
        fallback_id,
        true,
        None,
    )
}

fn parse_copilot_events_from_lines<I>(
    lines: I,
    fallback_id: &str,
    include_events: bool,
    source_path: Option<String>,
) -> anyhow::Result<Option<RawSession>>
where
    I: IntoIterator<Item = io::Result<String>>,
{
    let mut session_id: Option<String> = None;
    let mut directory: Option<String> = None;
    let mut current_directory: Option<String> = None;
    let mut external_calls: HashMap<String, (Option<String>, Option<String>)> = HashMap::new();
    let mut meta_started_at: Option<i64> = None;
    let mut last_event_timestamp = None;
    let mut tool_names: HashMap<String, String> = HashMap::new();
    let mut messages = Vec::new();
    let mut session_events = Vec::new();

    for item in jsonl_indexed(lines) {
        let (line_index, v) = item?;

        let event_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let timestamp = parse_timestamp(&v);
        last_event_timestamp = last_event_timestamp.max(timestamp);
        let line_id = v
            .get("id")
            .and_then(|id| id.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| line_index.to_string());

        match event_type {
            "session.start" => {
                if let Some(data) = v.get("data") {
                    session_id = data.get("sessionId").and_then(|s| s.as_str()).map(String::from);
                    meta_started_at = rfc3339_ms(data.get("startTime"));
                    directory = data
                        .get("context")
                        .and_then(|c| c.get("cwd"))
                        .and_then(|c| c.as_str())
                        .map(String::from);
                    current_directory.clone_from(&directory);
                }
            }
            "session.resume" => {
                current_directory = v
                    .pointer("/data/context/cwd")
                    .and_then(Value::as_str)
                    .filter(|cwd| !cwd.trim().is_empty())
                    .map(str::to_string);
            }
            "external_tool.requested" if include_events => {
                let Some(data) = v.get("data") else { continue };
                let call_id = data.get("toolCallId").and_then(Value::as_str).map(str::to_string);
                let name = data.get("toolName").and_then(Value::as_str).map(str::to_string);
                if let Some(request_id) = data.get("requestId").and_then(Value::as_str) {
                    external_calls.insert(request_id.to_string(), (call_id.clone(), name.clone()));
                }
                let cwd = data
                    .get("workingDirectory")
                    .and_then(Value::as_str)
                    .filter(|cwd| !cwd.trim().is_empty());
                let mut event = copilot_tool_call(
                    events::EventContext {
                        event_seq: session_events.len() as u32,
                        timestamp,
                        source_path: source_path.clone(),
                        source_event_id: Some(line_id),
                        message_seq: messages.len().checked_sub(1).map(|seq| seq as u32),
                        parser_version: EVENT_PARSER_VERSION,
                    },
                    name.as_deref().unwrap_or("tool"),
                    data.get("arguments"),
                    cwd,
                    cwd,
                );
                event.kind = "tool_start".to_string();
                event.tool_call_id = call_id;
                event.attrs_json = Some(v.to_string());
                session_events.push(event);
            }
            "external_tool.completed" if include_events => {
                let request = v
                    .pointer("/data/requestId")
                    .and_then(Value::as_str)
                    .and_then(|id| external_calls.get(id));
                let mut event = events::tool_result_event(
                    events::EventContext {
                        event_seq: session_events.len() as u32,
                        timestamp,
                        source_path: source_path.clone(),
                        source_event_id: Some(line_id),
                        message_seq: messages.len().checked_sub(1).map(|seq| seq as u32),
                        parser_version: EVENT_PARSER_VERSION,
                    },
                    request.and_then(|(_, name)| name.clone()),
                    None,
                );
                event.tool_call_id = request.and_then(|(id, _)| id.clone());
                event.attrs_json = Some(v.to_string());
                session_events.push(event);
            }
            "user.message" => {
                let Some(data) = v.get("data") else { continue };
                let content =
                    data.get("content").and_then(|c| c.as_str()).unwrap_or("").trim().to_string();
                if content.is_empty() {
                    continue;
                }
                messages.push(RawMessage { role: Role::User, content, timestamp });
            }
            "assistant.message" => {
                let Some(data) = v.get("data") else { continue };
                let prose = data.get("content").and_then(Value::as_str).unwrap_or("").trim();
                if !prose.is_empty() {
                    messages.push(RawMessage {
                        role: Role::Assistant,
                        content: prose.to_string(),
                        timestamp,
                    });
                }
                if let Some(requests) = data.get("toolRequests").and_then(Value::as_array) {
                    for (index, request) in requests.iter().enumerate() {
                        let name = request.get("name").and_then(Value::as_str).unwrap_or("tool");
                        let call_id = request.get("toolCallId").and_then(Value::as_str);
                        if let Some(call_id) = call_id {
                            tool_names.insert(call_id.to_string(), name.to_string());
                        }
                        if include_events {
                            let context = events::EventContext {
                                event_seq: session_events.len() as u32,
                                timestamp,
                                source_path: source_path.clone(),
                                source_event_id: Some(format!("{line_id}:tool:{index}")),
                                message_seq: messages.len().checked_sub(1).map(|seq| seq as u32),
                                parser_version: EVENT_PARSER_VERSION,
                            };
                            let mut event = copilot_tool_call(
                                context,
                                name,
                                request.get("arguments"),
                                current_directory.as_deref(),
                                None,
                            );
                            event.tool_call_id = call_id.map(str::to_string);
                            event.attrs_json = Some(v.to_string());
                            session_events.push(event);
                        }
                    }
                }
            }
            "tool.execution_start" => {
                let Some(data) = v.get("data") else { continue };
                let call_id = data.get("toolCallId").and_then(Value::as_str);
                let name = data.get("toolName").and_then(Value::as_str);
                if let (Some(id), Some(name)) = (call_id, name) {
                    tool_names.insert(id.to_string(), name.to_string());
                }
                if include_events {
                    let context = events::EventContext {
                        event_seq: session_events.len() as u32,
                        timestamp,
                        source_path: source_path.clone(),
                        source_event_id: Some(line_id),
                        message_seq: messages.len().checked_sub(1).map(|seq| seq as u32),
                        parser_version: EVENT_PARSER_VERSION,
                    };
                    let mut event = copilot_tool_call(
                        context,
                        name.unwrap_or("tool"),
                        data.get("arguments"),
                        current_directory.as_deref(),
                        None,
                    );
                    event.kind = "tool_start".to_string();
                    event.tool_call_id = call_id.map(str::to_string);
                    event.attrs_json = Some(v.to_string());
                    session_events.push(event);
                }
            }
            "tool.execution_complete" if include_events => {
                let Some(data) = v.get("data") else { continue };
                let call_id = data.get("toolCallId").and_then(Value::as_str);
                let name = data
                    .get("toolName")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| call_id.and_then(|id| tool_names.get(id).cloned()));
                let text = data
                    .get("result")
                    .and_then(|result| {
                        result
                            .get("detailedContent")
                            .and_then(Value::as_str)
                            .or_else(|| result.get("content").and_then(Value::as_str))
                    })
                    .map(str::to_string)
                    .or_else(|| data.get("error").and_then(Value::as_str).map(str::to_string));
                let mut event = events::tool_result_event(
                    events::EventContext {
                        event_seq: session_events.len() as u32,
                        timestamp,
                        source_path: source_path.clone(),
                        source_event_id: Some(line_id),
                        message_seq: messages.len().checked_sub(1).map(|seq| seq as u32),
                        parser_version: EVENT_PARSER_VERSION,
                    },
                    name,
                    text,
                );
                event.tool_call_id = call_id.map(str::to_string);
                event.status = data
                    .get("success")
                    .and_then(Value::as_bool)
                    .map(|success| if success { "success" } else { "error" }.to_string());
                event.attrs_json = Some(v.to_string());
                session_events.push(event);
            }
            _ => {}
        }
    }

    if messages.is_empty() && session_events.is_empty() {
        return Ok(None);
    }

    let source_id = session_id.unwrap_or_else(|| fallback_id.to_string());
    let started_at = first_timestamp(meta_started_at, &messages, &[], &session_events).unwrap_or(0);
    let updated_at =
        last_event_timestamp.or_else(|| last_timestamp(None, &messages, &[], &session_events));

    let mut session =
        RawSession::search_only(source_id, directory, started_at, updated_at, None, messages);
    session.metadata_parser_version = Some(METADATA_PARSER_VERSION);
    session.refresh_session_on_metadata_backfill = true;
    if include_events {
        session = session.with_events(session_events, EVENT_PARSER_VERSION);
    }
    Ok(Some(session))
}

fn copilot_tool_call(
    context: events::EventContext,
    name: &str,
    args: Option<&Value>,
    directory: Option<&str>,
    command_cwd: Option<&str>,
) -> RawSessionEvent {
    let mut event = events::tool_call_event(context, name.to_string(), args);
    event.kind = "tool_call".to_string();
    event.target = None;
    if name == "apply_patch" {
        if let Some(patch) = args.and_then(Value::as_str) {
            event.files = events::patch_file_evidence(patch);
            for file in &mut event.files {
                file.cwd = directory.map(str::to_string);
            }
            if !event.files.is_empty() {
                event.kind = "file_write".to_string();
            }
            event.target = event.files.first().map(|file| file.path.clone());
        }
    } else if name == "view" {
        if let Some(path) = args.and_then(|args| args.get("path")).and_then(Value::as_str)
            && !path.trim().is_empty()
        {
            event.kind = "file_read".to_string();
            event.target = Some(path.to_string());
            event.files.push(FileEvidence {
                path: path.to_string(),
                operation: FileOperation::Read,
                kind: FileEvidenceKind::Call,
                cwd: directory.map(str::to_string),
                target: None,
            });
        }
    } else if name == "bash" {
        event.kind = "command".to_string();
        event.target =
            args.and_then(|args| args.get("command")).and_then(Value::as_str).map(str::to_string);
        if let Some(command) = event.target.as_deref() {
            let (files, status) = events::shell_file_evidence(command, command_cwd);
            event.files = files;
            event.command_evidence_status = Some(status);
        } else {
            event.command_evidence_status = Some(CommandEvidenceStatus::Unsupported);
        }
    }
    event
}

fn parse_timestamp(v: &Value) -> Option<i64> {
    rfc3339_ms(v.get("timestamp"))
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

    fn temp_copilot_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "recall-cpl-test-{}-{}",
            label,
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_copilot_session(
        sessions_dir: &Path,
        dir_name: &str,
        session_id: &str,
        user_text: &str,
    ) -> PathBuf {
        let session_dir = sessions_dir.join(dir_name);
        fs::create_dir_all(&session_dir).unwrap();
        let events_path = session_dir.join("events.jsonl");

        let start = serde_json::json!({
            "type": "session.start",
            "timestamp": "2026-04-13T10:00:00Z",
            "data": {
                "sessionId": session_id,
                "startTime": "2026-04-13T10:00:00Z",
                "context": { "cwd": "/tmp/foo" }
            }
        });
        let user = serde_json::json!({
            "type": "user.message",
            "timestamp": "2026-04-13T10:00:05Z",
            "data": { "content": user_text }
        });

        let mut f = fs::File::create(&events_path).unwrap();
        writeln!(f, "{start}").unwrap();
        writeln!(f, "{user}").unwrap();
        events_path
    }

    fn make_existing_session(source_id: &str, updated_at: i64, message_count: u32) -> Session {
        Session {
            id: format!("internal-{source_id}"),
            source: "copilot-cli".to_string(),
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
    fn copilot_app_command_opens_session_deeplink() {
        let command = CopilotAdapter.app_command("7d5c993a-0966-4e3e-9622-7a39ba9576ba").unwrap();

        assert!(
            command
                .args
                .iter()
                .any(|arg| arg == "ghapp://sessions/7d5c993a-0966-4e3e-9622-7a39ba9576ba")
        );
    }

    #[test]
    fn peek_copilot_session_id_reads_session_start() {
        let root = temp_copilot_root("peek");
        let sessions_dir = root.join("session-state");
        let uuid = "f3eca837-818f-44d7-9158-bf242901f960";
        let events_path = write_copilot_session(&sessions_dir, "dir-alias", uuid, "hello");

        assert_eq!(peek_copilot_session_id(&events_path), Some(uuid.to_string()));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn peek_copilot_session_id_falls_back_when_no_session_start() {
        let root = temp_copilot_root("peek-missing");
        let sessions_dir = root.join("session-state");
        let dir = sessions_dir.join("dir-alias");
        fs::create_dir_all(&dir).unwrap();
        let events_path = dir.join("events.jsonl");
        let msg = serde_json::json!({
            "type": "user.message",
            "timestamp": "2026-04-13T10:00:00Z",
            "data": { "content": "hi" }
        });
        let mut f = fs::File::create(&events_path).unwrap();
        writeln!(f, "{msg}").unwrap();

        assert_eq!(peek_copilot_session_id(&events_path), None);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn collect_copilot_entries_skips_dirs_without_events() {
        let root = temp_copilot_root("collect-skip");
        let sessions_dir = root.join("session-state");
        fs::create_dir_all(sessions_dir.join("empty-dir")).unwrap();
        write_copilot_session(
            &sessions_dir,
            "good-dir",
            "f3eca837-818f-44d7-9158-bf242901f960",
            "hello",
        );

        let entries = collect_copilot_entries(&sessions_dir);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id, "f3eca837-818f-44d7-9158-bf242901f960");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_copilot_events_extracts_tool_events() {
        let jsonl = r##"{"type":"session.start","data":{"sessionId":"sess-2","startTime":"2026-04-13T10:00:00Z","context":{"cwd":"/proj"}},"id":"e1","timestamp":"2026-04-13T10:00:00Z","parentId":null}
{"type":"assistant.message","data":{"messageId":"m1","content":"Let me read the file.","toolRequests":[{"toolCallId":"tc1","name":"view","arguments":{"path":"/tmp/README.md"},"type":"function"}]},"id":"e2","timestamp":"2026-04-13T10:00:05Z","parentId":"e1"}
{"type":"tool.execution_complete","data":{"toolCallId":"tc1","toolName":"view","success":true,"result":{"content":"short summary","detailedContent":"# My Project\nHello world."}},"id":"e4","timestamp":"2026-04-13T10:00:06Z","parentId":"e3"}"##;

        let session = parse_copilot_events(jsonl, "fallback").unwrap().unwrap();
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].content, "Let me read the file.");
        assert_eq!(session.events.len(), 2);
        assert_eq!(session.events[0].tool_call_id.as_deref(), Some("tc1"));
        assert_eq!(session.events[1].tool_call_id, session.events[0].tool_call_id);
        assert_eq!(session.events[0].source_event_id.as_deref(), Some("e2:tool:0"));
        assert_eq!(session.events[1].source_event_id.as_deref(), Some("e4"));
        assert_eq!(session.events[0].files[0].cwd.as_deref(), Some("/proj"));
        assert_eq!(session.events[0].files[0].operation, FileOperation::Read);
        assert!(session.events[1].files.is_empty());
        assert_eq!(session.events[0].kind, "file_read");
        assert_eq!(session.events[0].name.as_deref(), Some("view"));
        assert_eq!(session.events[0].target.as_deref(), Some("/tmp/README.md"));
        assert_eq!(session.events[1].kind, "tool_result");
        assert_eq!(session.events[1].status.as_deref(), Some("success"));
        assert_eq!(session.event_parser_version, Some(EVENT_PARSER_VERSION));
    }

    #[test]
    fn native_patch_lifecycle_keeps_failures_and_visible_anchors() {
        let jsonl = r#"{"type":"session.start","data":{"sessionId":"native","context":{"cwd":"/repo"}}}
{"type":"assistant.message","id":"request-1","data":{"content":"","toolRequests":[{"toolCallId":"patch-1","name":"apply_patch","arguments":"*** Begin Patch\n*** Update File: a.rs\n@@\n-old\n+new\n*** Delete File: b.rs\n*** End Patch"}]}}
{"type":"tool.execution_start","id":"start-1","data":{"toolCallId":"patch-1","toolName":"apply_patch"}}
{"type":"tool.execution_complete","id":"failed-1","data":{"toolCallId":"patch-1","success":false,"error":"No matching lines"}}
{"type":"user.message","data":{"content":"Try again"}}
{"type":"assistant.message","id":"request-2","data":{"content":"","toolRequests":[{"toolCallId":"read-2","name":"view","arguments":{"path":"a.rs"}}]}}
{"type":"tool.execution_complete","id":"result-2","data":{"toolCallId":"read-2","result":{"detailedContent":"file text"}}}
{"type":"session.resume","data":{"context":{"cwd":"/resumed"}}}
{"type":"assistant.message","id":"shell-request","data":{"content":"","toolRequests":[{"toolCallId":"shell-1","name":"bash","arguments":{"command":"mv input.rs output.rs"}},{"toolCallId":"read-3","name":"view","arguments":{"path":"output.rs"}}]}}
{"type":"external_tool.requested","id":"external-start","data":{"requestId":"external-1","toolCallId":"shell-1","toolName":"bash","arguments":{"command":"mv input.rs output.rs"},"workingDirectory":"/explicit"}}
{"type":"external_tool.completed","id":"external-complete","data":{"requestId":"external-1"}}
"#;
        let raw = parse_copilot_events_from_lines(
            jsonl.lines().map(|line| Ok(line.to_string())),
            "fallback",
            true,
            Some("events.jsonl".to_string()),
        )
        .unwrap()
        .unwrap();
        assert_eq!(raw.messages.len(), 1);
        assert_eq!(raw.events.len(), 9);
        assert_eq!(raw.directory.as_deref(), Some("/repo"));
        assert_eq!(raw.events[5].kind, "command");
        assert_eq!(raw.events[5].files.len(), 2);
        assert!(raw.events[5].files.iter().all(|file| file.cwd.is_none()));
        assert_eq!(raw.events[5].command_evidence_status, Some(CommandEvidenceStatus::Unsupported));
        assert_eq!(raw.events[6].files[0].cwd.as_deref(), Some("/resumed"));
        assert_eq!(raw.events[7].files.len(), 2);
        assert!(raw.events[7].files.iter().all(|file| file.cwd.as_deref() == Some("/explicit")));
        assert_eq!(raw.events[7].tool_call_id, raw.events[8].tool_call_id);
        assert!(raw.events[8].files.is_empty());
        assert_eq!(raw.events[8].status, None);
        assert_eq!(raw.events[0].files.len(), 2);
        assert_eq!(raw.events[0].files[0].path, "a.rs");
        assert_eq!(raw.events[0].files[1].operation, FileOperation::Delete);
        assert_eq!(raw.events[0].message_seq, None);
        assert_eq!(raw.events[1].kind, "tool_start");
        assert!(raw.events[1].files.is_empty());
        assert_eq!(raw.events[1].status, None);
        assert_eq!(raw.events[2].status.as_deref(), Some("error"));
        assert_eq!(raw.events[2].tool_call_id.as_deref(), Some("patch-1"));
        assert_eq!(raw.events[3].message_seq, Some(0));
        assert_eq!(raw.events[4].status, None);
        assert_eq!(raw.events[4].message_seq, Some(0));
        assert_eq!(raw.events[0].source_path.as_deref(), Some("events.jsonl"));
        let original: Value = serde_json::from_str(jsonl.lines().nth(1).unwrap()).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(raw.events[0].attrs_json.as_deref().unwrap()).unwrap(),
            original
        );
        let mut interrupted: Vec<io::Result<String>> =
            jsonl.lines().map(|line| Ok(line.to_string())).collect();
        interrupted.push(Err(io::Error::other("interrupted read")));
        assert!(parse_copilot_events_from_lines(interrupted, "fallback", true, None).is_err());
        let raw = parse_copilot_events_from_lines(
            jsonl.lines().map(|line| Ok(line.to_string())),
            "fallback",
            false,
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(raw.messages.len(), 1);
        assert!(raw.events.is_empty());
    }

    #[test]
    fn scan_for_sync_skips_unchanged_session() {
        let root = temp_copilot_root("skip");
        let sessions_dir = root.join("session-state");
        let uuid = "f3eca837-818f-44d7-9158-bf242901f960";
        let events_path = write_copilot_session(&sessions_dir, "dir-1", uuid, "hello");
        let mtime = file_scan::stat_mtime_ms(&events_path).unwrap();

        let store = setup_store();
        store.insert_session(&make_existing_session(uuid, mtime, 1)).unwrap();
        store
            .persist_session_events_for_existing_session(
                "copilot-cli",
                uuid,
                &[],
                EVENT_PARSER_VERSION,
                Some(mtime),
            )
            .unwrap();
        store
            .persist_usage_events_for_existing_session(
                "copilot-cli",
                uuid,
                &[],
                USAGE_PARSER_VERSION,
                Some(mtime),
            )
            .unwrap();

        let refreshed = scan_for_sync_impl(
            &sessions_dir,
            &AdapterSyncContext::from_store_for_test(&store, "copilot-cli").unwrap(),
            None,
            true,
            None,
        )
        .unwrap();
        assert_eq!(refreshed.sessions.len(), 1);
        assert!(refreshed.sessions[0].refresh_session_on_metadata_backfill);
        store
            .persist_topology_for_existing_session(
                "copilot-cli",
                uuid,
                &crate::db::store::SessionTopologyWrite {
                    thread_role: None,
                    parents: &[],
                    parser_version: Some(METADATA_PARSER_VERSION),
                },
            )
            .unwrap();
        let result = scan_for_sync_impl(
            &sessions_dir,
            &AdapterSyncContext::from_store_for_test(&store, "copilot-cli").unwrap(),
            None,
            true,
            None,
        )
        .unwrap();
        assert_eq!(result.sessions.len(), 0);
        assert_eq!(result.stats.skipped_sessions, 1);
        store
            .persist_session_events_for_existing_session(
                "copilot-cli",
                uuid,
                &[],
                EVENT_PARSER_VERSION - 1,
                Some(mtime),
            )
            .unwrap();
        let stale = scan_for_sync_impl(
            &sessions_dir,
            &AdapterSyncContext::from_store_for_test(&store, "copilot-cli").unwrap(),
            None,
            true,
            None,
        )
        .unwrap();
        assert_eq!(stale.sessions.len(), 1);
        assert_eq!(stale.sessions[0].event_parser_version, Some(EVENT_PARSER_VERSION));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_for_sync_reparses_when_mtime_changes() {
        let root = temp_copilot_root("mismatch");
        let sessions_dir = root.join("session-state");
        let uuid = "f3eca837-818f-44d7-9158-bf242901f960";
        let events_path = write_copilot_session(&sessions_dir, "dir-1", uuid, "hi");
        let actual_mtime = file_scan::stat_mtime_ms(&events_path).unwrap();

        let store = setup_store();
        store.insert_session(&make_existing_session(uuid, actual_mtime - 1_000, 1)).unwrap();

        let result = scan_for_sync_impl(
            &sessions_dir,
            &AdapterSyncContext::from_store_for_test(&store, "copilot-cli").unwrap(),
            None,
            true,
            None,
        )
        .unwrap();
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].source_id, uuid);
        assert_eq!(result.sessions[0].updated_at, Some(actual_mtime));
        assert_eq!(result.stats.skipped_sessions, 0);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_for_sync_picks_up_new_session() {
        let root = temp_copilot_root("new");
        let sessions_dir = root.join("session-state");
        let uuid = "f3eca837-818f-44d7-9158-bf242901f960";
        let events_path = write_copilot_session(&sessions_dir, "dir-1", uuid, "fresh");

        let store = setup_store();

        let result = scan_for_sync_impl(
            &sessions_dir,
            &AdapterSyncContext::from_store_for_test(&store, "copilot-cli").unwrap(),
            None,
            true,
            None,
        )
        .unwrap();
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].source_id, uuid);
        assert_eq!(result.sessions[0].source_file_path.as_deref(), events_path.to_str());
        assert_eq!(result.sessions[0].usage_parser_version, None);
        assert!(result.sessions[0].usage_events.is_empty());
        assert_eq!(result.stats.skipped_sessions, 0);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_for_sync_falls_back_to_dir_name_when_session_start_missing() {
        let root = temp_copilot_root("fallback");
        let sessions_dir = root.join("session-state");
        let dir_name = "0b247666-6f95-49e5-b68f-b05eb338e9c2";
        let session_dir = sessions_dir.join(dir_name);
        fs::create_dir_all(&session_dir).unwrap();
        let events_path = session_dir.join("events.jsonl");
        let user = serde_json::json!({
            "type": "user.message",
            "timestamp": "2026-04-13T10:00:00Z",
            "data": { "content": "legacy" }
        });
        let mut f = fs::File::create(&events_path).unwrap();
        writeln!(f, "{user}").unwrap();

        let store = setup_store();
        let result = scan_for_sync_impl(
            &sessions_dir,
            &AdapterSyncContext::from_store_for_test(&store, "copilot-cli").unwrap(),
            None,
            true,
            None,
        )
        .unwrap();
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].source_id, dir_name);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_for_sync_keeps_going_when_one_file_is_unreadable() {
        let root = temp_copilot_root("unreadable");
        let sessions_dir = root.join("session-state");

        let good_uuid = "f3eca837-818f-44d7-9158-bf242901f960";
        write_copilot_session(&sessions_dir, "good-dir", good_uuid, "still here");

        let bad_dir = sessions_dir.join("bad-dir");
        fs::create_dir_all(&bad_dir).unwrap();
        let bad_events = bad_dir.join("events.jsonl");
        let mut f = fs::File::create(&bad_events).unwrap();
        f.write_all(&[0xFF, 0xFE, 0xFD, 0xFC]).unwrap();

        let store = setup_store();
        let result = scan_for_sync_impl(
            &sessions_dir,
            &AdapterSyncContext::from_store_for_test(&store, "copilot-cli").unwrap(),
            None,
            true,
            None,
        )
        .unwrap();
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].source_id, good_uuid);

        let _ = fs::remove_dir_all(&root);
    }

    fn write_usage_schema(path: &Path) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE assistant_usage_events (
                id INTEGER PRIMARY KEY,
                session_id TEXT,
                turn_index INTEGER,
                model TEXT,
                input_tokens INTEGER,
                output_tokens INTEGER,
                cache_read_tokens INTEGER,
                cache_write_tokens INTEGER,
                reasoning_tokens INTEGER,
                created_at TEXT
            );",
        )
        .unwrap();
    }

    fn write_usage_db(path: &Path, session_id: &str, created_at: &str) {
        write_usage_schema(path);
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute(
            "INSERT INTO assistant_usage_events (
                session_id, turn_index, model, input_tokens, output_tokens,
                cache_read_tokens, cache_write_tokens, reasoning_tokens, created_at
             ) VALUES (?1, 0, 'gpt-5.6-luna', 100, 20, 5, 0, 3, ?2)",
            params![session_id, created_at],
        )
        .unwrap();
    }

    #[test]
    fn scan_for_sync_attaches_usage_from_session_store() {
        let root = temp_copilot_root("usage");
        let sessions_dir = root.join("session-state");
        let uuid = "f3eca837-818f-44d7-9158-bf242901f960";
        write_copilot_session(&sessions_dir, "dir-1", uuid, "hello");
        let usage_db = root.join("session-store.db");
        write_usage_db(&usage_db, uuid, "2026-08-27T19:12:13.186Z");

        let store = setup_store();
        let result = scan_for_sync_impl(
            &sessions_dir,
            &AdapterSyncContext::from_store_for_test(&store, "copilot-cli").unwrap(),
            None,
            true,
            Some(&usage_db),
        )
        .unwrap();
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].usage_parser_version, Some(USAGE_PARSER_VERSION));
        assert_eq!(result.sessions[0].usage_events.len(), 1);
        assert_eq!(result.sessions[0].usage_events[0].model, "gpt-5.6-luna");
        assert_eq!(result.sessions[0].usage_events[0].provider, "github");
        assert_eq!(result.sessions[0].usage_events[0].input_tokens, 95);
        assert_eq!(result.sessions[0].usage_events[0].output_tokens, 20);
        assert_eq!(result.sessions[0].usage_events[0].cache_read_tokens, 5);
        assert_eq!(result.sessions[0].usage_events[0].reasoning_tokens, 3);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn usage_subtracts_inclusive_cache_from_input() {
        let root = temp_copilot_root("usage-cache");
        let sessions_dir = root.join("session-state");
        let uuid = "f3eca837-818f-44d7-9158-bf242901f960";
        write_copilot_session(&sessions_dir, "dir-1", uuid, "hello");
        let usage_db = root.join("session-store.db");
        write_usage_schema(&usage_db);
        let conn = rusqlite::Connection::open(&usage_db).unwrap();
        conn.execute(
            "INSERT INTO assistant_usage_events (
                session_id, turn_index, model, input_tokens, output_tokens,
                cache_read_tokens, cache_write_tokens, reasoning_tokens, created_at
             ) VALUES (?1, 0, 'gpt-5.6-luna', 31734, 80, 30612, 1120, 0, '2026-08-27T19:12:13.186Z')",
            params![uuid],
        )
        .unwrap();

        let store = setup_store();
        let result = scan_for_sync_impl(
            &sessions_dir,
            &AdapterSyncContext::from_store_for_test(&store, "copilot-cli").unwrap(),
            None,
            true,
            Some(&usage_db),
        )
        .unwrap();
        assert_eq!(result.sessions[0].usage_events[0].input_tokens, 2);
        assert_eq!(result.sessions[0].usage_events[0].cache_read_tokens, 30_612);
        assert_eq!(result.sessions[0].usage_events[0].cache_write_tokens, 1_120);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_for_sync_marks_empty_usage_when_db_is_readable() {
        let root = temp_copilot_root("usage-empty");
        let sessions_dir = root.join("session-state");
        let uuid = "f3eca837-818f-44d7-9158-bf242901f960";
        write_copilot_session(&sessions_dir, "dir-1", uuid, "hello");
        let usage_db = root.join("session-store.db");
        write_usage_schema(&usage_db);

        let store = setup_store();
        let result = scan_for_sync_impl(
            &sessions_dir,
            &AdapterSyncContext::from_store_for_test(&store, "copilot-cli").unwrap(),
            None,
            true,
            Some(&usage_db),
        )
        .unwrap();
        assert_eq!(result.sessions[0].usage_parser_version, Some(USAGE_PARSER_VERSION));
        assert!(result.sessions[0].usage_events.is_empty());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_for_sync_imports_usage_after_missing_db_appears() {
        let root = temp_copilot_root("usage-late");
        let sessions_dir = root.join("session-state");
        let uuid = "f3eca837-818f-44d7-9158-bf242901f960";
        let events_path = write_copilot_session(&sessions_dir, "dir-1", uuid, "hello");
        let mtime = file_scan::stat_mtime_ms(&events_path).unwrap();

        let store = setup_store();
        store.insert_session(&make_existing_session(uuid, mtime, 1)).unwrap();
        store
            .persist_session_events_for_existing_session(
                "copilot-cli",
                uuid,
                &[],
                EVENT_PARSER_VERSION,
                Some(mtime),
            )
            .unwrap();

        store
            .persist_topology_for_existing_session(
                "copilot-cli",
                uuid,
                &crate::db::store::SessionTopologyWrite {
                    thread_role: None,
                    parents: &[],
                    parser_version: Some(METADATA_PARSER_VERSION),
                },
            )
            .unwrap();
        let skipped = scan_for_sync_impl(
            &sessions_dir,
            &AdapterSyncContext::from_store_for_test(&store, "copilot-cli").unwrap(),
            None,
            true,
            None,
        )
        .unwrap();
        assert_eq!(skipped.sessions.len(), 0);
        assert_eq!(skipped.stats.skipped_sessions, 1);

        let usage_db = root.join("session-store.db");
        write_usage_db(&usage_db, uuid, "2020-01-01T00:00:00.000Z");

        let result = scan_for_sync_impl(
            &sessions_dir,
            &AdapterSyncContext::from_store_for_test(&store, "copilot-cli").unwrap(),
            None,
            true,
            Some(&usage_db),
        )
        .unwrap();
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].usage_events.len(), 1);
        assert_eq!(result.sessions[0].usage_events[0].input_tokens, 95);

        let _ = fs::remove_dir_all(&root);
    }
}
