use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;
use tracing::warn;
use walkdir::WalkDir;

use crate::adapters::AdapterSyncContext;
use crate::adapters::events;
use crate::adapters::file_scan::{self, FileScanEntry, FileScanOptions};
use crate::adapters::json_util::{json_i64, jsonl_indexed, rfc3339_ms};
use crate::adapters::usage::usage_count;
use crate::adapters::{
    RawMessage, RawSession, ResumeCommand, SourceAdapter, SyncScanResult, SyncScanStats,
    first_timestamp,
};
use crate::types::{RawSessionEvent, RawUsageEvent, Role};

const SOURCE: &str = "factory";
const USAGE_PARSER_VERSION: u32 = 1;
const EVENT_PARSER_VERSION: u32 = 1;

pub(crate) struct FactoryAdapter;

impl SourceAdapter for FactoryAdapter {
    fn id(&self) -> &str {
        SOURCE
    }

    fn label(&self) -> &str {
        "FAC"
    }

    fn usage_parser_version(&self) -> Option<u32> {
        Some(USAGE_PARSER_VERSION)
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        Some(ResumeCommand {
            program: "droid".to_string(),
            args: vec!["--resume".to_string(), source_id.to_string()],
        })
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let mut sessions = Vec::new();
        for entry in collect_session_entries(&sessions_dir()) {
            let Some(mtime_ms) = file_scan::stat_mtime_ms(&entry.stat_target) else {
                continue;
            };
            if let Some(raw) = parse_session_entry(entry, mtime_ms, true)? {
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
        let Some(root) = sessions_dir() else {
            return Ok(Some(SyncScanResult {
                sessions: Vec::new(),
                stats: SyncScanStats::default(),
                observations: Vec::new(),
            }));
        };
        Ok(Some(file_scan::run_file_scan_with_options(
            context,
            since_ts,
            FileScanOptions {
                usage_parser_version: Some(USAGE_PARSER_VERSION),
                event_parser_version: include_events.then_some(EVENT_PARSER_VERSION),
                metadata_parser_version: None,
            },
            collect_session_entries(&Some(root)),
            |entry, mtime_ms| parse_session_entry(entry, mtime_ms, include_events),
        )?))
    }
}

fn sessions_dir() -> Option<PathBuf> {
    sessions_dir_from(dirs::home_dir())
}

fn sessions_dir_from(home: Option<PathBuf>) -> Option<PathBuf> {
    let dir = home?.join(".factory/sessions");
    dir.is_dir().then_some(dir)
}

fn collect_session_entries(root: &Option<PathBuf>) -> Vec<FileScanEntry> {
    let Some(root) = root else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for entry in WalkDir::new(root).into_iter().filter_map(Result::ok) {
        let path = entry.path();
        if !path.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(session_id) = path.file_stem().and_then(|stem| stem.to_str()).map(str::to_string)
        else {
            continue;
        };
        entries.push(FileScanEntry {
            session_id,
            stat_target: path.to_path_buf(),
            directory: None,
        });
    }
    entries
}

fn parse_session_entry(
    entry: FileScanEntry,
    mtime_ms: i64,
    include_events: bool,
) -> anyhow::Result<Option<RawSession>> {
    match parse_session_file(&entry.stat_target, &entry.session_id, mtime_ms, include_events) {
        Ok(raw) => Ok(raw),
        Err(err) => {
            warn!("failed to parse Factory session {}: {err}", entry.stat_target.display());
            Ok(None)
        }
    }
}

fn parse_session_file(
    path: &Path,
    fallback_id: &str,
    mtime_ms: i64,
    include_events: bool,
) -> anyhow::Result<Option<RawSession>> {
    let file = fs::File::open(path)?;
    let settings = read_settings(&settings_path(path));
    parse_factory_jsonl(
        BufReader::new(file).lines(),
        fallback_id,
        mtime_ms,
        path.to_str().map(str::to_string),
        settings.as_ref(),
        include_events,
    )
}

fn settings_path(jsonl: &Path) -> PathBuf {
    let stem = jsonl.file_stem().and_then(|stem| stem.to_str()).unwrap_or_default();
    jsonl.with_file_name(format!("{stem}.settings.json"))
}

fn read_settings(path: &Path) -> Option<Value> {
    let raw = fs::read_to_string(path).ok()?;
    match serde_json::from_str(&raw) {
        Ok(value) => Some(value),
        Err(err) => {
            warn!("failed to parse Factory settings {}: {err}", path.display());
            None
        }
    }
}

fn parse_factory_jsonl(
    lines: impl Iterator<Item = std::io::Result<String>>,
    fallback_id: &str,
    mtime_ms: i64,
    source_path: Option<String>,
    settings: Option<&Value>,
    include_events: bool,
) -> anyhow::Result<Option<RawSession>> {
    let mut messages = Vec::new();
    let mut events = Vec::new();
    let mut source_id = fallback_id.to_string();
    let mut directory = None;
    let mut custom_title = None;

    for item in jsonl_indexed(lines) {
        let (_index, record) = item?;
        take_meta(&record, &mut source_id, &mut directory, &mut custom_title);
        let kind = record_kind(&record);
        let timestamp =
            json_i64(record.get("timestamp")).or_else(|| rfc3339_ms(record.get("timestamp")));
        match kind {
            "user" | "human" => {
                if let Some(content) = record_text(&record) {
                    messages.push(RawMessage { role: Role::User, content, timestamp });
                }
            }
            "assistant" | "agent" => {
                if let Some(content) = record_text(&record) {
                    messages.push(RawMessage { role: Role::Assistant, content, timestamp });
                }
                if include_events {
                    collect_tool_calls(&record, timestamp, source_path.as_deref(), &mut events);
                }
            }
            "tool" | "tool_result" | "tool_use" if include_events => {
                if kind == "tool_use" {
                    collect_tool_calls(&record, timestamp, source_path.as_deref(), &mut events);
                } else {
                    events.push(events::tool_result_event(
                        events::EventContext {
                            event_seq: events.len() as u32,
                            timestamp,
                            source_path: source_path.clone(),
                            source_event_id: json_opt(&record, &["id", "tool_use_id"]),
                            message_seq: None,
                            parser_version: EVENT_PARSER_VERSION,
                        },
                        json_opt(&record, &["name", "tool"]),
                        record_text(&record),
                    ));
                }
            }
            _ => {}
        }
    }

    if custom_title.is_none() {
        custom_title = settings.and_then(|value| json_opt(value, &["title"]));
    }

    let usage_events = settings
        .map(|value| session_usage(value, &source_id, mtime_ms, source_path.as_deref()))
        .unwrap_or_default();
    if messages.is_empty() && usage_events.is_empty() && events.is_empty() {
        return Ok(None);
    }

    let started_at = first_timestamp(None, &messages, &usage_events, &events).unwrap_or(mtime_ms);
    let mut raw =
        RawSession::search_only(source_id, directory, started_at, Some(mtime_ms), None, messages)
            .with_usage(usage_events, USAGE_PARSER_VERSION);
    raw.source_file_path = source_path;
    raw.custom_title = custom_title;
    if include_events {
        raw = raw.with_events(events, EVENT_PARSER_VERSION);
    }
    Ok(Some(raw))
}

fn take_meta(
    record: &Value,
    source_id: &mut String,
    directory: &mut Option<String>,
    custom_title: &mut Option<String>,
) {
    if let Some(id) = json_opt(record, &["sessionId", "session_id", "id"])
        && record_kind(record).is_empty()
    {
        *source_id = id;
    }
    if directory.is_none() {
        *directory = json_opt(record, &["cwd", "workingDirectory", "working_directory"]);
    }
    if custom_title.is_none() {
        *custom_title = json_opt(record, &["title"]);
    }
}

fn record_kind(record: &Value) -> &str {
    record.get("type").or_else(|| record.get("role")).and_then(Value::as_str).unwrap_or("")
}

fn record_text(record: &Value) -> Option<String> {
    extract_text(record.get("content"))
        .or_else(|| extract_text(record.pointer("/message/content")))
        .or_else(|| extract_text(record.get("text")))
}

fn extract_text(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(text)) => {
            let text = text.trim();
            if text.is_empty() { None } else { Some(text.to_string()) }
        }
        Some(Value::Array(blocks)) => {
            let text = blocks.iter().filter_map(block_text).collect::<Vec<_>>().join("\n");
            let text = text.trim();
            if text.is_empty() { None } else { Some(text.to_string()) }
        }
        _ => None,
    }
}

