use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;
use tracing::{debug, warn};
use walkdir::WalkDir;

use crate::adapters::events::{EventContext, tool_call_event, tool_result_event};
use crate::adapters::file_scan::{self, FileScanEntry};
use crate::adapters::json_util::{json_i64, jsonl_indexed, rfc3339_ms};
use crate::adapters::usage::{disjoint_output_and_reasoning, usage_count};
use crate::adapters::{
    AdapterSyncContext, RawMessage, RawSession, SyncScanResult, first_timestamp,
};
use crate::types::{
    EvidenceVisibility, FileEvidence, FileOperation, ParentLink, ParentRelation, RawSessionEvent,
    RawUsageEvent, Role, ThreadRole,
};

pub(super) const METADATA_PARSER_VERSION: u32 = 2;
pub(super) const USAGE_PARSER_VERSION: u32 = 3;
pub(super) const EVENT_PARSER_VERSION: u32 = 1;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Format {
    Pi,
    Omp,
}

impl Format {
    fn source(self) -> &'static str {
        match self {
            Self::Pi => "pi",
            Self::Omp => "omp",
        }
    }
}

pub(super) fn scan(session_dirs: &[PathBuf], format: Format) -> anyhow::Result<Vec<RawSession>> {
    let mut sessions = Vec::new();
    for entry in collect_entries(session_dirs, format) {
        let Some(mtime_ms) = file_scan::stat_mtime_ms(&entry.stat_target) else {
            continue;
        };
        if let Some(raw) = parse_session_file(entry, mtime_ms, true, format)? {
            sessions.push(raw);
        }
    }
    Ok(sessions)
}

struct ParsedSession {
    session_id: Option<String>,
    cwd: Option<String>,
    started_at: Option<i64>,
    custom_title: Option<String>,
    messages: Vec<RawMessage>,
    usage_events: Vec<RawUsageEvent>,
    events: Vec<RawSessionEvent>,
    parent_session: Option<String>,
}

pub(super) fn push_existing_unique_dir(
    dirs: &mut Vec<PathBuf>,
    seen: &mut HashSet<String>,
    dir: PathBuf,
) {
    if !dir.exists() {
        return;
    }

    let key = fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone()).to_string_lossy().to_string();
    if seen.insert(key) {
        dirs.push(dir);
    }
}

pub(super) fn scan_for_sync(
    session_dirs: &[PathBuf],
    context: &AdapterSyncContext,
    since_ts: Option<i64>,
    include_events: bool,
    format: Format,
) -> anyhow::Result<SyncScanResult> {
    let entries = collect_entries(session_dirs, format);
    file_scan::run_file_scan_with_options(
        context,
        since_ts,
        file_scan::FileScanOptions {
            usage_parser_version: Some(USAGE_PARSER_VERSION),
            event_parser_version: include_events.then_some(EVENT_PARSER_VERSION),
            metadata_parser_version: include_events.then_some(METADATA_PARSER_VERSION),
        },
        entries,
        |entry, mtime_ms| parse_session_file(entry, mtime_ms, include_events, format),
    )
}

pub(super) fn collect_entries(session_dirs: &[PathBuf], format: Format) -> Vec<FileScanEntry> {
    let mut entries = Vec::new();
    let mut seen_files = HashSet::new();

    for session_dir in session_dirs {
        if !session_dir.exists() {
            continue;
        }

        for entry in WalkDir::new(session_dir).into_iter().filter_map(|entry| entry.ok()) {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            if !path.is_file() {
                continue;
            }

            let key = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            if !seen_files.insert(key.to_string_lossy().to_string()) {
                continue;
            }

            let stem = match path.file_stem().and_then(|stem| stem.to_str()) {
                Some(stem) if !stem.is_empty() => stem,
                _ => continue,
            };
            let session_id =
                extract_session_id_from_filename(stem).unwrap_or_else(|| stem.to_string());
            let directory = path
                .parent()
                .and_then(|parent| parent.file_name())
                .and_then(|name| name.to_str())
                .and_then(|name| decode_session_dir_name(name, format));

            entries.push(FileScanEntry { session_id, stat_target: path.to_path_buf(), directory });
        }
    }

    entries
}

