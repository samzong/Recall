use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde_json::Value;
use tracing::warn;
use walkdir::WalkDir;

use crate::adapters::AdapterSyncContext;
use crate::adapters::events::{EventContext, tool_call_event, tool_result_event};
use crate::adapters::file_scan::{self, FileScanEntry};
use crate::adapters::json_util::{json_i64, jsonl_indexed};
use crate::adapters::paths;
use crate::adapters::usage::usage_count;
use crate::adapters::{
    RawMessage, RawSession, ResumeCommand, SourceAdapter, SyncScanResult, first_timestamp,
};
use crate::types::{
    FileEvidence, FileEvidenceKind, FileOperation, RawSessionEvent, RawUsageEvent, Role,
};

pub(crate) struct KimiCodeAdapter;

const USAGE_PARSER_VERSION: u32 = 1;
const EVENT_PARSER_VERSION: u32 = 1;

fn kimi_scan_options(include_events: bool) -> file_scan::FileScanOptions {
    file_scan::FileScanOptions {
        usage_parser_version: Some(USAGE_PARSER_VERSION),
        event_parser_version: include_events.then_some(EVENT_PARSER_VERSION),
        ..Default::default()
    }
}

impl SourceAdapter for KimiCodeAdapter {
    fn id(&self) -> &str {
        "kimi-code"
    }
    fn label(&self) -> &str {
        "KC"
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        Some(ResumeCommand {
            program: "kimi".to_string(),
            args: vec!["--session".to_string(), source_id.to_string()],
        })
    }

    fn usage_parser_version(&self) -> Option<u32> {
        Some(USAGE_PARSER_VERSION)
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        scan_kimi_dirs(&resolve_kimi_dirs()?)
    }

    fn scan_for_sync(
        &self,
        context: &AdapterSyncContext,
        since_ts: Option<i64>,
        include_events: bool,
    ) -> anyhow::Result<Option<SyncScanResult>> {
        Ok(Some(scan_kimi_dirs_for_sync(&resolve_kimi_dirs()?, context, since_ts, include_events)?))
    }
}

fn scan_kimi_dirs(sessions_dirs: &[PathBuf]) -> anyhow::Result<Vec<RawSession>> {
    let mut sessions = Vec::new();
    let mut claimed = HashSet::new();
    for sessions_dir in sessions_dirs {
        sessions.extend(scan_kimi_session_entries_with_parser(
            claim_session_entries(collect_session_entries(sessions_dir), &mut claimed),
            |entry, mtime_ms| parse_kimi_session_file(entry, mtime_ms, true),
        )?);
    }
    Ok(sessions)
}

fn scan_kimi_dirs_for_sync(
    sessions_dirs: &[PathBuf],
    context: &AdapterSyncContext,
    since_ts: Option<i64>,
    include_events: bool,
) -> anyhow::Result<SyncScanResult> {
    let mut combined = SyncScanResult::default();
    let mut claimed = HashSet::new();
    for sessions_dir in sessions_dirs {
        combined.absorb(file_scan::run_file_scan_with_options_and_snapshot(
            context,
            since_ts,
            kimi_scan_options(include_events),
            claim_session_entries(collect_session_entries(sessions_dir), &mut claimed),
            kimi_session_snapshot,
            |entry, mtime_ms| parse_kimi_session_file(entry, mtime_ms, include_events),
        )?);
    }
    Ok(combined)
}

fn claim_session_entries(
    entries: Vec<FileScanEntry>,
    claimed: &mut HashSet<String>,
) -> Vec<FileScanEntry> {
    entries.into_iter().filter(|entry| claimed.insert(entry.session_id.clone())).collect()
}

fn resolve_kimi_dirs() -> anyhow::Result<Vec<PathBuf>> {
    if let Some(home) = paths::env_path_dir("KIMI_CODE_HOME") {
        return Ok(paths::existing_dir(home.join("sessions")).into_iter().collect());
    }
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home dir"))?;
    let mut dirs = Vec::new();
    for relative in [".kimi-code/sessions", ".kimi/sessions"] {
        if let Some(dir) = paths::existing_dir(home.join(relative)) {
            dirs.push(dir);
        }
    }
    if dirs.is_empty() {
        warn!("~/.kimi-code/sessions not found, skipping Kimi Code");
    }
    Ok(dirs)
}

fn scan_kimi_session_entries_with_parser<F>(
    entries: Vec<FileScanEntry>,
    parse_fn: F,
) -> anyhow::Result<Vec<RawSession>>
where
    F: Fn(FileScanEntry, i64) -> anyhow::Result<Option<RawSession>>,
{
    let mut sessions = Vec::new();
    for entry in entries {
        let Some(snapshot) = kimi_session_snapshot(&entry) else {
            continue;
        };
        let raw = parse_fn(entry.clone(), snapshot.effective_mtime_ms())?;
        if kimi_session_snapshot(&entry).as_ref() != Some(&snapshot) {
            warn!(
                "skipping unstable Kimi Code session {}: source files changed while parsing ({})",
                entry.session_id,
                entry.stat_target.display()
            );
            continue;
        }
        if let Some(raw) = raw {
            sessions.push(raw);
        }
    }
    Ok(sessions)
}