fn block_text(block: &Value) -> Option<&str> {
    if let Some(text) = block.as_str().map(str::trim).filter(|text| !text.is_empty()) {
        return Some(text);
    }
    let kind = block.get("type").and_then(Value::as_str).unwrap_or("text");
    if kind != "text" {
        return None;
    }
    block.get("text").and_then(Value::as_str).map(str::trim).filter(|text| !text.is_empty())
}

fn collect_tool_calls(
    record: &Value,
    timestamp: Option<i64>,
    source_path: Option<&str>,
    events_out: &mut Vec<RawSessionEvent>,
) {
    if record_kind(record) == "tool_use"
        && let Some(name) = json_opt(record, &["name", "tool"])
    {
        events_out.push(events::tool_call_event(
            events::EventContext {
                event_seq: events_out.len() as u32,
                timestamp,
                source_path: source_path.map(str::to_string),
                source_event_id: json_opt(record, &["id"]),
                message_seq: None,
                parser_version: EVENT_PARSER_VERSION,
            },
            name,
            record.get("input").or_else(|| record.get("arguments")),
        ));
    }
    let content = record.get("content").or_else(|| record.pointer("/message/content"));
    let Some(Value::Array(blocks)) = content else {
        return;
    };
    for block in blocks {
        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
            continue;
        }
        let Some(name) = json_opt(block, &["name", "tool"]) else {
            continue;
        };
        events_out.push(events::tool_call_event(
            events::EventContext {
                event_seq: events_out.len() as u32,
                timestamp,
                source_path: source_path.map(str::to_string),
                source_event_id: json_opt(block, &["id"]),
                message_seq: None,
                parser_version: EVENT_PARSER_VERSION,
            },
            name,
            block.get("input").or_else(|| block.get("arguments")),
        ));
    }
}