pub(super) fn extract_session_id_from_filename(stem: &str) -> Option<String> {
    let candidate = stem.rsplit_once('_').map(|(_, tail)| tail).unwrap_or(stem);
    uuid::Uuid::try_parse(candidate).ok().map(|_| candidate.to_string())
}

fn normalize_parent_id(parent: &str) -> Option<String> {
    let stem = Path::new(parent).file_stem().and_then(|stem| stem.to_str()).unwrap_or(parent);
    extract_session_id_from_filename(stem)
}

pub(super) fn decode_session_dir_name(name: &str, format: Format) -> Option<String> {
    if format == Format::Omp && name == "-" {
        return dirs::home_dir().map(|home| home.to_string_lossy().into_owned());
    }
    let inner = name.strip_prefix("--")?.strip_suffix("--")?;
    if inner.is_empty() {
        return None;
    }
    Some(format!("/{}", inner.replace('-', "/")))
}

pub(super) fn parse_session_file(
    entry: FileScanEntry,
    mtime_ms: i64,
    include_events: bool,
    format: Format,
) -> anyhow::Result<Option<RawSession>> {
    let source_file_path = entry.stat_target.to_str().map(str::to_string);
    let parsed = match parse_session(&entry.stat_target, mtime_ms, include_events, format) {
        Ok(parsed) => parsed,
        Err(err) => {
            match format {
                Format::Pi => {
                    debug!("failed to parse Pi session {}: {err}", entry.stat_target.display())
                }
                Format::Omp => {
                    warn!("failed to parse OMP session {}: {err}", entry.stat_target.display())
                }
            }
            return Ok(None);
        }
    };

    if parsed.messages.is_empty() && parsed.usage_events.is_empty() && parsed.events.is_empty() {
        return Ok(None);
    }

    let started_at =
        first_timestamp(parsed.started_at, &parsed.messages, &parsed.usage_events, &parsed.events)
            .unwrap_or(0);

    let source_id = parsed.session_id.unwrap_or(entry.session_id);
    let parent_links = match parsed
        .parent_session
        .as_deref()
        .and_then(normalize_parent_id)
        .filter(|parent| parent != &source_id)
    {
        Some(parent) => vec![ParentLink {
            relation: ParentRelation::Fork,
            source: format.source().to_string(),
            source_id: parent,
        }],
        None => Vec::new(),
    };

    Ok(Some(RawSession {
        source_id,
        directory: parsed.cwd.or(entry.directory),
        started_at,
        updated_at: Some(mtime_ms),
        entrypoint: None,
        messages: parsed.messages,
        usage_events: parsed.usage_events,
        usage_parser_version: Some(USAGE_PARSER_VERSION),
        events: parsed.events,
        event_parser_version: include_events.then_some(EVENT_PARSER_VERSION),
        source_file_path,
        custom_title: parsed.custom_title,
        summary: None,
        duration_minutes: None,
        thread_role: Some(ThreadRole::Primary),
        parent_links,
        metadata_parser_version: Some(METADATA_PARSER_VERSION),
        refresh_session_on_metadata_backfill: true,
    }))
}

