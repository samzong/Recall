use std::fs;
use std::io::BufRead;
use std::path::{Path, PathBuf};

use serde_json::Value;
use tracing::warn;
use walkdir::WalkDir;

use crate::adapters::AdapterSyncContext;
use crate::adapters::events::{
    EventContext, shell_file_evidence, tool_call_event, tool_result_event,
};
use crate::adapters::file_scan::{self, FileMetadataSnapshot, FileScanEntry, FileScanOptions};
use crate::adapters::json_util::{json_i64, jsonl_indexed, rfc3339_ms};
use crate::adapters::usage::usage_count;
use crate::adapters::{
    RawMessage, RawSession, ResumeCommand, SourceAdapter, SyncScanResult, first_timestamp,
};
use crate::types::{FileEvidence, FileEvidenceKind, FileOperation, RawUsageEvent, Role};

const USAGE_PARSER_VERSION: u32 = 1;
const EVENT_PARSER_VERSION: u32 = 1;

pub(crate) struct DroidAdapter;

impl SourceAdapter for DroidAdapter {
    fn id(&self) -> &str {
        "droid"
    }

    fn label(&self) -> &str {
        "DR"
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

    fn start_command(&self, prompt: String) -> Option<ResumeCommand> {
        Some(crate::adapters::prompt_start("droid", prompt))
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let Some(sessions_dir) = resolve_sessions_dir() else {
            return Ok(vec![]);
        };
        scan_droid_sessions(&sessions_dir)
    }

    fn scan_for_sync(
        &self,
        context: &AdapterSyncContext,
        since_ts: Option<i64>,
        include_events: bool,
    ) -> anyhow::Result<Option<SyncScanResult>> {
        let Some(sessions_dir) = resolve_sessions_dir() else {
            return Ok(Some(SyncScanResult {
                sessions: vec![],
                stats: Default::default(),
                observations: Vec::new(),
            }));
        };
        Ok(Some(file_scan::run_file_scan_with_options_and_snapshot(
            context,
            since_ts,
            FileScanOptions {
                usage_parser_version: Some(USAGE_PARSER_VERSION),
                event_parser_version: include_events.then_some(EVENT_PARSER_VERSION),
                metadata_parser_version: None,
            },
            collect_session_entries(&sessions_dir),
            droid_session_snapshot,
            |entry, mtime| parse_droid_session_file(entry, mtime, include_events),
        )?))
    }
}

fn resolve_sessions_dir() -> Option<PathBuf> {
    resolve_sessions_dir_from(std::env::var("DROID_SESSIONS_DIR").ok(), dirs::home_dir())
}

fn resolve_sessions_dir_from(env_dir: Option<String>, home: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(raw) = env_dir.as_deref().map(str::trim).filter(|value| !value.is_empty()) {
        let dir = expand_user_path(raw, home.as_deref());
        if !dir.exists() {
            tracing::debug!("DROID_SESSIONS_DIR not found, skipping Droid");
            return None;
        }
        return Some(dir);
    }
    let home = home?;
    let dir = home.join(".factory/sessions");
    if !dir.exists() {
        tracing::debug!("~/.factory/sessions not found, skipping Droid");
        return None;
    }
    Some(dir)
}

fn expand_user_path(raw: &str, home: Option<&Path>) -> PathBuf {
    if raw == "~" {
        return home.map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from(raw));
    }
    if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
        return home.map(|home| home.join(rest)).unwrap_or_else(|| PathBuf::from(raw));
    }
    PathBuf::from(raw)
}

fn scan_droid_sessions(sessions_dir: &Path) -> anyhow::Result<Vec<RawSession>> {
    let mut sessions = Vec::new();
    for entry in collect_session_entries(sessions_dir) {
        let Some(snapshot) = droid_session_snapshot(&entry) else {
            continue;
        };
        if let Some(raw) =
            parse_droid_session_file(entry.clone(), snapshot.effective_mtime_ms(), true)?
            && droid_session_snapshot(&entry).as_ref() == Some(&snapshot)
        {
            sessions.push(raw);
        }
    }
    Ok(sessions)
}

