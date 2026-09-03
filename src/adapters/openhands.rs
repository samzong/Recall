use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use chrono::TimeZone;
use serde::Deserialize;
use serde_json::Value;
use tracing::{debug, warn};

use crate::adapters::AdapterSyncContext;
use crate::adapters::events;
use crate::adapters::file_scan::{
    self, FileMetadataSnapshot, FileScanEntry, FileScanOptions, FileScanSnapshot,
};
use crate::adapters::json_util::{json_i64, rfc3339_ms};
use crate::adapters::{
    RawMessage, RawSession, ResumeCommand, SourceAdapter, SyncScanResult, SyncScanStats,
    first_timestamp, last_timestamp,
};
use crate::types::{RawSessionEvent, Role};

const SOURCE: &str = "openhands";
const EVENT_PARSER_VERSION: u32 = 1;

#[derive(Deserialize)]
struct EventIndexEntry {
    id: String,
    #[serde(default)]
    parent_id: Option<String>,
    #[serde(default)]
    kind: String,
    #[serde(default, rename = "type")]
    type_name: String,
    #[serde(default)]
    event_type: String,
}

pub(crate) struct OpenHandsAdapter;

impl SourceAdapter for OpenHandsAdapter {
    fn id(&self) -> &str {
        SOURCE
    }

    fn label(&self) -> &str {
        "OH"
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        Some(ResumeCommand {
            program: "openhands".to_string(),
            args: vec!["--resume".to_string(), source_id.to_string()],
        })
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        scan_conversations(conversations_dir())
    }

    fn scan_for_sync(
        &self,
        context: &AdapterSyncContext,
        since_ts: Option<i64>,
        include_events: bool,
    ) -> anyhow::Result<Option<SyncScanResult>> {
        let Some(root) = conversations_dir() else {
            return Ok(Some(SyncScanResult {
                sessions: Vec::new(),
                stats: SyncScanStats::default(),
                observations: Vec::new(),
            }));
        };
        Ok(Some(file_scan::run_file_scan_with_options_and_snapshot(
            context,
            since_ts,
            FileScanOptions {
                usage_parser_version: None,
                event_parser_version: include_events.then_some(EVENT_PARSER_VERSION),
                metadata_parser_version: None,
            },
            collect_conversation_entries(&Some(root)),
            conversation_snapshot,
            |entry, mtime_ms| parse_conversation_entry(entry, Some(mtime_ms), include_events),
        )?))
    }
}

fn conversations_dir() -> Option<PathBuf> {
    conversations_dir_from(
        std::env::var("OPENHANDS_CONVERSATIONS_DIR").ok(),
        std::env::var("OPENHANDS_PERSISTENCE_DIR").ok(),
        std::env::var("OH_PERSISTENCE_DIR").ok(),
        std::env::var("FILE_STORE_PATH").ok(),
        dirs::home_dir(),
    )
}

fn nonempty_path(value: Option<String>) -> Option<PathBuf> {
    value.as_deref().map(str::trim).filter(|value| !value.is_empty()).map(PathBuf::from)
}

fn conversations_dir_from(
    conversations: Option<String>,
    persistence: Option<String>,
    oh_persistence: Option<String>,
    file_store: Option<String>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(dir) = nonempty_path(conversations).filter(|dir| dir.is_dir()) {
        return Some(dir);
    }
    [persistence, oh_persistence, file_store]
        .into_iter()
        .find_map(|root| {
            nonempty_path(root).map(|path| path.join("conversations")).filter(|dir| dir.is_dir())
        })
        .or_else(|| {
            home.map(|home| home.join(".openhands").join("conversations"))
                .filter(|path| path.is_dir())
        })
}

fn scan_conversations(root: Option<PathBuf>) -> anyhow::Result<Vec<RawSession>> {
    let mut sessions = Vec::new();
    for entry in collect_conversation_entries(&root) {
        let mtime_ms = conversation_snapshot(&entry).map(|snapshot| snapshot.effective_mtime_ms());
        if let Some(raw) = parse_conversation_entry(entry, mtime_ms, true)? {
            sessions.push(raw);
        }
    }
    Ok(sessions)
}

