use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;
use tracing::warn;

use crate::adapters::file_scan::{self, FileScanEntry, FileScanOptions};
use crate::adapters::json_util::{jsonl_indexed, rfc3339_ms};
use crate::adapters::paths::resolve_home_dir;
use crate::adapters::usage::usage_count;
use crate::adapters::{
    RawMessage, RawSession, ResumeCommand, SourceAdapter, SyncScanResult, SyncScanStats,
    first_timestamp,
};
use crate::db::store::Store;
use crate::types::{RawUsageEvent, Role};

const USAGE_PARSER_VERSION: u32 = 1;

pub(crate) struct QwenAdapter;

impl SourceAdapter for QwenAdapter {
    fn id(&self) -> &str {
        "qwen-code"
    }

    fn label(&self) -> &str {
        "QW"
    }

    fn usage_parser_version(&self) -> Option<u32> {
        Some(USAGE_PARSER_VERSION)
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        Some(ResumeCommand {
            program: "qwen".to_string(),
            args: vec!["--resume".to_string(), source_id.to_string()],
        })
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let Some(runtime_dir) = resolve_qwen_runtime_dir()? else {
            return Ok(vec![]);
        };
        scan_qwen_sessions(&runtime_dir)
    }

    fn scan_for_sync(
        &self,
        store: &Store,
        since_ts: Option<i64>,
        _include_events: bool,
    ) -> anyhow::Result<Option<SyncScanResult>> {
        let Some(runtime_dir) = resolve_qwen_runtime_dir()? else {
            return Ok(Some(SyncScanResult { sessions: vec![], stats: SyncScanStats::default() }));
        };
        Ok(Some(file_scan::run_file_scan_with_options(
            store,
            "qwen-code",
            since_ts,
            FileScanOptions {
                usage_parser_version: Some(USAGE_PARSER_VERSION),
                event_parser_version: None,
                metadata_parser_version: None,
            },
            collect_session_entries(&runtime_dir),
            parse_qwen_session_file,
        )?))
    }
}

fn resolve_qwen_runtime_dir() -> anyhow::Result<Option<PathBuf>> {
    if let Some(dir) = env_path("QWEN_RUNTIME_DIR") {
        return existing_dir(dir, "QWEN_RUNTIME_DIR not found, skipping Qwen Code");
    }
    if let Some(dir) = env_path("QWEN_HOME") {
        return existing_dir(dir, "QWEN_HOME not found, skipping Qwen Code");
    }
    resolve_home_dir(".qwen", "~/.qwen not found, skipping Qwen Code")
}

fn env_path(name: &str) -> Option<PathBuf> {
    let raw = std::env::var(name).ok().filter(|value| !value.is_empty())?;
    expand_user_path(&raw)
}

fn expand_user_path(raw: &str) -> Option<PathBuf> {
    if raw == "~" {
        return dirs::home_dir();
    }
    if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
        return dirs::home_dir().map(|home| home.join(rest));
    }
    Some(PathBuf::from(raw))
}

fn existing_dir(dir: PathBuf, missing_message: &str) -> anyhow::Result<Option<PathBuf>> {
    if !dir.exists() {
        tracing::debug!("{missing_message}");
        return Ok(None);
    }
    Ok(Some(dir))
}

fn scan_qwen_sessions(runtime_dir: &Path) -> anyhow::Result<Vec<RawSession>> {
    let mut sessions = Vec::new();
    for entry in collect_session_entries(runtime_dir) {
        let Some(mtime_ms) = file_scan::stat_mtime_ms(&entry.stat_target) else {
            continue;
        };
        if let Some(raw) = parse_qwen_session_file(entry, mtime_ms)? {
            sessions.push(raw);
        }
    }
    Ok(sessions)
}

