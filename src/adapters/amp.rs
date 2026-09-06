use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use tracing::{debug, warn};

use crate::adapters::AdapterSyncContext;
use crate::adapters::events::{
    EventContext, patch_file_evidence, shell_file_evidence, tool_call_event, tool_result_event,
};
use crate::adapters::file_scan::{self, FileScanEntry, FileScanOptions, FileScanSnapshot};
use crate::adapters::json_util::{json_i64, rfc3339_ms};
use crate::adapters::paths::{file_uri_to_path, vscode_extension_storage_dirs_from};
use crate::adapters::{
    RawMessage, RawSession, ResumeCommand, SourceAdapter, SyncScanResult, first_timestamp,
};
use crate::types::{
    CommandEvidenceStatus, FileEvidence, FileEvidenceKind, FileOperation, RawSessionEvent, Role,
};

const SOURCE: &str = "amp";
const EXTENSION_ID: &str = "sourcegraph.amp";
const EVENT_PARSER_VERSION: u32 = 1;
const SKIP_NAMES: &[&str] = &["config.json", "settings.json", "secrets.json"];

pub(crate) struct AmpAdapter;

impl SourceAdapter for AmpAdapter {
    fn id(&self) -> &str {
        SOURCE
    }

    fn label(&self) -> &str {
        "AM"
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        Some(ResumeCommand {
            program: "amp".to_string(),
            args: vec!["threads".to_string(), "continue".to_string(), source_id.to_string()],
        })
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        Ok(scan_threads(&thread_roots(), None)?.sessions)
    }

    fn scan_for_sync(
        &self,
        _context: &AdapterSyncContext,
        since_ts: Option<i64>,
        include_events: bool,
    ) -> anyhow::Result<Option<SyncScanResult>> {
        Ok(Some(scan_threads(&thread_roots(), if include_events { None } else { since_ts })?))
    }
}

fn thread_roots() -> Vec<PathBuf> {
    thread_roots_from(
        std::env::var("AMP_THREADS_DIR").ok(),
        std::env::var("XDG_DATA_HOME").ok(),
        dirs::home_dir(),
        dirs::config_dir(),
    )
}

fn thread_roots_from(
    amp_threads_dir: Option<String>,
    xdg_data_home: Option<String>,
    home: Option<PathBuf>,
    config_dir: Option<PathBuf>,
) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(dir) = amp_threads_dir.as_deref().map(str::trim).filter(|value| !value.is_empty()) {
        let path = PathBuf::from(dir);
        if path.is_dir() {
            roots.push(path);
        }
    }
    let xdg = xdg_data_home
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.map(|home| home.join(".local/share")));
    if let Some(xdg) = xdg {
        let path = xdg.join("amp/threads");
        if path.is_dir() {
            roots.push(path);
        }
    }
    for storage in vscode_extension_storage_dirs_from(config_dir, EXTENSION_ID) {
        for name in ["threads3", "threads"] {
            let path = storage.join(name);
            if path.is_dir() {
                roots.push(path);
            }
        }
    }
    roots.sort();
    roots.dedup();
    roots
}

fn scan_threads(roots: &[PathBuf], since_ts: Option<i64>) -> anyhow::Result<SyncScanResult> {
    let context = AdapterSyncContext::new(
        SOURCE.to_string(),
        Default::default(),
        Default::default(),
        Default::default(),
        Default::default(),
        Default::default(),
        Default::default(),
    );
    let entries = collect_thread_files(roots).into_iter().map(|path| FileScanEntry {
        session_id: path.to_string_lossy().into_owned(),
        stat_target: path,
        directory: None,
    });
    let mut scan = file_scan::run_file_scan_with_options_and_snapshot(
        &context,
        None,
        FileScanOptions { event_parser_version: Some(EVENT_PARSER_VERSION), ..Default::default() },
        entries,
        |entry| {
            let snapshot = file_scan::file_metadata_snapshot(&entry.stat_target)?;
            Some(FileScanSnapshot::new(snapshot.mtime_ms()?, snapshot))
        },
        |entry, _| match parse_thread_file(&entry.stat_target) {
            Ok(raw) => Ok(raw),
            Err(error) => {
                warn!("failed to parse Amp thread {}: {error}", entry.stat_target.display());
                Ok(None)
            }
        },
    )?;
    let mut by_id: HashMap<String, RawSession> = HashMap::new();
    for raw in scan.sessions {
        if since_ts.is_some_and(|cutoff| raw.updated_at.is_some_and(|updated| updated < cutoff)) {
            scan.stats.filtered_sessions += 1;
            continue;
        }
        by_id.entry(raw.source_id.clone()).or_insert(raw);
    }
    scan.sessions = by_id.into_values().collect();
    Ok(scan)
}

