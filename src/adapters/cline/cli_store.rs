use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use tracing::debug;

use crate::adapters::file_scan::{self, FileScanEntry};
use crate::adapters::json_util;
use crate::adapters::paths;
use crate::adapters::{RawMessage, RawSession, ResumeCommand, SyncScanResult};
use crate::db::store::Store;
use crate::types::Role;

pub(super) fn resume_command(source_id: &str) -> Option<ResumeCommand> {
    let sessions_dir = resolve_sessions_dir().ok().flatten()?;
    find_messages_path(&sessions_dir, source_id)?;
    Some(ResumeCommand {
        program: "cline".to_string(),
        args: vec!["--id".to_string(), source_id.to_string()],
    })
}

pub(super) fn scan_uncovered(covered: &HashSet<String>) -> anyhow::Result<Vec<RawSession>> {
    let Some(sessions_dir) = resolve_sessions_dir()? else {
        return Ok(vec![]);
    };
    scan_session_dirs(&sessions_dir, covered)
}

pub(super) fn scan_for_sync(
    store: &Store,
    since_ts: Option<i64>,
    covered: &HashSet<String>,
) -> anyhow::Result<SyncScanResult> {
    let Some(sessions_dir) = resolve_sessions_dir()? else {
        return Ok(SyncScanResult { sessions: vec![], stats: Default::default() });
    };
    let entries = collect_session_entries(&sessions_dir, covered);
    file_scan::run_file_scan_with_options_and_mtime(
        store,
        "cline",
        since_ts,
        Default::default(),
        entries,
        scan_timestamp_ms,
        parse_session_entry,
    )
}

fn scan_session_dirs(
    sessions_dir: &Path,
    covered: &HashSet<String>,
) -> anyhow::Result<Vec<RawSession>> {
    let mut sessions = Vec::new();
    for entry in collect_session_entries(sessions_dir, covered) {
        let Some(mtime_ms) = scan_timestamp_ms(&entry) else {
            continue;
        };
        match parse_session_entry(entry, mtime_ms) {
            Ok(Some(raw)) => sessions.push(raw),
            Ok(None) => {}
            Err(err) => {
                debug!("failed to parse Cline CLI session: {err}");
            }
        }
    }
    Ok(sessions)
}

fn resolve_sessions_dir() -> anyhow::Result<Option<PathBuf>> {
    let Some(data_dir) = resolve_data_dir()? else {
        return Ok(None);
    };
    let sessions_dir = data_dir.join("sessions");
    if !sessions_dir.is_dir() {
        debug!("Cline CLI sessions directory not found, skipping Cline CLI store");
        return Ok(None);
    }
    Ok(Some(sessions_dir))
}

fn resolve_data_dir() -> anyhow::Result<Option<PathBuf>> {
    if let Some(path) = env_dir("CLINE_DATA_DIR") {
        return Ok(path.is_dir().then_some(path));
    }
    if let Some(path) = env_dir("CLINE_DIR") {
        let data_dir = path.join("data");
        return Ok(data_dir.is_dir().then_some(data_dir));
    }
    paths::resolve_home_dir(
        ".cline/data",
        "Cline CLI data directory not found, skipping Cline CLI store",
    )
}

fn env_dir(name: &str) -> Option<PathBuf> {
    let value = std::env::var(name).ok()?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(PathBuf::from(trimmed))
}

fn collect_session_entries(sessions_dir: &Path, covered: &HashSet<String>) -> Vec<FileScanEntry> {
    let mut entries = Vec::new();
    let read = match fs::read_dir(sessions_dir) {
        Ok(read) => read,
        Err(err) => {
            debug!("cannot read {}: {err}", sessions_dir.display());
            return entries;
        }
    };
    for session in read.flatten() {
        let session_path = session.path();
        if !session_path.is_dir() {
            continue;
        }
        let Some(session_id) = session_path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if covered.contains(session_id) {
            continue;
        }
        let Some(messages_path) = find_messages_path(sessions_dir, session_id) else {
            continue;
        };
        entries.push(FileScanEntry {
            session_id: session_id.to_string(),
            stat_target: messages_path,
            directory: None,
        });
    }
    entries
}

fn find_messages_path(sessions_dir: &Path, session_id: &str) -> Option<PathBuf> {
    let path = sessions_dir.join(session_id).join(format!("{session_id}.messages.json"));
    path.is_file().then_some(path)
}

fn scan_timestamp_ms(entry: &FileScanEntry) -> Option<i64> {
    let messages_mtime = file_scan::stat_mtime_ms(&entry.stat_target);
    let manifest_mtime = entry
        .stat_target
        .parent()
        .map(|dir| dir.join(format!("{}.json", entry.session_id)))
        .and_then(|path| file_scan::stat_mtime_ms(&path));
    match (messages_mtime, manifest_mtime) {
        (Some(messages), Some(manifest)) => Some(messages.max(manifest)),
        (mtime, sidecar) => mtime.or(sidecar),
    }
}

fn parse_session_entry(entry: FileScanEntry, mtime_ms: i64) -> anyhow::Result<Option<RawSession>> {
    parse_session(&entry.stat_target, &entry.session_id, mtime_ms)
}

