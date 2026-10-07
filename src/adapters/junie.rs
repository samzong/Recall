use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;
use tracing::warn;

use crate::adapters::events::{self, EventContext};
use crate::adapters::file_scan::{self, FileMetadataSnapshot, FileScanEntry, FileScanOptions};
use crate::adapters::json_util::{json_i64, jsonl_indexed};
use crate::adapters::paths;
use crate::adapters::usage::usage_count;
use crate::adapters::{
    AdapterSyncContext, RawMessage, RawSession, ResumeCommand, SourceAdapter, SyncScanResult,
};
use crate::db::store::IndexedSessionMeta;
use crate::types::{FileEvidence, FileOperation, RawSessionEvent, RawUsageEvent, Role};

pub(crate) struct JunieAdapter;

const USAGE_PARSER_VERSION: u32 = 1;
const EVENT_PARSER_VERSION: u32 = 1;

type Fingerprint = Vec<(PathBuf, Option<FileMetadataSnapshot>)>;

impl SourceAdapter for JunieAdapter {
    fn id(&self) -> &str {
        "junie"
    }
    fn label(&self) -> &str {
        "JN"
    }
    fn usage_parser_version(&self) -> Option<u32> {
        Some(USAGE_PARSER_VERSION)
    }
    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        Some(ResumeCommand::new("junie", &["--session-id", source_id]))
    }
    fn start_command(&self, prompt: String) -> Option<ResumeCommand> {
        Some(ResumeCommand { program: "junie".into(), args: vec!["--prompt".into(), prompt] })
    }
    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let Some(root) = sessions_root() else {
            return Ok(vec![]);
        };
        let index = read_index(&root)?;
        let mut sessions = Vec::new();
        for entry in collect_entries(&root)? {
            let Some(before) = session_snapshot(&entry, &index) else {
                continue;
            };
            let session = parse_entry(entry.clone(), before.effective_mtime_ms(), true, &index)?;
            if session_snapshot(&entry, &index).as_ref() == Some(&before) {
                sessions.extend(session);
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
        let Some(root) = sessions_root() else {
            return Ok(Some(SyncScanResult::default()));
        };
        Ok(Some(scan_for_sync_at(&root, context, since_ts, include_events)?))
    }
}

fn scan_for_sync_at(
    root: &Path,
    context: &AdapterSyncContext,
    since_ts: Option<i64>,
    include_events: bool,
) -> anyhow::Result<SyncScanResult> {
    let index = read_index(root)?;
    file_scan::run_file_scan_with_options_and_snapshot(
        context,
        since_ts,
        FileScanOptions {
            stream_sessions: true,
            usage_parser_version: Some(USAGE_PARSER_VERSION),
            event_parser_version: include_events.then_some(EVENT_PARSER_VERSION),
            metadata_parser_version: None,
        },
        collect_entries(root)?,
        |entry| {
            session_snapshot(entry, &index).map(|snapshot| {
                snapshot.requiring_parse(index_metadata_changed(
                    index.values.get(&entry.session_id),
                    context.session_meta().get(&entry.session_id),
                ))
            })
        },
        |entry, mtime| {
            let changed = index_metadata_changed(
                index.values.get(&entry.session_id),
                context.session_meta().get(&entry.session_id),
            );
            let raw = parse_entry(entry, mtime, include_events, &index)?;
            Ok(raw.map(|mut raw| {
                raw.refresh_session_metadata = changed;
                raw
            }))
        },
    )
}

fn sessions_root() -> Option<PathBuf> {
    let home = paths::env_path_dir("JUNIE_HOME")
        .or_else(|| dirs::home_dir().map(|home| home.join(".junie")))?;
    paths::existing_dir(home.join("sessions"))
}

fn collect_entries(root: &Path) -> anyhow::Result<Vec<FileScanEntry>> {
    let mut entries = Vec::new();
    for item in fs::read_dir(root)? {
        let item = item?;
        if !item.file_type()?.is_dir() {
            continue;
        }
        let path = item.path().join("events.jsonl");
        if path.is_file() {
            entries.push(FileScanEntry {
                session_id: item.file_name().to_string_lossy().into_owned(),
                stat_target: path,
                directory: None,
            });
        }
    }
    entries.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    Ok(entries)
}

