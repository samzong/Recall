use std::{
    collections::HashMap,
    env,
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tracing::{debug, warn};
use walkdir::WalkDir;

use crate::{
    adapters::AdapterSyncContext,
    adapters::{
        RawMessage, RawSession, ResumeCommand, SourceAdapter, SyncScanResult,
        events::{EventContext, shell_file_evidence, tool_call_event, tool_result_event},
        file_scan::{self, FileScanEntry},
        usage::usage_count,
    },
    types::{FileEvidence, FileEvidenceKind, FileOperation, RawSessionEvent, RawUsageEvent, Role},
};

const DSH_SESSION_FORMAT_VERSION: i64 = 0;
const USAGE_PARSER_VERSION: u32 = 2;
const METADATA_PARSER_VERSION: u32 = 1;
const EVENT_PARSER_VERSION: u32 = 1;

pub(crate) struct DeepSeekHarnessAdapter;

impl SourceAdapter for DeepSeekHarnessAdapter {
    fn id(&self) -> &str {
        "deepseek-harness"
    }

    fn label(&self) -> &str {
        "DSH"
    }

    fn usage_parser_version(&self) -> Option<u32> {
        Some(USAGE_PARSER_VERSION)
    }

    fn scan(&self) -> Result<Vec<RawSession>> {
        let Some(sessions_dir) = resolve_dsh_sessions_dir()? else {
            return Ok(Vec::new());
        };

        let mut sessions = Vec::new();
        for entry in collect_dsh_entries(&sessions_dir) {
            let Some(snapshot) = dsh_snapshot(&entry) else {
                continue;
            };
            if let Some(session) =
                parse_dsh_session_for_entry(entry.clone(), snapshot.effective_mtime_ms(), true)?
                && dsh_snapshot(&entry).as_ref() == Some(&snapshot)
            {
                sessions.push(session);
            }
        }
        Ok(sessions)
    }

    fn scan_for_sync(
        &self,
        context: &AdapterSyncContext,
        since_ts: Option<i64>,
        include_events: bool,
    ) -> Result<Option<SyncScanResult>> {
        let Some(sessions_dir) = resolve_dsh_sessions_dir()? else {
            return Ok(Some(SyncScanResult {
                sessions: Vec::new(),
                stats: Default::default(),
                observations: Vec::new(),
            }));
        };

        Ok(Some(file_scan::run_file_scan_with_options_and_snapshot(
            context,
            since_ts,
            file_scan::FileScanOptions {
                usage_parser_version: Some(USAGE_PARSER_VERSION),
                event_parser_version: include_events.then_some(EVENT_PARSER_VERSION),
                metadata_parser_version: include_events.then_some(METADATA_PARSER_VERSION),
            },
            collect_dsh_entries(&sessions_dir),
            dsh_snapshot,
            |entry, mtime| parse_dsh_session_for_entry(entry, mtime, include_events),
        )?))
    }

    fn resume_command(&self, _source_id: &str) -> Option<ResumeCommand> {
        None
    }
}

struct ParsedDshSession {
    id: String,
    cwd: Option<String>,
    created_at: i64,
    title: Option<String>,
    messages: Vec<RawMessage>,
    usage_events: Vec<RawUsageEvent>,
    events: Vec<RawSessionEvent>,
}

fn resolve_dsh_sessions_dir() -> Result<Option<PathBuf>> {
    let root = match env::var_os("DSH_HOME").filter(|value| !value.is_empty()) {
        Some(value) => expand_home(PathBuf::from(value))?,
        None => dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home dir"))?.join(".dsh"),
    };
    let sessions_dir = root.join("sessions");
    if !sessions_dir.exists() {
        debug!("DeepSeek Harness sessions directory not found");
        return Ok(None);
    }
    Ok(Some(sessions_dir))
}

fn expand_home(path: PathBuf) -> Result<PathBuf> {
    let Ok(relative) = path.strip_prefix("~") else {
        return Ok(path);
    };
    Ok(dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home dir"))?.join(relative))
}

fn collect_dsh_entries(sessions_dir: &Path) -> Vec<FileScanEntry> {
    WalkDir::new(sessions_dir)
        .min_depth(3)
        .max_depth(3)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry.file_type().is_file()
                && matches!(
                    entry.file_name().to_str(),
                    Some("session.jsonl" | "session.jsonl.zstd")
                )
        })
        .filter_map(|entry| {
            let path = entry.into_path();
            let session_id = decode_dsh_session_id(path.parent()?.file_name()?.to_str()?)?;
            Some(FileScanEntry { session_id, stat_target: path, directory: None })
        })
        .collect()
}

