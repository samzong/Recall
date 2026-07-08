use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;
use tracing::debug;
use walkdir::WalkDir;

use crate::adapters::file_scan::{self, FileScanEntry};
use crate::adapters::json_util::{json_i64, jsonl_indexed, rfc3339_ms};
use crate::adapters::{
    RawMessage, RawSession, ResumeCommand, SourceAdapter, SyncScanResult, SyncScanStats,
    first_timestamp,
};
use crate::db::store::Store;
use crate::types::{RawUsageEvent, Role, TokenSource};

pub(crate) struct PiAdapter;

const USAGE_PARSER_VERSION: u32 = 1;

impl SourceAdapter for PiAdapter {
    fn id(&self) -> &str {
        "pi"
    }

    fn label(&self) -> &str {
        "PI"
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        Some(ResumeCommand {
            program: "pi".to_string(),
            args: vec!["--session".to_string(), source_id.to_string()],
        })
    }

    fn usage_parser_version(&self) -> Option<u32> {
        Some(USAGE_PARSER_VERSION)
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let session_dirs = resolve_pi_session_dirs()?;
        if session_dirs.is_empty() {
            return Ok(vec![]);
        }

        let mut sessions = Vec::new();
        for entry in collect_pi_entries(&session_dirs) {
            let Some(mtime_ms) = file_scan::stat_mtime_ms(&entry.stat_target) else {
                continue;
            };
            if let Some(raw) = parse_pi_session_file(entry, mtime_ms)? {
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
        let session_dirs = resolve_pi_session_dirs()?;
        if session_dirs.is_empty() {
            return Ok(Some(SyncScanResult { sessions: vec![], stats: SyncScanStats::default() }));
        }

        Ok(Some(scan_for_sync_impl(&session_dirs, store, since_ts)?))
    }
}

struct ParsedPiSession {
    session_id: Option<String>,
    cwd: Option<String>,
    started_at: Option<i64>,
    messages: Vec<RawMessage>,
    usage_events: Vec<RawUsageEvent>,
}

fn resolve_pi_session_dirs() -> anyhow::Result<Vec<PathBuf>> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home dir"))?;
    let mut session_dirs = Vec::new();
    let mut seen = HashSet::new();

    let env_session_dir =
        std::env::var("PI_CODING_AGENT_SESSION_DIR").ok().filter(|path| !path.trim().is_empty());
    let agent_dir = std::env::var("PI_CODING_AGENT_DIR")
        .ok()
        .filter(|path| !path.trim().is_empty())
        .map(|path| expand_home_path(path.trim(), &home))
        .unwrap_or_else(|| home.join(".pi").join("agent"));

    if let Some(session_dir) = env_session_dir.as_deref() {
        push_existing_unique_dir(
            &mut session_dirs,
            &mut seen,
            expand_home_path(session_dir.trim(), &home),
        );
        if session_dirs.is_empty() {
            debug!("Pi session directory from PI_CODING_AGENT_SESSION_DIR not found, skipping Pi");
            return Ok(session_dirs);
        }
    } else if let Some(session_dir) = settings_session_dir(&agent_dir, &home) {
        push_existing_unique_dir(&mut session_dirs, &mut seen, session_dir);
    }
    if env_session_dir.is_none() {
        push_existing_unique_dir(&mut session_dirs, &mut seen, agent_dir.join("sessions"));
    }

    if session_dirs.is_empty() {
        debug!("Pi session directory not found, skipping Pi");
    }

    Ok(session_dirs)
}

fn settings_session_dir(agent_dir: &Path, home: &Path) -> Option<PathBuf> {
    settings_session_dir_with_cwd(agent_dir, home, std::env::current_dir().ok().as_deref())
}

fn settings_session_dir_with_cwd(
    agent_dir: &Path,
    home: &Path,
    current_dir: Option<&Path>,
) -> Option<PathBuf> {
    let global = session_dir_from_settings(&agent_dir.join("settings.json"), home);
    let Some(current_dir) = current_dir else {
        return global;
    };
    let project_settings_dir = current_dir.join(".pi");
    session_dir_from_settings(&project_settings_dir.join("settings.json"), home).or(global)
}

fn session_dir_from_settings(settings_path: &Path, home: &Path) -> Option<PathBuf> {
    let content = fs::read_to_string(settings_path).ok()?;
    let settings: Value = serde_json::from_str(&content).ok()?;
    let session_dir = settings.get("sessionDir")?.as_str()?.trim();
    if session_dir.is_empty() {
        return None;
    }
    let session_dir = expand_home_path(session_dir, home);
    if session_dir.is_relative() {
        return Some(settings_path.parent()?.join(session_dir));
    }
    Some(session_dir)
}

fn expand_home_path(path: &str, home: &Path) -> PathBuf {
    if path == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return home.join(rest);
    }
    PathBuf::from(path)
}

