use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use tracing::debug;

use crate::adapters::file_scan::{self, FileScanEntry};
use crate::adapters::{
    RawMessage, RawSession, ResumeCommand, SourceAdapter, SyncScanResult, SyncScanStats,
};
use crate::db::store::Store;
use crate::types::Role;

pub struct ClineAdapter;

impl SourceAdapter for ClineAdapter {
    fn id(&self) -> &str {
        "cline"
    }
    fn label(&self) -> &str {
        "CL"
    }

    fn resume_command(&self, _source_id: &str) -> Option<ResumeCommand> {
        None
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let Some(tasks_dir) = resolve_tasks_dir()? else {
            return Ok(vec![]);
        };
        let mut sessions = Vec::new();
        for entry in collect_cline_entries(&tasks_dir) {
            let Some(mtime_ms) = file_scan::stat_mtime_ms(&entry.stat_target) else {
                continue;
            };
            if let Some(raw) = parse_cline_task(entry, mtime_ms)? {
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
        let Some(tasks_dir) = resolve_tasks_dir()? else {
            return Ok(Some(SyncScanResult { sessions: vec![], stats: SyncScanStats::default() }));
        };
        let result = scan_for_sync_impl(&tasks_dir, store, since_ts)?;
        Ok(Some(result))
    }
}

fn scan_for_sync_impl(
    tasks_dir: &Path,
    store: &Store,
    since_ts: Option<i64>,
) -> anyhow::Result<SyncScanResult> {
    let entries = collect_cline_entries(tasks_dir);
    file_scan::run_file_scan(store, "cline", since_ts, entries, |entry, mtime_ms| {
        parse_cline_task(entry, mtime_ms)
    })
}

fn resolve_tasks_dir() -> anyhow::Result<Option<PathBuf>> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home dir"))?;
    let dir = home
        .join("Library/Application Support/Code/User/globalStorage/saoudrizwan.claude-dev/tasks");
    if !dir.exists() {
        debug!("Cline tasks directory not found, skipping Cline");
        return Ok(None);
    }
    Ok(Some(dir))
}

fn collect_cline_entries(tasks_dir: &Path) -> Vec<FileScanEntry> {
    let mut entries = Vec::new();

    let dir_entries = match fs::read_dir(tasks_dir) {
        Ok(e) => e,
        Err(e) => {
            debug!("cannot read Cline tasks dir: {e}");
            return vec![];
        }
    };

    for entry in dir_entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let dir_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        // Directory name is a timestamp (e.g., "1765706891317")
        let _started_at: i64 = match dir_name.parse() {
            Ok(ts) => ts,
            Err(_) => continue,
        };
        let messages_path = path.join("ui_messages.json");
        if !messages_path.exists() {
            continue;
        }
        entries.push(FileScanEntry {
            session_id: dir_name,
            stat_target: messages_path,
            directory: None,
        });
    }

    entries
}

fn parse_cline_task(entry: FileScanEntry, mtime_ms: i64) -> anyhow::Result<Option<RawSession>> {
    // Directory name is a timestamp, parse it for started_at
    let started_at: i64 = entry.session_id.parse().unwrap_or(0);
    let messages = match load_ui_messages(&entry.stat_target) {
        Ok(m) => m,
        Err(e) => {
            debug!("failed to parse Cline ui_messages {}: {e}", entry.stat_target.display());
            return Ok(None);
        }
    };

    if messages.is_empty() {
        return Ok(None);
    }

    let directory = extract_directory(&entry.stat_target);

    Ok(Some(RawSession::search_only(
        entry.session_id,
        directory,
        started_at,
        Some(mtime_ms),
        None,
        messages,
    )))
}

