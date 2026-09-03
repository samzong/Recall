use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use tracing::{debug, warn};

use crate::adapters::AdapterSyncContext;
use crate::adapters::json_util::{json_i64, rfc3339_ms};
use crate::adapters::paths::vscode_extension_storage_dirs_from;
use crate::adapters::{
    RawMessage, RawSession, ResumeCommand, SourceAdapter, SyncScanResult, SyncScanStats,
    first_timestamp, last_timestamp,
};
use crate::types::Role;

const SOURCE: &str = "amp";
const EXTENSION_ID: &str = "sourcegraph.amp";
const MAX_THREAD_BYTES: u64 = 8 * 1024 * 1024;
const SKIP_NAMES: &[&str] = &["config.json", "settings.json", "secrets.json"];

pub(crate) struct AmpAdapter;

impl SourceAdapter for AmpAdapter {
    fn id(&self) -> &str {
        SOURCE
    }

    fn label(&self) -> &str {
        "AM"
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        Some(ResumeCommand {
            program: "amp".to_string(),
            args: vec!["threads".to_string(), "continue".to_string(), source_id.to_string()],
        })
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        Ok(scan_threads(&thread_roots(), None)?.sessions)
    }

    fn scan_for_sync(
        &self,
        _context: &AdapterSyncContext,
        since_ts: Option<i64>,
        _include_events: bool,
    ) -> anyhow::Result<Option<SyncScanResult>> {
        Ok(Some(scan_threads(&thread_roots(), since_ts)?))
    }
}

fn thread_roots() -> Vec<PathBuf> {
    thread_roots_from(
        std::env::var("AMP_THREADS_DIR").ok(),
        std::env::var("XDG_DATA_HOME").ok(),
        dirs::home_dir(),
        dirs::config_dir(),
    )
}

fn thread_roots_from(
    amp_threads_dir: Option<String>,
    xdg_data_home: Option<String>,
    home: Option<PathBuf>,
    config_dir: Option<PathBuf>,
) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(dir) = amp_threads_dir.as_deref().map(str::trim).filter(|value| !value.is_empty()) {
        let path = PathBuf::from(dir);
        if path.is_dir() {
            roots.push(path);
        }
    }
    let xdg = xdg_data_home
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.map(|home| home.join(".local/share")));
    if let Some(xdg) = xdg {
        let path = xdg.join("amp/threads");
        if path.is_dir() {
            roots.push(path);
        }
    }
    for storage in vscode_extension_storage_dirs_from(config_dir, EXTENSION_ID) {
        for name in ["threads3", "threads"] {
            let path = storage.join(name);
            if path.is_dir() {
                roots.push(path);
            }
        }
    }
    roots.sort();
    roots.dedup();
    roots
}

fn scan_threads(roots: &[PathBuf], since_ts: Option<i64>) -> anyhow::Result<SyncScanResult> {
    let mut sessions = Vec::new();
    let mut stats = SyncScanStats::default();
    let mut by_id: HashMap<String, RawSession> = HashMap::new();

    for path in collect_thread_files(roots) {
        stats.candidates += 1;
        let Ok(meta) = fs::metadata(&path) else {
            stats.rejected_before_parse += 1;
            continue;
        };
        if meta.len() > MAX_THREAD_BYTES {
            debug!("skipping oversized Amp thread {}", path.display());
            stats.rejected_before_parse += 1;
            continue;
        }
        match parse_thread_file(&path) {
            Ok(Some(raw)) => {
                if since_ts.is_some_and(|cutoff| {
                    last_timestamp(raw.updated_at, &raw.messages, &[], &[])
                        .is_some_and(|updated| updated < cutoff)
                }) {
                    stats.filtered_sessions += 1;
                    continue;
                }
                stats.parsed += 1;
                by_id.entry(raw.source_id.clone()).or_insert(raw);
            }
            Ok(None) => {}
            Err(err) => warn!("failed to parse Amp thread {}: {err}", path.display()),
        }
    }

    sessions.extend(by_id.into_values());
    Ok(SyncScanResult { sessions, stats, observations: Vec::new() })
}

fn collect_thread_files(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for root in roots {
        let read = match fs::read_dir(root) {
            Ok(read) => read,
            Err(err) => {
                debug!("cannot read Amp threads dir {}: {err}", root.display());
                continue;
            }
        };
        for entry in read.flatten() {
            let path = entry.path();
            if path.is_file() && is_amp_thread_file(&path) {
                files.push(path);
            }
        }
    }
    files
}

fn is_amp_thread_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if !name.ends_with(".json") || SKIP_NAMES.iter().any(|skip| *skip == name) {
        return false;
    }
    let parent = path.parent().and_then(|parent| parent.file_name()).and_then(|name| name.to_str());
    if matches!(parent, Some("threads" | "threads3")) {
        return true;
    }
    name == "thread.json"
        || name.starts_with("conversation")
        || name.starts_with("chat")
        || name.starts_with("T-")
}

