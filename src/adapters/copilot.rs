use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;
use tracing::debug;

use crate::adapters::file_scan::{self, FileScanEntry};
use crate::adapters::{
    RawMessage, RawSession, ResumeCommand, SourceAdapter, SyncScanResult, SyncScanStats,
};
use crate::db::store::Store;
use crate::types::Role;

pub struct CopilotAdapter;

impl SourceAdapter for CopilotAdapter {
    fn id(&self) -> &str {
        "copilot-cli"
    }
    fn label(&self) -> &str {
        "CPL"
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        Some(ResumeCommand {
            program: "copilot".to_string(),
            args: vec![format!("--resume={source_id}")],
        })
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let Some(sessions_dir) = resolve_copilot_dir()? else {
            return Ok(vec![]);
        };

        let mut sessions = Vec::new();
        for entry in collect_copilot_entries(&sessions_dir) {
            let Some(mtime_ms) = file_scan::stat_mtime_ms(&entry.stat_target) else {
                continue;
            };
            if let Some(raw) = parse_copilot_session_for_entry(entry, mtime_ms)? {
                sessions.push(raw);
            }
        }
        Ok(sessions)
    }

    fn scan_for_sync(
        &self,
        store: &Store,
        since_ts: Option<i64>,
    ) -> anyhow::Result<Option<SyncScanResult>> {
        let Some(sessions_dir) = resolve_copilot_dir()? else {
            return Ok(Some(SyncScanResult { sessions: vec![], stats: SyncScanStats::default() }));
        };
        let result = scan_for_sync_impl(&sessions_dir, store, since_ts)?;
        Ok(Some(result))
    }
}

fn resolve_copilot_dir() -> anyhow::Result<Option<PathBuf>> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home dir"))?;
    let dir = home.join(".copilot/session-state");
    if !dir.exists() {
        debug!("~/.copilot/session-state not found, skipping Copilot CLI");
        return Ok(None);
    }
    Ok(Some(dir))
}

fn scan_for_sync_impl(
    sessions_dir: &Path,
    store: &Store,
    since_ts: Option<i64>,
) -> anyhow::Result<SyncScanResult> {
    let entries = collect_copilot_entries(sessions_dir);
    file_scan::run_file_scan(
        store,
        "copilot-cli",
        since_ts,
        entries,
        parse_copilot_session_for_entry,
    )
}

fn collect_copilot_entries(sessions_dir: &Path) -> Vec<FileScanEntry> {
    let mut entries = Vec::new();
    let read = match fs::read_dir(sessions_dir) {
        Ok(r) => r,
        Err(e) => {
            debug!("cannot read {}: {e}", sessions_dir.display());
            return entries;
        }
    };

    for dir_entry in read.flatten() {
        let session_dir = dir_entry.path();
        if !session_dir.is_dir() {
            continue;
        }
        let events_path = session_dir.join("events.jsonl");
        if !events_path.is_file() {
            continue;
        }
        let dir_name = match session_dir.file_name().and_then(|n| n.to_str()) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => continue,
        };
        let session_id = peek_copilot_session_id(&events_path).unwrap_or_else(|| dir_name.clone());

        entries.push(FileScanEntry { session_id, stat_target: events_path, directory: None });
    }
    entries
}

fn peek_copilot_session_id(events_path: &Path) -> Option<String> {
    let file = fs::File::open(events_path).ok()?;
    let reader = BufReader::new(file);
    for (idx, line) in reader.lines().enumerate() {
        if idx >= 16 {
            break;
        }
        let Ok(line) = line else { continue };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("type").and_then(|t| t.as_str()) == Some("session.start") {
            return v
                .get("data")
                .and_then(|d| d.get("sessionId"))
                .and_then(|s| s.as_str())
                .map(String::from);
        }
    }
    None
}