fn decode_dsh_session_id(encoded: &str) -> Option<String> {
    let bytes = encoded.as_bytes();
    let mut units = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'~' {
            let hex = std::str::from_utf8(bytes.get(index + 1..index + 5)?).ok()?;
            units.push(u16::from_str_radix(hex, 16).ok()?);
            index += 5;
        } else {
            if !byte.is_ascii_alphanumeric() && !matches!(byte, b'.' | b'_' | b'-') {
                return None;
            }
            units.push(byte.into());
            index += 1;
        }
    }
    String::from_utf16(&units).ok()
}

fn dsh_snapshot(
    entry: &FileScanEntry,
) -> Option<file_scan::FileScanSnapshot<file_scan::FileMetadataSnapshot>> {
    let fingerprint = file_scan::file_metadata_snapshot(&entry.stat_target)?;
    Some(file_scan::FileScanSnapshot::new(fingerprint.mtime_ms()?, fingerprint))
}

fn parse_dsh_session_for_entry(
    entry: FileScanEntry,
    mtime_ms: i64,
    include_events: bool,
) -> Result<Option<RawSession>> {
    let parsed = match parse_dsh_session(&entry.stat_target, include_events) {
        Ok(parsed) => parsed,
        Err(error) => {
            warn!(
                "failed to parse DeepSeek Harness session {}: {error}",
                entry.stat_target.display()
            );
            return Ok(None);
        }
    };

    if parsed.id != entry.session_id {
        warn!("DeepSeek Harness session id mismatch in {}", entry.stat_target.display());
        return Ok(None);
    }
    if parsed.messages.is_empty() && parsed.usage_events.is_empty() && parsed.events.is_empty() {
        return Ok(None);
    }

    let mut session = RawSession::search_only(
        parsed.id,
        parsed.cwd,
        parsed.created_at,
        Some(mtime_ms),
        None,
        parsed.messages,
    )
    .with_usage(parsed.usage_events, USAGE_PARSER_VERSION);
    if include_events {
        session = session.with_events(parsed.events, EVENT_PARSER_VERSION);
    }
    session.source_file_path = entry.stat_target.to_str().map(str::to_string);
    session.custom_title = parsed.title;
    session.metadata_parser_version = Some(METADATA_PARSER_VERSION);
    session.refresh_session_on_metadata_backfill = true;
    Ok(Some(session))
}

