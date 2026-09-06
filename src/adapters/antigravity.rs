use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;
use tracing::debug;
use walkdir::WalkDir;

use crate::adapters::AdapterSyncContext;
use crate::adapters::events::{
    EventContext, shell_file_evidence, tool_call_event, tool_result_event,
};
use crate::adapters::file_scan::{self, FileScanEntry};
use crate::adapters::paths::resolve_home_dir;
use crate::adapters::{
    RawMessage, RawSession, ResumeCommand, SourceAdapter, SyncScanResult, SyncScanStats,
};
use crate::types::{FileEvidence, FileEvidenceKind, FileOperation, Role};

const EVENT_PARSER_VERSION: u32 = 1;

const TRANSCRIPT_RELATIVE_PATH: &[&str] = &[".system_generated", "logs", "transcript.jsonl"];

pub(crate) struct AntigravityAdapter;

impl SourceAdapter for AntigravityAdapter {
    fn id(&self) -> &str {
        "antigravity-cli"
    }

    fn label(&self) -> &str {
        "AGY"
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        Some(ResumeCommand {
            program: "agy".to_string(),
            args: vec!["--conversation".to_string(), source_id.to_string()],
        })
    }

    fn start_command(&self, prompt: String) -> Option<ResumeCommand> {
        Some(ResumeCommand { program: "agy".to_string(), args: vec!["-i".to_string(), prompt] })
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let Some(cli_dir) = resolve_antigravity_dir()? else {
            return Ok(vec![]);
        };

        let mut sessions = Vec::new();
        for entry in collect_antigravity_entries(&cli_dir)? {
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
        let Some(cli_dir) = resolve_antigravity_dir()? else {
            return Ok(Some(SyncScanResult {
                sessions: vec![],
                stats: SyncScanStats::default(),
                observations: Vec::new(),
            }));
        };
        Ok(Some(scan_for_sync_impl(&cli_dir, context, since_ts, include_events)?))
    }
}

fn resolve_antigravity_dir() -> anyhow::Result<Option<PathBuf>> {
    resolve_home_dir(
        ".gemini/antigravity-cli",
        "~/.gemini/antigravity-cli not found, skipping Antigravity CLI",
    )
}

fn scan_for_sync_impl(
    cli_dir: &Path,
    context: &AdapterSyncContext,
    since_ts: Option<i64>,
    include_events: bool,
) -> anyhow::Result<SyncScanResult> {
    let entries = collect_antigravity_entries(cli_dir)?;
    file_scan::run_file_scan_with_options_and_snapshot(
        context,
        since_ts,
        file_scan::FileScanOptions {
            event_parser_version: include_events.then_some(EVENT_PARSER_VERSION),
            ..Default::default()
        },
        entries,
        antigravity_snapshot,
        |entry, mtime| parse_antigravity_session_for_entry(entry, mtime, include_events),
    )
}

fn antigravity_snapshot(
    entry: &FileScanEntry,
) -> Option<file_scan::FileScanSnapshot<file_scan::FileMetadataSnapshot>> {
    let fingerprint = file_scan::file_metadata_snapshot(&entry.stat_target)?;
    Some(file_scan::FileScanSnapshot::new(fingerprint.mtime_ms()?, fingerprint))
}

fn collect_antigravity_entries(cli_dir: &Path) -> anyhow::Result<Vec<FileScanEntry>> {
    let brain_dir = cli_dir.join("brain");
    if !brain_dir.exists() {
        return Ok(vec![]);
    }

    let workspace_by_conversation = load_history_workspace_map(&cli_dir.join("history.jsonl"))?;
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
                            event.files.push(FileEvidence {
                                path: path.to_string(),
                                operation,
                                kind: FileEvidenceKind::Call,
                                cwd: cwd.map(str::to_string),
                                target: None,
                            });
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
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::db::{schema, store::Store};
    use crate::types::Session;

    fn setup_store() -> Store {
        schema::register_sqlite_vec();
        Store::open_in_memory().unwrap()
    }

    fn temp_antigravity_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "recall-agy-test-{}-{}",
            label,
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_transcript(root: &Path, conversation_id: &str, text: &str) -> PathBuf {
        let transcript = TRANSCRIPT_RELATIVE_PATH
            .iter()
            .fold(root.join("brain").join(conversation_id), |acc, part| acc.join(part));
        fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        fs::write(&transcript, text).unwrap();
        transcript
    }

    fn make_existing_session(source_id: &str, updated_at: i64, message_count: u32) -> Session {
        Session {
            id: format!("internal-{source_id}"),
            source: "antigravity-cli".to_string(),
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
    fn parse_antigravity_transcript_extracts_user_and_assistant_text() {
        let jsonl = r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-05-20T06:03:19Z","content":"<USER_REQUEST>\nAnalyze this project\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nignored\n</ADDITIONAL_METADATA>"}
{"step_index":2,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-05-20T06:03:19Z","tool_calls":[{"name":"replace_file_content","args":{"TargetFile":"/work/project/src/auth.rs","TargetContent":"old","ReplacementContent":"new"}},{"name":"view_file","args":{"AbsolutePath":"/work/project/src/auth.rs"}}]}
{"step_index":3,"source":"MODEL","type":"GENERIC","status":"DONE","created_at":"2026-05-20T06:03:21Z","content":"tool output should not be indexed","truncated_fields":["content"]}
{"step_index":15,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-05-20T06:03:30Z","content":"This project is a local agent configuration hub."}
"#;
        let session = parse_antigravity_transcript_reader(
            Cursor::new(jsonl),
            "agy-session",
            Some("/tmp/transcript.jsonl"),
            Some("/work/project"),
            true,
        )
        .unwrap()
        .unwrap();

        assert_eq!(session.source_id, "agy-session");
        assert_eq!(session.started_at, 1_779_256_999_000);
        assert_eq!(session.updated_at, Some(1_779_257_010_000));
        assert_eq!(session.events.len(), 3);
        assert_eq!(session.events[0].files[0].operation, FileOperation::Write);
        assert_eq!(session.events[0].files[0].cwd.as_deref(), Some("/work/project"));
        assert_eq!(session.events[1].files[0].operation, FileOperation::Read);
        assert_eq!(session.events[0].source_event_id.as_deref(), Some("line:1:part:0"));
        assert_eq!(session.events[0].message_seq, Some(0));
        assert!(
            session
                .events
                .iter()
                .all(|event| event.status.is_none() && event.tool_call_id.is_none())
        );
        assert_eq!(session.events[2].kind, "native_record");
        assert!(session.events[2].files.is_empty());
        assert!(session.events[2].attrs_json.as_deref().unwrap().contains("truncated_fields"));
        let tool_only = jsonl.lines().nth(1).unwrap().replace("DONE", "RUNNING");
        let pending = parse_antigravity_transcript_reader(
            Cursor::new(&tool_only),
            "pending",
            None,
            None,
            true,
        )
        .unwrap()
        .unwrap();
        assert!(pending.messages.is_empty());
        assert_eq!(pending.events.len(), 2);
        assert!(pending.events[0].message_seq.is_none());
        for cwd in [Some("/work/elsewhere"), None] {
            let command = serde_json::json!({"source":"MODEL","type":"PLANNER_RESPONSE","status":"RUNNING","tool_calls":[{"name":"run_command","args":{"CommandLine":"git restore -- src/auth.rs","Cwd":cwd}}]});
            let command = parse_antigravity_transcript_reader(
                Cursor::new(command.to_string()),
                "commands",
                None,
                Some("/not-command-cwd"),
                true,
            )
            .unwrap()
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
            parse_antigravity_transcript_reader(
                Cursor::new([0xff, b'\n']),
                "broken",
                None,
                None,
                true
            )
            .is_err()
        );
        assert!(
            parse_antigravity_transcript_reader(
                Cursor::new(tool_only),
                "pending",
                None,
                None,
                false
            )
            .unwrap()
            .is_none()
        );
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, Role::User);
        assert_eq!(session.messages[0].content, "Analyze this project");
        assert_eq!(session.messages[1].role, Role::Assistant);
        assert_eq!(session.messages[1].content, "This project is a local agent configuration hub.");
    }

    #[test]
    fn collect_antigravity_entries_joins_history_workspace() {
        let root = temp_antigravity_root("collect");
        let conversation_id = "52d82992-7695-4d38-8d02-9747eecba839";
        write_transcript(&root, conversation_id, "");
        fs::write(
            root.join("history.jsonl"),
            format!(
                r#"{{"display":"hi","workspace":"/tmp/project","conversationId":"{conversation_id}"}}"#
            ),
        )
        .unwrap();

        let entries = collect_antigravity_entries(&root).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id, conversation_id);
        assert_eq!(entries[0].directory.as_deref(), Some("/tmp/project"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_for_sync_skips_unchanged_session() {
        let root = temp_antigravity_root("skip");
        let conversation_id = "52d82992-7695-4d38-8d02-9747eecba839";
        let transcript = write_transcript(
            &root,
            conversation_id,
            r#"{"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-05-20T06:03:19Z","content":"hello"}"#,
        );
        let mtime = file_scan::stat_mtime_ms(&transcript).unwrap();
        let store = setup_store();

        let fresh = scan_for_sync_impl(
            &root,
            &AdapterSyncContext::from_store_for_test(&store, "antigravity-cli").unwrap(),
            None,
            false,
        )
        .unwrap();
        assert_eq!(fresh.sessions[0].source_file_path.as_deref(), transcript.to_str());

        store.insert_session(&make_existing_session(conversation_id, mtime, 1)).unwrap();

        let result = scan_for_sync_impl(
            &root,
            &AdapterSyncContext::from_store_for_test(&store, "antigravity-cli").unwrap(),
            None,
            false,
        )
        .unwrap();

        assert_eq!(result.sessions.len(), 0);
        assert_eq!(result.stats.skipped_sessions, 1);
        for version in [None, Some(EVENT_PARSER_VERSION - 1), Some(EVENT_PARSER_VERSION)] {
            if let Some(version) = version {
                store
                    .persist_session_events_for_existing_session(
                        "antigravity-cli",
                        conversation_id,
                        &[],
                        version,
                        Some(mtime),
                    )
                    .unwrap();
            }
            let result = scan_for_sync_impl(
                &root,
                &AdapterSyncContext::from_store_for_test(&store, "antigravity-cli").unwrap(),
                None,
                true,
            )
            .unwrap();
            assert_eq!(result.sessions.len(), usize::from(version != Some(EVENT_PARSER_VERSION)));
            if let Some(session) = result.sessions.first() {
                assert_eq!(session.event_parser_version, Some(EVENT_PARSER_VERSION));
            }
        }

        let _ = fs::remove_dir_all(&root);
    }
}
