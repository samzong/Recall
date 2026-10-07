use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;
use tracing::{debug, warn};

use crate::adapters::AdapterSyncContext;
use crate::adapters::events::{self, EventContext};
use crate::adapters::file_scan::{self, FileScanEntry, FileScanOptions};
use crate::adapters::json_util::{json_i64, jsonl_indexed};
use crate::adapters::paths::{self, resolve_home_dir};
use crate::adapters::usage::usage_count;
use crate::adapters::{
    RawMessage, RawSession, ResumeCommand, SourceAdapter, SyncScanResult, first_timestamp,
};
use crate::types::{
    FileEvidence, FileOperation, ParentLink, ParentRelation, RawSessionEvent, RawUsageEvent, Role,
    ThreadRole,
};

pub(crate) struct CodeBuddyAdapter;

const SOURCE: &str = "codebuddy";
const USAGE_PARSER_VERSION: u32 = 1;
const EVENT_PARSER_VERSION: u32 = 1;
const METADATA_PARSER_VERSION: u32 = 1;

const ENVELOPE_PREFIXES: &[&str] = &[
    "<command-name>",
    "<command-message>",
    "<command-args>",
    "<system-reminder",
    "<local-command-",
    "<teammate-message",
    "<task-notification",
    "<agent-notification",
];

impl SourceAdapter for CodeBuddyAdapter {
    fn id(&self) -> &str {
        SOURCE
    }

    fn label(&self) -> &str {
        "CB"
    }

    fn usage_parser_version(&self) -> Option<u32> {
        Some(USAGE_PARSER_VERSION)
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        Some(ResumeCommand::new("codebuddy", &["--resume", source_id]))
    }

    fn start_command(&self, prompt: String) -> Option<ResumeCommand> {
        Some(crate::adapters::prompt_start("codebuddy", prompt))
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let Some(config_dir) = resolve_config_dir()? else {
            return Ok(vec![]);
        };
        let mut sessions = Vec::new();
        for entry in collect_entries(&config_dir) {
            let Some(mtime_ms) = file_scan::stat_mtime_ms(&entry.stat_target) else {
                continue;
            };
            sessions.extend(parse_entry(entry, mtime_ms, true)?);
        }
        Ok(sessions)
    }

    fn scan_for_sync(
        &self,
        context: &AdapterSyncContext,
        since_ts: Option<i64>,
        include_events: bool,
    ) -> anyhow::Result<Option<SyncScanResult>> {
        let Some(config_dir) = resolve_config_dir()? else {
            return Ok(Some(SyncScanResult::default()));
        };
        Ok(Some(file_scan::run_file_scan_with_options_and_snapshot(
            context,
            since_ts,
            FileScanOptions {
                stream_sessions: true,
                usage_parser_version: Some(USAGE_PARSER_VERSION),
                event_parser_version: include_events.then_some(EVENT_PARSER_VERSION),
                metadata_parser_version: Some(METADATA_PARSER_VERSION),
            },
            collect_entries(&config_dir),
            |entry| {
                let snapshot = file_scan::file_metadata_snapshot(&entry.stat_target)?;
                Some(file_scan::FileScanSnapshot::new(snapshot.mtime_ms()?, snapshot))
            },
            |entry, mtime_ms| parse_entry(entry, mtime_ms, include_events),
        )?))
    }
}

fn resolve_config_dir() -> anyhow::Result<Option<PathBuf>> {
    if let Some(dir) = paths::env_path_dir("CODEBUDDY_CONFIG_DIR") {
        if dir.is_dir() {
            return Ok(Some(dir));
        }
        debug!("CODEBUDDY_CONFIG_DIR not found, skipping CodeBuddy");
        return Ok(None);
    }
    resolve_home_dir(".codebuddy", "~/.codebuddy not found, skipping CodeBuddy")
}

fn collect_entries(config_dir: &Path) -> Vec<FileScanEntry> {
    let mut entries = Vec::new();
    let Ok(projects) = fs::read_dir(config_dir.join("projects")) else {
        return entries;
    };
    for project in projects.flatten() {
        let Ok(children) = fs::read_dir(project.path()) else {
            continue;
        };
        for child in children.flatten() {
            let path = child.path();
            if path.is_dir() {
                let Ok(subagents) = fs::read_dir(path.join("subagents")) else {
                    continue;
                };
                entries
                    .extend(subagents.flatten().filter_map(|file| transcript_entry(file.path())));
            } else {
                entries.extend(transcript_entry(path));
            }
        }
    }
    entries
}