fn parse_dsh_session(path: &Path, include_events: bool) -> Result<ParsedDshSession> {
    let mut lines = open_dsh_log(path)?.lines();
    let header_line = lines.next().transpose()?.context("empty session log")?;
    let header: Value = serde_json::from_str(&header_line).context("invalid session header")?;

    if header.get("type").and_then(Value::as_str) != Some("session") {
        bail!("first line is not a session header");
    }
    let version =
        header.get("version").and_then(Value::as_i64).context("missing format version")?;
    if version != DSH_SESSION_FORMAT_VERSION {
        bail!("unsupported session format version {version}");
    }

    let id = header
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .context("missing session id")?
        .to_string();
    let created_at =
        header.get("createdAt").and_then(Value::as_i64).context("missing creation time")?;
    let cwd = header.get("cwd").and_then(Value::as_str).map(str::to_string);
    let mut title = None;
    let mut current_provider = None;
    let mut current_model = None;
    let mut last_usage_sample = None;
    let mut messages = Vec::new();
    let mut usage_events = Vec::new();
    let mut events: Vec<RawSessionEvent> = Vec::new();
    let mut call_indices = HashMap::new();

    for (line_index, line) in lines.enumerate() {
        let line =
            line.with_context(|| format!("unreadable session record at line {}", line_index + 2))?;
        if line.trim().is_empty() {
            continue;
        }
        let event: Value = serde_json::from_str(&line)
            .with_context(|| format!("invalid session record at line {}", line_index + 2))?;
        let timestamp = event.get("time").and_then(Value::as_i64);
        let event_seq = event
            .get("seq")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(line_index as u32);

        let assistant_message = if event.get("type").and_then(Value::as_str)
            == Some("assistant/message")
        {
            match event.pointer("/data/message") {
                Some(message) => (message.get("role").and_then(Value::as_str) == Some("assistant"))
                    .then_some((message, message.get("source"))),
                None => event
                    .get("data")
                    .filter(|data| {
                        data.get("content").is_some() && data.get("provenance").is_some()
                    })
                    .map(|data| (data, data.get("provenance"))),
            }
        } else {
            None
        };

        if include_events {
            let event_type = event.get("type").and_then(Value::as_str);
            let parts: Vec<(usize, &Value)> = match event_type {
                Some("tool/call") => event.get("data").map(|data| (0, data)).into_iter().collect(),
                Some("assistant/message") => assistant_message
                    .and_then(|(message, _)| message.get("content"))
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .enumerate()
                    .filter(|(_, part)| {
                        part.get("type").and_then(Value::as_str) == Some("tool-call")
                    })
                    .collect(),
                Some("tool/result") if event.pointer("/data/message").is_some() => event
                    .pointer("/data/message/content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .enumerate()
                    .filter(|(_, part)| {
                        part.get("type").and_then(Value::as_str) == Some("tool-result")
                    })
                    .collect(),
                Some("tool/result") => {
                    event.get("data").map(|data| (0, data)).into_iter().collect()
                }
                _ => Vec::new(),
            };
            for (part_index, part) in parts {
                let context = EventContext {
                    event_seq: events.len() as u32,
                    timestamp,
                    source_path: path.to_str().map(str::to_string),
                    source_event_id: Some(format!("line:{}:part:{part_index}", line_index + 1)),
                    message_seq: messages
                        .len()
                        .checked_sub(1)
                        .and_then(|index| u32::try_from(index).ok()),
                    parser_version: EVENT_PARSER_VERSION,
                };
                let is_result = event_type == Some("tool/result");
                let mut parsed = if is_result {
                    let mut parsed =
                        tool_result_event(context, None, part.get("content").map(Value::to_string));
                    parsed.tool_call_id = part
                        .get("toolCallId")
                        .or_else(|| part.get("callId"))
                        .or_else(|| event.pointer("/data/message/source/callId"))
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty())
                        .map(str::to_string);
                    parsed.status = match event.pointer("/data/error/code").and_then(Value::as_str)
                    {
                        Some("TOOL_OUTCOME_UNKNOWN") => None,
                        Some("TOOL_NOT_STARTED") => Some("not_started".to_string()),
                        _ => part
                            .get("isError")
                            .and_then(Value::as_bool)
                            .map(|error| if error { "error" } else { "success" }.to_string()),
                    };
                    parsed
                } else {
                    let Some(name) = part
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|name| !name.trim().is_empty())
                    else {
                        continue;
                    };
                    let args = part
                        .get("arguments")
                        .and_then(Value::as_str)
                        .and_then(|args| serde_json::from_str::<Value>(args).ok());
                    let mut parsed = tool_call_event(context, name.to_string(), args.as_ref());
                    parsed.kind = "tool_call".to_string();
                    parsed.target = None;
                    parsed.tool_call_id = part
                        .get("callId")
                        .or_else(|| part.get("id"))
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty())
                        .map(str::to_string);
                    let operation = match name {
                        "read" => Some(FileOperation::Read),
                        "edit" | "write" => Some(FileOperation::Write),
                        _ => None,
                    };
                    if let (Some(operation), Some(file_path)) = (
                        operation,
                        args.as_ref()
                            .and_then(|args| args.get("file_path"))
                            .and_then(Value::as_str)
                            .filter(|path| !path.trim().is_empty()),
                    ) {
                        parsed.kind = if operation == FileOperation::Read {
                            "file_read"
                        } else {
                            "file_write"
                        }
                        .to_string();
                        parsed.target = Some(file_path.to_string());
                        parsed.files.push(FileEvidence {
                            path: file_path.to_string(),
                            operation,
                            kind: FileEvidenceKind::Call,
                            cwd: cwd.clone(),
                            target: None,
                        });
                    }
                    if name == "bash" {
                        parsed.kind = "command".to_string();
                        parsed.target = args
                            .as_ref()
                            .and_then(|args| args.get("command"))
                            .and_then(Value::as_str)
                            .map(str::to_string);
                        if let Some(command) = parsed.target.as_deref() {
                            let workdir = args
                                .as_ref()
                                .and_then(|args| args.get("workdir"))
                                .and_then(Value::as_str);
                            let command_cwd = match workdir {
                                Some(workdir) if Path::new(workdir).is_absolute() => {
                                    Some(workdir.to_string())
                                }
                                Some(workdir) => cwd
                                    .as_deref()
                                    .filter(|cwd| Path::new(cwd).is_absolute())
                                    .and_then(|cwd| {
                                        Path::new(cwd).join(workdir).to_str().map(str::to_string)
                                    }),
                                None => cwd.clone(),
                            };
                            let (files, status) =
                                shell_file_evidence(command, command_cwd.as_deref());
                            parsed.files = files;
                            parsed.command_evidence_status = Some(status);
                        }
                    }
                    parsed
                };
                parsed.attrs_json = Some(event.to_string());
                if !is_result
                    && let (Some((turn, step)), Some(call_id)) =
                        (dsh_turn_step(&event), parsed.tool_call_id.as_ref())
                {
                    let key = (turn, step, call_id.clone());
                    if let Some(&index) = call_indices.get(&key) {
                        let previous: &mut RawSessionEvent = &mut events[index];
                        let records = serde_json::json!({"records": [previous.attrs_json.as_deref().and_then(|raw| serde_json::from_str::<Value>(raw).ok()), event]});
                        if event_type == Some("tool/call") {
                            parsed.event_seq = previous.event_seq;
                            *previous = parsed;
                        }
                        previous.attrs_json = Some(records.to_string());
                        continue;
                    }
                    call_indices.insert(key, events.len());
                }
                events.push(parsed);
            }
        }
        match event.get("type").and_then(Value::as_str) {
            Some("request/header") => {
                current_provider = event
                    .pointer("/data/header/config/provider")
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                    .map(str::to_string)
                    .or(current_provider);
                current_model = event
                    .pointer("/data/header/config/model")
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                    .map(str::to_string)
                    .or(current_model);
            }
            Some("user/message")
                if event.pointer("/data/source/kind").and_then(Value::as_str) == Some("user") =>
            {
                if let Some(content) = text_content(event.pointer("/data/content")) {
                    messages.push(RawMessage { role: Role::User, content, timestamp });
                }
            }
            Some("assistant/chunk")
                if event.pointer("/data/chunk/type").and_then(Value::as_str) == Some("usage") =>
            {
                if let (Some(usage), Some((turn, step))) =
                    (event.pointer("/data/chunk/usage"), dsh_turn_step(&event))
                {
                    let usage_event = extract_dsh_usage_event(
                        usage,
                        event_seq,
                        timestamp.unwrap_or(created_at),
                        None,
                        current_provider.as_deref(),
                        current_model.as_deref(),
                        turn,
                        step,
                        path,
                    );
                    record_dsh_usage_sample(
                        &mut usage_events,
                        &mut last_usage_sample,
                        turn,
                        step,
                        usage_event,
                    );
                }
            }
            Some("assistant/message") => {
                let Some((message, source)) = assistant_message else {
                    continue;
                };
                let content = text_content(message.get("content"));
                let message_seq = content.as_ref().map(|_| messages.len() as u32);
                let provider = source
                    .and_then(|source| source.get("provider"))
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                    .map(str::to_string)
                    .or_else(|| current_provider.clone());
                let model = source
                    .and_then(|source| source.get("model"))
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                    .map(str::to_string)
                    .or_else(|| current_model.clone());
                current_provider.clone_from(&provider);
                current_model.clone_from(&model);

                if let (Some(usage), Some((turn, step))) =
                    (event.pointer("/data/usage"), dsh_turn_step(&event))
                {
                    let usage_event = extract_dsh_usage_event(
                        usage,
                        event_seq,
                        timestamp.unwrap_or(created_at),
                        message_seq,
                        provider.as_deref(),
                        model.as_deref(),
                        turn,
                        step,
                        path,
                    );
                    record_dsh_usage_sample(
                        &mut usage_events,
                        &mut last_usage_sample,
                        turn,
                        step,
                        usage_event,
                    );
                }
                if let Some(content) = content {
                    messages.push(RawMessage { role: Role::Assistant, content, timestamp });
                }
            }
            Some("session/title") => {
                title = event
                    .pointer("/data/title")
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                    .map(str::to_string)
                    .or(title);
            }
            _ => {}
        }
    }

    Ok(ParsedDshSession { id, cwd, created_at, title, messages, usage_events, events })
}