fn parse_session(
    messages_path: &Path,
    session_id: &str,
    mtime_ms: i64,
) -> anyhow::Result<Option<RawSession>> {
    let messages = match load_messages(messages_path) {
        Ok(messages) => messages,
        Err(err) => {
            debug!("failed to parse Cline CLI messages {}: {err}", messages_path.display());
            return Ok(None);
        }
    };
    if messages.is_empty() {
        return Ok(None);
    }

    let manifest = messages_path
        .parent()
        .map(|dir| dir.join(format!("{session_id}.json")))
        .and_then(|path| read_json(&path));
    let started_at = manifest
        .as_ref()
        .and_then(|value| json_util::rfc3339_ms(value.get("started_at")))
        .or_else(|| messages.first().and_then(|message| message.timestamp))
        .unwrap_or(0);
    let directory = manifest.as_ref().and_then(directory_from_manifest);
    let custom_title = manifest.as_ref().and_then(title_from_manifest);

    let mut raw = RawSession::search_only(
        session_id.to_string(),
        directory,
        started_at,
        Some(mtime_ms),
        Some("cli".to_string()),
        messages,
    );
    raw.source_file_path = messages_path.to_str().map(str::to_string);
    raw.custom_title = custom_title;
    Ok(Some(raw))
}

fn load_messages(path: &Path) -> anyhow::Result<Vec<RawMessage>> {
    let root = read_json(path).ok_or_else(|| anyhow::anyhow!("unreadable messages file"))?;
    let Some(messages) = root.get("messages").and_then(Value::as_array) else {
        return Ok(vec![]);
    };

    let mut result = Vec::new();
    for message in messages {
        let role = match message.get("role").and_then(Value::as_str) {
            Some("user") => Role::User,
            Some("assistant") => Role::Assistant,
            _ => continue,
        };
        let Some(content) = extract_message_text(message) else {
            continue;
        };
        let timestamp = json_util::json_i64(message.get("ts"));
        result.push(RawMessage { role, content, timestamp });
    }
    Ok(result)
}

fn extract_message_text(message: &Value) -> Option<String> {
    match message.get("content") {
        Some(Value::String(text)) => {
            let text = unwrap_user_input(text);
            (!text.trim().is_empty()).then_some(text)
        }
        Some(Value::Array(parts)) => {
            let mut texts = Vec::new();
            for part in parts {
                if part.get("type").and_then(Value::as_str) != Some("text") {
                    continue;
                }
                let Some(text) = part.get("text").and_then(Value::as_str) else {
                    continue;
                };
                let text = unwrap_user_input(text);
                if !text.trim().is_empty() {
                    texts.push(text);
                }
            }
            if texts.is_empty() { None } else { Some(texts.join("\n")) }
        }
        _ => None,
    }
}

fn unwrap_user_input(text: &str) -> String {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix("<user_input") else {
        return trimmed.to_string();
    };
    let Some(start) = rest.find('>') else {
        return trimmed.to_string();
    };
    let Some(inner) = rest[start + 1..].strip_suffix("</user_input>") else {
        return trimmed.to_string();
    };
    inner.to_string()
}

