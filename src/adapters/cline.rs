mod cli_store;

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use tracing::debug;

use crate::adapters::AdapterSyncContext;
use crate::adapters::events::{self, EventContext};
use crate::adapters::file_scan::{self, FileScanEntry};
use crate::adapters::json_util;
use crate::adapters::paths;
use crate::adapters::{RawMessage, RawSession, ResumeCommand, SourceAdapter, SyncScanResult};
use crate::types::{FileEvidence, FileEvidenceKind, FileOperation, RawSessionEvent, Role};

pub(super) const EVENT_PARSER_VERSION: u32 = 1;
pub(super) const METADATA_PARSER_VERSION: u32 = 1;

const CLINE_EXTENSION_ID: &str = "saoudrizwan.claude-dev";

pub(crate) struct ClineAdapter;

impl SourceAdapter for ClineAdapter {
    fn id(&self) -> &str {
        "cline"
    }
    fn label(&self) -> &str {
        "CL"
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        cli_store::resume_command(source_id)
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        append_cli_store_sessions(scan_task_dirs(&resolve_tasks_dirs())?)
    }

    fn scan_for_sync(
        &self,
        context: &AdapterSyncContext,
        since_ts: Option<i64>,
        include_events: bool,
    ) -> anyhow::Result<Option<SyncScanResult>> {
        let mut result =
            scan_task_dirs_for_sync(&resolve_tasks_dirs(), context, since_ts, include_events)?;
        let covered = ids_covered_by_plugin(context, &result.sessions);
        merge_scan_results(
            &mut result,
            cli_store::scan_for_sync(context, since_ts, &covered, include_events)?,
        );
        Ok(Some(result))
    }
}

pub(crate) fn scan_task_dirs(tasks_dirs: &[PathBuf]) -> anyhow::Result<Vec<RawSession>> {
    let mut sessions = Vec::new();
    for entry in collect_all_entries(tasks_dirs) {
        let Some(snapshot) = task_snapshot(&entry) else {
            continue;
        };
        let observed = entry.clone();
        let raw = parse_cline_task(entry, snapshot.effective_mtime_ms())?;
        if task_snapshot(&observed).as_ref() != Some(&snapshot) {
            continue;
        }
        if let Some(raw) = raw {
            sessions.push(raw);
        }
    }
    Ok(sessions)
}

pub(crate) fn scan_task_dirs_for_sync(
    tasks_dirs: &[PathBuf],
    context: &AdapterSyncContext,
    since_ts: Option<i64>,
    include_events: bool,
) -> anyhow::Result<SyncScanResult> {
    let entries = collect_all_entries(tasks_dirs);
    file_scan::run_file_scan_with_options_and_snapshot(
        context,
        since_ts,
        file_scan::FileScanOptions {
            event_parser_version: include_events.then_some(EVENT_PARSER_VERSION),
            metadata_parser_version: include_events.then_some(METADATA_PARSER_VERSION),
            ..Default::default()
        },
        entries,
        task_snapshot,
        parse_cline_task,
    )
}

fn resolve_tasks_dirs() -> Vec<PathBuf> {
    let dirs = paths::vscode_extension_task_dirs(CLINE_EXTENSION_ID);
    if dirs.is_empty() {
        debug!("Cline tasks directory not found, skipping Cline");
    }
    dirs
}

fn collect_all_entries(tasks_dirs: &[PathBuf]) -> Vec<FileScanEntry> {
    tasks_dirs.iter().flat_map(|dir| collect_cline_entries(dir)).collect()
}

fn collect_cline_entries(tasks_dir: &Path) -> Vec<FileScanEntry> {
    let mut entries = Vec::new();

    let dir_entries = match fs::read_dir(tasks_dir) {
        Ok(e) => e,
        Err(e) => {
            debug!("cannot read Cline tasks dir: {e}");
            return vec![];
        }
    };

    for entry in dir_entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let dir_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let ui_path = path.join("ui_messages.json");
        let api_path = path.join("api_conversation_history.json");
        let messages_path = if ui_path.is_file() {
            ui_path
        } else if api_path.is_file() {
            api_path
        } else {
            continue;
        };
        entries.push(FileScanEntry {
            session_id: dir_name,
            stat_target: messages_path,
            directory: None,
        });
    }

    entries
}

fn task_snapshot(
    entry: &FileScanEntry,
) -> Option<file_scan::FileScanSnapshot<Vec<Option<file_scan::FileMetadataSnapshot>>>> {
    let dir = entry.stat_target.parent()?;
    let files = [
        "ui_messages.json",
        "api_conversation_history.json",
        "history_item.json",
        "history.json",
        "task_metadata.json",
    ]
    .into_iter()
    .map(|name| file_scan::file_metadata_snapshot(&dir.join(name)))
    .collect::<Vec<_>>();
    let mtime =
        files.iter().flatten().filter_map(file_scan::FileMetadataSnapshot::mtime_ms).max()?;
    Some(file_scan::FileScanSnapshot::new(mtime, files))
}

