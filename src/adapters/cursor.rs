use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use tracing::debug;
use walkdir::WalkDir;

use crate::adapters::file_scan::{self, FileScanEntry};
use crate::adapters::{
    RawMessage, RawSession, ResumeCommand, SourceAdapter, SyncScanResult, SyncScanStats,
};
use crate::db::store::Store;
use crate::types::Role;

pub struct CursorAdapter;

impl SourceAdapter for CursorAdapter {
    fn id(&self) -> &str {
        "cursor"
    }
    fn label(&self) -> &str {
        "CUR"
    }

    fn resume_command(&self, _source_id: &str) -> Option<ResumeCommand> {
        None
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let Some(projects_dir) = resolve_projects_dir()? else {
            return Ok(vec![]);
        };
        let cwd_map = build_cwd_map(resolve_state_db_path().as_deref());
        let mut sessions = Vec::new();
        for entry in collect_cursor_entries(&projects_dir) {
            let Some(mtime_ms) = file_scan::stat_mtime_ms(&entry.stat_target) else {
                continue;
            };
            if let Some(raw) = parse_cursor_session_for_entry(entry, mtime_ms, &cwd_map)? {
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
        let Some(projects_dir) = resolve_projects_dir()? else {
            return Ok(Some(SyncScanResult { sessions: vec![], stats: SyncScanStats::default() }));
        };
        let cwd_map = build_cwd_map(resolve_state_db_path().as_deref());
        let entries = collect_cursor_entries(&projects_dir);
        let result =
            file_scan::run_file_scan(store, "cursor", since_ts, entries, |entry, mtime_ms| {
                parse_cursor_session_for_entry(entry, mtime_ms, &cwd_map)
            })?;
        Ok(Some(result))
    }
}

fn resolve_projects_dir() -> anyhow::Result<Option<PathBuf>> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home dir"))?;
    let dir = home.join(".cursor").join("projects");
    if !dir.exists() {
        debug!("~/.cursor/projects not found, skipping Cursor");
        return Ok(None);
    }
    Ok(Some(dir))
}

fn resolve_state_db_path() -> Option<PathBuf> {
    let db = dirs::config_dir()?.join("Cursor/User/globalStorage/state.vscdb");
    if db.exists() { Some(db) } else { None }
}

fn collect_cursor_entries(projects_dir: &Path) -> Vec<FileScanEntry> {
    let mut entries = Vec::new();
    for walk_entry in WalkDir::new(projects_dir).into_iter().filter_map(|e| e.ok()) {
        let path = walk_entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if uuid::Uuid::try_parse(stem).is_err() {
            continue;
        }
        let parent_name = path.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str());
        if parent_name != Some(stem) {
            continue;
        }
        let grandparent_name = path
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str());
        if grandparent_name != Some("agent-transcripts") {
            continue;
        }
        entries.push(FileScanEntry {
            session_id: stem.to_string(),
            stat_target: path.to_path_buf(),
            directory: None,
        });
    }
    entries
}

fn parse_cursor_session_for_entry(
    entry: FileScanEntry,
    mtime_ms: i64,
    cwd_map: &HashMap<String, String>,
) -> anyhow::Result<Option<RawSession>> {
    let path = &entry.stat_target;
    let mut raw = match parse_cursor_session(path) {
        Ok(Some(raw)) => raw,
        Ok(None) => return Ok(None),
        Err(e) => {
            debug!("failed to parse cursor session {}: {e}", path.display());
            return Ok(None);
        }
    };
    raw.directory = cwd_map.get(&entry.session_id).cloned();
    raw.started_at = stat_birth_ms(path).unwrap_or(mtime_ms);
    raw.updated_at = Some(mtime_ms);
    raw.source_id = entry.session_id;
    Ok(Some(raw))
}

fn parse_cursor_session(path: &Path) -> anyhow::Result<Option<RawSession>> {
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
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
        let role = match v.get("role").and_then(|r| r.as_str()) {
            Some("user") => Role::User,
            Some("assistant") => Role::Assistant,
            _ => continue,
        };
        let content_array =
            v.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_array());
        let Some(items) = content_array else {
            continue;
        };
        let is_user = matches!(role, Role::User);
        let content = render_content_items(items, is_user);
        if content.is_empty() {
            continue;
        }
        messages.push(RawMessage { role, content, timestamp: None });
    }

    if messages.is_empty() {
        return Ok(None);
    }

    Ok(Some(RawSession {
        source_id: String::new(),
        directory: None,
        started_at: 0,
        updated_at: None,
        entrypoint: None,
        messages,
    }))
}

