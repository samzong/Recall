use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use tracing::{debug, warn};

use crate::adapters::file_scan::{self, FileScanEntry};
use crate::adapters::json_util::{json_i64, jsonl_indexed, rfc3339_ms};
use crate::adapters::{
    RawMessage, RawSession, ResumeCommand, SourceAdapter, SyncScanResult, SyncScanStats,
    first_timestamp, last_timestamp,
};
use crate::db::store::{SessionPath, Store};
use crate::types::Role;

pub(crate) struct KiroAdapter;

impl SourceAdapter for KiroAdapter {
    fn id(&self) -> &str {
        "kiro-cli"
    }
    fn label(&self) -> &str {
        "KIRO"
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        Some(ResumeCommand {
            program: "kiro-cli".to_string(),
            args: vec!["chat".to_string(), "--resume-id".to_string(), source_id.to_string()],
        })
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let mut sessions = scan_file_sessions();
        let covered = covered_ids(&sessions);
        for session in scan_sqlite_sessions() {
            if covered_contains(&covered, &session.source_id) {
                continue;
            }
            sessions.push(session);
        }
        Ok(sessions)
    }

    fn scan_for_sync(
        &self,
        store: &Store,
        since_ts: Option<i64>,
        _include_events: bool,
    ) -> anyhow::Result<Option<SyncScanResult>> {
        let mut result = if let Some(sessions_dir) = resolve_sessions_dir() {
            file_scan::run_file_scan_with_options(
                store,
                "kiro-cli",
                since_ts,
                file_scan::FileScanOptions::default(),
                collect_file_entries(&sessions_dir),
                parse_kiro_file_entry,
            )?
        } else {
            SyncScanResult { sessions: Vec::new(), stats: SyncScanStats::default() }
        };

        let paths = store.session_paths_for_source("kiro-cli").unwrap_or_default();
        reconcile_file_source_ids(&mut result.sessions, &paths);
        let mut covered = covered_for_sqlite(&result.sessions, &paths);
        for session in scan_sqlite_sessions() {
            if covered_contains(&covered, &session.source_id) {
                continue;
            }
            insert_id_keys(&mut covered, &session.source_id);
            result.sessions.push(session);
            result.stats.candidates += 1;
            result.stats.parsed += 1;
        }
        Ok(Some(result))
    }
}

fn resolve_kiro_home() -> Option<PathBuf> {
    if let Ok(raw) = std::env::var("KIRO_HOME") {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    dirs::home_dir().map(|home| home.join(".kiro"))
}

fn resolve_sessions_dir() -> Option<PathBuf> {
    let dir = resolve_kiro_home()?.join("sessions");
    if dir.is_dir() { Some(dir) } else { None }
}

fn kiro_db_path() -> Option<PathBuf> {
    if let Ok(raw) = std::env::var("KIRO_DATA_DIR") {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed).join("data.sqlite3"));
        }
    }
    dirs::data_dir().map(|data_dir| data_dir.join("kiro-cli/data.sqlite3"))
}

fn scan_file_sessions() -> Vec<RawSession> {
    let Some(sessions_dir) = resolve_sessions_dir() else {
        return Vec::new();
    };
    let mut sessions = Vec::new();
    for entry in collect_file_entries(&sessions_dir) {
        let Some(mtime_ms) = file_scan::stat_mtime_ms(&entry.stat_target) else {
            continue;
        };
        match parse_kiro_file_entry(entry, mtime_ms) {
            Ok(Some(session)) => sessions.push(session),
            Ok(None) => {}
            Err(error) => warn!("failed to parse kiro session: {error}"),
        }
    }
    sessions
}

fn collect_file_entries(sessions_dir: &Path) -> Vec<FileScanEntry> {
    let mut entries = Vec::new();
    collect_v2_entries(sessions_dir, &mut entries);
    collect_v3_entries(sessions_dir, &mut entries);
    entries
}