fn parse_cline_task(entry: FileScanEntry, mtime_ms: i64) -> anyhow::Result<Option<RawSession>> {
    let Some(task_dir) = entry.stat_target.parent() else {
        return Ok(None);
    };
    let ui_path = task_dir.join("ui_messages.json");
    let api_path = task_dir.join("api_conversation_history.json");
    let ui_records =
        if ui_path.is_file() { load_ui_records(&ui_path) } else { Ok((Vec::new(), Vec::new())) };
    let (messages, mut events) = match ui_records {
        Ok(m) => m,
        Err(e) => {
            debug!("failed to parse Cline ui_messages {}: {e}", entry.stat_target.display());
            return Ok(None);
        }
    };

    if api_path.is_file() {
        let parsed = fs::read_to_string(&api_path).map_err(anyhow::Error::from).and_then(|text| {
            serde_json::from_str::<Vec<Value>>(&text).map_err(anyhow::Error::from)
        });
        let records = match parsed {
            Ok(records) => records,
            Err(error) => {
                debug!("failed to parse Cline API history {}: {error}", api_path.display());
                return Ok(None);
            }
        };
        let cwd = read_history_workspace(&task_dir.join("history_item.json"))
            .or_else(|| read_history_workspace(&task_dir.join("history.json")));
        for (index, record) in records.iter().enumerate() {
            let start = events.len();
            cli_store::append_tool_events(record, index, &api_path, None, &mut events);
            if events.len() == start
                && matches!(record.get("role").and_then(Value::as_str), Some("assistant" | "user"))
            {
                append_legacy_api_events(record, index, &api_path, &mut events);
            }
            for event in &mut events[start..] {
                for file in &mut event.files {
                    if file.kind != FileEvidenceKind::Command {
                        file.cwd = cwd.clone();
                    }
                }
            }
        }
    }
    if messages.is_empty() && events.is_empty() {
        return Ok(None);
    }

    let started_at = task_started_at(&entry, &messages);
    let directory = extract_directory(&entry.stat_target);
    let source_file_path = entry.stat_target.to_str().map(str::to_string);

    let mut raw = RawSession::search_only(
        entry.session_id,
        directory,
        started_at,
        Some(mtime_ms),
        None,
        messages,
    );
    raw = raw.with_events(events, EVENT_PARSER_VERSION);
    raw.metadata_parser_version = Some(METADATA_PARSER_VERSION);
    raw.refresh_session_on_metadata_backfill = true;
    raw.source_file_path = source_file_path;
    Ok(Some(raw))
}

fn append_legacy_api_events(
    record: &Value,
    index: usize,
    path: &Path,
    events: &mut Vec<RawSessionEvent>,
) {
    const TOOLS: &[&str] = &[
        "ask_followup_question",
        "attempt_completion",
        "execute_command",
        "replace_in_file",
        "read_file",
        "write_to_file",
        "search_files",
        "list_files",
        "list_code_definition_names",
        "browser_action",
        "use_mcp_tool",
        "access_mcp_resource",
        "load_mcp_documentation",
        "new_task",
        "plan_mode_respond",
        "act_mode_respond",
        "focus_chain",
        "web_fetch",
        "web_search",
        "condense",
        "summarize_task",
        "report_bug",
        "new_rule",
        "apply_patch",
        "generate_explanation",
        "use_skill",
        "use_subagents",
        "apply_diff",
        "insert_content",
        "switch_mode",
        "fetch_instructions",
        "codebase_search",
        "update_todo_list",
        "run_slash_command",
        "generate_image",
    ];
    const PARAMS: &[&str] = &[
        "command",
        "requires_approval",
        "path",
        "absolutePath",
        "content",
        "diff",
        "regex",
        "file_pattern",
        "recursive",
        "action",
        "url",
        "coordinate",
        "text",
        "query",
        "allowed_domains",
        "blocked_domains",
        "prompt",
        "server_name",
        "tool_name",
        "arguments",
        "uri",
        "question",
        "options",
        "response",
        "result",
        "context",
        "title",
        "what_happened",
        "steps_to_reproduce",
        "api_request_output",
        "additional_context",
        "needs_more_exploration",
        "task_progress",
        "timeout",
        "input",
        "from_ref",
        "to_ref",
        "skill_name",
        "prompt_1",
        "prompt_2",
        "prompt_3",
        "prompt_4",
        "prompt_5",
        "line_count",
        "mode_slug",
        "reason",
        "line",
        "mode",
        "message",
        "cwd",
        "follow_up",
        "task",
        "size",
        "args",
        "start_line",
        "end_line",
        "todos",
        "image",
        "files",
    ];
    let content = record.get("content").unwrap_or(&Value::Null);
    let parts =
        content.as_array().map(Vec::as_slice).unwrap_or_else(|| std::slice::from_ref(content));
    for (part_index, part) in parts.iter().enumerate() {
        let text = part.as_str().or_else(|| {
            (part.get("type").and_then(Value::as_str) == Some("text"))
                .then(|| part.get("text").and_then(Value::as_str))
                .flatten()
        });
        let Some(text) = text else {
            continue;
        };
        if record.get("role").and_then(Value::as_str) == Some("user") {
            if let Some((header, _)) =
                text.strip_prefix('[').and_then(|text| text.split_once("] Result:"))
                && let Some(name) =
                    header.split_whitespace().next().filter(|name| TOOLS.contains(name))
            {
                let mut event = events::tool_result_event(
                    EventContext {
                        event_seq: events.len() as u32,
                        timestamp: json_util::json_i64(record.get("ts")),
                        source_path: path.to_str().map(str::to_string),
                        source_event_id: Some(format!(
                            "{}:{index}:text:{part_index}",
                            record.get("id").and_then(Value::as_str).unwrap_or("message")
                        )),
                        message_seq: None,
                        parser_version: EVENT_PARSER_VERSION,
                    },
                    Some(name.to_string()),
                    Some(text.to_string()),
                );
                event.attrs_json = Some(record.to_string());
                events.push(event);
            }
            continue;
        }
        let mut cursor = 0;
        while cursor < text.len() {
            let Some((start, name)) = TOOLS
                .iter()
                .filter_map(|name| {
                    text[cursor..].find(&format!("<{name}>")).map(|offset| (cursor + offset, *name))
                })
                .min_by_key(|(start, _)| *start)
            else {
                break;
            };
            cursor = start + name.len() + 2;
            let close = format!("</{name}>");
            let mut input = serde_json::Map::new();
            while cursor < text.len() {
                if text[cursor..].starts_with(&close) {
                    cursor += close.len();
                    break;
                }
                if let Some(param) =
                    PARAMS.iter().find(|param| text[cursor..].starts_with(&format!("<{param}>")))
                {
                    cursor += param.len() + 2;
                    let close_param = format!("</{param}>");
                    let Some(end) = text[cursor..].find(&close_param) else {
                        cursor = text.len();
                        break;
                    };
                    input.insert(
                        (*param).to_string(),
                        Value::String(text[cursor..cursor + end].trim().to_string()),
                    );
                    cursor += end + close_param.len();
                } else {
                    cursor += text[cursor..].chars().next().unwrap().len_utf8();
                }
            }
            if matches!(name, "read_file" | "apply_diff")
                && let Some(args) = input.get("args").and_then(Value::as_str)
            {
                let mut files = Vec::new();
                let mut remaining = args;
                while let Some((_, after_open)) = remaining.split_once("<file>") {
                    let Some((file, after_close)) = after_open.split_once("</file>") else {
                        break;
                    };
                    if let Some((_, after_path)) = file.split_once("<path>")
                        && let Some((path, _)) = after_path.split_once("</path>")
                    {
                        let mut decoded = String::new();
                        let mut rest = path.trim();
                        while let Some((prefix, entity)) = rest.split_once('&') {
                            decoded.push_str(prefix);
                            let Some((entity, after)) = entity.split_once(';') else {
                                decoded.push('&');
                                decoded.push_str(entity);
                                rest = "";
                                break;
                            };
                            let character = match entity {
                                "amp" => Some('&'),
                                "lt" => Some('<'),
                                "gt" => Some('>'),
                                "quot" => Some('"'),
                                "apos" => Some('\''),
                                entity if entity.starts_with("#x") => {
                                    u32::from_str_radix(&entity[2..], 16)
                                        .ok()
                                        .and_then(char::from_u32)
                                }
                                entity if entity.starts_with('#') => {
                                    entity[1..].parse::<u32>().ok().and_then(char::from_u32)
                                }
                                _ => None,
                            };
                            if let Some(character) = character {
                                decoded.push(character);
                            } else {
                                decoded.push('&');
                                decoded.push_str(entity);
                                decoded.push(';');
                            }
                            rest = after;
                        }
                        decoded.push_str(rest);
                        files.push(serde_json::json!({ "path":decoded }));
                    }
                    remaining = after_close;
                }
                input.insert("files".to_string(), Value::Array(files));
            }
            let message = serde_json::json!({
                "id":record.get("id"), "ts":record.get("ts"), "role":"assistant",
                "content":[{"type":"tool_use","name":name,"input":input}]
            });
            let before = events.len();
            cli_store::append_tool_events(&message, index, path, None, events);
            for event in &mut events[before..] {
                event.source_event_id = Some(format!(
                    "{}:{index}:text:{part_index}:byte:{start}",
                    record.get("id").and_then(Value::as_str).unwrap_or("message")
                ));
                event.attrs_json = Some(record.to_string());
            }
        }
    }
}