fn collect_session_entries(runtime_dir: &Path) -> Vec<FileScanEntry> {
    let mut entries = Vec::new();
    let Ok(projects) = fs::read_dir(runtime_dir.join("projects")) else {
        return entries;
    };

    for project in projects.flatten() {
        let chats_dir = project.path().join("chats");
        let Ok(files) = fs::read_dir(chats_dir) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !is_qwen_session_file(name) {
                continue;
            }
            let Some(session_id) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            entries.push(FileScanEntry {
                session_id: session_id.to_string(),
                stat_target: path,
                directory: None,
            });
        }
    }

    entries
}

fn is_qwen_session_file(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".jsonl") else {
        return false;
    };
    (32..=36).contains(&stem.len())
        && stem.bytes().all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
}

fn parse_qwen_session_file(
    entry: FileScanEntry,
    mtime_ms: i64,
) -> anyhow::Result<Option<RawSession>> {
    match parse_qwen_session_file_impl(&entry, mtime_ms) {
        Ok(raw) => Ok(raw),
        Err(error) => {
            warn!("failed to parse {}: {error}", entry.stat_target.display());
            Ok(None)
        }
    }
}

fn parse_qwen_session_file_impl(
    entry: &FileScanEntry,
    mtime_ms: i64,
) -> anyhow::Result<Option<RawSession>> {
    let file = fs::File::open(&entry.stat_target)?;
    let reader = BufReader::new(file);
    let source_path = entry.stat_target.to_str().map(str::to_string);
    parse_qwen_jsonl(reader.lines(), entry.session_id.clone(), mtime_ms, source_path)
}

fn parse_qwen_jsonl(
    lines: impl Iterator<Item = std::io::Result<String>>,
    session_id: String,
    mtime_ms: i64,
    source_path: Option<String>,
) -> anyhow::Result<Option<RawSession>> {
    let mut messages = Vec::new();
    let mut usage_events = Vec::new();
    let mut directory = None;
    let mut custom_title = None;
    let mut summary = None;

    for item in jsonl_indexed(lines) {
        let (line_index, record) = item?;
        if directory.is_none() {
            directory = record
                .get("cwd")
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string);
        }

        if let Some((title, source)) = custom_title_from(&record) {
            if source == Some("auto") {
                summary = Some(title);
                custom_title = None;
            } else {
                custom_title = Some(title);
                summary = None;
            }
            continue;
        }

        let record_type = record.get("type").and_then(|value| value.as_str()).unwrap_or("");
        let role = match record_type {
            "user" if record.get("subtype").is_none() => Role::User,
            "assistant" => Role::Assistant,
            _ => continue,
        };

        let content = extract_text_parts(record.pointer("/message/parts"));
        let timestamp = rfc3339_ms(record.get("timestamp"));
        let message_seq = if content.is_empty() {
            None
        } else {
            let message_seq = messages.len() as u32;
            messages.push(RawMessage { role, content, timestamp });
            Some(message_seq)
        };

        if record_type == "assistant"
            && let Some(event) = extract_usage_event(
                &record,
                line_index,
                message_seq,
                timestamp.unwrap_or(mtime_ms),
                source_path.as_deref(),
            )
        {
            usage_events.push(event);
        }
    }

    if messages.is_empty() && usage_events.is_empty() {
        return Ok(None);
    }

    let started_at = first_timestamp(None, &messages, &usage_events, &[]).unwrap_or(mtime_ms);
    let mut session =
        RawSession::search_only(session_id, directory, started_at, Some(mtime_ms), None, messages);
    session.source_file_path = source_path;
    session.custom_title = custom_title;
    session.summary = summary;
    session = session.with_usage(usage_events, USAGE_PARSER_VERSION);
    Ok(Some(session))
}