fn parse_copilot_session_for_entry(
    entry: FileScanEntry,
    mtime_ms: i64,
) -> anyhow::Result<Option<RawSession>> {
    let file = match fs::File::open(&entry.stat_target) {
        Ok(f) => f,
        Err(e) => {
            debug!("failed to read {}: {e}", entry.stat_target.display());
            return Ok(None);
        }
    };
    let lines = BufReader::new(file).lines().map_while(Result::ok);
    let mut raw = match parse_copilot_events_from_lines(lines, &entry.session_id) {
        Ok(Some(raw)) => raw,
        Ok(None) => return Ok(None),
        Err(e) => {
            debug!("failed to parse copilot session {}: {e}", entry.stat_target.display());
            return Ok(None);
        }
    };
    raw.source_id = entry.session_id;
    raw.updated_at = Some(mtime_ms);
    Ok(Some(raw))
}

pub fn parse_copilot_events(
    content: &str,
    fallback_id: &str,
) -> anyhow::Result<Option<RawSession>> {
    parse_copilot_events_from_lines(content.lines().map(String::from), fallback_id)
}

fn parse_copilot_events_from_lines<I>(
    lines: I,
    fallback_id: &str,
) -> anyhow::Result<Option<RawSession>>
where
    I: IntoIterator<Item = String>,
{
    let mut session_id: Option<String> = None;
    let mut directory: Option<String> = None;
    let mut meta_started_at: Option<i64> = None;
    let mut tool_names: HashMap<String, String> = HashMap::new();
    let mut messages = Vec::new();

    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let event_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let timestamp = parse_timestamp(&v);

        match event_type {
            "session.start" => {
                if let Some(data) = v.get("data") {
                    session_id = data.get("sessionId").and_then(|s| s.as_str()).map(String::from);
                    meta_started_at = data
                        .get("startTime")
                        .and_then(|t| t.as_str())
                        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                        .map(|dt| dt.timestamp_millis());
                    directory = data
                        .get("context")
                        .and_then(|c| c.get("cwd"))
                        .and_then(|c| c.as_str())
                        .map(String::from);
                }
            }
            "user.message" => {
                let Some(data) = v.get("data") else { continue };
                let content =
                    data.get("content").and_then(|c| c.as_str()).unwrap_or("").trim().to_string();
                if content.is_empty() {
                    continue;
                }
                messages.push(RawMessage { role: Role::User, content, timestamp });
            }
            "assistant.message" => {
                let Some(data) = v.get("data") else { continue };
                let prose =
                    data.get("content").and_then(|c| c.as_str()).unwrap_or("").trim().to_string();
                let tool_text = extract_tool_requests(data.get("toolRequests"));
                let content = match (prose.is_empty(), tool_text.is_empty()) {
                    (true, true) => continue,
                    (false, true) => prose,
                    (true, false) => tool_text,
                    (false, false) => format!("{prose}\n{tool_text}"),
                };
                messages.push(RawMessage { role: Role::Assistant, content, timestamp });
            }
            "tool.execution_start" => {
                if let Some(data) = v.get("data")
                    && let (Some(id), Some(name)) = (
                        data.get("toolCallId").and_then(|s| s.as_str()),
                        data.get("toolName").and_then(|s| s.as_str()),
                    )
                {
                    tool_names.insert(id.to_string(), name.to_string());
                }
            }
            "tool.execution_complete" => {
                let Some(data) = v.get("data") else { continue };
                let Some(result) = data.get("result") else { continue };
                let text = result
                    .get("detailedContent")
                    .and_then(|c| c.as_str())
                    .or_else(|| result.get("content").and_then(|c| c.as_str()))
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if text.is_empty() {
                    continue;
                }
                let tool_name = data
                    .get("toolCallId")
                    .and_then(|s| s.as_str())
                    .and_then(|id| tool_names.get(id).cloned())
                    .unwrap_or_else(|| "tool".to_string());
                messages.push(RawMessage {
                    role: Role::Assistant,
                    content: format!("[{tool_name}] {text}"),
                    timestamp,
                });
            }
            _ => {}
        }
    }

    if messages.is_empty() {
        return Ok(None);
    }

    let source_id = session_id.unwrap_or_else(|| fallback_id.to_string());
    let started_at =
        meta_started_at.or_else(|| messages.first().and_then(|m| m.timestamp)).unwrap_or(0);
    let updated_at = messages.last().and_then(|m| m.timestamp);

    Ok(Some(RawSession {
        source_id,
        directory,
        started_at,
        updated_at,
        entrypoint: None,
        messages,
    }))
}

