use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use crate::adapters::events::{
    EventContext, shell_file_evidence, tool_call_event, tool_result_event,
};
use crate::adapters::sync_state::{parser_state_is_current, session_state_is_current};
use crate::adapters::usage::usage_count;
use crate::adapters::{
    AdapterSyncContext, RawMessage, RawSession, ResumeCommand, SourceAdapter, SourceObservation,
    SyncScanResult,
};
use crate::types::{
    FileEvidence, FileOperation, ParentLink, ParentRelation, RawUsageEvent, Role, ThreadRole,
};

const USAGE_PARSER_VERSION: u32 = 1;
const EVENT_PARSER_VERSION: u32 = 2;
const METADATA_PARSER_VERSION: u32 = 2;

pub(crate) struct ZedAdapter;

impl SourceAdapter for ZedAdapter {
    fn id(&self) -> &str {
        "zed"
    }

    fn label(&self) -> &str {
        "ZED"
    }

    fn usage_parser_version(&self) -> Option<u32> {
        Some(USAGE_PARSER_VERSION)
    }

    fn scan(&self) -> Result<Vec<RawSession>> {
        Ok(scan_database(&database_path()?, None, None, true)?.sessions)
    }

    fn scan_for_sync(
        &self,
        context: &AdapterSyncContext,
        since_ts: Option<i64>,
        include_events: bool,
    ) -> Result<Option<SyncScanResult>> {
        Ok(Some(scan_database(&database_path()?, Some(context), since_ts, include_events)?))
    }

    fn resume_command(&self, _source_id: &str) -> Option<ResumeCommand> {
        None
    }

    fn start_command(&self, prompt: String) -> Option<ResumeCommand> {
        let mut url = String::from("zed://agent?prompt=");
        for byte in prompt.bytes() {
            if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                url.push(char::from(byte));
            } else {
                write!(url, "%{byte:02X}").ok()?;
            }
        }
        Some(ResumeCommand { program: "zed".into(), args: vec![url] })
    }
}

fn database_path() -> Result<PathBuf> {
    let data = dirs::data_local_dir().context("cannot determine Zed data directory")?;
    let application = if cfg!(target_os = "linux") { "zed" } else { "Zed" };
    Ok(data.join(application).join("threads/threads.db"))
}

struct ThreadMetadata {
    id: String,
    title: String,
    updated_at: String,
    data_type: String,
    created_at: Option<String>,
    folder_paths: Option<String>,
    folder_paths_order: Option<String>,
    parent_id: Option<String>,
}

fn scan_database(
    path: &Path,
    context: Option<&AdapterSyncContext>,
    since_ts: Option<i64>,
    include_events: bool,
) -> Result<SyncScanResult> {
    let mut result = SyncScanResult::default();
    if !path.exists() {
        return Ok(result);
    }
    let mut conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("cannot open Zed threads database {}", path.display()))?;
    let tx = conn.transaction()?;
    let columns: HashSet<String> = tx
        .prepare("SELECT name FROM pragma_table_info('threads')")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let optional = |name: &str| {
        if columns.contains(name) { name.to_string() } else { format!("NULL AS {name}") }
    };
    let query = format!(
        "SELECT id, summary, updated_at, data_type, {}, {}, {}, {} FROM threads ORDER BY id",
        optional("created_at"),
        optional("folder_paths"),
        optional("folder_paths_order"),
        optional("parent_id"),
    );
    let mut statement = tx.prepare(&query)?;
    let rows = statement.query_map([], |row| {
        Ok(ThreadMetadata {
            id: row.get(0)?,
            title: row.get(1)?,
            updated_at: row.get(2)?,
            data_type: row.get(3)?,
            created_at: row.get(4)?,
            folder_paths: row.get(5)?,
            folder_paths_order: row.get(6)?,
            parent_id: row.get(7)?,
        })
    })?;
    for row in rows {
        let meta = row?;
        if context.and_then(AdapterSyncContext::target_source_id).is_some_and(|id| id != meta.id) {
            continue;
        }
        result.stats.candidates += 1;
        let updated_at = timestamp(&meta.updated_at);
        if since_ts.is_some_and(|cutoff| updated_at.is_some_and(|ts| ts < cutoff)) {
            result.stats.filtered_sessions += 1;
            result.stats.rejected_before_parse += 1;
            continue;
        }
        if let Some(context) = context
            && updated_at.is_some()
            && context.session_meta().get(&meta.id).is_some_and(|old| old.updated_at == updated_at)
            && parser_state_is_current(
                METADATA_PARSER_VERSION,
                context.metadata_state().get(&meta.id).copied(),
                updated_at,
            )
            && session_state_is_current(
                USAGE_PARSER_VERSION,
                EVENT_PARSER_VERSION,
                context.usage_state().get(&meta.id).copied(),
                context.event_state().get(&meta.id).copied(),
                updated_at,
                include_events,
            )
        {
            result.stats.skipped_sessions += 1;
            result.stats.rejected_before_parse += 1;
            result.observations.push(SourceObservation {
                source_id: meta.id,
                source_file_path: Some(path.to_string_lossy().into_owned()),
            });
            continue;
        }
        let parsed = (|| {
            let data =
                tx.query_row("SELECT data FROM threads WHERE id = ?1", [&meta.id], |row| {
                    Ok(row.get_ref(0)?.as_bytes()?.to_vec())
                })?;
            let json = match meta.data_type.as_str() {
                "zstd" => zstd::decode_all(data.as_slice())?,
                "json" => data,
                other => bail!("unsupported Zed data type {other}"),
            };
            parse_thread(&serde_json::from_slice(&json)?, &meta, path, include_events)
        })();
        match parsed {
            Ok(Some(raw)) => {
                result.stats.parsed += 1;
                if let Some(raw) = match context {
                    Some(context) => context.stream_session(raw)?,
                    None => Some(raw),
                } {
                    result.sessions.push(raw);
                }
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(thread_id = %meta.id, path = %path.display(), "failed to parse Zed thread: {error}")
            }
        }
    }
    Ok(result)
}