fn collect_v2_entries(sessions_dir: &Path, entries: &mut Vec<FileScanEntry>) {
    let cli_dir = sessions_dir.join("cli");
    let read = match fs::read_dir(&cli_dir) {
        Ok(read) => read,
        Err(error) => {
            debug!("cannot read {}: {error}", cli_dir.display());
            return;
        }
    };
    for dir_entry in read.flatten() {
        let path = dir_entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") || !path.is_file() {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|name| name.to_str()) else {
            continue;
        };
        if stem.starts_with("sess_") {
            continue;
        }
        entries.push(FileScanEntry {
            session_id: stem.to_string(),
            stat_target: path,
            directory: None,
        });
    }
}

fn collect_v3_entries(sessions_dir: &Path, entries: &mut Vec<FileScanEntry>) {
    let read = match fs::read_dir(sessions_dir) {
        Ok(read) => read,
        Err(error) => {
            debug!("cannot read {}: {error}", sessions_dir.display());
            return;
        }
    };
    for dir_entry in read.flatten() {
        let bucket = dir_entry.path();
        if !bucket.is_dir() {
            continue;
        }
        if bucket.file_name().and_then(|name| name.to_str()) == Some("cli") {
            continue;
        }
        let children = match fs::read_dir(&bucket) {
            Ok(children) => children,
            Err(error) => {
                debug!("cannot read {}: {error}", bucket.display());
                continue;
            }
        };
        for child in children.flatten() {
            let session_dir = child.path();
            let Some(name) = session_dir.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !name.starts_with("sess_") || !session_dir.is_dir() {
                continue;
            }
            let messages = session_dir.join("messages.jsonl");
            if !messages.is_file() {
                continue;
            }
            entries.push(FileScanEntry {
                session_id: name.to_string(),
                stat_target: messages,
                directory: None,
            });
        }
    }
}

fn parse_kiro_file_entry(
    entry: FileScanEntry,
    mtime_ms: i64,
) -> anyhow::Result<Option<RawSession>> {
    let is_v3 =
        entry.stat_target.file_name().and_then(|name| name.to_str()) == Some("messages.jsonl");
    let jsonl = match fs::read_to_string(&entry.stat_target) {
        Ok(text) => text,
        Err(error) => {
            warn!("failed to read {}: {error}", entry.stat_target.display());
            return Ok(None);
        }
    };
    let source_path = entry.stat_target.to_str().map(str::to_string);
    let parsed = if is_v3 {
        let sidecar = entry
            .stat_target
            .parent()
            .map(|parent| parent.join("session.json"))
            .and_then(|path| fs::read_to_string(path).ok());
        parse_kiro_v3_session(&jsonl, sidecar.as_deref(), &entry.session_id, mtime_ms, source_path)
    } else {
        let sidecar = entry
            .stat_target
            .with_extension("json")
            .to_str()
            .and_then(|path| fs::read_to_string(path).ok());
        parse_kiro_v2_session(&jsonl, sidecar.as_deref(), &entry.session_id, mtime_ms, source_path)
    };
    match parsed {
        Ok(session) => Ok(session),
        Err(error) => {
            warn!("failed to parse {}: {error}", entry.stat_target.display());
            Ok(None)
        }
    }
}