fn collect_thread_files(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for root in roots {
        let read = match fs::read_dir(root) {
            Ok(read) => read,
            Err(err) => {
                debug!("cannot read Amp threads dir {}: {err}", root.display());
                continue;
            }
        };
        for entry in read.flatten() {
            let path = entry.path();
            if path.is_file() && is_amp_thread_file(&path) {
                files.push(path);
            }
        }
    }
    files
}

fn is_amp_thread_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if !name.ends_with(".json") || SKIP_NAMES.contains(&name) {
        return false;
    }
    let parent = path.parent().and_then(|parent| parent.file_name()).and_then(|name| name.to_str());
    if matches!(parent, Some("threads" | "threads3")) {
        return true;
    }
    name == "thread.json"
        || name.starts_with("conversation")
        || name.starts_with("chat")
        || name.starts_with("T-")
}

fn parse_thread_file(path: &Path) -> anyhow::Result<Option<RawSession>> {
    let raw = fs::read_to_string(path)?;
    let value: Value = match serde_json::from_str(&raw) {
        Ok(value) => value,
        Err(err) => {
            warn!("failed to parse Amp JSON {}: {err}", path.display());
            return Ok(None);
        }
    };
    Ok(parse_thread_value(&value, path))
}

fn parse_thread_value(value: &Value, path: &Path) -> Option<RawSession> {
    let doc = value.get("thread").filter(|thread| thread.is_object()).unwrap_or(value);
    let source_id = doc
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .filter(|stem| *stem != "thread")
                .map(|stem| stem.to_string())
        })?;
    let messages_value = doc.get("messages").or_else(|| value.get("messages"));
    let directory = env_cwd(doc.get("env")).or_else(|| env_cwd(value.get("env")));
    let (messages, events) = parse_messages(messages_value, path);
    if messages.is_empty() && events.is_empty() {
        return None;
    }
    let created = json_i64(doc.get("created")).or_else(|| rfc3339_ms(doc.get("created")));
    let started_at = first_timestamp(created, &messages, &[], &events).unwrap_or(0);
    let updated_at = messages
        .iter()
        .filter_map(|message| message.timestamp)
        .chain(events.iter().filter_map(|event| event.timestamp))
        .max();
    let mut session =
        RawSession::search_only(source_id, directory, started_at, updated_at, None, messages)
            .with_events(events, EVENT_PARSER_VERSION);
    session.source_file_path = path.to_str().map(str::to_string);
    session.custom_title = doc
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_string);
    Some(session)
}