fn collect_session_entries(sessions_dir: &Path) -> Vec<FileScanEntry> {
    let mut entries = Vec::new();
    if !sessions_dir.exists() {
        return entries;
    }

    for dir_entry in
        WalkDir::new(sessions_dir).max_depth(3).into_iter().filter_map(|entry| entry.ok())
    {
        let path = dir_entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with('.') || !name.ends_with(".jsonl") {
            continue;
        }
        let Some(session_id) =
            path.file_stem().and_then(|stem| stem.to_str()).filter(|id| !id.is_empty())
        else {
            continue;
        };
        entries.push(FileScanEntry {
            session_id: session_id.to_string(),
            stat_target: path.to_path_buf(),
            directory: None,
        });
    }
    entries
}

fn settings_path(jsonl: &Path) -> PathBuf {
    let stem = jsonl.file_stem().and_then(|stem| stem.to_str()).unwrap_or_default();
    jsonl.with_file_name(format!("{stem}.settings.json"))
}

#[derive(Debug, PartialEq, Eq)]
struct DroidSessionSnapshot {
    jsonl: FileMetadataSnapshot,
    settings: Option<FileMetadataSnapshot>,
}

fn droid_session_snapshot(
    entry: &FileScanEntry,
) -> Option<file_scan::FileScanSnapshot<DroidSessionSnapshot>> {
    let jsonl = file_scan::file_metadata_snapshot(&entry.stat_target)?;
    let settings = file_scan::file_metadata_snapshot(&settings_path(&entry.stat_target));
    let settings_mtime = settings.as_ref().and_then(FileMetadataSnapshot::mtime_ms);
    let effective_mtime_ms = match (jsonl.mtime_ms()?, settings_mtime) {
        (jsonl_mtime, Some(settings_mtime)) => jsonl_mtime.max(settings_mtime),
        (jsonl_mtime, None) => jsonl_mtime,
    };
    Some(file_scan::FileScanSnapshot::new(
        effective_mtime_ms,
        DroidSessionSnapshot { jsonl, settings },
    ))
}

fn parse_droid_session_file(
    entry: FileScanEntry,
    mtime_ms: i64,
    include_events: bool,
) -> anyhow::Result<Option<RawSession>> {
    match parse_droid_session_file_impl(&entry, mtime_ms, include_events) {
        Ok(raw) => Ok(raw),
        Err(error) => {
            warn!("failed to parse {}: {error}", entry.stat_target.display());
            Ok(None)
        }
    }
}

fn parse_droid_session_file_impl(
    entry: &FileScanEntry,
    mtime_ms: i64,
    include_events: bool,
) -> anyhow::Result<Option<RawSession>> {
    let file = fs::File::open(&entry.stat_target)?;
    let reader = std::io::BufReader::new(file);
    let lines =
        reader.lines().enumerate().map(|(index, line)| {
            line.map(|value| {
                if index == 0 { value.trim_start_matches('\u{feff}').to_string() } else { value }
            })
        });
    let settings = read_settings(&settings_path(&entry.stat_target))?;
    parse_droid_jsonl(
        lines,
        entry.session_id.clone(),
        mtime_ms,
        entry.stat_target.to_str().map(str::to_string),
        settings,
        include_events,
    )
}

struct SettingsMeta {
    model: Option<String>,
    duration_minutes: Option<u32>,
    usage: Option<Value>,
    source_path: Option<String>,
}

fn read_settings(path: &Path) -> anyhow::Result<Option<SettingsMeta>> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let value: Value = serde_json::from_str(&content)?;
    settings_meta_from_value(value, path.to_str().map(str::to_string))
        .map(Some)
        .ok_or_else(|| anyhow::anyhow!("invalid Droid session settings"))
}