fn transcript_entry(path: PathBuf) -> Option<FileScanEntry> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_suffix(".jsonl")?;
    if stem.is_empty() || !path.is_file() {
        return None;
    }
    Some(FileScanEntry { session_id: stem.to_string(), stat_target: path, directory: None })
}

fn parent_session_id(path: &Path) -> Option<String> {
    let subagents = path.parent()?;
    if subagents.file_name()? != "subagents" {
        return None;
    }
    let session_dir = subagents.parent()?;
    let parent = session_dir.file_name()?.to_str()?;
    session_dir.with_file_name(format!("{parent}.jsonl")).is_file().then(|| parent.to_string())
}

fn parse_entry(
    entry: FileScanEntry,
    mtime_ms: i64,
    include_events: bool,
) -> anyhow::Result<Option<RawSession>> {
    match parse_transcript(&entry.stat_target, &entry.session_id, mtime_ms, include_events) {
        Ok(raw) => Ok(raw),
        Err(error) => {
            warn!("failed to parse {}: {error}", entry.stat_target.display());
            Ok(None)
        }
    }
}

#[derive(Default)]
struct Titles {
    custom: Option<String>,
    generated: Option<String>,
    topic: Option<String>,
}

impl Titles {
    fn observe(&mut self, record_type: &str, record: &Value) {
        let (slot, key) = match record_type {
            "custom-title" => (&mut self.custom, "customTitle"),
            "ai-title" => (&mut self.generated, "aiTitle"),
            "topic" => (&mut self.topic, "topic"),
            _ => return,
        };
        if let Some(title) = record
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|title| !is_placeholder_title(title))
        {
            *slot = Some(title.to_string());
        }
    }

    fn best(self) -> Option<String> {
        self.custom.or(self.generated).or(self.topic)
    }
}

fn is_placeholder_title(title: &str) -> bool {
    title.is_empty() || title == "(No content)" || title == "/compact"
}

fn parse_transcript(
    path: &Path,
    session_id: &str,
    mtime_ms: i64,
    include_events: bool,
) -> anyhow::Result<Option<RawSession>> {
    let reader = BufReader::new(fs::File::open(path)?);
    let source_path = path.to_str().map(str::to_string);
    let mut directory = None;
    let mut titles = Titles::default();
    let mut messages = Vec::new();
    let mut events = Vec::new();
    let mut usage = UsageCollector::default();
    let mut call_names = HashMap::new();

    for item in jsonl_indexed(reader.lines()) {
        let (line_index, record) = item?;
        if directory.is_none() {
            directory = record
                .get("cwd")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|cwd| !cwd.is_empty())
                .map(str::to_string);
        }
        let record_type = record.get("type").and_then(Value::as_str).unwrap_or("");
        titles.observe(record_type, &record);
        if !matches!(
            record_type,
            "message" | "function_call" | "function_call_result" | "model-usage"
        ) {
            continue;
        }
        let timestamp = json_i64(record.get("timestamp"));
        usage.observe(&record, line_index, timestamp.unwrap_or(mtime_ms), source_path.as_deref());
        if record_type == "model-usage" || is_client_plumbing(&record) {
            continue;
        }
        let context = EventContext {
            event_seq: events.len() as u32,
            timestamp,
            source_path: source_path.clone(),
            source_event_id: Some(
                record
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("line:{line_index}")),
            ),
            message_seq: messages.len().checked_sub(1).map(|seq| seq as u32),
            parser_version: EVENT_PARSER_VERSION,
        };
        match record_type {
            "message" => {
                let role = match record.get("role").and_then(Value::as_str) {
                    Some("user") => Role::User,
                    Some("assistant") => Role::Assistant,
                    _ => continue,
                };
                let content = message_text(record.get("content"));
                if !content.is_empty() {
                    messages.push(RawMessage { role, content, timestamp });
                }
            }
            "function_call" if include_events => {
                if let Some(event) = tool_call_event(&record, context, directory.as_deref()) {
                    if let (Some(id), Some(name)) = (&event.tool_call_id, &event.name) {
                        call_names.insert(id.clone(), name.clone());
                    }
                    events.push(event);
                }
            }
            "function_call_result" if include_events => {
                events.push(tool_result_event(&record, context, &call_names));
            }
            _ => {}
        }
    }

    let usage_events = usage.finish();
    if messages.is_empty() && usage_events.is_empty() && events.is_empty() {
        return Ok(None);
    }
    let started_at = first_timestamp(None, &messages, &usage_events, &events).unwrap_or(mtime_ms);
    let parent = parent_session_id(path);
    let mut session =
        RawSession::search_only(session_id, directory, started_at, Some(mtime_ms), None, messages);
    session.source_file_path = source_path;
    session.custom_title = titles.best();
    let is_subagent = path.parent().and_then(Path::file_name).is_some_and(|dir| dir == "subagents");
    session.thread_role =
        Some(if is_subagent { ThreadRole::Subagent } else { ThreadRole::Primary });
    session.parent_links = parent
        .into_iter()
        .map(|parent| ParentLink {
            relation: ParentRelation::Spawn,
            source: SOURCE.to_string(),
            source_id: parent,
        })
        .collect();
    session.metadata_parser_version = Some(METADATA_PARSER_VERSION);
    session = session.with_usage(usage_events, USAGE_PARSER_VERSION);
    if include_events {
        session = session.with_events(events, EVENT_PARSER_VERSION);
    }
    Ok(Some(session))
}