fn load_ui_messages(path: &Path) -> anyhow::Result<Vec<RawMessage>> {
    let content = fs::read_to_string(path)?;
    let messages: Vec<Value> = serde_json::from_str(&content)?;

    let mut result = Vec::new();
    let mut user_input_seen = false;

    for msg in messages {
        let msg_type = msg.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match msg_type {
            "say" => {
                let say_type = msg.get("say").and_then(|v| v.as_str()).unwrap_or("");
                match say_type {
                    "task" => {
                        // "task" is the user's initial task message
                        if let Some(text) = extract_text(&msg) {
                            let timestamp = msg.get("ts").and_then(|v| v.as_i64());
                            result.push(RawMessage { role: Role::User, content: text, timestamp });
                            user_input_seen = true;
                        }
                    }
                    "text" => {
                        if let Some(text) = extract_text(&msg) {
                            // Skip empty text messages
                            if text.trim().is_empty() {
                                continue;
                            }
                            // First non-empty text message is User input (if not already seen from "task")
                            let role = if !user_input_seen {
                                user_input_seen = true;
                                Role::User
                            } else {
                                Role::Assistant
                            };
                            let timestamp = msg.get("ts").and_then(|v| v.as_i64());
                            result.push(RawMessage { role, content: text, timestamp });
                        }
                    }
                    "user_feedback" => {
                        if let Some(text) = extract_text(&msg) {
                            let timestamp = msg.get("ts").and_then(|v| v.as_i64());
                            result.push(RawMessage { role: Role::User, content: text, timestamp });
                        }
                    }
                    // api_req_started contains the full request (user input + system context),
                    // which is not a meaningful AI reply. Skip it.
                    "api_req_started" => {}
                    "tool" => {
                        if let Some(content) = format_tool_message(&msg) {
                            // Skip empty content messages
                            if content.trim().is_empty() {
                                continue;
                            }
                            let timestamp = msg.get("ts").and_then(|v| v.as_i64());
                            result.push(RawMessage { role: Role::Assistant, content, timestamp });
                        }
                    }
                    "reasoning" | "command" | "completion_result" | "task_progress" => {
                        if let Some(text) = extract_text(&msg) {
                            // Skip empty text messages
                            if text.trim().is_empty() {
                                continue;
                            }
                            let timestamp = msg.get("ts").and_then(|v| v.as_i64());
                            result.push(RawMessage {
                                role: Role::Assistant,
                                content: text,
                                timestamp,
                            });
                        }
                    }
                    _ => {}
                }
            }
            "ask" => {
                // "ask" is AI's response to user (e.g., plan_mode_respond, completion_result)
                if let Some(text) = extract_text(&msg) {
                    let timestamp = msg.get("ts").and_then(|v| v.as_i64());
                    result.push(RawMessage { role: Role::Assistant, content: text, timestamp });
                }
            }
            "question" => {
                // AI asking user a question, e.g., {"type": "question", "question": "请问你xxxx"}
                if let Some(text) =
                    msg.get("question").and_then(|v| v.as_str()).map(|s| s.to_string())
                {
                    let timestamp = msg.get("ts").and_then(|v| v.as_i64());
                    result.push(RawMessage { role: Role::Assistant, content: text, timestamp });
                }
            }
            _ => {}
        }
    }

    Ok(result)
}

fn extract_text(msg: &Value) -> Option<String> {
    msg.get("text").and_then(|v| v.as_str()).map(|s| s.to_string())
}

fn format_tool_message(msg: &Value) -> Option<String> {
    // Try to parse the text as JSON
    let text = extract_text(msg)?;
    let tool_json: Value = serde_json::from_str(&text).ok()?;

    let tool_name = tool_json.get("tool").and_then(|v| v.as_str()).unwrap_or("Unknown");
    let path = tool_json.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let regex = tool_json.get("regex").and_then(|v| v.as_str()).unwrap_or("");
    let file_pattern = tool_json.get("filePattern").and_then(|v| v.as_str()).unwrap_or("");

    // Format based on tool name
    let formatted = match tool_name {
        "readFile" => format!("[ReadFile] {path}"),
        "editedExistingFile" => format!("[EditedFile] {path}"),
        "listFilesTopLevel" => format!("[ListFiles] {path}"),
        "listFilesRecursive" => format!("[ListFilesRecursive] {path}"),
        "searchFiles" => {
            let mut parts = vec![format!("[SearchFiles] {path}")];
            if !regex.is_empty() {
                parts.push(format!("regex: {regex}"));
            }
            if !file_pattern.is_empty() {
                parts.push(format!("pattern: {file_pattern}"));
            }
            parts.join(" - ")
        }
        _ => format!("[{tool_name}] {path}"),
    };

    Some(formatted)
}

