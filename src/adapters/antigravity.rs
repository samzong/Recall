use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use tracing::{debug, warn};
use walkdir::WalkDir;

use crate::adapters::AdapterSyncContext;
use crate::adapters::events::{
    EventContext, shell_file_evidence, tool_call_event, tool_result_event,
};
use crate::adapters::file_scan::{self, FileScanEntry};
use crate::adapters::paths::{file_uri_to_path, resolve_home_dir};
use crate::adapters::{RawMessage, RawSession, ResumeCommand, SourceAdapter, SyncScanResult};
use crate::db::store::IndexedSessionMeta;
use crate::types::{FileEvidence, FileOperation, ParentLink, ParentRelation, Role, ThreadRole};

const EVENT_PARSER_VERSION: u32 = 2;
const METADATA_PARSER_VERSION: u32 = 1;

const TRANSCRIPT_RELATIVE_PATH: &[&str] = &[".system_generated", "logs", "transcript.jsonl"];
const CLI_DIR: &str = ".gemini/antigravity-cli";
const ACP_DIR: &str = ".gemini/antigravity-acp";
const DESKTOP_DIR: &str = ".gemini/antigravity";

#[derive(Clone, Copy)]
pub(crate) enum AntigravityAdapter {
    Cli,
    Desktop,
}

impl SourceAdapter for AntigravityAdapter {
    fn id(&self) -> &str {
        match self {
            Self::Cli => "antigravity-cli",
            Self::Desktop => "antigravity",
        }
    }

    fn label(&self) -> &str {
        match self {
            Self::Cli => "AGY",
            Self::Desktop => "AGYD",
        }
    }

    fn usage_parser_version(&self) -> Option<u32> {
        Some(usage::USAGE_PARSER_VERSION)
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        match self {
            Self::Cli => {
                if is_acp_only_conversation(source_id) {
                    None
                } else {
                    Some(ResumeCommand::new("agy", &["--conversation", source_id]))
                }
            }
            Self::Desktop => None,
        }
    }