fn session_snapshot(
    entry: &FileScanEntry,
    index: &SessionIndex,
) -> Option<file_scan::FileScanSnapshot<Fingerprint>> {
    let dir = entry.stat_target.parent()?;
    let mut fingerprint = Vec::new();
    let mut mtime =
        index.values.get(&entry.session_id).and_then(|meta| timestamp(meta, "updatedAt"));
    for path in [entry.stat_target.clone(), dir.join("state.json"), dir.join("tasks.json")] {
        let snapshot = file_scan::file_metadata_snapshot(&path);
        if path == entry.stat_target && snapshot.is_none() {
            return None;
        }
        mtime = mtime.max(snapshot.as_ref().and_then(FileMetadataSnapshot::mtime_ms));
        fingerprint.push((path, snapshot));
    }
    let index_path = dir.parent()?.join("index.jsonl");
    fingerprint.push((index_path.clone(), file_scan::file_metadata_snapshot(&index_path)));
    Some(file_scan::FileScanSnapshot::new(mtime?, fingerprint))
}

fn index_metadata_changed(meta: Option<&Value>, old: Option<&IndexedSessionMeta>) -> bool {
    let (Some(meta), Some(old)) = (meta, old) else {
        return false;
    };
    text(meta, "projectDir").is_some_and(|cwd| old.directory.as_ref() != Some(&cwd))
        || text(meta, "taskName").is_some_and(|title| old.custom_title.as_ref() != Some(&title))
        || timestamp(meta, "createdAt").is_some_and(|created| old.started_at != created)
}

struct SessionIndex {
    values: HashMap<String, Value>,
    snapshot: Option<FileMetadataSnapshot>,
}

fn read_index(root: &Path) -> anyhow::Result<SessionIndex> {
    let path = root.join("index.jsonl");
    let before = file_scan::file_metadata_snapshot(&path);
    let mut index = HashMap::new();
    if !path.exists() {
        return Ok(SessionIndex { values: index, snapshot: before });
    }
    for record in jsonl_indexed(BufReader::new(fs::File::open(&path)?).lines()) {
        let (_, value) = record?;
        if let Some(id) = value.get("sessionId").and_then(Value::as_str) {
            index.insert(id.to_string(), value);
        }
    }
    anyhow::ensure!(
        file_scan::file_metadata_snapshot(&path) == before,
        "Junie index changed while reading"
    );
    Ok(SessionIndex { values: index, snapshot: before })
}

fn parse_entry(
    entry: FileScanEntry,
    mtime: i64,
    include_events: bool,
    index: &SessionIndex,
) -> anyhow::Result<Option<RawSession>> {
    let index_path =
        entry.stat_target.parent().and_then(Path::parent).map(|root| root.join("index.jsonl"));
    if index_path.as_deref().and_then(file_scan::file_metadata_snapshot) != index.snapshot {
        return Ok(None);
    }
    match parse_session(
        &entry.stat_target,
        &entry.session_id,
        mtime,
        include_events,
        index.values.get(&entry.session_id),
    ) {
        Ok(session) => Ok(session),
        Err(error) => {
            warn!("failed to parse {}: {error}", entry.stat_target.display());
            Ok(None)
        }
    }
}

fn text(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).filter(|s| !s.trim().is_empty()).map(str::to_string)
}

fn timestamp(value: &Value, key: &str) -> Option<i64> {
    json_i64(value.get(key)).filter(|ts| *ts > 0)
}

struct Message {
    raw: RawMessage,
    active: bool,
}

struct Block {
    value: Value,
    timestamp: Option<i64>,
    line: usize,
    cwd: Option<String>,
}