fn timestamp(text: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(text).ok().map(|ts| ts.timestamp_millis())
}

fn parse_thread(
    thread: &Value,
    meta: &ThreadMetadata,
    path: &Path,
    include_events: bool,
) -> Result<Option<RawSession>> {
    let current = match thread.get("version").and_then(Value::as_str) {
        Some("0.3.0") => true,
        Some("0.2.0" | "0.1.0") | None => false,
        Some(other) => bail!("unsupported Zed thread version {other}"),
    };
    let source_messages =
        thread.get("messages").and_then(Value::as_array).context("missing Zed messages")?;
    let snapshot = thread.get("initial_project_snapshot");
    let updated_at = timestamp(&meta.updated_at)
        .or_else(|| thread.get("updated_at").and_then(Value::as_str).and_then(timestamp))
        .context("missing Zed thread timestamp")?;
    let started_at = snapshot
        .and_then(|snapshot| snapshot.get("timestamp"))
        .and_then(Value::as_str)
        .and_then(timestamp)
        .or_else(|| meta.created_at.as_deref().and_then(timestamp))
        .unwrap_or(updated_at);
    let directory = meta
        .folder_paths
        .as_deref()
        .and_then(|paths| {
            let paths: Vec<_> = paths.lines().collect();
            let first = meta
                .folder_paths_order
                .as_deref()
                .and_then(|order| {
                    order
                        .split(',')
                        .map(|index| index.parse::<usize>().ok())
                        .collect::<Option<Vec<_>>>()
                })
                .filter(|order| {
                    order.len() == paths.len()
                        && order.iter().all(|index| *index < paths.len())
                        && order.iter().collect::<HashSet<_>>().len() == paths.len()
                })
                .and_then(|order| order.iter().position(|index| *index == 0))
                .unwrap_or(0);
            paths.get(first).filter(|path| !path.is_empty()).map(|path| (*path).to_string())
        })
        .or_else(|| {
            snapshot
                .and_then(|snapshot| snapshot.get("worktree_snapshots"))
                .and_then(Value::as_array)
                .and_then(|worktrees| worktrees.first())
                .and_then(|worktree| worktree.get("worktree_path"))
                .and_then(Value::as_str)
                .map(str::to_string)
        });
    let mut messages = Vec::new();
    let mut events = Vec::new();
    let mut tools = HashMap::new();
    for (index, source_message) in source_messages.iter().enumerate() {
        let (role, message) = if current {
            if let Some(message) = source_message.get("User") {
                (Role::User, message)
            } else if let Some(message) = source_message.get("Agent") {
                (Role::Assistant, message)
            } else {
                continue;
            }
        } else {
            match source_message.get("role").and_then(Value::as_str) {
                Some("user") => (Role::User, source_message),
                Some("assistant") => (Role::Assistant, source_message),
                _ => continue,
            }
        };
        let hidden = message.get("is_hidden").and_then(Value::as_bool).unwrap_or(false);
        let content = if current {
            text_parts(message.get("content"))
        } else if let Some(segments) = message.get("segments").and_then(Value::as_array) {
            segments
                .iter()
                .filter(|segment| segment.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|segment| segment.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            message.get("text").and_then(Value::as_str).unwrap_or_default().to_string()
        };
        let message_seq = (!hidden && !content.trim().is_empty()).then_some(messages.len() as u32);
        if message_seq.is_some() {
            messages.push(RawMessage { role, content, timestamp: None });
        }
        if !include_events {
            continue;
        }
        let calls = if current {
            message
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|part| part.get("ToolUse"))
                .collect::<Vec<_>>()
        } else {
            message.get("tool_uses").and_then(Value::as_array).into_iter().flatten().collect()
        };
        for call in calls {
            let Some(id) = call.get("id").and_then(Value::as_str) else { continue };
            let Some(name) = call.get("name").and_then(Value::as_str) else { continue };
            let args = call.get("input").map(|input| {
                if input
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| kind == "json" || kind == "text")
                {
                    input.get("value").unwrap_or(input)
                } else {
                    input
                }
            });
            let mut event = tool_call_event(
                event_context(events.len(), index, message_seq, path),
                name.to_string(),
                args,
            );
            let scoped_id = format!("message:{index}:{id}");
            event.tool_call_id = Some(scoped_id.clone());
            if name == "terminal" {
                event.kind = "command".into();
                let command = args.and_then(|args| args.get("command")).and_then(Value::as_str);
                let cwd = args
                    .and_then(|args| args.get("cd"))
                    .and_then(Value::as_str)
                    .or(directory.as_deref());
                if let Some(command) = command {
                    let (files, status) = shell_file_evidence(command, cwd);
                    event.files = files;
                    event.command_evidence_status = Some(status);
                }
            } else if matches!(name, "read_file" | "edit_file" | "write_file" | "delete_path")
                && let Some(target) = event.target.as_ref()
            {
                let operation = match name {
                    "read_file" => FileOperation::Read,
                    "delete_path" => FileOperation::Delete,
                    _ => FileOperation::Write,
                };
                event.kind =
                    if operation == FileOperation::Read { "file_read" } else { "file_write" }
                        .into();
                event.files.push(FileEvidence::call(target.clone(), operation, directory.clone()));
            } else if matches!(name, "copy_path" | "move_path") {
                event.kind = "file_write".into();
                for (key, operation) in if name == "move_path" {
                    [
                        ("source_path", FileOperation::MoveFrom),
                        ("destination_path", FileOperation::MoveTo),
                    ]
                } else {
                    [
                        ("source_path", FileOperation::Read),
                        ("destination_path", FileOperation::Write),
                    ]
                } {
                    if let Some(target) =
                        args.and_then(|args| args.get(key)).and_then(Value::as_str)
                    {
                        event.files.push(FileEvidence::call(
                            target.into(),
                            operation,
                            directory.clone(),
                        ));
                    }
                }
                event.target = args
                    .and_then(|args| args.get("destination_path"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            tools.insert(id.to_string(), (name.to_string(), scoped_id));
            events.push(event);
        }
        let results = match message.get("tool_results") {
            Some(Value::Object(results)) => results.values().collect::<Vec<_>>(),
            Some(Value::Array(results)) => results.iter().collect(),
            _ => Vec::new(),
        };
        for result in results {
            let Some(id) = result.get("tool_use_id").and_then(Value::as_str) else { continue };
            let name = result
                .get("tool_name")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| tools.get(id).map(|(name, _)| name.clone()));
            let mut event = tool_result_event(
                event_context(events.len(), index, message_seq, path),
                name,
                Some(text_parts(result.get("content"))),
            );
            event.tool_call_id = Some(
                tools
                    .get(id)
                    .map(|(_, scoped_id)| scoped_id.clone())
                    .unwrap_or_else(|| format!("message:{index}:{id}")),
            );
            event.status = result
                .get("is_error")
                .and_then(Value::as_bool)
                .map(|error| if error { "error" } else { "success" }.to_string());
            events.push(event);
        }
    }
    if messages.is_empty() {
        return Ok(None);
    }
    let mut raw =
        RawSession::search_only(&meta.id, directory, started_at, Some(updated_at), None, messages)
            .with_usage(parse_usage(thread, updated_at, path), USAGE_PARSER_VERSION);
    if include_events {
        raw = raw.with_events(events, EVENT_PARSER_VERSION);
    }
    raw.metadata_parser_version = Some(METADATA_PARSER_VERSION);
    raw.refresh_session_on_metadata_backfill = true;
    if let Some(parent) = meta.parent_id.as_deref().map(str::trim).filter(|id| !id.is_empty()) {
        raw.thread_role = Some(ThreadRole::Subagent);
        raw.parent_links = vec![ParentLink {
            relation: ParentRelation::Spawn,
            source: "zed".into(),
            source_id: parent.into(),
        }];
    }
    raw.source_file_path = Some(path.to_string_lossy().into_owned());
    raw.custom_title = thread
        .get("title")
        .or_else(|| thread.get("summary"))
        .and_then(Value::as_str)
        .filter(|title| !title.trim().is_empty())
        .map(str::to_string)
        .or_else(|| (!meta.title.trim().is_empty()).then(|| meta.title.clone()));
    Ok(Some(raw))
}

fn text_parts(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .map(|part| text_parts(Some(part)))
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Some(Value::Object(part)) => part
            .get("Text")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| mention_label(part.get("Mention")?.get("uri")))
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn mention_label(uri: Option<&Value>) -> Option<String> {
    let uri = uri?.as_object()?;
    if uri.len() != 1 {
        return None;
    }
    let (kind, data) = uri.iter().next()?;
    let field = match kind.as_str() {
        "File" | "Directory" | "Symbol" | "Selection" => "abs_path",
        "Thread" | "Rule" | "Skill" | "PastedImage" => "name",
        "Fetch" => "url",
        "GitDiff" => "base_ref",
        "MergeConflict" => "file_path",
        _ => return None,
    };
    let reference = data.get(field)?.as_str()?;
    if reference.trim().is_empty() {
        return None;
    }
    if kind == "Symbol"
        && let Some(name) = data.get("name").and_then(Value::as_str)
        && !name.trim().is_empty()
    {
        Some(format!("@{reference}#{name}"))
    } else {
        Some(format!("@{reference}"))
    }
}

fn event_context(seq: usize, index: usize, message_seq: Option<u32>, path: &Path) -> EventContext {
    EventContext {
        event_seq: seq as u32,
        timestamp: None,
        source_path: Some(path.to_string_lossy().into_owned()),
        source_event_id: Some(format!("message:{index}:event:{seq}")),
        message_seq,
        parser_version: EVENT_PARSER_VERSION,
    }
}

fn parse_usage(thread: &Value, updated_at: i64, path: &Path) -> Vec<RawUsageEvent> {
    let Some(usage) = thread.get("cumulative_token_usage") else { return Vec::new() };
    let input = usage_count(usage, &["input_tokens"]);
    let output = usage_count(usage, &["output_tokens"]);
    let cache_read = usage_count(usage, &["cache_read_input_tokens"]);
    let cache_write = usage_count(usage, &["cache_creation_input_tokens"]);
    if input == 0 && output == 0 && cache_read == 0 && cache_write == 0 {
        return Vec::new();
    }
    let mut event =
        RawUsageEvent::observed("cumulative".into(), 0, updated_at, USAGE_PARSER_VERSION);
    event.input_tokens = input;
    event.output_tokens = output;
    event.cache_read_tokens = cache_read;
    event.cache_write_tokens = cache_write;
    event.model =
        thread.pointer("/model/model").and_then(Value::as_str).unwrap_or("unknown").into();
    event.provider =
        thread.pointer("/model/provider").and_then(Value::as_str).unwrap_or("unknown").into();
    event.source_path = Some(path.to_string_lossy().into_owned());
    event.raw_usage_json = Some(usage.to_string());
    vec![event]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::store::{IndexedSessionMeta, ParserStateMeta};
    use serde_json::json;

    fn metadata() -> ThreadMetadata {
        ThreadMetadata {
            id: "native-thread".into(),
            title: "Task".into(),
            updated_at: "2026-10-07T11:00:00Z".into(),
            data_type: "zstd".into(),
            created_at: Some("2026-10-07T10:00:00Z".into()),
            folder_paths: Some("/project/a\n/project/b\n/project/c".into()),
            folder_paths_order: Some("2,0,1".into()),
            parent_id: None,
        }
    }

    fn current_thread() -> Value {
        json!({
            "version": "0.3.0", "title": "Read, count and edit",
            "updated_at": "2026-10-07T11:00:00Z",
            "initial_project_snapshot": {
                "timestamp": "2026-10-07T09:59:59.123Z",
                "worktree_snapshots": [{"worktree_path": "/project/b", "git_state": null}]
            },
            "messages": [
                {"User": {"id": "user-1", "content": [{"Text": "Read, count and edit input.txt"}, {"Image": {"source": "ignored"}}]}},
                {"Agent": {"content": [
                    {"Thinking": {"text": "private reasoning", "signature": null}},
                    {"ToolUse": {"id": "read-1", "name": "read_file", "input": {"type": "json", "value": {"path": "/project/b/input.txt"}}}},
                    {"ToolUse": {"id": "shell-1", "name": "terminal", "input": {"command": "wc -l input.txt", "cd": "/project/b"}}},
                    {"ToolUse": {"id": "edit-1", "name": "edit_file", "input": {"path": "/project/b/input.txt", "edits": [{"old_text": "beta", "new_text": "beta\ngamma"}]}}}
                ], "tool_results": {
                    "read-1": {"tool_use_id": "read-1", "tool_name": "read_file", "is_error": false, "content": [{"Text": "alpha\nbeta"}, {"Image": {"source": "ignored"}}]},
                    "shell-1": {"tool_use_id": "shell-1", "tool_name": "terminal", "is_error": false, "content": {"Text": "2 input.txt"}},
                    "edit-1": {"tool_use_id": "edit-1", "tool_name": "edit_file", "is_error": false, "content": {"Text": "Edited input.txt"}}
                }}},
                "Resume", {"Compaction": {"Summary": "internal summary"}},
                {"Agent": {"content": [{"Text": "Counted two lines and added gamma."}], "tool_results": {}}}
            ],
            "cumulative_token_usage": {"input_tokens": 1200, "output_tokens": 150, "cache_read_input_tokens": 600, "cache_creation_input_tokens": 200},
            "request_token_usage": {"user-1": {"input_tokens": 500, "output_tokens": 30}},
            "model": {"provider": "custom", "model": "test-model"}
        })
    }

    #[test]
    fn real_zed_thread_preserves_reused_tool_ids_and_reported_totals() {
        let thread: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/zed/thread-0.3.0.json"))
                .unwrap();
        let mut meta = metadata();
        meta.folder_paths = None;
        meta.updated_at = thread["updated_at"].as_str().unwrap().into();
        let raw = parse_thread(&thread, &meta, Path::new("threads.db"), true).unwrap().unwrap();
        assert_eq!(raw.messages.len(), 3);
        assert_eq!(raw.directory.as_deref(), Some("/project/zed-demo"));
        assert_eq!(raw.events.len(), 6);
        let read = raw
            .events
            .iter()
            .find(|event| event.name.as_deref() == Some("read_file") && event.actor == "assistant")
            .unwrap();
        let edit = raw
            .events
            .iter()
            .find(|event| event.name.as_deref() == Some("edit_file") && event.actor == "assistant")
            .unwrap();
        assert_ne!(read.tool_call_id, edit.tool_call_id);
        for call in raw.events.iter().filter(|event| event.actor == "assistant") {
            let results: Vec<_> = raw
                .events
                .iter()
                .filter(|event| {
                    event.kind == "tool_result" && event.tool_call_id == call.tool_call_id
                })
                .collect();
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].name, call.name);
        }
        let usage = &raw.usage_events[0];
        assert_eq!(
            (
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_write_tokens,
                usage.cache_read_tokens
            ),
            (6, 161, 18121, 36002)
        );
        assert_eq!(raw.started_at, timestamp("2026-10-07T11:11:54.699111Z").unwrap());
    }

    #[test]
    fn current_messages_events_and_cumulative_usage_stay_separate() {
        let raw = parse_thread(&current_thread(), &metadata(), Path::new("threads.db"), true)
            .unwrap()
            .unwrap();
        assert_eq!(raw.messages.len(), 2);
        assert_eq!(raw.messages[0].role, Role::User);
        assert_eq!(raw.messages[1].role, Role::Assistant);
        assert_eq!(raw.messages[1].content, "Counted two lines and added gamma.");
        assert!(raw.messages.iter().all(|message| message.timestamp.is_none()));
        assert_eq!(raw.directory.as_deref(), Some("/project/b"));
        assert_eq!(raw.started_at, timestamp("2026-10-07T09:59:59.123Z").unwrap());
        assert_eq!(raw.updated_at, timestamp("2026-10-07T11:00:00Z"));
        assert_eq!(raw.custom_title.as_deref(), Some("Read, count and edit"));
        assert_eq!(raw.events.len(), 6);
        assert_eq!(raw.events[0].kind, "file_read");
        assert_eq!(raw.events[0].files[0].operation, FileOperation::Read);
        assert_eq!(raw.events[1].kind, "command");
        assert_eq!(raw.events[1].target.as_deref(), Some("wc -l input.txt"));
        assert_eq!(raw.events[2].files[0].operation, FileOperation::Write);
        assert!(
            raw.events[3..]
                .iter()
                .all(|event| event.kind == "tool_result"
                    && event.status.as_deref() == Some("success"))
        );
        assert!(
            raw.events
                .iter()
                .all(|event| event.timestamp.is_none() && event.tool_call_id.is_some())
        );
        let usage = &raw.usage_events[0];
        assert_eq!(raw.usage_events.len(), 1);
        assert_eq!(
            (
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_read_tokens,
                usage.cache_write_tokens
            ),
            (1200, 150, 600, 200)
        );
        assert_eq!(usage.timestamp, raw.updated_at.unwrap());
        assert_eq!(usage.model, "test-model");
        assert_eq!(usage.provider, "custom");
        assert_eq!(usage.reasoning_tokens, 0);
    }

    #[test]
    fn invalid_workspace_order_falls_back_to_first_sorted_path() {
        for order in [None, Some(""), Some("1,0"), Some("2,0,0"), Some("3,0,1"), Some("x,0,1")] {
            let mut meta = metadata();
            meta.folder_paths_order = order.map(str::to_string);
            let raw = parse_thread(&current_thread(), &meta, Path::new("threads.db"), false)
                .unwrap()
                .unwrap();
            assert_eq!(raw.directory.as_deref(), Some("/project/a"), "order: {order:?}");
        }
    }

    #[test]
    fn user_mentions_keep_reference_labels_without_attached_content() {
        let mut thread = current_thread();
        thread["messages"][0]["User"]["content"] = json!([
            {"Text": "Check this file"},
            {"Mention": {"uri": {"File": {"abs_path": "/project/b/input.txt"}}, "content": "alpha\nbeta"}},
            {"Mention": {"uri": {"File": {"abs_path": "/project/b/empty.txt"}}, "content": ""}},
            {"Image": {"source": "ignored"}}
        ]);
        let raw =
            parse_thread(&thread, &metadata(), Path::new("threads.db"), false).unwrap().unwrap();
        assert_eq!(
            raw.messages[0].content,
            "Check this file\n@/project/b/input.txt\n@/project/b/empty.txt"
        );
        assert!(!raw.messages[0].content.contains("alpha"));
        assert!(!raw.messages[0].content.contains("beta"));
        assert_eq!(raw.messages.len(), 2);
    }

    #[test]
    fn mention_references_use_known_uri_fields_and_ignore_unknown_payloads() {
        let cases = [
            (
                json!({"File": {"abs_path": "/project/file with spaces.txt "}}),
                Some("@/project/file with spaces.txt "),
            ),
            (json!({"Directory": {"abs_path": "/project/src"}}), Some("@/project/src")),
            (
                json!({"Symbol": {"abs_path": "/project/src/lib.rs", "name": "parse", "line_range": {"start": 3, "end": 5}}}),
                Some("@/project/src/lib.rs#parse"),
            ),
            (json!({"Thread": {"id": "native-id", "name": "Earlier task"}}), Some("@Earlier task")),
            (
                json!({"Rule": {"id": {"User": {"uuid": "rule-id"}}, "name": "Project rules"}}),
                Some("@Project rules"),
            ),
            (
                json!({"Skill": {"name": "Review", "source": "project", "skill_file_path": "/project/.agents/review.md"}}),
                Some("@Review"),
            ),
            (json!({"PastedImage": {"name": "diagram.png"}}), Some("@diagram.png")),
            (
                json!({"Selection": {"abs_path": "/project/input.txt", "line_range": {"start": 0, "end": 1}}}),
                Some("@/project/input.txt"),
            ),
            (
                json!({"Fetch": {"url": "https://example.com/spec"}}),
                Some("@https://example.com/spec"),
            ),
            (json!({"GitDiff": {"base_ref": "main"}}), Some("@main")),
            (json!({"MergeConflict": {"file_path": "src/lib.rs"}}), Some("@src/lib.rs")),
            (json!({"Selection": {"abs_path": null, "line_range": {"start": 0, "end": 1}}}), None),
            (json!({"Diagnostics": {"include_errors": true, "include_warnings": false}}), None),
            (json!({"TerminalSelection": {"line_count": 3}}), None),
            (json!({"Unknown": {"name": "unsupported", "abs_path": "/not-a-reference"}}), None),
            (json!({"File": {"abs_path": ""}}), None),
            (json!({"File": {"abs_path": 42}}), None),
            (json!("file:///unknown-shape"), None),
            (Value::Null, None),
        ];
        for (uri, label) in cases {
            let mut thread = current_thread();
            thread["messages"][0]["User"]["content"] = json!([
                {"Text": "Inspect reference"},
                {"Mention": {"uri": uri, "content": "attachment_payload".repeat(10000)}}
            ]);
            let raw = parse_thread(&thread, &metadata(), Path::new("threads.db"), false)
                .unwrap()
                .unwrap();
            let expected = label
                .map(|label| format!("Inspect reference\n{label}"))
                .unwrap_or_else(|| "Inspect reference".into());
            assert_eq!(raw.messages[0].content.len(), expected.len());
            assert_eq!(raw.messages[0].content, expected);
            assert!(!raw.messages[0].content.contains("attachment_payload"));
        }
    }

    #[test]
    fn previous_metadata_parser_reloads_unchanged_mention_messages() {
        let root = tempfile::tempdir().unwrap();
        let (path, conn) = database(root.path(), true);
        let mut thread = current_thread();
        thread["messages"][0]["User"]["content"] = json!([
            {"Text": "Read file"},
            {"Mention": {"uri": {"File": {"abs_path": "/project/input.txt"}}, "content": "attachment_payload"}}
        ]);
        insert(&conn, "native-thread", "json", &serde_json::to_vec(&thread).unwrap());
        let updated_at = timestamp(&metadata().updated_at);
        let context = AdapterSyncContext::new(
            "zed".into(),
            HashMap::from([(
                "native-thread".into(),
                IndexedSessionMeta { id: "indexed".into(), updated_at, message_count: 2 },
            )]),
            HashMap::new(),
            HashSet::new(),
            HashMap::from([(
                "native-thread".into(),
                ParserStateMeta {
                    parser_version: USAGE_PARSER_VERSION,
                    source_updated_at: updated_at,
                },
            )]),
            HashMap::from([(
                "native-thread".into(),
                ParserStateMeta {
                    parser_version: EVENT_PARSER_VERSION,
                    source_updated_at: updated_at,
                },
            )]),
            HashMap::from([(
                "native-thread".into(),
                ParserStateMeta { parser_version: 1, source_updated_at: updated_at },
            )]),
        );
        let scan = scan_database(&path, Some(&context), None, true).unwrap();
        assert_eq!(scan.sessions.len(), 1);
        assert_eq!(scan.sessions[0].messages[0].content, "Read file\n@/project/input.txt");
        assert!(scan.sessions[0].refresh_session_on_metadata_backfill);
    }

    #[test]
    fn native_write_copy_and_move_calls_preserve_file_operations() {
        let mut thread = current_thread();
        thread["messages"][1]["Agent"]["content"] = json!([
            {"ToolUse": {"id": "write", "name": "write_file", "input": {"path": "new.txt", "content": "created"}}},
            {"ToolUse": {"id": "copy", "name": "copy_path", "input": {"source_path": "new.txt", "destination_path": "copy.txt"}}},
            {"ToolUse": {"id": "move", "name": "move_path", "input": {"source_path": "copy.txt", "destination_path": "moved.txt"}}}
        ]);
        thread["messages"][1]["Agent"]["tool_results"] = json!({});
        let raw =
            parse_thread(&thread, &metadata(), Path::new("threads.db"), true).unwrap().unwrap();
        assert_eq!(raw.events.len(), 3);
        let expected = [
            vec![("new.txt", FileOperation::Write)],
            vec![("new.txt", FileOperation::Read), ("copy.txt", FileOperation::Write)],
            vec![("copy.txt", FileOperation::MoveFrom), ("moved.txt", FileOperation::MoveTo)],
        ];
        for (event, expected) in raw.events.iter().zip(expected) {
            assert_eq!(event.kind, "file_write");
            assert_eq!(
                event
                    .files
                    .iter()
                    .map(|file| (file.path.as_str(), file.operation.clone()))
                    .collect::<Vec<_>>(),
                expected
            );
            assert!(event.files.iter().all(|file| file.cwd.as_deref() == Some("/project/b")));
        }
    }

    #[test]
    fn database_parent_id_marks_subagent_and_links_native_parent() {
        let root = tempfile::tempdir().unwrap();
        let (path, conn) = database(root.path(), true);
        conn.execute_batch("ALTER TABLE threads ADD COLUMN parent_id TEXT;").unwrap();
        let data = serde_json::to_vec(&current_thread()).unwrap();
        for id in ["child", "primary", "blank"] {
            insert(&conn, id, "json", &data);
        }
        conn.execute("UPDATE threads SET parent_id = 'parent-native-id' WHERE id = 'child'", [])
            .unwrap();
        conn.execute("UPDATE threads SET parent_id = '  ' WHERE id = 'blank'", []).unwrap();
        let scan = scan_database(&path, None, None, true).unwrap();
        let child = scan.sessions.iter().find(|raw| raw.source_id == "child").unwrap();
        assert_eq!(child.thread_role, Some(crate::types::ThreadRole::Subagent));
        assert_eq!(
            child.parent_links,
            vec![crate::types::ParentLink {
                relation: crate::types::ParentRelation::Spawn,
                source: "zed".into(),
                source_id: "parent-native-id".into(),
            }]
        );
        assert!(
            scan.sessions
                .iter()
                .filter(|raw| raw.source_id != "child")
                .all(|raw| raw.thread_role.is_none() && raw.parent_links.is_empty())
        );
    }

    #[test]
    fn legacy_versions_keep_text_and_tools_without_injected_messages() {
        for version in [Some("0.2.0"), Some("0.1.0"), None] {
            let mut thread = json!({
                "summary": "Legacy", "updated_at": "2026-10-07T11:00:00Z",
                "initial_project_snapshot": {"worktree_snapshots": [{"worktree_path": "/legacy"}], "timestamp": "2026-10-07T09:00:00Z"},
                "messages": [
                    {"id": 0, "role": "system", "text": "ignored"},
                    {"id": 1, "role": "user", "text": "Read input.txt", "segments": [{"type": "text", "text": "Read input.txt"}]},
                    {"id": 2, "role": "assistant", "text": "Done", "segments": [{"type": "thinking", "text": "ignored"}, {"type": "text", "text": "Done"}], "tool_uses": [{"id": "call", "name": "read_file", "input": {"path": "/legacy/input.txt"}}]},
                    {"id": 3, "role": "user", "tool_results": [{"tool_use_id": "call", "is_error": true, "content": "failed"}]},
                    {"id": 4, "role": "user", "is_hidden": true, "text": "internal context", "segments": [{"type": "text", "text": "internal context"}]}
                ]
            });
            if let Some(version) = version {
                thread["version"] = json!(version);
            }
            if version.is_none() {
                for message in thread["messages"].as_array_mut().unwrap() {
                    message.as_object_mut().unwrap().remove("segments");
                }
            }
            let mut meta = metadata();
            meta.folder_paths = None;
            let raw = parse_thread(&thread, &meta, Path::new("threads.db"), true).unwrap().unwrap();
            assert_eq!(raw.messages.len(), 2);
            assert_eq!(raw.messages[1].content, "Done");
            assert_eq!(raw.directory.as_deref(), Some("/legacy"));
            assert_eq!(raw.events.len(), 2);
            assert_eq!(raw.events[1].name.as_deref(), Some("read_file"));
            assert_eq!(raw.events[1].status.as_deref(), Some("error"));
            assert!(raw.usage_events.is_empty());
        }
    }

    #[test]
    fn token_snapshots_estimates_and_zero_defaults_do_not_become_usage() {
        let mut thread = current_thread();
        thread.as_object_mut().unwrap().remove("cumulative_token_usage");
        assert!(parse_usage(&thread, 1, Path::new("threads.db")).is_empty());
        thread["cumulative_token_usage"] = json!({"input_tokens": 0, "output_tokens": 0});
        thread["token_count"] = json!(12000);
        assert!(parse_usage(&thread, 1, Path::new("threads.db")).is_empty());
        thread["cumulative_token_usage"] = json!({"output_tokens": 1});
        assert_eq!(parse_usage(&thread, 1, Path::new("threads.db"))[0].output_tokens, 1);
    }

    fn database(root: &Path, modern: bool) -> (PathBuf, Connection) {
        let path = root.join("threads.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY, summary TEXT NOT NULL, updated_at TEXT NOT NULL, data_type TEXT NOT NULL, data BLOB NOT NULL)").unwrap();
        if modern {
            conn.execute_batch("ALTER TABLE threads ADD COLUMN created_at TEXT; ALTER TABLE threads ADD COLUMN folder_paths TEXT; ALTER TABLE threads ADD COLUMN folder_paths_order TEXT;").unwrap();
        }
        (path, conn)
    }

    fn insert(conn: &Connection, id: &str, encoding: &str, data: &[u8]) {
        conn.execute("INSERT INTO threads (id, summary, updated_at, data_type, data) VALUES (?1, 'Task', '2026-10-07T11:00:00Z', ?2, ?3)", rusqlite::params![id, encoding, data]).unwrap();
    }

    #[test]
    fn database_reads_json_zstd_and_wal_without_writing_and_skips_bad_rows() {
        for modern in [true, false] {
            let root = tempfile::tempdir().unwrap();
            let (path, conn) = database(root.path(), modern);
            conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
            let json = serde_json::to_vec(&current_thread()).unwrap();
            insert(&conn, "zstd", "zstd", &zstd::encode_all(json.as_slice(), 3).unwrap());
            insert(&conn, "json", "json", &json);
            insert(&conn, "corrupt", "zstd", b"corrupt");
            insert(&conn, "future", "other", &json);
            insert(&conn, "external", "json", br#"{"agent":"claude-code","messages":[{"role":"user","content":"external prompt"}]}"#);
            let before = std::fs::read(&path).unwrap();
            let result = scan_database(&path, None, None, true).unwrap();
            assert_eq!(result.sessions.len(), 2);
            assert_eq!(result.stats.candidates, 5);
            assert_eq!(result.stats.parsed, 2);
            assert_eq!(std::fs::read(&path).unwrap(), before);
            let restricted = AdapterSyncContext::empty_for_test("zed").restricted_to("json");
            assert_eq!(
                scan_database(&path, Some(&restricted), None, false).unwrap().sessions.len(),
                1
            );
            assert!(scan_database(&path, None, Some(i64::MAX), true).unwrap().sessions.is_empty());
        }
        let root = tempfile::tempdir().unwrap();
        assert!(
            scan_database(&root.path().join("absent.db"), None, None, true)
                .unwrap()
                .sessions
                .is_empty()
        );
        assert!(!root.path().join("absent.db").exists());
    }

    #[test]
    fn incremental_sync_backfills_events_and_refreshes_changed_usage() {
        let root = tempfile::tempdir().unwrap();
        let (path, conn) = database(root.path(), true);
        insert(&conn, "native-thread", "json", &serde_json::to_vec(&current_thread()).unwrap());
        let updated_at = timestamp(&metadata().updated_at);
        let mut context = AdapterSyncContext::new(
            "zed".into(),
            HashMap::from([(
                "native-thread".into(),
                IndexedSessionMeta { id: "indexed".into(), updated_at, message_count: 2 },
            )]),
            HashMap::new(),
            HashSet::new(),
            HashMap::from([(
                "native-thread".into(),
                ParserStateMeta { parser_version: 1, source_updated_at: updated_at },
            )]),
            HashMap::new(),
            HashMap::from([(
                "native-thread".into(),
                ParserStateMeta {
                    parser_version: METADATA_PARSER_VERSION,
                    source_updated_at: updated_at,
                },
            )]),
        );
        let scan = scan_database(&path, Some(&context), None, false).unwrap();
        assert_eq!(scan.stats.skipped_sessions, 1);
        assert_eq!(scan.observations.len(), 1);
        assert!(scan.sessions.is_empty());
        let scan = scan_database(&path, Some(&context), None, true).unwrap();
        assert_eq!(scan.sessions[0].events.len(), 6);
        context = AdapterSyncContext::new(
            "zed".into(),
            context.session_meta().clone(),
            HashMap::new(),
            HashSet::new(),
            context.usage_state().clone(),
            HashMap::from([(
                "native-thread".into(),
                ParserStateMeta { parser_version: 0, source_updated_at: updated_at },
            )]),
            context.metadata_state().clone(),
        );
        assert_eq!(scan_database(&path, Some(&context), None, true).unwrap().sessions.len(), 1);
        let old_context = AdapterSyncContext::new(
            "zed".into(),
            context.session_meta().clone(),
            HashMap::new(),
            HashSet::new(),
            context.usage_state().clone(),
            HashMap::from([(
                "native-thread".into(),
                ParserStateMeta { parser_version: 1, source_updated_at: updated_at },
            )]),
            HashMap::new(),
        );
        let repaired = scan_database(&path, Some(&old_context), None, true).unwrap();
        assert_eq!(repaired.sessions.len(), 1);
        assert_eq!(repaired.sessions[0].metadata_parser_version, Some(METADATA_PARSER_VERSION));
        assert!(repaired.sessions[0].refresh_session_on_metadata_backfill);
        assert_eq!(repaired.sessions[0].event_parser_version, Some(EVENT_PARSER_VERSION));
        assert_eq!(
            scan_database(&path, Some(&old_context), None, false).unwrap().sessions.len(),
            1
        );
        let current_context = AdapterSyncContext::new(
            "zed".into(),
            context.session_meta().clone(),
            HashMap::new(),
            HashSet::new(),
            context.usage_state().clone(),
            HashMap::from([(
                "native-thread".into(),
                ParserStateMeta {
                    parser_version: EVENT_PARSER_VERSION,
                    source_updated_at: updated_at,
                },
            )]),
            context.metadata_state().clone(),
        );
        assert_eq!(
            scan_database(&path, Some(&current_context), None, true)
                .unwrap()
                .stats
                .skipped_sessions,
            1
        );
        let mut thread = current_thread();
        thread["cumulative_token_usage"]["output_tokens"] = json!(250);
        conn.execute(
            "UPDATE threads SET updated_at='2026-10-07T12:00:00Z', data=?1",
            [thread.to_string()],
        )
        .unwrap();
        let scan = scan_database(&path, Some(&context), None, false).unwrap();
        assert_eq!(scan.sessions[0].usage_events[0].output_tokens, 250);
        assert!(scan.sessions[0].events.is_empty());
        assert_eq!(scan.sessions[0].event_parser_version, None);
    }

    #[test]
    fn start_url_preserves_unicode_and_query_delimiters() {
        let prompt = "Read a&b? + 中文\nThen edit";
        let command = ZedAdapter.start_command(prompt.into()).unwrap();
        assert_eq!(command.program, "zed");
        let encoded = command.args[0].strip_prefix("zed://agent?prompt=").unwrap();
        let decoded = crate::adapters::paths::file_uri_to_path(&format!("file://{encoded}"));
        assert_eq!(decoded.as_deref(), Some(prompt));
        assert!(!encoded.contains(['&', '?', '+', '\n']));
    }
}