pub(crate) fn parse_kiro_v2_session(
    jsonl: &str,
    sidecar: Option<&str>,
    fallback_id: &str,
    mtime_ms: i64,
    source_path: Option<String>,
) -> anyhow::Result<Option<RawSession>> {
    let meta = sidecar.and_then(|text| serde_json::from_str::<Value>(text).ok());
    let session_id = meta
        .as_ref()
        .and_then(|value| value.get("session_id"))
        .and_then(Value::as_str)
        .unwrap_or(fallback_id)
        .to_string();
    let directory = meta
        .as_ref()
        .and_then(|value| value.get("cwd"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let title = meta
        .as_ref()
        .and_then(|value| value.get("title"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_string);
    let meta_started = meta.as_ref().and_then(|value| rfc3339_ms(value.get("created_at")));

    let mut messages = Vec::new();
    let mut last_prompt_ts = None;
    for item in jsonl_indexed(jsonl.lines().map(|line| Ok(line.to_string()))) {
        let (_, record) = item?;
        let kind = record.get("kind").and_then(Value::as_str).unwrap_or("");
        match kind {
            "Prompt" => {
                let content =
                    extract_v2_text(record.get("data").and_then(|data| data.get("content")));
                let timestamp = record
                    .get("data")
                    .and_then(|data| data.get("meta"))
                    .and_then(|meta| json_i64(meta.get("timestamp")))
                    .map(unix_ts_to_ms);
                last_prompt_ts = timestamp.or(last_prompt_ts);
                if !content.is_empty() {
                    messages.push(RawMessage { role: Role::User, content, timestamp });
                }
            }
            "AssistantMessage" => {
                let content =
                    extract_v2_text(record.get("data").and_then(|data| data.get("content")));
                if !content.is_empty() {
                    messages.push(RawMessage {
                        role: Role::Assistant,
                        content,
                        timestamp: last_prompt_ts,
                    });
                }
            }
            _ => {}
        }
    }

    finish_session(
        FileMeta { session_id, directory, title, started_at: meta_started },
        mtime_ms,
        source_path,
        messages,
    )
}

pub(crate) fn parse_kiro_v3_session(
    jsonl: &str,
    sidecar: Option<&str>,
    fallback_id: &str,
    mtime_ms: i64,
    source_path: Option<String>,
) -> anyhow::Result<Option<RawSession>> {
    let meta = sidecar.and_then(|text| serde_json::from_str::<Value>(text).ok());
    let session_id = meta
        .as_ref()
        .and_then(|value| {
            value.get("id").or_else(|| value.get("session_id")).or_else(|| value.get("sessionId"))
        })
        .and_then(Value::as_str)
        .unwrap_or(fallback_id)
        .to_string();
    let directory = meta.as_ref().and_then(|value| {
        first_path(value.get("workspacePaths"))
            .or_else(|| first_path(value.get("rootPaths")))
            .or_else(|| value.get("cwd").and_then(Value::as_str).map(str::to_string))
    });
    let title = meta
        .as_ref()
        .and_then(|value| value.get("title"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_string);
    let meta_started = meta.as_ref().and_then(|value| rfc3339_ms(value.get("createdAt")));

    let mut messages = Vec::new();
    for item in jsonl_indexed(jsonl.lines().map(|line| Ok(line.to_string()))) {
        let (_, record) = item?;
        let payload = record.get("payload").unwrap_or(&record);
        let kind = payload
            .get("type")
            .or_else(|| record.get("role"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let timestamp = rfc3339_ms(record.get("timestamp"))
            .or_else(|| json_i64(record.get("timestamp")).map(unix_ts_to_ms));
        match kind {
            "user" => {
                let content = extract_v3_text(payload.get("content"));
                if !content.is_empty() {
                    messages.push(RawMessage { role: Role::User, content, timestamp });
                }
            }
            "assistant" => {
                if payload.get("operationType").and_then(Value::as_str) == Some("Reasoning") {
                    continue;
                }
                let content = extract_v3_text(payload.get("content"));
                if !content.is_empty() {
                    messages.push(RawMessage { role: Role::Assistant, content, timestamp });
                }
            }
            _ => {}
        }
    }

    finish_session(
        FileMeta { session_id, directory, title, started_at: meta_started },
        mtime_ms,
        source_path,
        messages,
    )
}

struct FileMeta {
    session_id: String,
    directory: Option<String>,
    title: Option<String>,
    started_at: Option<i64>,
}

fn finish_session(
    meta: FileMeta,
    mtime_ms: i64,
    source_path: Option<String>,
    messages: Vec<RawMessage>,
) -> anyhow::Result<Option<RawSession>> {
    if messages.is_empty() {
        return Ok(None);
    }
    let started_at = first_timestamp(meta.started_at, &messages, &[], &[]).unwrap_or(mtime_ms);
    let mut session = RawSession::search_only(
        meta.session_id,
        meta.directory,
        started_at,
        Some(mtime_ms),
        None,
        messages,
    );
    session.source_file_path = source_path;
    session.custom_title = meta.title;
    Ok(Some(session))
}

fn extract_v2_text(content: Option<&Value>) -> String {
    let Some(content) = content else {
        return String::new();
    };
    if let Some(text) = content.as_str() {
        return usable_text(text);
    }
    let Some(items) = content.as_array() else {
        return String::new();
    };
    let mut parts = Vec::new();
    for item in items {
        if item.get("kind").and_then(Value::as_str) != Some("text") {
            continue;
        }
        if let Some(text) = item.get("data").and_then(Value::as_str) {
            let text = usable_text(text);
            if !text.is_empty() {
                parts.push(text);
            }
        }
    }
    parts.join("\n")
}

fn extract_v3_text(content: Option<&Value>) -> String {
    let Some(content) = content else {
        return String::new();
    };
    if let Some(text) = content.as_str() {
        return usable_text(text);
    }
    let Some(items) = content.as_array() else {
        return String::new();
    };
    let mut parts = Vec::new();
    for item in items {
        if let Some(text) = item.as_str() {
            let text = usable_text(text);
            if !text.is_empty() {
                parts.push(text);
            }
            continue;
        }
        if let Some(text) = item.get("text").or_else(|| item.get("content")).and_then(Value::as_str)
        {
            let text = usable_text(text);
            if !text.is_empty() {
                parts.push(text);
            }
        }
    }
    parts.join("\n")
}

fn usable_text(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed == "..." { String::new() } else { trimmed.to_string() }
}

fn first_path(value: Option<&Value>) -> Option<String> {
    value?.as_array()?.iter().find_map(|item| item.as_str()).map(str::to_string)
}

fn unix_ts_to_ms(value: i64) -> i64 {
    if value.abs() >= 1_000_000_000_000 { value } else { value.saturating_mul(1000) }
}

fn covered_ids(sessions: &[RawSession]) -> HashSet<String> {
    let mut covered = HashSet::new();
    for session in sessions {
        insert_id_keys(&mut covered, &session.source_id);
    }
    covered
}

fn insert_id_keys(covered: &mut HashSet<String>, source_id: &str) {
    for key in id_alias_keys(source_id) {
        covered.insert(key);
    }
}

fn covered_contains(covered: &HashSet<String>, source_id: &str) -> bool {
    id_alias_keys(source_id).iter().any(|key| covered.contains(key))
}

fn id_alias_keys(source_id: &str) -> [String; 3] {
    let bare = source_id.strip_prefix("sess_").unwrap_or(source_id);
    [source_id.to_string(), bare.to_string(), format!("sess_{bare}")]
}

fn existing_source_id(source_id: &str, paths: &[SessionPath]) -> Option<String> {
    if paths.iter().any(|path| path.source_id == source_id) {
        return None;
    }
    let aliases = id_alias_keys(source_id);
    paths
        .iter()
        .find(|path| aliases.iter().any(|alias| alias == &path.source_id))
        .map(|path| path.source_id.clone())
}

fn reconcile_file_source_ids(sessions: &mut [RawSession], paths: &[SessionPath]) {
    for session in sessions {
        if let Some(existing) = existing_source_id(&session.source_id, paths) {
            session.source_id = existing;
        }
    }
}

fn covered_for_sqlite(sessions: &[RawSession], paths: &[SessionPath]) -> HashSet<String> {
    let mut covered = covered_ids(sessions);
    for path in paths {
        if path.source_file_path.is_some() {
            insert_id_keys(&mut covered, &path.source_id);
        }
    }
    covered
}

fn scan_sqlite_sessions() -> Vec<RawSession> {
    let Some(db_path) = kiro_db_path() else {
        return Vec::new();
    };
    if !db_path.exists() {
        debug!("Kiro CLI DB not found at {}, skipping", db_path.display());
        return Vec::new();
    }
    let conn = match Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(conn) => conn,
        Err(error) => {
            warn!("failed to open {}: {error}", db_path.display());
            return Vec::new();
        }
    };

    let mut sessions = Vec::new();
    let mut seen = HashSet::new();
    if table_exists(&conn, "conversations_v2") {
        push_sqlite_v2(&conn, &mut sessions, &mut seen);
    }
    if table_exists(&conn, "conversations") {
        push_sqlite_v1(&conn, &mut sessions, &mut seen);
    }
    sessions
}

fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1", [name], |row| {
        row.get::<_, i64>(0)
    })
    .is_ok()
}

fn push_sqlite_v2(conn: &Connection, sessions: &mut Vec<RawSession>, seen: &mut HashSet<String>) {
    let mut stmt = match conn.prepare(
        "SELECT key, conversation_id, value, created_at, updated_at
         FROM conversations_v2
         ORDER BY updated_at DESC",
    ) {
        Ok(stmt) => stmt,
        Err(error) => {
            warn!("failed to query conversations_v2: {error}");
            return;
        }
    };
    let rows = match stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
        ))
    }) {
        Ok(rows) => rows,
        Err(error) => {
            warn!("failed to read conversations_v2: {error}");
            return;
        }
    };
    for row in rows {
        let (cwd, conversation_id, value_json, created_at, updated_at) = match row {
            Ok(row) => row,
            Err(error) => {
                debug!("failed to read kiro row: {error}");
                continue;
            }
        };
        if !seen.insert(conversation_id.clone()) {
            continue;
        }
        match parse_kiro_conversation(&conversation_id, &cwd, &value_json, created_at, updated_at) {
            Ok(Some(session)) => sessions.push(session),
            Ok(None) => {}
            Err(error) => debug!("failed to parse kiro conversation {conversation_id}: {error}"),
        }
    }
}

