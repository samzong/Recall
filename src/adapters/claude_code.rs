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

pub struct ClaudeCodeAdapter;

impl SourceAdapter for ClaudeCodeAdapter {
    fn id(&self) -> &str {
        "claude-code"
    }
    fn label(&self) -> &str {
        "CC"
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        Some(ResumeCommand {
            program: "claude".to_string(),
            args: vec!["--resume".to_string(), source_id.to_string()],
        })
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let Some(claude_dir) = resolve_claude_dir()? else {
            return Ok(vec![]);
        };
        let session_index = load_session_index(&claude_dir);

        let mut sessions = Vec::new();
        let mut entries = collect_project_entries(&claude_dir, &session_index);
        entries.extend(collect_transcript_entries(&claude_dir));

        for entry in entries {
            let Some(mtime_ms) = file_scan::stat_mtime_ms(&entry.stat_target) else {
                continue;
            };
            if let Some(raw) = parse_claude_session_file(entry, mtime_ms, &session_index)? {
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
        let Some(claude_dir) = resolve_claude_dir()? else {
            return Ok(Some(SyncScanResult { sessions: vec![], stats: SyncScanStats::default() }));
        };
        let result = scan_for_sync_impl(&claude_dir, store, since_ts)?;
        Ok(Some(result))
    }
}

struct SessionMeta {
    cwd: Option<String>,
    started_at: i64,
    entrypoint: Option<String>,
}

fn resolve_claude_dir() -> anyhow::Result<Option<PathBuf>> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home dir"))?;
    let dir = home.join(".claude");
    if !dir.exists() {
        debug!("~/.claude not found, skipping Claude Code");
        return Ok(None);
    }
    Ok(Some(dir))
}

fn scan_for_sync_impl(
    claude_dir: &Path,
    store: &Store,
    since_ts: Option<i64>,
) -> anyhow::Result<SyncScanResult> {
    let session_index = load_session_index(claude_dir);
    let mut entries = collect_project_entries(claude_dir, &session_index);
    entries.extend(collect_transcript_entries(claude_dir));

    file_scan::run_file_scan(store, "claude-code", since_ts, entries, |entry, mtime_ms| {
        parse_claude_session_file(entry, mtime_ms, &session_index)
    })
}

fn load_session_index(claude_dir: &Path) -> HashMap<String, SessionMeta> {
    let sessions_dir = claude_dir.join("sessions");
    let mut index = HashMap::new();
    if !sessions_dir.exists() {
        return index;
    }

    let entries = match fs::read_dir(&sessions_dir) {
        Ok(e) => e,
        Err(e) => {
            debug!("cannot read ~/.claude/sessions: {e}");
            return index;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let v: Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(session_id) = v.get("sessionId").and_then(|s| s.as_str()) {
            let meta = SessionMeta {
                cwd: v.get("cwd").and_then(|s| s.as_str()).map(|s| s.to_string()),
                started_at: v.get("startedAt").and_then(|s| s.as_i64()).unwrap_or(0),
                entrypoint: v.get("entrypoint").and_then(|s| s.as_str()).map(|s| s.to_string()),
            };
            index.insert(session_id.to_string(), meta);
        }
    }
    index
}

fn collect_project_entries(
    claude_dir: &Path,
    session_index: &HashMap<String, SessionMeta>,
) -> Vec<FileScanEntry> {
    let projects_dir = claude_dir.join("projects");
    if !projects_dir.exists() {
        return vec![];
    }

    let mut entries = Vec::new();

    let project_dirs = match fs::read_dir(&projects_dir) {
        Ok(e) => e,
        Err(e) => {
            debug!("cannot read ~/.claude/projects: {e}");
            return vec![];
        }
    };

    for project_entry in project_dirs.flatten() {
        let project_path = project_entry.path();
        if !project_path.is_dir() {
            continue;
        }

        let dir_name = project_entry.file_name().to_string_lossy().to_string();
        let directory_hint = project_key_to_path(&dir_name);

        let jsonl_files = match fs::read_dir(&project_path) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for file_entry in jsonl_files.flatten() {
            let file_path = file_entry.path();
            if file_path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let session_id = match file_path.file_stem().and_then(|s| s.to_str()) {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => continue,
            };

            let meta_cwd = session_index.get(&session_id).and_then(|m| m.cwd.clone());
            let directory = meta_cwd.or_else(|| Some(directory_hint.clone()));

            entries.push(FileScanEntry { session_id, stat_target: file_path, directory });
        }
    }

    entries
}

fn collect_transcript_entries(claude_dir: &Path) -> Vec<FileScanEntry> {
    let transcripts_dir = claude_dir.join("transcripts");
    if !transcripts_dir.exists() {
        return vec![];
    }

    let mut entries = Vec::new();

    for entry in WalkDir::new(&transcripts_dir).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        if !path.is_file() {
            continue;
        }
        let session_id = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => continue,
        };

        entries.push(FileScanEntry {
            session_id,
            stat_target: path.to_path_buf(),
            directory: None,
        });
    }