fn render_content_items(items: &[Value], is_user: bool) -> String {
    let mut parts = Vec::new();
    for item in items {
        match item.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                let Some(text) = item.get("text").and_then(|t| t.as_str()) else {
                    continue;
                };
                let normalized = if is_user { strip_user_query_envelope(text) } else { text };
                let trimmed = normalized.trim();
                if !trimmed.is_empty() {
                    parts.push(trimmed.to_string());
                }
            }
            Some("tool_use") => {
                let name = item.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                let rendered_input = match item.get("input") {
                    Some(Value::String(s)) => s.clone(),
                    Some(v) => serde_json::to_string(v).unwrap_or_default(),
                    None => String::new(),
                };
                parts.push(format!("[tool_use:{name}] {rendered_input}"));
            }
            _ => {}
        }
    }
    parts.join("\n")
}

fn strip_user_query_envelope(text: &str) -> &str {
    const OPEN: &str = "<user_query>";
    const CLOSE: &str = "</user_query>";
    let trimmed = text.trim();
    if let Some(inner) = trimmed.strip_prefix(OPEN).and_then(|s| s.strip_suffix(CLOSE)) {
        inner
    } else {
        text
    }
}

fn stat_birth_ms(path: &Path) -> Option<i64> {
    let meta = fs::metadata(path).ok()?;
    let created = meta.created().ok()?;
    let duration = created.duration_since(UNIX_EPOCH).ok()?;
    Some(duration.as_millis() as i64)
}

fn build_cwd_map(db_path: Option<&Path>) -> HashMap<String, String> {
    let Some(db_path) = db_path else {
        return HashMap::new();
    };
    match read_cwd_map(db_path) {
        Ok(map) => map,
        Err(e) => {
            debug!("cursor cwd map unavailable from {}: {e}", db_path.display());
            HashMap::new()
        }
    }
}

fn read_cwd_map(db_path: &Path) -> anyhow::Result<HashMap<String, String>> {
    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let mut project_to_path: HashMap<String, String> = HashMap::new();
    if let Some(projects_json) = read_item_value(&conn, "glass.localAgentProjects.v1")
        && let Ok(projects) = serde_json::from_str::<Value>(&projects_json)
        && let Some(arr) = projects.as_array()
    {
        for item in arr {
            let project_id = match item.get("id").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let fs_path = item
                .get("workspace")
                .and_then(|w| w.get("uri"))
                .and_then(|u| u.get("fsPath"))
                .and_then(|p| p.as_str());
            if let Some(fs_path) = fs_path {
                project_to_path.insert(project_id, fs_path.to_string());
            }
        }
    }

    let mut session_to_path: HashMap<String, String> = HashMap::new();
    if let Some(membership_json) = read_item_value(&conn, "glass.localAgentProjectMembership.v1")
        && let Ok(membership) = serde_json::from_str::<Value>(&membership_json)
        && let Some(obj) = membership.as_object()
    {
        for (session_id, project_val) in obj {
            let Some(project_id) = project_val.as_str() else {
                continue;
            };
            if let Some(path) = project_to_path.get(project_id) {
                session_to_path.insert(session_id.clone(), path.clone());
            }
        }
    }
    Ok(session_to_path)
}