fn directory_from_manifest(manifest: &Value) -> Option<String> {
    ["workspace_root", "cwd"].into_iter().find_map(|field| {
        manifest
            .get(field)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

fn title_from_manifest(manifest: &Value) -> Option<String> {
    manifest
        .get("metadata")
        .and_then(|metadata| metadata.get("title"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_string)
}

fn read_json(path: &Path) -> Option<Value> {
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
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
            "recall-cline-cli-test-{}-{}",
            label,
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_session(root: &Path, session_id: &str, messages: &str, manifest: &str) -> PathBuf {
        let session_dir = root.join(session_id);
        fs::create_dir_all(&session_dir).unwrap();
        let messages_path = session_dir.join(format!("{session_id}.messages.json"));
        fs::write(&messages_path, messages).unwrap();
        fs::write(session_dir.join(format!("{session_id}.json")), manifest).unwrap();
        messages_path
    }

    fn sample_messages(user: &str, assistant: &str) -> String {
        serde_json::json!({
            "version": 1,
            "updated_at": "2026-09-01T18:05:34.196Z",
            "sessionId": "ignored",
            "messages": [
                {
                    "id": "msg_user",
                    "role": "user",
                    "content": [{"type": "text", "text": user}],
                    "ts": 1_788_285_882_068_i64
                },
                {
                    "id": "msg_assistant",
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "thinking": "plan first"},
                        {"type": "text", "text": assistant},
                        {
                            "type": "tool_use",
                            "id": "call_1",
                            "name": "read_files",
                            "input": {"files": [{"path": "/tmp/x"}]}
                        },
                        {"type": "tool_result", "tool_use_id": "call_1", "content": "skip"}
                    ],
                    "ts": 1_788_285_884_886_i64
                }
            ]
        })
        .to_string()
    }

    fn sample_manifest(session_id: &str, workspace: &str, title: &str) -> String {
        serde_json::json!({
            "version": 1,
            "session_id": session_id,
            "source": "cli",
            "started_at": "2026-09-01T18:04:28.712Z",
            "ended_at": "2026-09-01T18:08:12.610Z",
            "cwd": "/tmp/other",
            "workspace_root": workspace,
            "prompt": title,
            "metadata": { "title": title }
        })
        .to_string()
    }

    fn make_existing_session(source_id: &str, updated_at: i64) -> Session {
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
            message_count: 1,
            entrypoint: None,
            custom_title: None,
            summary: None,
            duration_minutes: None,
            source_file_path: None,
            is_import: false,
        }
    }

    #[test]
    fn parse_session_extracts_text_and_skips_tools() {
        let root = temp_root("parse");
        let session_id = "1788285868712_2wszo";
        let path = write_session(
            &root,
            session_id,
            &sample_messages("<user_input mode=\"act\">hello, analyze</user_input>", "done"),
            &sample_manifest(session_id, "/work/recall", "hello, analyze"),
        );
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();
        let raw = parse_session(&path, session_id, mtime).unwrap().unwrap();
        assert_eq!(raw.source_id, session_id);
        assert_eq!(raw.messages.len(), 2);
        assert_eq!(raw.messages[0].role, Role::User);
        assert_eq!(raw.messages[0].content, "hello, analyze");
        assert_eq!(raw.messages[1].role, Role::Assistant);
        assert_eq!(raw.messages[1].content, "done");
        assert_eq!(raw.directory.as_deref(), Some("/work/recall"));
        assert_eq!(raw.entrypoint.as_deref(), Some("cli"));
        assert_eq!(raw.started_at, 1_788_285_868_712);
        assert_eq!(raw.updated_at, Some(mtime));
        assert_eq!(raw.source_file_path.as_deref(), path.to_str());
        assert_eq!(raw.custom_title.as_deref(), Some("hello, analyze"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn parse_session_skips_assistant_without_text() {
        let root = temp_root("empty-assistant");
        let session_id = "1788285542858_9c8cv";
        let messages = serde_json::json!({
            "messages": [
                {
                    "role": "user",
                    "content": [{"type": "text", "text": "pong"}],
                    "ts": 10
                },
                {
                    "role": "assistant",
                    "content": [],
                    "ts": 11
                }
            ]
        })
        .to_string();
        let path = write_session(&root, session_id, &messages, "{}");
        let raw = parse_session(&path, session_id, 20).unwrap().unwrap();
        assert_eq!(raw.messages.len(), 1);
        assert_eq!(raw.messages[0].content, "pong");
        assert_eq!(raw.started_at, 10);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn parse_session_skips_empty_messages() {
        let root = temp_root("empty");
        let session_id = "empty_sess";
        let path = write_session(&root, session_id, r#"{"messages":[]}"#, "{}");
        assert!(parse_session(&path, session_id, 1).unwrap().is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn collect_session_entries_skips_covered_ids() {
        let root = temp_root("collect");
        write_session(
            &root,
            "keep_1",
            &sample_messages("keep", "ok"),
            &sample_manifest("keep_1", "/tmp/keep", "keep"),
        );
        write_session(
            &root,
            "skip_1",
            &sample_messages("skip", "no"),
            &sample_manifest("skip_1", "/tmp/skip", "skip"),
        );
        let covered = HashSet::from(["skip_1".to_string()]);
        let entries = collect_session_entries(&root, &covered);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id, "keep_1");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn scan_session_dirs_reads_uncovered_sessions() {
        let root = temp_root("scan");
        write_session(
            &root,
            "1788285868712_2wszo",
            &sample_messages("hello", "world"),
            &sample_manifest("1788285868712_2wszo", "/repo", "hello"),
        );
        let sessions = scan_session_dirs(&root, &HashSet::new()).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].messages[0].content, "hello");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn scan_for_sync_skips_unchanged_session() {
        let root = temp_root("skip");
        let session_id = "1788285868712_2wszo";
        let path = write_session(
            &root,
            session_id,
            &sample_messages("hello", "world"),
            &sample_manifest(session_id, "/repo", "hello"),
        );
        let mtime = scan_timestamp_ms(&FileScanEntry {
            session_id: session_id.to_string(),
            stat_target: path,
            directory: None,
        })
        .unwrap();
        let store = setup_store();
        store.insert_session(&make_existing_session(session_id, mtime)).unwrap();
        let result = file_scan::run_file_scan_with_options_and_mtime(
            &store,
            "cline",
            None,
            Default::default(),
            collect_session_entries(&root, &HashSet::new()),
            scan_timestamp_ms,
            parse_session_entry,
        )
        .unwrap();
        assert_eq!(result.sessions.len(), 0);
        assert_eq!(result.stats.skipped_sessions, 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn resume_command_requires_existing_session() {
        assert!(resume_command("missing_session").is_none());
    }

    #[test]
    fn unwrap_user_input_leaves_plain_text() {
        assert_eq!(unwrap_user_input("just text"), "just text");
        assert_eq!(unwrap_user_input("<user_input mode=\"plan\">ask</user_input>"), "ask");
    }
}