fn settings_meta_from_value(value: Value, source_path: Option<String>) -> Option<SettingsMeta> {
    if !value.is_object() {
        return None;
    }
    let model = value
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_string);
    let duration_minutes = json_i64(value.get("assistantActiveTimeMs"))
        .filter(|ms| *ms > 0)
        .and_then(|ms| u32::try_from(ms / 60_000).ok());
    let usage = value.get("tokenUsage").cloned().filter(Value::is_object);
    Some(SettingsMeta { model, duration_minutes, usage, source_path })
}

fn parse_droid_jsonl(
    lines: impl Iterator<Item = std::io::Result<String>>,
    session_id: String,
    mtime_ms: i64,
    source_path: Option<String>,
    settings: Option<SettingsMeta>,
    include_events: bool,
) -> anyhow::Result<Option<RawSession>> {
    let mut messages = Vec::new();
    let mut events = Vec::new();
    let mut directory = None;
    let mut custom_title = None;
    let mut started_meta = None;

    for item in jsonl_indexed(lines) {
        let (line_index, record) = item?;
        match record.get("type").and_then(Value::as_str).unwrap_or("") {
            "session_start" => {
                if directory.is_none() {
                    directory = record
                        .get("cwd")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|cwd| !cwd.is_empty())
                        .map(str::to_string);
                }
                if custom_title.is_none() {
                    custom_title = record
                        .get("sessionTitle")
                        .or_else(|| record.get("title"))
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|title| !title.is_empty())
                        .map(str::to_string);
                }
                if started_meta.is_none() {
                    started_meta = rfc3339_ms(record.get("timestamp"))
                        .or_else(|| json_i64(record.get("timestamp")));
                }
            }
            "message" => {
                if include_events {
                    for (part_index, part) in record
                        .pointer("/message/content")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .enumerate()
                    {
                        let context = EventContext {
                            event_seq: events.len() as u32,
                            timestamp: rfc3339_ms(record.get("timestamp"))
                                .or_else(|| json_i64(record.get("timestamp"))),
                            source_path: source_path.clone(),
                            source_event_id: Some(format!(
                                "{}:line:{line_index}:part:{part_index}",
                                record.get("id").and_then(Value::as_str).unwrap_or("")
                            )),
                            message_seq: messages
                                .len()
                                .checked_sub(1)
                                .and_then(|index| u32::try_from(index).ok()),
                            parser_version: EVENT_PARSER_VERSION,
                        };
                        let mut event = match part.get("type").and_then(Value::as_str) {
                            Some("tool_use") => {
                                let Some(name) = part
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .filter(|name| !name.trim().is_empty())
                                else {
                                    continue;
                                };
                                let args = part.get("input");
                                let mut event = tool_call_event(context, name.to_string(), args);
                                event.kind = "tool_call".to_string();
                                event.target = None;
                                event.tool_call_id = part
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .filter(|id| !id.is_empty())
                                    .map(str::to_string);
                                let operation = match name {
                                    "Read" => Some(FileOperation::Read),
                                    "Edit" | "Create" => Some(FileOperation::Write),
                                    _ => None,
                                };
                                if let (Some(operation), Some(path)) = (
                                    operation,
                                    args.and_then(|args| args.get("file_path"))
                                        .and_then(Value::as_str)
                                        .filter(|path| !path.trim().is_empty()),
                                ) {
                                    event.kind = if operation == FileOperation::Read {
                                        "file_read"
                                    } else {
                                        "file_write"
                                    }
                                    .to_string();
                                    event.target = Some(path.to_string());
                                    event.files.push(FileEvidence {
                                        path: path.to_string(),
                                        operation,
                                        kind: FileEvidenceKind::Call,
                                        cwd: directory.clone(),
                                        target: None,
                                    });
                                }
                                if name == "Execute" {
                                    event.kind = "command".to_string();
                                    event.target = args
                                        .and_then(|args| args.get("command"))
                                        .and_then(Value::as_str)
                                        .map(str::to_string);
                                    if let Some(command) = event.target.as_deref() {
                                        let (files, status) =
                                            shell_file_evidence(command, directory.as_deref());
                                        event.files = files;
                                        event.command_evidence_status = Some(status);
                                    }
                                }
                                event
                            }
                            Some("tool_result") => {
                                let mut event = tool_result_event(
                                    context,
                                    None,
                                    part.get("content").map(|content| {
                                        content
                                            .as_str()
                                            .map(str::to_string)
                                            .unwrap_or_else(|| content.to_string())
                                    }),
                                );
                                event.tool_call_id = part
                                    .get("tool_use_id")
                                    .and_then(Value::as_str)
                                    .filter(|id| !id.is_empty())
                                    .map(str::to_string);
                                event.status =
                                    part.get("is_error").and_then(Value::as_bool).map(|error| {
                                        if error { "error" } else { "success" }.to_string()
                                    });
                                event
                            }
                            _ => continue,
                        };
                        event.attrs_json = Some(record.to_string());
                        events.push(event);
                    }
                }
                let role = match record.pointer("/message/role").and_then(Value::as_str) {
                    Some("user") => Role::User,
                    Some("assistant") => Role::Assistant,
                    _ => continue,
                };
                let content = extract_text(record.pointer("/message/content"));
                if content.is_empty() {
                    continue;
                }
                messages.push(RawMessage {
                    role,
                    content,
                    timestamp: rfc3339_ms(record.get("timestamp"))
                        .or_else(|| json_i64(record.get("timestamp"))),
                });
            }
            _ => {}
        }
    }

    let usage_events = settings
        .as_ref()
        .and_then(|meta| {
            extract_settings_usage(
                meta,
                &session_id,
                mtime_ms,
                messages.last().and_then(|message| message.timestamp),
            )
        })
        .into_iter()
        .collect::<Vec<_>>();

    if messages.is_empty() && usage_events.is_empty() && events.is_empty() {
        return Ok(None);
    }

    let started_at =
        first_timestamp(started_meta, &messages, &usage_events, &events).unwrap_or(mtime_ms);
    let mut session =
        RawSession::search_only(session_id, directory, started_at, Some(mtime_ms), None, messages);
    session.source_file_path = source_path;
    session.custom_title = custom_title;
    session.duration_minutes = settings.as_ref().and_then(|meta| meta.duration_minutes);
    session = session.with_usage(usage_events, USAGE_PARSER_VERSION);
    if include_events {
        session = session.with_events(events, EVENT_PARSER_VERSION);
    }
    Ok(Some(session))
}