fn collect_conversation_entries(root: &Option<PathBuf>) -> Vec<FileScanEntry> {
    let Some(root) = root else {
        return Vec::new();
    };
    let read = match fs::read_dir(root) {
        Ok(read) => read,
        Err(err) => {
            debug!("cannot read OpenHands conversations {}: {err}", root.display());
            return Vec::new();
        }
    };
    let mut entries = Vec::new();
    for entry in read.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(source_id) = path.file_name().and_then(|name| name.to_str()).map(str::to_string)
        else {
            continue;
        };
        let events_dir = path.join("events");
        let conversation_json = path.join("conversation.json");
        let stat_target = if events_dir.is_dir() {
            events_dir
        } else if conversation_json.is_file() {
            conversation_json
        } else {
            continue;
        };
        entries.push(FileScanEntry { session_id: source_id, stat_target, directory: None });
    }
    entries
}

fn conversation_snapshot(
    entry: &FileScanEntry,
) -> Option<FileScanSnapshot<(FileMetadataSnapshot, Option<FileMetadataSnapshot>)>> {
    let primary = file_scan::file_metadata_snapshot(&entry.stat_target)?;
    let base = if entry.stat_target.is_dir() {
        entry
            .stat_target
            .parent()
            .and_then(|dir| file_scan::file_metadata_snapshot(&dir.join("base_state.json")))
    } else {
        None
    };
    let mut effective_mtime_ms = primary.mtime_ms()?;
    if let Some(base_mtime_ms) = base.as_ref().and_then(FileMetadataSnapshot::mtime_ms) {
        effective_mtime_ms = effective_mtime_ms.max(base_mtime_ms);
    }
    Some(FileScanSnapshot::new(effective_mtime_ms, (primary, base)))
}

fn parse_conversation_entry(
    entry: FileScanEntry,
    mtime_ms: Option<i64>,
    include_events: bool,
) -> anyhow::Result<Option<RawSession>> {
    let Some(dir) = entry.stat_target.parent().map(Path::to_path_buf) else {
        return Ok(None);
    };
    match parse_conversation_dir(&dir, &entry.session_id, mtime_ms, include_events) {
        Ok(raw) => Ok(raw),
        Err(err) => {
            warn!("failed to parse OpenHands conversation {}: {err}", dir.display());
            Ok(None)
        }
    }
}

fn parse_conversation_dir(
    dir: &Path,
    source_id: &str,
    mtime_ms: Option<i64>,
    include_events: bool,
) -> anyhow::Result<Option<RawSession>> {
    let events_dir = dir.join("events");
    if events_dir.is_dir() {
        return parse_sdk_layout(dir, &events_dir, source_id, mtime_ms, include_events);
    }
    let conversation_json = dir.join("conversation.json");
    if conversation_json.is_file() {
        return parse_docs_layout(&conversation_json, source_id, mtime_ms);
    }
    Ok(None)
}

fn parse_sdk_layout(
    dir: &Path,
    events_dir: &Path,
    source_id: &str,
    mtime_ms: Option<i64>,
    include_events: bool,
) -> anyhow::Result<Option<RawSession>> {
    let base = read_json(&dir.join("base_state.json")).unwrap_or(Value::Null);
    let directory = workspace_dir(&base);
    let title = json_string(&base, &["title", "name"]);
    let mut messages = Vec::new();
    let mut events = Vec::new();
    let stored_events = list_event_files(events_dir)
        .into_iter()
        .filter_map(|path| read_event_index(&path).map(|event| (path, event)))
        .collect::<Vec<_>>();
    for index in active_event_indices(&base, &stored_events)? {
        let path = &stored_events[index].0;
        let Some(value) = read_json(path) else {
            continue;
        };
        let kind = event_kind(&value);
        let timestamp = event_timestamp(&value);
        match kind {
            "SystemPromptEvent" => {}
            "MessageEvent" => {
                if let Some(role) = message_role(&value)
                    && let Some(content) = message_text(&value)
                {
                    messages.push(RawMessage { role, content, timestamp });
                }
            }
            "ActionEvent" if include_events => {
                if let Some(event) = action_event(&value, timestamp, events.len() as u32, path) {
                    events.push(event);
                }
            }
            "ObservationEvent" if include_events => {
                events.push(events::tool_result_event(
                    event_context(&value, timestamp, events.len() as u32, path),
                    json_string(&value, &["tool_name", "name"]),
                    extract_text(value.get("content"))
                        .or_else(|| extract_text(value.pointer("/observation/content"))),
                ));
            }
            _ => {}
        }
    }
    if messages.is_empty() && events.is_empty() {
        return Ok(None);
    }
    let started_at = first_timestamp(event_timestamp(&base), &messages, &[], &events)
        .unwrap_or(mtime_ms.unwrap_or(0));
    let updated_at = last_timestamp(mtime_ms, &messages, &[], &events).or(mtime_ms);
    let mut raw = RawSession::search_only(
        source_id.to_string(),
        directory,
        started_at,
        updated_at,
        None,
        messages,
    );
    raw.source_file_path = dir.to_str().map(str::to_string);
    raw.custom_title = title;
    if include_events {
        raw = raw.with_events(events, EVENT_PARSER_VERSION);
    }
    Ok(Some(raw))
}

