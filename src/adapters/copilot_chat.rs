use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use tracing::debug;

use crate::adapters::file_scan::{self, FileScanEntry};
use crate::adapters::json_util::json_i64;
use crate::adapters::{
    RawMessage, RawSession, ResumeCommand, SourceAdapter, SyncScanResult, first_timestamp,
};
use crate::db::store::Store;
use crate::types::Role;

pub(crate) struct CopilotChatAdapter;

const HOSTS: &[&str] = &["Code", "Code - Insiders"];

impl SourceAdapter for CopilotChatAdapter {
    fn id(&self) -> &str {
        "copilot-chat"
    }

    fn label(&self) -> &str {
        "CCH"
    }

    fn resume_command(&self, _source_id: &str) -> Option<ResumeCommand> {
        None
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let mut sessions = Vec::new();
        for entry in collect_chat_entries(&vscode_user_roots()) {
            let Some(mtime_ms) = file_scan::stat_mtime_ms(&entry.stat_target) else {
                continue;
            };
            if let Some(raw) = parse_chat_session_for_entry(entry, mtime_ms)? {
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
        let entries = collect_chat_entries(&vscode_user_roots());
        let result = file_scan::run_file_scan(
            store,
            "copilot-chat",
            since_ts,
            entries,
            parse_chat_session_for_entry,
        )?;
        Ok(Some(result))
    }
}

fn vscode_user_roots() -> Vec<PathBuf> {
    let Some(config) = dirs::config_dir() else {
        return Vec::new();
    };
    HOSTS.iter().map(|host| config.join(host).join("User")).filter(|path| path.is_dir()).collect()
}

fn collect_chat_entries(user_roots: &[PathBuf]) -> Vec<FileScanEntry> {
    let mut by_id: HashMap<String, FileScanEntry> = HashMap::new();
    for user_root in user_roots {
        collect_empty_window_entries(user_root, &mut by_id);
        collect_workspace_entries(user_root, &mut by_id);
    }
    by_id.into_values().collect()
}

fn collect_empty_window_entries(user_root: &Path, by_id: &mut HashMap<String, FileScanEntry>) {
    let dir = user_root.join("globalStorage").join("emptyWindowChatSessions");
    collect_session_files(&dir, None, by_id);
}

fn collect_workspace_entries(user_root: &Path, by_id: &mut HashMap<String, FileScanEntry>) {
    let storage = user_root.join("workspaceStorage");
    let read = match fs::read_dir(&storage) {
        Ok(read) => read,
        Err(error) => {
            debug!("cannot read {}: {error}", storage.display());
            return;
        }
    };
    for entry in read.flatten() {
        let workspace_dir = entry.path();
        if !workspace_dir.is_dir() {
            continue;
        }
        let directory = workspace_folder(&workspace_dir.join("workspace.json"));
        collect_session_files(&workspace_dir.join("chatSessions"), directory, by_id);
    }
}

fn collect_session_files(
    dir: &Path,
    directory: Option<String>,
    by_id: &mut HashMap<String, FileScanEntry>,
) {
    let read = match fs::read_dir(dir) {
        Ok(read) => read,
        Err(_) => return,
    };
    for entry in read.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let ext = path.extension().and_then(|ext| ext.to_str());
        if ext != Some("jsonl") && ext != Some("json") {
            continue;
        }
        let session_id = match path.file_stem().and_then(|name| name.to_str()) {
            Some(name) if !name.is_empty() => name.to_string(),
            _ => continue,
        };
        match by_id.get(&session_id) {
            Some(existing)
                if existing.stat_target.extension().and_then(|ext| ext.to_str())
                    == Some("jsonl")
                    && ext == Some("json") =>
            {
                continue;
            }
            _ => {}
        }
        by_id.insert(
            session_id.clone(),
            FileScanEntry { session_id, stat_target: path, directory: directory.clone() },
        );
    }
}

fn workspace_folder(path: &Path) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&content).ok()?;
    file_uri_to_path(value.get("folder").and_then(|folder| folder.as_str())?)
}

fn file_uri_to_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let decoded = percent_decode(rest)?;
    if cfg!(windows)
        && let Some(drive) = decoded.strip_prefix('/')
        && drive.len() >= 2
        && drive.as_bytes()[1] == b':'
    {
        return Some(drive.to_string());
    }
    Some(decoded)
}

fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return None;
            }
            let hi = from_hex(bytes[index + 1])?;
            let lo = from_hex(bytes[index + 2])?;
            out.push((hi << 4) | lo);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn from_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn parse_chat_session_for_entry(
    entry: FileScanEntry,
    mtime_ms: i64,
) -> anyhow::Result<Option<RawSession>> {
    let source_file_path = entry.stat_target.to_str().map(str::to_string);
    let Some(doc) = load_chat_document(&entry.stat_target) else {
        return Ok(None);
    };
    let parsed = messages_from_doc(&doc);
    if parsed.messages.is_empty() {
        return Ok(None);
    }
    let started_at = first_timestamp(parsed.started_at, &parsed.messages, &[], &[]).unwrap_or(0);
    let updated_at = Some(mtime_ms);
    let mut session = RawSession::search_only(
        entry.session_id,
        entry.directory,
        started_at,
        updated_at,
        None,
        parsed.messages,
    );
    session.source_file_path = source_file_path;
    session.custom_title = parsed.custom_title;
    Ok(Some(session))
}

fn load_chat_document(path: &Path) -> Option<Value> {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("jsonl") => replay_chat_jsonl(path),
        Some("json") => match fs::read_to_string(path) {
            Ok(content) => match serde_json::from_str(&content) {
                Ok(value) => Some(value),
                Err(error) => {
                    debug!("failed to parse copilot chat session {}: {error}", path.display());
                    None
                }
            },
            Err(error) => {
                debug!("failed to read copilot chat session {}: {error}", path.display());
                None
            }
        },
        _ => None,
    }
}

fn replay_chat_jsonl(path: &Path) -> Option<Value> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) => {
            debug!("failed to read copilot chat session {}: {error}", path.display());
            return None;
        }
    };
    let mut doc = None;
    for line in BufReader::new(file).lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => break,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let event: Value = match serde_json::from_str(line) {
            Ok(event) => event,
            Err(_) => break,
        };
        match json_i64(event.get("kind")).unwrap_or(-1) {
            0 => doc = event.get("v").cloned(),
            1 => {
                if let Some(root) = doc.as_mut() {
                    apply_set(root, event.get("k"), event.get("v").cloned().unwrap_or(Value::Null));
                }
            }
            2 => {
                if let Some(root) = doc.as_mut() {
                    apply_delete(root, event.get("k"));
                }
            }
            _ => {}
        }
    }
    doc
}

enum PathSeg {
    Key(String),
    Index(usize),
}

fn path_segs(path: Option<&Value>) -> Option<Vec<PathSeg>> {
    let segments = path?.as_array()?;
    let mut out = Vec::with_capacity(segments.len());
    for segment in segments {
        if let Some(key) = segment.as_str() {
            out.push(PathSeg::Key(key.to_string()));
        } else {
            let index = json_i64(Some(segment)).filter(|index| *index >= 0)?;
            out.push(PathSeg::Index(index as usize));
        }
    }
    Some(out)
}

fn apply_set(root: &mut Value, path: Option<&Value>, value: Value) {
    let Some(segs) = path_segs(path) else {
        return;
    };
    if segs.is_empty() {
        *root = value;
        return;
    }
    let mut current = root;
    for index in 0..segs.len() - 1 {
        current = match get_or_create(current, &segs[index], Some(&segs[index + 1])) {
            Some(next) => next,
            None => return,
        };
    }
    if let Some(last) = segs.last() {
        assign_last(current, last, value);
    }
}

fn apply_delete(root: &mut Value, path: Option<&Value>) {
    let Some(segs) = path_segs(path) else {
        return;
    };
    if segs.is_empty() {
        return;
    }
    let mut current = root;
    for seg in segs.iter().take(segs.len() - 1) {
        current = match descend_existing(current, seg) {
            Some(next) => next,
            None => return,
        };
    }
    match segs.last() {
        Some(PathSeg::Key(key)) => {
            if let Some(object) = current.as_object_mut() {
                object.remove(key);
            }
        }
        Some(PathSeg::Index(index)) => {
            if let Some(array) = current.as_array_mut()
                && *index < array.len()
            {
                array.remove(*index);
            }
        }
        None => {}
    }
}