fn push_existing_unique_dir(dirs: &mut Vec<PathBuf>, seen: &mut HashSet<String>, dir: PathBuf) {
    if !dir.exists() {
        return;
    }

    let key = fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone()).to_string_lossy().to_string();
    if seen.insert(key) {
        dirs.push(dir);
    }
}

fn scan_for_sync_impl(
    session_dirs: &[PathBuf],
    store: &Store,
    since_ts: Option<i64>,
) -> anyhow::Result<SyncScanResult> {
    let entries = collect_pi_entries(session_dirs);
    file_scan::run_file_scan_with_options(
        store,
        "pi",
        since_ts,
        file_scan::FileScanOptions {
            usage_parser_version: Some(USAGE_PARSER_VERSION),
            event_parser_version: None,
        },
        entries,
        parse_pi_session_file,
    )
}

fn collect_pi_entries(session_dirs: &[PathBuf]) -> Vec<FileScanEntry> {
    let mut entries = Vec::new();
    let mut seen_files = HashSet::new();

    for session_dir in session_dirs {
        if !session_dir.exists() {
            continue;
        }

        for entry in WalkDir::new(session_dir).into_iter().filter_map(|entry| entry.ok()) {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            if !path.is_file() {
                continue;
            }

            let key = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            if !seen_files.insert(key.to_string_lossy().to_string()) {
                continue;
            }

            let stem = match path.file_stem().and_then(|stem| stem.to_str()) {
                Some(stem) if !stem.is_empty() => stem,
                _ => continue,
            };
            let session_id =
                extract_session_id_from_filename(stem).unwrap_or_else(|| stem.to_string());
            let directory = path
                .parent()
                .and_then(|parent| parent.file_name())
                .and_then(|name| name.to_str())
                .and_then(decode_session_dir_name);

            entries.push(FileScanEntry { session_id, stat_target: path.to_path_buf(), directory });
        }
    }

    entries
}

fn extract_session_id_from_filename(stem: &str) -> Option<String> {
    let candidate = stem.rsplit_once('_').map(|(_, tail)| tail).unwrap_or(stem);
    uuid::Uuid::try_parse(candidate).ok().map(|_| candidate.to_string())
}

fn decode_session_dir_name(name: &str) -> Option<String> {
    let inner = name.strip_prefix("--")?.strip_suffix("--")?;
    if inner.is_empty() {
        return None;
    }
    Some(format!("/{}", inner.replace('-', "/")))
}

fn parse_pi_session_file(
    entry: FileScanEntry,
    mtime_ms: i64,
) -> anyhow::Result<Option<RawSession>> {
    let parsed = match parse_pi_session(&entry.stat_target, mtime_ms) {
        Ok(parsed) => parsed,
        Err(err) => {
            debug!("failed to parse Pi session {}: {err}", entry.stat_target.display());
            return Ok(None);
        }
    };

    if parsed.messages.is_empty() && parsed.usage_events.is_empty() {
        return Ok(None);
    }

    let started_at =
        first_timestamp(parsed.started_at, &parsed.messages, &parsed.usage_events, &[])
            .unwrap_or(0);

    Ok(Some(RawSession {
        source_id: parsed.session_id.unwrap_or(entry.session_id),
        directory: parsed.cwd.or(entry.directory),
        started_at,
        updated_at: Some(mtime_ms),
        entrypoint: None,
        messages: parsed.messages,
        usage_events: parsed.usage_events,
        usage_parser_version: Some(USAGE_PARSER_VERSION),
        events: Vec::new(),
        event_parser_version: None,
        source_file_path: None,
        custom_title: None,
        summary: None,
        duration_minutes: None,
    }))
}