fn parse_messages(value: Option<&Value>, path: &Path) -> (Vec<RawMessage>, Vec<RawSessionEvent>) {
    let mut messages = Vec::new();
    let mut events = Vec::new();
    let mut calls = HashMap::<String, String>::new();
    for (message_index, item) in value.and_then(Value::as_array).into_iter().flatten().enumerate() {
        let Some(role) = parse_role(item.get("role").and_then(Value::as_str).unwrap_or("")) else {
            continue;
        };
        let timestamp = json_i64(item.get("created"))
            .or_else(|| rfc3339_ms(item.get("created")))
            .or_else(|| json_i64(item.get("timestamp")))
            .or_else(|| rfc3339_ms(item.get("timestamp")))
            .or_else(|| json_i64(item.pointer("/meta/sentAt")));
        for (content_index, block) in
            item.get("content").and_then(Value::as_array).into_iter().flatten().enumerate()
        {
            let context = EventContext {
                event_seq: events.len() as u32,
                timestamp,
                source_path: path.to_str().map(str::to_string),
                source_event_id: item.get("messageId").and_then(Value::as_str).map(str::to_string),
                message_seq: messages.len().checked_sub(1).map(|seq| seq as u32),
                parser_version: EVENT_PARSER_VERSION,
            };
            let mut event = match (&role, block.get("type").and_then(Value::as_str)) {
                (Role::Assistant, Some("tool_use")) => {
                    let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                    let input = block.get("input");
                    let mut event = tool_call_event(context, name.to_string(), input);
                    event.kind = "tool_call".to_string();
                    event.target = None;
                    event.tool_call_id =
                        block.get("id").and_then(Value::as_str).map(str::to_string);
                    if let Some(id) = &event.tool_call_id {
                        calls.insert(id.clone(), name.to_string());
                    }
                    if matches!(name, "Bash" | "shell_command" | "async_shell_command") {
                        event.kind = "command".to_string();
                        let (command_key, cwd_key) =
                            if name == "Bash" { ("cmd", "cwd") } else { ("command", "workdir") };
                        let command =
                            input.and_then(|input| input.get(command_key)).and_then(Value::as_str);
                        let raw_cwd = input.and_then(|input| input.get(cwd_key));
                        let cwd = raw_cwd
                            .and_then(Value::as_str)
                            .filter(|cwd| Path::new(cwd).is_absolute());
                        let (files, mut status) = command
                            .map(|command| shell_file_evidence(command, cwd))
                            .unwrap_or((Vec::new(), CommandEvidenceStatus::Unsupported));
                        if raw_cwd.is_some()
                            && cwd.is_none()
                            && status != CommandEvidenceStatus::LimitExceeded
                        {
                            status = CommandEvidenceStatus::Unsupported;
                        }
                        event.files = files;
                        event.command_evidence_status = Some(status);
                    } else if name == "apply_patch" {
                        event.files = input
                            .and_then(|input| input.get("patchText"))
                            .and_then(Value::as_str)
                            .map(patch_file_evidence)
                            .unwrap_or_default();
                    } else if matches!(name, "Read" | "edit_file" | "create_file")
                        && let Some(path) = input
                            .and_then(|input| input.get("path"))
                            .and_then(Value::as_str)
                            .filter(|path| !path.trim().is_empty())
                    {
                        event.files.push(FileEvidence {
                            path: path.to_string(),
                            operation: if name == "Read" {
                                FileOperation::Read
                            } else {
                                FileOperation::Write
                            },
                            kind: FileEvidenceKind::Call,
                            cwd: None,
                            target: None,
                        });
                    }
                    event
                }
                (Role::User, Some("tool_result")) => {
                    let id = block.get("toolUseID").and_then(Value::as_str);
                    let name = id.and_then(|id| calls.get(id)).cloned();
                    let run = block.get("run");
                    let mut event =
                        tool_result_event(context, name.clone(), run.map(Value::to_string));
                    event.tool_call_id = id.map(str::to_string);
                    event.status = run
                        .and_then(|run| run.get("status"))
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    if name.as_deref() == Some("apply_patch") {
                        for file in run
                            .and_then(|run| {
                                run.pointer("/result/files")
                                    .or_else(|| run.pointer("/error/partialResult/files"))
                            })
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                        {
                            if let Some(path) =
                                file.get("uri").and_then(Value::as_str).and_then(file_uri_to_path)
                            {
                                event.files.push(FileEvidence {
                                    path,
                                    operation: FileOperation::Write,
                                    kind: FileEvidenceKind::Observation,
                                    cwd: None,
                                    target: None,
                                });
                            }
                        }
                    }
                    event
                }
                _ => continue,
            };
            event.attrs_json = Some(
                serde_json::json!({
                    "messageId": item.get("messageId"),
                    "message_index": message_index,
                    "content_index": content_index,
                    "part": block
                })
                .to_string(),
            );
            events.push(event);
        }
        let content = extract_text(item.get("content"));
        if !content.is_empty() {
            messages.push(RawMessage { role, content, timestamp });
        }
    }
    (messages, events)
}

fn parse_role(role: &str) -> Option<Role> {
    match role {
        "human" | "user" => Some(Role::User),
        "agent" | "assistant" => Some(Role::Assistant),
        _ => None,
    }
}

fn extract_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.trim().to_string(),
        Some(Value::Array(blocks)) => {
            blocks.iter().filter_map(block_text).collect::<Vec<_>>().join("\n").trim().to_string()
        }
        _ => String::new(),
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