fn parse_session(
    path: &Path,
    fallback_timestamp: i64,
    include_events: bool,
    format: Format,
) -> anyhow::Result<ParsedSession> {
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let source_path = path.to_string_lossy().to_string();

    let mut session_id = None;
    let mut cwd = None;
    let mut started_at = None;
    let mut custom_title = None;
    let mut current_provider: Option<String> = None;
    let mut current_model: Option<String> = None;
    let mut inherited_usage_cutoff = None;
    let mut parent_session = None;
    let mut messages = Vec::new();
    let mut usage_events = Vec::new();
    let mut events = Vec::new();

    for item in jsonl_indexed(reader.lines()) {
        let (line_index, entry) = item?;

        match entry.get("type").and_then(|value| value.as_str()).unwrap_or("") {
            "title" if format == Format::Omp => {
                if custom_title.is_none() {
                    custom_title = non_empty_str(entry.get("title")).map(str::to_string);
                }
            }
            "session" => {
                let header_timestamp = parse_entry_timestamp(&entry);
                session_id = entry
                    .get("id")
                    .and_then(|value| value.as_str())
                    .filter(|value| !value.trim().is_empty())
                    .map(str::to_string)
                    .or(session_id);
                cwd = entry
                    .get("cwd")
                    .and_then(|value| value.as_str())
                    .filter(|value| !value.trim().is_empty())
                    .map(str::to_string)
                    .or(cwd);
                started_at = header_timestamp.or(started_at);
                if format == Format::Omp && custom_title.is_none() {
                    custom_title = non_empty_str(entry.get("title")).map(str::to_string);
                }
                if let Some(parent) = entry
                    .get("parentSession")
                    .and_then(|value| value.as_str())
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                {
                    inherited_usage_cutoff = header_timestamp;
                    parent_session = Some(parent.to_string());
                }
            }
            "model_change" => {
                current_provider = entry
                    .get("provider")
                    .and_then(|value| value.as_str())
                    .filter(|value| !value.trim().is_empty())
                    .map(str::to_string)
                    .or(current_provider);
                current_model = entry
                    .get("modelId")
                    .or_else(|| entry.get("model"))
                    .and_then(|value| value.as_str())
                    .filter(|value| !value.trim().is_empty())
                    .map(str::to_string)
                    .or(current_model);
            }
            "message" => {
                if let Some(message) = entry.get("message") {
                    let timestamp = json_i64(message.get("timestamp"))
                        .or_else(|| parse_entry_timestamp(&entry))
                        .unwrap_or(fallback_timestamp);
                    if include_events
                        && inherited_usage_cutoff.is_none_or(|cutoff| timestamp > cutoff)
                    {
                        extract_events(
                            &entry,
                            message,
                            line_index,
                            timestamp,
                            (&source_path, messages.len().checked_sub(1).map(|seq| seq as u32)),
                            cwd.as_deref(),
                            format,
                            &mut events,
                        );
                    }
                    parse_message(
                        &entry,
                        message,
                        line_index as u32,
                        timestamp,
                        current_provider.as_deref(),
                        current_model.as_deref(),
                        &source_path,
                        inherited_usage_cutoff,
                        &mut messages,
                        &mut usage_events,
                    );
                }
            }
            "custom_message" => {
                let timestamp = parse_entry_timestamp(&entry).unwrap_or(fallback_timestamp);
                let content = extract_content(entry.get("content"));
                if !content.trim().is_empty() {
                    messages.push(RawMessage {
                        role: Role::User,
                        content,
                        timestamp: Some(timestamp),
                    });
                }
            }
            "compaction" | "branch_summary" => {
                if let Some(summary) = entry.get("summary").and_then(|value| value.as_str())
                    && !summary.trim().is_empty()
                {
                    let timestamp = parse_entry_timestamp(&entry).unwrap_or(fallback_timestamp);
                    messages.push(RawMessage {
                        role: Role::Assistant,
                        content: summary.to_string(),
                        timestamp: Some(timestamp),
                    });
                }
            }
            _ => {}
        }
    }

    Ok(ParsedSession {
        session_id,
        cwd,
        started_at,
        custom_title,
        messages,
        usage_events,
        events,
        parent_session,
    })
}