fn open_dsh_log(path: &Path) -> Result<Box<dyn BufRead>> {
    let file = File::open(path)?;
    match path.file_name().and_then(|name| name.to_str()) {
        Some("session.jsonl") => Ok(Box::new(BufReader::new(file))),
        Some("session.jsonl.zstd") => {
            let decoder = zstd::stream::read::Decoder::new(file)?;
            Ok(Box::new(BufReader::new(decoder)))
        }
        _ => bail!("unsupported session log filename"),
    }
}

fn text_content(value: Option<&Value>) -> Option<String> {
    let content = value?
        .as_array()?
        .iter()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    (!content.is_empty()).then_some(content)
}

#[allow(clippy::too_many_arguments)]
fn extract_dsh_usage_event(
    usage: &Value,
    event_seq: u32,
    timestamp: i64,
    message_seq: Option<u32>,
    provider: Option<&str>,
    model: Option<&str>,
    turn: u32,
    step: u32,
    path: &Path,
) -> RawUsageEvent {
    let reasoning_tokens = usage_count(usage, &["reasoningTokens"]);
    // DSH reports reasoning inside outputTokens; Recall stores disjoint buckets.
    let output_tokens = usage_count(usage, &["outputTokens"]).saturating_sub(reasoning_tokens);

    RawUsageEvent {
        message_seq,
        model: model.filter(|value| !value.trim().is_empty()).unwrap_or("unknown").to_string(),
        provider: provider
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("unknown")
            .to_string(),
        input_tokens: usage_count(usage, &["inputTokens"]),
        output_tokens,
        cache_read_tokens: usage_count(usage, &["cacheReadTokens"]),
        cache_write_tokens: usage_count(usage, &["cacheWriteTokens"]),
        reasoning_tokens,
        source_path: path.to_str().map(str::to_string),
        raw_usage_json: Some(usage.to_string()),
        ..RawUsageEvent::observed(
            format!("step:{turn}:{step}"),
            event_seq,
            timestamp,
            USAGE_PARSER_VERSION,
        )
    }
}

