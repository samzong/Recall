use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use tracing::debug;

use crate::adapters::file_scan::{self, FileScanEntry, FileScanOptions};
use crate::adapters::json_util::json_i64;
use crate::adapters::paths::resolve_home_dir;
use crate::adapters::{RawMessage, RawSession, ResumeCommand, SyncScanResult};
use crate::db::store::Store;
use crate::types::Role;

pub(super) fn resume_command(source_id: &str) -> Option<ResumeCommand> {
    find_store_db(source_id)?;
    Some(ResumeCommand {
        program: "agent".to_string(),
        args: vec!["--resume".to_string(), source_id.to_string()],
    })
}

pub(super) fn scan_uncovered(
    covered: &HashSet<String>,
    usage_parser_version: u32,
) -> anyhow::Result<Vec<RawSession>> {
    let Some(chats_dir) = resolve_chats_dir()? else {
        return Ok(vec![]);
    };
    let mut sessions = Vec::new();
    for entry in collect_store_entries(&chats_dir, covered) {
        let Some(mtime_ms) = file_scan::stat_mtime_ms(&entry.stat_target) else {
            continue;
        };
        match parse_store_entry(&entry, mtime_ms, usage_parser_version) {
            Ok(Some(raw)) => sessions.push(raw),
            Ok(None) => {}
            Err(err) => {
                debug!("failed to parse Cursor CLI store {}: {err}", entry.stat_target.display());
            }
        }
    }
    Ok(sessions)
}

pub(super) fn scan_for_sync(
    store: &Store,
    since_ts: Option<i64>,
    covered: &HashSet<String>,
    usage_parser_version: u32,
) -> anyhow::Result<SyncScanResult> {
    let Some(chats_dir) = resolve_chats_dir()? else {
        return Ok(SyncScanResult { sessions: vec![], stats: Default::default() });
    };
    let entries = collect_store_entries(&chats_dir, covered);
    file_scan::run_file_scan_with_options(
        store,
        "cursor",
        since_ts,
        FileScanOptions { usage_parser_version: Some(usage_parser_version), ..Default::default() },
        entries,
        |entry, mtime_ms| parse_store_entry(&entry, mtime_ms, usage_parser_version),
    )
}

fn resolve_chats_dir() -> anyhow::Result<Option<PathBuf>> {
    resolve_home_dir(".cursor/chats", "~/.cursor/chats not found, skipping Cursor CLI store")
}

fn collect_store_entries(chats_dir: &Path, covered: &HashSet<String>) -> Vec<FileScanEntry> {
    let mut entries = Vec::new();
    let workspaces = match fs::read_dir(chats_dir) {
        Ok(read) => read,
        Err(err) => {
            debug!("cannot read {}: {err}", chats_dir.display());
            return entries;
        }
    };
    for workspace in workspaces.flatten() {
        let workspace_path = workspace.path();
        if !workspace_path.is_dir() {
            continue;
        }
        let sessions = match fs::read_dir(&workspace_path) {
            Ok(read) => read,
            Err(err) => {
                debug!("cannot read {}: {err}", workspace_path.display());
                continue;
            }
        };
        for session in sessions.flatten() {
            let session_path = session.path();
            if !session_path.is_dir() {
                continue;
            }
            let Some(session_id) = session_path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if uuid::Uuid::try_parse(session_id).is_err() {
                continue;
            }
            if covered.contains(session_id) {
                continue;
            }
            let store_db = session_path.join("store.db");
            if !store_db.is_file() {
                continue;
            }
            entries.push(FileScanEntry {
                session_id: session_id.to_string(),
                stat_target: store_db,
                directory: None,
            });
        }
    }
    entries
}

fn find_store_db(source_id: &str) -> Option<PathBuf> {
    if uuid::Uuid::try_parse(source_id).is_err() {
        return None;
    }
    let chats_dir = resolve_chats_dir().ok().flatten()?;
    let workspaces = fs::read_dir(chats_dir).ok()?;
    for workspace in workspaces.flatten() {
        let store_db = workspace.path().join(source_id).join("store.db");
        if store_db.is_file() {
            return Some(store_db);
        }
    }
    None
}

fn parse_store_entry(
    entry: &FileScanEntry,
    mtime_ms: i64,
    usage_parser_version: u32,
) -> anyhow::Result<Option<RawSession>> {
    parse_store_db(&entry.stat_target, &entry.session_id, mtime_ms, usage_parser_version)
}