fn extract_text(content: Option<&Value>) -> String {
    let Some(content) = content else {
        return String::new();
    };
    if let Some(text) = content.as_str() {
        return text.trim().to_string();
    }
    let Some(parts) = content.as_array() else {
        return String::new();
    };
    let text = parts
        .iter()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .filter(|text| !text.trim_start().starts_with("<system"))
        .collect::<Vec<_>>()
        .join("\n");
    text.trim().to_string()
}

fn extract_settings_usage(
    settings: &SettingsMeta,
    session_id: &str,
    mtime_ms: i64,
    last_message_ts: Option<i64>,
) -> Option<RawUsageEvent> {
    let usage = settings.usage.as_ref()?;
    let input_tokens = usage_count(usage, &["inputTokens"]);
    let output_tokens = usage_count(usage, &["outputTokens"]);
    let cache_write_tokens = usage_count(usage, &["cacheCreationTokens"]);
    let cache_read_tokens = usage_count(usage, &["cacheReadTokens"]);
    let reasoning_tokens = usage_count(usage, &["thinkingTokens"]);
    if input_tokens == 0
        && output_tokens == 0
        && cache_write_tokens == 0
        && cache_read_tokens == 0
        && reasoning_tokens == 0
    {
        return None;
    }

    Some(RawUsageEvent {
        model: settings.model.clone().unwrap_or_else(|| "unknown".to_string()),
        provider: "droid".to_string(),
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_write_tokens,
        reasoning_tokens,
        source_path: settings.source_path.clone(),
        raw_usage_json: Some(usage.to_string()),
        ..RawUsageEvent::observed(
            format!("settings:{session_id}"),
            0,
            last_message_ts.unwrap_or(mtime_ms),
            USAGE_PARSER_VERSION,
        )
    })
}