    fn start_command(&self, prompt: String) -> Option<ResumeCommand> {
        match self {
            Self::Cli => Some(ResumeCommand {
                program: "agy".to_string(),
                args: vec!["-i".to_string(), prompt],
            }),
            Self::Desktop => None,
        }
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let mut sessions = Vec::new();
        for root_dir in resolve_antigravity_dirs(*self)? {
            let scan_data = collect_antigravity_scan_data(&root_dir)?;
            for entry in scan_data.entries {
                let db_path = scan_data.db_paths.get(&entry.session_id).map(PathBuf::as_path);
                let Some(snapshot) = antigravity_snapshot(&entry, db_path) else {
                    continue;
                };
                if let Some(raw) = parse_antigravity_session_for_entry(
                    entry.clone(),
                    db_path,
                    &scan_data.live_cascade_ids,
                    snapshot.effective_mtime_ms(),
                    true,
                    None,
                    self.id(),
                )? && antigravity_snapshot(&entry, db_path).as_ref() == Some(&snapshot)
                {
                    sessions.push(raw);
                }
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
        let mut combined = SyncScanResult::default();
        for root_dir in resolve_antigravity_dirs(*self)? {
            let part = scan_for_sync_impl(&root_dir, *self, context, since_ts, include_events)?;
            combined.absorb(part);
        }
        Ok(Some(combined))
    }
}

fn is_acp_only_conversation(source_id: &str) -> bool {
    let Ok(Some(acp_dir)) = resolve_home_dir(ACP_DIR, "~/.gemini/antigravity-acp not found") else {
        return false;
    };
    let in_acp = acp_dir.join("conversations").join(format!("{source_id}.db")).is_file()
        || acp_dir.join("brain").join(source_id).is_dir();
    if !in_acp {
        return false;
    }
    let Ok(Some(cli_dir)) = resolve_home_dir(CLI_DIR, "~/.gemini/antigravity-cli not found") else {
        return true;
    };
    let in_cli = cli_dir.join("conversations").join(format!("{source_id}.db")).is_file()
        || cli_dir.join("brain").join(source_id).is_dir();
    !in_cli
}

fn resolve_antigravity_dirs(adapter: AntigravityAdapter) -> anyhow::Result<Vec<PathBuf>> {
    let mut dirs = Vec::new();
    match adapter {
        AntigravityAdapter::Cli => {
            if let Some(cli) =
                resolve_home_dir(CLI_DIR, "~/.gemini/antigravity-cli not found, skipping")?
            {
                dirs.push(cli);
            }
            if let Some(acp) =
                resolve_home_dir(ACP_DIR, "~/.gemini/antigravity-acp not found, skipping")?
            {
                dirs.push(acp);
            }
        }
        AntigravityAdapter::Desktop => {
            if let Some(desktop) =
                resolve_home_dir(DESKTOP_DIR, "~/.gemini/antigravity not found, skipping")?
            {
                dirs.push(desktop);
            }
        }
    }
    Ok(dirs)
}

fn scan_for_sync_impl(
    root_dir: &Path,
    adapter: AntigravityAdapter,
    context: &AdapterSyncContext,
    since_ts: Option<i64>,
    include_events: bool,
) -> anyhow::Result<SyncScanResult> {
    let scan_data = collect_antigravity_scan_data(root_dir)?;
    let db_paths = &scan_data.db_paths;
    let live_cascade_ids = &scan_data.live_cascade_ids;
    file_scan::run_file_scan_with_options_and_snapshot(
        context,
        since_ts,
        file_scan::FileScanOptions {
            stream_sessions: true,
            usage_parser_version: Some(usage::USAGE_PARSER_VERSION),
            event_parser_version: include_events.then_some(EVENT_PARSER_VERSION),
            metadata_parser_version: include_events.then_some(METADATA_PARSER_VERSION),
        },
        scan_data.entries,
        |entry| {
            let db_path = db_paths.get(&entry.session_id).map(PathBuf::as_path);
            antigravity_snapshot(entry, db_path).map(|snapshot| {
                let directory_changed = entry.directory.as_ref().is_some_and(|directory| {
                    context
                        .session_meta()
                        .get(&entry.session_id)
                        .is_some_and(|old| old.directory.as_ref() != Some(directory))
                });
                snapshot.requiring_parse(directory_changed)
            })
        },
        |entry, mtime| {
            let db_path = db_paths.get(&entry.session_id).map(PathBuf::as_path);
            let shell_meta = (context.is_usage_only() && !include_events)
                .then(|| context.session_meta().get(&entry.session_id))
                .flatten();
            parse_antigravity_session_for_entry(
                entry,
                db_path,
                live_cascade_ids,
                mtime,
                include_events,
                shell_meta,
                adapter.id(),
            )
        },
    )
}

fn wal_path(db_path: &Path) -> PathBuf {
    let mut os = db_path.as_os_str().to_os_string();
    os.push("-wal");
    PathBuf::from(os)
}

fn antigravity_snapshot(
    entry: &FileScanEntry,
    db_path: Option<&Path>,
) -> Option<file_scan::FileScanSnapshot<[Option<file_scan::FileMetadataSnapshot>; 3]>> {
    let transcript = file_scan::file_metadata_snapshot(&entry.stat_target)?;
    let db = db_path.and_then(file_scan::file_metadata_snapshot);
    let wal = db_path
        .and_then(|path| file_scan::file_metadata_snapshot(&wal_path(path)))
        .filter(|meta| meta.len() > 0);
    let effective_mtime = [Some(&transcript), db.as_ref(), wal.as_ref()]
        .into_iter()
        .flatten()
        .filter_map(file_scan::FileMetadataSnapshot::mtime_ms)
        .max()?;
    Some(file_scan::FileScanSnapshot::new(effective_mtime, [Some(transcript), db, wal]))
}

struct AntigravityScanData {
    entries: Vec<FileScanEntry>,
    db_paths: HashMap<String, PathBuf>,
    live_cascade_ids: HashSet<String>,
}

#[cfg(test)]
fn collect_antigravity_entries(
    cli_dir: &Path,
    _adapter: AntigravityAdapter,
) -> anyhow::Result<Vec<FileScanEntry>> {
    Ok(collect_antigravity_scan_data(cli_dir)?.entries)
}

fn collect_antigravity_scan_data(root_dir: &Path) -> anyhow::Result<AntigravityScanData> {
    let brain_dir = root_dir.join("brain");
    if !brain_dir.exists() {
        return Ok(AntigravityScanData {
            entries: Vec::new(),
            db_paths: HashMap::new(),
            live_cascade_ids: HashSet::new(),
        });
    }

    let mut workspace_by_conversation = HashMap::new();
    let summaries_path = root_dir.join("conversation_summaries.db");
    match load_summaries_workspace_map(&summaries_path) {
        Ok(map) => workspace_by_conversation.extend(map),
        Err(err) => {
            warn!("failed to read Antigravity workspaces {}: {err}", summaries_path.display());
        }
    }
    workspace_by_conversation.extend(load_history_workspace_map(&root_dir.join("history.jsonl"))?);

    let conversations_dir = root_dir.join("conversations");
    let mut db_paths = HashMap::new();
    let mut live_cascade_ids = HashSet::new();
    if conversations_dir.is_dir() {
        for walk_entry in WalkDir::new(&conversations_dir)
            .min_depth(1)
            .max_depth(1)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = walk_entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if uuid::Uuid::try_parse(stem).is_err() {
                continue;
            }
            match path.extension().and_then(|e| e.to_str()) {
                Some("db") => {
                    if fs::metadata(path).is_ok_and(|m| m.len() > 0) {
                        live_cascade_ids.insert(stem.to_string());
                    }
                    db_paths.insert(stem.to_string(), path.to_path_buf());
                }
                Some("meta") => {
                    if let Some(cwd) = read_meta_cwd(path) {
                        workspace_by_conversation.entry(stem.to_string()).or_insert(cwd);
                    }
                }
                _ => {}
            }
        }
    }

    let mut entries = Vec::new();
    for walk_entry in
        WalkDir::new(&brain_dir).min_depth(1).max_depth(1).into_iter().filter_map(|e| e.ok())
    {
        let path = walk_entry.path();
        if !path.is_dir() {
            continue;
        }

        let Some(conversation_id) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if uuid::Uuid::try_parse(conversation_id).is_err() {
            continue;
        }

        let transcript_path =
            TRANSCRIPT_RELATIVE_PATH.iter().fold(path.to_path_buf(), |acc, part| acc.join(part));
        if !transcript_path.is_file() {
            continue;
        }

        entries.push(FileScanEntry {
            session_id: conversation_id.to_string(),
            stat_target: transcript_path,
            directory: workspace_by_conversation.get(conversation_id).cloned(),
        });
    }

    Ok(AntigravityScanData { entries, db_paths, live_cascade_ids })
}

fn read_meta_cwd(path: &Path) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&content).ok()?;
    let cwd = value.get("cwd").and_then(Value::as_str)?.trim();
    (!cwd.is_empty()).then(|| cwd.to_string())
}