fn parse_docs_layout(
    path: &Path,
    source_id: &str,
    mtime_ms: Option<i64>,
) -> anyhow::Result<Option<RawSession>> {
    let Some(value) = read_json(path) else {
        return Ok(None);
    };
    let id =
        json_string(&value, &["id", "conversation_id"]).unwrap_or_else(|| source_id.to_string());
    let messages = parse_docs_messages(&value);
    if messages.is_empty() {
        return Ok(None);
    }
    let started_at = first_timestamp(event_timestamp(&value), &messages, &[], &[])
        .unwrap_or(mtime_ms.unwrap_or(0));
    let mut raw = RawSession::search_only(
        id,
        workspace_dir(&value),
        started_at,
        last_timestamp(mtime_ms, &messages, &[], &[]).or(mtime_ms),
        None,
        messages,
    );
    raw.source_file_path = path.to_str().map(str::to_string);
    raw.custom_title = json_string(&value, &["title", "name"]);
    Ok(Some(raw))
}

fn parse_docs_messages(value: &Value) -> Vec<RawMessage> {
    let Some(items) = value.get("messages").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut messages = Vec::new();
    for item in items {
        let Some(role) = parse_role(item.get("role").and_then(Value::as_str).unwrap_or("")) else {
            continue;
        };
        let Some(content) = extract_text(item.get("content")) else {
            continue;
        };
        messages.push(RawMessage { role, content, timestamp: event_timestamp(item) });
    }
    messages
}

fn list_event_files(events_dir: &Path) -> Vec<PathBuf> {
    let read = match fs::read_dir(events_dir) {
        Ok(read) => read,
        Err(err) => {
            debug!("cannot read OpenHands events {}: {err}", events_dir.display());
            return Vec::new();
        }
    };
    let mut files: Vec<(u64, String, PathBuf)> = Vec::new();
    for entry in read.flatten() {
        let path = entry.path();
        if !path.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default().to_string();
        files.push((event_ordinal(&name).unwrap_or(u64::MAX), name, path));
    }
    files.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    files.into_iter().map(|(_, _, path)| path).collect()
}

fn active_event_indices(
    base: &Value,
    events: &[(PathBuf, EventIndexEntry)],
) -> anyhow::Result<Vec<usize>> {
    let mut by_id = HashMap::new();
    for (index, (_, event)) in events.iter().enumerate() {
        let id = event.id.trim().to_string();
        if by_id.insert(id.clone(), index).is_some() {
            return Err(anyhow::anyhow!("OpenHands event id {id} is duplicated"));
        }
    }
    let leaf = if let Some(leaf) = json_string(base, &["leaf_event_id"]) {
        Some(leaf)
    } else if base.get("head_is_empty").and_then(Value::as_bool) == Some(true) {
        None
    } else {
        events
            .iter()
            .rev()
            .find(|(_, event)| {
                !matches!(
                    indexed_event_kind(event),
                    "ConversationStateUpdateEvent" | "ConversationErrorEvent"
                )
            })
            .map(|(_, event)| event.id.trim().to_string())
    };
    let Some(mut current) = leaf else {
        return Ok(Vec::new());
    };
    let mut indices = Vec::new();
    let mut seen = HashSet::new();
    loop {
        if !seen.insert(current.clone()) {
            return Err(anyhow::anyhow!("cycle in OpenHands event tree at {current}"));
        }
        let index = *by_id.get(&current).ok_or_else(|| {
            anyhow::anyhow!("OpenHands event tree references missing id {current}")
        })?;
        indices.push(index);
        current = match events[index].1.parent_id.as_deref().map(str::trim) {
            Some("__root__") => break,
            Some(parent) => parent.to_string(),
            None if index == 0 => break,
            None => events[index - 1].1.id.trim().to_string(),
        };
    }
    indices.reverse();
    Ok(indices)
}