fn record_dsh_usage_sample(
    usage_events: &mut Vec<RawUsageEvent>,
    last_sample: &mut Option<(u32, u32, usize)>,
    turn: u32,
    step: u32,
    event: RawUsageEvent,
) {
    if let Some((last_turn, last_step, index)) = *last_sample
        && (last_turn, last_step) == (turn, step)
    {
        usage_events[index] = event;
        return;
    }

    let index = usage_events.len();
    usage_events.push(event);
    *last_sample = Some((turn, step, index));
}

fn dsh_turn_step(event: &Value) -> Option<(u32, u32)> {
    Some((
        u32::try_from(event.pointer("/data/turn")?.as_u64()?).ok()?,
        u32::try_from(event.pointer("/data/step")?.as_u64()?).ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn parses_multiframe_zstd_with_encoded_id_and_filters_injected_context() {
        let root = tempdir().unwrap();
        let session_dir = root.path().join("--project--").join("session~002Ftest");
        fs::create_dir_all(&session_dir).unwrap();
        let path = session_dir.join("session.jsonl.zstd");
        let records = [
            json!({
                "type": "session",
                "version": 0,
                "id": "session/test",
                "cwd": "/work/project",
                "createdAt": 1000,
                "delegationDepth": 0
            }),
            json!({
                "type": "request/header",
                "seq": 0,
                "time": 1050,
                "data": {"header": {"config": {
                    "provider": "deepseek-official",
                    "model": "deepseek-v4-pro"
                }}}
            }),
            json!({
                "type": "user/message",
                "seq": 1,
                "time": 1100,
                "data": {
                    "role": "user",
                    "source": {"kind": "user"},
                    "content": [{"type": "text", "text": "real question"}]
                }
            }),
            json!({
                "type": "user/message",
                "seq": 2,
                "time": 1200,
                "data": {
                    "role": "user",
                    "source": {"kind": "plugin"},
                    "content": [{"type": "text", "text": "injected instructions"}]
                }
            }),
            json!({
                "type": "assistant/chunk",
                "seq": 3,
                "time": 1290,
                "data": {
                    "turn": 1,
                    "step": 1,
                    "chunk": {"type": "usage", "usage": {
                        "inputTokens": 100,
                        "outputTokens": 40,
                        "cacheReadTokens": 25,
                        "reasoningTokens": 15
                    }}
                }
            }),
            json!({
                "type": "assistant/message",
                "seq": 4,
                "time": 1300,
                "data": {
                    "turn": 1,
                    "step": 1,
                    "message": {
                        "role": "assistant",
                        "source": {
                            "kind": "model",
                            "provider": "deepseek-official",
                            "model": "deepseek-v4-pro"
                        },
                        "content": [
                            {"type": "reasoning", "text": "private reasoning"},
                            {"type": "text", "text": "final answer"},
                            {"type": "tool-call", "id": "read-1", "name": "read", "arguments": "{\"file_path\":\"src/auth.rs\"}"}
                        ]
                    },
                    "usage": {
                        "inputTokens": 100,
                        "outputTokens": 40,
                        "cacheReadTokens": 25,
                        "reasoningTokens": 15
                    }
                }
            }),
            json!({
                "type": "request/header",
                "seq": 5,
                "time": 1350,
                "data": {"header": {"config": {
                    "provider": "deepseek-official",
                    "model": "deepseek-v4-flash"
                }}}
            }),
            json!({
                "type": "assistant/chunk",
                "seq": 6,
                "time": 1400,
                "data": {
                    "turn": 1,
                    "step": 2,
                    "chunk": {"type": "usage", "usage": {
                        "inputTokens": 10,
                        "outputTokens": 5,
                        "cacheReadTokens": 0,
                        "reasoningTokens": 5
                    }}
                }
            }),
            json!({"type": "tool/call", "seq": 7, "time": 1500, "data": {"turn": 1, "step": 1, "callId": "read-1", "name": "read", "arguments": "{\"file_path\":\"src/auth.rs\"}"}}),
            json!({"type": "tool/result", "seq": 8, "time": 1600, "data": {"turn": 1, "step": 1, "callId": "read-1", "content": [{"type": "text", "text": "file contents"}], "isError": false}}),
            json!({"type": "tool/call", "seq": 9, "time": 1700, "data": {"turn": 2, "step": 1, "callId": "edit-1", "name": "edit", "arguments": "{\"file_path\":\"src/auth.rs\",\"old_string\":\"old\",\"new_string\":\"new\"}"}}),
            json!({"type": "tool/result", "seq": 10, "time": 1800, "data": {"turn": 2, "step": 1, "error": {"code": "TOOL_OUTCOME_UNKNOWN", "name": "ToolOutcomeUnknownError"}, "message": {"id": "message-result", "role": "user", "source": {"kind": "tool", "callId": "edit-1"}, "content": [{"type": "tool-result", "toolCallId": "edit-1", "isError": true, "content": [{"type": "text", "text": "outcome unknown"}]}]}}}),
            json!({"type": "session/title", "data": {"title": "Conversation title"}}),
        ];
        let header = records[0].to_string() + "\n";
        let body = records[1..].iter().map(Value::to_string).collect::<Vec<_>>().join("\n") + "\n";
        let mut compressed = zstd::stream::encode_all(header.as_bytes(), 0).unwrap();
        compressed.extend(zstd::stream::encode_all(body.as_bytes(), 0).unwrap());
        fs::write(&path, compressed).unwrap();

        let entries = collect_dsh_entries(root.path());
        assert_eq!(entries.len(), 1);
        let session = parse_dsh_session_for_entry(entries.into_iter().next().unwrap(), 2000, true)
            .unwrap()
            .unwrap();

        assert_eq!(session.source_id, "session/test");
        assert_eq!(session.directory.as_deref(), Some("/work/project"));
        assert_eq!(session.started_at, 1000);
        assert_eq!(session.updated_at, Some(2000));
        assert_eq!(session.custom_title.as_deref(), Some("Conversation title"));
        assert_eq!(session.messages.len(), 2);
        assert!(matches!(session.messages[0].role, Role::User));
        assert_eq!(session.messages[0].content, "real question");
        assert_eq!(session.messages[0].timestamp, Some(1100));
        assert!(matches!(session.messages[1].role, Role::Assistant));
        assert_eq!(session.messages[1].content, "final answer");
        assert_eq!(session.messages[1].timestamp, Some(1300));

        assert_eq!(session.events.len(), 4);
        assert_eq!(session.events[0].tool_call_id.as_deref(), Some("read-1"));
        assert_eq!(session.events[0].files[0].operation, FileOperation::Read);
        assert_eq!(session.events[0].message_seq, Some(1));
        assert!(session.events[0].attrs_json.as_deref().unwrap().contains("records"));
        assert_eq!(session.events[1].status.as_deref(), Some("success"));
        assert!(session.events[1].files.is_empty());
        assert_eq!(session.events[2].files[0].cwd.as_deref(), Some("/work/project"));
        assert_eq!(session.events[2].files[0].operation, FileOperation::Write);
        assert_eq!(session.events[3].tool_call_id.as_deref(), Some("edit-1"));
        assert!(session.events[3].status.is_none());
        assert!(session.events[3].files.is_empty());
        let plain = session_dir.join("session.jsonl");
        fs::write(&plain, header.clone() + &body).unwrap();
        let plain_parsed = parse_dsh_session(&plain, true).unwrap();
        assert_eq!(plain_parsed.events.len(), session.events.len());
        assert!(parse_dsh_session(&plain, false).unwrap().events.is_empty());
        let mut legacy_records = records.clone();
        for record in &mut legacy_records {
            if record.get("type").and_then(Value::as_str) != Some("assistant/message") {
                continue;
            }
            let data = record.get_mut("data").unwrap().as_object_mut().unwrap();
            let mut message = data.remove("message").unwrap();
            data.insert("content".to_string(), message.get_mut("content").unwrap().take());
            data.insert("provenance".to_string(), message.get_mut("source").unwrap().take());
        }
        legacy_records.last_mut().unwrap().clone_from(&json!({
            "type": "assistant/message", "seq": 11, "time": 1900,
            "data": {
                "turn": 3, "step": 1,
                "content": [
                    {"type": "text", "text": "why this edit"},
                    {"type": "tool-call", "id": "orphan-write", "name": "write",
                     "arguments": "{\"file_path\":\"src/new.rs\",\"content\":\"new body\"}"}
                ],
                "provenance": {"provider": "deepseek-official", "model": "deepseek-v4-pro"}
            },
            "surfaceOp": "append"
        }));
        let legacy = legacy_records.iter().map(Value::to_string).collect::<Vec<_>>().join("\n");
        for legacy_path in [&plain, &path] {
            if legacy_path == &path {
                fs::write(legacy_path, zstd::stream::encode_all(legacy.as_bytes(), 0).unwrap())
                    .unwrap();
            } else {
                fs::write(legacy_path, &legacy).unwrap();
            }
            let parsed = parse_dsh_session(legacy_path, true).unwrap();
            assert_eq!(
                parsed.messages.iter().map(|message| message.content.as_str()).collect::<Vec<_>>(),
                ["real question", "final answer", "why this edit"]
            );
            assert_eq!(parsed.events.len(), 5);
            assert_eq!(parsed.usage_events[0].message_seq, Some(1));
            assert_eq!(parsed.usage_events[0].model, "deepseek-v4-pro");
            assert_eq!(parsed.usage_events[0].input_tokens, 100);
            let records: Value =
                serde_json::from_str(parsed.events[0].attrs_json.as_deref().unwrap()).unwrap();
            assert_eq!(records["records"][0], legacy_records[5]);
            let orphan = parsed.events.last().unwrap();
            assert_eq!(orphan.tool_call_id.as_deref(), Some("orphan-write"));
            assert_eq!(orphan.message_seq, Some(1));
            assert_eq!(orphan.files[0].path, "src/new.rs");
            assert_eq!(orphan.files[0].operation, FileOperation::Write);
            assert!(orphan.status.is_none());
            assert_eq!(
                serde_json::from_str::<Value>(orphan.attrs_json.as_deref().unwrap()).unwrap(),
                *legacy_records.last().unwrap()
            );
        }
        fs::write(&plain, header.clone() + &body + "{\"type\":").unwrap();
        assert!(parse_dsh_session(&plain, true).is_err());
        assert!(
            parse_dsh_session_for_entry(
                FileScanEntry {
                    session_id: "session/test".into(),
                    stat_target: plain.clone(),
                    directory: None
                },
                2000,
                true
            )
            .unwrap()
            .is_none()
        );
        let mut broken = (header.clone() + &body).into_bytes();
        broken.push(0xff);
        fs::write(&plain, broken).unwrap();
        assert!(parse_dsh_session(&plain, true).is_err());
        for (workdir, expected) in [
            (Some("/other"), "/other"),
            (Some("nested"), "/work/project/nested"),
            (None, "/work/project"),
        ] {
            let args =
                serde_json::json!({"command":"git restore -- src/auth.rs","workdir":workdir});
            let call = serde_json::json!({"type":"tool/call","data":{"turn":1,"step":1,"callId":"shell","name":"bash","arguments":args.to_string()}});
            fs::write(&plain, header.clone() + &call.to_string()).unwrap();
            let parsed = parse_dsh_session(&plain, true).unwrap();
            assert!(parsed.messages.is_empty());
            assert_eq!(parsed.events[0].kind, "command");
            assert_eq!(parsed.events[0].files[0].path, "src/auth.rs");
            assert_eq!(parsed.events[0].files[0].cwd.as_deref(), Some(expected));
            assert_eq!(parsed.events[0].files[0].kind, FileEvidenceKind::Command);
        }
        let tool_only_dir = root.path().join("tool-only");
        fs::create_dir_all(&tool_only_dir).unwrap();
        let tool_only = tool_only_dir.join("session.jsonl");
        fs::write(&tool_only, header.clone() + &serde_json::json!({"type": "assistant/message", "data": {"turn": 1, "step": 1, "content": [{"type": "tool-call", "id": "write-1", "name": "write", "arguments": "{\"file_path\":\"new.rs\",\"content\":\"new contents\"}"}], "provenance": {"provider": "mock", "model": "mock"}}}).to_string()).unwrap();
        let only = parse_dsh_session_for_entry(
            FileScanEntry {
                session_id: "session/test".to_string(),
                stat_target: tool_only,
                directory: None,
            },
            2000,
            true,
        )
        .unwrap()
        .unwrap();
        assert!(only.messages.is_empty());
        assert!(only.usage_events.is_empty());
        assert_eq!(only.events[0].files[0].operation, FileOperation::Write);
        assert!(only.events[0].message_seq.is_none());
        assert_eq!(session.event_parser_version, Some(EVENT_PARSER_VERSION));
        assert_eq!(session.usage_parser_version, Some(USAGE_PARSER_VERSION));
        assert_eq!(session.usage_events.len(), 2);
        let text_usage = &session.usage_events[0];
        assert_eq!(text_usage.event_key, "step:1:1");
        assert_eq!(text_usage.event_seq, 4);
        assert_eq!(text_usage.message_seq, Some(1));
        assert_eq!(text_usage.timestamp, 1300);
        assert_eq!(text_usage.provider, "deepseek-official");
        assert_eq!(text_usage.model, "deepseek-v4-pro");
        assert_eq!(text_usage.input_tokens, 100);
        assert_eq!(text_usage.output_tokens, 25);
        assert_eq!(text_usage.cache_read_tokens, 25);
        assert_eq!(text_usage.reasoning_tokens, 15);
        assert_eq!(text_usage.token_source, crate::types::TokenSource::Observed);

        let chunk_only_usage = &session.usage_events[1];
        assert_eq!(chunk_only_usage.event_key, "step:1:2");
        assert_eq!(chunk_only_usage.event_seq, 6);
        assert_eq!(chunk_only_usage.message_seq, None);
        assert_eq!(chunk_only_usage.provider, "deepseek-official");
        assert_eq!(chunk_only_usage.model, "deepseek-v4-flash");
        assert_eq!(chunk_only_usage.output_tokens, 0);
        assert_eq!(chunk_only_usage.reasoning_tokens, 5);
        crate::db::schema::register_sqlite_vec();
        let store = crate::db::store::Store::open_in_memory().unwrap();
        let mtime = file_scan::stat_mtime_ms(&plain).unwrap();
        store.conn.execute("INSERT INTO sessions (id, source, source_id, title, started_at, updated_at, message_count) VALUES ('stored-id', 'deepseek-harness', 'session/test', 'title', 1000, ?1, 2)", [mtime]).unwrap();
        store
            .persist_usage_events_for_existing_session(
                "deepseek-harness",
                "session/test",
                &[],
                USAGE_PARSER_VERSION,
                Some(mtime),
            )
            .unwrap();
        for (version, metadata_current) in [
            (None, false),
            (Some(EVENT_PARSER_VERSION - 1), false),
            (Some(EVENT_PARSER_VERSION), false),
            (Some(EVENT_PARSER_VERSION), true),
        ] {
            if metadata_current {
                store
                    .persist_topology_for_existing_session(
                        "deepseek-harness",
                        "session/test",
                        &crate::db::store::SessionTopologyWrite {
                            thread_role: None,
                            parents: &[],
                            parser_version: Some(METADATA_PARSER_VERSION),
                        },
                    )
                    .unwrap();
            }
            if let Some(version) = version {
                store
                    .persist_session_events_for_existing_session(
                        "deepseek-harness",
                        "session/test",
                        &[],
                        version,
                        Some(mtime),
                    )
                    .unwrap();
            }
            let result = file_scan::run_file_scan_with_options_and_snapshot(
                &AdapterSyncContext::from_store_for_test(&store, "deepseek-harness").unwrap(),
                None,
                file_scan::FileScanOptions {
                    usage_parser_version: Some(USAGE_PARSER_VERSION),
                    event_parser_version: Some(EVENT_PARSER_VERSION),
                    metadata_parser_version: Some(METADATA_PARSER_VERSION),
                },
                [FileScanEntry {
                    session_id: "session/test".to_string(),
                    stat_target: plain.clone(),
                    directory: None,
                }],
                dsh_snapshot,
                |entry, mtime| parse_dsh_session_for_entry(entry, mtime, true),
            )
            .unwrap();
            assert_eq!(result.sessions.len(), usize::from(!metadata_current));
            if let Some(session) = result.sessions.first() {
                assert!(session.refresh_session_on_metadata_backfill);
                assert_eq!(session.metadata_parser_version, Some(METADATA_PARSER_VERSION));
            }
        }
    }
}