fn parse_session(
    path: &Path,
    id: &str,
    mtime: i64,
    include_events: bool,
    meta: Option<&Value>,
) -> anyhow::Result<Option<RawSession>> {
    let mut messages: Vec<Message> = Vec::new();
    let mut user_ids = HashMap::new();
    let mut assistant_ids = HashMap::new();
    let mut usage = Vec::new();
    let mut usage_seen = HashSet::new();
    let mut blocks: Vec<Block> = Vec::new();
    let mut block_ids = HashMap::new();
    let mut directory = meta.and_then(|m| text(m, "projectDir"));
    let mut current_directory = directory.clone();
    let mut title = None;
    let mut started = meta.and_then(|m| timestamp(m, "createdAt"));
    let mut task_times = Vec::new();
    let mut saw_prompt = false;
    for record in jsonl_indexed(BufReader::new(fs::File::open(path)?).lines()) {
        let (line, value) = record?;
        let ts = timestamp(&value, "timestampMs");
        started = started.or(ts);
        match value.get("kind").and_then(Value::as_str).unwrap_or("") {
            "TaskStartedEvent" => task_times.push(ts),
            "SessionTitleSetEvent" => title = text(&value, "name"),
            "UserPromptEvent" => {
                saw_prompt = true;
                let content = text(&value, "presentablePrompt")
                    .or_else(|| text(&value, "prompt"))
                    .unwrap_or_default();
                let active = value.get("delivery").and_then(Value::as_str) != Some("Failed");
                upsert_message(
                    &mut messages,
                    &mut user_ids,
                    text(&value, "requestId"),
                    Role::User,
                    content,
                    ts,
                    active,
                );
            }
            "UserResponseEvent" => {
                messages.push(Message {
                    raw: RawMessage {
                        role: Role::User,
                        content: text(&value, "prompt").unwrap_or_default(),
                        timestamp: ts,
                    },
                    active: true,
                });
            }
            "UserAsyncResponseEvent" => {
                let content = value
                    .get("entries")
                    .and_then(Value::as_array)
                    .map(|entries| {
                        entries
                            .iter()
                            .map(|entry| {
                                [text(entry, "question"), text(entry, "answer")]
                                    .into_iter()
                                    .flatten()
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            })
                            .collect::<Vec<_>>()
                            .join("\n\n")
                    })
                    .unwrap_or_default();
                messages.push(Message {
                    raw: RawMessage { role: Role::User, content, timestamp: ts },
                    active: true,
                });
            }
            "UserMessagesCommittedToHistory"
            | "UserMessagesDroppedFromHistory"
            | "UserMessagesFailedInHistory" => {
                let active = value["kind"] == "UserMessagesCommittedToHistory";
                if let Some(ids) = value.get("userMessageIds").and_then(Value::as_array) {
                    for id in ids.iter().filter_map(Value::as_str) {
                        if let Some(&index) = user_ids.get(id) {
                            messages[index].active = active;
                        }
                    }
                }
            }
            "SessionA2uxEvent" => {
                let Some(agent) = value.pointer("/event/agentEvent") else {
                    continue;
                };
                let kind = agent.get("kind").and_then(Value::as_str).unwrap_or("");
                if kind == "LlmResponseMetadataEvent"
                    && let Some(rows) = agent.get("modelUsage").and_then(Value::as_array)
                {
                    let identity = format!("{}:{agent}", ts.unwrap_or_default());
                    if usage_seen.insert(identity) {
                        for (row, tokens) in rows.iter().enumerate() {
                            if let Some(event) = parse_usage(tokens, ts, line, row, path) {
                                usage.push(event);
                            }
                        }
                    }
                }
                if agent
                    .pointer("/agent/kind")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| kind != "MainAgent")
                {
                    continue;
                }
                match kind {
                    "CurrentDirectoryUpdatedEvent" => {
                        if let Some(cwd) = text(agent, "currentDirectory") {
                            directory = directory.or_else(|| Some(cwd.clone()));
                            current_directory = Some(cwd);
                        }
                    }
                    "AgentTaskNameUpdatedEvent" => {
                        title = title.or_else(|| text(agent, "name"));
                    }
                    "MarkdownBlockUpdatedEvent" | "ResultBlockUpdatedEvent" => {
                        let content = text(
                            agent,
                            if kind == "MarkdownBlockUpdatedEvent" { "text" } else { "result" },
                        )
                        .unwrap_or_default();
                        let content = content
                            .strip_prefix("<!-- ANSWER -->")
                            .unwrap_or(&content)
                            .trim()
                            .to_string();
                        upsert_message(
                            &mut messages,
                            &mut assistant_ids,
                            text(agent, "stepId"),
                            Role::Assistant,
                            content,
                            ts,
                            true,
                        );
                    }
                    "ViewFilesBlockUpdatedEvent"
                    | "TerminalBlockUpdatedEvent"
                    | "FileChangesBlockUpdatedEvent"
                    | "ToolBlockUpdatedEvent"
                        if include_events =>
                    {
                        let key = text(agent, "stepId").unwrap_or_else(|| format!("line:{line}"));
                        if let Some(&index) = block_ids.get(&key) {
                            let block: &mut Block = &mut blocks[index];
                            if (kind != "ToolBlockUpdatedEvent" || block.value["kind"] == kind)
                                && let (Some(previous), Some(update)) =
                                    (block.value.as_object_mut(), agent.as_object())
                            {
                                previous.extend(update.clone());
                            }
                        } else {
                            block_ids.insert(key, blocks.len());
                            blocks.push(Block {
                                value: agent.clone(),
                                timestamp: ts,
                                line,
                                cwd: current_directory.clone(),
                            });
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    if !saw_prompt {
        let state_path = path.with_file_name("state.json");
        if state_path.exists() {
            match read_state(&state_path) {
                Ok(state) => {
                    directory = directory.or_else(|| text(&state, "currentDirectory"));
                    let prompts = state_prompts(&state);
                    let offset = task_times.len().saturating_sub(prompts.len());
                    for (index, content) in prompts.into_iter().enumerate() {
                        messages.push(Message {
                            raw: RawMessage {
                                role: Role::User,
                                content,
                                timestamp: task_times
                                    .get(offset + index)
                                    .copied()
                                    .flatten()
                                    .or(started),
                            },
                            active: true,
                        });
                    }
                }
                Err(error) => warn!("failed to parse {}: {error}", state_path.display()),
            }
        }
    }
    let mut messages: Vec<_> = messages
        .into_iter()
        .filter(|m| m.active && !m.raw.content.trim().is_empty())
        .map(|m| m.raw)
        .collect();
    messages.sort_by_key(|m| m.timestamp);
    if messages.is_empty() && usage.is_empty() {
        return Ok(None);
    }
    let session_events = blocks
        .into_iter()
        .enumerate()
        .map(|(seq, block)| parse_block(block, seq as u32, path))
        .collect();
    let mut session = RawSession::search_only(
        id,
        directory,
        started.unwrap_or(mtime),
        Some(mtime),
        None,
        messages,
    )
    .with_usage(usage, USAGE_PARSER_VERSION);
    session.source_file_path = path.to_str().map(str::to_string);
    session.custom_title = meta.and_then(|m| text(m, "taskName")).or(title);
    if include_events {
        session = session.with_events(session_events, EVENT_PARSER_VERSION);
    }
    Ok(Some(session))
}

fn upsert_message(
    messages: &mut Vec<Message>,
    ids: &mut HashMap<String, usize>,
    id: Option<String>,
    role: Role,
    content: String,
    timestamp: Option<i64>,
    active: bool,
) {
    if let Some(index) = id.as_ref().and_then(|id| ids.get(id)).copied() {
        messages[index].raw.content = content;
        messages[index].active = active;
    } else {
        if let Some(id) = id {
            ids.insert(id, messages.len());
        }
        messages.push(Message { raw: RawMessage { role, content, timestamp }, active });
    }
}

fn read_state(path: &Path) -> anyhow::Result<Value> {
    let value: Value = serde_json::from_reader(fs::File::open(path)?)?;
    let blob = value
        .pointer("/event/agentEvent/blob")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing agent state blob"))?;
    Ok(serde_json::from_str(blob)?)
}

fn state_prompts(state: &Value) -> Vec<String> {
    let mut prompts = state
        .get("previousPromptDescriptions")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(|prompt| prompt.strip_prefix("## Issue Task\n").unwrap_or(prompt).to_string())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if let Some(issue) = state.pointer("/lastAgentState/issue")
        && let Some(prompt) = text(issue, "description")
    {
        prompts.push(prompt);
    }
    prompts
}

fn parse_usage(
    tokens: &Value,
    ts: Option<i64>,
    line: usize,
    row: usize,
    path: &Path,
) -> Option<RawUsageEvent> {
    let ts = ts?;
    if !["inputTokens", "outputTokens", "cacheInputTokens", "cacheCreateTokens"]
        .iter()
        .any(|key| json_i64(tokens.get(*key)).is_some())
    {
        return None;
    }
    let mut event = RawUsageEvent::observed(
        format!("line:{line}:{row}"),
        line as u32,
        ts,
        USAGE_PARSER_VERSION,
    );
    event.model = text(tokens, "model").unwrap_or_else(|| "unknown".into());
    event.provider = text(tokens, "provider").unwrap_or_else(|| "unknown".into());
    event.input_tokens = usage_count(tokens, &["inputTokens"]);
    event.output_tokens = usage_count(tokens, &["outputTokens"]);
    event.cache_read_tokens = usage_count(tokens, &["cacheInputTokens"]);
    event.cache_write_tokens = usage_count(tokens, &["cacheCreateTokens"]);
    event.source_path = path.to_str().map(str::to_string);
    event.raw_usage_json = Some(tokens.to_string());
    Some(event)
}

fn parse_block(block: Block, seq: u32, path: &Path) -> RawSessionEvent {
    let value = &block.value;
    let kind = value["kind"].as_str().unwrap_or("");
    let (event_kind, name) = match kind {
        "ViewFilesBlockUpdatedEvent" => ("file_read", "open"),
        "TerminalBlockUpdatedEvent" => ("command", "bash"),
        "FileChangesBlockUpdatedEvent" => ("file_write", "edit"),
        _ => ("tool_call", "tool"),
    };
    let mut event = EventContext {
        event_seq: seq,
        timestamp: block.timestamp,
        source_path: path.to_str().map(str::to_string),
        source_event_id: Some(format!("line:{}", block.line)),
        message_seq: None,
        parser_version: EVENT_PARSER_VERSION,
    }
    .event(event_kind, "assistant");
    event.name = Some(name.into());
    event.tool_call_id = text(value, "stepId");
    event.status = text(value, "status").map(|status| match status.as_str() {
        "COMPLETED" => "success".into(),
        "FAILED" => "error".into(),
        _ => status.to_ascii_lowercase(),
    });
    event.summary = text(value, "output")
        .or_else(|| text(value, "details"))
        .or_else(|| text(value, "text"))
        .map(events::bounded_summary);
    if kind == "TerminalBlockUpdatedEvent" {
        event.target = text(value, "command");
        if let Some(command) = &event.target {
            let (files, status) = events::shell_file_evidence(command, block.cwd.as_deref());
            event.files = files;
            event.command_evidence_status = Some(status);
        }
        if let Some(code) = json_i64(value.get("exitCode")) {
            event.status = Some(if code == 0 { "success" } else { "error" }.into());
            event.attrs_json = Some(serde_json::json!({"exit_code": code}).to_string());
        }
    } else if kind == "ViewFilesBlockUpdatedEvent" {
        if let Some(files) = value.get("files").and_then(Value::as_array) {
            for file in files {
                if let Some(path) =
                    text(file, "absolutePath").or_else(|| text(file, "relativePath"))
                {
                    event.files.push(FileEvidence::call(
                        path,
                        FileOperation::Read,
                        block.cwd.clone(),
                    ));
                }
            }
        }
    } else if kind == "FileChangesBlockUpdatedEvent"
        && let Some(changes) = value.get("changes").and_then(Value::as_array)
    {
        for change in changes {
            let before =
                text(change, "beforeAbsolutePath").or_else(|| text(change, "beforeRelativePath"));
            let after =
                text(change, "afterAbsolutePath").or_else(|| text(change, "afterRelativePath"));
            match (before, after) {
                (Some(before), Some(after)) if before != after => {
                    event.files.push(FileEvidence::call(
                        before,
                        FileOperation::MoveFrom,
                        block.cwd.clone(),
                    ));
                    event.files.push(FileEvidence::call(
                        after,
                        FileOperation::MoveTo,
                        block.cwd.clone(),
                    ));
                }
                (_, Some(after)) => event.files.push(FileEvidence::call(
                    after,
                    FileOperation::Write,
                    block.cwd.clone(),
                )),
                (Some(before), None) => event.files.push(FileEvidence::call(
                    before,
                    FileOperation::Delete,
                    block.cwd.clone(),
                )),
                _ => {}
            }
        }
    }
    if event.target.is_none() {
        event.target = event.files.first().map(|file| file.path.clone());
    }
    event
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let session = root.path().join("session-261007-070507-916x");
        fs::create_dir_all(&session).unwrap();
        fs::write(
            session.join("events.jsonl"),
            include_str!("../../tests/fixtures/junie/events.jsonl"),
        )
        .unwrap();
        fs::write(
            session.join("state.json"),
            include_str!("../../tests/fixtures/junie/state.json"),
        )
        .unwrap();
        fs::write(
            root.path().join("index.jsonl"),
            include_str!("../../tests/fixtures/junie/index.jsonl"),
        )
        .unwrap();
        (root, session.join("events.jsonl"))
    }

    fn parse_fixture(path: &Path, include_events: bool) -> RawSession {
        let id = path.parent().unwrap().file_name().unwrap().to_str().unwrap();
        let index = read_index(path.parent().unwrap().parent().unwrap()).unwrap();
        parse_session(path, id, 1791371400000, include_events, index.values.get(id))
            .unwrap()
            .unwrap()
    }

    fn write_records(path: &Path, records: &[Value]) {
        fs::write(path, records.iter().map(|v| format!("{v}\n")).collect::<String>()).unwrap();
    }

    fn agent(at: i64, value: Value) -> Value {
        json!({"kind":"SessionA2uxEvent","timestampMs":at,"event":{"agentEvent":value}})
    }

    #[test]
    fn real_headless_session_and_resume_keep_prompts_tools_and_usage() {
        let (_root, path) = fixture();
        let session = parse_fixture(&path, true);
        assert_eq!(session.directory.as_deref(), Some("/workspace/junie-probe"));
        assert_eq!(session.started_at, 1791371107366);
        assert_eq!(session.source_file_path.as_deref(), path.to_str());
        let users: Vec<_> = session.messages.iter().filter(|m| m.role == Role::User).collect();
        assert_eq!(users.len(), 2);
        assert!(users[0].content.starts_with("Use a native file-reading tool"));
        assert_eq!(users[0].timestamp, Some(1791371107393));
        assert_eq!(
            users[1].content,
            "Read probe.txt and confirm the word after. Do not modify files or delegate."
        );
        assert_eq!(users[1].timestamp, Some(1791371311896));
        assert_eq!(session.messages.iter().filter(|m| m.role == Role::Assistant).count(), 2);
        assert!(session.messages.iter().all(|m| !m.content.contains("tool parameters")));
        assert_eq!(session.events.len(), 4);
        assert_eq!(session.events.iter().filter(|e| e.kind == "file_read").count(), 2);
        let command = session.events.iter().find(|e| e.kind == "command").unwrap();
        assert_eq!(command.target.as_deref(), Some("wc -l probe.txt"));
        assert_eq!(command.status.as_deref(), Some("success"));
        assert_eq!(command.summary.as_deref(), Some("       1 probe.txt"));
        assert_eq!(command.attrs_json.as_deref(), Some("{\"exit_code\":0}"));
        let write = session.events.iter().find(|e| e.kind == "file_write").unwrap();
        assert_eq!(write.files[0].path, "/workspace/junie-probe/probe.txt");
        assert_eq!(write.files[0].operation, FileOperation::Write);
        assert_eq!(session.usage_events.len(), 14);
        let totals = session.usage_events.iter().fold([0; 4], |mut sums, u| {
            for (sum, n) in sums.iter_mut().zip([
                u.input_tokens,
                u.output_tokens,
                u.cache_read_tokens,
                u.cache_write_tokens,
            ]) {
                *sum += n;
            }
            sums
        });
        assert_eq!(totals, [2778, 895, 39301, 20682]);
        assert!(
            session
                .usage_events
                .iter()
                .all(|u| u.model == "gpt-5.6-luna" && u.reasoning_tokens == 0)
        );
        let without_events = parse_fixture(&path, false);
        assert!(without_events.events.is_empty());
        assert!(without_events.event_parser_version.is_none());
        assert_eq!(without_events.messages.len(), session.messages.len());
        assert_eq!(without_events.usage_events.len(), 14);
    }

    #[test]
    fn streaming_replies_and_prompt_delivery_are_projected_once() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("events.jsonl");
        write_records(
            &path,
            &[
                json!({"kind":"UserPromptEvent","timestampMs":1000,"requestId":"one","prompt":"raw","presentablePrompt":"help"}),
                json!({"kind":"UserPromptEvent","timestampMs":1001,"requestId":"one","presentablePrompt":"help"}),
                json!({"kind":"UserPromptEvent","timestampMs":1002,"requestId":"two","prompt":"dropped"}),
                json!({"kind":"UserMessagesDroppedFromHistory","userMessageIds":["two"]}),
                json!({"kind":"UserPromptEvent","timestampMs":1003,"requestId":"three","prompt":"failed","delivery":"Failed"}),
                agent(
                    1004,
                    json!({"kind":"MarkdownBlockUpdatedEvent","stepId":"reply","text":"partial"}),
                ),
                agent(
                    1005,
                    json!({"kind":"MarkdownBlockUpdatedEvent","stepId":"reply","text":"complete"}),
                ),
                agent(
                    1006,
                    json!({"kind":"MarkdownBlockUpdatedEvent","agent":{"kind":"SubAgent"},"stepId":"child","text":"private child reply"}),
                ),
                json!({"kind":"UserResponseEvent","timestampMs":1007,"prompt":"yes"}),
                json!({"kind":"UserAsyncResponseEvent","timestampMs":1008,"entries":[{"question":"which?","answer":"first"}]}),
            ],
        );
        let session = parse_session(&path, "s", 2000, true, None).unwrap().unwrap();
        assert_eq!(
            session.messages.iter().map(|m| m.content.as_str()).collect::<Vec<_>>(),
            ["help", "complete", "yes", "which?\nfirst"]
        );
        assert_eq!(session.messages[1].timestamp, Some(1004));
    }

    #[test]
    fn usage_requires_recorded_counts_and_deduplicates_replays_only() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("events.jsonl");
        let usage = agent(
            2000,
            json!({"kind":"LlmResponseMetadataEvent","modelUsage":[{"model":"test","inputTokens":10,"outputTokens":5},{"model":"test","inputTokens":10,"outputTokens":5}]}),
        );
        write_records(
            &path,
            &[
                json!({"kind":"UserPromptEvent","timestampMs":1000,"prompt":"hello"}),
                usage.clone(),
                usage,
                agent(
                    3000,
                    json!({"kind":"LlmResponseMetadataEvent","modelUsage":[{"model":"test","cost":1.2},{"model":"test","inputTokens":-3,"outputTokens":0}]}),
                ),
            ],
        );
        let session = parse_session(&path, "s", 4000, true, None).unwrap().unwrap();
        assert_eq!(session.usage_events.len(), 3);
        assert_ne!(session.usage_events[0].event_key, session.usage_events[1].event_key);
        assert_eq!(session.usage_events[2].input_tokens, 0);
        assert_eq!(session.usage_events.iter().map(|u| u.input_tokens).sum::<i64>(), 20);
        write_records(
            &path,
            &[json!({"kind":"UserPromptEvent","timestampMs":1000,"prompt":"hello"})],
        );
        let session = parse_session(&path, "s", 4000, true, None).unwrap().unwrap();
        assert!(session.usage_events.is_empty());
    }

    #[test]
    fn malformed_lines_missing_metadata_and_unknown_records_are_tolerated() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("events.jsonl");
        fs::write(&path, "broken\n{\"kind\":\"unknown\",\"timestampMs\":500}\n{\"kind\":\"UserPromptEvent\",\"timestampMs\":1000,\"prompt\":\"hello\"}\n").unwrap();
        fs::write(path.with_file_name("state.json"), "broken").unwrap();
        let session = parse_session(&path, "s", 4000, true, None).unwrap().unwrap();
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.started_at, 500);
        assert!(session.directory.is_none());
        assert!(read_index(root.path()).unwrap().values.is_empty());
        write_records(&path, &[json!({"kind":"unknown"})]);
        assert!(parse_session(&path, "s", 4000, true, None).unwrap().is_none());
    }

    #[test]
    fn terminal_cwd_failure_and_file_moves_keep_source_evidence() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("events.jsonl");
        write_records(
            &path,
            &[
                json!({"kind":"UserPromptEvent","timestampMs":1000,"prompt":"hello"}),
                agent(
                    1100,
                    json!({"kind":"CurrentDirectoryUpdatedEvent","currentDirectory":"/repo/sub"}),
                ),
                agent(
                    1200,
                    json!({"kind":"TerminalBlockUpdatedEvent","stepId":"run","command":"git restore -- missing","status":"COMPLETED","exitCode":1}),
                ),
                agent(
                    1300,
                    json!({"kind":"FileChangesBlockUpdatedEvent","stepId":"move","changes":[{"beforeRelativePath":"old","afterRelativePath":"new"},{"beforeRelativePath":"deleted"},{"afterRelativePath":"added"}]}),
                ),
            ],
        );
        let session = parse_session(&path, "s", 4000, true, Some(&json!({"projectDir":"/repo"})))
            .unwrap()
            .unwrap();
        assert_eq!(session.directory.as_deref(), Some("/repo"));
        assert_eq!(session.events[0].status.as_deref(), Some("error"));
        assert_eq!(session.events[0].files[0].cwd.as_deref(), Some("/repo/sub"));
        assert_eq!(
            session.events[1].files.iter().map(|f| f.operation.clone()).collect::<Vec<_>>(),
            [
                FileOperation::MoveFrom,
                FileOperation::MoveTo,
                FileOperation::Delete,
                FileOperation::Write
            ]
        );
    }

    #[test]
    fn session_index_changes_are_isolated_and_metadata_only_changes_refresh() {
        let (root, _path) = fixture();
        let events = root.path().join("session-261007-070507-916x/events.jsonl");
        let mut body = fs::read_to_string(&events).unwrap();
        body.push_str("{\"kind\":\"SessionTitleSetEvent\",\"name\":\"Old historical title\"}\n");
        fs::write(events, body).unwrap();
        let context = AdapterSyncContext::empty_for_test("junie");
        let first = scan_for_sync_at(root.path(), &context, None, true).unwrap();
        let raw = &first.sessions[0];
        let mut session = crate::types::test_support::session("indexed");
        session.source = "junie".into();
        session.source_id = raw.source_id.clone();
        session.started_at = raw.started_at;
        session.updated_at = raw.updated_at;
        session.directory = raw.directory.clone();
        session.custom_title = raw.custom_title.clone();
        session.message_count = raw.messages.len() as u32;
        let store = crate::adapters::test_support::store();
        store.insert_session(&session).unwrap();
        crate::adapters::test_support::seed_empty_usage_state(
            &store,
            "junie",
            &raw.source_id,
            USAGE_PARSER_VERSION,
            raw.updated_at,
        );
        crate::adapters::test_support::seed_empty_event_state(
            &store,
            "junie",
            &raw.source_id,
            EVENT_PARSER_VERSION,
            raw.updated_at,
        );
        let context = AdapterSyncContext::from_store_for_test(&store, "junie").unwrap();
        let entries = collect_entries(root.path()).unwrap();
        let old_index = read_index(root.path()).unwrap();
        let before = session_snapshot(&entries[0], &old_index).unwrap();
        let mut body = fs::read_to_string(root.path().join("index.jsonl")).unwrap();
        body.push_str("{\"sessionId\":\"other\",\"updatedAt\":1999999999999}\n");
        fs::write(root.path().join("index.jsonl"), &body).unwrap();
        let current = read_index(root.path()).unwrap();
        assert_eq!(
            session_snapshot(&entries[0], &current).unwrap().effective_mtime_ms(),
            before.effective_mtime_ms()
        );
        let other_changed = scan_for_sync_at(root.path(), &context, None, true).unwrap();
        assert_eq!(other_changed.stats.parsed, 0);
        assert_eq!(other_changed.stats.skipped_sessions, 1);
        assert!(other_changed.sessions.is_empty());
        assert!(
            parse_entry(entries[0].clone(), before.effective_mtime_ms(), true, &old_index)
                .unwrap()
                .is_none()
        );
        let mut own = current.values[&raw.source_id].clone();
        own["taskName"] = "Renamed task".into();
        own["projectDir"] = "/changed".into();
        fs::write(root.path().join("index.jsonl"), format!("{own}\n")).unwrap();
        let own_changed = scan_for_sync_at(root.path(), &context, None, true).unwrap();
        assert_eq!(own_changed.stats.parsed, 1);
        assert_eq!(own_changed.sessions[0].updated_at, raw.updated_at);
        assert_eq!(own_changed.sessions[0].custom_title.as_deref(), Some("Renamed task"));
        assert_eq!(own_changed.sessions[0].directory.as_deref(), Some("/changed"));
        assert!(own_changed.sessions[0].refresh_session_metadata);
        let current = read_index(root.path()).unwrap();
        let before = session_snapshot(&entries[0], &current).unwrap();
        fs::write(entries[0].stat_target.with_file_name("state.json"), "{}").unwrap();
        assert_ne!(session_snapshot(&entries[0], &current).unwrap(), before);
    }

    #[test]
    fn session_snapshot_uses_own_files_and_index_entry_time_only() {
        let (root, _path) = fixture();
        let entries = collect_entries(root.path()).unwrap();
        let mut index = read_index(root.path()).unwrap();
        let files_mtime = session_snapshot(&entries[0], &index).unwrap().effective_mtime_ms();
        index.values.get_mut(&entries[0].session_id).unwrap()["updatedAt"] =
            (files_mtime + 1000).into();
        assert_eq!(
            session_snapshot(&entries[0], &index).unwrap().effective_mtime_ms(),
            files_mtime + 1000
        );
        index.values.get_mut(&entries[0].session_id).unwrap()["updatedAt"] = 1000.into();
        assert_eq!(
            session_snapshot(&entries[0], &index).unwrap().effective_mtime_ms(),
            files_mtime
        );
        let before = session_snapshot(&entries[0], &index).unwrap();
        let tasks_path = entries[0].stat_target.with_file_name("tasks.json");
        fs::write(&tasks_path, "[]").unwrap();
        let tasks_time = files_mtime + 2000;
        fs::File::open(&tasks_path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(
                std::time::UNIX_EPOCH + std::time::Duration::from_millis(tasks_time as u64),
            ))
            .unwrap();
        let after = session_snapshot(&entries[0], &index).unwrap();
        assert_ne!(after, before);
        assert_eq!(after.effective_mtime_ms(), tasks_time);
        fs::create_dir_all(root.path().join("other/subagents/child")).unwrap();
        fs::write(root.path().join("other/subagents/child/events.jsonl"), "{}").unwrap();
        assert_eq!(collect_entries(root.path()).unwrap().len(), 1);
    }
}