fn get_or_create<'a>(
    parent: &'a mut Value,
    seg: &PathSeg,
    next: Option<&PathSeg>,
) -> Option<&'a mut Value> {
    let init = match next {
        Some(PathSeg::Index(_)) => Value::Array(Vec::new()),
        _ => Value::Object(Map::new()),
    };
    match seg {
        PathSeg::Key(key) => {
            if !parent.is_object() {
                *parent = Value::Object(Map::new());
            }
            let object = parent.as_object_mut()?;
            if !object.contains_key(key) {
                object.insert(key.clone(), init);
            }
            object.get_mut(key)
        }
        PathSeg::Index(index) => {
            if !parent.is_array() {
                *parent = Value::Array(Vec::new());
            }
            let array = parent.as_array_mut()?;
            while array.len() <= *index {
                array.push(if array.len() == *index { init.clone() } else { Value::Null });
            }
            if array[*index].is_null() {
                array[*index] = init;
            }
            array.get_mut(*index)
        }
    }
}

fn descend_existing<'a>(parent: &'a mut Value, seg: &PathSeg) -> Option<&'a mut Value> {
    match seg {
        PathSeg::Key(key) => parent.as_object_mut()?.get_mut(key),
        PathSeg::Index(index) => parent.as_array_mut()?.get_mut(*index),
    }
}

fn assign_last(parent: &mut Value, seg: &PathSeg, value: Value) {
    match seg {
        PathSeg::Key(key) => {
            if !parent.is_object() {
                *parent = Value::Object(Map::new());
            }
            if let Some(object) = parent.as_object_mut() {
                object.insert(key.clone(), value);
            }
        }
        PathSeg::Index(index) => {
            if !parent.is_array() {
                *parent = Value::Array(Vec::new());
            }
            if let Some(array) = parent.as_array_mut() {
                while array.len() <= *index {
                    array.push(Value::Null);
                }
                array[*index] = value;
            }
        }
    }
}

struct ParsedChat {
    messages: Vec<RawMessage>,
    custom_title: Option<String>,
    started_at: Option<i64>,
}

fn messages_from_doc(doc: &Value) -> ParsedChat {
    let custom_title = doc
        .get("customTitle")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let started_at = json_i64(doc.get("creationDate"));
    let mut messages = Vec::new();
    let Some(requests) = doc.get("requests").and_then(|value| value.as_array()) else {
        return ParsedChat { messages, custom_title, started_at };
    };
    for request in requests {
        if request.get("hiddenFromTranscript").and_then(|value| value.as_bool()) == Some(true) {
            continue;
        }
        let timestamp = json_i64(request.get("timestamp"))
            .or_else(|| json_i64(request.get("responseTimestamp")));
        if let Some(text) = request
            .get("message")
            .and_then(|message| message.get("text"))
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            messages.push(RawMessage { role: Role::User, content: text.to_string(), timestamp });
        }
        let assistant = response_text(request.get("response"));
        if !assistant.is_empty() {
            messages.push(RawMessage { role: Role::Assistant, content: assistant, timestamp });
        }
    }
    ParsedChat { messages, custom_title, started_at }
}