fn is_client_plumbing(record: &Value) -> bool {
    let Some(provider_data) = record.get("providerData") else {
        return false;
    };
    let flag = |key: &str| provider_data.get(key).and_then(Value::as_bool) == Some(true);
    ["skipRun", "isMeta", "isCompactInternal", "isCompacted", "isSummary"].into_iter().any(flag)
        || provider_data.get("compactType").is_some_and(Value::is_string)
        || provider_data.get("agent").and_then(Value::as_str) == Some("compact")
        || provider_data
            .pointer("/teammateMessage/from")
            .and_then(Value::as_str)
            .is_some_and(|from| !from.is_empty())
}

fn message_text(content: Option<&Value>) -> String {
    let parts: Vec<&str> = match content {
        Some(Value::String(text)) => vec![text.as_str()],
        Some(Value::Array(items)) => items
            .iter()
            .filter(|item| {
                matches!(
                    item.get("type").and_then(Value::as_str),
                    Some("input_text" | "output_text" | "text")
                )
            })
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect(),
        _ => Vec::new(),
    };
    parts
        .into_iter()
        .map(str::trim)
        .filter(|text| !text.is_empty() && !is_envelope(text))
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_envelope(text: &str) -> bool {
    ENVELOPE_PREFIXES.iter().any(|prefix| text.starts_with(prefix))
}

fn tool_call_event(
    record: &Value,
    context: EventContext,
    cwd: Option<&str>,
) -> Option<RawSessionEvent> {
    let name = record.get("name").and_then(Value::as_str).filter(|name| !name.is_empty())?;
    let args = match record.get("arguments") {
        Some(Value::String(text)) => serde_json::from_str::<Value>(text).ok(),
        Some(other) => Some(other.clone()),
        None => None,
    };
    let arg = |key: &str| {
        args.as_ref()
            .and_then(|args| args.get(key))
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
    };
    let mut event = events::tool_call_event(context, name.to_string(), args.as_ref());
    let operation = match name {
        "Read" => Some(FileOperation::Read),
        "Edit" | "MultiEdit" | "Write" => Some(FileOperation::Write),
        _ => None,
    };
    if let Some(operation) = operation
        && let Some(path) = arg("file_path")
    {
        event.files.push(FileEvidence::call(path.to_string(), operation, cwd.map(str::to_string)));
    } else if name == "Bash"
        && let Some(command) = arg("command")
    {
        let (files, status) = events::shell_file_evidence(command, cwd);
        event.files = files;
        event.command_evidence_status = Some(status);
    }
    event.tool_call_id = call_id(record);
    Some(event)
}

fn tool_result_event(
    record: &Value,
    context: EventContext,
    call_names: &HashMap<String, String>,
) -> RawSessionEvent {
    let call_id = call_id(record);
    let name = call_id.as_ref().and_then(|id| call_names.get(id)).cloned();
    let mut event = events::tool_result_event(context, name, output_text(record.get("output")));
    let raw_response = record.pointer("/providerData/toolResult/rawResponse");
    event.status = raw_response
        .and_then(|raw| json_i64(raw.get("exitCode")))
        .map(|code| if code == 0 { "success" } else { "error" }.to_string());
    event.tool_call_id = call_id;
    event.attrs_json = Some(record.to_string());
    event
}

fn call_id(record: &Value) -> Option<String> {
    record.get("callId").and_then(Value::as_str).filter(|id| !id.is_empty()).map(str::to_string)
}

fn output_text(output: Option<&Value>) -> Option<String> {
    let text = match output? {
        Value::String(text) => text.clone(),
        Value::Object(item) => item.get("text").and_then(Value::as_str)?.to_string(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.to_string(),
    };
    (!text.trim().is_empty()).then_some(text)
}

#[derive(Default)]
struct UsageCollector {
    events: Vec<RawUsageEvent>,
    by_key: HashMap<String, usize>,
}

impl UsageCollector {
    fn observe(
        &mut self,
        record: &Value,
        line_index: usize,
        timestamp: i64,
        source_path: Option<&str>,
    ) {
        let Some(event) = usage_event(record, line_index, timestamp, source_path) else {
            return;
        };
        let key = event.event_key.clone();
        match self.by_key.get(&key) {
            Some(&index) if total(&self.events[index]) >= total(&event) => {}
            Some(&index) => {
                self.events[index] =
                    RawUsageEvent { event_seq: self.events[index].event_seq, ..event }
            }
            None => {
                self.by_key.insert(key, self.events.len());
                self.events.push(RawUsageEvent { event_seq: self.events.len() as u32, ..event });
            }
        }
    }

    fn finish(self) -> Vec<RawUsageEvent> {
        self.events
    }
}

fn total(event: &RawUsageEvent) -> i64 {
    event.input_tokens
        + event.output_tokens
        + event.cache_read_tokens
        + event.cache_write_tokens
        + event.reasoning_tokens
}

fn usage_event(
    record: &Value,
    line_index: usize,
    timestamp: i64,
    source_path: Option<&str>,
) -> Option<RawUsageEvent> {
    let provider_data = record.get("providerData")?;
    let usage = provider_data.get("usage").filter(|usage| usage.is_object());
    let raw_usage = provider_data.get("rawUsage").filter(|usage| usage.is_object());
    let detail = raw_usage.or(usage)?;
    let counts = usage.or(raw_usage)?;
    let reported_input = usage_count(counts, &["inputTokens", "input_tokens", "prompt_tokens"]);
    let reported_output =
        usage_count(counts, &["outputTokens", "output_tokens", "completion_tokens"]);
    let cache_read_tokens = first_positive(&[
        json_i64(detail.get("cache_read_input_tokens")),
        json_i64(detail.get("cacheReadInputTokens")),
        detail_sum(detail.get("inputTokensDetails"), "cached_tokens"),
        json_i64(detail.pointer("/prompt_tokens_details/cached_tokens")),
        json_i64(detail.get("prompt_cache_hit_tokens")),
    ]);
    let cache_write_tokens = first_positive(&[
        json_i64(detail.get("cache_creation_input_tokens")),
        json_i64(detail.get("cacheCreationInputTokens")),
        json_i64(detail.get("prompt_cache_write_tokens")),
        json_i64(detail.get("prompt_cache_miss_tokens")),
    ]);
    let input_includes_cache = [
        "prompt_tokens",
        "prompt_tokens_details",
        "prompt_cache_hit_tokens",
        "prompt_cache_miss_tokens",
        "inputTokensDetails",
    ]
    .iter()
    .any(|key| detail.get(*key).is_some());
    let input_tokens = if input_includes_cache {
        (reported_input - cache_read_tokens - cache_write_tokens).max(0)
    } else {
        reported_input
    };
    let reasoning_tokens = first_positive(&[
        detail_sum(detail.get("outputTokensDetails"), "reasoning_tokens"),
        detail_sum(counts.get("outputTokensDetails"), "reasoning_tokens"),
        json_i64(detail.pointer("/completion_tokens_details/reasoning_tokens")),
    ])
    .min(reported_output);
    let output_tokens = reported_output - reasoning_tokens;
    if input_tokens == 0
        && output_tokens == 0
        && cache_read_tokens == 0
        && cache_write_tokens == 0
        && reasoning_tokens == 0
    {
        return None;
    }
    let text = |key: &str| {
        provider_data
            .get(key)
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty())
            .map(str::to_string)
    };
    let model =
        text("model").or_else(|| text("requestModelId")).unwrap_or_else(|| "unknown".into());
    let event_key = text("messageId")
        .or_else(|| record.get("id").and_then(Value::as_str).map(str::to_string))
        .map(|id| format!("message:{id}"))
        .unwrap_or_else(|| format!("line:{line_index}"));
    Some(RawUsageEvent {
        model,
        provider: SOURCE.to_string(),
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_write_tokens,
        reasoning_tokens,
        source_path: source_path.map(str::to_string),
        raw_usage_json: Some(detail.to_string()),
        ..RawUsageEvent::observed(event_key, 0, timestamp, USAGE_PARSER_VERSION)
    })
}

fn first_positive(counts: &[Option<i64>]) -> i64 {
    counts.iter().flatten().copied().find(|count| *count > 0).unwrap_or(0)
}

fn detail_sum(details: Option<&Value>, key: &str) -> Option<i64> {
    match details? {
        Value::Array(items) => Some(items.iter().filter_map(|item| json_i64(item.get(key))).sum()),
        item => json_i64(item.get(key)),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const SESSION: &str = "01a11229-2c02-7000-8659-d8ffe1b926bf";

    fn write(path: &Path, records: &[Value]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body: String = records.iter().map(|record| format!("{record}\n")).collect();
        fs::write(path, body).unwrap();
    }

    fn main_transcript(config_dir: &Path) -> PathBuf {
        let path = config_dir.join("projects/private-tmp-repo").join(format!("{SESSION}.jsonl"));
        write(
            &path,
            &[
                json!({"type": "message", "role": "user", "timestamp": 1_791_306_050_600i64, "cwd": "/private/tmp/repo",
                    "content": [{"type": "input_text", "text": "<system-reminder>plan mode</system-reminder>"}]}),
                json!({"type": "message", "role": "user", "timestamp": 1_791_306_050_601i64,
                    "content": [{"type": "input_text", "text": "/review"}], "providerData": {"skipRun": true}}),
                json!({"id": "u1", "type": "message", "role": "user", "timestamp": 1_791_306_050_671i64,
                    "content": [{"type": "input_text", "text": "rename the flag"}]}),
                json!({"type": "file-history-snapshot", "timestamp": 1_791_306_050_680i64}),
                json!({"id": "f1", "type": "function_call", "timestamp": 1_791_306_050_700i64, "callId": "c1", "name": "Edit",
                    "arguments": "{\"file_path\":\"/private/tmp/repo/main.go\",\"old_string\":\"oldFlag\",\"new_string\":\"newFlag\"}",
                    "providerData": {"model": "glm-5.2", "messageId": "m1", "usage": {"inputTokens": 10000, "outputTokens": 300, "totalTokens": 10300, "inputTokensDetails": [{"cached_tokens": 9000}], "outputTokensDetails": [{"reasoning_tokens": 50}]}, "rawUsage": {"prompt_tokens": 10000, "completion_tokens": 300, "prompt_tokens_details": {"cached_tokens": 9000}, "completion_tokens_details": {"reasoning_tokens": 50}}}}),
                json!({"id": "f2", "type": "function_call", "timestamp": 1_791_306_050_701i64, "callId": "c2", "name": "Bash",
                    "arguments": {"command": "go test ./..."},
                    "providerData": {"model": "glm-5.2", "messageId": "m1", "usage": {"inputTokens": 10000, "outputTokens": 300, "totalTokens": 10300, "inputTokensDetails": [{"cached_tokens": 9000}], "outputTokensDetails": [{"reasoning_tokens": 50}]}, "rawUsage": {"prompt_tokens": 10000, "completion_tokens": 300, "prompt_tokens_details": {"cached_tokens": 9000}, "completion_tokens_details": {"reasoning_tokens": 50}}}}),
                json!({"id": "r2", "type": "function_call_result", "timestamp": 1_791_306_050_800i64, "callId": "c2",
                    "output": {"type": "text", "text": "FAIL ./flags"},
                    "providerData": {"toolResult": {"rawResponse": {"exitCode": 1}}}}),
                json!({"id": "a1", "type": "message", "role": "assistant", "timestamp": 1_791_306_050_900i64, "status": "completed",
                    "content": [{"type": "output_text", "text": "Renamed; one test still fails."}],
                    "providerData": {"model": "glm-5.2", "messageId": "m2", "usage": {"inputTokens": 100, "outputTokens": 20, "cacheReadInputTokens": 200}}}),
                json!({"id": "mu1", "type": "model-usage", "timestamp": 1_791_306_050_910i64,
                    "providerData": {"model": "glm-5.2", "messageId": "m3", "usage": {"inputTokens": 40, "outputTokens": 0}}}),
                json!({"type": "ai-title", "aiTitle": "Generated title", "timestamp": 1_791_306_050_950i64}),
                json!({"type": "custom-title", "customTitle": "Flag rename", "timestamp": 1_791_306_050_960i64}),
                json!({"type": "ai-title", "aiTitle": "(No content)", "timestamp": 1_791_306_050_970i64}),
            ],
        );
        path
    }

    #[test]
    fn parses_conversation_tools_and_usage() {
        let root = tempfile::tempdir().unwrap();
        let path = main_transcript(root.path());

        let session = parse_transcript(&path, SESSION, 1_791_306_051_000, true).unwrap().unwrap();

        let messages: Vec<_> =
            session.messages.iter().map(|m| (m.role.as_str(), m.content.as_str())).collect();
        assert_eq!(
            messages,
            [("user", "rename the flag"), ("assistant", "Renamed; one test still fails.")]
        );
        assert_eq!(session.directory.as_deref(), Some("/private/tmp/repo"));
        assert_eq!(session.custom_title.as_deref(), Some("Flag rename"));
        assert_eq!(session.thread_role, Some(ThreadRole::Primary));

        let edit = session.events.iter().find(|e| e.name.as_deref() == Some("Edit")).unwrap();
        assert_eq!(edit.files[0].path, "/private/tmp/repo/main.go");
        assert_eq!(edit.files[0].operation, FileOperation::Write);
        let result = session.events.iter().find(|e| e.kind == "tool_result").unwrap();
        assert_eq!(result.name.as_deref(), Some("Bash"));
        assert_eq!(result.status.as_deref(), Some("error"));
        assert_eq!(result.summary.as_deref(), Some("FAIL ./flags"));

        let usage: Vec<_> = session
            .usage_events
            .iter()
            .map(|u| {
                (
                    u.event_key.as_str(),
                    u.input_tokens,
                    u.cache_read_tokens,
                    u.output_tokens,
                    u.reasoning_tokens,
                )
            })
            .collect();
        assert_eq!(
            usage,
            [
                ("message:m1", 1000, 9000, 250, 50),
                ("message:m2", 100, 200, 20, 0),
                ("message:m3", 40, 0, 0, 0),
            ]
        );
        assert!(session.usage_events.iter().all(|u| u.model == "glm-5.2"));
    }

    #[test]
    fn indexes_subagent_transcripts_with_parent_link() {
        let root = tempfile::tempdir().unwrap();
        main_transcript(root.path());
        let child = root
            .path()
            .join("projects/private-tmp-repo")
            .join(SESSION)
            .join("subagents/agent-a1b2.jsonl");
        write(
            &child,
            &[json!({"type": "message", "role": "user", "timestamp": 1_791_306_050_700i64,
                "content": [{"type": "input_text", "text": "find callers of oldFlag"}]})],
        );
        write(
            &root.path().join("projects/private-tmp-repo").join(format!("{SESSION}.meta.json")),
            &[],
        );

        let mut ids: Vec<_> =
            collect_entries(root.path()).into_iter().map(|entry| entry.session_id).collect();
        ids.sort();
        assert_eq!(ids, [SESSION, "agent-a1b2"]);

        let session = parse_transcript(&child, "agent-a1b2", 1, true).unwrap().unwrap();
        assert_eq!(session.thread_role, Some(ThreadRole::Subagent));
        assert_eq!(session.parent_links[0].source_id, SESSION);
        assert_eq!(session.parent_links[0].source, "codebuddy");

        let team_member =
            root.path().join("projects/private-tmp-repo/release-team/subagents/task-1.jsonl");
        fs::copy(&child, {
            fs::create_dir_all(team_member.parent().unwrap()).unwrap();
            &team_member
        })
        .unwrap();
        let session = parse_transcript(&team_member, "task-1", 1, true).unwrap().unwrap();
        assert_eq!(session.thread_role, Some(ThreadRole::Subagent));
        assert!(session.parent_links.is_empty());
    }
}