fn env_cwd(env: Option<&Value>) -> Option<String> {
    let env = env?;
    if let Some(cwd) =
        string_path(env.get("cwd")).or_else(|| file_uri_value_to_path(env.get("workingDirectory")))
    {
        return Some(cwd);
    }
    if let Some(cwd) = env_cwd(env.get("initial")) {
        return Some(cwd);
    }
    let trees = env.get("trees").and_then(Value::as_array)?;
    for tree in trees {
        if let Some(cwd) =
            string_path(tree.get("cwd")).or_else(|| file_uri_value_to_path(tree.get("uri")))
        {
            return Some(cwd);
        }
    }
    None
}

fn file_uri_value_to_path(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).and_then(file_uri_to_path)
}

fn string_path(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn fixtures_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/amp")
    }

    #[test]
    fn resume_uses_threads_continue_and_json_id() {
        let command = AmpAdapter.resume_command("T-0199aaaa-bbbb-7ccc-8ddd-eeeeffff0001").unwrap();
        assert_eq!(command.program, "amp");
        assert_eq!(
            command.args,
            vec!["threads", "continue", "T-0199aaaa-bbbb-7ccc-8ddd-eeeeffff0001"]
        );
    }

    #[test]
    fn missing_roots_are_empty() {
        let roots = thread_roots_from(None, None, Some(PathBuf::from("/no/such/home")), None);
        assert!(roots.is_empty());
        let result = scan_threads(&roots, None).unwrap();
        assert!(result.sessions.is_empty());
    }

    #[test]
    fn amp_threads_dir_and_xdg_are_scanned_when_present() {
        let root = tempfile::tempdir().unwrap();
        let amp_dir = root.path().join("amp-threads");
        let xdg = root.path().join("xdg");
        fs::create_dir_all(&amp_dir).unwrap();
        fs::create_dir_all(xdg.join("amp/threads")).unwrap();
        let roots = thread_roots_from(
            Some(amp_dir.to_string_lossy().into_owned()),
            Some(xdg.to_string_lossy().into_owned()),
            Some(PathBuf::from("/unused")),
            None,
        );
        assert_eq!(roots, vec![amp_dir, xdg.join("amp/threads")]);
    }

    #[test]
    fn vscode_threads3_and_threads_children_are_roots() {
        let config = tempfile::tempdir().unwrap();
        let storage =
            config.path().join("Code").join("User").join("globalStorage").join(EXTENSION_ID);
        fs::create_dir_all(storage.join("threads3")).unwrap();
        fs::create_dir_all(storage.join("threads")).unwrap();
        let roots = thread_roots_from(
            None,
            None,
            Some(PathBuf::from("/unused")),
            Some(config.path().to_path_buf()),
        );
        assert!(roots.contains(&storage.join("threads3")));
        assert!(roots.contains(&storage.join("threads")));
    }

    #[test]
    fn parse_prefers_object_id_over_filename_thread_json() {
        let path = fixtures_dir().join("thread.json");
        let session = parse_thread_file(&path).unwrap().unwrap();
        assert_eq!(session.source_id, "T-0199aaaa-bbbb-7ccc-8ddd-eeeeffff0001");
        assert_eq!(session.custom_title.as_deref(), Some("Amp thread title"));
        assert_eq!(session.directory.as_deref(), Some("/tmp/amp-project"));
        assert_eq!(session.started_at, 1_700_000_000_000);
        assert_eq!(session.updated_at, Some(1_700_000_002_000));
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, Role::User);
        assert_eq!(session.messages[0].content, "hello from amp");
        assert_eq!(session.messages[1].role, Role::Assistant);
        assert_eq!(session.messages[1].content, "hi back");
    }

    #[test]
    fn parse_reads_thread_messages_wrapper() {
        let path = fixtures_dir().join("wrapped-thread.json");
        let session = parse_thread_file(&path).unwrap().unwrap();
        assert_eq!(session.source_id, "T-0199cccc-dddd-7eee-8fff-000011112222");
        assert_eq!(session.messages[0].content, "wrapped user");
        assert_eq!(session.messages[1].content, "wrapped assistant");
        let payload = "完整".repeat(3000);
        let value = serde_json::json!({
            "thread": { "id": "native", "env": {"initial": {"trees": [{"uri": "file:///tmp/work"}]}},
                "messages": [
                    {"messageId": "m1", "role": "user", "content": [{"type": "text", "text": "update both files"}]},
                    {"messageId": "m2", "role": "assistant", "created": 1700000004000i64, "content": [
                        {"type": "tool_use", "id": "read1", "name": "Read", "input": {"path": "a.rs"}},
                        {"type": "tool_use", "id": "edit1", "name": "edit_file", "input": {"path": "a.rs", "old_str": "a", "new_str": "b"}},
                        {"type": "tool_use", "id": "patch1", "name": "apply_patch", "input": {"patchText": "*** Begin Patch\n*** Update File: a.rs\n*** Move to: b.rs\n*** Add File: c.rs\n+new\n*** End Patch"}},
                        {"type": "tool_use", "id": "blank", "name": "create_file", "input": {"path": "   "}}
                    ]},
                    {"messageId": "m3", "role": "user", "created": 1700000005000i64, "content": [
                        {"type": "tool_result", "toolUseID": "read1", "run": {"status": "done", "result": payload}},
                        {"type": "tool_result", "toolUseID": "edit1", "run": {"status": "rejected-by-user"}},
                        {"type": "tool_result", "toolUseID": "patch1", "run": {"status": "error", "error": {"partialResult": {"files": [{"uri": "file:///tmp/work/b.rs"}, {"uri": "file:///tmp/work/c.rs"}]}}}},
                        {"type": "tool_use", "id": "wrong-role", "name": "create_file", "input": {"path": "not-a-call"}}
                    ]}
                ]}
        });
        let session = parse_thread_value(&value, &path).unwrap();
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.events.len(), 7);
        assert_eq!(session.events[0].files[0].operation, FileOperation::Read);
        assert_eq!(session.events[1].files[0].operation, FileOperation::Write);
        assert!(
            session.events.iter().flat_map(|event| &event.files).all(|file| file.cwd.is_none())
        );
        assert_eq!(
            session.events[2]
                .files
                .iter()
                .map(|file| (&file.path, &file.operation))
                .collect::<Vec<_>>(),
            vec![
                (&"a.rs".to_string(), &FileOperation::MoveFrom),
                (&"b.rs".to_string(), &FileOperation::MoveTo),
                (&"c.rs".to_string(), &FileOperation::Write)
            ]
        );
        assert!(session.events[3].files.is_empty());
        assert_eq!(session.events[4].status.as_deref(), Some("done"));
        assert!(session.events[4].files.is_empty());
        assert!(session.events[4].attrs_json.as_deref().unwrap().contains(&payload));
        assert_eq!(session.events[5].status.as_deref(), Some("rejected-by-user"));
        assert_eq!(session.events[6].status.as_deref(), Some("error"));
        assert_eq!(session.events[6].files.len(), 2);
        assert!(
            session.events[6].files.iter().all(|file| file.kind == FileEvidenceKind::Observation)
        );
        assert_eq!(session.events[6].tool_call_id.as_deref(), Some("patch1"));
        assert_eq!(session.events[6].source_event_id.as_deref(), Some("m3"));
        assert!(session.events.iter().all(|event| event.message_seq == Some(0)));
        assert_eq!(session.updated_at, Some(1700000005000));
        let mut value = value;
        value["thread"]["env"]["initial"]["trees"] = serde_json::json!([
            {"uri": "file:///wrong/first"}, {"uri": "file:///other/second"}
        ]);
        value["thread"]["messages"] = serde_json::json!([
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "bash", "name": "Bash", "input": {"cmd": "mv a.rs b.rs", "cwd": "/actual/base"}},
                {"type": "tool_use", "id": "shell", "name": "shell_command", "input": {"command": "git restore -- a.rs", "workdir": "/actual/other"}},
                {"type": "tool_use", "id": "async", "name": "async_shell_command", "input": {"command": "mv c.rs d.rs"}},
                {"type": "tool_use", "id": "dynamic", "name": "Bash", "input": {"cmd": "mv a.rs b.rs", "cwd": "relative"}}
            ]},
            {"role": "user", "content": [{"type": "tool_result", "toolUseID": "bash", "run": {"status": "done", "result": {"output": "Error: failed", "exitCode": 1}}}]}
        ]);
        let session = parse_thread_value(&value, &path).unwrap();
        assert!(session.messages.is_empty());
        assert_eq!(session.directory.as_deref(), Some("/wrong/first"));
        assert_eq!(session.events[0].files[0].cwd.as_deref(), Some("/actual/base"));
        assert_eq!(session.events[1].files[0].cwd.as_deref(), Some("/actual/other"));
        for event in &session.events[2..4] {
            assert_eq!(event.command_evidence_status, Some(CommandEvidenceStatus::Unsupported));
            assert!(event.files.iter().all(|file| file.cwd.is_none()));
        }
        assert!(session.events.iter().all(|event| event.message_seq.is_none()));
        assert!(
            session
                .events
                .iter()
                .flat_map(|event| &event.files)
                .all(|file| file.kind == FileEvidenceKind::Command && file.target.is_none())
        );
        assert_eq!(session.events[4].status.as_deref(), Some("done"));
        let attrs: Value =
            serde_json::from_str(session.events[4].attrs_json.as_deref().unwrap()).unwrap();
        assert_eq!(attrs.pointer("/part/run/result/exitCode"), Some(&serde_json::json!(1)));
    }

    #[test]
    fn env_cwd_falls_back_to_current_amp_tree_uri() {
        let env = serde_json::json!({
            "initial": {
                "trees": [{"uri": "file:///tmp/amp%20project"}]
            }
        });
        assert_eq!(env_cwd(Some(&env)).as_deref(), Some("/tmp/amp project"));
    }

    #[test]
    fn skips_secrets_and_settings() {
        assert!(!is_amp_thread_file(Path::new("/tmp/threads/secrets.json")));
        assert!(!is_amp_thread_file(Path::new("/tmp/threads/settings.json")));
        assert!(!is_amp_thread_file(Path::new("/tmp/threads/config.json")));
        assert!(is_amp_thread_file(Path::new("/tmp/threads/T-abc.json")));
        assert!(is_amp_thread_file(Path::new("/tmp/threads/thread.json")));
        assert!(is_amp_thread_file(Path::new("/tmp/threads/conversation-1.json")));
    }

    #[test]
    fn incremental_rereads_when_mtime_stale_and_size_grown() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("thread.json");
        fs::copy(fixtures_dir().join("thread.json"), &path).unwrap();
        let first_mtime = 1_700_000_000_000i64;
        fs::File::open(&path)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_millis(first_mtime as u64))
            .unwrap();

        let first = scan_threads(&[root.path().to_path_buf()], None).unwrap();
        assert_eq!(first.sessions.len(), 1);
        assert_eq!(first.sessions[0].messages.len(), 2);

        let mut value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        value["messages"].as_array_mut().unwrap().push(serde_json::json!({
            "role": "human",
            "content": [{ "type": "text", "text": "appended without mtime" }],
            "created": 1700000003000u64
        }));
        let content = "large native payload ".repeat(450_000);
        value["messages"].as_array_mut().unwrap().push(serde_json::json!({
            "messageId": "large-call-message", "role": "assistant", "created": 1700000004000i64,
            "content": [{"type": "tool_use", "id": "large-call", "name": "create_file", "input": {"path": "large.txt", "content": content}}]
        }));
        fs::write(&path, serde_json::to_string(&value).unwrap()).unwrap();
        fs::File::open(&path)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_millis(first_mtime as u64))
            .unwrap();

        let result = scan_threads(&[root.path().to_path_buf()], None).unwrap();
        assert_eq!(result.stats.skipped_sessions, 0);
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].messages.len(), 3);
        assert_eq!(result.sessions[0].messages[2].content, "appended without mtime");
        assert_eq!(result.sessions[0].events.len(), 2);
        assert_eq!(result.sessions[0].events[1].files[0].path, "large.txt");
        assert_eq!(result.sessions[0].events[1].tool_call_id.as_deref(), Some("large-call"));
        let attrs: Value =
            serde_json::from_str(result.sessions[0].events[1].attrs_json.as_deref().unwrap())
                .unwrap();
        assert_eq!(
            attrs.pointer("/part/input/content").and_then(Value::as_str),
            Some(content.as_str())
        );
        assert_eq!(result.sessions[0].updated_at, Some(1700000004000));
    }
}