fn extract_tool_requests(tool_requests: Option<&Value>) -> String {
    let Some(arr) = tool_requests.and_then(|v| v.as_array()) else {
        return String::new();
    };

    let mut parts = Vec::new();
    for req in arr {
        let name = req.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
        let args = req
            .get("arguments")
            .map(|a| serde_json::to_string(a).unwrap_or_default())
            .unwrap_or_default();
        parts.push(format!("[{name}] {args}"));
    }
    parts.join("\n")
}

fn parse_timestamp(v: &Value) -> Option<i64> {
    v.get("timestamp")
        .and_then(|t| t.as_str())
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|dt| dt.timestamp_millis())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    use crate::db::{schema, store::Store};
    use crate::types::Session;

    fn setup_store() -> Store {
        schema::register_sqlite_vec();
        Store::open_in_memory().unwrap()
    }

    fn temp_copilot_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "recall-cpl-test-{}-{}",
            label,
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_copilot_session(
        sessions_dir: &Path,
        dir_name: &str,
        session_id: &str,
        user_text: &str,
    ) -> PathBuf {
        let session_dir = sessions_dir.join(dir_name);
        fs::create_dir_all(&session_dir).unwrap();
        let events_path = session_dir.join("events.jsonl");

        let start = serde_json::json!({
            "type": "session.start",
            "timestamp": "2026-04-13T10:00:00Z",
            "data": {
                "sessionId": session_id,
                "startTime": "2026-04-13T10:00:00Z",
                "context": { "cwd": "/tmp/foo" }
            }
        });
        let user = serde_json::json!({
            "type": "user.message",
            "timestamp": "2026-04-13T10:00:05Z",
            "data": { "content": user_text }
        });

        let mut f = fs::File::create(&events_path).unwrap();
        writeln!(f, "{start}").unwrap();
        writeln!(f, "{user}").unwrap();
        events_path
    }

    fn make_existing_session(source_id: &str, updated_at: i64, message_count: u32) -> Session {
        Session {
            id: format!("internal-{source_id}"),
            source: "copilot-cli".to_string(),
            source_id: source_id.to_string(),
            title: "existing".to_string(),
            directory: None,
            started_at: 0,
            updated_at: Some(updated_at),
            message_count,
            entrypoint: None,
        }
    }

    #[test]
    fn peek_copilot_session_id_reads_session_start() {
        let root = temp_copilot_root("peek");
        let sessions_dir = root.join("session-state");
        let uuid = "f3eca837-818f-44d7-9158-bf242901f960";
        let events_path = write_copilot_session(&sessions_dir, "dir-alias", uuid, "hello");

        assert_eq!(peek_copilot_session_id(&events_path), Some(uuid.to_string()));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn peek_copilot_session_id_falls_back_when_no_session_start() {
        let root = temp_copilot_root("peek-missing");
        let sessions_dir = root.join("session-state");
        let dir = sessions_dir.join("dir-alias");
        fs::create_dir_all(&dir).unwrap();
        let events_path = dir.join("events.jsonl");
        let msg = serde_json::json!({
            "type": "user.message",
            "timestamp": "2026-04-13T10:00:00Z",
            "data": { "content": "hi" }
        });
        let mut f = fs::File::create(&events_path).unwrap();
        writeln!(f, "{msg}").unwrap();

        assert_eq!(peek_copilot_session_id(&events_path), None);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn collect_copilot_entries_skips_dirs_without_events() {
        let root = temp_copilot_root("collect-skip");
        let sessions_dir = root.join("session-state");
        fs::create_dir_all(sessions_dir.join("empty-dir")).unwrap();
        write_copilot_session(
            &sessions_dir,
            "good-dir",
            "f3eca837-818f-44d7-9158-bf242901f960",
            "hello",
        );

        let entries = collect_copilot_entries(&sessions_dir);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id, "f3eca837-818f-44d7-9158-bf242901f960");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_for_sync_skips_unchanged_session() {
        let root = temp_copilot_root("skip");
        let sessions_dir = root.join("session-state");
        let uuid = "f3eca837-818f-44d7-9158-bf242901f960";
        let events_path = write_copilot_session(&sessions_dir, "dir-1", uuid, "hello");
        let mtime = file_scan::stat_mtime_ms(&events_path).unwrap();

        let store = setup_store();
        store.insert_session(&make_existing_session(uuid, mtime, 1)).unwrap();

        let result = scan_for_sync_impl(&sessions_dir, &store, None).unwrap();
        assert_eq!(result.sessions.len(), 0);
        assert_eq!(result.stats.skipped_sessions, 1);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_for_sync_reparses_when_mtime_changes() {
        let root = temp_copilot_root("mismatch");
        let sessions_dir = root.join("session-state");
        let uuid = "f3eca837-818f-44d7-9158-bf242901f960";
        let events_path = write_copilot_session(&sessions_dir, "dir-1", uuid, "hi");
        let actual_mtime = file_scan::stat_mtime_ms(&events_path).unwrap();

        let store = setup_store();
        store.insert_session(&make_existing_session(uuid, actual_mtime - 1_000, 1)).unwrap();

        let result = scan_for_sync_impl(&sessions_dir, &store, None).unwrap();
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].source_id, uuid);
        assert_eq!(result.sessions[0].updated_at, Some(actual_mtime));
        assert_eq!(result.stats.skipped_sessions, 0);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_for_sync_picks_up_new_session() {
        let root = temp_copilot_root("new");
        let sessions_dir = root.join("session-state");
        let uuid = "f3eca837-818f-44d7-9158-bf242901f960";
        write_copilot_session(&sessions_dir, "dir-1", uuid, "fresh");

        let store = setup_store();

        let result = scan_for_sync_impl(&sessions_dir, &store, None).unwrap();
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].source_id, uuid);
        assert_eq!(result.stats.skipped_sessions, 0);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_for_sync_falls_back_to_dir_name_when_session_start_missing() {
        let root = temp_copilot_root("fallback");
        let sessions_dir = root.join("session-state");
        let dir_name = "0b247666-6f95-49e5-b68f-b05eb338e9c2";
        let session_dir = sessions_dir.join(dir_name);
        fs::create_dir_all(&session_dir).unwrap();
        let events_path = session_dir.join("events.jsonl");
        let user = serde_json::json!({
            "type": "user.message",
            "timestamp": "2026-04-13T10:00:00Z",
            "data": { "content": "legacy" }
        });
        let mut f = fs::File::create(&events_path).unwrap();
        writeln!(f, "{user}").unwrap();

        let store = setup_store();
        let result = scan_for_sync_impl(&sessions_dir, &store, None).unwrap();
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].source_id, dir_name);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_for_sync_keeps_going_when_one_file_is_unreadable() {
        let root = temp_copilot_root("unreadable");
        let sessions_dir = root.join("session-state");

        let good_uuid = "f3eca837-818f-44d7-9158-bf242901f960";
        write_copilot_session(&sessions_dir, "good-dir", good_uuid, "still here");

        let bad_dir = sessions_dir.join("bad-dir");
        fs::create_dir_all(&bad_dir).unwrap();
        let bad_events = bad_dir.join("events.jsonl");
        let mut f = fs::File::create(&bad_events).unwrap();
        f.write_all(&[0xFF, 0xFE, 0xFD, 0xFC]).unwrap();

        let store = setup_store();
        let result = scan_for_sync_impl(&sessions_dir, &store, None).unwrap();
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].source_id, good_uuid);

        let _ = fs::remove_dir_all(&root);
    }
}