#[cfg(test)]
fn parse_droid_session(
    jsonl: &str,
    settings: Option<&str>,
    session_id: &str,
) -> Option<RawSession> {
    let lines = jsonl.lines().map(|line| Ok(line.to_string()));
    let settings = settings.and_then(|content| {
        settings_meta_from_value(
            serde_json::from_str(content).ok()?,
            Some("/tmp/session.settings.json".to_string()),
        )
    });
    parse_droid_jsonl(lines, session_id.to_string(), 1_700_000_000_000, None, settings, true)
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::store::Store;

    const SESSION_ID: &str = "550e8400-e29b-41d4-a716-446655440000";

    fn fixture_jsonl() -> String {
        [
            r#"{"type":"session_start","id":"550e8400-e29b-41d4-a716-446655440000","sessionTitle":"Fix auth","title":"ignored","cwd":"/Users/x/git/demo","timestamp":"2024-01-01T00:00:00.000Z"}"#,
            r#"{"type":"message","id":"user-1","parentId":null,"timestamp":"2024-01-01T00:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"fix the login"},{"type":"text","text":"<system-reminder>hide</system-reminder>"},{"type":"tool_result","text":"noise"}]}}"#,
            r#"{"type":"message","id":"assistant-1","parentId":"user-1","timestamp":"2024-01-01T00:00:05.000Z","message":{"role":"assistant","content":[{"type":"text","text":"working on it"},{"type":"tool_use","id":"read-1","name":"Read","input":{"file_path":"src/auth.rs"}},{"type":"tool_use","id":"edit-1","name":"Edit","input":{"file_path":"src/auth.rs","old_str":"old","new_str":"new","change_all":false},"thought_signature":"signature","namespace":"native","script_execution":{"run_id":"run-1","outer_tool_use_id":"outer-1"}}]}}"#,
            r#"{"type":"message","id":"result-1","parentId":"assistant-1","timestamp":"2024-01-01T00:00:06.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"read-1","is_error":false,"content":"skip"},{"type":"tool_result","tool_use_id":"edit-1","content":"result without status"}]}}"#,
        ]
        .join("\n")
    }

    fn fixture_settings() -> &'static str {
        r#"{"model":"claude-sonnet-4-5","autonomyMode":"medium","assistantActiveTimeMs":180000,"tokenUsage":{"inputTokens":100,"outputTokens":20,"cacheCreationTokens":10,"cacheReadTokens":5,"thinkingTokens":3}}"#
    }

    fn write_session(root: &Path, project: &str, jsonl: &str, settings: Option<&str>) -> PathBuf {
        let dir = root.join(project);
        fs::create_dir_all(&dir).unwrap();
        let jsonl_path = dir.join(format!("{SESSION_ID}.jsonl"));
        fs::write(&jsonl_path, jsonl).unwrap();
        if let Some(settings) = settings {
            fs::write(dir.join(format!("{SESSION_ID}.settings.json")), settings).unwrap();
        }
        jsonl_path
    }

    #[test]
    fn parse_extracts_messages_title_cwd_and_settings_usage() {
        let session =
            parse_droid_session(&fixture_jsonl(), Some(fixture_settings()), SESSION_ID).unwrap();

        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, Role::User);
        assert_eq!(session.messages[0].content, "fix the login");
        assert_eq!(session.messages[1].role, Role::Assistant);
        assert_eq!(session.messages[1].content, "working on it");
        assert_eq!(session.directory.as_deref(), Some("/Users/x/git/demo"));
        assert_eq!(session.custom_title.as_deref(), Some("Fix auth"));
        assert_eq!(session.started_at, 1_704_067_200_000);
        assert_eq!(session.duration_minutes, Some(3));

        assert_eq!(session.events.len(), 5);
        let read = &session.events[1];
        assert_eq!(read.tool_call_id.as_deref(), Some("read-1"));
        assert_eq!(read.message_seq, Some(0));
        assert_eq!(read.source_event_id.as_deref(), Some("assistant-1:line:2:part:1"));
        assert_eq!(read.files[0].cwd.as_deref(), Some("/Users/x/git/demo"));
        assert_eq!(session.events[2].files[0].operation, FileOperation::Write);
        assert!(session.events[2].attrs_json.as_deref().unwrap().contains("thought_signature"));
        assert_eq!(session.events[3].status.as_deref(), Some("success"));
        assert!(session.events[4].status.is_none());
        assert!(session.events[4].files.is_empty());
        assert_eq!(session.events[4].message_seq, Some(1));
        assert_eq!(session.event_parser_version, Some(EVENT_PARSER_VERSION));
        assert_eq!(session.usage_events.len(), 1);
        let event = &session.usage_events[0];
        assert_eq!(event.event_key, format!("settings:{SESSION_ID}"));
        assert_eq!(event.model, "claude-sonnet-4-5");
        assert_eq!(event.provider, "droid");
        assert_eq!(event.input_tokens, 100);
        assert_eq!(event.output_tokens, 20);
        assert_eq!(event.cache_write_tokens, 10);
        assert_eq!(event.cache_read_tokens, 5);
        assert_eq!(event.reasoning_tokens, 3);
        assert_eq!(event.token_source, crate::types::TokenSource::Observed);
        assert_eq!(session.usage_parser_version, Some(USAGE_PARSER_VERSION));
    }

    #[test]
    fn parse_accepts_string_content_and_title_fallback() {
        let jsonl = [
            r#"{"type":"session_start","title":"Plain title","cwd":"/repo"}"#,
            r#"{"type":"message","timestamp":"2024-01-01T00:00:01.000Z","message":{"role":"user","content":"hello"}}"#,
        ]
        .join("\n");
        let session = parse_droid_session(&jsonl, None, SESSION_ID).unwrap();
        assert_eq!(session.messages[0].content, "hello");
        assert_eq!(session.custom_title.as_deref(), Some("Plain title"));
        assert!(session.usage_events.is_empty());
    }

    #[test]
    fn parse_distinguishes_empty_and_tool_only_records() {
        assert!(parse_droid_session("", None, SESSION_ID).is_none());
        let jsonl = r#"{"type":"message","id":"write-message","message":{"role":"assistant","content":[{"type":"tool_use","id":"write-1","name":"Create","input":{"file_path":"/repo/new.rs","content":"new contents"}}]}}"#;
        let session = parse_droid_session(jsonl, None, SESSION_ID).unwrap();
        assert!(session.messages.is_empty());
        assert!(session.usage_events.is_empty());
        assert_eq!(session.events[0].files[0].operation, FileOperation::Write);
        assert!(session.events[0].message_seq.is_none());
        for cwd in [Some("/repo"), None] {
            let header = serde_json::json!({"type":"session_start","cwd":cwd});
            let call = serde_json::json!({"type":"message","id":"execute-record","message":{"role":"assistant","content":[{"type":"tool_use","id":"execute-1","name":"Execute","input":{"command":"git restore -- src/auth.rs"}}]}});
            let command = parse_droid_session(
                &(header.to_string() + "\n" + &call.to_string()),
                None,
                SESSION_ID,
            )
            .unwrap();
            assert!(command.messages.is_empty());
            assert_eq!(command.events[0].kind, "command");
            assert_eq!(command.events[0].files[0].path, "src/auth.rs");
            assert_eq!(command.events[0].files[0].cwd.as_deref(), cwd);
            assert_eq!(command.events[0].files[0].kind, FileEvidenceKind::Command);
            assert_eq!(
                command.events[0].command_evidence_status,
                Some(if cwd.is_some() {
                    crate::types::CommandEvidenceStatus::Complete
                } else {
                    crate::types::CommandEvidenceStatus::Unsupported
                })
            );
        }
        assert!(
            parse_droid_jsonl(
                jsonl.lines().map(|line| Ok(line.to_string())),
                SESSION_ID.to_string(),
                0,
                None,
                None,
                false
            )
            .unwrap()
            .is_none()
        );
        assert!(
            parse_droid_session(r#"{"type":"session_start","cwd":"/repo"}"#, None, SESSION_ID)
                .is_none()
        );
    }

    #[test]
    fn parse_skips_malformed_jsonl_lines() {
        let jsonl = [
            r#"{"type":"session_start","cwd":"/repo","sessionTitle":"ok"}"#,
            "not-json",
            r#"{"type":"message","timestamp":"2024-01-01T00:00:01.000Z","message":{"role":"user","content":"kept"}}"#,
        ]
        .join("\n");
        let session = parse_droid_session(&jsonl, None, SESSION_ID).unwrap();
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].content, "kept");
        assert!(
            parse_droid_jsonl(
                std::iter::once(Err(std::io::Error::from(std::io::ErrorKind::InvalidData))),
                SESSION_ID.to_string(),
                0,
                None,
                None,
                true
            )
            .is_err()
        );
    }

    #[test]
    fn scan_discovers_project_jsonl_and_ignores_sidecars() {
        let root = tempfile::tempdir().unwrap();
        write_session(root.path(), "-Users-x-git-demo", &fixture_jsonl(), Some(fixture_settings()));
        fs::write(root.path().join("-Users-x-git-demo").join("notes.txt"), "ignore").unwrap();
        fs::write(root.path().join(".favorites"), "[]").unwrap();

        let sessions = scan_droid_sessions(root.path()).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].source_id, SESSION_ID);
        assert_eq!(sessions[0].directory.as_deref(), Some("/Users/x/git/demo"));
        assert_eq!(sessions[0].usage_events.len(), 1);
        let path =
            root.path().join("-Users-x-git-demo").join(format!("{SESSION_ID}.settings.json"));
        fs::write(&path, "{\"model\":").unwrap();
        assert!(scan_droid_sessions(root.path()).unwrap().is_empty());
        fs::write(&path, [0xff]).unwrap();
        assert!(scan_droid_sessions(root.path()).unwrap().is_empty());
        fs::write(&path, fixture_settings()).unwrap();
        assert_eq!(scan_droid_sessions(root.path()).unwrap()[0].usage_events.len(), 1);
    }

    #[test]
    fn scan_does_not_invent_cwd_from_project_slug() {
        let root = tempfile::tempdir().unwrap();
        let jsonl = r#"{"type":"message","timestamp":"2024-01-01T00:00:01.000Z","message":{"role":"user","content":"hello"}}"#;
        write_session(root.path(), "-Users-x-git-demo", jsonl, None);

        let sessions = scan_droid_sessions(root.path()).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].directory, None);
    }

    #[test]
    fn missing_sessions_dir_yields_no_sessions() {
        let root = tempfile::tempdir().unwrap();
        assert!(scan_droid_sessions(root.path()).unwrap().is_empty());
    }

    #[test]
    fn resolve_prefers_existing_env_dir() {
        let root = tempfile::tempdir().unwrap();
        let resolved = resolve_sessions_dir_from(
            Some(root.path().to_string_lossy().into_owned()),
            Some(PathBuf::from("/unused-home")),
        );
        assert_eq!(resolved.as_deref(), Some(root.path()));
    }

    #[test]
    fn resolve_missing_env_dir_skips() {
        let resolved = resolve_sessions_dir_from(Some("/no/such/droid-sessions".to_string()), None);
        assert!(resolved.is_none());
    }

    #[test]
    fn resolve_uses_home_factory_sessions() {
        let home = tempfile::tempdir().unwrap();
        let sessions = home.path().join(".factory/sessions");
        fs::create_dir_all(&sessions).unwrap();
        let resolved = resolve_sessions_dir_from(None, Some(home.path().to_path_buf()));
        assert_eq!(resolved.as_deref(), Some(sessions.as_path()));
    }

    #[test]
    fn incremental_scan_skips_unchanged_snapshot() {
        let root = tempfile::tempdir().unwrap();
        write_session(root.path(), "-Users-x-git-demo", &fixture_jsonl(), Some(fixture_settings()));

        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let first = file_scan::run_file_scan_with_options_and_snapshot(
            &AdapterSyncContext::from_store_for_test(&store, "droid").unwrap(),
            None,
            FileScanOptions {
                usage_parser_version: Some(USAGE_PARSER_VERSION),
                event_parser_version: None,
                metadata_parser_version: None,
            },
            collect_session_entries(root.path()),
            droid_session_snapshot,
            |entry, mtime| parse_droid_session_file(entry, mtime, false),
        )
        .unwrap();
        assert_eq!(first.stats.parsed, 1);
        assert_eq!(first.sessions.len(), 1);

        store
            .conn
            .execute(
                "INSERT INTO sessions (id, source, source_id, title, started_at, updated_at, message_count) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    "stored-id",
                    "droid",
                    SESSION_ID,
                    "Fix auth",
                    1_704_067_200_000_i64,
                    first.sessions[0].updated_at.unwrap(),
                    2,
                ],
            )
            .unwrap();
        store
            .persist_usage_events_for_existing_session(
                "droid",
                SESSION_ID,
                &[],
                USAGE_PARSER_VERSION,
                first.sessions[0].updated_at,
            )
            .unwrap();

        let second = file_scan::run_file_scan_with_options_and_snapshot(
            &AdapterSyncContext::from_store_for_test(&store, "droid").unwrap(),
            None,
            FileScanOptions {
                usage_parser_version: Some(USAGE_PARSER_VERSION),
                event_parser_version: None,
                metadata_parser_version: None,
            },
            collect_session_entries(root.path()),
            droid_session_snapshot,
            |entry, mtime| parse_droid_session_file(entry, mtime, false),
        )
        .unwrap();
        assert_eq!(second.stats.parsed, 0);
        assert!(second.sessions.is_empty());
        for version in [None, Some(EVENT_PARSER_VERSION - 1), Some(EVENT_PARSER_VERSION)] {
            if let Some(version) = version {
                store
                    .persist_session_events_for_existing_session(
                        "droid",
                        SESSION_ID,
                        &[],
                        version,
                        first.sessions[0].updated_at,
                    )
                    .unwrap();
            }
            let result = file_scan::run_file_scan_with_options_and_snapshot(
                &AdapterSyncContext::from_store_for_test(&store, "droid").unwrap(),
                None,
                FileScanOptions {
                    usage_parser_version: Some(USAGE_PARSER_VERSION),
                    event_parser_version: Some(EVENT_PARSER_VERSION),
                    metadata_parser_version: None,
                },
                collect_session_entries(root.path()),
                droid_session_snapshot,
                |entry, mtime| parse_droid_session_file(entry, mtime, true),
            )
            .unwrap();
            assert_eq!(result.sessions.len(), usize::from(version != Some(EVENT_PARSER_VERSION)));
            if let Some(session) = result.sessions.first() {
                assert_eq!(session.events.len(), 5);
            }
        }
    }

    #[test]
    fn resume_uses_official_flag() {
        let command = DroidAdapter.resume_command(SESSION_ID).unwrap();
        assert_eq!(command.program, "droid");
        assert_eq!(command.args, vec!["--resume", SESSION_ID]);
    }
}
