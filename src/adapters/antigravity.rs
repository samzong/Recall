use std::collections::HashMap;
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
use crate::types::{FileEvidence, FileOperation, Role};

const EVENT_PARSER_VERSION: u32 = 1;

const TRANSCRIPT_RELATIVE_PATH: &[&str] = &[".system_generated", "logs", "transcript.jsonl"];

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

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        match self {
            Self::Cli => Some(ResumeCommand::new("agy", &["--conversation", source_id])),
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
        let Some(cli_dir) = resolve_antigravity_dir(*self)? else {
            return Ok(vec![]);
        };

        let mut sessions = Vec::new();
        for entry in collect_antigravity_entries(&cli_dir, *self)? {
            let Some(snapshot) = antigravity_snapshot(&entry) else {
                continue;
            };
            if let Some(raw) = parse_antigravity_session_for_entry(
                entry.clone(),
                snapshot.effective_mtime_ms(),
                true,
            )? && antigravity_snapshot(&entry).as_ref() == Some(&snapshot)
            {
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
        let Some(cli_dir) = resolve_antigravity_dir(*self)? else {
            return Ok(Some(SyncScanResult::default()));
        };
        Ok(Some(scan_for_sync_impl(&cli_dir, *self, context, since_ts, include_events)?))
    }
}

fn resolve_antigravity_dir(adapter: AntigravityAdapter) -> anyhow::Result<Option<PathBuf>> {
    match adapter {
        AntigravityAdapter::Cli => resolve_home_dir(
            ".gemini/antigravity-cli",
            "~/.gemini/antigravity-cli not found, skipping Antigravity CLI",
        ),
        AntigravityAdapter::Desktop => resolve_home_dir(
            ".gemini/antigravity",
            "~/.gemini/antigravity not found, skipping Antigravity desktop",
        ),
    }
}

fn scan_for_sync_impl(
    cli_dir: &Path,
    adapter: AntigravityAdapter,
    context: &AdapterSyncContext,
    since_ts: Option<i64>,
    include_events: bool,
) -> anyhow::Result<SyncScanResult> {
    let entries = collect_antigravity_entries(cli_dir, adapter)?;
    file_scan::run_file_scan_with_options_and_snapshot(
        context,
        since_ts,
        file_scan::FileScanOptions {
            stream_sessions: true,
            event_parser_version: include_events.then_some(EVENT_PARSER_VERSION),
            ..Default::default()
        },
        entries,
        |entry| {
            antigravity_snapshot(entry).map(|snapshot| {
                let directory_changed = matches!(adapter, AntigravityAdapter::Desktop)
                    && entry.directory.as_ref().is_some_and(|directory| {
                        context
                            .session_meta()
                            .get(&entry.session_id)
                            .is_some_and(|old| old.directory.as_ref() != Some(directory))
                    });
                snapshot.requiring_parse(directory_changed)
            })
        },
        |entry, mtime| parse_antigravity_session_for_entry(entry, mtime, include_events),
    )
}

fn antigravity_snapshot(
    entry: &FileScanEntry,
) -> Option<file_scan::FileScanSnapshot<file_scan::FileMetadataSnapshot>> {
    let fingerprint = file_scan::file_metadata_snapshot(&entry.stat_target)?;
    Some(file_scan::FileScanSnapshot::new(fingerprint.mtime_ms()?, fingerprint))
}

fn collect_antigravity_entries(
    cli_dir: &Path,
    adapter: AntigravityAdapter,
) -> anyhow::Result<Vec<FileScanEntry>> {
    let brain_dir = cli_dir.join("brain");
    if !brain_dir.exists() {
        return Ok(vec![]);
    }

    let workspace_by_conversation = match adapter {
        AntigravityAdapter::Cli => load_history_workspace_map(&cli_dir.join("history.jsonl"))?,
        AntigravityAdapter::Desktop => {
            let path = cli_dir.join("conversation_summaries.db");
            load_desktop_workspace_map(&path).unwrap_or_else(|err| {
                warn!("failed to read Antigravity workspaces {}: {err}", path.display());
                HashMap::new()
            })
        }
    };
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

    Ok(entries)
}

fn load_desktop_workspace_map(path: &Path) -> anyhow::Result<HashMap<String, String>> {
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
    mtime_ms: i64,
    include_events: bool,
) -> anyhow::Result<Option<RawSession>> {
    let source_file_path = entry.stat_target.to_str().map(str::to_string);
    let mut raw = match parse_antigravity_transcript(
        &entry.stat_target,
        &entry.session_id,
        entry.directory.as_deref(),
        include_events,
    ) {
        Ok(Some(raw)) => raw,
        Ok(None) => return Ok(None),
        Err(e) => {
            debug!("failed to parse Antigravity transcript {}: {e}", entry.stat_target.display());
            return Ok(None);
        }
    };
    raw.directory = entry.directory;
    raw.updated_at = Some(mtime_ms);
    raw.source_file_path = source_file_path;
    Ok(Some(raw))
}

fn parse_antigravity_transcript(
    path: &Path,
    fallback_id: &str,
    cwd: Option<&str>,
    include_events: bool,
) -> anyhow::Result<Option<RawSession>> {
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    parse_antigravity_transcript_reader(reader, fallback_id, path.to_str(), cwd, include_events)
}

fn parse_antigravity_transcript_reader<R: BufRead>(
    reader: R,
    fallback_id: &str,
    source_path: Option<&str>,
    cwd: Option<&str>,
    include_events: bool,
) -> anyhow::Result<Option<RawSession>> {
    let mut messages = Vec::new();
    let mut events = Vec::new();

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
            }
            _ => {}
        }
    }

    if messages.is_empty() && events.is_empty() {
        return Ok(None);
    }

    let started_at = crate::adapters::first_timestamp(None, &messages, &[], &events).unwrap_or(0);

    let mut session = RawSession::search_only(
        fallback_id.to_string(),
        None,
        started_at,
        messages.last().and_then(|message| message.timestamp),
        None,
        messages,
    );
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

#[cfg(test)]
mod tests;