fn indexed_event_kind(event: &EventIndexEntry) -> &str {
    [&event.kind, &event.type_name, &event.event_type]
        .into_iter()
        .find(|kind| !kind.is_empty())
        .map(String::as_str)
        .unwrap_or("")
}

fn event_ordinal(name: &str) -> Option<u64> {
    let stem = name.strip_suffix(".json")?;
    let rest = stem.strip_prefix("event-").or_else(|| stem.strip_prefix("event_"))?;
    rest.split(['-', '_']).next()?.parse().ok()
}

fn event_kind(value: &Value) -> &str {
    value
        .get("kind")
        .or_else(|| value.get("type"))
        .or_else(|| value.get("event_type"))
        .and_then(Value::as_str)
        .unwrap_or("")
}

fn event_timestamp(value: &Value) -> Option<i64> {
    timestamp_field(value.get("timestamp")).or_else(|| timestamp_field(value.get("created_at")))
}

fn timestamp_field(value: Option<&Value>) -> Option<i64> {
    json_i64(value)
        .or_else(|| rfc3339_ms(value))
        .or_else(|| naive_iso8601_ms(value, &chrono::Local))
}

fn naive_iso8601_ms<Tz: TimeZone>(value: Option<&Value>, timezone: &Tz) -> Option<i64> {
    let text = value?.as_str()?.trim();
    const FMTS: &[&str] = &["%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M:%S%.f"];
    for fmt in FMTS {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(text, fmt) {
            return timezone
                .from_local_datetime(&naive)
                .earliest()
                .map(|timestamp| timestamp.timestamp_millis());
        }
    }
    None
}

fn workspace_dir(value: &Value) -> Option<String> {
    if let Some(text) = json_string(value, &["workspace", "cwd", "working_directory"]) {
        return Some(text);
    }
    json_string(
        value.get("workspace").unwrap_or(&Value::Null),
        &["working_dir", "working_directory", "cwd", "path"],
    )
}

fn message_role(value: &Value) -> Option<Role> {
    let source = value.get("source").and_then(Value::as_str).unwrap_or("");
    match source {
        "user" => Some(Role::User),
        "agent" => Some(Role::Assistant),
        _ => {
            let role = value
                .pointer("/llm_message/role")
                .or_else(|| value.pointer("/message/role"))
                .and_then(Value::as_str)
                .unwrap_or("");
            parse_role(role)
        }
    }
}

fn message_text(value: &Value) -> Option<String> {
    let content = extract_text(value.pointer("/llm_message/content"))
        .or_else(|| extract_text(value.pointer("/message/content")))
        .or_else(|| extract_text(value.get("content")))
        .or_else(|| extract_text(value.get("text")));
    let content = content?;
    if content.is_empty() { None } else { Some(content) }
}

fn parse_role(role: &str) -> Option<Role> {
    match role {
        "user" | "human" => Some(Role::User),
        "assistant" | "agent" => Some(Role::Assistant),
        _ => None,
    }
}

fn extract_text(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(text)) => {
            let text = text.trim();
            if text.is_empty() { None } else { Some(text.to_string()) }
        }
        Some(Value::Array(blocks)) => {
            let text = blocks.iter().filter_map(block_text).collect::<Vec<_>>().join("\n");
            let text = text.trim();
            if text.is_empty() { None } else { Some(text.to_string()) }
        }
        _ => None,
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

fn action_event(
    value: &Value,
    timestamp: Option<i64>,
    event_seq: u32,
    path: &Path,
) -> Option<RawSessionEvent> {
    let name = json_string(value, &["tool_name", "name"])
        .or_else(|| {
            json_string(value.pointer("/action").unwrap_or(&Value::Null), &["kind", "name"])
        })
        .filter(|name| !name.is_empty())?;
    let args = value.get("action").or_else(|| value.get("tool_call")).or_else(|| value.get("args"));
    Some(events::tool_call_event(event_context(value, timestamp, event_seq, path), name, args))
}