fn custom_title_from(record: &Value) -> Option<(String, Option<&str>)> {
    if record.get("type").and_then(|value| value.as_str()) != Some("system") {
        return None;
    }
    if record.get("subtype").and_then(|value| value.as_str()) != Some("custom_title") {
        return None;
    }
    let title = record
        .pointer("/systemPayload/customTitle")
        .or_else(|| record.get("customTitle"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())?
        .to_string();
    let source = record
        .pointer("/systemPayload/titleSource")
        .or_else(|| record.get("titleSource"))
        .and_then(|value| value.as_str());
    Some((title, source))
}

fn extract_text_parts(parts: Option<&Value>) -> String {
    let Some(parts) = parts.and_then(|value| value.as_array()) else {
        return String::new();
    };
    let text = parts
        .iter()
        .filter(|part| part.get("thought").and_then(|value| value.as_bool()) != Some(true))
        .filter(|part| part.get("functionCall").is_none() && part.get("functionResponse").is_none())
        .filter_map(|part| part.get("text").and_then(|value| value.as_str()))
        .collect::<Vec<_>>()
        .join("");
    text.trim().to_string()
}

fn extract_usage_event(
    record: &Value,
    event_seq: usize,
    message_seq: Option<u32>,
    timestamp: i64,
    source_path: Option<&str>,
) -> Option<RawUsageEvent> {
    let usage = record.get("usageMetadata").or_else(|| record.get("tokens"))?;
    let input_tokens = usage_count(usage, &["promptTokenCount", "input"]);
    let output_tokens = usage_count(usage, &["candidatesTokenCount", "output"]);
    let cache_read_tokens = usage_count(usage, &["cachedContentTokenCount", "cached"]);
    let reasoning_tokens = usage_count(usage, &["thoughtsTokenCount", "thoughts"]);
    if input_tokens == 0 && output_tokens == 0 && cache_read_tokens == 0 && reasoning_tokens == 0 {
        return None;
    }

    let model = record
        .get("model")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("unknown")
        .to_string();
    let event_key = record
        .get("uuid")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(|uuid| format!("message:{uuid}"))
        .unwrap_or_else(|| format!("line:{event_seq}"));

    Some(RawUsageEvent {
        message_seq,
        model,
        provider: "qwen".to_string(),
        input_tokens,
        output_tokens,
        cache_read_tokens,
        reasoning_tokens,
        source_path: source_path.map(str::to_string),
        raw_usage_json: Some(usage.to_string()),
        ..RawUsageEvent::observed(event_key, event_seq as u32, timestamp, USAGE_PARSER_VERSION)
    })
}

#[cfg(test)]
fn parse_qwen_session(jsonl: &str, session_id: &str) -> Option<RawSession> {
    let lines = jsonl.lines().map(|line| Ok(line.to_string()));
    parse_qwen_jsonl(lines, session_id.to_string(), 1_700_000_000_000, None).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION_ID: &str = "550e8400-e29b-41d4-a716-446655440000";

    fn fixture_jsonl() -> String {
        format!(
            "{}\n{}\n{}\n{}\n{}",
            r#"{"uuid":"a1","parentUuid":null,"sessionId":"550e8400-e29b-41d4-a716-446655440000","timestamp":"2024-01-01T00:00:00.000Z","type":"user","message":{"role":"user","parts":[{"text":"hello session a"}]},"cwd":"/test/project/root","version":"1.0.0"}"#,
            r#"{"uuid":"a2","parentUuid":"a1","sessionId":"550e8400-e29b-41d4-a716-446655440000","timestamp":"2024-01-01T00:00:05.000Z","type":"assistant","message":{"role":"model","parts":[{"thought":true,"text":"hidden"},{"text":"hey back"}]},"cwd":"/test/project/root","version":"1.0.0","model":"qwen3-coder-plus","usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":20,"cachedContentTokenCount":30,"thoughtsTokenCount":5}}"#,
            r#"{"uuid":"a3","parentUuid":"a2","sessionId":"550e8400-e29b-41d4-a716-446655440000","timestamp":"2024-01-01T00:00:06.000Z","type":"tool_result","message":{"role":"user","parts":[{"functionResponse":{"name":"read"}}]},"cwd":"/test/project/root","version":"1.0.0"}"#,
            r#"{"uuid":"a4","parentUuid":"a3","sessionId":"550e8400-e29b-41d4-a716-446655440000","timestamp":"2024-01-01T00:00:07.000Z","type":"user","subtype":"notification","message":{"role":"user","parts":[{"text":"cron noise"}]},"cwd":"/test/project/root","version":"1.0.0"}"#,
            r#"{"uuid":"a5","parentUuid":"a4","sessionId":"550e8400-e29b-41d4-a716-446655440000","timestamp":"2024-01-01T00:00:08.000Z","type":"system","subtype":"custom_title","systemPayload":{"customTitle":"hello session a","titleSource":"manual"},"cwd":"/test/project/root","version":"1.0.0"}"#,
        )
    }

    #[test]
    fn parse_qwen_session_extracts_messages_usage_and_title() {
        let session = parse_qwen_session(&fixture_jsonl(), SESSION_ID).unwrap();

        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, Role::User);
        assert_eq!(session.messages[0].content, "hello session a");
        assert_eq!(session.messages[1].role, Role::Assistant);
        assert_eq!(session.messages[1].content, "hey back");
        assert_eq!(session.directory.as_deref(), Some("/test/project/root"));
        assert_eq!(session.started_at, 1_704_067_200_000);
        assert_eq!(session.custom_title.as_deref(), Some("hello session a"));
        assert_eq!(session.summary, None);

        assert_eq!(session.usage_events.len(), 1);
        let event = &session.usage_events[0];
        assert_eq!(event.model, "qwen3-coder-plus");
        assert_eq!(event.provider, "qwen");
        assert_eq!(event.input_tokens, 100);
        assert_eq!(event.output_tokens, 20);
        assert_eq!(event.cache_read_tokens, 30);
        assert_eq!(event.reasoning_tokens, 5);
        assert_eq!(event.token_source, crate::types::TokenSource::Observed);
        assert_eq!(session.usage_parser_version, Some(USAGE_PARSER_VERSION));
    }

    #[test]
    fn parse_qwen_session_auto_title_goes_to_summary() {
        let jsonl = format!(
            "{}\n{}",
            r#"{"uuid":"u1","sessionId":"550e8400-e29b-41d4-a716-446655440000","timestamp":"2024-01-01T00:00:00.000Z","type":"user","message":{"role":"user","parts":[{"text":"hello"}]},"cwd":"/repo","version":"1.0.0"}"#,
            r#"{"uuid":"t1","sessionId":"550e8400-e29b-41d4-a716-446655440000","timestamp":"2024-01-01T00:00:01.000Z","type":"system","subtype":"custom_title","systemPayload":{"customTitle":"auto name","titleSource":"auto"},"cwd":"/repo","version":"1.0.0"}"#,
        );
        let session = parse_qwen_session(&jsonl, SESSION_ID).unwrap();
        assert_eq!(session.summary.as_deref(), Some("auto name"));
        assert_eq!(session.custom_title, None);
    }

    #[test]
    fn parse_qwen_session_keeps_usage_without_visible_text() {
        let jsonl = r#"{"uuid":"a2","sessionId":"550e8400-e29b-41d4-a716-446655440000","timestamp":"2024-01-01T00:00:05.000Z","type":"assistant","message":{"role":"model","parts":[{"thought":true,"text":"hidden"}]},"cwd":"/repo","version":"1.0.0","model":"qwen3-coder-plus","usageMetadata":{"promptTokenCount":12,"candidatesTokenCount":3}}"#;
        let session = parse_qwen_session(jsonl, SESSION_ID).unwrap();
        assert!(session.messages.is_empty());
        assert_eq!(session.usage_events.len(), 1);
        assert_eq!(session.usage_events[0].input_tokens, 12);
        assert_eq!(session.usage_events[0].message_seq, None);
    }

    #[test]
    fn parse_qwen_session_empty_or_non_chat_returns_none() {
        assert!(parse_qwen_session("", SESSION_ID).is_none());
        assert!(
            parse_qwen_session(
                r#"{"uuid":"a1","type":"system","subtype":"ui_telemetry","cwd":"/repo","timestamp":"2024-01-01T00:00:00.000Z"}"#,
                SESSION_ID
            )
            .is_none()
        );
    }

    #[test]
    fn is_qwen_session_file_matches_official_pattern() {
        assert!(is_qwen_session_file("550e8400-e29b-41d4-a716-446655440000.jsonl"));
        assert!(is_qwen_session_file("550e8400e29b41d4a716446655440000.jsonl"));
        assert!(!is_qwen_session_file("550e8400-e29b-41d4-a716-446655440000.runtime.json"));
        assert!(!is_qwen_session_file("session-abc.json"));
        assert!(!is_qwen_session_file("notes.jsonl"));
    }

    #[test]
    fn scan_discovers_project_chat_jsonl() {
        let root = tempfile::tempdir().unwrap();
        let chats = root.path().join("projects").join("Users-x-repo").join("chats");
        fs::create_dir_all(&chats).unwrap();
        fs::write(chats.join(format!("{SESSION_ID}.jsonl")), fixture_jsonl()).unwrap();
        fs::write(chats.join(format!("{SESSION_ID}.runtime.json")), "{}").unwrap();
        fs::write(chats.join("notes.jsonl"), r#"{"type":"user"}"#).unwrap();

        let sessions = scan_qwen_sessions(root.path()).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].source_id, SESSION_ID);
        assert_eq!(
            sessions[0].source_file_path.as_deref(),
            chats.join(format!("{SESSION_ID}.jsonl")).to_str()
        );
    }

    #[test]
    fn missing_runtime_dir_yields_no_sessions() {
        let root = tempfile::tempdir().unwrap();
        assert!(scan_qwen_sessions(root.path()).unwrap().is_empty());
    }

    #[test]
    fn incremental_scan_skips_unchanged_mtime() {
        let root = tempfile::tempdir().unwrap();
        let chats = root.path().join("projects").join("repo").join("chats");
        fs::create_dir_all(&chats).unwrap();
        let path = chats.join(format!("{SESSION_ID}.jsonl"));
        fs::write(&path, fixture_jsonl()).unwrap();

        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let first = file_scan::run_file_scan_with_options(
            &store,
            "qwen-code",
            None,
            FileScanOptions {
                usage_parser_version: Some(USAGE_PARSER_VERSION),
                event_parser_version: None,
                metadata_parser_version: None,
            },
            collect_session_entries(root.path()),
            parse_qwen_session_file,
        )
        .unwrap();
        assert_eq!(first.stats.parsed, 1);
        assert_eq!(first.sessions.len(), 1);

        store
            .conn
            .execute(
                "INSERT INTO sessions (id, source, source_id, title, started_at, updated_at, message_count) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    "stored-id",
                    "qwen-code",
                    SESSION_ID,
                    "hello session a",
                    1_704_067_200_000_i64,
                    first.sessions[0].updated_at.unwrap(),
                    2,
                ],
            )
            .unwrap();
        store
            .persist_usage_events_for_existing_session(
                "qwen-code",
                SESSION_ID,
                &[],
                USAGE_PARSER_VERSION,
                first.sessions[0].updated_at,
            )
            .unwrap();

        let second = file_scan::run_file_scan_with_options(
            &store,
            "qwen-code",
            None,
            FileScanOptions {
                usage_parser_version: Some(USAGE_PARSER_VERSION),
                event_parser_version: None,
                metadata_parser_version: None,
            },
            collect_session_entries(root.path()),
            parse_qwen_session_file,
        )
        .unwrap();
        assert_eq!(second.stats.parsed, 0);
        assert!(second.sessions.is_empty());
    }

    #[test]
    fn expand_user_path_handles_home_and_absolute() {
        let expanded = expand_user_path("~/.qwen").unwrap();
        assert!(expanded.ends_with(".qwen"));
        assert_eq!(expand_user_path("/tmp/qwen").unwrap(), PathBuf::from("/tmp/qwen"));
    }

    #[test]
    fn resume_uses_official_flag() {
        let command = QwenAdapter.resume_command(SESSION_ID).unwrap();
        assert_eq!(command.program, "qwen");
        assert_eq!(command.args, vec!["--resume", SESSION_ID]);
    }
}