fn parse_pi_session(path: &Path, fallback_timestamp: i64) -> anyhow::Result<ParsedPiSession> {
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let source_path = path.to_string_lossy().to_string();

    let mut session_id = None;
    let mut cwd = None;
    let mut started_at = None;
    let mut current_provider: Option<String> = None;
    let mut current_model: Option<String> = None;
    let mut inherited_usage_cutoff = None;
    let mut messages = Vec::new();
    let mut usage_events = Vec::new();

    for item in jsonl_indexed(reader.lines()) {
        let (line_index, entry) = item?;

        match entry.get("type").and_then(|value| value.as_str()).unwrap_or("") {
            "session" => {
                let header_timestamp = parse_entry_timestamp(&entry);
                session_id = entry
                    .get("id")
                    .and_then(|value| value.as_str())
                    .filter(|value| !value.trim().is_empty())
                    .map(str::to_string)
                    .or(session_id);
                cwd = entry
                    .get("cwd")
                    .and_then(|value| value.as_str())
                    .filter(|value| !value.trim().is_empty())
                    .map(str::to_string)
                    .or(cwd);
                started_at = header_timestamp.or(started_at);
                if entry
                    .get("parentSession")
                    .and_then(|value| value.as_str())
                    .is_some_and(|value| !value.trim().is_empty())
                {
                    inherited_usage_cutoff = header_timestamp;
                }
            }
            "model_change" => {
                current_provider = entry
                    .get("provider")
                    .and_then(|value| value.as_str())
                    .filter(|value| !value.trim().is_empty())
                    .map(str::to_string)
                    .or(current_provider);
                current_model = entry
                    .get("modelId")
                    .or_else(|| entry.get("model"))
                    .and_then(|value| value.as_str())
                    .filter(|value| !value.trim().is_empty())
                    .map(str::to_string)
                    .or(current_model);
            }
            "message" => {
                if let Some(message) = entry.get("message") {
                    parse_pi_message(
                        &entry,
                        message,
                        line_index as u32,
                        fallback_timestamp,
                        current_provider.as_deref(),
                        current_model.as_deref(),
                        &source_path,
                        inherited_usage_cutoff,
                        &mut messages,
                        &mut usage_events,
                    );
                }
            }
            "custom_message" => {
                let timestamp = parse_entry_timestamp(&entry).unwrap_or(fallback_timestamp);
                let content = extract_content(entry.get("content"));
                if !content.trim().is_empty() {
                    messages.push(RawMessage {
                        role: Role::User,
                        content,
                        timestamp: Some(timestamp),
                    });
                }
            }
            "compaction" | "branch_summary" => {
                if let Some(summary) = entry.get("summary").and_then(|value| value.as_str())
                    && !summary.trim().is_empty()
                {
                    let timestamp = parse_entry_timestamp(&entry).unwrap_or(fallback_timestamp);
                    messages.push(RawMessage {
                        role: Role::Assistant,
                        content: summary.to_string(),
                        timestamp: Some(timestamp),
                    });
                }
            }
            _ => {}
        }
    }

    Ok(ParsedPiSession { session_id, cwd, started_at, messages, usage_events })
}