#[derive(Clone)]
struct StateMeta {
    id: Option<String>,
    cwd: Option<String>,
    created_at: Option<i64>,
    title: Option<String>,
    is_custom_title: bool,
}

fn parse_state_json(content: &str) -> anyhow::Result<StateMeta> {
    let v: Value = serde_json::from_str(content)?;
    anyhow::ensure!(v.is_object(), "state must be a JSON object");
    Ok(StateMeta {
        id: v.get("id").and_then(|s| s.as_str()).map(str::to_string),
        cwd: v.get("cwd").and_then(|s| s.as_str()).map(str::to_string),
        created_at: json_i64(v.get("createdAt")),
        title: v.get("title").and_then(|s| s.as_str()).map(str::to_string),
        is_custom_title: v.get("isCustomTitle").and_then(|b| b.as_bool()).unwrap_or(false),
    })
}

fn read_state_json(path: &Path) -> anyhow::Result<StateMeta> {
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    parse_state_json(&content).with_context(|| format!("failed to parse {}", path.display()))
}

fn collect_session_entries(sessions_dir: &Path) -> Vec<FileScanEntry> {
    let mut entries = Vec::new();
    if !sessions_dir.exists() {
        return entries;
    }

    for dir_entry in WalkDir::new(sessions_dir).max_depth(3).into_iter().filter_map(|e| e.ok()) {
        let path = dir_entry.path();
        if !path.is_file() || path.file_name().and_then(|n| n.to_str()) != Some("state.json") {
            continue;
        }
        let Some(session_dir) = path.parent() else {
            continue;
        };
        let dir_name = session_dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !dir_name.starts_with("session_") {
            continue;
        }
        let wire_path = session_dir.join("agents").join("main").join("wire.jsonl");
        if !wire_path.is_file() {
            continue;
        }

        let meta = match read_state_json(path) {
            Ok(meta) => meta,
            Err(error) => {
                warn!("{error}");
                continue;
            }
        };
        let session_id = meta.id.clone().unwrap_or_else(|| dir_name.to_string());
        entries.push(FileScanEntry {
            session_id,
            stat_target: wire_path,
            directory: meta.cwd.clone(),
        });
    }

    entries
}

fn parse_kimi_session_file(
    entry: FileScanEntry,
    mtime_ms: i64,
    include_events: bool,
) -> anyhow::Result<Option<RawSession>> {
    match parse_kimi_session_file_impl(&entry, mtime_ms, include_events) {
        Ok(raw) => Ok(raw),
        Err(e) => {
            warn!("failed to parse {}: {e}", entry.stat_target.display());
            Ok(None)
        }
    }
}

fn parse_kimi_session_file_impl(
    entry: &FileScanEntry,
    mtime_ms: i64,
    include_events: bool,
) -> anyhow::Result<Option<RawSession>> {
    let wire_path = &entry.stat_target;
    let Some(session_dir) = session_dir_for_wire(wire_path) else {
        return Ok(None);
    };
    let meta = read_state_json(&session_dir.join("state.json"))?;
    let session_id = meta.id.clone().unwrap_or_else(|| entry.session_id.clone());

    let file = fs::File::open(wire_path)?;
    let reader = BufReader::new(file);
    let source_path = wire_path.to_str().map(str::to_string);
    parse_kimi_wire(meta, session_id, reader.lines(), mtime_ms, None, source_path, include_events)
}

#[cfg(test)]
pub(crate) fn parse_conformance_fixture(sessions_dir: &Path) -> anyhow::Result<Option<RawSession>> {
    let Some(entry) = collect_session_entries(sessions_dir).into_iter().next() else {
        return Ok(None);
    };
    let Some(snapshot) = kimi_session_snapshot(&entry) else {
        return Ok(None);
    };
    parse_kimi_session_file_impl(&entry, snapshot.effective_mtime_ms(), true)
}

fn session_dir_for_wire(wire_path: &Path) -> Option<&Path> {
    wire_path.parent().and_then(Path::parent).and_then(Path::parent)
}

#[derive(Debug, PartialEq, Eq)]
struct KimiSessionSnapshot {
    state: file_scan::FileMetadataSnapshot,
    wire: file_scan::FileMetadataSnapshot,
}

fn kimi_session_snapshot(
    entry: &FileScanEntry,
) -> Option<file_scan::FileScanSnapshot<KimiSessionSnapshot>> {
    let session_dir = session_dir_for_wire(&entry.stat_target)?;
    let state = file_scan::file_metadata_snapshot(&session_dir.join("state.json"))?;
    let wire = file_scan::file_metadata_snapshot(&entry.stat_target)?;
    let effective_mtime_ms = state.mtime_ms()?.max(wire.mtime_ms()?);
    Some(file_scan::FileScanSnapshot::new(effective_mtime_ms, KimiSessionSnapshot { state, wire }))
}

