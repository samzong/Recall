use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;
use tracing::warn;

use crate::adapters::events::{self, EventContext};
use crate::adapters::file_scan::{self, FileScanEntry, FileScanOptions, FileScanSnapshot};
use crate::adapters::json_util::{jsonl_indexed, rfc3339_ms};
use crate::adapters::paths;
use crate::adapters::{
    AdapterSyncContext, RawMessage, RawSession, ResumeCommand, SourceAdapter, SyncScanResult,
    first_timestamp,
};
use crate::types::{
    CommandEvidenceStatus, EvidenceVisibility, FileEvidence, FileOperation, ParentLink,
    ParentRelation, RawUsageEvent, Role, ThreadRole,
};

const USAGE_PARSER_VERSION: u32 = 2;
const EVENT_PARSER_VERSION: u32 = 2;
const METADATA_PARSER_VERSION: u32 = 3;

pub(crate) struct QoderAdapter;

struct QoderEntries {
    entries: Vec<FileScanEntry>,
    parent_ids: HashMap<String, String>,
}

impl SourceAdapter for QoderAdapter {
    fn id(&self) -> &str {
        "qoder"
    }

    fn label(&self) -> &str {
        "QD"
    }

    fn usage_parser_version(&self) -> Option<u32> {
        Some(USAGE_PARSER_VERSION)
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        let (program, id) = native_command(source_id);
        (!id.contains(":agent-")).then(|| ResumeCommand::new(program, &["--resume", id]))
    }