#[allow(clippy::too_many_arguments)]
fn parse_pi_message(
    entry: &Value,
    message: &Value,
    line_index: u32,
    fallback_timestamp: i64,
    current_provider: Option<&str>,
    current_model: Option<&str>,
    source_path: &str,
    inherited_usage_cutoff: Option<i64>,
    messages: &mut Vec<RawMessage>,
    usage_events: &mut Vec<RawUsageEvent>,
) {
    let timestamp = json_i64(message.get("timestamp"))
        .or_else(|| parse_entry_timestamp(entry))
        .unwrap_or(fallback_timestamp);

    match message.get("role").and_then(|value| value.as_str()).unwrap_or("") {
        "user" => {
            let content = extract_content(message.get("content"));
            if !content.trim().is_empty() {
                messages.push(RawMessage { role: Role::User, content, timestamp: Some(timestamp) });
            }
        }
        "assistant" => {
            let content = extract_content(message.get("content"));
            let message_seq =
                if content.trim().is_empty() { None } else { Some(messages.len() as u32) };

            if inherited_usage_cutoff.is_none_or(|cutoff| timestamp > cutoff)
                && let Some(event) = extract_pi_usage_event(
                    entry,
                    message,
                    line_index,
                    timestamp,
                    message_seq,
                    (current_provider, current_model),
                    source_path,
                )
            {
                usage_events.push(event);
            }

            if !content.trim().is_empty() {
                messages.push(RawMessage {
                    role: Role::Assistant,
                    content,
                    timestamp: Some(timestamp),
                });
            }
        }
        "toolResult" => {
            let content = extract_tool_result_content(message);
            if !content.trim().is_empty() {
                messages.push(RawMessage {
                    role: Role::Assistant,
                    content,
                    timestamp: Some(timestamp),
                });
            }
        }
        "bashExecution" => {
            if message.get("excludeFromContext").and_then(Value::as_bool) == Some(true) {
                return;
            }
            let content = extract_bash_execution_content(message);
            if !content.trim().is_empty() {
                messages.push(RawMessage {
                    role: Role::Assistant,
                    content,
                    timestamp: Some(timestamp),
                });
            }
        }
        "custom" => {
            let content = extract_content(message.get("content"));
            if !content.trim().is_empty() {
                messages.push(RawMessage { role: Role::User, content, timestamp: Some(timestamp) });
            }
        }
        _ => {}
    }
}

fn extract_pi_usage_event(
    entry: &Value,
    message: &Value,
    event_seq: u32,
    timestamp: i64,
    message_seq: Option<u32>,
    current_provider_model: (Option<&str>, Option<&str>),
    source_path: &str,
) -> Option<RawUsageEvent> {
    let (current_provider, current_model) = current_provider_model;
    let usage = message.get("usage")?;
    let provider = non_empty_str(message.get("provider"))
        .or(current_provider)
        .unwrap_or("unknown")
        .to_string();
    let model =
        non_empty_str(message.get("model")).or(current_model).unwrap_or("unknown").to_string();

    let event_key = entry
        .get("id")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(|id| format!("message:{id}"))
        .unwrap_or_else(|| format!("line:{event_seq}"));

    Some(RawUsageEvent {
        event_key,
        event_seq,
        message_seq,
        timestamp,
        model,
        provider,
        input_tokens: usage_count(usage, &["input", "inputTokens", "input_tokens"]),
        output_tokens: usage_count(usage, &["output", "outputTokens", "output_tokens"]),
        cache_read_tokens: usage_count(
            usage,
            &[
                "cacheRead",
                "cache_read",
                "cacheReadTokens",
                "cache_read_tokens",
                "cachedInputTokens",
                "cached_input_tokens",
            ],
        ),
        cache_write_tokens: usage_count(
            usage,
            &["cacheWrite", "cache_write", "cacheWriteTokens", "cache_write_tokens"],
        ),
        reasoning_tokens: usage_count(
            usage,
            &[
                "reasoning",
                "reasoningTokens",
                "reasoning_tokens",
                "reasoningOutputTokens",
                "reasoning_output_tokens",
            ],
        ),
        token_source: TokenSource::Observed,
        parser_version: USAGE_PARSER_VERSION,
        source_path: Some(source_path.to_string()),
        raw_usage_json: Some(usage.to_string()),
    })
}

fn non_empty_str(value: Option<&Value>) -> Option<&str> {
    value.and_then(|value| value.as_str()).filter(|value| !value.trim().is_empty())
}

fn usage_count(usage: &Value, keys: &[&str]) -> i64 {
    keys.iter().find_map(|key| json_i64(usage.get(*key))).unwrap_or(0).max(0)
}

