use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;
use tracing::debug;
use walkdir::WalkDir;

use crate::adapters::AdapterSyncContext;
use crate::adapters::events::{EventContext, tool_call_event, tool_result_event};
use crate::adapters::file_scan::{self, FileScanEntry};
use crate::adapters::json_util::{json_i64, jsonl_indexed, rfc3339_ms};
use crate::adapters::usage::{disjoint_output_and_reasoning, usage_count};
use crate::adapters::{
    RawMessage, RawSession, ResumeCommand, SourceAdapter, SyncScanResult, SyncScanStats,
    first_timestamp,
};
use crate::types::{
    EvidenceVisibility, FileEvidence, FileEvidenceKind, FileOperation, ParentLink, ParentRelation,
    RawSessionEvent, RawUsageEvent, Role, ThreadRole,
};

pub(crate) struct PiAdapter;

const METADATA_PARSER_VERSION: u32 = 2;

const USAGE_PARSER_VERSION: u32 = 3;
const EVENT_PARSER_VERSION: u32 = 1;

impl SourceAdapter for PiAdapter {
    fn id(&self) -> &str {
        "pi"
    }

    fn label(&self) -> &str {
        "PI"
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        Some(ResumeCommand {
            program: "pi".to_string(),
            args: vec!["--session".to_string(), source_id.to_string()],
        })
    }

    fn start_command(&self, prompt: String) -> Option<ResumeCommand> {
        Some(crate::adapters::prompt_start("pi", prompt))
    }

    fn usage_parser_version(&self) -> Option<u32> {
        Some(USAGE_PARSER_VERSION)
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let session_dirs = resolve_pi_session_dirs()?;
        if session_dirs.is_empty() {
            return Ok(vec![]);
        }

        let mut sessions = Vec::new();
        for entry in collect_pi_entries(&session_dirs) {
            let Some(mtime_ms) = file_scan::stat_mtime_ms(&entry.stat_target) else {
                continue;
            };
            if let Some(raw) = parse_pi_session_file(entry, mtime_ms, true)? {
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
        let session_dirs = resolve_pi_session_dirs()?;
        if session_dirs.is_empty() {
            return Ok(Some(SyncScanResult {
                sessions: vec![],
                stats: SyncScanStats::default(),
                observations: Vec::new(),
            }));
        }

        Ok(Some(scan_for_sync_impl(&session_dirs, context, since_ts, include_events)?))
    }
}

struct ParsedPiSession {
    session_id: Option<String>,
    cwd: Option<String>,
    started_at: Option<i64>,
    messages: Vec<RawMessage>,
    usage_events: Vec<RawUsageEvent>,
    events: Vec<RawSessionEvent>,
    parent_session: Option<String>,
}

fn resolve_pi_session_dirs() -> anyhow::Result<Vec<PathBuf>> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home dir"))?;
    let mut session_dirs = Vec::new();
    let mut seen = HashSet::new();

    let env_session_dir =
        std::env::var("PI_CODING_AGENT_SESSION_DIR").ok().filter(|path| !path.trim().is_empty());
    let agent_dir = std::env::var("PI_CODING_AGENT_DIR")
        .ok()
        .filter(|path| !path.trim().is_empty())
        .map(|path| expand_home_path(path.trim(), &home))
        .unwrap_or_else(|| home.join(".pi").join("agent"));

    if let Some(session_dir) = env_session_dir.as_deref() {
        push_existing_unique_dir(
            &mut session_dirs,
            &mut seen,
            expand_home_path(session_dir.trim(), &home),
        );
        if session_dirs.is_empty() {
            debug!("Pi session directory from PI_CODING_AGENT_SESSION_DIR not found, skipping Pi");
            return Ok(session_dirs);
        }
    } else if let Some(session_dir) = settings_session_dir(&agent_dir, &home) {
        push_existing_unique_dir(&mut session_dirs, &mut seen, session_dir);
    }
    if env_session_dir.is_none() {
        push_existing_unique_dir(&mut session_dirs, &mut seen, agent_dir.join("sessions"));
    }

    if session_dirs.is_empty() {
        debug!("Pi session directory not found, skipping Pi");
    }

    Ok(session_dirs)
}

fn settings_session_dir(agent_dir: &Path, home: &Path) -> Option<PathBuf> {
    settings_session_dir_with_cwd(agent_dir, home, std::env::current_dir().ok().as_deref())
}

fn settings_session_dir_with_cwd(
    agent_dir: &Path,
    home: &Path,
    current_dir: Option<&Path>,
) -> Option<PathBuf> {
    let global = session_dir_from_settings(&agent_dir.join("settings.json"), home);
    let Some(current_dir) = current_dir else {
        return global;
    };
    let project_settings_dir = current_dir.join(".pi");
    session_dir_from_settings(&project_settings_dir.join("settings.json"), home).or(global)
}

fn session_dir_from_settings(settings_path: &Path, home: &Path) -> Option<PathBuf> {
    let content = fs::read_to_string(settings_path).ok()?;
    let settings: Value = serde_json::from_str(&content).ok()?;
    let session_dir = settings.get("sessionDir")?.as_str()?.trim();
    if session_dir.is_empty() {
        return None;
    }
    let session_dir = expand_home_path(session_dir, home);
    if session_dir.is_relative() {
        return Some(settings_path.parent()?.join(session_dir));
    }
    Some(session_dir)
}

fn expand_home_path(path: &str, home: &Path) -> PathBuf {
    if path == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return home.join(rest);
    }
    PathBuf::from(path)
}