    entries
}

fn parse_claude_session_file(
    entry: FileScanEntry,
    mtime_ms: i64,
    session_index: &HashMap<String, SessionMeta>,
) -> anyhow::Result<Option<RawSession>> {
    let messages = match parse_conversation_jsonl(&entry.stat_target) {
        Ok(m) => m,
        Err(e) => {
            debug!("failed to parse {}: {e}", entry.stat_target.display());
            return Ok(None);
        }
    };

    if messages.is_empty() {
        return Ok(None);
    }

    let meta = session_index.get(&entry.session_id);
    let started_at = meta
        .map(|m| m.started_at)
        .or_else(|| messages.first().and_then(|m| m.timestamp))
        .unwrap_or(0);
    let directory = meta.and_then(|m| m.cwd.clone()).or(entry.directory);
    let entrypoint = meta.and_then(|m| m.entrypoint.clone());

    Ok(Some(RawSession {
        source_id: entry.session_id,
        directory,
        started_at,
        updated_at: Some(mtime_ms),
        entrypoint,
        messages,
    }))
}

fn parse_conversation_jsonl(path: &Path) -> anyhow::Result<Vec<RawMessage>> {
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut messages = Vec::new();

    for line in reader.lines().map_while(Result::ok) {
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
            "user" | "assistant" => {}
            _ => continue,
        }

        let role = if msg_type == "user" { Role::User } else { Role::Assistant };

        let message = match v.get("message") {
            Some(m) => m,
            None => continue,
        };

        let text = extract_content(message.get("content"));
        if text.is_empty() {
            continue;
        }

        let timestamp = v
            .get("timestamp")
            .and_then(|t| t.as_str())
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map(|dt| dt.timestamp_millis());

        messages.push(RawMessage { role, content: text, timestamp });
    }

    Ok(messages)
}