fn session_usage(
    settings: &Value,
    session_id: &str,
    timestamp: i64,
    source_path: Option<&str>,
) -> Vec<RawUsageEvent> {
    let Some(usage) = settings.get("tokenUsage").or_else(|| settings.get("token_usage")) else {
        return Vec::new();
    };
    let input_tokens = usage_count(usage, &["inputTokens", "input_tokens", "input"]);
    let output_tokens = usage_count(usage, &["outputTokens", "output_tokens", "output"]);
    let cache_read_tokens =
        usage_count(usage, &["cacheReadTokens", "cache_read_tokens", "cache", "cacheRead"]);
    let cache_write_tokens = usage_count(
        usage,
        &["cacheCreationTokens", "cache_creation_tokens", "cacheWriteTokens", "cache_write"],
    );
    let reasoning_tokens =
        usage_count(usage, &["thinkingTokens", "thinking_tokens", "thinking", "reasoning"]);
    if input_tokens == 0
        && output_tokens == 0
        && cache_read_tokens == 0
        && cache_write_tokens == 0
        && reasoning_tokens == 0
    {
        return Vec::new();
    }
    let model = json_opt(settings, &["model"]).unwrap_or_else(|| "unknown".to_string());
    let provider = json_opt(settings, &["provider"]).unwrap_or_else(|| "factory".to_string());
    let mut event = RawUsageEvent::observed(
        format!("settings:{session_id}"),
        0,
        timestamp,
        USAGE_PARSER_VERSION,
    );
    event.model = model;
    event.provider = provider;
    event.input_tokens = input_tokens;
    event.output_tokens = output_tokens;
    event.cache_read_tokens = cache_read_tokens;
    event.cache_write_tokens = cache_write_tokens;
    event.reasoning_tokens = reasoning_tokens;
    event.source_path = source_path.map(str::to_string);
    event.raw_usage_json = Some(usage.to_string());
    vec![event]
}