fn push_existing_unique_dir(dirs: &mut Vec<PathBuf>, seen: &mut HashSet<String>, dir: PathBuf) {
    if !dir.exists() {
        return;
    }

    let key = fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone()).to_string_lossy().to_string();
    if seen.insert(key) {
        dirs.push(dir);
    }
}

fn scan_for_sync_impl(
    session_dirs: &[PathBuf],
    context: &AdapterSyncContext,
    since_ts: Option<i64>,
    include_events: bool,
) -> anyhow::Result<SyncScanResult> {
    let entries = collect_pi_entries(session_dirs);
    file_scan::run_file_scan_with_options(
        context,
        since_ts,
        file_scan::FileScanOptions {
            usage_parser_version: Some(USAGE_PARSER_VERSION),
            event_parser_version: include_events.then_some(EVENT_PARSER_VERSION),
            metadata_parser_version: include_events.then_some(METADATA_PARSER_VERSION),
        },
        entries,
        |entry, mtime_ms| parse_pi_session_file(entry, mtime_ms, include_events),
    )
}

fn collect_pi_entries(session_dirs: &[PathBuf]) -> Vec<FileScanEntry> {
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
                .and_then(decode_session_dir_name);

            entries.push(FileScanEntry { session_id, stat_target: path.to_path_buf(), directory });
        }
    }

    entries
}

fn extract_session_id_from_filename(stem: &str) -> Option<String> {
    let candidate = stem.rsplit_once('_').map(|(_, tail)| tail).unwrap_or(stem);
    uuid::Uuid::try_parse(candidate).ok().map(|_| candidate.to_string())
}

/// Pi `parentSession` is a local path; return the portable id or `None` (never store a path).
fn normalize_pi_parent_id(parent: &str) -> Option<String> {
    let stem = Path::new(parent).file_stem().and_then(|stem| stem.to_str()).unwrap_or(parent);
    extract_session_id_from_filename(stem)
}

fn decode_session_dir_name(name: &str) -> Option<String> {
    let inner = name.strip_prefix("--")?.strip_suffix("--")?;
    if inner.is_empty() {
        return None;
    }
    Some(format!("/{}", inner.replace('-', "/")))
}