fn extract_directory(messages_path: &Path) -> Option<String> {
    let metadata_path = messages_path.parent()?.join("task_metadata.json");
    let content = fs::read_to_string(&metadata_path).ok()?;
    let meta: Value = serde_json::from_str(&content).ok()?;
    let files = meta.get("files_in_context").and_then(|v| v.as_array())?;
    if let Some(first_file) = files.first()
        && let Some(path_str) = first_file.get("path").and_then(|v| v.as_str())
        && let Some(parent) = Path::new(path_str).parent()
    {
        return Some(parent.to_string_lossy().to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{schema, store::Store};
    use crate::types::Session;

    fn setup_store() -> Store {
        schema::register_sqlite_vec();
        Store::open_in_memory().unwrap()
    }

    fn temp_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "recall-cline-test-{}-{}",
            label,
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_task(root: &Path, task_id: &str, messages: &str) -> PathBuf {
        let task_dir = root.join(task_id);
        fs::create_dir_all(&task_dir).unwrap();
        let path = task_dir.join("ui_messages.json");
        fs::write(&path, messages).unwrap();
        path
    }

    fn make_existing_session(source_id: &str, updated_at: i64, message_count: u32) -> Session {
        Session {
            id: format!("internal-{source_id}"),
            source: "cline".to_string(),
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
    fn load_ui_messages_parses_text_and_feedback() {
        let root = temp_root("parse");
        let messages_json = r#"[
            {"ts": 1000, "type": "say", "say": "text", "text": "hello world"},
            {"ts": 2000, "type": "say", "say": "text", "text": "hi there"},
            {"ts": 3000, "type": "say", "say": "user_feedback", "text": "fix it"},
            {"ts": 4000, "type": "say", "say": "tool", "text": "{\"tool\":\"readFile\",\"path\":\"foo.txt\"}"}
        ]"#;
        let path = write_task(&root, "1000", messages_json);

        let msgs = load_ui_messages(&path).unwrap();
        assert_eq!(msgs.len(), 4);
        assert!(matches!(msgs[0].role, Role::User));
        assert_eq!(msgs[0].content, "hello world");
        assert!(matches!(msgs[1].role, Role::Assistant));
        assert_eq!(msgs[1].content, "hi there");
        assert!(matches!(msgs[2].role, Role::User));
        assert_eq!(msgs[2].content, "fix it");
        assert!(matches!(msgs[3].role, Role::Assistant));
        assert_eq!(msgs[3].content, "[ReadFile] foo.txt");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn load_ui_messages_question_type() {
        let root = temp_root("question");
        let messages_json = r#"[
            {"ts": 1000, "type": "say", "say": "task", "text": "do something"},
            {"ts": 2000, "type": "say", "say": "text", "text": "ok, let me check"},
            {"ts": 3000, "type": "question", "question": "请问你要选择哪个方案？"},
            {"ts": 4000, "type": "say", "say": "text", "text": "根据你的选择继续"}
        ]"#;
        let path = write_task(&root, "3000", messages_json);

        let msgs = load_ui_messages(&path).unwrap();
        assert_eq!(msgs.len(), 4);
        // Task is User
        assert!(matches!(msgs[0].role, Role::User));
        assert_eq!(msgs[0].content, "do something");
        // Text after task is Assistant
        assert!(matches!(msgs[1].role, Role::Assistant));
        assert_eq!(msgs[1].content, "ok, let me check");
        // Question is Assistant
        assert!(matches!(msgs[2].role, Role::Assistant));
        assert_eq!(msgs[2].content, "请问你要选择哪个方案？");
        // Text is Assistant
        assert!(matches!(msgs[3].role, Role::Assistant));
        assert_eq!(msgs[3].content, "根据你的选择继续");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn format_tool_message_various_tools() {
        let root = temp_root("tools");
        let messages_json = r#"[
            {"ts": 1000, "type": "say", "say": "task", "text": "test tools"},
            {"ts": 2000, "type": "say", "say": "tool", "text": "{\"tool\":\"readFile\",\"path\":\"src/main.rs\"}"},
            {"ts": 3000, "type": "say", "say": "tool", "text": "{\"tool\":\"editedExistingFile\",\"path\":\"src/main.rs\"}"},
            {"ts": 4000, "type": "say", "say": "tool", "text": "{\"tool\":\"listFilesTopLevel\",\"path\":\"src\"}"},
            {"ts": 5000, "type": "say", "say": "tool", "text": "{\"tool\":\"searchFiles\",\"path\":\"vllm\",\"regex\":\"gelu\",\"filePattern\":\"*.py\"}"}
        ]"#;
        let path = write_task(&root, "1000", messages_json);

        let msgs = load_ui_messages(&path).unwrap();
        assert_eq!(msgs.len(), 5);
        assert_eq!(msgs[1].content, "[ReadFile] src/main.rs");
        assert_eq!(msgs[2].content, "[EditedFile] src/main.rs");
        assert_eq!(msgs[3].content, "[ListFiles] src");
        assert_eq!(msgs[4].content, "[SearchFiles] vllm - regex: gelu - pattern: *.py");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn load_ui_messages_first_text_is_user() {
        let root = temp_root("firstuser");
        let messages_json = r#"[
            {"ts": 1000, "type": "say", "say": "checkpoint_created"},
            {"ts": 2000, "type": "say", "say": "text", "text": "my task"},
            {"ts": 3000, "type": "say", "say": "text", "text": "response"}
        ]"#;
        let path = write_task(&root, "2000", messages_json);

        let msgs = load_ui_messages(&path).unwrap();
        assert_eq!(msgs.len(), 2);
        assert!(matches!(msgs[0].role, Role::User));
        assert!(matches!(msgs[1].role, Role::Assistant));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn load_ui_messages_task_type_is_user() {
        let root = temp_root("tasktype");
        let messages_json = r#"[
            {"ts": 1000, "type": "say", "say": "task", "text": "fix the bug"},
            {"ts": 2000, "type": "say", "say": "checkpoint_created"},
            {"ts": 3000, "type": "say", "say": "api_req_started", "text": "{\"request\":\"<task>fix the bug</task>\"}"},
            {"ts": 4000, "type": "say", "say": "reasoning", "text": "用户想修复bug"},
            {"ts": 5000, "type": "say", "say": "text", "text": "I found the issue"}
        ]"#;
        let path = write_task(&root, "3000", messages_json);

        let msgs = load_ui_messages(&path).unwrap();
        assert_eq!(msgs.len(), 3);
        // First message should be User from "task" type
        assert!(matches!(msgs[0].role, Role::User));
        assert_eq!(msgs[0].content, "fix the bug");
        // Reasoning should be Assistant
        assert!(matches!(msgs[1].role, Role::Assistant));
        // Text after task should be Assistant
        assert!(matches!(msgs[2].role, Role::Assistant));
        assert_eq!(msgs[2].content, "I found the issue");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn collect_cline_entries_skips_non_timestamp_dirs() {
        let root = temp_root("collect");
        let good = root.join("1765706891317");
        fs::create_dir_all(&good).unwrap();
        fs::write(good.join("ui_messages.json"), "[]").unwrap();

        let bad = root.join("not-a-timestamp");
        fs::create_dir_all(&bad).unwrap();
        fs::write(bad.join("ui_messages.json"), "[]").unwrap();

        let entries = collect_cline_entries(&root);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id, "1765706891317");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_cline_task_sets_started_at_from_dir_name() {
        let root = temp_root("startedat");
        let messages_json = r#"[
            {"ts": 1000, "type": "say", "say": "text", "text": "hello"}
        ]"#;
        let path = write_task(&root, "1765706891317", messages_json);
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();

        let entry = FileScanEntry {
            session_id: "1765706891317".to_string(),
            stat_target: path.clone(),
            directory: None,
        };
        let raw = parse_cline_task(entry, mtime).unwrap().unwrap();

        assert_eq!(raw.source_id, "1765706891317");
        assert_eq!(raw.started_at, 1765706891317);
        assert_eq!(raw.updated_at, Some(mtime));
        assert_eq!(raw.messages.len(), 1);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_cline_task_returns_none_for_empty_messages() {
        let root = temp_root("empty");
        let path = write_task(&root, "1000", "[]");
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();

        let entry = FileScanEntry {
            session_id: "1000".to_string(),
            stat_target: path.clone(),
            directory: None,
        };
        let raw = parse_cline_task(entry, mtime).unwrap();
        assert!(raw.is_none());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_cline_task_returns_none_for_invalid_json() {
        let root = temp_root("invalid");
        let task_dir = root.join("1000");
        fs::create_dir_all(&task_dir).unwrap();
        let path = task_dir.join("ui_messages.json");
        fs::write(&path, "not valid json").unwrap();
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();

        let entry = FileScanEntry {
            session_id: "1000".to_string(),
            stat_target: path.clone(),
            directory: None,
        };
        let raw = parse_cline_task(entry, mtime).unwrap();
        assert!(raw.is_none());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_for_sync_skips_unchanged_session() {
        let root = temp_root("skip");
        let messages_json = r#"[
            {"ts": 1000, "type": "say", "say": "text", "text": "hello"}
        ]"#;
        let path = write_task(&root, "1765706891317", messages_json);
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();

        let store = setup_store();
        store.insert_session(&make_existing_session("1765706891317", mtime, 1)).unwrap();

        let result = scan_for_sync_impl(&root, &store, None).unwrap();
        assert_eq!(result.sessions.len(), 0);
        assert_eq!(result.stats.skipped_sessions, 1);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_for_sync_reparses_when_mtime_diverges() {
        let root = temp_root("mismatch");
        let messages_json = r#"[
            {"ts": 1000, "type": "say", "say": "text", "text": "hello"}
        ]"#;
        let path = write_task(&root, "1765706891317", messages_json);
        let actual_mtime = file_scan::stat_mtime_ms(&path).unwrap();

        let store = setup_store();
        store
            .insert_session(&make_existing_session("1765706891317", actual_mtime - 1_000, 1))
            .unwrap();

        let result = scan_for_sync_impl(&root, &store, None).unwrap();
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].source_id, "1765706891317");
        assert_eq!(result.sessions[0].updated_at, Some(actual_mtime));
        assert_eq!(result.stats.skipped_sessions, 0);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_for_sync_picks_up_new_session() {
        let root = temp_root("new");
        let messages_json = r#"[
            {"ts": 1000, "type": "say", "say": "text", "text": "new task"}
        ]"#;
        write_task(&root, "1765706891317", messages_json);

        let store = setup_store();

        let result = scan_for_sync_impl(&root, &store, None).unwrap();
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].source_id, "1765706891317");
        assert_eq!(result.stats.skipped_sessions, 0);

        let _ = fs::remove_dir_all(&root);
    }
}