fn read_item_value(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM ItemTable WHERE key = ?1", [key], |row| {
        row.get::<_, String>(0)
    })
    .ok()
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn temp_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "recall-cursor-test-{}-{}",
            label,
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_jsonl(path: &Path, lines: &[&str]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut f = fs::File::create(path).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
    }

    #[test]
    fn strip_user_query_envelope_strips_wrapper() {
        let text = "<user_query>\nhello world\n</user_query>";
        assert_eq!(strip_user_query_envelope(text).trim(), "hello world");
    }

    #[test]
    fn strip_user_query_envelope_keeps_text_without_wrapper() {
        let text = "plain user input";
        assert_eq!(strip_user_query_envelope(text), "plain user input");
    }

    #[test]
    fn strip_user_query_envelope_ignores_partial_wrapper() {
        let text = "<user_query>\nhalf only";
        assert_eq!(strip_user_query_envelope(text), text);
    }

    #[test]
    fn render_content_items_strips_envelope_for_user_only() {
        let items = vec![serde_json::json!({
            "type": "text",
            "text": "<user_query>\nfix the bug\n</user_query>"
        })];
        assert_eq!(render_content_items(&items, true), "fix the bug");
        assert_eq!(render_content_items(&items, false), "<user_query>\nfix the bug\n</user_query>");
    }

    #[test]
    fn render_content_items_leaves_xml_inside_assistant_text() {
        let items = vec![serde_json::json!({
            "type": "text",
            "text": "saw a <tool> tag in code"
        })];
        assert_eq!(render_content_items(&items, false), "saw a <tool> tag in code");
    }

    #[test]
    fn render_content_items_serializes_object_tool_input() {
        let items = vec![serde_json::json!({
            "type": "tool_use",
            "name": "Glob",
            "input": {"glob_pattern": "*.rs"}
        })];
        let rendered = render_content_items(&items, false);
        assert!(rendered.starts_with("[tool_use:Glob] "));
        assert!(rendered.contains("\"glob_pattern\""));
        assert!(rendered.contains("*.rs"));
    }

    #[test]
    fn render_content_items_passes_through_string_tool_input() {
        let items = vec![serde_json::json!({
            "type": "tool_use",
            "name": "ApplyPatch",
            "input": "*** Begin Patch\nsome diff\n*** End Patch"
        })];
        let rendered = render_content_items(&items, false);
        assert_eq!(rendered, "[tool_use:ApplyPatch] *** Begin Patch\nsome diff\n*** End Patch");
    }

    #[test]
    fn parse_cursor_session_happy_path() {
        let root = temp_root("parse");
        let uuid = uuid::Uuid::new_v4().to_string();
        let jsonl_path = root.join(&uuid).join(format!("{uuid}.jsonl"));
        write_jsonl(
            &jsonl_path,
            &[
                r#"{"role":"user","message":{"content":[{"type":"text","text":"<user_query>\nhello\n</user_query>"}]}}"#,
                r#"{"role":"assistant","message":{"content":[{"type":"text","text":"hi"},{"type":"tool_use","name":"Glob","input":{"glob_pattern":"*.rs"}}]}}"#,
            ],
        );
        let raw = parse_cursor_session(&jsonl_path).unwrap().unwrap();
        assert_eq!(raw.messages.len(), 2);
        assert!(matches!(raw.messages[0].role, Role::User));
        assert_eq!(raw.messages[0].content, "hello");
        assert!(matches!(raw.messages[1].role, Role::Assistant));
        assert!(raw.messages[1].content.contains("hi"));
        assert!(raw.messages[1].content.contains("[tool_use:Glob]"));
        assert!(raw.messages[0].timestamp.is_none());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_cursor_session_skips_unknown_roles_and_malformed_lines() {
        let root = temp_root("skip");
        let uuid = uuid::Uuid::new_v4().to_string();
        let jsonl_path = root.join(&uuid).join(format!("{uuid}.jsonl"));
        write_jsonl(
            &jsonl_path,
            &[
                "not json",
                r#"{"role":"system","message":{"content":[{"type":"text","text":"sys"}]}}"#,
                r#"{"role":"user","message":{"content":[{"type":"text","text":"real"}]}}"#,
            ],
        );
        let raw = parse_cursor_session(&jsonl_path).unwrap().unwrap();
        assert_eq!(raw.messages.len(), 1);
        assert_eq!(raw.messages[0].content, "real");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_cursor_session_returns_none_for_no_messages() {
        let root = temp_root("empty");
        let uuid = uuid::Uuid::new_v4().to_string();
        let jsonl_path = root.join(&uuid).join(format!("{uuid}.jsonl"));
        write_jsonl(&jsonl_path, &[""]);
        let raw = parse_cursor_session(&jsonl_path).unwrap();
        assert!(raw.is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn collect_cursor_entries_requires_agent_transcripts_parent() {
        let root = temp_root("collect");
        let projects = root.join("projects");
        let uuid = uuid::Uuid::new_v4().to_string();

        let good = projects.join("proj-a").join("agent-transcripts").join(&uuid);
        write_jsonl(&good.join(format!("{uuid}.jsonl")), &["{}"]);

        let bad = projects.join("proj-a").join("other-dir").join(&uuid);
        write_jsonl(&bad.join(format!("{uuid}.jsonl")), &["{}"]);

        let not_uuid_dir = projects.join("proj-a").join("agent-transcripts").join("notauuid");
        write_jsonl(&not_uuid_dir.join("notauuid.jsonl"), &["{}"]);

        let entries = collect_cursor_entries(&projects);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id, uuid);
        let _ = fs::remove_dir_all(&root);
    }

    fn seed_state_db(path: &Path, session_uuid: &str, fs_path: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let conn = Connection::open(path).unwrap();
        conn.execute("CREATE TABLE IF NOT EXISTS ItemTable (key TEXT PRIMARY KEY, value TEXT)", [])
            .unwrap();

        let membership = serde_json::json!({ session_uuid: "project-1" });
        let projects = serde_json::json!([{
            "id": "project-1",
            "workspace": { "uri": { "fsPath": fs_path } }
        }]);
        conn.execute(
            "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
            ["glass.localAgentProjectMembership.v1", &membership.to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
            ["glass.localAgentProjects.v1", &projects.to_string()],
        )
        .unwrap();
    }

    #[test]
    fn read_cwd_map_resolves_session_to_fspath() {
        let root = temp_root("cwdmap");
        let db_path = root.join("state.vscdb");
        let session_uuid = uuid::Uuid::new_v4().to_string();
        seed_state_db(&db_path, &session_uuid, "/Users/x/git/samzong/Recall");

        let map = read_cwd_map(&db_path).unwrap();
        assert_eq!(map.get(&session_uuid).map(String::as_str), Some("/Users/x/git/samzong/Recall"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn build_cwd_map_returns_empty_when_db_missing() {
        let map = build_cwd_map(None);
        assert!(map.is_empty());
    }
}