fn parse_kimi_wire(
    meta: StateMeta,
    session_id: String,
    lines: impl Iterator<Item = std::io::Result<String>>,
    mtime_ms: i64,
    directory: Option<String>,
    source_path: Option<String>,
    include_events: bool,
) -> anyhow::Result<Option<RawSession>> {
    let directory = meta.cwd.clone().or(directory);
    let mut messages = Vec::new();
    let mut events: Vec<RawSessionEvent> = Vec::new();
    let mut usage_events: Vec<RawUsageEvent> = Vec::new();
    let mut last_provider: Option<String> = None;

    for item in jsonl_indexed(lines) {
        let (line_index, v) = item?;
        let line_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let time = json_i64(v.get("time"));

        match line_type {
            "llm.request" => {
                if let Some(provider) =
                    v.get("provider").and_then(|p| p.as_str()).filter(|p| !p.is_empty())
                {
                    last_provider = Some(provider.to_string());
                }
            }
            "context.append_message" => {
                let origin = v.pointer("/message/origin/kind").and_then(|k| k.as_str());
                let role = match v.pointer("/message/role").and_then(|r| r.as_str()) {
                    Some("user") if origin == Some("user") => Role::User,
                    Some("assistant") => Role::Assistant,
                    _ => continue,
                };
                let text = extract_text_parts(v.pointer("/message/content"));
                if text.is_empty() {
                    continue;
                }
                messages.push(RawMessage { role, content: text, timestamp: time });
            }
            "context.append_loop_event" => {
                let event = v.get("event");
                let event_type = event.and_then(|e| e.get("type")).and_then(|t| t.as_str());
                if include_events
                    && let Some(event) = event
                    && let Some(raw) = extract_tool_event(
                        &v,
                        event,
                        EventContext {
                            event_seq: events.len() as u32,
                            timestamp: time,
                            source_path: source_path.clone(),
                            source_event_id: Some(
                                event
                                    .get("uuid")
                                    .and_then(Value::as_str)
                                    .filter(|id| !id.is_empty())
                                    .map(str::to_string)
                                    .unwrap_or_else(|| format!("wire:{line_index}")),
                            ),
                            message_seq: messages.len().checked_sub(1).map(|seq| seq as u32),
                            parser_version: EVENT_PARSER_VERSION,
                        },
                        directory.as_deref(),
                    )
                {
                    events.push(raw);
                }
                let part = event.and_then(|e| e.get("part"));
                let part_type = part.and_then(|p| p.get("type")).and_then(|t| t.as_str());
                if event_type != Some("content.part") || part_type != Some("text") {
                    continue;
                }
                let text = part
                    .and_then(|p| p.get("text"))
                    .and_then(|t| t.as_str())
                    .map(str::trim)
                    .unwrap_or("");
                if text.is_empty() {
                    continue;
                }
                messages.push(RawMessage {
                    role: Role::Assistant,
                    content: text.to_string(),
                    timestamp: time,
                });
            }
            "usage.record" => {
                if let Some(event) = extract_usage_event(
                    &v,
                    line_index,
                    time.unwrap_or(mtime_ms),
                    last_provider.as_deref(),
                    source_path.as_deref(),
                ) {
                    usage_events.push(event);
                }
            }
            _ => {}
        }
    }

    if messages.is_empty() && usage_events.is_empty() && events.is_empty() {
        return Ok(None);
    }

    let started_at =
        first_timestamp(meta.created_at, &messages, &usage_events, &events).unwrap_or(0);
    let duration_minutes = match (
        first_timestamp(None, &messages, &usage_events, &events),
        crate::adapters::last_timestamp(None, &messages, &usage_events, &events),
    ) {
        (Some(first), Some(last)) if last >= first => Some(((last - first) / 60_000) as u32),
        _ => None,
    };
    let (custom_title, summary) =
        if meta.is_custom_title { (meta.title, None) } else { (None, meta.title) };

    let mut session =
        RawSession::search_only(session_id, directory, started_at, Some(mtime_ms), None, messages);
    session.source_file_path = source_path;
    session.custom_title = custom_title;
    session.summary = summary;
    session.duration_minutes = duration_minutes;
    session = session.with_usage(usage_events, USAGE_PARSER_VERSION);
    if include_events {
        session = session.with_events(events, EVENT_PARSER_VERSION);
    }
    Ok(Some(session))
}