fn event_context(
    value: &Value,
    timestamp: Option<i64>,
    event_seq: u32,
    path: &Path,
) -> events::EventContext {
    events::EventContext {
        event_seq,
        timestamp,
        source_path: path.to_str().map(str::to_string),
        source_event_id: json_string(value, &["id"]),
        message_seq: None,
        parser_version: EVENT_PARSER_VERSION,
    }
}

fn json_string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn read_json(path: &Path) -> Option<Value> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(_) => return None,
    };
    match serde_json::from_str(&raw) {
        Ok(value) => Some(value),
        Err(err) => {
            warn!("failed to parse OpenHands JSON {}: {err}", path.display());
            None
        }
    }
}

fn read_event_index(path: &Path) -> Option<EventIndexEntry> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(_) => return None,
    };
    match serde_json::from_reader(BufReader::new(file)) {
        Ok(event) => Some(event),
        Err(err) => {
            warn!("failed to index OpenHands JSON {}: {err}", path.display());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixtures_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/openhands")
    }

    fn copy_tree(from: &Path, to: &Path) {
        for entry in walkdir::WalkDir::new(from).into_iter().filter_map(Result::ok) {
            let relative = entry.path().strip_prefix(from).unwrap();
            let dest = to.join(relative);
            if entry.file_type().is_dir() {
                fs::create_dir_all(&dest).unwrap();
            } else if entry.file_type().is_file() {
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent).unwrap();
                }
                fs::copy(entry.path(), dest).unwrap();
            }
        }
    }

    #[test]
    fn resume_uses_official_flag() {
        let command = OpenHandsAdapter.resume_command("abc123def456").unwrap();
        assert_eq!(command.program, "openhands");
        assert_eq!(command.args, vec!["--resume", "abc123def456"]);
    }

    #[test]
    fn default_root_is_home_openhands_conversations() {
        let home = tempfile::tempdir().unwrap();
        assert!(
            conversations_dir_from(None, None, None, None, Some(home.path().to_path_buf()))
                .is_none()
        );
        fs::create_dir_all(home.path().join(".openhands/conversations")).unwrap();
        let resolved =
            conversations_dir_from(None, None, None, None, Some(home.path().to_path_buf()))
                .unwrap();
        assert_eq!(resolved, home.path().join(".openhands/conversations"));
    }

    #[test]
    fn openhands_conversations_dir_wins() {
        let root = tempfile::tempdir().unwrap();
        let conversations = root.path().join("cli-conversations");
        fs::create_dir_all(&conversations).unwrap();
        fs::create_dir_all(root.path().join("persist/conversations")).unwrap();
        fs::create_dir_all(root.path().join("oh/conversations")).unwrap();
        fs::create_dir_all(root.path().join("legacy/conversations")).unwrap();
        fs::create_dir_all(root.path().join("home/.openhands/conversations")).unwrap();
        let resolved = conversations_dir_from(
            Some(conversations.to_string_lossy().into_owned()),
            Some(root.path().join("persist").to_string_lossy().into_owned()),
            Some(root.path().join("oh").to_string_lossy().into_owned()),
            Some(root.path().join("legacy").to_string_lossy().into_owned()),
            Some(root.path().join("home")),
        )
        .unwrap();
        assert_eq!(resolved, conversations);
    }

    #[test]
    fn file_store_path_without_conversations_does_not_veto_home() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        fs::create_dir_all(home.join(".openhands/conversations")).unwrap();
        let unrelated = root.path().join("unrelated-store");
        fs::create_dir_all(&unrelated).unwrap();
        let resolved = conversations_dir_from(
            None,
            None,
            None,
            Some(unrelated.to_string_lossy().into_owned()),
            Some(home.clone()),
        )
        .unwrap();
        assert_eq!(resolved, home.join(".openhands/conversations"));
    }

    #[test]
    fn missing_persistence_candidates_are_empty() {
        let root = tempfile::tempdir().unwrap();
        assert!(
            conversations_dir_from(
                Some(root.path().join("missing-conversations").to_string_lossy().into_owned()),
                Some(root.path().join("missing-persist").to_string_lossy().into_owned()),
                Some(root.path().join("missing-oh").to_string_lossy().into_owned()),
                Some(root.path().join("missing-store").to_string_lossy().into_owned()),
                Some(root.path().to_path_buf()),
            )
            .is_none()
        );
    }

    #[test]
    fn oh_persistence_dir_wins_over_file_store_path() {
        let root = tempfile::tempdir().unwrap();
        let persistence = root.path().join("oh");
        fs::create_dir_all(persistence.join("conversations")).unwrap();
        fs::create_dir_all(root.path().join("legacy/conversations")).unwrap();
        let resolved = conversations_dir_from(
            None,
            None,
            Some(persistence.to_string_lossy().into_owned()),
            Some(root.path().join("legacy").to_string_lossy().into_owned()),
            Some(PathBuf::from("/unused")),
        )
        .unwrap();
        assert_eq!(resolved, persistence.join("conversations"));
    }

    #[test]
    fn file_store_path_is_legacy_fallback() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join("legacy");
        fs::create_dir_all(legacy.join("conversations")).unwrap();
        let resolved = conversations_dir_from(
            None,
            None,
            None,
            Some(legacy.to_string_lossy().into_owned()),
            Some(PathBuf::from("/unused")),
        )
        .unwrap();
        assert_eq!(resolved, legacy.join("conversations"));
    }

    #[test]
    fn naive_iso8601_timestamp_uses_source_local_timezone() {
        let naive = serde_json::json!({ "timestamp": "2026-01-01T00:00:01" });
        let zulu = serde_json::json!({ "timestamp": "2026-01-01T00:00:01Z" });
        let eastern = chrono::FixedOffset::west_opt(5 * 60 * 60).unwrap();
        assert_eq!(naive_iso8601_ms(naive.get("timestamp"), &eastern), Some(1_767_243_601_000));
        assert_eq!(event_timestamp(&zulu), Some(1_767_225_601_000));
    }

    #[test]
    fn sdk_layout_sorts_by_numeric_ordinal_not_lex() {
        assert!(
            event_ordinal("event-100000-ddd.json").unwrap()
                > event_ordinal("event-99999-ccc.json").unwrap()
        );
        assert!(
            "event-100000-ddd.json" < "event-99999-ccc.json",
            "lex order would invert 100000 vs 99999"
        );
        let session =
            parse_conversation_dir(&fixtures_dir().join("sdk-layout"), "sdk-conv-1", Some(1), true)
                .unwrap()
                .unwrap();
        assert_eq!(session.source_id, "sdk-conv-1");
        assert_eq!(session.directory.as_deref(), Some("/tmp/oh-sdk"));
        assert_eq!(session.started_at, 1_767_225_601_000);
        assert_eq!(session.custom_title.as_deref(), Some("SDK conversation"));
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].content, "first user turn");
        assert_eq!(session.messages[1].content, "ordinal 99999 assistant");
        assert_eq!(session.events.len(), 2);
        assert_eq!(session.events[0].name.as_deref(), Some("terminal"));
        assert_eq!(session.events[0].kind, "tool_call");
        assert_eq!(session.events[1].kind, "tool_result");
    }

    #[test]
    fn sdk_layout_reads_only_the_active_branch() {
        let root = tempfile::tempdir().unwrap();
        let conversation = root.path().join("branching");
        let events = conversation.join("events");
        fs::create_dir_all(&events).unwrap();
        fs::write(
            conversation.join("base_state.json"),
            serde_json::to_vec(&serde_json::json!({
                "leaf_event_id": "active"
            }))
            .unwrap(),
        )
        .unwrap();
        for (ordinal, value) in [
            serde_json::json!({
                "kind": "MessageEvent",
                "id": "root",
                "parent_id": null,
                "source": "user",
                "llm_message": { "content": "root message" }
            }),
            serde_json::json!({
                "kind": "MessageEvent",
                "id": "abandoned",
                "parent_id": "root",
                "source": "agent",
                "llm_message": { "content": "abandoned branch" }
            }),
            serde_json::json!({
                "kind": "MessageEvent",
                "id": "active",
                "parent_id": "root",
                "source": "agent",
                "llm_message": { "content": "active branch" }
            }),
        ]
        .into_iter()
        .enumerate()
        {
            fs::write(
                events.join(format!("event-{ordinal:05}-{ordinal}.json")),
                serde_json::to_vec(&value).unwrap(),
            )
            .unwrap();
        }
        let session =
            parse_conversation_dir(&conversation, "branching", Some(1), false).unwrap().unwrap();
        let messages =
            session.messages.into_iter().map(|message| message.content).collect::<Vec<_>>();
        assert_eq!(messages, vec!["root message", "active branch"]);
    }

    #[test]
    fn sdk_layout_respects_an_empty_head() {
        let root = tempfile::tempdir().unwrap();
        let conversation = root.path().join("empty-head");
        copy_tree(&fixtures_dir().join("sdk-layout"), &conversation);
        fs::write(
            conversation.join("base_state.json"),
            serde_json::to_vec(&serde_json::json!({
                "leaf_event_id": null,
                "head_is_empty": true
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(
            parse_conversation_dir(&conversation, "empty-head", Some(1), true).unwrap().is_none()
        );
    }

    #[test]
    fn sdk_snapshot_tracks_base_state_changes() {
        let root = tempfile::tempdir().unwrap();
        let conversation = root.path().join("sdk-conv-1");
        copy_tree(&fixtures_dir().join("sdk-layout"), &conversation);
        let entry = collect_conversation_entries(&Some(root.path().to_path_buf())).pop().unwrap();
        let before = conversation_snapshot(&entry).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(
            conversation.join("base_state.json"),
            serde_json::to_vec(&serde_json::json!({
                "leaf_event_id": "msg-user-1",
                "head_is_empty": false
            }))
            .unwrap(),
        )
        .unwrap();
        let after = conversation_snapshot(&entry).unwrap();
        assert_ne!(before, after);
        assert!(after.effective_mtime_ms() > before.effective_mtime_ms());
    }

    #[test]
    fn docs_layout_conversation_json_is_used_only_without_events() {
        let session = parse_conversation_dir(
            &fixtures_dir().join("docs-layout"),
            "abc123def456",
            Some(9),
            false,
        )
        .unwrap()
        .unwrap();
        assert_eq!(session.source_id, "abc123def456");
        assert_eq!(session.directory.as_deref(), Some("/tmp/oh-docs"));
        assert_eq!(session.messages[0].content, "Fix the login bug in auth.py");
        assert_eq!(session.messages[1].content, "Looking at auth.py now");
        assert!(session.events.is_empty());
    }

    #[test]
    fn events_dir_wins_over_stale_conversation_json() {
        let root = tempfile::tempdir().unwrap();
        let conv = root.path().join("mixed");
        copy_tree(&fixtures_dir().join("sdk-layout"), &conv);
        fs::copy(
            fixtures_dir().join("docs-layout/conversation.json"),
            conv.join("conversation.json"),
        )
        .unwrap();
        let session = parse_conversation_dir(&conv, "mixed", Some(1), true).unwrap().unwrap();
        assert_eq!(session.messages[0].content, "first user turn");
        assert_ne!(session.messages[0].content, "Fix the login bug in auth.py");
    }

    #[test]
    fn scan_uses_stat_target_mtime() {
        let sessions = scan_conversations(Some(fixtures_dir())).unwrap();
        let sdk =
            sessions.iter().find(|session| session.source_id == "sdk-layout").expect("sdk-layout");
        assert_ne!(sdk.updated_at, Some(0));
        assert!(sdk.updated_at.unwrap_or(0) > 0);
    }

    #[test]
    fn missing_conversations_dir_is_empty() {
        let entries = collect_conversation_entries(&None);
        assert!(entries.is_empty());
    }

    #[test]
    fn collect_identifies_events_or_conversation_json() {
        let root = tempfile::tempdir().unwrap();
        copy_tree(&fixtures_dir().join("sdk-layout"), &root.path().join("sdk-conv-1"));
        copy_tree(&fixtures_dir().join("docs-layout"), &root.path().join("abc123def456"));
        fs::create_dir_all(root.path().join("empty")).unwrap();
        let mut ids: Vec<_> = collect_conversation_entries(&Some(root.path().to_path_buf()))
            .into_iter()
            .map(|entry| entry.session_id)
            .collect();
        ids.sort();
        assert_eq!(ids, vec!["abc123def456", "sdk-conv-1"]);
    }
}