fn parse_store_db(
    path: &Path,
    session_id: &str,
    mtime_ms: i64,
    usage_parser_version: u32,
) -> anyhow::Result<Option<RawSession>> {
    let conn = match Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(conn) => conn,
        Err(err) => {
            debug!("cannot open Cursor CLI store {}: {err}", path.display());
            return Ok(None);
        }
    };

    let Some(store_meta) = read_store_meta(&conn) else {
        return Ok(None);
    };
    let Some(root_id) = store_meta.get("latestRootBlobId").and_then(Value::as_str) else {
        return Ok(None);
    };
    let Some(root) = read_blob(&conn, root_id) else {
        return Ok(None);
    };
    if root.is_empty() {
        return Ok(None);
    }

    let root_fields = decode_protobuf_fields(&root);
    let mut messages = Vec::new();
    for blob_id in message_blob_ids(&root_fields) {
        let Some(blob) = read_blob(&conn, &blob_id) else {
            continue;
        };
        if let Some(message) = parse_message_blob(&blob) {
            messages.push(message);
        }
    }
    if messages.is_empty() {
        return Ok(None);
    }

    let sidecar = read_sidecar_meta(path);
    let started_at = json_i64(store_meta.get("createdAt"))
        .or_else(|| sidecar.as_ref().and_then(|value| json_i64(value.get("createdAtMs"))))
        .or_else(|| first_varint_field(&root_fields, 26))
        .unwrap_or(mtime_ms);
    let updated_at = sidecar
        .as_ref()
        .and_then(|value| json_i64(value.get("updatedAtMs")))
        .or_else(|| first_varint_field(&root_fields, 26))
        .or(Some(mtime_ms));
    let directory = first_len_field(&root_fields, 9).and_then(|uri| directory_from_file_uri(&uri));
    let entrypoint = first_len_field(&root_fields, 22).filter(|value| !value.is_empty());

    let mut session = RawSession::search_only(
        session_id.to_string(),
        directory,
        started_at,
        updated_at,
        entrypoint,
        messages,
    )
    .with_usage(Vec::new(), usage_parser_version);
    session.source_file_path = path.to_str().map(str::to_string);
    Ok(Some(session))
}

fn read_store_meta(conn: &Connection) -> Option<Value> {
    let raw: String =
        conn.query_row("SELECT value FROM meta LIMIT 1", [], |row| row.get(0)).ok()?;
    if let Some(bytes) = decode_hex(&raw)
        && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
    {
        return Some(value);
    }
    serde_json::from_str(&raw).ok()
}

fn read_sidecar_meta(store_db: &Path) -> Option<Value> {
    let path = store_db.parent()?.join("meta.json");
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn read_blob(conn: &Connection, id: &str) -> Option<Vec<u8>> {
    conn.query_row("SELECT data FROM blobs WHERE id = ?1", [id], |row| row.get(0)).ok()
}

fn parse_message_blob(data: &[u8]) -> Option<RawMessage> {
    if data.first().is_none_or(|byte| *byte != b'{' && *byte != b'[') {
        return None;
    }
    let value: Value = serde_json::from_slice(data).ok()?;
    let role = match value.get("role").and_then(Value::as_str) {
        Some("user") => Role::User,
        Some("assistant") => Role::Assistant,
        _ => return None,
    };
    let content = render_message_content(value.get("content"), matches!(role, Role::User))?;
    Some(RawMessage { role, content, timestamp: None })
}

fn render_message_content(content: Option<&Value>, is_user: bool) -> Option<String> {
    let rendered = match content? {
        Value::String(text) => extract_visible_text(text, is_user).unwrap_or_default(),
        Value::Array(items) => {
            let mut parts = Vec::new();
            for item in items {
                match item.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(text) = item.get("text").and_then(Value::as_str)
                            && let Some(visible) = extract_visible_text(text, is_user)
                        {
                            parts.push(visible);
                        }
                    }
                    Some("tool-call" | "tool_use" | "redacted-reasoning" | "reasoning") => {}
                    _ => {}
                }
            }
            parts.join("\n")
        }
        _ => String::new(),
    };
    let trimmed = rendered.trim();
    if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
}

fn extract_visible_text(text: &str, is_user: bool) -> Option<String> {
    if is_user {
        if let Some(query) = extract_tagged_block(text, "user_query") {
            let trimmed = query.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
        let trimmed = text.trim();
        if trimmed.starts_with("<user_info>")
            || trimmed.starts_with("<agent_skill")
            || trimmed.starts_with("<available_skills>")
            || trimmed.starts_with("<manually_attached")
            || trimmed.starts_with("<cursor_commands>")
        {
            return None;
        }
        let stripped = super::strip_user_query_envelope(trimmed).trim();
        if stripped.is_empty() { None } else { Some(stripped.to_string()) }
    } else {
        let trimmed = text.trim();
        if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
    }
}

fn extract_tagged_block<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&close)? + start;
    Some(&text[start..end])
}