fn parse_thread_file(path: &Path) -> anyhow::Result<Option<RawSession>> {
    let raw = fs::read_to_string(path)?;
    let value: Value = match serde_json::from_str(&raw) {
        Ok(value) => value,
        Err(err) => {
            warn!("failed to parse Amp JSON {}: {err}", path.display());
            return Ok(None);
        }
    };
    Ok(parse_thread_value(&value, path))
}

fn parse_thread_value(value: &Value, path: &Path) -> Option<RawSession> {
    let doc = value.get("thread").filter(|thread| thread.is_object()).unwrap_or(value);
    let source_id = doc
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .filter(|stem| *stem != "thread")
                .map(|stem| stem.to_string())
        })?;
    let messages_value = doc.get("messages").or_else(|| value.get("messages"));
    let messages = parse_messages(messages_value);
    if messages.is_empty() {
        return None;
    }
    let created = json_i64(doc.get("created")).or_else(|| rfc3339_ms(doc.get("created")));
    let started_at = first_timestamp(created, &messages, &[], &[]).unwrap_or(0);
    let updated_at = last_timestamp(created, &messages, &[], &[]);
    let mut session = RawSession::search_only(
        source_id,
        env_cwd(doc.get("env")).or_else(|| env_cwd(value.get("env"))),
        started_at,
        updated_at,
        None,
        messages,
    );
    session.source_file_path = path.to_str().map(str::to_string);
    session.custom_title = doc
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_string);
    Some(session)
}

fn parse_messages(value: Option<&Value>) -> Vec<RawMessage> {
    let Some(items) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut messages = Vec::new();
    for item in items {
        let Some(role) = parse_role(item.get("role").and_then(Value::as_str).unwrap_or("")) else {
            continue;
        };
        let content = extract_text(item.get("content"));
        if content.is_empty() {
            continue;
        }
        let timestamp = json_i64(item.get("created"))
            .or_else(|| rfc3339_ms(item.get("created")))
            .or_else(|| json_i64(item.get("timestamp")))
            .or_else(|| rfc3339_ms(item.get("timestamp")))
            .or_else(|| json_i64(item.pointer("/meta/sentAt")));
        messages.push(RawMessage { role, content, timestamp });
    }
    messages
}

fn parse_role(role: &str) -> Option<Role> {
    match role {
        "human" | "user" => Some(Role::User),
        "agent" | "assistant" => Some(Role::Assistant),
        _ => None,
    }
}

fn extract_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.trim().to_string(),
        Some(Value::Array(blocks)) => {
            blocks.iter().filter_map(block_text).collect::<Vec<_>>().join("\n").trim().to_string()
        }
        _ => String::new(),
    }
}

fn block_text(block: &Value) -> Option<&str> {
    if let Some(text) = block.as_str().map(str::trim).filter(|text| !text.is_empty()) {
        return Some(text);
    }
    let kind = block.get("type").and_then(Value::as_str).unwrap_or("text");
    if kind != "text" {
        return None;
    }
    block.get("text").and_then(Value::as_str).map(str::trim).filter(|text| !text.is_empty())
}

fn env_cwd(env: Option<&Value>) -> Option<String> {
    let env = env?;
    if let Some(cwd) = string_path(env.get("cwd")) {
        return Some(cwd);
    }
    if let Some(cwd) = env.get("initial").and_then(env_cwd) {
        return Some(cwd);
    }
    let trees = env.get("trees").and_then(Value::as_array)?;
    for tree in trees {
        if let Some(cwd) = string_path(tree.get("cwd"))
            .or_else(|| string_path(tree.get("path")))
            .or_else(|| string_path(tree.get("fsPath")))
            .or_else(|| file_uri_path(tree.get("uri")))
        {
            return Some(cwd);
        }
    }
    None
}

