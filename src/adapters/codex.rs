use std::fs;
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

pub struct CodexAdapter;

impl SourceAdapter for CodexAdapter {
    fn id(&self) -> &str {
        "codex"
    }
    fn label(&self) -> &str {
        "CDX"
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        Some(ResumeCommand {
            program: "codex".to_string(),
            args: vec!["resume".to_string(), source_id.to_string()],
        })
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let Some(codex_dir) = resolve_codex_dir()? else {
            return Ok(vec![]);
        };
        let sessions_dir = codex_dir.join("sessions");
        let archived_dir = codex_dir.join("archived_sessions");

        let mut sessions = Vec::new();
        for entry in collect_codex_entries(&[&sessions_dir, &archived_dir]) {
            let Some(mtime_ms) = file_scan::stat_mtime_ms(&entry.stat_target) else {
                continue;
            };
            if let Some(raw) = parse_codex_session_for_entry(entry, mtime_ms)? {
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
        let Some(codex_dir) = resolve_codex_dir()? else {
            return Ok(Some(SyncScanResult { sessions: vec![], stats: SyncScanStats::default() }));
        };
        let result = scan_for_sync_impl(&codex_dir, store, since_ts)?;
        Ok(Some(result))
    }
}

fn resolve_codex_dir() -> anyhow::Result<Option<PathBuf>> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home dir"))?;
    let dir = home.join(".codex");
    if !dir.exists() {
        debug!("~/.codex not found, skipping Codex");
        return Ok(None);
    }
    Ok(Some(dir))
}

fn scan_for_sync_impl(
    codex_dir: &Path,
    store: &Store,
    since_ts: Option<i64>,
) -> anyhow::Result<SyncScanResult> {
    let sessions_dir = codex_dir.join("sessions");
    let archived_dir = codex_dir.join("archived_sessions");
    let entries = collect_codex_entries(&[&sessions_dir, &archived_dir]);
    file_scan::run_file_scan(store, "codex", since_ts, entries, parse_codex_session_for_entry)
}

fn collect_codex_entries(base_dirs: &[&Path]) -> Vec<FileScanEntry> {
    let mut entries = Vec::new();
    for dir in base_dirs {
        if !dir.exists() {
            continue;
        }
        for walk_entry in WalkDir::new(dir).into_iter().filter_map(|e| e.ok()) {
            let path = walk_entry.path();
            let ext = path.extension().and_then(|e| e.to_str());
            if ext != Some("jsonl") && ext != Some("json") {
                continue;
            }
            if !path.is_file() {
                continue;
            }
            let stem = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) if !s.is_empty() => s,
                _ => continue,
            };
            let Some(session_id) = extract_session_id_from_filename(stem) else {
                continue;
            };
            entries.push(FileScanEntry {
                session_id,
                stat_target: path.to_path_buf(),
                directory: None,
            });
        }
    }
    entries
}

fn parse_codex_session_for_entry(
    entry: FileScanEntry,
    mtime_ms: i64,
) -> anyhow::Result<Option<RawSession>> {
    let Some(mut raw) = parse_codex_session(&entry.stat_target)? else {
        return Ok(None);
    };
    raw.source_id = entry.session_id;
    raw.updated_at = Some(mtime_ms);
    Ok(Some(raw))
}

fn extract_session_id_from_filename(stem: &str) -> Option<String> {
    if stem.len() < 37 {
        return None;
    }
    let (prefix, tail) = stem.split_at(stem.len() - 36);
    if !prefix.ends_with('-') {
        return None;
    }
    uuid::Uuid::try_parse(tail).ok().map(|_| tail.to_string())
}