fn json_opt(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixtures_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/factory")
    }

    fn fixture_session() -> PathBuf {
        fixtures_dir()
            .join("-Users-alice-Dev-myproject")
            .join("11111111-2222-4333-8444-555555555555.jsonl")
    }

    #[test]
    fn resume_uses_droid_flag() {
        let command =
            FactoryAdapter.resume_command("11111111-2222-4333-8444-555555555555").unwrap();
        assert_eq!(command.program, "droid");
        assert_eq!(command.args, vec!["--resume", "11111111-2222-4333-8444-555555555555"]);
    }

    #[test]
    fn missing_sessions_dir_is_empty() {
        let home = tempfile::tempdir().unwrap();
        assert!(sessions_dir_from(Some(home.path().to_path_buf())).is_none());
        assert!(collect_session_entries(&None).is_empty());
    }

    #[test]
    fn only_factory_sessions_are_walked() {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join(".factory/other")).unwrap();
        fs::write(home.path().join(".factory/settings.json"), "{}").unwrap();
        fs::create_dir_all(home.path().join(".factory/sessions")).unwrap();
        assert_eq!(
            sessions_dir_from(Some(home.path().to_path_buf())).unwrap(),
            home.path().join(".factory/sessions")
        );
        assert!(
            collect_session_entries(&sessions_dir_from(Some(home.path().to_path_buf()))).is_empty()
        );
    }

    #[test]
    fn scan_reads_slug_jsonl_and_settings_token_usage() {
        let home = tempfile::tempdir().unwrap();
        let dest = home.path().join(".factory/sessions/-Users-alice-Dev-myproject");
        fs::create_dir_all(&dest).unwrap();
        fs::copy(fixture_session(), dest.join("11111111-2222-4333-8444-555555555555.jsonl"))
            .unwrap();
        fs::copy(
            fixtures_dir()
                .join("-Users-alice-Dev-myproject")
                .join("11111111-2222-4333-8444-555555555555.settings.json"),
            dest.join("11111111-2222-4333-8444-555555555555.settings.json"),
        )
        .unwrap();

        let entries = collect_session_entries(&sessions_dir_from(Some(home.path().to_path_buf())));
        assert_eq!(entries.len(), 1);
        let session =
            parse_session_entry(entries.into_iter().next().unwrap(), 99, true).unwrap().unwrap();
        assert_eq!(session.source_id, "11111111-2222-4333-8444-555555555555");
        assert_eq!(session.directory.as_deref(), Some("/Users/alice/Dev/myproject"));
        assert_eq!(session.custom_title.as_deref(), Some("Fix auth"));
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].content, "fix the login");
        assert_eq!(session.messages[1].content, "looking at auth.py");
        assert_eq!(session.events.len(), 2);
        assert_eq!(session.events[0].name.as_deref(), Some("read"));
        assert_eq!(session.events[0].target.as_deref(), Some("/Users/alice/Dev/myproject/auth.py"));
        assert_eq!(session.events[1].kind, "tool_result");
        assert_eq!(session.usage_events.len(), 1);
        assert_eq!(session.usage_events[0].model, "claude-sonnet-4");
        assert_eq!(session.usage_events[0].input_tokens, 100);
        assert_eq!(session.usage_events[0].output_tokens, 40);
        assert_eq!(session.usage_events[0].cache_read_tokens, 10);
        assert_eq!(session.usage_events[0].cache_write_tokens, 5);
        assert_eq!(session.usage_events[0].reasoning_tokens, 8);
    }

    #[test]
    fn settings_json_is_not_a_session() {
        let home = tempfile::tempdir().unwrap();
        let dest = home.path().join(".factory/sessions/proj");
        fs::create_dir_all(&dest).unwrap();
        fs::write(
            dest.join("sess.settings.json"),
            r#"{"model":"x","tokenUsage":{"inputTokens":1}}"#,
        )
        .unwrap();
        assert!(collect_session_entries(&Some(home.path().join(".factory/sessions"))).is_empty());
    }
}