fn extract_content(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.to_string(),
        Some(Value::Array(items)) => {
            let mut parts = Vec::new();
            for item in items {
                match item.get("type").and_then(|value| value.as_str()).unwrap_or("") {
                    "text" | "output_text" => {
                        if let Some(text) = item.get("text").and_then(|value| value.as_str())
                            && !text.trim().is_empty()
                        {
                            parts.push(text.to_string());
                        }
                    }
                    "toolCall" | "tool_call" | "function_call" => {
                        let name = item
                            .get("name")
                            .and_then(|value| value.as_str())
                            .filter(|value| !value.trim().is_empty())
                            .unwrap_or("tool");
                        let arguments = item
                            .get("arguments")
                            .or_else(|| item.get("input"))
                            .map(|value| match value {
                                Value::String(text) => text.to_string(),
                                other => other.to_string(),
                            })
                            .unwrap_or_default();
                        if arguments.trim().is_empty() {
                            parts.push(format!("[{name}]"));
                        } else {
                            parts.push(format!("[{name}] {arguments}"));
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

fn extract_tool_result_content(message: &Value) -> String {
    let content = extract_content(message.get("content"));
    if content.trim().is_empty() {
        return String::new();
    }

    let tool_name = message
        .get("toolName")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("tool");
    format!("[{tool_name} result]\n{content}")
}

fn extract_bash_execution_content(message: &Value) -> String {
    let command = message
        .get("command")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty());
    let output = message
        .get("output")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty());

    match (command, output) {
        (Some(command), Some(output)) => format!("[bash] {command}\n{output}"),
        (Some(command), None) => format!("[bash] {command}"),
        (None, Some(output)) => output.to_string(),
        (None, None) => String::new(),
    }
}

fn parse_entry_timestamp(entry: &Value) -> Option<i64> {
    rfc3339_ms(entry.get("timestamp"))
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

    fn temp_pi_root(label: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("recall-pi-test-{}-{}", label, uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_pi_session(dir: &Path, session_id: &str, lines: &[Value]) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(format!("2026-05-24T17-04-51-496Z_{session_id}.jsonl"));
        let mut file = fs::File::create(&path).unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
        path
    }

    fn make_existing_session(source_id: &str, updated_at: i64, message_count: u32) -> Session {
        Session {
            id: format!("internal-{source_id}"),
            source: "pi".to_string(),
            source_id: source_id.to_string(),
            title: "existing".to_string(),
            directory: Some("/tmp/pi-project".to_string()),
            repo_remote: None,
            repo_slug: None,
            repo_name: None,
            started_at: 1_000,
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
    fn extract_session_id_from_filename_reads_pi_uuid_tail() {
        assert_eq!(
            extract_session_id_from_filename(
                "2026-05-24T17-04-51-496Z_019e5af2-5528-7d10-888a-b299c21d0e2e"
            ),
            Some("019e5af2-5528-7d10-888a-b299c21d0e2e".to_string())
        );
        assert_eq!(extract_session_id_from_filename("not-a-session"), None);
    }

    #[test]
    fn session_dir_from_settings_resolves_relative_paths_from_settings_scope() {
        let root = temp_pi_root("settings-dir");
        let home = root.join("home");
        let agent_dir = home.join(".pi").join("agent");
        fs::create_dir_all(&agent_dir).unwrap();
        let settings_path = agent_dir.join("settings.json");
        fs::write(&settings_path, r#"{"sessionDir":"custom-sessions"}"#).unwrap();

        assert_eq!(
            session_dir_from_settings(&settings_path, &home).as_deref(),
            Some(agent_dir.join("custom-sessions").as_path())
        );

        fs::write(&settings_path, r#"{"sessionDir":"~/pi-sessions"}"#).unwrap();
        assert_eq!(
            session_dir_from_settings(&settings_path, &home).as_deref(),
            Some(home.join("pi-sessions").as_path())
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn settings_session_dir_keeps_global_when_current_dir_is_unavailable() {
        let root = temp_pi_root("settings-no-cwd");
        let home = root.join("home");
        let agent_dir = home.join(".pi").join("agent");
        let global_session_dir = root.join("global-sessions");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::create_dir_all(&global_session_dir).unwrap();
        fs::write(
            agent_dir.join("settings.json"),
            format!(r#"{{"sessionDir":"{}"}}"#, global_session_dir.display()),
        )
        .unwrap();

        assert_eq!(
            settings_session_dir_with_cwd(&agent_dir, &home, None).as_deref(),
            Some(global_session_dir.as_path())
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_pi_session_file_extracts_messages_and_usage() {
        let root = temp_pi_root("parse");
        let session_dir = root.join("--tmp-pi-project--");
        let session_id = "019e5af2-5528-7d10-888a-b299c21d0e2e";
        let path = write_pi_session(
            &session_dir,
            session_id,
            &[
                serde_json::json!({
                    "type": "session",
                    "version": 3,
                    "id": session_id,
                    "timestamp": "1970-01-01T00:00:01.000Z",
                    "cwd": "/tmp/pi-project"
                }),
                serde_json::json!({
                    "type": "message",
                    "id": "user1",
                    "parentId": null,
                    "timestamp": "1970-01-01T00:00:02.000Z",
                    "message": {
                        "role": "user",
                        "content": [{"type": "text", "text": "hello pi"}],
                        "timestamp": 2000
                    }
                }),
                serde_json::json!({
                    "type": "message",
                    "id": "assistant1",
                    "parentId": "user1",
                    "timestamp": "1970-01-01T00:00:03.000Z",
                    "message": {
                        "role": "assistant",
                        "content": [
                            {"type": "thinking", "thinking": "hidden chain of thought"},
                            {"type": "toolCall", "name": "read", "arguments": {"path": "README.md"}},
                            {"type": "image", "mimeType": "image/png"}
                        ],
                        "provider": "openai-codex",
                        "model": "gpt-5.5",
                        "usage": {
                            "input": 10,
                            "output": 3,
                            "cacheRead": 2,
                            "cacheWrite": 1,
                            "totalTokens": 16,
                            "cost": {"total": 0.1}
                        },
                        "timestamp": 3000
                    }
                }),
                serde_json::json!({
                    "type": "message",
                    "id": "tool1",
                    "parentId": "assistant1",
                    "timestamp": "1970-01-01T00:00:04.000Z",
                    "message": {
                        "role": "toolResult",
                        "toolName": "read",
                        "content": [{"type": "text", "text": "file content"}],
                        "timestamp": 4000
                    }
                }),
            ],
        );
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();
        let raw = parse_pi_session_file(
            FileScanEntry {
                session_id: session_id.to_string(),
                stat_target: path.clone(),
                directory: Some("/wrong".to_string()),
            },
            mtime,
        )
        .unwrap()
        .unwrap();

        assert_eq!(raw.source_id, session_id);
        assert_eq!(raw.directory.as_deref(), Some("/tmp/pi-project"));
        assert_eq!(raw.started_at, 1_000);
        assert_eq!(raw.updated_at, Some(mtime));
        assert_eq!(raw.messages.len(), 3);
        assert_eq!(raw.messages[0].role, Role::User);
        assert_eq!(raw.messages[0].content, "hello pi");
        assert!(raw.messages[1].content.contains("[read]"));
        assert!(!raw.messages[1].content.contains("hidden chain of thought"));
        assert!(!raw.messages[1].content.contains("image/png"));
        assert!(raw.messages[2].content.contains("[read result]"));

        assert_eq!(raw.usage_events.len(), 1);
        let event = &raw.usage_events[0];
        assert_eq!(event.event_key, "message:assistant1");
        assert_eq!(event.message_seq, Some(1));
        assert_eq!(event.timestamp, 3_000);
        assert_eq!(event.provider, "openai-codex");
        assert_eq!(event.model, "gpt-5.5");
        assert_eq!(event.input_tokens, 10);
        assert_eq!(event.output_tokens, 3);
        assert_eq!(event.cache_read_tokens, 2);
        assert_eq!(event.cache_write_tokens, 1);
        assert_eq!(event.token_source, TokenSource::Observed);
        assert_eq!(event.parser_version, USAGE_PARSER_VERSION);
        assert_eq!(event.source_path.as_deref(), Some(path.to_string_lossy().as_ref()));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_pi_session_file_indexes_custom_role_message_content() {
        let root = temp_pi_root("custom-role");
        let session_dir = root.join("--tmp-pi-project--");
        let session_id = "019e5af2-5528-7d10-888a-b299c21d0e2e";
        let path = write_pi_session(
            &session_dir,
            session_id,
            &[
                serde_json::json!({
                    "type": "session",
                    "version": 3,
                    "id": session_id,
                    "timestamp": "1970-01-01T00:00:01.000Z",
                    "cwd": "/tmp/pi-project"
                }),
                serde_json::json!({
                    "type": "message",
                    "id": "custom1",
                    "parentId": null,
                    "timestamp": "1970-01-01T00:00:02.000Z",
                    "message": {
                        "role": "custom",
                        "customType": "extension-context",
                        "content": [{"type": "text", "text": "injected context"}],
                        "display": false,
                        "timestamp": 2000
                    }
                }),
            ],
        );
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();
        let raw = parse_pi_session_file(
            FileScanEntry {
                session_id: session_id.to_string(),
                stat_target: path,
                directory: None,
            },
            mtime,
        )
        .unwrap()
        .unwrap();

        assert_eq!(raw.messages.len(), 1);
        assert_eq!(raw.messages[0].role, Role::User);
        assert_eq!(raw.messages[0].content, "injected context");
        assert_eq!(raw.messages[0].timestamp, Some(2_000));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_pi_session_file_skips_hidden_bash_execution() {
        let root = temp_pi_root("hidden-bash");
        let session_dir = root.join("--tmp-pi-project--");
        let session_id = "019e5af2-5528-7d10-888a-b299c21d0e2e";
        let path = write_pi_session(
            &session_dir,
            session_id,
            &[
                serde_json::json!({
                    "type": "session", "version": 3, "id": session_id,
                    "timestamp": "1970-01-01T00:00:01.000Z", "cwd": "/tmp/pi-project"
                }),
                serde_json::json!({
                    "type": "message", "id": "user1", "parentId": null,
                    "timestamp": "1970-01-01T00:00:02.000Z",
                    "message": {"role": "user", "content": "visible", "timestamp": 2000}
                }),
                serde_json::json!({
                    "type": "message", "id": "bash1", "parentId": "user1",
                    "timestamp": "1970-01-01T00:00:03.000Z",
                    "message": {
                        "role": "bashExecution",
                        "command": "cat secret.txt",
                        "output": "secret output",
                        "excludeFromContext": true,
                        "timestamp": 3000
                    }
                }),
            ],
        );
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();
        let raw = parse_pi_session_file(
            FileScanEntry {
                session_id: session_id.to_string(),
                stat_target: path,
                directory: None,
            },
            mtime,
        )
        .unwrap()
        .unwrap();

        assert_eq!(raw.messages.len(), 1);
        assert_eq!(raw.messages[0].content, "visible");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_pi_session_file_uses_model_change_for_usage_only_assistant_message() {
        let root = temp_pi_root("usage-only");
        let session_dir = root.join("--tmp-pi-project--");
        let session_id = "019e5af2-5528-7d10-888a-b299c21d0e2e";
        let path = write_pi_session(
            &session_dir,
            session_id,
            &[
                serde_json::json!({
                    "type": "session",
                    "version": 3,
                    "id": session_id,
                    "timestamp": "1970-01-01T00:00:01.000Z",
                    "cwd": "/tmp/pi-project"
                }),
                serde_json::json!({
                    "type": "model_change",
                    "id": "model1",
                    "parentId": null,
                    "timestamp": "1970-01-01T00:00:02.000Z",
                    "provider": "anthropic",
                    "modelId": "claude-opus-4-7"
                }),
                serde_json::json!({
                    "type": "message",
                    "id": "assistant-empty",
                    "parentId": "model1",
                    "timestamp": "1970-01-01T00:00:03.000Z",
                    "message": {
                        "role": "assistant",
                        "content": [],
                        "usage": {
                            "input": 5,
                            "output": 7,
                            "cacheRead": 11,
                            "cacheWrite": 13,
                            "totalTokens": 36
                        },
                        "timestamp": 3000
                    }
                }),
            ],
        );
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();
        let raw = parse_pi_session_file(
            FileScanEntry {
                session_id: session_id.to_string(),
                stat_target: path,
                directory: None,
            },
            mtime,
        )
        .unwrap()
        .unwrap();

        assert!(raw.messages.is_empty());
        assert_eq!(raw.started_at, 1_000);
        assert_eq!(raw.usage_events.len(), 1);
        assert_eq!(raw.usage_events[0].message_seq, None);
        assert_eq!(raw.usage_events[0].provider, "anthropic");
        assert_eq!(raw.usage_events[0].model, "claude-opus-4-7");
        assert_eq!(raw.usage_events[0].input_tokens, 5);
        assert_eq!(raw.usage_events[0].output_tokens, 7);
        assert_eq!(raw.usage_events[0].cache_read_tokens, 11);
        assert_eq!(raw.usage_events[0].cache_write_tokens, 13);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_pi_session_file_skips_fork_inherited_usage() {
        let root = temp_pi_root("fork-usage");
        let session_dir = root.join("--tmp-pi-project--");
        let session_id = "019e5af2-5528-7d10-888a-b299c21d0e2e";
        let path = write_pi_session(
            &session_dir,
            session_id,
            &[
                serde_json::json!({
                    "type": "session", "version": 3, "id": session_id,
                    "timestamp": "1970-01-01T00:00:03.000Z", "cwd": "/tmp/pi-project",
                    "parentSession": "/tmp/parent.jsonl"
                }),
                serde_json::json!({
                    "type": "message", "id": "parent-assistant", "timestamp": "1970-01-01T00:00:02.000Z",
                    "message": {"role": "assistant", "content": "old", "usage": {"input": 10}, "timestamp": 2000}
                }),
                serde_json::json!({
                    "type": "message", "id": "child-assistant", "timestamp": "1970-01-01T00:00:04.000Z",
                    "message": {"role": "assistant", "content": "new", "usage": {"input": 5}, "timestamp": 4000}
                }),
            ],
        );

        let parsed = parse_pi_session(&path, 0).unwrap();

        assert_eq!(parsed.messages.len(), 2);
        assert_eq!(parsed.usage_events.len(), 1);
        assert_eq!(parsed.usage_events[0].event_key, "message:child-assistant");
        assert_eq!(parsed.usage_events[0].input_tokens, 5);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_for_sync_skips_unchanged_session_when_usage_state_is_current() {
        let root = temp_pi_root("skip");
        let session_dir = root.join("--tmp-pi-project--");
        let session_id = "019e5af2-5528-7d10-888a-b299c21d0e2e";
        let path = write_pi_session(
            &session_dir,
            session_id,
            &[
                serde_json::json!({
                    "type": "session",
                    "version": 3,
                    "id": session_id,
                    "timestamp": "1970-01-01T00:00:01.000Z",
                    "cwd": "/tmp/pi-project"
                }),
                serde_json::json!({
                    "type": "message",
                    "id": "user1",
                    "parentId": null,
                    "timestamp": "1970-01-01T00:00:02.000Z",
                    "message": {
                        "role": "user",
                        "content": "hello pi",
                        "timestamp": 2000
                    }
                }),
            ],
        );
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();
        let store = setup_store();
        store.insert_session(&make_existing_session(session_id, mtime, 1)).unwrap();
        store
            .persist_usage_events_for_existing_session(
                "pi",
                session_id,
                &[],
                USAGE_PARSER_VERSION,
                Some(mtime),
            )
            .unwrap();

        let result = scan_for_sync_impl(&[root.join("--tmp-pi-project--")], &store, None).unwrap();
        assert_eq!(result.sessions.len(), 0);
        assert_eq!(result.stats.skipped_sessions, 1);

        let _ = fs::remove_dir_all(&root);
    }
}