fn load_summaries_workspace_map(path: &Path) -> anyhow::Result<HashMap<String, String>> {
    if !path.is_file() {
        return Ok(HashMap::new());
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt =
        conn.prepare("SELECT conversation_id, workspace_uris FROM conversation_summaries")?;
    let rows =
        stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
    let mut map = HashMap::new();
    for row in rows {
        let (id, uris) = row?;
        let Ok(uris) = serde_json::from_str::<Vec<String>>(&uris) else {
            continue;
        };
        if let Some(path) = uris.iter().find_map(|uri| file_uri_to_path(uri)) {
            map.insert(id, path);
        }
    }
    Ok(map)
}

fn load_history_workspace_map(path: &Path) -> anyhow::Result<HashMap<String, String>> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(err) => return Err(err.into()),
    };

    let reader = BufReader::new(file);
    let mut map = HashMap::new();
    for line in reader.lines() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(conversation_id) = v.get("conversationId").and_then(|id| id.as_str()) else {
            continue;
        };
        let Some(workspace) = v.get("workspace").and_then(|workspace| workspace.as_str()) else {
            continue;
        };
        if !workspace.is_empty() {
            map.insert(conversation_id.to_string(), workspace.to_string());
        }
    }
    Ok(map)
}

fn parse_antigravity_session_for_entry(
    entry: FileScanEntry,
    db_path: Option<&Path>,
    live_cascade_ids: &HashSet<String>,
    mtime_ms: i64,
    include_events: bool,
    shell_meta: Option<&IndexedSessionMeta>,
    adapter_id: &str,
) -> anyhow::Result<Option<RawSession>> {
    let source_file_path = entry.stat_target.to_str().map(str::to_string);

    if let Some(meta) = shell_meta {
        let (usage_events, db_workspace) = match db_path {
            Some(path) => match usage::parse_antigravity_db(
                path,
                &entry.session_id,
                live_cascade_ids,
                Some(meta.started_at.max(mtime_ms)),
            ) {
                Ok(parsed) => (parsed.usage_events, parsed.workspace_dir),
                Err(err) => {
                    debug!("failed to parse Antigravity usage db {}: {err}", path.display());
                    return Ok(None);
                }
            },
            None => (Vec::new(), None),
        };
        let mut raw = RawSession::search_only(
            entry.session_id,
            entry.directory.or(db_workspace),
            meta.started_at,
            Some(mtime_ms),
            None,
            Vec::new(),
        );
        raw.source_file_path = source_file_path;
        return Ok(Some(raw.with_usage(usage_events, usage::USAGE_PARSER_VERSION)));
    }

    let parsed_db = match db_path {
        Some(path) => {
            match usage::parse_antigravity_db(path, &entry.session_id, live_cascade_ids, None) {
                Ok(parsed) => Some(parsed),
                Err(err) => {
                    debug!("failed to parse Antigravity usage db {}: {err}", path.display());
                    return Ok(None);
                }
            }
        }
        None => None,
    };

    let (mut usage_events, db_created_at, db_workspace) = match parsed_db {
        Some(parsed) => (parsed.usage_events, parsed.created_at_ms, parsed.workspace_dir),
        None => (Vec::new(), None, None),
    };
    let effective_directory = entry.directory.or(db_workspace);

    let raw_transcript = match parse_antigravity_transcript(
        &entry.stat_target,
        &entry.session_id,
        effective_directory.as_deref(),
        include_events,
        adapter_id,
    ) {
        Ok(raw) => raw,
        Err(e) => {
            debug!("failed to parse Antigravity transcript {}: {e}", entry.stat_target.display());
            return Ok(None);
        }
    };

    let fallback_ts = raw_transcript
        .as_ref()
        .map(|raw| raw.started_at)
        .filter(|&ts| ts > 0)
        .or(db_created_at)
        .unwrap_or(mtime_ms);
    for event in &mut usage_events {
        if event.timestamp == 0 {
            event.timestamp = fallback_ts;
        }
    }

    let mut raw = match raw_transcript {
        Some(raw) => raw,
        None if !usage_events.is_empty() => RawSession::search_only(
            entry.session_id,
            None,
            db_created_at
                .or_else(|| usage_events.first().map(|event| event.timestamp))
                .unwrap_or(mtime_ms),
            Some(mtime_ms),
            None,
            Vec::new(),
        ),
        None => return Ok(None),
    };

    raw.directory = effective_directory;
    raw.updated_at = Some(mtime_ms);
    raw.source_file_path = source_file_path;
    Ok(Some(raw.with_usage(usage_events, usage::USAGE_PARSER_VERSION)))
}