fn push_sqlite_v1(conn: &Connection, sessions: &mut Vec<RawSession>, seen: &mut HashSet<String>) {
    let mut stmt = match conn.prepare("SELECT key, value FROM conversations") {
        Ok(stmt) => stmt,
        Err(error) => {
            warn!("failed to query conversations: {error}");
            return;
        }
    };
    let rows =
        match stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))) {
            Ok(rows) => rows,
            Err(error) => {
                warn!("failed to read conversations: {error}");
                return;
            }
        };
    for row in rows {
        let (cwd, value_json) = match row {
            Ok(row) => row,
            Err(error) => {
                debug!("failed to read kiro conversations row: {error}");
                continue;
            }
        };
        let doc: Value = match serde_json::from_str(&value_json) {
            Ok(doc) => doc,
            Err(error) => {
                debug!("failed to parse kiro conversations blob: {error}");
                continue;
            }
        };
        let Some(conversation_id) = doc
            .get("conversation_id")
            .or_else(|| doc.get("conversationId"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        if !seen.insert(conversation_id.to_string()) {
            continue;
        }
        match parse_kiro_conversation(conversation_id, &cwd, &value_json, 0, 0) {
            Ok(Some(mut session)) => {
                apply_v1_timestamps(&mut session);
                sessions.push(session);
            }
            Ok(None) => {}
            Err(error) => debug!("failed to parse kiro conversation {conversation_id}: {error}"),
        }
    }
}

fn apply_v1_timestamps(session: &mut RawSession) {
    if session.started_at == 0 {
        session.started_at = first_timestamp(None, &session.messages, &[], &[]).unwrap_or(0);
    }
    if session.updated_at == Some(0) {
        session.updated_at = last_timestamp(None, &session.messages, &[], &[]);
    }
}

pub(crate) fn parse_kiro_conversation(
    conversation_id: &str,
    cwd: &str,
    value_json: &str,
    created_at: i64,
    updated_at: i64,
) -> anyhow::Result<Option<RawSession>> {
    let doc: Value = serde_json::from_str(value_json)?;

    let history = match doc.get("history").and_then(|h| h.as_array()) {
        Some(arr) => arr,
        None => return Ok(None),
    };

    let mut messages = Vec::new();

    for turn in history {
        if let Some(user_obj) = turn.get("user") {
            let content = extract_user_content(user_obj);
            let timestamp = parse_kiro_timestamp(user_obj.get("timestamp"));
            if !content.is_empty() {
                messages.push(RawMessage { role: Role::User, content, timestamp });
            }
        }

        if let Some(assistant_obj) = turn.get("assistant") {
            let content = extract_assistant_content(assistant_obj);
            let timestamp = turn
                .get("request_metadata")
                .and_then(|m| m.get("request_start_timestamp_ms"))
                .and_then(|t| t.as_i64());
            if !content.is_empty() {
                messages.push(RawMessage { role: Role::Assistant, content, timestamp });
            }
        }
    }

    if messages.is_empty() {
        return Ok(None);
    }

    Ok(Some(RawSession::search_only(
        conversation_id.to_string(),
        Some(cwd.to_string()),
        created_at,
        Some(updated_at),
        None,
        messages,
    )))
}

fn extract_user_content(user_obj: &Value) -> String {
    let content = match user_obj.get("content") {
        Some(c) => c,
        None => return String::new(),
    };

    if let Some(prompt_obj) = content.get("Prompt")
        && let Some(text) = prompt_obj.get("prompt").and_then(|p| p.as_str())
    {
        return text.to_string();
    }

    if let Some(tool_results) = content.get("ToolUseResults")
        && let Some(arr) = tool_results.get("tool_use_results").and_then(|v| v.as_array())
    {
        let mut parts = Vec::new();
        for result in arr {
            let Some(inner) = result.get("content").and_then(|c| c.as_array()) else {
                continue;
            };
            for item in inner {
                if let Some(text) = item.get("Text").and_then(|t| t.as_str()) {
                    parts.push(text.to_string());
                } else if let Some(json_val) = item.get("Json")
                    && let Ok(s) = serde_json::to_string(json_val)
                {
                    parts.push(s);
                }
            }
        }
        if !parts.is_empty() {
            return parts.join("\n");
        }
    }

    String::new()
}

fn extract_assistant_content(assistant_obj: &Value) -> String {
    if let Some(response) = assistant_obj.get("Response")
        && let Some(text) = response.get("content").and_then(|c| c.as_str())
    {
        return text.to_string();
    }

    if let Some(tool_use) = assistant_obj.get("ToolUse") {
        let mut parts = Vec::new();
        if let Some(prose) = tool_use.get("content").and_then(|c| c.as_str())
            && !prose.is_empty()
        {
            parts.push(prose.to_string());
        }
        if let Some(tool_uses) = tool_use.get("tool_uses").and_then(|v| v.as_array()) {
            for tu in tool_uses {
                let name = tu.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                let args = tu
                    .get("args")
                    .map(|a| serde_json::to_string(a).unwrap_or_default())
                    .unwrap_or_default();
                parts.push(format!("[{name}] {args}"));
            }
        }
        if !parts.is_empty() {
            return parts.join("\n");
        }
    }

    String::new()
}

fn parse_kiro_timestamp(ts: Option<&Value>) -> Option<i64> {
    ts.and_then(|t| t.as_str())
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|dt| dt.timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_file_entries_reads_v2_and_v3_skips_cli_sess_prefix() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let cli = sessions.join("cli");
        let v3 =
            sessions.join("08c01b0af7295b04").join("sess_90e28400-458e-47f0-8793-70137f0c92c5");
        fs::create_dir_all(&cli).unwrap();
        fs::create_dir_all(&v3).unwrap();
        fs::write(
            cli.join("790bb539-44be-40bd-85ac-0bb1a3fc6b47.jsonl"),
            "{\"version\":\"v1\",\"kind\":\"Prompt\",\"data\":{\"content\":[{\"kind\":\"text\",\"data\":\"v2 hi\"}],\"meta\":{\"timestamp\":1788285089}}}\n",
        )
        .unwrap();
        fs::write(
            cli.join("sess_90e28400-458e-47f0-8793-70137f0c92c5.jsonl"),
            "{\"version\":\"v1\",\"kind\":\"Prompt\",\"data\":{\"content\":[{\"kind\":\"text\",\"data\":\"should skip\"}]}}\n",
        )
        .unwrap();
        fs::write(
            v3.join("messages.jsonl"),
            "{\"id\":\"u1\",\"timestamp\":\"2026-09-01T17:52:55.268Z\",\"payload\":{\"type\":\"user\",\"content\":\"v3 hi\"}}\n",
        )
        .unwrap();

        let entries = collect_file_entries(&sessions);
        let ids: Vec<_> = entries.iter().map(|entry| entry.session_id.as_str()).collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"790bb539-44be-40bd-85ac-0bb1a3fc6b47"));
        assert!(ids.contains(&"sess_90e28400-458e-47f0-8793-70137f0c92c5"));
    }

    fn session_path(source_id: &str, file: Option<&str>) -> SessionPath {
        SessionPath {
            source_id: source_id.to_string(),
            directory: None,
            source_file_path: file.map(str::to_string),
            repo_remote: None,
            repo_slug: None,
            repo_name: None,
        }
    }

    fn raw(source_id: &str) -> RawSession {
        RawSession::search_only(source_id, None, 1, Some(2), None, Vec::new())
    }

    #[test]
    fn existing_source_id_remaps_sess_prefix_to_store_row() {
        let paths = [session_path("790bb539-44be-40bd-85ac-0bb1a3fc6b47", None)];
        assert_eq!(
            existing_source_id("sess_790bb539-44be-40bd-85ac-0bb1a3fc6b47", &paths).as_deref(),
            Some("790bb539-44be-40bd-85ac-0bb1a3fc6b47")
        );
        assert_eq!(existing_source_id("790bb539-44be-40bd-85ac-0bb1a3fc6b47", &paths), None);
    }

    #[test]
    fn reconcile_file_source_ids_keeps_existing_store_id() {
        let paths = [session_path("790bb539-44be-40bd-85ac-0bb1a3fc6b47", None)];
        let mut sessions = [raw("sess_790bb539-44be-40bd-85ac-0bb1a3fc6b47")];
        reconcile_file_source_ids(&mut sessions, &paths);
        assert_eq!(sessions[0].source_id, "790bb539-44be-40bd-85ac-0bb1a3fc6b47");
    }

    #[test]
    fn covered_for_sqlite_ignores_sqlite_only_store_rows() {
        let file = raw("file-1");
        let paths =
            [session_path("file-1", Some("/tmp/file.jsonl")), session_path("sqlite-only", None)];
        let covered = covered_for_sqlite(&[file], &paths);
        assert!(covered_contains(&covered, "file-1"));
        assert!(!covered_contains(&covered, "sqlite-only"));
    }

    #[test]
    fn apply_v1_timestamps_replaces_zero_updated_at() {
        let json = r#"{
            "history": [{
                "user": {
                    "content": {"Prompt": {"prompt": "hello"}},
                    "timestamp": "2026-04-11T00:34:50.549369+08:00"
                },
                "assistant": {
                    "Response": {"message_id": "m1", "content": "world"}
                },
                "request_metadata": {"request_start_timestamp_ms": 1775838890550}
            }]
        }"#;
        let mut session = parse_kiro_conversation("c", "/proj", json, 0, 0).unwrap().unwrap();
        assert_eq!(session.updated_at, Some(0));
        apply_v1_timestamps(&mut session);
        assert_eq!(session.started_at, 1_775_838_890_549);
        assert_eq!(session.updated_at, Some(1_775_838_890_550));
    }
}