fn response_text(response: Option<&Value>) -> String {
    let Some(items) = response.and_then(|value| value.as_array()) else {
        return String::new();
    };
    let mut parts = Vec::new();
    for item in items {
        let Some(object) = item.as_object() else {
            continue;
        };
        if object.get("kind").and_then(|value| value.as_str()).is_some() {
            continue;
        }
        if let Some(text) = object
            .get("value")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            parts.push(text.to_string());
        }
    }
    parts.join("\n")
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
            "recall-cch-test-{}-{}",
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
        fs::write(path, lines.join("\n") + "\n").unwrap();
    }

    fn sample_jsonl_lines() -> [&'static str; 4] {
        [
            r#"{"kind":0,"v":{"version":3,"creationDate":1700000000000,"customTitle":"Fix login","sessionId":"sess-1","requests":[],"lastMessageDate":1700000005000}}"#,
            r#"{"kind":1,"k":["requests",0],"v":{"requestId":"r1","timestamp":1700000001000,"message":{"text":"hello"},"response":[]}}"#,
            r#"{"kind":1,"k":["requests",0,"response"],"v":[{"kind":"thinking","value":"plan"},{"value":"Hi there."}]}"#,
            r#"{"kind":1,"k":["responderUsername"],"v":"GitHub Copilot"}"#,
        ]
    }

    #[test]
    fn parse_jsonl_replays_patches_and_skips_thinking() {
        let root = temp_root("jsonl");
        let path = root.join("sess-1.jsonl");
        write_jsonl(&path, &sample_jsonl_lines());

        let session = parse_chat_session_for_entry(
            FileScanEntry {
                session_id: "sess-1".to_string(),
                stat_target: path,
                directory: Some("/tmp/proj".to_string()),
            },
            99,
        )
        .unwrap()
        .unwrap();

        assert_eq!(session.source_id, "sess-1");
        assert_eq!(session.directory.as_deref(), Some("/tmp/proj"));
        assert_eq!(session.custom_title.as_deref(), Some("Fix login"));
        assert_eq!(session.started_at, 1_700_000_000_000);
        assert_eq!(session.updated_at, Some(99));
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, Role::User);
        assert_eq!(session.messages[0].content, "hello");
        assert_eq!(session.messages[1].role, Role::Assistant);
        assert_eq!(session.messages[1].content, "Hi there.");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_json_snapshot_extracts_requests() {
        let root = temp_root("json");
        let path = root.join("sess-2.json");
        fs::write(
            &path,
            r#"{"version":3,"sessionId":"sess-2","creationDate":1700000000000,"customTitle":"Old chat","requests":[{"timestamp":1700000001000,"message":{"text":"translate this"},"response":[{"kind":"toolInvocationSerialized","value":"read"},{"value":"done"}]}]}"#,
        )
        .unwrap();

        let session = parse_chat_session_for_entry(
            FileScanEntry { session_id: "sess-2".to_string(), stat_target: path, directory: None },
            1,
        )
        .unwrap()
        .unwrap();

        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].content, "translate this");
        assert_eq!(session.messages[1].content, "done");
        assert_eq!(session.custom_title.as_deref(), Some("Old chat"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn empty_requests_are_skipped() {
        let root = temp_root("empty");
        let path = root.join("empty.jsonl");
        write_jsonl(&path, &[r#"{"kind":0,"v":{"version":3,"sessionId":"empty","requests":[]}}"#]);
        let session = parse_chat_session_for_entry(
            FileScanEntry { session_id: "empty".to_string(), stat_target: path, directory: None },
            1,
        )
        .unwrap();
        assert!(session.is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn truncated_jsonl_keeps_replayed_prefix() {
        let root = temp_root("trunc");
        let path = root.join("trunc.jsonl");
        write_jsonl(
            &path,
            &[
                r#"{"kind":0,"v":{"version":3,"creationDate":1,"requests":[{"timestamp":2,"message":{"text":"keep"},"response":[{"value":"ok"}]}]}}"#,
                r#"{"kind":1,"k":["requests",0,"response""#,
            ],
        );
        let session = parse_chat_session_for_entry(
            FileScanEntry { session_id: "trunc".to_string(), stat_target: path, directory: None },
            1,
        )
        .unwrap()
        .unwrap();
        assert_eq!(session.messages[0].content, "keep");
        assert_eq!(session.messages[1].content, "ok");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn collect_prefers_jsonl_over_json_and_reads_workspace_folder() {
        let root = temp_root("collect");
        let user = root.join("User");
        let ws = user.join("workspaceStorage").join("abc");
        fs::create_dir_all(ws.join("chatSessions")).unwrap();
        fs::write(ws.join("workspace.json"), r#"{"folder":"file:///Users/x/git/foo%20bar"}"#)
            .unwrap();
        write_jsonl(&ws.join("chatSessions").join("sess-1.jsonl"), &sample_jsonl_lines());
        fs::write(ws.join("chatSessions").join("sess-1.json"), r#"{"requests":[]}"#).unwrap();
        fs::create_dir_all(user.join("globalStorage").join("emptyWindowChatSessions")).unwrap();
        write_jsonl(
            &user.join("globalStorage").join("emptyWindowChatSessions").join("empty.jsonl"),
            &[r#"{"kind":0,"v":{"version":3,"requests":[]}}"#],
        );

        let entries = collect_chat_entries(&[user]);
        assert_eq!(entries.len(), 2);
        let sess = entries.iter().find(|entry| entry.session_id == "sess-1").unwrap();
        assert!(sess.stat_target.extension().unwrap() == "jsonl");
        assert_eq!(sess.directory.as_deref(), Some("/Users/x/git/foo bar"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn file_uri_to_path_decodes_and_keeps_posix_absolute() {
        assert_eq!(
            file_uri_to_path("file:///Users/x/git/foo%20bar").as_deref(),
            Some("/Users/x/git/foo bar")
        );
        assert_eq!(file_uri_to_path("https://example.com"), None);
    }

    #[test]
    fn hidden_requests_are_skipped() {
        let doc = serde_json::json!({
            "creationDate": 1,
            "requests": [{
                "hiddenFromTranscript": true,
                "timestamp": 2,
                "message": {"text": "secret"},
                "response": [{"value": "nope"}]
            }, {
                "timestamp": 3,
                "message": {"text": "visible"},
                "response": [{"value": "ok"}]
            }]
        });
        let parsed = messages_from_doc(&doc);
        assert_eq!(parsed.messages.len(), 2);
        assert_eq!(parsed.messages[0].content, "visible");
    }

    #[test]
    fn scan_for_sync_skips_unchanged_session() {
        let root = temp_root("skip");
        let user = root.join("User");
        let path = user.join("globalStorage").join("emptyWindowChatSessions").join("sess-1.jsonl");
        write_jsonl(&path, &sample_jsonl_lines());
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();

        let store = setup_store();
        store
            .insert_session(&Session {
                id: "internal-sess-1".to_string(),
                source: "copilot-chat".to_string(),
                source_id: "sess-1".to_string(),
                title: "existing".to_string(),
                directory: None,
                repo_remote: None,
                repo_slug: None,
                repo_name: None,
                started_at: 0,
                updated_at: Some(mtime),
                message_count: 2,
                entrypoint: None,
                custom_title: None,
                summary: None,
                duration_minutes: None,
                source_file_path: None,
                is_import: false,
            })
            .unwrap();

        let entries = collect_chat_entries(&[user]);
        let result = file_scan::run_file_scan(
            &store,
            "copilot-chat",
            None,
            entries,
            parse_chat_session_for_entry,
        )
        .unwrap();
        assert_eq!(result.sessions.len(), 0);
        assert_eq!(result.stats.skipped_sessions, 1);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_for_sync_reparses_when_same_turn_response_grows() {
        use std::time::{Duration, UNIX_EPOCH};

        let root = temp_root("same-turn");
        let user = root.join("User");
        let path = user.join("globalStorage").join("emptyWindowChatSessions").join("sess-1.jsonl");
        write_jsonl(&path, &sample_jsonl_lines());
        let first_mtime = 1_700_000_000_000i64;
        fs::File::open(&path)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_millis(first_mtime as u64))
            .unwrap();

        let store = setup_store();
        store
            .insert_session(&Session {
                id: "internal-sess-1".to_string(),
                source: "copilot-chat".to_string(),
                source_id: "sess-1".to_string(),
                title: "existing".to_string(),
                directory: None,
                repo_remote: None,
                repo_slug: None,
                repo_name: None,
                started_at: 0,
                updated_at: Some(first_mtime),
                message_count: 2,
                entrypoint: None,
                custom_title: None,
                summary: None,
                duration_minutes: None,
                source_file_path: None,
                is_import: false,
            })
            .unwrap();

        let later_mtime = first_mtime + 5_000;
        write_jsonl(
            &path,
            &[
                sample_jsonl_lines()[0],
                sample_jsonl_lines()[1],
                r#"{"kind":1,"k":["requests",0,"response"],"v":[{"kind":"thinking","value":"plan"},{"value":"Hi there. Full answer."}]}"#,
                sample_jsonl_lines()[3],
            ],
        );
        fs::File::open(&path)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_millis(later_mtime as u64))
            .unwrap();

        let entries = collect_chat_entries(&[user]);
        let result = file_scan::run_file_scan(
            &store,
            "copilot-chat",
            None,
            entries,
            parse_chat_session_for_entry,
        )
        .unwrap();
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].updated_at, Some(later_mtime));
        assert_eq!(result.sessions[0].messages.len(), 2);
        assert_eq!(result.sessions[0].messages[1].content, "Hi there. Full answer.");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn delete_patch_removes_object_key() {
        let mut doc = serde_json::json!({"pendingRequests":[{"id":1}],"requests":[]});
        apply_delete(&mut doc, Some(&serde_json::json!(["pendingRequests"])));
        assert!(doc.get("pendingRequests").is_none());
    }
}
