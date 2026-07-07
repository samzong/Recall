use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;
use tracing::debug;
use walkdir::WalkDir;

use crate::adapters::file_scan::{self, FileScanEntry};
use crate::adapters::{
    RawMessage, RawSession, ResumeCommand, SourceAdapter, SyncScanResult, SyncScanStats,
};
use crate::db::store::Store;
use crate::types::Role;

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

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let Some(cli_dir) = resolve_antigravity_dir()? else {
            return Ok(vec![]);
        };

        let mut sessions = Vec::new();
        for entry in collect_antigravity_entries(&cli_dir)? {
            let Some(mtime_ms) = file_scan::stat_mtime_ms(&entry.stat_target) else {
                continue;
            };
            if let Some(raw) = parse_antigravity_session_for_entry(entry, mtime_ms)? {
                sessions.push(raw);
            }
        }
        Ok(sessions)
    }

    fn scan_for_sync(
        &self,
        store: &Store,
        since_ts: Option<i64>,
        _include_events: bool,
    ) -> anyhow::Result<Option<SyncScanResult>> {
        let Some(cli_dir) = resolve_antigravity_dir()? else {
            return Ok(Some(SyncScanResult { sessions: vec![], stats: SyncScanStats::default() }));
        };
        Ok(Some(scan_for_sync_impl(&cli_dir, store, since_ts)?))
    }
}

fn resolve_antigravity_dir() -> anyhow::Result<Option<PathBuf>> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home dir"))?;
    let dir = home.join(".gemini/antigravity-cli");
    if !dir.exists() {
        debug!("~/.gemini/antigravity-cli not found, skipping Antigravity CLI");
        return Ok(None);
    }
    Ok(Some(dir))
}

fn scan_for_sync_impl(
    cli_dir: &Path,
    store: &Store,
    since_ts: Option<i64>,
) -> anyhow::Result<SyncScanResult> {
    let entries = collect_antigravity_entries(cli_dir)?;
    file_scan::run_file_scan(
        store,
        "antigravity-cli",
        since_ts,
        entries,
        parse_antigravity_session_for_entry,
    )
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
) -> anyhow::Result<Option<RawSession>> {
    let mut raw = match parse_antigravity_transcript(&entry.stat_target, &entry.session_id) {
        Ok(Some(raw)) => raw,
        Ok(None) => return Ok(None),
        Err(e) => {
            debug!("failed to parse Antigravity transcript {}: {e}", entry.stat_target.display());
            return Ok(None);
        }
    };
    raw.directory = entry.directory;
    raw.updated_at = Some(mtime_ms);
    Ok(Some(raw))
}

fn parse_antigravity_transcript(
    path: &Path,
    fallback_id: &str,
) -> anyhow::Result<Option<RawSession>> {
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    parse_antigravity_transcript_reader(reader, fallback_id)
}

fn parse_antigravity_transcript_reader<R: BufRead>(
    reader: R,
    fallback_id: &str,
) -> anyhow::Result<Option<RawSession>> {
    let mut messages = Vec::new();

    for line in reader.lines() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("status").and_then(|status| status.as_str()) != Some("DONE") {
            continue;
        }

        let source = v.get("source").and_then(|source| source.as_str()).unwrap_or("");
        let event_type = v.get("type").and_then(|event_type| event_type.as_str()).unwrap_or("");
        let timestamp = parse_created_at(&v);

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

    if messages.is_empty() {
        return Ok(None);
    }

    let started_at = messages.first().and_then(|message| message.timestamp).unwrap_or(0);

    Ok(Some(RawSession::search_only(
        fallback_id.to_string(),
        None,
        started_at,
        messages.last().and_then(|message| message.timestamp),
        None,
        messages,
    )))
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
{"step_index":2,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-05-20T06:03:19Z","tool_calls":[{"name":"list_dir"}]}
{"step_index":3,"source":"MODEL","type":"LIST_DIRECTORY","status":"DONE","created_at":"2026-05-20T06:03:21Z","content":"tool output should not be indexed"}
{"step_index":15,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-05-20T06:03:30Z","content":"This project is a local agent configuration hub."}
"#;
        let session = parse_antigravity_transcript_reader(Cursor::new(jsonl), "agy-session")
            .unwrap()
            .unwrap();

        assert_eq!(session.source_id, "agy-session");
        assert_eq!(session.started_at, 1_779_256_999_000);
        assert_eq!(session.updated_at, Some(1_779_257_010_000));
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
        store.insert_session(&make_existing_session(conversation_id, mtime, 1)).unwrap();

        let result = scan_for_sync_impl(&root, &store, None).unwrap();

        assert_eq!(result.sessions.len(), 0);
        assert_eq!(result.stats.skipped_sessions, 1);

        let _ = fs::remove_dir_all(&root);
    }
}