#[cfg(test)]
fn load_ui_messages(path: &Path) -> anyhow::Result<Vec<RawMessage>> {
    load_ui_records(path).map(|(messages, _)| messages)
}

fn load_ui_records(path: &Path) -> anyhow::Result<(Vec<RawMessage>, Vec<RawSessionEvent>)> {
    let content = fs::read_to_string(path)?;
    let messages: Vec<Value> = serde_json::from_str(&content)?;

    let mut result = Vec::new();
    let mut user_input_seen = false;
    let mut events = Vec::new();
    let cwd = path.parent().and_then(|parent| {
        read_history_workspace(&parent.join("history_item.json"))
            .or_else(|| read_history_workspace(&parent.join("history.json")))
    });

    for (index, msg) in messages.into_iter().enumerate() {
        let event_start = events.len();
        let record_type = msg.get("type").and_then(Value::as_str).unwrap_or("");
        let category = msg.get(record_type).and_then(Value::as_str).unwrap_or("");
        let discussion = match record_type {
            "say" => matches!(
                category,
                "task"
                    | "text"
                    | "user_feedback"
                    | "reasoning"
                    | "completion_result"
                    | "plan_completion_result"
                    | "task_progress"
            ),
            "ask" => matches!(
                category,
                "" | "followup" | "plan_mode_respond" | "act_mode_respond" | "completion_result"
            ),
            "question" => true,
            _ => false,
        };
        if matches!(msg.get("type").and_then(Value::as_str), Some("ask" | "say"))
            && (msg.get("ask").and_then(Value::as_str) == Some("tool")
                || msg.get("say").and_then(Value::as_str) == Some("tool"))
            && let Some(tool) = msg
                .get("text")
                .and_then(Value::as_str)
                .and_then(|text| serde_json::from_str::<Value>(text).ok())
            && let Some(name) = tool.get("tool").and_then(Value::as_str)
        {
            let mut event = events::tool_call_event(
                EventContext {
                    event_seq: events.len() as u32,
                    timestamp: json_util::json_i64(msg.get("ts")),
                    source_path: path.to_str().map(str::to_string),
                    source_event_id: Some(format!(
                        "ui_messages:{index}:{}",
                        msg.get("ts").unwrap_or(&Value::Null)
                    )),
                    message_seq: result.len().checked_sub(1).map(|seq| seq as u32),
                    parser_version: EVENT_PARSER_VERSION,
                },
                name.to_string(),
                Some(&tool),
            );
            event.kind = "approval".to_string();
            event.target = None;
            let operation = match name {
                "readFile" => Some(FileOperation::Read),
                "editedExistingFile" | "newFileCreated" | "appliedDiff" => {
                    Some(FileOperation::Write)
                }
                "fileDeleted" => Some(FileOperation::Delete),
                _ => None,
            };
            if let Some(operation) = operation
                && msg.get("partial").and_then(Value::as_bool) != Some(true)
            {
                let files = tool
                    .get("batchFiles")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .filter(|_| name == "readFile")
                    .unwrap_or_else(|| std::slice::from_ref(&tool));
                for file in files {
                    if let Some(path) = file
                        .get("path")
                        .and_then(Value::as_str)
                        .filter(|path| !path.trim().is_empty())
                    {
                        event.files.push(FileEvidence {
                            path: path.to_string(),
                            operation: operation.clone(),
                            kind: FileEvidenceKind::Call,
                            cwd: cwd.clone(),
                            target: None,
                        });
                    }
                }
                event.target = event.files.first().map(|file| file.path.clone());
            }
            event.attrs_json = Some(msg.to_string());
            events.push(event);
        }
        if !discussion {
            if events.len() == event_start {
                let mut event = events::tool_call_event(
                    EventContext {
                        event_seq: events.len() as u32,
                        timestamp: json_util::json_i64(msg.get("ts")),
                        source_path: path.to_str().map(str::to_string),
                        source_event_id: Some(format!(
                            "ui_messages:{index}:{}",
                            msg.get("ts").unwrap_or(&Value::Null)
                        )),
                        message_seq: result.len().checked_sub(1).map(|seq| seq as u32),
                        parser_version: EVENT_PARSER_VERSION,
                    },
                    category.to_string(),
                    msg.get("text"),
                );
                event.kind =
                    if record_type == "ask" { "approval" } else { "observation" }.to_string();
                event.target = None;
                if category == "command"
                    && let Some(command) = msg.get("text").and_then(Value::as_str)
                {
                    let (files, status) = events::shell_file_evidence(command, None);
                    event.files = files;
                    event.command_evidence_status = Some(status);
                }
                event.attrs_json = Some(msg.to_string());
                events.push(event);
            }
            continue;
        }
        let msg_type = msg.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match msg_type {
            "say" => {
                let say_type = msg.get("say").and_then(|v| v.as_str()).unwrap_or("");
                match say_type {
                    "task" => {
                        if let Some(text) = extract_text(&msg) {
                            let timestamp = msg.get("ts").and_then(|v| v.as_i64());
                            result.push(RawMessage { role: Role::User, content: text, timestamp });
                            user_input_seen = true;
                        }
                    }
                    "text" => {
                        if let Some(text) = extract_text(&msg) {
                            if text.trim().is_empty() {
                                continue;
                            }
                            let role = if !user_input_seen {
                                user_input_seen = true;
                                Role::User
                            } else {
                                Role::Assistant
                            };
                            let timestamp = msg.get("ts").and_then(|v| v.as_i64());
                            result.push(RawMessage { role, content: text, timestamp });
                        }
                    }
                    "user_feedback" => {
                        if let Some(text) = extract_text(&msg) {
                            let timestamp = msg.get("ts").and_then(|v| v.as_i64());
                            result.push(RawMessage { role: Role::User, content: text, timestamp });
                        }
                    }
                    "api_req_started" => {}
                    "reasoning"
                    | "completion_result"
                    | "plan_completion_result"
                    | "task_progress" => {
                        if let Some(text) = extract_text(&msg) {
                            if text.trim().is_empty() {
                                continue;
                            }
                            let timestamp = msg.get("ts").and_then(|v| v.as_i64());
                            result.push(RawMessage {
                                role: Role::Assistant,
                                content: text,
                                timestamp,
                            });
                        }
                    }
                    _ => {}
                }
            }
            "ask" => {
                if let Some(text) = extract_text(&msg) {
                    let timestamp = msg.get("ts").and_then(|v| v.as_i64());
                    result.push(RawMessage { role: Role::Assistant, content: text, timestamp });
                }
            }
            "question" => {
                if let Some(text) =
                    msg.get("question").and_then(|v| v.as_str()).map(|s| s.to_string())
                {
                    let timestamp = msg.get("ts").and_then(|v| v.as_i64());
                    result.push(RawMessage { role: Role::Assistant, content: text, timestamp });
                }
            }
            _ => {}
        }
    }

    Ok((result, events))
}