fn parse_codex_session(path: &Path) -> anyhow::Result<Option<RawSession>> {
    let content = fs::read_to_string(path)?;
    let mut meta_id: Option<String> = None;
    let mut meta_cwd: Option<String> = None;
    let mut meta_timestamp: Option<i64> = None;
    let mut messages = Vec::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let msg_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");

        match msg_type {
            "session_meta" => {
                if let Some(payload) = v.get("payload") {
                    meta_id = payload.get("id").and_then(|s| s.as_str()).map(String::from);
                    meta_cwd = payload.get("cwd").and_then(|s| s.as_str()).map(String::from);
                    meta_timestamp = payload
                        .get("timestamp")
                        .and_then(|t| t.as_str())
                        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                        .map(|dt| dt.timestamp_millis());
                }
            }
            "event_msg" => {
                if let Some(payload) = v.get("payload") {
                    let payload_type = payload.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    match payload_type {
                        "user_message" => {
                            if let Some(text) = payload.get("message").and_then(|m| m.as_str())
                                && !text.is_empty()
                            {
                                let ts = parse_timestamp(&v);
                                messages.push(RawMessage {
                                    role: Role::User,
                                    content: text.to_string(),
                                    timestamp: ts,
                                });
                            }
                        }
                        "agent_message" => {
                            if let Some(text) = payload.get("message").and_then(|m| m.as_str())
                                && !text.is_empty()
                            {
                                let ts = parse_timestamp(&v);
                                messages.push(RawMessage {
                                    role: Role::Assistant,
                                    content: text.to_string(),
                                    timestamp: ts,
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
            "response_item" => {
                if let Some(payload) = v.get("payload")
                    && payload.get("type").and_then(|t| t.as_str()) == Some("message")
                    && payload.get("role").and_then(|r| r.as_str()) == Some("assistant")
                {
                    let text = extract_content_array(payload.get("content"));
                    if !text.is_empty() {
                        let ts = parse_timestamp(&v);
                        messages.push(RawMessage {
                            role: Role::Assistant,
                            content: text,
                            timestamp: ts,
                        });
                    }
                }
            }
            _ => {}
        }
    }

    if messages.is_empty() {
        return Ok(None);
    }

    let source_id = meta_id.unwrap_or_else(|| {
        path.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown").to_string()
    });

    let started_at =
        meta_timestamp.or_else(|| messages.first().and_then(|m| m.timestamp)).unwrap_or(0);

    Ok(Some(RawSession {
        source_id,
        directory: meta_cwd,
        started_at,
        updated_at: messages.last().and_then(|m| m.timestamp),
        entrypoint: None,
        messages,
    }))
}

fn extract_content_array(content: Option<&Value>) -> String {
    match content {
        Some(Value::Array(arr)) => {
            let mut parts = Vec::new();
            for item in arr {
                match item.get("type").and_then(|t| t.as_str()) {
                    Some("text" | "output_text") => {
                        if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                            parts.push(text.to_string());
                        }
                    }
                    Some("function_call") => {
                        let name = item.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                        if let Some(args) = item.get("arguments").and_then(|a| a.as_str()) {
                            parts.push(format!("[{name}] {args}"));
                        }
                    }
                    Some("function_call_output") => {
                        if let Some(output) = item.get("output").and_then(|o| o.as_str()) {
                            parts.push(output.to_string());
                        }
                    }
                    _ => {}
                }
            }
            parts.join("\n")
        }
        _ => String::new(),
    }
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

    fn temp_codex_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "recall-cdx-test-{}-{}",
            label,
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_codex_rollout(sessions_dir: &Path, session_uuid: &str, text: &str) -> PathBuf {
        fs::create_dir_all(sessions_dir).unwrap();
        let filename = format!("rollout-2026-04-13T10-00-00-{session_uuid}.jsonl");
        let path = sessions_dir.join(filename);
        let meta = serde_json::json!({
            "type": "session_meta",
            "payload": {
                "id": session_uuid,
                "timestamp": "2026-04-13T10:00:00Z",
                "cwd": "/tmp/foo"
            }
        });
        let msg = serde_json::json!({
            "type": "event_msg",
            "timestamp": "2026-04-13T10:00:30Z",
            "payload": {
                "type": "user_message",
                "message": text
            }
        });
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "{meta}").unwrap();
        writeln!(f, "{msg}").unwrap();
        path
    }

    fn make_existing_session(source_id: &str, updated_at: i64, message_count: u32) -> Session {
        Session {
            id: format!("internal-{source_id}"),
            source: "codex".to_string(),
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
    fn extract_session_id_from_filename_happy_path() {
        let stem = "rollout-2025-11-04T07-16-24-019a4c01-e8f4-7270-bdab-7f19273b237e";
        assert_eq!(
            extract_session_id_from_filename(stem),
            Some("019a4c01-e8f4-7270-bdab-7f19273b237e".to_string())
        );
    }

    #[test]
    fn extract_session_id_from_filename_rejects_non_uuid_tail() {
        assert_eq!(extract_session_id_from_filename("short"), None);
        assert_eq!(extract_session_id_from_filename("rollout-no-uuid-at-end"), None);
        let non_hex_tail = "rollout-xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx";
        assert_eq!(extract_session_id_from_filename(non_hex_tail), None);
        let no_separator = "rolloutX019a4c01-e8f4-7270-bdab-7f19273b237e";
        assert_eq!(extract_session_id_from_filename(no_separator), None);
        let bad_dash_layout = "rollout-019a4c01Xe8f4-7270-bdab-7f19273b237e";
        assert_eq!(extract_session_id_from_filename(bad_dash_layout), None);
    }

    #[test]
    fn scan_for_sync_skips_unchanged_session() {
        let root = temp_codex_root("skip");
        let sessions_dir = root.join("sessions");
        let uuid = "019a4c01-e8f4-7270-bdab-7f19273b237e";
        let path = write_codex_rollout(&sessions_dir, uuid, "hello");
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();

        let store = setup_store();
        store.insert_session(&make_existing_session(uuid, mtime, 1)).unwrap();

        let result = scan_for_sync_impl(&root, &store, None).unwrap();
        assert_eq!(result.sessions.len(), 0);
        assert_eq!(result.stats.skipped_sessions, 1);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_for_sync_reparses_when_mtime_changes() {
        let root = temp_codex_root("mismatch");
        let sessions_dir = root.join("sessions");
        let uuid = "019a4c01-e8f4-7270-bdab-7f19273b237e";
        let path = write_codex_rollout(&sessions_dir, uuid, "hi");
        let actual_mtime = file_scan::stat_mtime_ms(&path).unwrap();

        let store = setup_store();
        store.insert_session(&make_existing_session(uuid, actual_mtime - 1_000, 1)).unwrap();

        let result = scan_for_sync_impl(&root, &store, None).unwrap();
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].source_id, uuid);
        assert_eq!(result.sessions[0].updated_at, Some(actual_mtime));
        assert_eq!(result.stats.skipped_sessions, 0);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_for_sync_picks_up_new_session() {
        let root = temp_codex_root("new");
        let sessions_dir = root.join("archived_sessions");
        let uuid = "019a4c01-e8f4-7270-bdab-7f19273b237e";
        write_codex_rollout(&sessions_dir, uuid, "fresh");

        let store = setup_store();

        let result = scan_for_sync_impl(&root, &store, None).unwrap();
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].source_id, uuid);
        assert_eq!(result.stats.skipped_sessions, 0);

        let _ = fs::remove_dir_all(&root);
    }
}