fn directory_from_file_uri(uri: &str) -> Option<String> {
    let path = uri.strip_prefix("file://")?;
    if path.is_empty() {
        return None;
    }
    Some(path.to_string())
}

struct ProtoField<'a> {
    number: u64,
    varint: Option<u64>,
    bytes: Option<&'a [u8]>,
}

fn decode_protobuf_fields(data: &[u8]) -> Vec<ProtoField<'_>> {
    let mut fields = Vec::new();
    let mut offset = 0;
    while offset < data.len() {
        let Some((key, next)) = read_varint(data, offset) else {
            break;
        };
        offset = next;
        let field_number = key >> 3;
        match key & 7 {
            0 => {
                let Some((value, next)) = read_varint(data, offset) else {
                    break;
                };
                offset = next;
                fields.push(ProtoField { number: field_number, varint: Some(value), bytes: None });
            }
            1 => {
                if offset + 8 > data.len() {
                    break;
                }
                offset += 8;
            }
            2 => {
                let Some((len, next)) = read_varint(data, offset) else {
                    break;
                };
                offset = next;
                let end = offset.saturating_add(len as usize);
                if end > data.len() {
                    break;
                }
                fields.push(ProtoField {
                    number: field_number,
                    varint: None,
                    bytes: Some(&data[offset..end]),
                });
                offset = end;
            }
            5 => {
                if offset + 4 > data.len() {
                    break;
                }
                offset += 4;
            }
            _ => break,
        }
    }
    fields
}