fn parse_antigravity_transcript(
    path: &Path,
    fallback_id: &str,
    cwd: Option<&str>,
    include_events: bool,
    adapter_id: &str,
) -> anyhow::Result<Option<RawSession>> {
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    parse_antigravity_transcript_reader(
        reader,
        fallback_id,
        path.to_str(),
        cwd,
        include_events,
        adapter_id,
    )
}

fn parse_antigravity_transcript_reader<R: BufRead>(
    reader: R,
    fallback_id: &str,
    source_path: Option<&str>,
    cwd: Option<&str>,
    include_events: bool,
    adapter_id: &str,
) -> anyhow::Result<Option<RawSession>> {
    let mut messages = Vec::new();
    let mut events = Vec::new();
    let mut subagent_parent_id: Option<String> = None;
    let mut saw_user_input = false;

    for (line_index, line) in reader.lines().enumerate() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let source = v.get("source").and_then(|source| source.as_str()).unwrap_or("");
        let event_type = v.get("type").and_then(|event_type| event_type.as_str()).unwrap_or("");
        let timestamp = parse_created_at(&v);

        if include_events {
            let context = |event_seq, part_index| EventContext {
                event_seq,
                timestamp,
                source_path: source_path.map(str::to_string),
                source_event_id: Some(format!("line:{line_index}:part:{part_index}")),
                message_seq: messages
                    .len()
                    .checked_sub(1)
                    .and_then(|index| u32::try_from(index).ok()),
                parser_version: EVENT_PARSER_VERSION,
            };
            if source == "MODEL" && event_type == "PLANNER_RESPONSE" {
                for (index, call) in
                    v.get("tool_calls").and_then(Value::as_array).into_iter().flatten().enumerate()
                {
                    let Some(name) = call
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|name| !name.trim().is_empty())
                    else {
                        continue;
                    };
                    let args = call.get("args");
                    let mut event = tool_call_event(
                        context(events.len() as u32, index),
                        name.to_string(),
                        args,
                    );
                    event.kind = "tool_call".to_string();
                    event.target = None;
                    let file = match name {
                        "replace_file_content" => Some(("TargetFile", FileOperation::Write)),
                        "view_file" => Some(("AbsolutePath", FileOperation::Read)),
                        _ => None,
                    };
                    if let Some((field, operation)) = file {
                        if let Some(path) = args
                            .and_then(|args| args.get(field))
                            .and_then(Value::as_str)
                            .filter(|path| !path.trim().is_empty())
                        {
                            event.kind = if operation == FileOperation::Read {
                                "file_read"
                            } else {
                                "file_write"
                            }
                            .to_string();
                            event.target = Some(path.to_string());
                            event.files.push(FileEvidence::call(
                                path.to_string(),
                                operation,
                                cwd.map(str::to_string),
                            ));
                        }
                    } else if name == "run_command" {
                        event.kind = "command".to_string();
                        event.target = args
                            .and_then(|args| args.get("CommandLine"))
                            .and_then(Value::as_str)
                            .filter(|command| !command.trim().is_empty())
                            .map(str::to_string);
                        if let Some(command) = event.target.as_deref() {
                            let command_cwd = args
                                .and_then(|args| args.get("Cwd"))
                                .and_then(Value::as_str)
                                .filter(|cwd| !cwd.trim().is_empty());
                            let (files, status) = shell_file_evidence(command, command_cwd);
                            event.files = files;
                            event.command_evidence_status = Some(status);
                        }
                    }
                    event.attrs_json = Some(v.to_string());
                    events.push(event);
                }
            } else if source == "MODEL" && event_type == "GENERIC" {
                let mut event = tool_result_event(
                    context(events.len() as u32, 0),
                    Some(event_type.to_string()),
                    v.get("content").and_then(Value::as_str).map(str::to_string),
                );
                event.kind = "native_record".to_string();
                event.actor = "assistant".to_string();
                event.attrs_json = Some(v.to_string());
                events.push(event);
            }
        }
        if event_type == "USER_INPUT" {
            saw_user_input = true;
        }
        let system_message = if source == "SYSTEM" && event_type == "SYSTEM_MESSAGE" {
            v.get("content").and_then(Value::as_str).and_then(parse_antigravity_system_message)
        } else {
            None
        };
        if let Some((sender, _)) = system_message
            && subagent_parent_id.is_none()
            && !saw_user_input
            && sender != fallback_id
            && uuid::Uuid::try_parse(sender).is_ok()
        {
            subagent_parent_id = Some(sender.to_string());
        }
        if v.get("status").and_then(Value::as_str) != Some("DONE") {
            continue;
        }
        match (source, event_type) {
            (_, "USER_INPUT") => {
                let content =
                    v.get("content").and_then(|content| content.as_str()).unwrap_or("").trim();
                let content = extract_user_request(content);
                if !content.is_empty() {
                    messages.push(RawMessage { role: Role::User, content, timestamp });
                }
            }
            ("SYSTEM", "SYSTEM_MESSAGE") => {
                if let Some((sender, body)) = system_message
                    && subagent_parent_id.as_deref() == Some(sender)
                {
                    let content = extract_antigravity_system_body(body);
                    if !content.is_empty() {
                        messages.push(RawMessage { role: Role::User, content, timestamp });
                    }
                }
            }
            ("MODEL", "PLANNER_RESPONSE") => {
                let content =
                    v.get("content").and_then(|content| content.as_str()).unwrap_or("").trim();
                if !content.is_empty() {
                    messages.push(RawMessage {
                        role: Role::Assistant,
                        content: content.to_string(),
                        timestamp,
                    });
                }
                if let Some(parent_id) = subagent_parent_id.as_deref() {
                    collect_send_message_reports(&v, parent_id, content, timestamp, &mut messages);
                }
            }
            _ => {}
        }
    }

    if messages.is_empty() && events.is_empty() {
        return Ok(None);
    }

    let started_at = crate::adapters::first_timestamp(None, &messages, &[], &events).unwrap_or(0);
    let (thread_role, parent_links) = match subagent_parent_id {
        Some(parent_id) => (
            Some(ThreadRole::Subagent),
            vec![ParentLink {
                relation: ParentRelation::Spawn,
                source: adapter_id.to_string(),
                source_id: parent_id,
            }],
        ),
        None => (Some(ThreadRole::Primary), Vec::new()),
    };

    let mut session = RawSession::search_only(
        fallback_id.to_string(),
        None,
        started_at,
        messages.last().and_then(|message| message.timestamp),
        None,
        messages,
    );
    session.thread_role = thread_role;
    session.parent_links = parent_links;
    session.metadata_parser_version = Some(METADATA_PARSER_VERSION);
    session.refresh_session_on_metadata_backfill = true;
    if include_events {
        session = session.with_events(events, EVENT_PARSER_VERSION);
    }
    Ok(Some(session))
}

fn extract_user_request(content: &str) -> String {
    let Some(start) = content.find("<USER_REQUEST>") else {
        return content.trim().to_string();
    };
    let request_start = start + "<USER_REQUEST>".len();
    let Some(end) = content[request_start..].find("</USER_REQUEST>") else {
        return content.trim().to_string();
    };
    content[request_start..request_start + end].trim().to_string()
}

fn parse_created_at(v: &Value) -> Option<i64> {
    v.get("created_at")
        .and_then(|timestamp| timestamp.as_str())
        .and_then(|timestamp| chrono::DateTime::parse_from_rfc3339(timestamp).ok())
        .map(|dt| dt.timestamp_millis())
}

mod subagent;
#[cfg(test)]
mod tests;
mod usage;

use subagent::{
    collect_send_message_reports, extract_antigravity_system_body, parse_antigravity_system_message,
};