#[allow(clippy::too_many_arguments)]
fn parse_message(
    entry: &Value,
    message: &Value,
    line_index: u32,
    timestamp: i64,
    current_provider: Option<&str>,
    current_model: Option<&str>,
    source_path: &str,
    inherited_usage_cutoff: Option<i64>,
    messages: &mut Vec<RawMessage>,
    usage_events: &mut Vec<RawUsageEvent>,
) {
    match message.get("role").and_then(|value| value.as_str()).unwrap_or("") {
        "user" | "custom" => {
            let content = extract_content(message.get("content"));
            if !content.trim().is_empty() {
                messages.push(RawMessage { role: Role::User, content, timestamp: Some(timestamp) });
            }
        }
        "assistant" => {
            let content = extract_content(message.get("content"));
            let message_seq =
                if content.trim().is_empty() { None } else { Some(messages.len() as u32) };

            if inherited_usage_cutoff.is_none_or(|cutoff| timestamp > cutoff)
                && let Some(event) = extract_usage_event(
                    entry,
                    message,
                    line_index,
                    timestamp,
                    message_seq,
                    (current_provider, current_model),
                    source_path,
                )
            {
                usage_events.push(event);
            }

            if !content.trim().is_empty() {
                messages.push(RawMessage {
                    role: Role::Assistant,
                    content,
                    timestamp: Some(timestamp),
                });
            }
        }
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn extract_events(
    entry: &Value,
    message: &Value,
    line_index: usize,
    timestamp: i64,
    (source_path, message_seq): (&str, Option<u32>),
    cwd: Option<&str>,
    format: Format,
    events: &mut Vec<RawSessionEvent>,
) {
    let context = |event_seq, part_index| EventContext {
        event_seq,
        timestamp: Some(timestamp),
        source_path: Some(source_path.to_string()),
        source_event_id: Some(format!(
            "{}:line:{line_index}:part:{part_index}",
            non_empty_str(entry.get("id")).unwrap_or("message")
        )),
        message_seq,
        parser_version: EVENT_PARSER_VERSION,
    };
    match message.get("role").and_then(Value::as_str) {
        Some("assistant") => {
            let Some(parts) = message.get("content").and_then(Value::as_array) else {
                return;
            };
            for (part_index, part) in parts.iter().enumerate() {
                if !matches!(
                    part.get("type").and_then(Value::as_str),
                    Some("toolCall" | "tool_call" | "function_call")
                ) {
                    continue;
                }
                let Some(name) = non_empty_str(part.get("name")) else {
                    continue;
                };
                let raw_args = part.get("arguments").or_else(|| part.get("input"));
                let decoded = raw_args
                    .and_then(Value::as_str)
                    .and_then(|text| serde_json::from_str::<Value>(text).ok());
                let args = decoded.as_ref().or(raw_args);
                let mut event = tool_call_event(
                    context(events.len() as u32, part_index),
                    name.to_string(),
                    args,
                );
                event.kind = "tool_call".to_string();
                event.target = None;
                let operation = match (name, format) {
                    ("read", _) => Some(FileOperation::Read),
                    ("edit" | "write", Format::Pi) => Some(FileOperation::Write),
                    _ => None,
                };
                if let Some(operation) = operation
                    && let Some(path) = args.and_then(|args| non_empty_str(args.get("path")))
                {
                    event.kind =
                        if operation == FileOperation::Read { "file_read" } else { "file_write" }
                            .to_string();
                    event.target = Some(path.to_string());
                    event.files.push(FileEvidence::call(
                        path.to_string(),
                        operation,
                        cwd.map(str::to_string),
                    ));
                } else if name == "bash" {
                    event.kind = "command".to_string();
                    event.target = args
                        .and_then(|args| non_empty_str(args.get("command")))
                        .map(str::to_string);
                    if let Some(command) = event.target.as_deref() {
                        let shell_cwd = match format {
                            Format::Pi => cwd,
                            Format::Omp => args
                                .and_then(|args| args.get("cwd"))
                                .and_then(Value::as_str)
                                .filter(|cwd| Path::new(cwd).is_absolute()),
                        };
                        let (files, status) =
                            crate::adapters::events::shell_file_evidence(command, shell_cwd);
                        event.files = files;
                        event.command_evidence_status = Some(status);
                    }
                } else if name == "grep" {
                    event.kind = "search".to_string();
                    event.target = args
                        .and_then(|args| non_empty_str(args.get("pattern")))
                        .map(str::to_string);
                }
                event.tool_call_id = non_empty_str(part.get("id")).map(str::to_string);
                event.attrs_json = Some(entry.to_string());
                events.push(event);
            }
        }
        Some("toolResult" | "bashExecution") => {
            let bash = message.get("role").and_then(Value::as_str) == Some("bashExecution");
            let mut event = tool_result_event(
                context(events.len() as u32, 0),
                if bash {
                    Some("bash".to_string())
                } else {
                    non_empty_str(message.get("toolName")).map(str::to_string)
                },
                Some(if bash {
                    extract_bash_execution_content(message)
                } else {
                    extract_content(message.get("content"))
                }),
            );
            if bash {
                event.target = non_empty_str(message.get("command")).map(str::to_string);
                if let Some(command) = event.target.as_deref() {
                    let (files, status) = crate::adapters::events::shell_file_evidence(
                        command,
                        (format == Format::Pi).then_some(cwd).flatten(),
                    );
                    event.files = files;
                    event.command_evidence_status = Some(status);
                }
                event.status = if message.get("cancelled").and_then(Value::as_bool) == Some(true) {
                    Some("cancelled".to_string())
                } else {
                    json_i64(message.get("exitCode"))
                        .map(|code| if code == 0 { "success" } else { "error" }.to_string())
                };
                if message.get("excludeFromContext").and_then(Value::as_bool) == Some(true) {
                    event.visibility = Some(EvidenceVisibility::Hidden);
                    event.message_seq = None;
                }
            } else {
                event.tool_call_id = non_empty_str(message.get("toolCallId")).map(str::to_string);
                event.status = message
                    .get("isError")
                    .and_then(Value::as_bool)
                    .map(|is_error| if is_error { "error" } else { "success" }.to_string());
            }
            event.attrs_json = Some(entry.to_string());
            events.push(event);
        }
        _ => {}
    }
}

pub(super) fn extract_usage_event(
    entry: &Value,
    message: &Value,
    event_seq: u32,
    timestamp: i64,
    message_seq: Option<u32>,
    current_provider_model: (Option<&str>, Option<&str>),
    source_path: &str,
) -> Option<RawUsageEvent> {
    let (current_provider, current_model) = current_provider_model;
    let usage = message.get("usage")?;
    let provider = non_empty_str(message.get("provider"))
        .or(current_provider)
        .unwrap_or("unknown")
        .to_string();
    let model =
        non_empty_str(message.get("model")).or(current_model).unwrap_or("unknown").to_string();

    let event_key = entry
        .get("id")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(|id| format!("message:{id}"))
        .unwrap_or_else(|| format!("line:{event_seq}"));

    let input_tokens = usage_count(usage, &["input", "inputTokens", "input_tokens"]);
    let raw_output_tokens = usage_count(usage, &["output", "outputTokens", "output_tokens"]);
    let cache_read_tokens = usage_count(
        usage,
        &[
            "cacheRead",
            "cache_read",
            "cacheReadTokens",
            "cache_read_tokens",
            "cachedInputTokens",
            "cached_input_tokens",
        ],
    );
    let cache_write_tokens = usage_count(
        usage,
        &["cacheWrite", "cache_write", "cacheWriteTokens", "cache_write_tokens"],
    );
    let raw_reasoning_tokens = usage_count(
        usage,
        &[
            "reasoning",
            "reasoningTokens",
            "reasoning_tokens",
            "reasoningOutputTokens",
            "reasoning_output_tokens",
        ],
    );
    let other_tokens =
        input_tokens.saturating_add(cache_read_tokens).saturating_add(cache_write_tokens);
    let (output_tokens, reasoning_tokens) =
        disjoint_output_and_reasoning(usage, raw_output_tokens, raw_reasoning_tokens, other_tokens);

    Some(RawUsageEvent {
        message_seq,
        model,
        provider,
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_write_tokens,
        reasoning_tokens,
        source_path: Some(source_path.to_string()),
        raw_usage_json: Some(usage.to_string()),
        ..RawUsageEvent::observed(event_key, event_seq, timestamp, USAGE_PARSER_VERSION)
    })
}

fn non_empty_str(value: Option<&Value>) -> Option<&str> {
    value.and_then(|value| value.as_str()).filter(|value| !value.trim().is_empty())
}

fn extract_content(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.to_string(),
        Some(Value::Array(items)) => items
            .iter()
            .filter(|item| {
                matches!(item.get("type").and_then(Value::as_str), Some("text" | "output_text"))
            })
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn extract_bash_execution_content(message: &Value) -> String {
    let command = message
        .get("command")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty());
    let output = message
        .get("output")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty());

    match (command, output) {
        (Some(command), Some(output)) => format!("[bash] {command}\n{output}"),
        (Some(command), None) => format!("[bash] {command}"),
        (None, Some(output)) => output.to_string(),
        (None, None) => String::new(),
    }
}

fn parse_entry_timestamp(entry: &Value) -> Option<i64> {
    rfc3339_ms(entry.get("timestamp"))
}

#[cfg(test)]
pub(super) mod test_support;