fn extract_tool_event(
    record: &Value,
    event: &Value,
    context: EventContext,
    cwd: Option<&str>,
) -> Option<RawSessionEvent> {
    let mut raw = match event.get("type")?.as_str()? {
        "tool.call" => {
            let name = event.get("name")?.as_str()?;
            let args = event.get("args");
            let path = args
                .and_then(|args| args.get("path"))
                .and_then(Value::as_str)
                .filter(|path| !path.trim().is_empty());
            let operation = match name {
                "Read" => Some(FileOperation::Read),
                "Write" | "Edit" => Some(FileOperation::Write),
                _ => None,
            };
            let mut call = tool_call_event(context, name.to_string(), args);
            call.kind = "tool_call".to_string();
            call.target = None;
            if let Some(operation) = operation
                && let Some(path) = path
            {
                call.kind = match operation {
                    FileOperation::Read => "file_read",
                    _ => "file_write",
                }
                .to_string();
                call.target = Some(path.to_string());
                call.files.push(FileEvidence {
                    path: path.to_string(),
                    operation,
                    kind: FileEvidenceKind::Call,
                    cwd: cwd.map(str::to_string),
                    target: None,
                });
            } else if name == "Bash" {
                call.kind = "command".to_string();
                call.target = args
                    .and_then(|args| args.get("command"))
                    .and_then(Value::as_str)
                    .filter(|command| !command.trim().is_empty())
                    .map(str::to_string);
                if let Some(command) = call.target.as_deref() {
                    let shell_cwd = match args.and_then(|args| args.get("cwd")) {
                        Some(value) => value.as_str(),
                        None => cwd,
                    }
                    .filter(|cwd| Path::new(cwd).is_absolute());
                    let (files, status) =
                        crate::adapters::events::shell_file_evidence(command, shell_cwd);
                    call.files = files;
                    call.command_evidence_status = Some(status);
                }
            }
            call
        }
        "tool.result" => {
            let result = event.get("result");
            let mut result_event = tool_result_event(
                context,
                None,
                result.and_then(|result| result.get("output")).map(|output| match output {
                    Value::String(text) => text.to_string(),
                    value => value.to_string(),
                }),
            );
            result_event.status = result
                .and_then(|result| result.get("isError"))
                .and_then(Value::as_bool)
                .map(|is_error| if is_error { "error" } else { "success" }.to_string());
            result_event
        }
        _ => return None,
    };
    raw.tool_call_id = event
        .get("toolCallId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string);
    raw.attrs_json = Some(record.to_string());
    Some(raw)
}

fn extract_text_parts(content: Option<&Value>) -> String {
    let Some(parts) = content.and_then(|c| c.as_array()) else {
        return String::new();
    };
    let text = parts
        .iter()
        .filter(|part| part.get("type").and_then(|t| t.as_str()) == Some("text"))
        .filter_map(|part| part.get("text").and_then(|t| t.as_str()))
        .collect::<Vec<_>>()
        .join("\n");
    text.trim().to_string()
}

fn extract_usage_event(
    v: &Value,
    line_index: usize,
    timestamp: i64,
    provider: Option<&str>,
    source_path: Option<&str>,
) -> Option<RawUsageEvent> {
    let usage = v.get("usage")?;
    let input_tokens = usage_count(usage, &["inputOther"]);
    let output_tokens = usage_count(usage, &["output"]);
    let cache_read_tokens = usage_count(usage, &["inputCacheRead"]);
    let cache_write_tokens = usage_count(usage, &["inputCacheCreation"]);
    if input_tokens == 0 && output_tokens == 0 && cache_read_tokens == 0 && cache_write_tokens == 0
    {
        return None;
    }

    let model = v
        .get("model")
        .and_then(|m| m.as_str())
        .filter(|m| !m.trim().is_empty())
        .unwrap_or("unknown")
        .to_string();
    Some(RawUsageEvent {
        model,
        provider: provider.unwrap_or("unknown").to_string(),
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_write_tokens,
        source_path: source_path.map(str::to_string),
        raw_usage_json: Some(usage.to_string()),
        ..RawUsageEvent::observed(
            format!("wire:{line_index}"),
            line_index as u32,
            timestamp,
            USAGE_PARSER_VERSION,
        )
    })
}

#[cfg(test)]
pub(crate) fn parse_kimi_session(
    state_json: &str,
    wire_jsonl: &str,
    session_id: &str,
) -> Option<RawSession> {
    let meta = parse_state_json(state_json).ok()?;
    let lines = wire_jsonl.lines().map(|line| Ok(line.to_string()));
    parse_kimi_wire(meta, session_id.to_string(), lines, 0, None, None, true).unwrap()
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;
    use crate::db::store::Store;

    fn fixture_state() -> &'static str {
        r#"{"id":"session_abc","cwd":"/repo","createdAt":1700000000000,"updatedAt":1700000060000,"title":"fix the bug","isCustomTitle":false,"archived":false}"#
    }

    fn fixture_wire() -> &'static str {
        concat!(
            r#"{"type":"metadata","protocol_version":"1.5","created_at":1700000000100}"#,
            "\n",
            r#"{"type":"turn.prompt","time":1700000001000,"origin":{"kind":"user"},"input":[{"type":"text","text":"fix the bug"}]}"#,
            "\n",
            r#"{"type":"context.append_message","time":1700000001001,"message":{"id":"m1","role":"user","origin":{"kind":"user"},"content":[{"type":"text","text":"fix the bug"}]}}"#,
            "\n",
            r#"{"type":"context.append_message","time":1700000002000,"message":{"id":"m2","role":"user","origin":{"kind":"injection"},"content":[{"type":"text","text":"<system-reminder>noise</system-reminder>"}]}}"#,
            "\n",
            r#"{"type":"context.append_message","time":1700000002500,"message":{"id":"m3","role":"user","origin":{"kind":"task"},"content":[{"type":"text","text":"<notification>background task done</notification>"}]}}"#,
            "\n",
            r#"{"type":"llm.request","time":1700000003000,"model":"kimi-k3","provider":"moonshot","kind":"loop"}"#,
            "\n",
            r#"{"type":"context.append_loop_event","time":1700000004000,"event":{"type":"content.part","uuid":"u1","part":{"type":"think","think":"pondering"}}}"#,
            "\n",
            r#"{"type":"context.append_loop_event","time":1700000005000,"event":{"type":"content.part","uuid":"u2","part":{"type":"text","text":"Here is the fix."}}}"#,
            "\n",
            r#"{"type":"context.append_loop_event","time":1700000005500,"event":{"type":"tool.call","uuid":"call-uuid","turnId":"1","step":1,"stepUuid":"step-uuid","toolCallId":"Edit:1","name":"Edit","args":{"path":"README.zh-CN.md","old_string":"old","new_string":"new"}}}"#,
            "\n",
            r#"{"type":"context.append_loop_event","time":1700000005600,"event":{"type":"tool.result","parentUuid":"call-uuid","toolCallId":"Edit:1","result":{"output":"Replaced 1 occurrence in README.zh-CN.md"}}}"#,
            "\n",
            r#"{"type":"usage.record","time":1700000006000,"model":"kimi-k3","usageScope":"turn","usage":{"inputOther":1200,"output":80,"inputCacheRead":300,"inputCacheCreation":40}}"#,
        )
    }

    fn write_kimi_fixture(root: &Path) -> (PathBuf, PathBuf) {
        let session_dir = root.join("wd_repo").join("session_abc");
        let wire_dir = session_dir.join("agents").join("main");
        fs::create_dir_all(&wire_dir).unwrap();
        let state_path = session_dir.join("state.json");
        let wire_path = wire_dir.join("wire.jsonl");
        fs::write(&state_path, fixture_state()).unwrap();
        fs::write(&wire_path, fixture_wire()).unwrap();
        (state_path, wire_path)
    }

    fn append_wire_change(path: &Path) -> std::io::Result<()> {
        fs::OpenOptions::new()
            .append(true)
            .open(path)?
            .write_all(b"\n{\"type\":\"metadata\",\"changed\":true}\n")
    }

    #[derive(Clone, Copy, Debug)]
    enum SnapshotMutation {
        State,
        AppendWire,
        ReplaceState,
    }

    fn run_kimi_snapshot_scan(
        context: &AdapterSyncContext,
        root: &Path,
        mutation: Option<SnapshotMutation>,
    ) -> SyncScanResult {
        file_scan::run_file_scan_with_options_and_snapshot(
            context,
            None,
            kimi_scan_options(true),
            collect_session_entries(root),
            kimi_session_snapshot,
            |entry, mtime_ms| {
                let state_path = session_dir_for_wire(&entry.stat_target).unwrap().join("state.json");
                match mutation {
                    Some(SnapshotMutation::State) => {
                        fs::write(
                            &state_path,
                            r#"{"id":"session_abc","cwd":"/repo","createdAt":1700000000000,"title":"changed while parsing","isCustomTitle":true}"#,
                        )?;
                        parse_kimi_session_file(entry, mtime_ms, true)
                    }
                    Some(SnapshotMutation::AppendWire) => {
                        let raw = parse_kimi_session_file(entry.clone(), mtime_ms, true)?;
                        append_wire_change(&entry.stat_target)?;
                        Ok(raw)
                    }
                    Some(SnapshotMutation::ReplaceState) => {
                        let raw = parse_kimi_session_file(entry, mtime_ms, true)?;
                        let replacement = state_path.with_extension("replacement");
                        fs::write(
                            &replacement,
                            r#"{"id":"session_abc","cwd":"/repo","createdAt":1700000000000,"title":"replacement state with a different length","isCustomTitle":false}"#,
                        )?;
                        fs::remove_file(&state_path)?;
                        fs::rename(replacement, state_path)?;
                        Ok(raw)
                    }
                    None => parse_kimi_session_file(entry, mtime_ms, true),
                }
            },
        )
        .unwrap()
    }

    #[test]
    fn parse_kimi_session_extracts_messages_and_usage() {
        let session = parse_kimi_session(fixture_state(), fixture_wire(), "session_abc").unwrap();

        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, Role::User);
        assert_eq!(session.messages[0].content, "fix the bug");
        assert_eq!(session.messages[1].role, Role::Assistant);
        assert_eq!(session.messages[1].content, "Here is the fix.");

        assert_eq!(session.directory.as_deref(), Some("/repo"));
        assert_eq!(session.started_at, 1700000000000);
        assert_eq!(session.summary.as_deref(), Some("fix the bug"));
        assert_eq!(session.custom_title, None);
        assert_eq!(session.duration_minutes, Some(0));

        assert_eq!(session.usage_events.len(), 1);
        let event = &session.usage_events[0];
        assert_eq!(event.model, "kimi-k3");
        assert_eq!(event.provider, "moonshot");
        assert_eq!(event.input_tokens, 1200);
        assert_eq!(event.output_tokens, 80);
        assert_eq!(event.cache_read_tokens, 300);
        assert_eq!(event.cache_write_tokens, 40);
        assert_eq!(event.token_source, crate::types::TokenSource::Observed);
        assert_eq!(session.usage_parser_version, Some(USAGE_PARSER_VERSION));
        assert_eq!(session.event_parser_version, Some(EVENT_PARSER_VERSION));
        assert_eq!(session.events.len(), 2);
        let call = &session.events[0];
        assert_eq!(call.source_event_id.as_deref(), Some("call-uuid"));
        assert_eq!(call.tool_call_id.as_deref(), Some("Edit:1"));
        assert_eq!(call.timestamp, Some(1700000005500));
        assert_eq!(call.message_seq, Some(1));
        assert_eq!(call.files.len(), 1);
        assert_eq!(call.files[0].path, "README.zh-CN.md");
        assert_eq!(call.files[0].cwd.as_deref(), Some("/repo"));
        assert_eq!(call.files[0].operation, FileOperation::Write);
        assert_eq!(call.files[0].kind, FileEvidenceKind::Call);
        let attrs: Value = serde_json::from_str(call.attrs_json.as_deref().unwrap()).unwrap();
        assert_eq!(attrs.pointer("/event/args/old_string"), Some(&Value::from("old")));
        assert_eq!(attrs.pointer("/event/args/new_string"), Some(&Value::from("new")));
        let result = &session.events[1];
        assert_eq!(result.tool_call_id, call.tool_call_id);
        assert_eq!(result.message_seq, Some(1));
        assert_eq!(result.status, None);
        assert!(result.files.is_empty());
        let attrs: Value = serde_json::from_str(result.attrs_json.as_deref().unwrap()).unwrap();
        assert_eq!(attrs.pointer("/event/parentUuid"), Some(&Value::from("call-uuid")));
        assert_eq!(
            attrs.pointer("/event/result/output"),
            Some(&Value::from("Replaced 1 occurrence in README.zh-CN.md"))
        );
    }

    #[test]
    fn parse_kimi_session_custom_title_goes_to_custom_title() {
        let state = r#"{"id":"session_x","title":"my rename","isCustomTitle":true}"#;
        let session = parse_kimi_session(state, fixture_wire(), "session_x").unwrap();
        assert_eq!(session.custom_title.as_deref(), Some("my rename"));
        assert_eq!(session.summary, None);
    }

    #[test]
    fn parse_kimi_session_empty_wire_returns_none() {
        let wire = r#"{"type":"metadata","protocol_version":"1.5","created_at":1700000000100}"#;
        assert!(parse_kimi_session(fixture_state(), wire, "session_abc").is_none());
        for (name, operation) in [
            ("Read", Some(FileOperation::Read)),
            ("Write", Some(FileOperation::Write)),
            ("DeleteFile", None),
            ("CustomEdit", None),
        ] {
            let record = serde_json::json!({
                "type": "context.append_loop_event", "time": 1700000001000_i64,
                "event": {"type": "tool.call", "uuid": "uuid", "toolCallId": "call",
                    "name": name, "args": {"path": " spaced file.rs ", "content": "code"}}
            })
            .to_string();
            let session = parse_kimi_session(fixture_state(), &record, "session_abc").unwrap();
            assert!(session.messages.is_empty());
            assert!(session.usage_events.is_empty());
            let event = &session.events[0];
            if let Some(operation) = operation {
                assert_eq!(event.files[0].path, " spaced file.rs ");
                assert_eq!(event.files[0].operation, operation);
            } else {
                assert!(event.files.is_empty());
                assert_eq!(event.kind, "tool_call");
                assert_eq!(event.target, None);
            }
        }
        let record = serde_json::json!({
            "type": "context.append_loop_event", "event": {
                "type": "tool.call", "name": "Bash", "args": {
                    "command": "git restore -- src/lib.rs", "cwd": "/other/repo"
                }
            }
        });
        let session =
            parse_kimi_session(fixture_state(), &record.to_string(), "session_abc").unwrap();
        assert_eq!(session.events[0].files[0].cwd.as_deref(), Some("/other/repo"));
        for (flag, status) in [
            (Value::Bool(true), Some("error")),
            (Value::Bool(false), Some("success")),
            (Value::Null, None),
        ] {
            let output = "result".repeat(1000);
            let record = serde_json::json!({
                "type": "context.append_loop_event", "event": {
                    "type": "tool.result", "parentUuid": "missing-call", "toolCallId": "orphan",
                    "result": {"output": output, "isError": flag}
                }
            })
            .to_string();
            let session = parse_kimi_session(fixture_state(), &record, "session_abc").unwrap();
            let event = &session.events[0];
            assert_eq!(event.status.as_deref(), status);
            assert!(event.files.is_empty());
            assert!(event.summary.as_ref().unwrap().len() < output.len());
            let attrs: Value = serde_json::from_str(event.attrs_json.as_deref().unwrap()).unwrap();
            assert_eq!(attrs.pointer("/event/result/output"), Some(&Value::from(output)));
        }
    }

    #[test]
    fn parse_kimi_session_invalid_state_returns_none() {
        for state in ["{", "[]"] {
            assert!(parse_kimi_session(state, fixture_wire(), "session_abc").is_none());
        }
    }

    #[test]
    fn parse_kimi_session_records_empty_usage_parse() {
        let wire = r#"{"type":"context.append_message","time":1700000001000,"message":{"role":"user","origin":{"kind":"user"},"content":[{"type":"text","text":"hello"}]}}"#;
        let session = parse_kimi_session(fixture_state(), wire, "session_abc").unwrap();

        assert!(session.usage_events.is_empty());
        assert_eq!(session.usage_parser_version, Some(USAGE_PARSER_VERSION));
    }

    #[test]
    fn snapshot_changes_withhold_incremental_candidate_until_stable_retry() {
        for mutation in
            [SnapshotMutation::State, SnapshotMutation::AppendWire, SnapshotMutation::ReplaceState]
        {
            let root = tempfile::tempdir().unwrap();
            write_kimi_fixture(root.path());
            crate::db::schema::register_sqlite_vec();
            let store = Store::open_in_memory().unwrap();
            let entry = collect_session_entries(root.path()).pop().unwrap();
            let initial_mtime = kimi_session_snapshot(&entry).unwrap().effective_mtime_ms();
            store
                .conn
                .execute(
                    "INSERT INTO sessions (id, source, source_id, title, started_at, updated_at, message_count) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    rusqlite::params![
                        "stored-id",
                        "kimi-code",
                        "session_abc",
                        "prior indexed title",
                        1_700_000_000_000_i64,
                        initial_mtime - 1,
                        1,
                    ],
                )
                .unwrap();

            let result = run_kimi_snapshot_scan(
                &AdapterSyncContext::from_store_for_test(&store, "kimi-code").unwrap(),
                root.path(),
                Some(mutation),
            );
            assert!(result.sessions.is_empty(), "{mutation:?}");
            assert_eq!(result.stats.unstable_sessions, 1, "{mutation:?}");
            let indexed = store.list_recent_sessions(10).unwrap();
            assert_eq!(indexed.len(), 1, "{mutation:?}");
            assert_eq!(indexed[0].title, "prior indexed title", "{mutation:?}");
            assert_eq!(indexed[0].updated_at, Some(initial_mtime - 1), "{mutation:?}");

            let retry = run_kimi_snapshot_scan(
                &AdapterSyncContext::from_store_for_test(&store, "kimi-code").unwrap(),
                root.path(),
                None,
            );
            assert_eq!(retry.sessions.len(), 1, "{mutation:?}");
            assert_eq!(retry.stats.unstable_sessions, 0, "{mutation:?}");
        }
    }

    #[test]
    fn incremental_snapshot_preserves_usage_backfill_and_stable_skip() {
        let root = tempfile::tempdir().unwrap();
        write_kimi_fixture(root.path());
        let entry = collect_session_entries(root.path()).pop().unwrap();
        let mtime_ms = kimi_session_snapshot(&entry).unwrap().effective_mtime_ms();
        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        store
            .conn
            .execute(
                "INSERT INTO sessions (id, source, source_id, title, started_at, updated_at, message_count) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    "stored-id",
                    "kimi-code",
                    "session_abc",
                    "existing",
                    1_700_000_000_000_i64,
                    mtime_ms,
                    1,
                ],
            )
            .unwrap();
        store
            .persist_usage_events_for_existing_session(
                "kimi-code",
                "session_abc",
                &[],
                USAGE_PARSER_VERSION - 1,
                Some(mtime_ms),
            )
            .unwrap();

        let backfill = file_scan::run_file_scan_with_options_and_snapshot(
            &AdapterSyncContext::from_store_for_test(&store, "kimi-code").unwrap(),
            None,
            kimi_scan_options(false),
            collect_session_entries(root.path()),
            kimi_session_snapshot,
            |entry, mtime_ms| parse_kimi_session_file(entry, mtime_ms, false),
        )
        .unwrap();
        assert_eq!(backfill.sessions.len(), 1);
        assert_eq!(backfill.stats.skipped_sessions, 0);
        assert!(backfill.sessions[0].events.is_empty());
        assert_eq!(backfill.sessions[0].event_parser_version, None);

        store
            .persist_usage_events_for_existing_session(
                "kimi-code",
                "session_abc",
                &[],
                USAGE_PARSER_VERSION,
                Some(mtime_ms),
            )
            .unwrap();
        let usage_only = scan_kimi_dirs_for_sync(
            &[root.path().to_path_buf()],
            &AdapterSyncContext::from_store_for_test(&store, "kimi-code").unwrap(),
            None,
            false,
        )
        .unwrap();
        assert_eq!(usage_only.stats.skipped_sessions, 1);
        for previous_version in [None, Some(EVENT_PARSER_VERSION - 1)] {
            if let Some(version) = previous_version {
                store
                    .persist_session_events_for_existing_session(
                        "kimi-code",
                        "session_abc",
                        &[],
                        version,
                        Some(mtime_ms),
                    )
                    .unwrap();
            }
            let events = scan_kimi_dirs_for_sync(
                &[root.path().to_path_buf()],
                &AdapterSyncContext::from_store_for_test(&store, "kimi-code").unwrap(),
                None,
                true,
            )
            .unwrap();
            assert_eq!(events.sessions.len(), 1);
            assert_eq!(events.sessions[0].events.len(), 2);
        }
        store
            .persist_session_events_for_existing_session(
                "kimi-code",
                "session_abc",
                &[],
                EVENT_PARSER_VERSION,
                Some(mtime_ms),
            )
            .unwrap();
        let skipped = file_scan::run_file_scan_with_options_and_snapshot(
            &AdapterSyncContext::from_store_for_test(&store, "kimi-code").unwrap(),
            None,
            kimi_scan_options(true),
            collect_session_entries(root.path()),
            kimi_session_snapshot,
            |_, _| panic!("stable current session must skip parsing"),
        )
        .unwrap();
        assert!(skipped.sessions.is_empty());
        assert_eq!(skipped.stats.skipped_sessions, 1);
    }

    #[test]
    fn full_scan_withholds_candidate_changed_during_parse() {
        let root = tempfile::tempdir().unwrap();
        write_kimi_fixture(root.path());

        let sessions = scan_kimi_session_entries_with_parser(
            collect_session_entries(root.path()),
            |entry, mtime_ms| {
                let raw = parse_kimi_session_file(entry.clone(), mtime_ms, true)?;
                append_wire_change(&entry.stat_target)?;
                Ok(raw)
            },
        )
        .unwrap();

        assert!(sessions.is_empty());
        assert_eq!(
            scan_kimi_session_entries_with_parser(
                collect_session_entries(root.path()),
                |entry, mtime_ms| parse_kimi_session_file(entry, mtime_ms, true),
            )
            .unwrap()
            .len(),
            1
        );
    }

    #[test]
    fn duplicate_session_ids_prefer_the_first_kimi_root() {
        let preferred = tempfile::tempdir().unwrap();
        let fallback = tempfile::tempdir().unwrap();
        write_kimi_fixture(preferred.path());
        let (_, fallback_wire) = write_kimi_fixture(fallback.path());
        fs::write(&fallback_wire, fixture_wire().replace("fix the bug", "fallback")).unwrap();
        let roots = [preferred.path().to_path_buf(), fallback.path().to_path_buf()];

        let sessions = scan_kimi_dirs(&roots).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].messages[0].content, "fix the bug");

        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let result = scan_kimi_dirs_for_sync(
            &roots,
            &AdapterSyncContext::from_store_for_test(&store, "kimi-code").unwrap(),
            None,
            true,
        )
        .unwrap();
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].messages[0].content, "fix the bug");
    }

    #[test]
    fn incremental_scan_reparses_when_only_state_changes() {
        let root = tempfile::tempdir().unwrap();
        let session_dir = root.path().join("wd_repo").join("session_rename");
        let wire_dir = session_dir.join("agents").join("main");
        fs::create_dir_all(&wire_dir).unwrap();
        let state_path = session_dir.join("state.json");
        let wire_path = wire_dir.join("wire.jsonl");
        fs::write(
            &state_path,
            r#"{"id":"session_rename","cwd":"/repo","createdAt":1700000000000,"title":"renamed","isCustomTitle":true}"#,
        )
        .unwrap();
        fs::write(
            &wire_path,
            r#"{"type":"context.append_message","time":1700000001000,"message":{"role":"user","origin":{"kind":"user"},"content":[{"type":"text","text":"hello"}]}}"#,
        )
        .unwrap();

        let wire_mtime = 1_700_000_002_000;
        let state_mtime = wire_mtime + 1_000;
        fs::File::open(&wire_path)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_millis(wire_mtime as u64))
            .unwrap();
        fs::File::open(&state_path)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_millis(state_mtime as u64))
            .unwrap();

        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        store
            .conn
            .execute(
                "INSERT INTO sessions (id, source, source_id, title, started_at, updated_at, message_count) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    "stored-id",
                    "kimi-code",
                    "session_rename",
                    "old title",
                    1_700_000_000_000_i64,
                    wire_mtime,
                    1,
                ],
            )
            .unwrap();
        store
            .persist_usage_events_for_existing_session(
                "kimi-code",
                "session_rename",
                &[],
                USAGE_PARSER_VERSION,
                Some(wire_mtime),
            )
            .unwrap();

        let result = file_scan::run_file_scan_with_options_and_snapshot(
            &AdapterSyncContext::from_store_for_test(&store, "kimi-code").unwrap(),
            None,
            kimi_scan_options(true),
            collect_session_entries(root.path()),
            kimi_session_snapshot,
            |entry, mtime_ms| parse_kimi_session_file(entry, mtime_ms, true),
        )
        .unwrap();

        assert_eq!(result.stats.parsed, 1);
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].updated_at, Some(state_mtime));
        assert_eq!(result.sessions[0].custom_title.as_deref(), Some("renamed"));

        let sessions = scan_kimi_session_entries_with_parser(
            collect_session_entries(root.path()),
            |entry, mtime_ms| parse_kimi_session_file(entry, mtime_ms, true),
        )
        .unwrap();
        assert_eq!(sessions[0].updated_at, Some(state_mtime));
    }
}