fn extract_text(msg: &Value) -> Option<String> {
    msg.get("text").and_then(|v| v.as_str()).map(|s| s.to_string())
}

fn task_started_at(entry: &FileScanEntry, messages: &[RawMessage]) -> i64 {
    if let Ok(ts) = entry.session_id.parse::<i64>() {
        return ts;
    }
    let Some(task_dir) = entry.stat_target.parent() else {
        return messages.first().and_then(|message| message.timestamp).unwrap_or(0);
    };
    read_history_i64(&task_dir.join("history_item.json"), "ts")
        .or_else(|| read_history_i64(&task_dir.join("history.json"), "ts"))
        .or_else(|| messages.first().and_then(|message| message.timestamp))
        .unwrap_or(0)
}

fn read_history_i64(path: &Path, field: &str) -> Option<i64> {
    let content = fs::read_to_string(path).ok()?;
    let meta: Value = serde_json::from_str(&content).ok()?;
    json_util::json_i64(meta.get(field))
}

fn extract_directory(messages_path: &Path) -> Option<String> {
    let task_dir = messages_path.parent()?;
    if let Some(workspace) = read_history_workspace(&task_dir.join("history_item.json"))
        .or_else(|| read_history_workspace(&task_dir.join("history.json")))
    {
        return Some(workspace);
    }
    extract_directory_from_metadata(&task_dir.join("task_metadata.json"))
}

fn read_history_workspace(path: &Path) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    let meta: Value = serde_json::from_str(&content).ok()?;
    meta.get("workspace")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn extract_directory_from_metadata(metadata_path: &Path) -> Option<String> {
    let content = fs::read_to_string(metadata_path).ok()?;
    let meta: Value = serde_json::from_str(&content).ok()?;
    let files = meta.get("files_in_context").and_then(|v| v.as_array())?;
    if let Some(first_file) = files.first()
        && let Some(path_str) = first_file.get("path").and_then(|v| v.as_str())
        && let Some(parent) = Path::new(path_str).parent()
    {
        return Some(parent.to_string_lossy().to_string());
    }
    None
}