    fn start_command(&self, prompt: String) -> Option<ResumeCommand> {
        Some(crate::adapters::prompt_start("qodercli", prompt))
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let mut sessions = Vec::new();
        let QoderEntries { entries, parent_ids } = collect_entries(&config_dirs()?);
        for entry in entries {
            let Some(before) = snapshot(&entry) else { continue };
            let raw = parse_entry(
                entry.clone(),
                before.effective_mtime_ms(),
                true,
                parent_ids.get(&entry.session_id).map(String::as_str),
            )?;
            if snapshot(&entry).as_ref() == Some(&before) {
                sessions.extend(raw);
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
        let QoderEntries { entries, parent_ids } = collect_entries(&config_dirs()?);
        Ok(Some(file_scan::run_file_scan_with_options_and_snapshot(
            context,
            since_ts,
            FileScanOptions {
                stream_sessions: true,
                usage_parser_version: Some(USAGE_PARSER_VERSION),
                event_parser_version: include_events.then_some(EVENT_PARSER_VERSION),
                metadata_parser_version: Some(METADATA_PARSER_VERSION),
            },
            entries,
            snapshot,
            |entry, mtime| {
                let parent = parent_ids.get(&entry.session_id).map(String::as_str);
                parse_entry(entry, mtime, include_events, parent)
            },
        )?))
    }
}

fn native_command(id: &str) -> (&str, &str) {
    id.strip_prefix("cn:").map_or(("qodercli", id), |id| ("qoderclicn", id))
}

fn config_dirs() -> anyhow::Result<Vec<(PathBuf, &'static str)>> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home dir"))?;
    Ok([
        (paths::env_path_dir("QODER_CONFIG_DIR").unwrap_or_else(|| home.join(".qoder")), ""),
        (
            paths::env_path_dir("QODERCN_CONFIG_DIR").unwrap_or_else(|| home.join(".qoder-cn")),
            "cn:",
        ),
    ]
    .into_iter()
    .filter(|(dir, _)| dir.is_dir())
    .collect())
}

fn collect_entries(dirs: &[(PathBuf, &str)]) -> QoderEntries {
    let mut entries = Vec::new();
    let mut parent_ids = HashMap::new();
    for (dir, prefix) in dirs {
        let Ok(projects) = fs::read_dir(dir.join("projects")) else { continue };
        for project in projects.flatten() {
            let Ok(files) = fs::read_dir(project.path()) else { continue };
            for file in files.flatten() {
                let path = file.path();
                if let Some(id) = transcript_id(&path, false).filter(|id| !id.starts_with("agent-"))
                {
                    entries.push(FileScanEntry {
                        session_id: format!("{prefix}{id}"),
                        stat_target: path,
                        directory: None,
                    });
                } else if path.is_dir() {
                    let Some(parent) = path.file_name().and_then(|name| name.to_str()) else {
                        continue;
                    };
                    if !valid_id(parent) {
                        continue;
                    }
                    let Ok(subagents) = fs::read_dir(path.join("subagents")) else { continue };
                    for file in subagents.flatten() {
                        let path = file.path();
                        if let Some(id) = transcript_id(&path, true) {
                            let session_id = format!("{prefix}{parent}:{id}");
                            parent_ids.insert(session_id.clone(), format!("{prefix}{parent}"));
                            entries.push(FileScanEntry {
                                session_id,
                                stat_target: path,
                                directory: None,
                            });
                        }
                    }
                }
            }
        }
    }
    entries.sort_by(|a, b| a.stat_target.cmp(&b.stat_target));
    let mut claimed = HashSet::new();
    entries.retain(|entry| claimed.insert(entry.session_id.clone()));
    QoderEntries { entries, parent_ids }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn transcript_id(path: &Path, subagent: bool) -> Option<String> {
    if !path.is_file() || path.extension()? != "jsonl" {
        return None;
    }
    let id = path.file_stem()?.to_str()?;
    let valid =
        if subagent { id.starts_with("agent-") && !id.contains(['/', '\\']) } else { valid_id(id) };
    valid.then(|| id.to_string())
}

fn snapshot(entry: &FileScanEntry) -> Option<FileScanSnapshot<file_scan::FileMetadataSnapshot>> {
    let transcript = file_scan::file_metadata_snapshot(&entry.stat_target)?;
    Some(FileScanSnapshot::new(transcript.mtime_ms()?, transcript))
}

fn parse_entry(
    entry: FileScanEntry,
    mtime: i64,
    include_events: bool,
    parent_id: Option<&str>,
) -> anyhow::Result<Option<RawSession>> {
    match parse_transcript(&entry, mtime, include_events, parent_id) {
        Ok(session) => Ok(session),
        Err(error) => {
            warn!("failed to parse {}: {error}", entry.stat_target.display());
            Ok(None)
        }
    }
}

fn parse_transcript(
    entry: &FileScanEntry,
    mtime: i64,
    include_events: bool,
    parent_id: Option<&str>,
) -> anyhow::Result<Option<RawSession>> {
    let path = &entry.stat_target;
    let subagent = parent_id.is_some();
    let source_path = path.to_string_lossy().to_string();
    let mut directory = None;
    let mut relocated_cwd = None;
    let mut worktree_cwd = None;
    let mut original_cwd = None;
    let mut custom_title = None;
    let mut generated_title = None;
    let mut fork_parent = None;
    let mut messages = Vec::new();
    let mut events = Vec::new();
    let mut usage = Vec::<RawUsageEvent>::new();
    let mut usage_index = HashMap::<String, usize>::new();
    let mut seen = HashSet::new();
    let mut calls = HashMap::new();
    let mut parents = HashMap::new();
    let mut event_uuids = Vec::new();
    let mut active_leaf = None;
    let mut rewound = false;
    for item in jsonl_indexed(BufReader::new(fs::File::open(path)?).lines()) {
        let (line, record) = item?;
        let kind = record.get("type").and_then(Value::as_str).unwrap_or("");
        if matches!(kind, "user" | "assistant" | "system")
            && (subagent || record.get("isSidechain").and_then(Value::as_bool) != Some(true))
            && let Some(cwd) = nonempty(record.get("cwd"))
        {
            directory = Some(cwd);
        }
        if fork_parent.is_none() {
            fork_parent = nonempty(record.pointer("/forkedFrom/sessionId"));
        }
        match kind {
            "relocated" => {
                relocated_cwd = nonempty(record.get("relocatedCwd"));
                continue;
            }
            "worktree-state" => {
                if record.get("worktreeSession").is_some_and(Value::is_null) {
                    worktree_cwd = original_cwd.clone();
                } else {
                    original_cwd = nonempty(record.pointer("/worktreeSession/originalCwd"));
                    worktree_cwd = nonempty(record.pointer("/worktreeSession/worktreeCwd"))
                        .or_else(|| nonempty(record.pointer("/worktreeSession/worktreePath")));
                }
                continue;
            }
            "custom-title" => {
                custom_title = nonempty(record.get("customTitle"));
                continue;
            }
            "ai-title" => {
                generated_title = nonempty(record.get("aiTitle"));
                continue;
            }
            "active-leaf" => {
                active_leaf = nonempty(record.get("leafUuid"));
                rewound |= record.get("rewound").and_then(Value::as_bool) == Some(true);
                continue;
            }
            _ => {}
        }
        let uuid = nonempty(record.get("uuid"));
        if let Some(id) = &uuid {
            if !seen.insert(id.clone()) {
                continue;
            }
            parents.insert(
                id.clone(),
                nonempty(record.get("parentUuid"))
                    .or_else(|| nonempty(record.get("logicalParentUuid"))),
            );
        }
        let role = match kind {
            "user" => Role::User,
            "assistant" => Role::Assistant,
            _ => continue,
        };
        if (!subagent && record.get("isSidechain").and_then(Value::as_bool) == Some(true))
            || record.get("isApiErrorMessage").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        let timestamp = rfc3339_ms(record.get("timestamp"));
        let hidden = ["isMeta", "isCompactSummary", "isVirtual", "isVisibleInTranscriptOnly"]
            .iter()
            .any(|key| record.get(*key).and_then(Value::as_bool) == Some(true));
        let content = record.pointer("/message/content");
        let text = visible_text(content);
        let message_seq = (!hidden && !text.is_empty()).then_some(messages.len() as u32);
        if role == Role::Assistant
            && let Some(mut event) = usage_event(
                &record,
                line as u32,
                timestamp.unwrap_or(mtime),
                &source_path,
                message_seq,
            )
        {
            if let Some(index) = usage_index.get(&event.event_key) {
                let existing = &mut usage[*index];
                existing.input_tokens = existing.input_tokens.max(event.input_tokens);
                existing.output_tokens = existing.output_tokens.max(event.output_tokens);
                existing.cache_read_tokens =
                    existing.cache_read_tokens.max(event.cache_read_tokens);
                existing.cache_write_tokens =
                    existing.cache_write_tokens.max(event.cache_write_tokens);
                existing.message_seq = existing.message_seq.or(event.message_seq);
            } else {
                event.event_seq = usage.len() as u32;
                usage_index.insert(event.event_key.clone(), usage.len());
                usage.push(event);
            }
        }
        let event_cwd = nonempty(record.get("cwd")).or_else(|| directory.clone());
        if include_events
            && !hidden
            && let Some(items) = content.and_then(Value::as_array)
        {
            for (index, item) in items.iter().enumerate() {
                let context = EventContext {
                    event_seq: events.len() as u32,
                    timestamp,
                    source_path: Some(source_path.clone()),
                    source_event_id: Some(format!(
                        "{}:{index}",
                        uuid.as_deref()
                            .map(str::to_string)
                            .unwrap_or_else(|| format!("line:{line}"))
                    )),
                    message_seq: message_seq
                        .or_else(|| messages.len().checked_sub(1).map(|s| s as u32)),
                    parser_version: EVENT_PARSER_VERSION,
                };
                let event = match item.get("type").and_then(Value::as_str) {
                    Some("tool_use") if role == Role::Assistant => {
                        let Some(name) = nonempty(item.get("name")) else { continue };
                        let mut event =
                            events::tool_call_event(context, name.clone(), item.get("input"));
                        event.tool_call_id = nonempty(item.get("id"));
                        if let Some(id) = &event.tool_call_id {
                            calls.insert(id.clone(), name.clone());
                        }
                        let operation = match name.as_str() {
                            "Read" => Some(FileOperation::Read),
                            "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => {
                                Some(FileOperation::Write)
                            }
                            _ => None,
                        };
                        if let Some(operation) = operation
                            && let Some(file) = nonempty(item.pointer("/input/file_path"))
                                .or_else(|| nonempty(item.pointer("/input/notebook_path")))
                        {
                            event.target = Some(file.clone());
                            event.kind = if operation == FileOperation::Read {
                                "file_read"
                            } else {
                                "file_write"
                            }
                            .to_string();
                            event.files.push(FileEvidence::call(
                                file,
                                operation,
                                event_cwd.clone(),
                            ));
                        }
                        if matches!(name.as_str(), "Bash" | "PowerShell")
                            && let Some(command) =
                                item.pointer("/input/command").and_then(Value::as_str)
                        {
                            let (files, status) = if name == "Bash" {
                                events::shell_file_evidence(command, event_cwd.as_deref())
                            } else {
                                (Vec::new(), CommandEvidenceStatus::Unsupported)
                            };
                            event.files = files;
                            event.command_evidence_status = Some(status);
                        }
                        event
                    }
                    Some("tool_result") => {
                        let id = nonempty(item.get("tool_use_id"));
                        let mut event = events::tool_result_event(
                            context,
                            id.as_ref().and_then(|id| calls.get(id)).cloned(),
                            Some(visible_text(item.get("content"))),
                        );
                        event.tool_call_id = id;
                        event.status = item
                            .get("is_error")
                            .and_then(Value::as_bool)
                            .map(|error| if error { "error" } else { "success" }.to_string());
                        event.attrs_json = Some(item.to_string());
                        event
                    }
                    _ => continue,
                };
                event_uuids.push(uuid.clone());
                events.push(event);
            }
        }
        if message_seq.is_some() {
            messages.push(RawMessage { role, content: text, timestamp });
        }
    }
    if rewound {
        let mut active = HashSet::new();
        let mut complete = true;
        while let Some(id) = active_leaf {
            if !active.insert(id.clone()) {
                complete = false;
                break;
            }
            let Some(parent) = parents.get(&id) else {
                complete = false;
                break;
            };
            active_leaf = parent.clone();
        }
        if complete && !active.is_empty() {
            for (event, uuid) in events.iter_mut().zip(event_uuids) {
                if uuid.is_some_and(|id| !active.contains(&id)) {
                    event.visibility = Some(EvidenceVisibility::Inactive);
                }
            }
        }
    }
    if messages.is_empty() && usage.is_empty() && events.is_empty() {
        return Ok(None);
    }
    let mut session = RawSession::search_only(
        &entry.session_id,
        worktree_cwd.or(relocated_cwd).or(directory),
        first_timestamp(None, &messages, &usage, &events).unwrap_or(mtime),
        Some(mtime),
        Some("cli".to_string()),
        messages,
    )
    .with_usage(usage, USAGE_PARSER_VERSION);
    session.source_file_path = Some(source_path);
    session.custom_title = custom_title.or(generated_title);
    session.thread_role = Some(if subagent { ThreadRole::Subagent } else { ThreadRole::Primary });
    if let Some(parent) = parent_id {
        session.parent_links.push(ParentLink {
            relation: ParentRelation::Spawn,
            source: "qoder".to_string(),
            source_id: parent.to_string(),
        });
    } else if let Some(parent) = fork_parent {
        let prefix = if entry.session_id.starts_with("cn:") { "cn:" } else { "" };
        session.parent_links.push(ParentLink {
            relation: ParentRelation::Fork,
            source: "qoder".to_string(),
            source_id: format!("{prefix}{parent}"),
        });
    }
    session.metadata_parser_version = Some(METADATA_PARSER_VERSION);
    session.refresh_session_on_metadata_backfill = true;
    if include_events {
        session = session.with_events(events, EVENT_PARSER_VERSION);
    }
    Ok(Some(session))
}

fn nonempty(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::trim).filter(|text| !text.is_empty()).map(str::to_string)
}

fn visible_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn usage_event(
    record: &Value,
    seq: u32,
    timestamp: i64,
    path: &str,
    message_seq: Option<u32>,
) -> Option<RawUsageEvent> {
    let usage = record.pointer("/message/usage")?;
    let count = |key: &str| usage.get(key).and_then(Value::as_i64).filter(|n| *n >= 0).unwrap_or(0);
    let input_tokens = count("input_tokens");
    let output_tokens = count("output_tokens");
    let cache_read_tokens = count("cache_read_input_tokens");
    let cache_write_tokens = count("cache_creation_input_tokens");
    if input_tokens == 0 && output_tokens == 0 && cache_read_tokens == 0 && cache_write_tokens == 0
    {
        return None;
    }
    let input_tokens =
        input_tokens.saturating_sub(cache_read_tokens).saturating_sub(cache_write_tokens).max(0);
    let id = nonempty(record.pointer("/message/id")).unwrap_or_else(|| format!("line:{seq}"));
    Some(RawUsageEvent {
        message_seq,
        model: nonempty(record.pointer("/message/model")).unwrap_or_else(|| "unknown".to_string()),
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_write_tokens,
        source_path: Some(path.to_string()),
        raw_usage_json: Some(usage.to_string()),
        ..RawUsageEvent::observed(format!("assistant:{id}"), seq, timestamp, USAGE_PARSER_VERSION)
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const SESSION: &str = "11111111-1111-4111-8111-111111111111";
    const FIXTURE: &str = include_str!("../../tests/fixtures/qoder/cli.jsonl");

    fn fixture(root: &Path) -> FileScanEntry {
        let path =
            root.join("projects/-tmp-qoder-fixture-project").join(format!("{SESSION}.jsonl"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, FIXTURE).unwrap();
        FileScanEntry { session_id: SESSION.to_string(), stat_target: path, directory: None }
    }

    fn write_records(entry: &FileScanEntry, records: &[Value]) {
        fs::write(&entry.stat_target, records.iter().map(|r| format!("{r}\n")).collect::<String>())
            .unwrap();
    }

    #[test]
    fn parses_cli_envelope_messages_timestamps_and_tools() {
        let root = tempfile::tempdir().unwrap();
        let entry = fixture(root.path());
        let raw = parse_transcript(&entry, 1791370810000, true, None).unwrap().unwrap();
        assert_eq!(raw.directory.as_deref(), Some("/tmp/qoder-fixture-project"));
        assert_eq!(raw.started_at, 1791370800000);
        assert_eq!(raw.updated_at, Some(1791370810000));
        assert_eq!(raw.messages.len(), 3);
        assert_eq!(raw.messages[0].role, Role::User);
        assert_eq!(raw.messages[1].content, "I will inspect the file.");
        assert_eq!(raw.messages[2].timestamp, Some(1791370805000));
        assert_eq!(raw.custom_title.as_deref(), Some("Verify sample file"));
        assert_eq!(raw.events.len(), 6);
        let read = &raw.events[0];
        assert_eq!(read.kind, "file_read");
        assert_eq!(read.files[0].operation, FileOperation::Read);
        assert_eq!(read.files[0].cwd.as_deref(), Some("/tmp/qoder-fixture-project"));
        assert_eq!(raw.events[1].kind, "command");
        assert_eq!(raw.events[1].target.as_deref(), Some("wc -l sample.txt"));
        assert_eq!(raw.events[3].name.as_deref(), Some("Bash"));
        assert_eq!(raw.events[3].status.as_deref(), Some("success"));
        assert_eq!(raw.events[3].tool_call_id.as_deref(), Some("call-shell"));
        assert_eq!(raw.events[4].kind, "file_write");
        assert_eq!(raw.usage_events.len(), 3);
        assert_eq!(raw.usage_events.iter().map(|u| u.input_tokens).sum::<i64>(), 480);
        assert!(raw.usage_events.iter().all(|u| u.provider == "unknown"));
        let without_events = parse_transcript(&entry, 1791370810000, false, None).unwrap().unwrap();
        assert!(without_events.events.is_empty());
        assert_eq!(without_events.event_parser_version, None);
        assert_eq!(without_events.messages.len(), raw.messages.len());
        assert_eq!(without_events.usage_events.len(), raw.usage_events.len());
    }

    #[test]
    fn filters_runtime_text_images_and_auth_errors_without_estimated_usage() {
        let root = tempfile::tempdir().unwrap();
        let entry = fixture(root.path());
        write_records(
            &entry,
            &[
                json!({"type":"user", "isMeta":true, "message":{"content":"injected context"}}),
                json!({"type":"user", "isCompactSummary":true, "message":{"content":"summary"}}),
                json!({"type":"user", "isSidechain":true, "message":{"content":"subagent prompt"}}),
                json!({"type":"assistant", "isApiErrorMessage":true, "message":{"content":[{"type":"text","text":"Please login"}]}}),
                json!({"type":"user", "message":{"content":[{"type":"image","source":{"data":"synthetic-image"}},{"type":"text","text":"describe file"}]}}),
                json!({"type":"assistant", "message":{"content":[{"type":"thinking","thinking":"internal"},{"type":"text","text":"done"}], "usage":{"input_tokens":0,"output_tokens":0,"credits":0.5}}}),
                json!({"type":"token-stats", "promptTokenCount":4000,"timestamp":1791370800000i64}),
            ],
        );
        let raw = parse_transcript(&entry, 10, true, None).unwrap().unwrap();
        assert_eq!(
            raw.messages.iter().map(|m| m.content.as_str()).collect::<Vec<_>>(),
            ["describe file", "done"]
        );
        assert!(raw.usage_events.is_empty());
        assert!(raw.events.is_empty());
    }

    #[test]
    fn deduplicates_response_usage_and_duplicate_records() {
        let root = tempfile::tempdir().unwrap();
        let entry = fixture(root.path());
        let first = json!({"type":"assistant","uuid":"one", "message":{"id":"response", "model":"fixture", "content":[{"type":"tool_use","name":"Read","id":"call","input":{"file_path":"sample.txt"}}]}});
        let last = json!({"type":"assistant","uuid":"two", "message":{"id":"response", "model":"fixture", "content":[{"type":"text","text":"done"}], "usage":{"input_tokens":80,"output_tokens":12}}});
        write_records(&entry, &[first.clone(), first, last.clone(), last]);
        let raw = parse_transcript(&entry, 10, true, None).unwrap().unwrap();
        assert_eq!(raw.messages.len(), 1);
        assert_eq!(raw.events.len(), 1);
        assert_eq!(raw.usage_events.len(), 1);
        assert_eq!(raw.usage_events[0].input_tokens, 80);
    }

    #[test]
    fn discovers_subagents_and_namespaces_china_edition() {
        let root = tempfile::tempdir().unwrap();
        let entry = fixture(root.path());
        let child = entry
            .stat_target
            .parent()
            .unwrap()
            .join(SESSION)
            .join("subagents/agent-a1b2c3d4.jsonl");
        fs::create_dir_all(child.parent().unwrap()).unwrap();
        fs::write(&child, r#"{"type":"user","isSidechain":true,"sessionId":"11111111-1111-4111-8111-111111111111","message":{"content":"inspect file"}}"#).unwrap();
        fs::write(entry.stat_target.parent().unwrap().join("agent-orphan.jsonl"), FIXTURE).unwrap();
        let QoderEntries { entries, parent_ids } =
            collect_entries(&[(root.path().to_path_buf(), "cn:")]);
        assert_eq!(entries.len(), 2);
        let child = entries.iter().find(|e| e.session_id.contains(":agent-")).unwrap();
        let raw = parse_transcript(
            child,
            10,
            true,
            parent_ids.get(&child.session_id).map(String::as_str),
        )
        .unwrap()
        .unwrap();
        assert_eq!(raw.messages[0].content, "inspect file");
        assert_eq!(raw.thread_role, Some(ThreadRole::Subagent));
        assert_eq!(raw.parent_links[0].source_id, format!("cn:{SESSION}"));
        assert!(QoderAdapter.resume_command(&child.session_id).is_none());
        let cmd = QoderAdapter.resume_command(&format!("cn:{SESSION}")).unwrap();
        assert_eq!(cmd.program, "qoderclicn");
        assert_eq!(cmd.args, ["--resume", SESSION]);
    }

    #[test]
    fn discovers_plugin_and_custom_subagent_names_without_losing_parent() {
        let root = tempfile::tempdir().unwrap();
        let entry = fixture(root.path());
        let children = entry.stat_target.parent().unwrap().join(SESSION).join("subagents");
        fs::create_dir_all(&children).unwrap();
        for name in [
            "agent-aplugin:namespace:name-abcd",
            "agent-aCustom agent.v2-abcd",
            "agent-aplugin:agent-helper-abcd",
        ] {
            fs::write(children.join(format!("{name}.jsonl")), r#"{"type":"user","isSidechain":true,"message":{"content":"plugin subagent result"}}"#).unwrap();
        }
        let QoderEntries { entries, parent_ids } =
            collect_entries(&[(root.path().to_path_buf(), "cn:")]);
        assert_eq!(entries.len(), 4);
        for child in entries.iter().filter(|entry| entry.session_id.contains(":agent-")) {
            let raw = parse_transcript(
                child,
                10,
                true,
                parent_ids.get(&child.session_id).map(String::as_str),
            )
            .unwrap()
            .unwrap();
            assert_eq!(raw.messages[0].content, "plugin subagent result");
            assert_eq!(raw.parent_links[0].source_id, format!("cn:{SESSION}"));
            assert_eq!(raw.parent_links[0].relation, ParentRelation::Spawn);
            assert!(QoderAdapter.resume_command(&child.session_id).is_none());
        }
    }

    #[test]
    fn session_directory_prefers_worktree_then_relocation_then_latest_cwd() {
        let root = tempfile::tempdir().unwrap();
        let entry = fixture(root.path());
        let mut records = vec![
            json!({"type":"user","cwd":"/tmp/original","message":{"content":"inspect file"}}),
            json!({"type":"assistant","cwd":"/tmp/latest","message":{"content":"done"}}),
        ];
        write_records(&entry, &records);
        assert_eq!(
            parse_transcript(&entry, 10, true, None).unwrap().unwrap().directory.as_deref(),
            Some("/tmp/latest")
        );
        records
            .push(json!({"type":"relocated","sessionId":SESSION,"relocatedCwd":"/tmp/relocated"}));
        write_records(&entry, &records);
        assert_eq!(
            parse_transcript(&entry, 10, true, None).unwrap().unwrap().directory.as_deref(),
            Some("/tmp/relocated")
        );
        records.push(json!({"type":"worktree-state","sessionId":SESSION,"worktreeSession":{"worktreeCwd":"/tmp/worktree/subdir","worktreePath":"/tmp/worktree","originalCwd":"/tmp/restored"}}));
        write_records(&entry, &records);
        assert_eq!(
            parse_transcript(&entry, 10, true, None).unwrap().unwrap().directory.as_deref(),
            Some("/tmp/worktree/subdir")
        );
        records.push(json!({"type":"worktree-state","sessionId":SESSION,"worktreeSession":null}));
        write_records(&entry, &records);
        assert_eq!(
            parse_transcript(&entry, 10, true, None).unwrap().unwrap().directory.as_deref(),
            Some("/tmp/restored")
        );
    }

    #[test]
    fn reads_first_fork_origin_and_namespaces_china_edition() {
        let root = tempfile::tempdir().unwrap();
        let mut entry = fixture(root.path());
        let mut records: Vec<Value> =
            FIXTURE.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        records[0]["forkedFrom"] = json!({"sessionId":"parent", "messageUuid":"original"});
        records[1]["forkedFrom"] = json!({"sessionId":"other", "messageUuid":"later"});
        write_records(&entry, &records);
        for prefix in ["", "cn:"] {
            entry.session_id = format!("{prefix}{SESSION}");
            let raw = parse_transcript(&entry, 10, true, None).unwrap().unwrap();
            assert_eq!(raw.parent_links.len(), 1);
            assert_eq!(raw.parent_links[0].source_id, format!("{prefix}parent"));
            assert_eq!(raw.parent_links[0].relation, ParentRelation::Fork);
            assert_eq!(raw.custom_title.as_deref(), Some("Verify sample file"));
        }
    }

    #[test]
    fn malformed_jsonl_records_do_not_drop_valid_conversation() {
        let root = tempfile::tempdir().unwrap();
        let entry = fixture(root.path());
        fs::write(&entry.stat_target, format!("invalid\n{FIXTURE}\n{{partial")).unwrap();
        let raw = parse_transcript(&entry, 10, true, None).unwrap().unwrap();
        assert_eq!(raw.messages.len(), 3);
        assert!(collect_entries(&[(root.path().join("missing"), "")]).entries.is_empty());
    }

    #[test]
    fn rewound_tool_events_are_inactive_without_erasing_history() {
        let root = tempfile::tempdir().unwrap();
        let entry = fixture(root.path());
        let mut records: Vec<Value> =
            FIXTURE.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        records.push(json!({"type":"active-leaf","sessionId":SESSION,"leafUuid":"u1","explicit":true,"rewound":true,"timestamp":1791370806000i64}));
        write_records(&entry, &records);
        let raw = parse_transcript(&entry, 10, true, None).unwrap().unwrap();
        assert_eq!(raw.messages.len(), 3);
        assert!(raw.events.iter().all(|e| e.visibility == Some(EvidenceVisibility::Inactive)));
    }
    #[test]
    fn splits_cache_inclusive_usage_and_ignores_invalid_counts() {
        let cached = json!({"type":"assistant","modelSource":"custom","message":{"id":"cached","model":"fixture","usage":{"input_tokens":3250,"output_tokens":40,"cache_read_input_tokens":3000,"cache_creation_input_tokens":200}}});
        let usage = usage_event(&cached, 0, 1, "fixture", None).unwrap();
        assert_eq!(usage.input_tokens, 50);
        assert_eq!(usage.output_tokens, 40);
        assert_eq!(usage.cache_read_tokens, 3000);
        assert_eq!(usage.cache_write_tokens, 200);
        let cache_only = json!({"type":"assistant","message":{"usage":{"input_tokens":5,"cache_read_input_tokens":8,"cache_creation_input_tokens":2}}});
        let usage = usage_event(&cache_only, 0, 1, "fixture", None).unwrap();
        assert_eq!(usage.input_tokens, 0);
        assert_eq!(usage.cache_read_tokens, 8);
        assert_eq!(usage.cache_write_tokens, 2);
        let invalid = json!({"type":"assistant","message":{"usage":{"input_tokens":"120","output_tokens":-5,"credits":0.5}}});
        assert!(usage_event(&invalid, 0, 1, "fixture", None).is_none());
    }

    #[test]
    fn rewind_keeps_tools_before_compaction_active() {
        let root = tempfile::tempdir().unwrap();
        let entry = fixture(root.path());
        write_records(
            &entry,
            &[
                json!({"type":"user","uuid":"u1","parentUuid":null,"message":{"content":"Read then edit"}}),
                json!({"type":"assistant","uuid":"read","parentUuid":"u1","message":{"content":[{"type":"tool_use","name":"Read","id":"read","input":{"file_path":"sample.txt"}}]}}),
                json!({"type":"system","subtype":"compact_boundary","uuid":"boundary","parentUuid":null,"logicalParentUuid":"read"}),
                json!({"type":"user","uuid":"summary","parentUuid":"boundary","isCompactSummary":true,"message":{"content":"summary"}}),
                json!({"type":"user","uuid":"u2","parentUuid":"summary","message":{"content":"Edit file"}}),
                json!({"type":"assistant","uuid":"edit","parentUuid":"u2","message":{"content":[{"type":"tool_use","name":"Edit","id":"edit","input":{"file_path":"sample.txt"}}]}}),
                json!({"type":"active-leaf","leafUuid":"u2","rewound":true}),
            ],
        );
        let raw = parse_transcript(&entry, 10, true, None).unwrap().unwrap();
        assert_eq!(raw.events.len(), 2);
        assert_eq!(raw.events[0].visibility, None);
        assert_eq!(raw.events[1].visibility, Some(EvidenceVisibility::Inactive));
    }

    #[test]
    fn records_powershell_command_and_notebook_file_evidence() {
        let root = tempfile::tempdir().unwrap();
        let entry = fixture(root.path());
        write_records(
            &entry,
            &[json!({"type":"assistant","cwd":"/tmp/project","message":{"content":[
                {"type":"tool_use","name":"PowerShell","id":"shell","input":{"command":"Get-Content sample.txt"}},
                {"type":"tool_use","name":"NotebookEdit","id":"notebook","input":{"notebook_path":"sample.ipynb","new_source":"print(1)"}}
            ]}})],
        );
        let raw = parse_transcript(&entry, 10, true, None).unwrap().unwrap();
        assert_eq!(raw.events[0].kind, "command");
        assert_eq!(raw.events[0].target.as_deref(), Some("Get-Content sample.txt"));
        assert_eq!(raw.events[0].command_evidence_status, Some(CommandEvidenceStatus::Unsupported));
        assert_eq!(raw.events[1].kind, "file_write");
        assert_eq!(raw.events[1].target.as_deref(), Some("sample.ipynb"));
        assert_eq!(raw.events[1].files[0].operation, FileOperation::Write);
        assert_eq!(raw.events[1].files[0].cwd.as_deref(), Some("/tmp/project"));
    }

    #[test]
    fn file_evidence_uses_record_cwd_after_workspace_change() {
        let root = tempfile::tempdir().unwrap();
        let entry = fixture(root.path());
        write_records(
            &entry,
            &[
                json!({"type":"user","cwd":"/tmp/old","message":{"content":"inspect file"}}),
                json!({"type":"assistant","cwd":"/tmp/new","message":{"content":[{"type":"tool_use","name":"Read","id":"read","input":{"file_path":"sample.txt"}}]}}),
            ],
        );
        let raw = parse_transcript(&entry, 10, true, None).unwrap().unwrap();
        assert_eq!(raw.directory.as_deref(), Some("/tmp/new"));
        assert_eq!(raw.events[0].files[0].cwd.as_deref(), Some("/tmp/new"));
    }
}