fn parse_pi_session_file(
    entry: FileScanEntry,
    mtime_ms: i64,
    include_events: bool,
) -> anyhow::Result<Option<RawSession>> {
    let source_file_path = entry.stat_target.to_str().map(str::to_string);
    let parsed = match parse_pi_session(&entry.stat_target, mtime_ms, include_events) {
        Ok(parsed) => parsed,
        Err(err) => {
            debug!("failed to parse Pi session {}: {err}", entry.stat_target.display());
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
        .and_then(normalize_pi_parent_id)
        .filter(|parent| parent != &source_id)
    {
        Some(parent) => vec![ParentLink {
            relation: ParentRelation::Fork,
            source: "pi".to_string(),
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
        custom_title: None,
        summary: None,
        duration_minutes: None,
        thread_role: Some(ThreadRole::Primary),
        parent_links,
        metadata_parser_version: Some(METADATA_PARSER_VERSION),
        refresh_session_on_metadata_backfill: true,
    }))
}

fn parse_pi_session(
    path: &Path,
    fallback_timestamp: i64,
    include_events: bool,
) -> anyhow::Result<ParsedPiSession> {
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let source_path = path.to_string_lossy().to_string();

    let mut session_id = None;
    let mut cwd = None;
    let mut started_at = None;
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
                        extract_pi_events(
                            &entry,
                            message,
                            line_index,
                            timestamp,
                            (&source_path, messages.len().checked_sub(1).map(|seq| seq as u32)),
                            cwd.as_deref(),
                            &mut events,
                        );
                    }
                    parse_pi_message(
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

    Ok(ParsedPiSession {
        session_id,
        cwd,
        started_at,
        messages,
        usage_events,
        events,
        parent_session,
    })
}

#[allow(clippy::too_many_arguments)]
fn parse_pi_message(
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
                && let Some(event) = extract_pi_usage_event(
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

fn extract_pi_events(
    entry: &Value,
    message: &Value,
    line_index: usize,
    timestamp: i64,
    (source_path, message_seq): (&str, Option<u32>),
    cwd: Option<&str>,
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
                let operation = match name {
                    "read" => Some(FileOperation::Read),
                    "edit" | "write" => Some(FileOperation::Write),
                    _ => None,
                };
                if let Some(operation) = operation
                    && let Some(path) = args.and_then(|args| non_empty_str(args.get("path")))
                {
                    event.kind =
                        if operation == FileOperation::Read { "file_read" } else { "file_write" }
                            .to_string();
                    event.target = Some(path.to_string());
                    event.files.push(FileEvidence {
                        path: path.to_string(),
                        operation,
                        kind: FileEvidenceKind::Call,
                        cwd: cwd.map(str::to_string),
                        target: None,
                    });
                } else if name == "bash" {
                    event.kind = "command".to_string();
                    event.target = args
                        .and_then(|args| non_empty_str(args.get("command")))
                        .map(str::to_string);
                    if let Some(command) = event.target.as_deref() {
                        let (files, status) =
                            crate::adapters::events::shell_file_evidence(command, cwd);
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
                    let (files, status) =
                        crate::adapters::events::shell_file_evidence(command, cwd);
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

fn extract_pi_usage_event(
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
mod tests {
    use std::io::Write;

    use super::*;
    use crate::db::{schema, store::Store};
    use crate::types::Session;

    fn setup_store() -> Store {
        schema::register_sqlite_vec();
        Store::open_in_memory().unwrap()
    }

    fn temp_pi_root(label: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("recall-pi-test-{}-{}", label, uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_pi_session(dir: &Path, session_id: &str, lines: &[Value]) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(format!("2026-05-24T17-04-51-496Z_{session_id}.jsonl"));
        let mut file = fs::File::create(&path).unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
        path
    }

    fn make_existing_session(source_id: &str, updated_at: i64, message_count: u32) -> Session {
        Session {
            id: format!("internal-{source_id}"),
            source: "pi".to_string(),
            source_id: source_id.to_string(),
            title: "existing".to_string(),
            directory: Some("/tmp/pi-project".to_string()),
            repo_remote: None,
            repo_slug: None,
            repo_name: None,
            started_at: 1_000,
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
    fn extract_session_id_from_filename_reads_pi_uuid_tail() {
        assert_eq!(
            extract_session_id_from_filename(
                "2026-05-24T17-04-51-496Z_019e5af2-5528-7d10-888a-b299c21d0e2e"
            ),
            Some("019e5af2-5528-7d10-888a-b299c21d0e2e".to_string())
        );
        assert_eq!(extract_session_id_from_filename("not-a-session"), None);
    }

    #[test]
    fn session_dir_from_settings_resolves_relative_paths_from_settings_scope() {
        let root = temp_pi_root("settings-dir");
        let home = root.join("home");
        let agent_dir = home.join(".pi").join("agent");
        fs::create_dir_all(&agent_dir).unwrap();
        let settings_path = agent_dir.join("settings.json");
        fs::write(&settings_path, r#"{"sessionDir":"custom-sessions"}"#).unwrap();

        assert_eq!(
            session_dir_from_settings(&settings_path, &home).as_deref(),
            Some(agent_dir.join("custom-sessions").as_path())
        );

        fs::write(&settings_path, r#"{"sessionDir":"~/pi-sessions"}"#).unwrap();
        assert_eq!(
            session_dir_from_settings(&settings_path, &home).as_deref(),
            Some(home.join("pi-sessions").as_path())
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn settings_session_dir_keeps_global_when_current_dir_is_unavailable() {
        let root = temp_pi_root("settings-no-cwd");
        let home = root.join("home");
        let agent_dir = home.join(".pi").join("agent");
        let global_session_dir = root.join("global-sessions");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::create_dir_all(&global_session_dir).unwrap();
        fs::write(
            agent_dir.join("settings.json"),
            format!(r#"{{"sessionDir":"{}"}}"#, global_session_dir.display()),
        )
        .unwrap();

        assert_eq!(
            settings_session_dir_with_cwd(&agent_dir, &home, None).as_deref(),
            Some(global_session_dir.as_path())
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_pi_session_file_extracts_messages_and_usage() {
        let root = temp_pi_root("parse");
        let session_dir = root.join("--tmp-pi-project--");
        let session_id = "019e5af2-5528-7d10-888a-b299c21d0e2e";
        let path = write_pi_session(
            &session_dir,
            session_id,
            &[
                serde_json::json!({
                    "type": "session",
                    "version": 3,
                    "id": session_id,
                    "timestamp": "1970-01-01T00:00:01.000Z",
                    "cwd": "/tmp/pi-project"
                }),
                serde_json::json!({
                    "type": "message",
                    "id": "user1",
                    "parentId": null,
                    "timestamp": "1970-01-01T00:00:02.000Z",
                    "message": {
                        "role": "user",
                        "content": [{"type": "text", "text": "hello pi"}],
                        "timestamp": 2000
                    }
                }),
                serde_json::json!({
                    "type": "message",
                    "id": "assistant1",
                    "parentId": "user1",
                    "timestamp": "1970-01-01T00:00:03.000Z",
                    "message": {
                        "role": "assistant",
                        "content": [
                            {"type": "thinking", "thinking": "hidden chain of thought"},
                            {"type": "toolCall", "id": "read-call", "name": "read", "arguments": {"path": "README.md"}},
                            {"type": "toolCall", "id": "edit-call", "name": "edit", "arguments": {"path": " spaced.rs ", "edits": [{"oldText": "old", "newText": "new"}]}},
                            {"type": "toolCall", "id": "write-call", "name": "write", "arguments": {"path": "new.rs", "content": "new file"}},
                            {"type": "image", "mimeType": "image/png"}
                        ],
                        "provider": "openai-codex",
                        "model": "gpt-5.5",
                        "usage": {
                            "input": 10,
                            "output": 3,
                            "cacheRead": 2,
                            "cacheWrite": 1,
                            "totalTokens": 16,
                            "cost": {"total": 0.1}
                        },
                        "timestamp": 3000
                    }
                }),
                serde_json::json!({
                    "type": "message",
                    "id": "tool1",
                    "parentId": "assistant1",
                    "timestamp": "1970-01-01T00:00:04.000Z",
                    "message": {
                        "role": "toolResult",
                        "toolName": "read",
                        "toolCallId": "read-call",
                        "isError": false,
                        "content": [{"type": "text", "text": "file content"}],
                        "timestamp": 4000
                    }
                }),
            ],
        );
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();
        let raw = parse_pi_session_file(
            FileScanEntry {
                session_id: session_id.to_string(),
                stat_target: path.clone(),
                directory: Some("/wrong".to_string()),
            },
            mtime,
            true,
        )
        .unwrap()
        .unwrap();

        assert_eq!(raw.source_id, session_id);
        assert_eq!(raw.directory.as_deref(), Some("/tmp/pi-project"));
        assert_eq!(raw.started_at, 1_000);
        assert_eq!(raw.updated_at, Some(mtime));
        assert_eq!(raw.source_file_path.as_deref(), path.to_str());
        assert_eq!(raw.messages.len(), 1);
        assert_eq!(raw.messages[0].role, Role::User);
        assert_eq!(raw.messages[0].content, "hello pi");
        assert_eq!(raw.events.len(), 4);
        let call = &raw.events[0];
        assert_eq!(call.tool_call_id.as_deref(), Some("read-call"));
        assert!(call.source_event_id.as_deref().unwrap().starts_with("assistant1:line:"));
        assert_eq!(call.source_path.as_deref(), path.to_str());
        assert_eq!(call.timestamp, Some(3_000));
        assert_eq!(call.message_seq, Some(0));
        assert_eq!(call.files.len(), 1);
        assert_eq!(call.files[0].path, "README.md");
        assert_eq!(call.files[0].operation, FileOperation::Read);
        assert_eq!(call.files[0].kind, FileEvidenceKind::Call);
        assert_eq!(call.files[0].cwd, raw.directory);
        assert_eq!(raw.events[1].files[0].path, " spaced.rs ");
        assert_eq!(raw.events[1].files[0].operation, FileOperation::Write);
        assert_eq!(raw.events[2].files[0].operation, FileOperation::Write);
        let payload: Value =
            serde_json::from_str(raw.events[1].attrs_json.as_deref().unwrap()).unwrap();
        assert_eq!(
            payload.pointer("/message/content/2/arguments/edits/0/oldText"),
            Some(&Value::from("old"))
        );
        let result = &raw.events[3];
        assert_eq!(result.tool_call_id, call.tool_call_id);
        assert_eq!(result.message_seq, Some(0));
        assert_eq!(result.status.as_deref(), Some("success"));
        assert!(result.files.is_empty());
        let payload: Value = serde_json::from_str(result.attrs_json.as_deref().unwrap()).unwrap();
        assert_eq!(payload.pointer("/message/content/0/text"), Some(&Value::from("file content")));
        assert_eq!(raw.event_parser_version, Some(EVENT_PARSER_VERSION));

        assert_eq!(raw.usage_events.len(), 1);
        let event = &raw.usage_events[0];
        assert_eq!(event.event_key, "message:assistant1");
        assert_eq!(event.message_seq, None);
        assert_eq!(event.timestamp, 3_000);
        assert_eq!(event.provider, "openai-codex");
        assert_eq!(event.model, "gpt-5.5");
        assert_eq!(event.input_tokens, 10);
        assert_eq!(event.output_tokens, 3);
        assert_eq!(event.cache_read_tokens, 2);
        assert_eq!(event.cache_write_tokens, 1);
        assert_eq!(event.token_source, crate::types::TokenSource::Observed);
        assert_eq!(event.parser_version, USAGE_PARSER_VERSION);
        assert_eq!(event.source_path.as_deref(), Some(path.to_string_lossy().as_ref()));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn usage_normalizes_reasoning_included_in_output() {
        let entry = serde_json::json!({"id": "assistant1"});
        let message = serde_json::json!({
            "provider": "openrouter",
            "model": "deepseek",
            "usage": {
                "input": 10,
                "output": 7,
                "cacheRead": 2,
                "cacheWrite": 1,
                "reasoningTokens": 4,
                "totalTokens": 20
            }
        });

        let event = extract_pi_usage_event(
            &entry,
            &message,
            1,
            3_000,
            Some(1),
            (None, None),
            "/tmp/session.jsonl",
        )
        .unwrap();

        assert_eq!(event.output_tokens, 3);
        assert_eq!(event.reasoning_tokens, 4);
    }

    #[test]
    fn usage_caps_reasoning_that_exceeds_inclusive_output() {
        let entry = serde_json::json!({"id": "assistant1"});
        let message = serde_json::json!({
            "provider": "xai",
            "model": "grok",
            "usage": {
                "input": 10,
                "output": 3,
                "cacheRead": 2,
                "reasoningTokens": 4,
                "totalTokens": 15
            }
        });

        let event = extract_pi_usage_event(
            &entry,
            &message,
            1,
            3_000,
            Some(1),
            (None, None),
            "/tmp/session.jsonl",
        )
        .unwrap();

        assert_eq!(event.output_tokens, 0);
        assert_eq!(event.reasoning_tokens, 3);
    }

    #[test]
    fn parse_pi_session_file_indexes_custom_role_message_content() {
        let root = temp_pi_root("custom-role");
        let session_dir = root.join("--tmp-pi-project--");
        let session_id = "019e5af2-5528-7d10-888a-b299c21d0e2e";
        let path = write_pi_session(
            &session_dir,
            session_id,
            &[
                serde_json::json!({
                    "type": "session",
                    "version": 3,
                    "id": session_id,
                    "timestamp": "1970-01-01T00:00:01.000Z",
                    "cwd": "/tmp/pi-project"
                }),
                serde_json::json!({
                    "type": "message",
                    "id": "custom1",
                    "parentId": null,
                    "timestamp": "1970-01-01T00:00:02.000Z",
                    "message": {
                        "role": "custom",
                        "customType": "extension-context",
                        "content": [{"type": "text", "text": "injected context"}],
                        "display": false,
                        "timestamp": 2000
                    }
                }),
            ],
        );
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();
        let raw = parse_pi_session_file(
            FileScanEntry {
                session_id: session_id.to_string(),
                stat_target: path,
                directory: None,
            },
            mtime,
            true,
        )
        .unwrap()
        .unwrap();

        assert_eq!(raw.messages.len(), 1);
        assert_eq!(raw.messages[0].role, Role::User);
        assert_eq!(raw.messages[0].content, "injected context");
        assert_eq!(raw.messages[0].timestamp, Some(2_000));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_pi_session_file_skips_hidden_bash_execution() {
        let root = temp_pi_root("hidden-bash");
        let session_dir = root.join("--tmp-pi-project--");
        let session_id = "019e5af2-5528-7d10-888a-b299c21d0e2e";
        let path = write_pi_session(
            &session_dir,
            session_id,
            &[
                serde_json::json!({
                    "type": "session", "version": 3, "id": session_id,
                    "timestamp": "1970-01-01T00:00:01.000Z", "cwd": "/tmp/pi-project"
                }),
                serde_json::json!({
                    "type": "message", "id": "bash1", "parentId": "user1",
                    "timestamp": "1970-01-01T00:00:03.000Z",
                    "message": {
                        "role": "bashExecution",
                        "command": "cat secret.txt",
                        "output": "secret output",
                        "excludeFromContext": true,
                        "exitCode": 7,
                        "cancelled": true,
                        "timestamp": 3000
                    }
                }),
            ],
        );
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();
        let raw = parse_pi_session_file(
            FileScanEntry {
                session_id: session_id.to_string(),
                stat_target: path,
                directory: None,
            },
            mtime,
            true,
        )
        .unwrap()
        .unwrap();

        assert!(raw.messages.is_empty());
        assert_eq!(raw.events.len(), 1);
        let event = &raw.events[0];
        assert_eq!(event.kind, "tool_result");
        assert_eq!(event.visibility, Some(EvidenceVisibility::Hidden));
        assert_eq!(event.status.as_deref(), Some("cancelled"));
        assert_eq!(event.target.as_deref(), Some("cat secret.txt"));
        assert!(event.tool_call_id.is_none());
        assert!(event.files.is_empty());
        let payload: Value = serde_json::from_str(event.attrs_json.as_deref().unwrap()).unwrap();
        assert_eq!(payload.pointer("/message/exitCode"), Some(&Value::from(7)));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_pi_session_file_uses_model_change_for_usage_only_assistant_message() {
        let root = temp_pi_root("usage-only");
        let session_dir = root.join("--tmp-pi-project--");
        let session_id = "019e5af2-5528-7d10-888a-b299c21d0e2e";
        let path = write_pi_session(
            &session_dir,
            session_id,
            &[
                serde_json::json!({
                    "type": "session",
                    "version": 3,
                    "id": session_id,
                    "timestamp": "1970-01-01T00:00:01.000Z",
                    "cwd": "/tmp/pi-project"
                }),
                serde_json::json!({
                    "type": "model_change",
                    "id": "model1",
                    "parentId": null,
                    "timestamp": "1970-01-01T00:00:02.000Z",
                    "provider": "anthropic",
                    "modelId": "claude-opus-4-7"
                }),
                serde_json::json!({
                    "type": "message",
                    "id": "assistant-empty",
                    "parentId": "model1",
                    "timestamp": "1970-01-01T00:00:03.000Z",
                    "message": {
                        "role": "assistant",
                        "content": [],
                        "usage": {
                            "input": 5,
                            "output": 7,
                            "cacheRead": 11,
                            "cacheWrite": 13,
                            "totalTokens": 36
                        },
                        "timestamp": 3000
                    }
                }),
            ],
        );
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();
        let raw = parse_pi_session_file(
            FileScanEntry {
                session_id: session_id.to_string(),
                stat_target: path,
                directory: None,
            },
            mtime,
            true,
        )
        .unwrap()
        .unwrap();

        assert!(raw.messages.is_empty());
        assert_eq!(raw.started_at, 1_000);
        assert_eq!(raw.usage_events.len(), 1);
        assert_eq!(raw.usage_events[0].message_seq, None);
        assert_eq!(raw.usage_events[0].provider, "anthropic");
        assert_eq!(raw.usage_events[0].model, "claude-opus-4-7");
        assert_eq!(raw.usage_events[0].input_tokens, 5);
        assert_eq!(raw.usage_events[0].output_tokens, 7);
        assert_eq!(raw.usage_events[0].cache_read_tokens, 11);
        assert_eq!(raw.usage_events[0].cache_write_tokens, 13);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_pi_session_file_skips_fork_inherited_usage() {
        let root = temp_pi_root("fork-usage");
        let session_dir = root.join("--tmp-pi-project--");
        let session_id = "019e5af2-5528-7d10-888a-b299c21d0e2e";
        let path = write_pi_session(
            &session_dir,
            session_id,
            &[
                serde_json::json!({
                    "type": "session", "version": 3, "id": session_id,
                    "timestamp": "1970-01-01T00:00:03.000Z", "cwd": "/tmp/pi-project",
                    "parentSession": "/tmp/parent.jsonl"
                }),
                serde_json::json!({
                    "type": "message", "id": "parent-assistant", "timestamp": "1970-01-01T00:00:02.000Z",
                    "message": {"role": "assistant", "content": [{"type":"toolCall","id":"old-call","name":"read","arguments":{"path":"old.rs"}}], "usage": {"input": 10}, "timestamp": 2000}
                }),
                serde_json::json!({
                    "type": "message", "id": "child-assistant", "timestamp": "1970-01-01T00:00:04.000Z",
                    "message": {"role": "assistant", "content": [{"type":"toolCall","id":"new-call","name":"read","arguments":{"path":"new.rs"}}], "usage": {"input": 5}, "timestamp": 4000}
                }),
            ],
        );

        let parsed = parse_pi_session(&path, 0, true).unwrap();

        assert!(parsed.messages.is_empty());
        assert_eq!(parsed.events.len(), 1);
        assert_eq!(parsed.events[0].tool_call_id.as_deref(), Some("new-call"));
        assert_eq!(parsed.events[0].files[0].path, "new.rs");
        assert_eq!(parsed.usage_events.len(), 1);
        assert_eq!(parsed.usage_events[0].event_key, "message:child-assistant");
        assert_eq!(parsed.usage_events[0].input_tokens, 5);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_for_sync_skips_unchanged_session_when_usage_state_is_current() {
        let root = temp_pi_root("skip");
        let session_dir = root.join("--tmp-pi-project--");
        let session_id = "019e5af2-5528-7d10-888a-b299c21d0e2e";
        let path = write_pi_session(
            &session_dir,
            session_id,
            &[
                serde_json::json!({
                    "type": "session",
                    "version": 3,
                    "id": session_id,
                    "timestamp": "1970-01-01T00:00:01.000Z",
                    "cwd": "/tmp/pi-project"
                }),
                serde_json::json!({
                    "type": "message",
                    "id": "user1",
                    "parentId": null,
                    "timestamp": "1970-01-01T00:00:02.000Z",
                    "message": {
                        "role": "user",
                        "content": "hello pi",
                        "timestamp": 2000
                    }
                }),
            ],
        );
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();
        let store = setup_store();
        store.insert_session(&make_existing_session(session_id, mtime, 1)).unwrap();
        store
            .persist_usage_events_for_existing_session(
                "pi",
                session_id,
                &[],
                USAGE_PARSER_VERSION,
                Some(mtime),
            )
            .unwrap();
        store
            .persist_topology_for_existing_session(
                "pi",
                session_id,
                &crate::db::store::SessionTopologyWrite {
                    thread_role: None,
                    parents: &[],
                    parser_version: Some(METADATA_PARSER_VERSION),
                },
            )
            .unwrap();

        let usage_only = scan_for_sync_impl(
            std::slice::from_ref(&session_dir),
            &AdapterSyncContext::from_store_for_test(&store, "pi").unwrap(),
            None,
            false,
        )
        .unwrap();
        assert_eq!(usage_only.stats.skipped_sessions, 1);
        for previous_version in [None, Some(EVENT_PARSER_VERSION - 1)] {
            if let Some(version) = previous_version {
                store
                    .persist_session_events_for_existing_session(
                        "pi",
                        session_id,
                        &[],
                        version,
                        Some(mtime),
                    )
                    .unwrap();
            }
            let backfill = scan_for_sync_impl(
                std::slice::from_ref(&session_dir),
                &AdapterSyncContext::from_store_for_test(&store, "pi").unwrap(),
                None,
                true,
            )
            .unwrap();
            assert_eq!(backfill.sessions.len(), 1);
            assert_eq!(backfill.sessions[0].event_parser_version, Some(EVENT_PARSER_VERSION));
        }
        store
            .persist_session_events_for_existing_session(
                "pi",
                session_id,
                &[],
                EVENT_PARSER_VERSION,
                Some(mtime),
            )
            .unwrap();
        let result = scan_for_sync_impl(
            &[root.join("--tmp-pi-project--")],
            &AdapterSyncContext::from_store_for_test(&store, "pi").unwrap(),
            None,
            true,
        )
        .unwrap();
        assert_eq!(result.sessions.len(), 0);
        assert_eq!(result.stats.skipped_sessions, 1);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_pi_session_maps_parent_session_to_primary_fork() {
        let root = temp_pi_root("parent-session");
        let session_dir = root.join("--tmp-pi-project--");
        let session_id = "019e5af2-5528-7d10-888a-b299c21d0e2e";
        let path = write_pi_session(
            &session_dir,
            session_id,
            &[
                serde_json::json!({
                    "type": "session",
                    "version": 3,
                    "id": session_id,
                    "parentSession": "/home/x/.pi/agent/sessions/--proj--/2026-05-24T17-04-51-496Z_019e0000-0000-0000-0000-000000000001.jsonl",
                    "timestamp": "1970-01-01T00:00:01.000Z",
                    "cwd": "/tmp/pi-project"
                }),
                serde_json::json!({
                    "type": "message",
                    "id": "user1",
                    "timestamp": "1970-01-01T00:00:02.000Z",
                    "message": {
                        "role": "user",
                        "content": [{"type": "text", "text": "hello pi"}],
                        "timestamp": 2000
                    }
                }),
            ],
        );
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();
        let entry = FileScanEntry {
            session_id: session_id.to_string(),
            stat_target: path,
            directory: None,
        };

        let raw = parse_pi_session_file(entry, mtime, true).unwrap().unwrap();

        assert_eq!(raw.thread_role, Some(ThreadRole::Primary));
        assert_eq!(
            raw.parent_links,
            vec![ParentLink {
                relation: ParentRelation::Fork,
                source: "pi".to_string(),
                source_id: "019e0000-0000-0000-0000-000000000001".to_string(),
            }]
        );
        assert_eq!(raw.metadata_parser_version, Some(METADATA_PARSER_VERSION));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_pi_session_drops_unresolvable_parent_session() {
        let root = temp_pi_root("parent-unresolvable");
        let session_dir = root.join("--tmp-pi-project--");
        let session_id = "019e5af2-5528-7d10-888a-b299c21d0e2f";
        let path = write_pi_session(
            &session_dir,
            session_id,
            &[
                serde_json::json!({
                    "type": "session",
                    "version": 3,
                    "id": session_id,
                    "parentSession": "not-a-session-path",
                    "timestamp": "1970-01-01T00:00:01.000Z",
                    "cwd": "/tmp/pi-project"
                }),
                serde_json::json!({
                    "type": "message",
                    "id": "user1",
                    "timestamp": "1970-01-01T00:00:02.000Z",
                    "message": {
                        "role": "user",
                        "content": [{"type": "text", "text": "hello pi"}],
                        "timestamp": 2000
                    }
                }),
            ],
        );
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();
        let entry = FileScanEntry {
            session_id: session_id.to_string(),
            stat_target: path,
            directory: None,
        };

        let raw = parse_pi_session_file(entry, mtime, true).unwrap().unwrap();

        assert_eq!(raw.thread_role, Some(ThreadRole::Primary));
        assert!(raw.parent_links.is_empty(), "an unparseable parent must not leak a path");

        let _ = fs::remove_dir_all(&root);
    }
}