fn append_cli_store_sessions(mut sessions: Vec<RawSession>) -> anyhow::Result<Vec<RawSession>> {
    let covered = sessions.iter().map(|session| session.source_id.clone()).collect();
    sessions.extend(cli_store::scan_uncovered(&covered)?);
    Ok(sessions)
}

fn ids_covered_by_plugin(context: &AdapterSyncContext, emitted: &[RawSession]) -> HashSet<String> {
    let mut ids = emitted.iter().map(|session| session.source_id.clone()).collect::<HashSet<_>>();
    for path in context.session_paths() {
        if !is_cli_source_path(path.source_file_path.as_deref()) {
            ids.insert(path.source_id.clone());
        }
    }
    ids
}

fn is_cli_source_path(path: Option<&str>) -> bool {
    path.is_some_and(|value| {
        value.ends_with(".messages.json") && !value.ends_with("ui_messages.json")
    })
}

fn merge_scan_results(into: &mut SyncScanResult, extra: SyncScanResult) {
    into.sessions.extend(extra.sessions);
    into.observations.extend(extra.observations);
    into.stats.skipped_sessions += extra.stats.skipped_sessions;
    into.stats.filtered_sessions += extra.stats.filtered_sessions;
    into.stats.candidates += extra.stats.candidates;
    into.stats.rejected_before_parse += extra.stats.rejected_before_parse;
    into.stats.parsed += extra.stats.parsed;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{schema, store::Store};
    use crate::types::Session;

    fn setup_store() -> Store {
        schema::register_sqlite_vec();
        Store::open_in_memory().unwrap()
    }

    fn temp_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "recall-cline-test-{}-{}",
            label,
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_task(root: &Path, task_id: &str, messages: &str) -> PathBuf {
        let task_dir = root.join(task_id);
        fs::create_dir_all(&task_dir).unwrap();
        let path = task_dir.join("ui_messages.json");
        fs::write(&path, messages).unwrap();
        path
    }

    fn make_existing_session(source_id: &str, updated_at: i64, message_count: u32) -> Session {
        Session {
            id: format!("internal-{source_id}"),
            source: "cline".to_string(),
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
    fn load_ui_messages_parses_text_and_feedback() {
        let root = temp_root("parse");
        let messages_json = r#"[
            {"ts": 1000, "type": "say", "say": "text", "text": "hello world"},
            {"ts": 2000, "type": "say", "say": "text", "text": "hi there"},
            {"ts": 3000, "type": "say", "say": "user_feedback", "text": "fix it"},
            {"ts": 4000, "type": "say", "say": "tool", "text": "{\"tool\":\"readFile\",\"path\":\"foo.txt\"}"}
        ]"#;
        let path = write_task(&root, "1000", messages_json);

        let (msgs, events) = load_ui_records(&path).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "approval");
        assert_eq!(events[0].files[0].path, "foo.txt");
        assert_eq!(events[0].message_seq, Some(2));
        assert_eq!(events[0].status, None);
        assert!(events[0].attrs_json.as_ref().unwrap().contains("readFile"));
        assert_eq!(msgs.len(), 3);
        assert!(matches!(msgs[0].role, Role::User));
        assert_eq!(msgs[0].content, "hello world");
        assert!(matches!(msgs[1].role, Role::Assistant));
        assert_eq!(msgs[1].content, "hi there");
        assert!(matches!(msgs[2].role, Role::User));
        assert_eq!(msgs[2].content, "fix it");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn load_ui_messages_question_type() {
        let root = temp_root("question");
        let messages_json = r#"[
            {"ts": 1000, "type": "say", "say": "task", "text": "do something"},
            {"ts": 2000, "type": "say", "say": "text", "text": "ok, let me check"},
            {"ts": 3000, "type": "question", "question": "请问你要选择哪个方案？"},
            {"ts": 4000, "type": "say", "say": "text", "text": "根据你的选择继续"}
        ]"#;
        let path = write_task(&root, "3000", messages_json);

        let msgs = load_ui_messages(&path).unwrap();
        assert_eq!(msgs.len(), 4);
        // Task is User
        assert!(matches!(msgs[0].role, Role::User));
        assert_eq!(msgs[0].content, "do something");
        // Text after task is Assistant
        assert!(matches!(msgs[1].role, Role::Assistant));
        assert_eq!(msgs[1].content, "ok, let me check");
        // Question is Assistant
        assert!(matches!(msgs[2].role, Role::Assistant));
        assert_eq!(msgs[2].content, "请问你要选择哪个方案？");
        // Text is Assistant
        assert!(matches!(msgs[3].role, Role::Assistant));
        assert_eq!(msgs[3].content, "根据你的选择继续");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn tool_approvals_preserve_evidence_outside_discussion() {
        let root = temp_root("tools");
        let messages_json = r#"[
            {"ts": 1000, "type": "say", "say": "task", "text": "test tools"},
            {"ts": 2000, "type": "say", "say": "tool", "text": "{\"tool\":\"readFile\",\"path\":\"src/main.rs\"}"},
            {"ts": 3000, "type": "say", "say": "tool", "text": "{\"tool\":\"editedExistingFile\",\"path\":\"src/main.rs\"}"},
            {"ts": 4000, "type": "say", "say": "tool", "text": "{\"tool\":\"listFilesTopLevel\",\"path\":\"src\"}"},
            {"ts": 5000, "type": "say", "say": "tool", "text": "{\"tool\":\"searchFiles\",\"path\":\"vllm\",\"regex\":\"gelu\",\"filePattern\":\"*.py\"}"},
            {"ts": 6000, "type": "ask", "ask": "command", "text": "rm -rf build", "partial": true},
            {"ts": 7000, "type": "say", "say": "command_output", "text": "permission denied"},
            {"ts": 8000, "type": "ask", "ask": "tool", "text": "{incomplete", "partial": true},
            {"ts": 9000, "type": "say", "say": "text", "text": "Approval is needed because this changes your project."}
        ]"#;
        let path = write_task(&root, "1000", messages_json);

        let msgs = load_ui_messages(&path).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].content, "test tools");
        assert_eq!(msgs[1].content, "Approval is needed because this changes your project.");
        let (_, events) = load_ui_records(&path).unwrap();
        assert_eq!(events.len(), 7);
        assert_eq!(events[4].kind, "approval");
        assert_eq!(events[5].kind, "observation");
        assert_eq!(events[6].kind, "approval");
        assert!(events.iter().all(|event| event.message_seq == Some(0) && event.status.is_none()));
        assert_eq!(
            serde_json::from_str::<Value>(events[6].attrs_json.as_deref().unwrap()).unwrap()["text"],
            "{incomplete"
        );
        assert_eq!(events[1].kind, "approval");
        assert_eq!(events[1].files[0].operation, FileOperation::Write);
        assert_eq!(events[1].status, None);
        assert!(events[2].files.is_empty());
        assert!(events[3].files.is_empty());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn load_ui_messages_first_text_is_user() {
        let root = temp_root("firstuser");
        let messages_json = r#"[
            {"ts": 1000, "type": "say", "say": "checkpoint_created"},
            {"ts": 2000, "type": "say", "say": "text", "text": "my task"},
            {"ts": 3000, "type": "say", "say": "text", "text": "response"}
        ]"#;
        let path = write_task(&root, "2000", messages_json);

        let msgs = load_ui_messages(&path).unwrap();
        assert_eq!(msgs.len(), 2);
        assert!(matches!(msgs[0].role, Role::User));
        assert!(matches!(msgs[1].role, Role::Assistant));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn load_ui_messages_task_type_is_user() {
        let root = temp_root("tasktype");
        let messages_json = r#"[
            {"ts": 1000, "type": "say", "say": "task", "text": "fix the bug"},
            {"ts": 2000, "type": "say", "say": "checkpoint_created"},
            {"ts": 3000, "type": "say", "say": "api_req_started", "text": "{\"request\":\"<task>fix the bug</task>\"}"},
            {"ts": 4000, "type": "say", "say": "reasoning", "text": "用户想修复bug"},
            {"ts": 5000, "type": "say", "say": "text", "text": "I found the issue"}
        ]"#;
        let path = write_task(&root, "3000", messages_json);

        let msgs = load_ui_messages(&path).unwrap();
        assert_eq!(msgs.len(), 3);
        // First message should be User from "task" type
        assert!(matches!(msgs[0].role, Role::User));
        assert_eq!(msgs[0].content, "fix the bug");
        // Reasoning should be Assistant
        assert!(matches!(msgs[1].role, Role::Assistant));
        // Text after task should be Assistant
        assert!(matches!(msgs[2].role, Role::Assistant));
        assert_eq!(msgs[2].content, "I found the issue");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn collect_cline_entries_skips_dirs_without_ui_messages() {
        let root = temp_root("collect");
        let good = root.join("1765706891317");
        fs::create_dir_all(&good).unwrap();
        fs::write(good.join("ui_messages.json"), "[]").unwrap();

        let empty = root.join("019e6d8d-588b-7fd2-a326-c525469ed120");
        fs::create_dir_all(&empty).unwrap();

        fs::write(empty.join("api_conversation_history.json"), r#"[{"role":"assistant","content":[{"type":"tool_use","id":"only","name":"write_to_file","input":{"path":"only.rs"}}]}]"#).unwrap();
        let entries = collect_cline_entries(&root);
        assert_eq!(entries.len(), 2);
        let sessions = scan_task_dirs(std::slice::from_ref(&root)).unwrap();
        assert_eq!(sessions.len(), 1);
        assert!(sessions[0].messages.is_empty());
        assert_eq!(sessions[0].events[0].files[0].path, "only.rs");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn collect_cline_entries_accepts_uuid_task_ids() {
        let root = temp_root("uuid-collect");
        let uuid = "019e6d8d-588b-7fd2-a326-c525469ed120";
        let task = root.join(uuid);
        fs::create_dir_all(&task).unwrap();
        fs::write(task.join("ui_messages.json"), "[]").unwrap();

        let entries = collect_cline_entries(&root);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id, uuid);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_cline_task_sets_started_at_from_dir_name() {
        let root = temp_root("startedat");
        let messages_json = r#"[
            {"ts": 1000, "type": "say", "say": "text", "text": "hello"}
        ]"#;
        let path = write_task(&root, "1765706891317", messages_json);
        let api_path = path.parent().unwrap().join("api_conversation_history.json");
        fs::write(&api_path, serde_json::json!([
            {"role":"assistant","id":"native-message","condenseParent":"summary-1","content":[
                {"type":"tool_use","id":"call-1","name":"write_to_file","input":{"path":"a.rs","content":"next"}},
                {"type":"tool_use","id":"call-2","name":"read_file","input":{"files":[{"path":"a.rs"},{"path":"b.rs"}]}}
            ]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"call-1","is_error":true,"content":"denied"}]},
            {"role":"assistant","content":"<write_to_file><path>legacy.rs</path><content><read_file><path>not-a-call</path></read_file></content></write_to_file><read_file><args><file><path>a&amp;b.rs</path></file><file><path>c.rs</path></file></args></read_file>"},
            {"role":"user","content":[{"type":"text","text":"[write_to_file for 'legacy.rs'] Result:"},{"type":"text","text":"operation denied"}]}
        ]).to_string()).unwrap();
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();

        let entry = FileScanEntry {
            session_id: "1765706891317".to_string(),
            stat_target: path.clone(),
            directory: None,
        };
        let raw = parse_cline_task(entry, mtime).unwrap().unwrap();

        assert_eq!(raw.source_id, "1765706891317");
        assert_eq!(raw.started_at, 1765706891317);
        assert_eq!(raw.updated_at, Some(mtime));
        assert_eq!(raw.source_file_path.as_deref(), path.to_str());
        assert_eq!(raw.messages.len(), 1);

        assert_eq!(raw.events.len(), 6);
        assert!(raw.events.iter().all(|event| event.message_seq.is_none()));
        assert_eq!(raw.events[0].tool_call_id.as_deref(), Some("call-1"));
        assert_eq!(raw.events[0].source_path.as_deref(), api_path.to_str());
        assert!(raw.events[0].attrs_json.as_ref().unwrap().contains("condenseParent"));
        assert_eq!(raw.events[1].files.len(), 2);
        assert_eq!(raw.events[2].status.as_deref(), Some("error"));
        assert!(raw.events[2].files.is_empty());
        assert_eq!(raw.events[3].files[0].path, "legacy.rs");
        assert_eq!(
            raw.events[4].files.iter().map(|file| file.path.as_str()).collect::<Vec<_>>(),
            ["a&b.rs", "c.rs"]
        );
        assert!(raw.events[3].tool_call_id.is_none());
        assert_eq!(raw.events[5].kind, "tool_result");
        assert_eq!(raw.events[5].status, None);
        assert!(raw.events[5].attrs_json.as_ref().unwrap().contains("operation denied"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_cline_task_sets_started_at_from_history_item_for_uuid() {
        let root = temp_root("uuid-startedat");
        let uuid = "019e6d8d-588b-7fd2-a326-c525469ed120";
        let messages_json = r#"[{"ts":1000,"type":"say","say":"text","text":"hello"}]"#;
        let path = write_task(&root, uuid, messages_json);
        fs::write(
            path.parent().unwrap().join("history_item.json"),
            r#"{"ts":1765706891317,"workspace":"/work/roo"}"#,
        )
        .unwrap();
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();

        let entry =
            FileScanEntry { session_id: uuid.to_string(), stat_target: path, directory: None };
        let raw = parse_cline_task(entry, mtime).unwrap().unwrap();
        assert_eq!(raw.source_id, uuid);
        assert_eq!(raw.started_at, 1765706891317);
        assert_eq!(raw.directory.as_deref(), Some("/work/roo"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_cline_task_returns_none_for_empty_messages() {
        let root = temp_root("empty");
        let path = write_task(&root, "1000", "[]");
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();

        let entry = FileScanEntry {
            session_id: "1000".to_string(),
            stat_target: path.clone(),
            directory: None,
        };
        let raw = parse_cline_task(entry, mtime).unwrap();
        assert!(raw.is_none());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_cline_task_returns_none_for_invalid_json() {
        let root = temp_root("invalid");
        let task_dir = root.join("1000");
        fs::create_dir_all(&task_dir).unwrap();
        let path = task_dir.join("ui_messages.json");
        fs::write(&path, "not valid json").unwrap();
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();

        let entry = FileScanEntry {
            session_id: "1000".to_string(),
            stat_target: path.clone(),
            directory: None,
        };
        let raw = parse_cline_task(entry, mtime).unwrap();
        assert!(raw.is_none());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_for_sync_skips_unchanged_session() {
        let root = temp_root("skip");
        let messages_json = r#"[
            {"ts": 1000, "type": "say", "say": "text", "text": "hello"}
        ]"#;
        let path = write_task(&root, "1765706891317", messages_json);
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();

        let store = setup_store();
        store.insert_session(&make_existing_session("1765706891317", mtime, 1)).unwrap();

        let result = scan_task_dirs_for_sync(
            std::slice::from_ref(&root),
            &AdapterSyncContext::from_store_for_test(&store, "cline").unwrap(),
            None,
            false,
        )
        .unwrap();
        assert_eq!(result.sessions.len(), 0);
        assert_eq!(result.stats.skipped_sessions, 1);
        let backfill = scan_task_dirs_for_sync(
            std::slice::from_ref(&root),
            &AdapterSyncContext::from_store_for_test(&store, "cline").unwrap(),
            None,
            true,
        )
        .unwrap();
        assert_eq!(backfill.sessions.len(), 1);
        assert_eq!(backfill.sessions[0].event_parser_version, Some(EVENT_PARSER_VERSION));
        store
            .persist_session_events_for_existing_session(
                "cline",
                "1765706891317",
                &[],
                EVENT_PARSER_VERSION,
                Some(mtime),
            )
            .unwrap();
        let refresh = scan_task_dirs_for_sync(
            std::slice::from_ref(&root),
            &AdapterSyncContext::from_store_for_test(&store, "cline").unwrap(),
            None,
            true,
        )
        .unwrap();
        assert_eq!(refresh.sessions.len(), 1);
        assert!(refresh.sessions[0].refresh_session_on_metadata_backfill);
        store
            .persist_topology_for_existing_session(
                "cline",
                "1765706891317",
                &crate::db::store::SessionTopologyWrite {
                    thread_role: None,
                    parents: &[],
                    parser_version: Some(METADATA_PARSER_VERSION),
                },
            )
            .unwrap();
        let current = scan_task_dirs_for_sync(
            std::slice::from_ref(&root),
            &AdapterSyncContext::from_store_for_test(&store, "cline").unwrap(),
            None,
            true,
        )
        .unwrap();
        assert_eq!(current.stats.skipped_sessions, 1);
        let api = path.parent().unwrap().join("api_conversation_history.json");
        fs::write(&api, r#"[{"role":"assistant","content":[{"type":"tool_use","id":"new-call","name":"write_to_file","input":{"path":"new.rs"}}]}]"#).unwrap();
        fs::File::options()
            .write(true)
            .open(&api)
            .unwrap()
            .set_modified(
                std::time::UNIX_EPOCH + std::time::Duration::from_millis((mtime + 1000) as u64),
            )
            .unwrap();
        let changed = scan_task_dirs_for_sync(
            std::slice::from_ref(&root),
            &AdapterSyncContext::from_store_for_test(&store, "cline").unwrap(),
            None,
            false,
        )
        .unwrap();
        assert_eq!(changed.sessions[0].events[0].files[0].path, "new.rs");
        assert_eq!(changed.sessions[0].updated_at, Some(mtime + 1000));
        assert_eq!(file_scan::stat_mtime_ms(&path), Some(mtime));
        let unstable = file_scan::run_file_scan_with_options_and_snapshot(
            &AdapterSyncContext::empty_for_test("cline"),
            None,
            Default::default(),
            collect_all_entries(std::slice::from_ref(&root)),
            task_snapshot,
            |entry, mtime| {
                let raw = parse_cline_task(entry, mtime)?;
                fs::write(&api, "[]")?;
                Ok(raw)
            },
        )
        .unwrap();
        assert!(unstable.sessions.is_empty());
        assert_eq!(unstable.stats.unstable_sessions, 1);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_for_sync_reparses_when_mtime_diverges() {
        let root = temp_root("mismatch");
        let messages_json = r#"[
            {"ts": 1000, "type": "say", "say": "text", "text": "hello"}
        ]"#;
        let path = write_task(&root, "1765706891317", messages_json);
        let actual_mtime = file_scan::stat_mtime_ms(&path).unwrap();

        let store = setup_store();
        store
            .insert_session(&make_existing_session("1765706891317", actual_mtime - 1_000, 1))
            .unwrap();

        let result = scan_task_dirs_for_sync(
            std::slice::from_ref(&root),
            &AdapterSyncContext::from_store_for_test(&store, "cline").unwrap(),
            None,
            false,
        )
        .unwrap();
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].source_id, "1765706891317");
        assert_eq!(result.sessions[0].updated_at, Some(actual_mtime));
        assert_eq!(result.stats.skipped_sessions, 0);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_for_sync_picks_up_new_session() {
        let root = temp_root("new");
        let messages_json = r#"[
            {"ts": 1000, "type": "say", "say": "text", "text": "new task"}
        ]"#;
        write_task(&root, "1765706891317", messages_json);

        let store = setup_store();

        let result = scan_task_dirs_for_sync(
            std::slice::from_ref(&root),
            &AdapterSyncContext::from_store_for_test(&store, "cline").unwrap(),
            None,
            false,
        )
        .unwrap();
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].source_id, "1765706891317");
        assert_eq!(result.stats.skipped_sessions, 0);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn collect_all_entries_reads_every_host_dir() {
        let first = temp_root("host-a");
        let second = temp_root("host-b");
        write_task(&first, "1765706891317", "[]");
        write_task(&second, "1765706891318", "[]");

        let entries = collect_all_entries(&[first.clone(), second.clone()]);
        let mut ids: Vec<_> = entries.into_iter().map(|entry| entry.session_id).collect();
        ids.sort();
        assert_eq!(ids, vec!["1765706891317", "1765706891318"]);

        let _ = fs::remove_dir_all(&first);
        let _ = fs::remove_dir_all(&second);
    }

    #[test]
    fn extract_directory_prefers_history_item_workspace() {
        let root = temp_root("history-workspace");
        let path = write_task(
            &root,
            "1765706891317",
            r#"[{"ts":1,"type":"say","say":"text","text":"hi"}]"#,
        );
        fs::write(
            path.parent().unwrap().join("history_item.json"),
            r#"{"workspace":"/Users/x/git/samzong/Recall"}"#,
        )
        .unwrap();
        fs::write(
            path.parent().unwrap().join("task_metadata.json"),
            r#"{"files_in_context":[{"path":"/tmp/other/file.rs"}]}"#,
        )
        .unwrap();

        assert_eq!(extract_directory(&path).as_deref(), Some("/Users/x/git/samzong/Recall"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn extract_directory_falls_back_to_task_metadata() {
        let root = temp_root("metadata-workspace");
        let path = write_task(
            &root,
            "1765706891317",
            r#"[{"ts":1,"type":"say","say":"text","text":"hi"}]"#,
        );
        fs::write(
            path.parent().unwrap().join("task_metadata.json"),
            r#"{"files_in_context":[{"path":"/repo/src/main.rs"}]}"#,
        )
        .unwrap();

        assert_eq!(extract_directory(&path).as_deref(), Some("/repo/src"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_task_dirs_sets_roo_source_id_and_workspace() {
        let root = temp_root("roo-scan");
        let path = write_task(
            &root,
            "1765706891317",
            r#"[{"ts":1000,"type":"say","say":"task","text":"fix it"}]"#,
        );
        fs::write(
            path.parent().unwrap().join("history_item.json"),
            r#"{"workspace":"/work/roo-project"}"#,
        )
        .unwrap();

        let store = setup_store();
        let result = scan_task_dirs_for_sync(
            std::slice::from_ref(&root),
            &AdapterSyncContext::from_store_for_test(&store, "roo").unwrap(),
            None,
            false,
        )
        .unwrap();
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].source_id, "1765706891317");
        assert_eq!(result.sessions[0].directory.as_deref(), Some("/work/roo-project"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn is_cli_source_path_distinguishes_plugin_and_cli() {
        assert!(!is_cli_source_path(Some(
            "/host/User/globalStorage/saoudrizwan.claude-dev/tasks/1/ui_messages.json"
        )));
        assert!(is_cli_source_path(Some(
            "/Users/x/.cline/data/sessions/1788285868712_2wszo/1788285868712_2wszo.messages.json"
        )));
        assert!(!is_cli_source_path(None));
    }
}