fn extract_content(content: Option<&Value>) -> String {
    match content {
        None => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => {
            let mut parts = Vec::new();
            for item in arr {
                match item.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                            parts.push(text.to_string());
                        }
                    }
                    Some("tool_use") => {
                        let name = item.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                        if let Some(input) = item.get("input") {
                            parts.push(format!("[{name}] {input}"));
                        }
                    }
                    Some("tool_result") => {
                        if let Some(content) = item.get("content") {
                            match content {
                                Value::String(s) => parts.push(s.clone()),
                                Value::Array(inner) => {
                                    for block in inner {
                                        if block.get("type").and_then(|t| t.as_str())
                                            == Some("text")
                                            && let Some(text) =
                                                block.get("text").and_then(|t| t.as_str())
                                        {
                                            parts.push(text.to_string());
                                        }
                                    }
                                }
                                _ => {}
                            }
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

fn project_key_to_path(key: &str) -> String {
    let key = key.strip_prefix('-').unwrap_or(key);
    let mut result = String::with_capacity(key.len() + 1);
    result.push('/');
    let mut chars = key.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '-' {
            if chars.peek() == Some(&'-') {
                chars.next();
                result.push_str("/.");
            } else {
                result.push('/');
            }
        } else {
            result.push(c);
        }
    }
    result
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

    fn temp_claude_root(label: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("recall-cc-test-{}-{}", label, uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_user_jsonl(project_dir: &Path, session_id: &str, text: &str) -> PathBuf {
        fs::create_dir_all(project_dir).unwrap();
        let path = project_dir.join(format!("{session_id}.jsonl"));
        let line = serde_json::json!({
            "type": "user",
            "message": {"content": text},
            "timestamp": "2026-04-13T10:00:00Z"
        });
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "{line}").unwrap();
        path
    }

    fn make_existing_session(source_id: &str, updated_at: i64, message_count: u32) -> Session {
        Session {
            id: format!("internal-{source_id}"),
            source: "claude-code".to_string(),
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
    fn parse_claude_session_file_sets_updated_at_to_mtime() {
        let root = temp_claude_root("parse");
        let project = root.join("projects").join("-tmp-foo");
        let path = write_user_jsonl(&project, "abc-123", "hello");
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();

        let entry = FileScanEntry {
            session_id: "abc-123".to_string(),
            stat_target: path.clone(),
            directory: Some("/tmp/foo".to_string()),
        };
        let session_index = HashMap::new();
        let raw = parse_claude_session_file(entry, mtime, &session_index).unwrap().unwrap();

        assert_eq!(raw.source_id, "abc-123");
        assert_eq!(raw.updated_at, Some(mtime));
        assert_eq!(raw.directory.as_deref(), Some("/tmp/foo"));
        assert_eq!(raw.messages.len(), 1);
        assert_eq!(raw.messages[0].content, "hello");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn collect_project_entries_walks_nested_projects() {
        let root = temp_claude_root("collect");
        let p1 = root.join("projects").join("-tmp-foo");
        let p2 = root.join("projects").join("-tmp-bar");
        write_user_jsonl(&p1, "sess-1", "a");
        write_user_jsonl(&p2, "sess-2", "b");

        let session_index = HashMap::new();
        let entries = collect_project_entries(&root, &session_index);
        assert_eq!(entries.len(), 2);
        let ids: Vec<_> = entries.iter().map(|e| e.session_id.clone()).collect();
        assert!(ids.contains(&"sess-1".to_string()));
        assert!(ids.contains(&"sess-2".to_string()));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_for_sync_skips_unchanged_session() {
        let root = temp_claude_root("skip");
        let project = root.join("projects").join("-tmp-proj");
        let path = write_user_jsonl(&project, "sess-skip", "hello");
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();

        let store = setup_store();
        store.insert_session(&make_existing_session("sess-skip", mtime, 1)).unwrap();

        let result = scan_for_sync_impl(&root, &store, None).unwrap();
        assert_eq!(result.sessions.len(), 0);
        assert_eq!(result.stats.skipped_sessions, 1);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_for_sync_reparses_when_mtime_diverges() {
        let root = temp_claude_root("mismatch");
        let project = root.join("projects").join("-tmp-proj");
        let path = write_user_jsonl(&project, "sess-stale", "hi");
        let actual_mtime = file_scan::stat_mtime_ms(&path).unwrap();

        let store = setup_store();
        store
            .insert_session(&make_existing_session("sess-stale", actual_mtime - 1_000, 1))
            .unwrap();

        let result = scan_for_sync_impl(&root, &store, None).unwrap();
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].source_id, "sess-stale");
        assert_eq!(result.sessions[0].updated_at, Some(actual_mtime));
        assert_eq!(result.stats.skipped_sessions, 0);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_for_sync_picks_up_new_session() {
        let root = temp_claude_root("new");
        let project = root.join("projects").join("-tmp-proj");
        write_user_jsonl(&project, "sess-fresh", "fresh");

        let store = setup_store();

        let result = scan_for_sync_impl(&root, &store, None).unwrap();
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].source_id, "sess-fresh");
        assert_eq!(result.stats.skipped_sessions, 0);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn project_key_to_path_decodes_dashes() {
        assert_eq!(project_key_to_path("-tmp-foo"), "/tmp/foo");
        assert_eq!(
            project_key_to_path("-Users-x-git-samzong-Recall"),
            "/Users/x/git/samzong/Recall"
        );
    }
}
