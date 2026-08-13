use std::{
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
    adapters::{
        RawMessage, RawSession, ResumeCommand, SourceAdapter, SyncScanResult,
        file_scan::{self, FileScanEntry},
        usage::usage_count,
    },
    db::store::Store,
    types::{RawUsageEvent, Role},
};

const DSH_SESSION_FORMAT_VERSION: i64 = 0;
const USAGE_PARSER_VERSION: u32 = 1;

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
            let Some(mtime_ms) = file_scan::stat_mtime_ms(&entry.stat_target) else {
                continue;
            };
            if let Some(session) = parse_dsh_session_for_entry(entry, mtime_ms)? {
                sessions.push(session);
            }
        }
        Ok(sessions)
    }

    fn scan_for_sync(
        &self,
        store: &Store,
        since_ts: Option<i64>,
        _include_events: bool,
    ) -> Result<Option<SyncScanResult>> {
        let Some(sessions_dir) = resolve_dsh_sessions_dir()? else {
            return Ok(Some(SyncScanResult { sessions: Vec::new(), stats: Default::default() }));
        };

        Ok(Some(file_scan::run_file_scan_with_options(
            store,
            self.id(),
            since_ts,
            file_scan::FileScanOptions {
                usage_parser_version: Some(USAGE_PARSER_VERSION),
                event_parser_version: None,
                metadata_parser_version: None,
            },
            collect_dsh_entries(&sessions_dir),
            parse_dsh_session_for_entry,
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

fn parse_dsh_session_for_entry(entry: FileScanEntry, mtime_ms: i64) -> Result<Option<RawSession>> {
    let parsed = match parse_dsh_session(&entry.stat_target) {
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
    if parsed.messages.is_empty() && parsed.usage_events.is_empty() {
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
    session.source_file_path = entry.stat_target.to_str().map(str::to_string);
    session.custom_title = parsed.title;
    Ok(Some(session))
}

fn parse_dsh_session(path: &Path) -> Result<ParsedDshSession> {
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

    for (line_index, line) in lines.enumerate() {
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                warn!(
                    "stopped at unreadable DeepSeek Harness tail {}:{}: {error}",
                    path.display(),
                    line_index + 2
                );
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let event: Value = match serde_json::from_str(&line) {
            Ok(event) => event,
            Err(error) => {
                warn!(
                    "stopped at invalid DeepSeek Harness tail {}:{}: {error}",
                    path.display(),
                    line_index + 2
                );
                break;
            }
        };
        let timestamp = event.get("time").and_then(Value::as_i64);
        let event_seq = event
            .get("seq")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(line_index as u32);

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
            Some("assistant/message")
                if event.pointer("/data/message/role").and_then(Value::as_str)
                    == Some("assistant") =>
            {
                let content = text_content(event.pointer("/data/message/content"));
                let message_seq = content.as_ref().map(|_| messages.len() as u32);
                let provider = event
                    .pointer("/data/message/source/provider")
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                    .map(str::to_string)
                    .or_else(|| current_provider.clone());
                let model = event
                    .pointer("/data/message/source/model")
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

    Ok(ParsedDshSession { id, cwd, created_at, title, messages, usage_events })
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
                            {"type": "tool-call", "name": "bash"}
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
            json!({"type": "session/title", "data": {"title": "Conversation title"}}),
        ];
        let header = records[0].to_string() + "\n";
        let body = records[1..].iter().map(Value::to_string).collect::<Vec<_>>().join("\n")
            + "\n{\"type\":\"assistant/message\"";
        let mut compressed = zstd::stream::encode_all(header.as_bytes(), 0).unwrap();
        compressed.extend(zstd::stream::encode_all(body.as_bytes(), 0).unwrap());
        fs::write(&path, compressed).unwrap();

        let entries = collect_dsh_entries(root.path());
        assert_eq!(entries.len(), 1);
        let session = parse_dsh_session_for_entry(entries.into_iter().next().unwrap(), 2000)
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
    }
}