fn string_path(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn file_uri_path(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(uri)) => {
            let text = uri.trim();
            if text.is_empty() {
                None
            } else if let Some(rest) = text.strip_prefix("file://") {
                Some(rest.to_string())
            } else {
                Some(text.to_string())
            }
        }
        Some(Value::Object(map)) => {
            string_path(map.get("fsPath")).or_else(|| string_path(map.get("path")))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{schema, store::Store};
    use crate::types::Session;
    use std::time::{Duration, UNIX_EPOCH};

    fn fixtures_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/amp")
    }

    fn setup_store() -> Store {
        schema::register_sqlite_vec();
        Store::open_in_memory().unwrap()
    }

    #[test]
    fn resume_uses_threads_continue_and_json_id() {
        let command = AmpAdapter.resume_command("T-0199aaaa-bbbb-7ccc-8ddd-eeeeffff0001").unwrap();
        assert_eq!(command.program, "amp");
        assert_eq!(command.args, vec![
            "threads",
            "continue",
            "T-0199aaaa-bbbb-7ccc-8ddd-eeeeffff0001"
        ]);
    }

    #[test]
    fn missing_roots_are_empty() {
        let roots = thread_roots_from(None, None, Some(PathBuf::from("/no/such/home")), None);
        assert!(roots.is_empty());
        let result = scan_threads(&roots, None).unwrap();
        assert!(result.sessions.is_empty());
    }

    #[test]
    fn amp_threads_dir_and_xdg_are_scanned_when_present() {
        let root = tempfile::tempdir().unwrap();
        let amp_dir = root.path().join("amp-threads");
        let xdg = root.path().join("xdg");
        fs::create_dir_all(&amp_dir).unwrap();
        fs::create_dir_all(xdg.join("amp/threads")).unwrap();
        let roots = thread_roots_from(
            Some(amp_dir.to_string_lossy().into_owned()),
            Some(xdg.to_string_lossy().into_owned()),
            Some(PathBuf::from("/unused")),
            None,
        );
        assert_eq!(roots, vec![amp_dir, xdg.join("amp/threads")]);
    }

    #[test]
    fn vscode_threads3_and_threads_children_are_roots() {
        let config = tempfile::tempdir().unwrap();
        let storage =
            config.path().join("Code").join("User").join("globalStorage").join(EXTENSION_ID);
        fs::create_dir_all(storage.join("threads3")).unwrap();
        fs::create_dir_all(storage.join("threads")).unwrap();
        let roots = thread_roots_from(
            None,
            None,
            Some(PathBuf::from("/unused")),
            Some(config.path().to_path_buf()),
        );
        assert!(roots.contains(&storage.join("threads3")));
        assert!(roots.contains(&storage.join("threads")));
    }

    #[test]
    fn parse_prefers_object_id_over_filename_thread_json() {
        let path = fixtures_dir().join("thread.json");
        let session = parse_thread_file(&path).unwrap().unwrap();
        assert_eq!(session.source_id, "T-0199aaaa-bbbb-7ccc-8ddd-eeeeffff0001");
        assert_eq!(session.custom_title.as_deref(), Some("Amp thread title"));
        assert_eq!(session.directory.as_deref(), Some("/tmp/amp-project"));
        assert_eq!(session.started_at, 1_700_000_000_000);
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, Role::User);
        assert_eq!(session.messages[0].content, "hello from amp");
        assert_eq!(session.messages[1].role, Role::Assistant);
        assert_eq!(session.messages[1].content, "hi back");
    }

    #[test]
    fn parse_reads_thread_messages_wrapper() {
        let path = fixtures_dir().join("wrapped-thread.json");
        let session = parse_thread_file(&path).unwrap().unwrap();
        assert_eq!(session.source_id, "T-0199cccc-dddd-7eee-8fff-000011112222");
        assert_eq!(session.messages[0].content, "wrapped user");
        assert_eq!(session.messages[1].content, "wrapped assistant");
    }

    #[test]
    fn skips_secrets_and_settings() {
        assert!(!is_amp_thread_file(Path::new("/tmp/threads/secrets.json")));
        assert!(!is_amp_thread_file(Path::new("/tmp/threads/settings.json")));
        assert!(!is_amp_thread_file(Path::new("/tmp/threads/config.json")));
        assert!(is_amp_thread_file(Path::new("/tmp/threads/T-abc.json")));
        assert!(is_amp_thread_file(Path::new("/tmp/threads/thread.json")));
        assert!(is_amp_thread_file(Path::new("/tmp/threads/conversation-1.json")));
    }

    #[test]
    fn incremental_rereads_when_mtime_stale_and_size_grown() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("thread.json");
        fs::copy(fixtures_dir().join("thread.json"), &path).unwrap();
        let first_mtime = 1_700_000_000_000i64;
        fs::File::open(&path)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_millis(first_mtime as u64))
            .unwrap();

        let first = scan_threads(&[root.path().to_path_buf()], None).unwrap();
        assert_eq!(first.sessions.len(), 1);
        assert_eq!(first.sessions[0].messages.len(), 2);

        let store = setup_store();
        store
            .insert_session(&Session {
                id: "local-amp".to_string(),
                source: SOURCE.to_string(),
                source_id: "T-0199aaaa-bbbb-7ccc-8ddd-eeeeffff0001".to_string(),
                title: "existing".to_string(),
                directory: None,
                repo_remote: None,
                repo_slug: None,
                repo_name: None,
                started_at: first_mtime,
                updated_at: Some(first_mtime),
                message_count: 2,
                entrypoint: None,
                custom_title: None,
                summary: None,
                duration_minutes: None,
                source_file_path: path.to_str().map(str::to_string),
                is_import: false,
            })
            .unwrap();

        let mut value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        value["messages"].as_array_mut().unwrap().push(serde_json::json!({
            "role": "human",
            "content": [{ "type": "text", "text": "appended without mtime" }],
            "created": 1700000003000u64
        }));
        fs::write(&path, serde_json::to_string(&value).unwrap()).unwrap();
        fs::File::open(&path)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_millis(first_mtime as u64))
            .unwrap();

        let result = scan_threads(&[root.path().to_path_buf()], None).unwrap();
        assert_eq!(result.stats.skipped_sessions, 0);
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].messages.len(), 3);
        assert_eq!(result.sessions[0].messages[2].content, "appended without mtime");
    }
}