fn read_varint(data: &[u8], mut offset: usize) -> Option<(u64, usize)> {
    let mut value = 0u64;
    let mut shift = 0;
    while offset < data.len() {
        let byte = data[offset];
        offset += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some((value, offset));
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
    None
}

fn message_blob_ids(fields: &[ProtoField<'_>]) -> Vec<String> {
    fields
        .iter()
        .filter(|field| field.number == 1)
        .filter_map(|field| field.bytes)
        .filter(|bytes| bytes.len() == 32)
        .map(to_hex)
        .collect()
}

fn first_len_field(fields: &[ProtoField<'_>], number: u64) -> Option<String> {
    fields.iter().find(|field| field.number == number).and_then(|field| {
        field
            .bytes
            .and_then(|bytes| String::from_utf8(bytes.to_vec()).ok())
            .filter(|s| !s.is_empty())
    })
}

fn first_varint_field(fields: &[ProtoField<'_>], number: u64) -> Option<i64> {
    fields
        .iter()
        .find(|field| field.number == number)
        .and_then(|field| field.varint)
        .and_then(|value| i64::try_from(value).ok())
}

fn decode_hex(input: &str) -> Option<Vec<u8>> {
    if input.is_empty() || !input.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(input.len() / 2);
    let bytes = input.as_bytes();
    for pair in bytes.chunks_exact(2) {
        out.push((hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?);
    }
    Some(out)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "recall-cursor-cli-{}-{}",
            label,
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn encode_varint(mut value: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let mut byte = (value & 0x7f) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if value == 0 {
                break;
            }
        }
        out
    }

    fn encode_len_field(number: u64, bytes: &[u8]) -> Vec<u8> {
        let mut out = encode_varint((number << 3) | 2);
        out.extend(encode_varint(bytes.len() as u64));
        out.extend(bytes);
        out
    }

    fn encode_varint_field(number: u64, value: u64) -> Vec<u8> {
        let mut out = encode_varint(number << 3);
        out.extend(encode_varint(value));
        out
    }

    fn write_store(
        path: &Path,
        session_id: &str,
        user: &str,
        assistant: &str,
        directory: &str,
    ) -> PathBuf {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let user_id = [0x11u8; 32];
        let assistant_id = [0x22u8; 32];
        let root_id = [0x33u8; 32];
        let mut root = Vec::new();
        root.extend(encode_len_field(1, &user_id));
        root.extend(encode_len_field(1, &assistant_id));
        root.extend(encode_len_field(9, format!("file://{directory}").as_bytes()));
        root.extend(encode_len_field(22, b"cli"));
        root.extend(encode_varint_field(26, 1_700_000_000_000));

        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE blobs (id TEXT PRIMARY KEY, data BLOB);
             CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO blobs (id, data) VALUES (?1, ?2)",
            rusqlite::params![to_hex(&user_id), user.as_bytes()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO blobs (id, data) VALUES (?1, ?2)",
            rusqlite::params![to_hex(&assistant_id), assistant.as_bytes()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO blobs (id, data) VALUES (?1, ?2)",
            rusqlite::params![to_hex(&root_id), root],
        )
        .unwrap();
        let meta = serde_json::json!({
            "agentId": session_id,
            "latestRootBlobId": to_hex(&root_id),
            "createdAt": 1_700_000_000_000_i64,
            "name": "New Agent",
            "mode": "default"
        });
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('0', ?1)",
            [to_hex(meta.to_string().as_bytes())],
        )
        .unwrap();
        fs::write(
            path.parent().unwrap().join("meta.json"),
            serde_json::json!({
                "schemaVersion": 1,
                "createdAtMs": 1_700_000_000_000_i64,
                "updatedAtMs": 1_700_000_100_000_i64,
                "hasConversation": true
            })
            .to_string(),
        )
        .unwrap();
        path.to_path_buf()
    }

    #[test]
    fn parse_store_db_extracts_user_query_and_directory() {
        let root = temp_root("parse");
        let session_id = uuid::Uuid::new_v4().to_string();
        let store_db = root.join("ws").join(&session_id).join("store.db");
        write_store(
            &store_db,
            &session_id,
            r#"{"role":"user","content":"<user_info>skip</user_info>\n<user_query>\nhello store\n</user_query>"}"#,
            r#"{"role":"assistant","content":[{"type":"redacted-reasoning","data":"x"},{"type":"text","text":"hi"},{"type":"tool-call","toolCallId":"t1"}]}"#,
            "/tmp/cursor-cli-project",
        );
        let raw = parse_store_db(&store_db, &session_id, 9, 2).unwrap().unwrap();
        assert_eq!(raw.source_id, session_id);
        assert_eq!(raw.messages.len(), 2);
        assert_eq!(raw.messages[0].content, "hello store");
        assert_eq!(raw.messages[1].content, "hi");
        assert_eq!(raw.directory.as_deref(), Some("/tmp/cursor-cli-project"));
        assert_eq!(raw.entrypoint.as_deref(), Some("cli"));
        assert_eq!(raw.started_at, 1_700_000_000_000);
        assert_eq!(raw.updated_at, Some(1_700_000_100_000));
        assert_eq!(raw.source_file_path.as_deref(), store_db.to_str());
        assert_eq!(raw.usage_parser_version, Some(2));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn parse_store_db_skips_empty_root() {
        let root = temp_root("empty");
        let session_id = uuid::Uuid::new_v4().to_string();
        let store_db = root.join("ws").join(&session_id).join("store.db");
        fs::create_dir_all(store_db.parent().unwrap()).unwrap();
        let conn = Connection::open(&store_db).unwrap();
        conn.execute_batch(
            "CREATE TABLE blobs (id TEXT PRIMARY KEY, data BLOB);
             CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);",
        )
        .unwrap();
        let sha_empty = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        conn.execute("INSERT INTO blobs (id, data) VALUES (?1, X'')", [sha_empty]).unwrap();
        let meta = serde_json::json!({
            "agentId": session_id,
            "latestRootBlobId": sha_empty
        });
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('0', ?1)",
            [to_hex(meta.to_string().as_bytes())],
        )
        .unwrap();
        assert!(parse_store_db(&store_db, &session_id, 1, 2).unwrap().is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn collect_store_entries_skips_covered_ids() {
        let root = temp_root("collect");
        let keep = uuid::Uuid::new_v4().to_string();
        let skip = uuid::Uuid::new_v4().to_string();
        write_store(
            &root.join("ws").join(&keep).join("store.db"),
            &keep,
            r#"{"role":"user","content":"<user_query>keep</user_query>"}"#,
            r#"{"role":"assistant","content":[{"type":"text","text":"ok"}]}"#,
            "/tmp/keep",
        );
        write_store(
            &root.join("ws").join(&skip).join("store.db"),
            &skip,
            r#"{"role":"user","content":"<user_query>skip</user_query>"}"#,
            r#"{"role":"assistant","content":[{"type":"text","text":"no"}]}"#,
            "/tmp/skip",
        );
        let covered = HashSet::from([skip]);
        let entries = collect_store_entries(&root, &covered);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id, keep);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn extract_visible_text_ignores_user_info_without_query() {
        assert_eq!(extract_visible_text("<user_info>os and skills</user_info>", true), None);
        assert_eq!(
            extract_visible_text("<user_query>\nreal ask\n</user_query>", true).as_deref(),
            Some("real ask")
        );
    }

    #[test]
    fn resume_command_requires_existing_store() {
        assert!(resume_command("not-a-uuid").is_none());
        assert!(resume_command("00000000-0000-0000-0000-000000000000").is_none());
    }
}
