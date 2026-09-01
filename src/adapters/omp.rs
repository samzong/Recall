use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;
use tracing::{debug, warn};
use walkdir::WalkDir;

use crate::adapters::file_scan::{self, FileScanEntry};
use crate::adapters::json_util::{json_i64, jsonl_indexed, rfc3339_ms};
use crate::adapters::usage::usage_count;
use crate::adapters::{
    RawMessage, RawSession, ResumeCommand, SourceAdapter, SyncScanResult, SyncScanStats,
    first_timestamp,
};
use crate::db::store::Store;
use crate::types::{ParentLink, ParentRelation, RawUsageEvent, Role, ThreadRole};

pub(crate) struct OmpAdapter;

const METADATA_PARSER_VERSION: u32 = 1;

const USAGE_PARSER_VERSION: u32 = 1;

impl SourceAdapter for OmpAdapter {
    fn id(&self) -> &str {
        "omp"
    }

    fn label(&self) -> &str {
        "OMP"
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        Some(ResumeCommand {
            program: "omp".to_string(),
            args: vec!["--resume".to_string(), source_id.to_string()],
        })
    }

    fn usage_parser_version(&self) -> Option<u32> {
        Some(USAGE_PARSER_VERSION)
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let session_dirs = resolve_omp_session_dirs()?;
        if session_dirs.is_empty() {
            return Ok(vec![]);
        }

        let mut sessions = Vec::new();
        for entry in collect_omp_entries(&session_dirs) {
            let Some(mtime_ms) = file_scan::stat_mtime_ms(&entry.stat_target) else {
                continue;
            };
            if let Some(raw) = parse_omp_session_file(entry, mtime_ms)? {
                sessions.push(raw);
            }
        }

        Ok(sessions)
    }

    fn scan_for_sync(
        &self,
        store: &Store,
        since_ts: Option<i64>,
        include_events: bool,
    ) -> anyhow::Result<Option<SyncScanResult>> {
        let session_dirs = resolve_omp_session_dirs()?;
        if session_dirs.is_empty() {
            return Ok(Some(SyncScanResult { sessions: vec![], stats: SyncScanStats::default() }));
        }

        Ok(Some(scan_for_sync_impl(&session_dirs, store, since_ts, include_events)?))
    }
}

struct ParsedOmpSession {
    session_id: Option<String>,
    cwd: Option<String>,
    started_at: Option<i64>,
    custom_title: Option<String>,
    messages: Vec<RawMessage>,
    usage_events: Vec<RawUsageEvent>,
    parent_session: Option<String>,
}

fn resolve_omp_session_dirs() -> anyhow::Result<Vec<PathBuf>> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home dir"))?;
    let session_dirs = resolve_omp_session_dirs_from(&home);
    if session_dirs.is_empty() {
        debug!("OMP session directory not found, skipping OMP");
    }
    Ok(session_dirs)
}

fn resolve_omp_session_dirs_from(home: &Path) -> Vec<PathBuf> {
    let mut session_dirs = Vec::new();
    let mut seen = HashSet::new();

    push_existing_unique_dir(
        &mut session_dirs,
        &mut seen,
        home.join(".omp").join("agent").join("sessions"),
    );

    let profiles = home.join(".omp").join("profiles");
    if let Ok(entries) = fs::read_dir(profiles) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                push_existing_unique_dir(
                    &mut session_dirs,
                    &mut seen,
                    path.join("agent").join("sessions"),
                );
            }
        }
    }

    session_dirs
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
    include_events: bool,
) -> anyhow::Result<SyncScanResult> {
    let entries = collect_omp_entries(session_dirs);
    file_scan::run_file_scan_with_options(
        store,
        "omp",
        since_ts,
        file_scan::FileScanOptions {
            usage_parser_version: Some(USAGE_PARSER_VERSION),
            event_parser_version: None,
            metadata_parser_version: include_events.then_some(METADATA_PARSER_VERSION),
        },
        entries,
        parse_omp_session_file,
    )
}

fn collect_omp_entries(session_dirs: &[PathBuf]) -> Vec<FileScanEntry> {
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

fn normalize_omp_parent_id(parent: &str) -> Option<String> {
    let stem = Path::new(parent).file_stem().and_then(|stem| stem.to_str()).unwrap_or(parent);
    extract_session_id_from_filename(stem)
}

fn decode_session_dir_name(name: &str) -> Option<String> {
    if name == "-" {
        return dirs::home_dir().map(|home| home.to_string_lossy().into_owned());
    }
    let inner = name.strip_prefix("--")?.strip_suffix("--")?;
    if inner.is_empty() {
        return None;
    }
    Some(format!("/{}", inner.replace('-', "/")))
}

fn parse_omp_session_file(
    entry: FileScanEntry,
    mtime_ms: i64,
) -> anyhow::Result<Option<RawSession>> {
    let source_file_path = entry.stat_target.to_str().map(str::to_string);
    let parsed = match parse_omp_session(&entry.stat_target, mtime_ms) {
        Ok(parsed) => parsed,
        Err(err) => {
            warn!("failed to parse OMP session {}: {err}", entry.stat_target.display());
            return Ok(None);
        }
    };

    if parsed.messages.is_empty() && parsed.usage_events.is_empty() {
        return Ok(None);
    }

    let started_at =
        first_timestamp(parsed.started_at, &parsed.messages, &parsed.usage_events, &[])
            .unwrap_or(0);

    let source_id = parsed.session_id.unwrap_or(entry.session_id);
    let parent_links = match parsed
        .parent_session
        .as_deref()
        .and_then(normalize_omp_parent_id)
        .filter(|parent| parent != &source_id)
    {
        Some(parent) => vec![ParentLink {
            relation: ParentRelation::Fork,
            source: "omp".to_string(),
            source_id: parent,
        }],
        None => Vec::new(),
    };

    Ok(Some(RawSession {
        source_id,
        directory: parsed.cwd.or(entry.directory),
        started_at,
        updated_at: Some(mtime_ms),
        entrypoint: None,
        messages: parsed.messages,
        usage_events: parsed.usage_events,
        usage_parser_version: Some(USAGE_PARSER_VERSION),
        events: Vec::new(),
        event_parser_version: None,
        source_file_path,
        custom_title: parsed.custom_title,
        summary: None,
        duration_minutes: None,
        thread_role: Some(ThreadRole::Primary),
        parent_links,
        metadata_parser_version: Some(METADATA_PARSER_VERSION),
    }))
}

fn parse_omp_session(path: &Path, fallback_timestamp: i64) -> anyhow::Result<ParsedOmpSession> {
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let source_path = path.to_string_lossy().to_string();

    let mut session_id = None;
    let mut cwd = None;
    let mut started_at = None;
    let mut custom_title = None;
    let mut current_provider: Option<String> = None;
    let mut current_model: Option<String> = None;
    let mut inherited_usage_cutoff = None;
    let mut parent_session = None;
    let mut messages = Vec::new();
    let mut usage_events = Vec::new();

    for item in jsonl_indexed(reader.lines()) {
        let (line_index, entry) = item?;

        match entry.get("type").and_then(|value| value.as_str()).unwrap_or("") {
            "title" => {
                if custom_title.is_none() {
                    custom_title = non_empty_str(entry.get("title")).map(str::to_string);
                }
            }
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
                if custom_title.is_none() {
                    custom_title = non_empty_str(entry.get("title")).map(str::to_string);
                }
                if let Some(parent) = entry
                    .get("parentSession")
                    .and_then(|value| value.as_str())
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                {
                    inherited_usage_cutoff = header_timestamp;
                    parent_session = Some(parent.to_string());
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
                    parse_omp_message(
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

    Ok(ParsedOmpSession {
        session_id,
        cwd,
        started_at,
        custom_title,
        messages,
        usage_events,
        parent_session,
    })
}

#[allow(clippy::too_many_arguments)]
fn parse_omp_message(
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
                && let Some(event) = extract_omp_usage_event(
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

fn extract_omp_usage_event(
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
        message_seq,
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
        source_path: Some(source_path.to_string()),
        raw_usage_json: Some(usage.to_string()),
        ..RawUsageEvent::observed(event_key, event_seq, timestamp, USAGE_PARSER_VERSION)
    })
}

fn non_empty_str(value: Option<&Value>) -> Option<&str> {
    value.and_then(|value| value.as_str()).filter(|value| !value.trim().is_empty())
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

    fn temp_omp_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "recall-omp-test-{}-{}",
            label,
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn title_slot_line(title: &str) -> String {
        let mut value = serde_json::json!({
            "type": "title",
            "v": 1,
            "title": title,
            "source": "auto",
            "updatedAt": "1970-01-01T00:00:01.000Z",
            "pad": ""
        });
        loop {
            let serialized = serde_json::to_string(&value).unwrap();
            if serialized.len() >= 255 {
                return serialized;
            }
            let pad = " ".repeat(255 - serialized.len());
            value["pad"] = Value::String(pad);
        }
    }

    fn write_omp_session(
        dir: &Path,
        session_id: &str,
        title: Option<&str>,
        lines: &[Value],
    ) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(format!("2026-09-01T16-39-58-396Z_{session_id}.jsonl"));
        let mut file = fs::File::create(&path).unwrap();
        if let Some(title) = title {
            writeln!(file, "{}", title_slot_line(title)).unwrap();
        }
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
        path
    }

    fn make_existing_session(source_id: &str, updated_at: i64, message_count: u32) -> Session {
        Session {
            id: format!("internal-{source_id}"),
            source: "omp".to_string(),
            source_id: source_id.to_string(),
            title: "existing".to_string(),
            directory: Some("/tmp/omp-project".to_string()),
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
    fn resume_uses_official_flag() {
        let command = OmpAdapter.resume_command("01a05dd7-7cbc-7005-818b-73de30e4dc42").unwrap();
        assert_eq!(command.program, "omp");
        assert_eq!(
            command.args,
            vec!["--resume".to_string(), "01a05dd7-7cbc-7005-818b-73de30e4dc42".to_string()]
        );
    }

    #[test]
    fn extract_session_id_from_filename_reads_uuid_tail() {
        assert_eq!(
            extract_session_id_from_filename(
                "2026-09-01T16-39-58-396Z_01a05dd7-7cbc-7005-818b-73de30e4dc42"
            ),
            Some("01a05dd7-7cbc-7005-818b-73de30e4dc42".to_string())
        );
        assert_eq!(extract_session_id_from_filename("not-a-session"), None);
    }

    #[test]
    fn decode_session_dir_name_reads_absolute_and_home_buckets() {
        assert_eq!(decode_session_dir_name("--private-tmp--").as_deref(), Some("/private/tmp"));
        assert_eq!(decode_session_dir_name("-git-samzong-Recall"), None);
    }

    #[test]
    fn resolve_session_dirs_finds_default_and_profile_roots() {
        let root = temp_omp_root("dirs");
        let default_sessions = root.join(".omp").join("agent").join("sessions");
        let profile_sessions =
            root.join(".omp").join("profiles").join("work").join("agent").join("sessions");
        fs::create_dir_all(&default_sessions).unwrap();
        fs::create_dir_all(&profile_sessions).unwrap();

        let dirs = resolve_omp_session_dirs_from(&root);
        assert!(dirs.iter().any(|dir| dir == &default_sessions));
        assert!(dirs.iter().any(|dir| dir == &profile_sessions));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_session_dirs_ignores_pi_and_custom_agent_roots() {
        let root = temp_omp_root("no-pi");
        let omp_sessions = root.join(".omp").join("agent").join("sessions");
        let pi_sessions = root.join(".pi").join("agent").join("sessions");
        let custom_sessions = root.join("custom-agent").join("sessions");
        fs::create_dir_all(&omp_sessions).unwrap();
        fs::create_dir_all(&pi_sessions).unwrap();
        fs::create_dir_all(&custom_sessions).unwrap();

        assert_eq!(resolve_omp_session_dirs_from(&root), vec![omp_sessions]);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_session_dirs_skips_missing_roots() {
        let root = temp_omp_root("missing");
        assert!(resolve_omp_session_dirs_from(&root).is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_omp_session_file_reads_title_slot_messages_and_usage() {
        let root = temp_omp_root("parse");
        let session_dir = root.join("-git-samzong-Recall");
        let session_id = "01a05dd7-7cbc-7005-818b-73de30e4dc42";
        let path = write_omp_session(
            &session_dir,
            session_id,
            Some("Add omp.sh as recall adapter"),
            &[
                serde_json::json!({
                    "type": "session",
                    "version": 3,
                    "id": session_id,
                    "timestamp": "1970-01-01T00:00:01.000Z",
                    "cwd": "/Users/x/git/samzong/Recall"
                }),
                serde_json::json!({
                    "type": "model_change",
                    "id": "model1",
                    "parentId": null,
                    "timestamp": "1970-01-01T00:00:01.500Z",
                    "model": "openrouter/deepseek/deepseek-v4-flash-0731"
                }),
                serde_json::json!({
                    "type": "message",
                    "id": "user1",
                    "parentId": null,
                    "timestamp": "1970-01-01T00:00:02.000Z",
                    "message": {
                        "role": "user",
                        "content": [{"type": "text", "text": "hello omp"}],
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
                            {"type": "text", "text": "done"},
                            {"type": "image", "mimeType": "image/png"}
                        ],
                        "provider": "openrouter",
                        "model": "deepseek/deepseek-v4-flash-0731",
                        "usage": {
                            "input": 10,
                            "output": 3,
                            "cacheRead": 2,
                            "cacheWrite": 1,
                            "reasoningTokens": 4,
                            "totalTokens": 20
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
        let slot = fs::read_to_string(&path).unwrap();
        assert_eq!(slot.find('\n'), Some(255));

        let mtime = file_scan::stat_mtime_ms(&path).unwrap();
        let raw = parse_omp_session_file(
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
        assert_eq!(raw.directory.as_deref(), Some("/Users/x/git/samzong/Recall"));
        assert_eq!(raw.custom_title.as_deref(), Some("Add omp.sh as recall adapter"));
        assert_eq!(raw.started_at, 1_000);
        assert_eq!(raw.updated_at, Some(mtime));
        assert_eq!(raw.source_file_path.as_deref(), path.to_str());
        assert_eq!(raw.messages.len(), 3);
        assert_eq!(raw.messages[0].role, Role::User);
        assert_eq!(raw.messages[0].content, "hello omp");
        assert!(raw.messages[1].content.contains("done"));
        assert!(raw.messages[1].content.contains("[read]"));
        assert!(!raw.messages[1].content.contains("hidden chain of thought"));
        assert!(!raw.messages[1].content.contains("image/png"));
        assert!(raw.messages[2].content.contains("[read result]"));

        assert_eq!(raw.usage_events.len(), 1);
        let event = &raw.usage_events[0];
        assert_eq!(event.event_key, "message:assistant1");
        assert_eq!(event.message_seq, Some(1));
        assert_eq!(event.timestamp, 3_000);
        assert_eq!(event.provider, "openrouter");
        assert_eq!(event.model, "deepseek/deepseek-v4-flash-0731");
        assert_eq!(event.input_tokens, 10);
        assert_eq!(event.output_tokens, 3);
        assert_eq!(event.cache_read_tokens, 2);
        assert_eq!(event.cache_write_tokens, 1);
        assert_eq!(event.reasoning_tokens, 4);
        assert_eq!(event.token_source, crate::types::TokenSource::Observed);
        assert_eq!(event.parser_version, USAGE_PARSER_VERSION);
        assert_eq!(event.source_path.as_deref(), Some(path.to_string_lossy().as_ref()));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_omp_session_file_prefers_title_slot_over_header() {
        let root = temp_omp_root("title-pref");
        let session_dir = root.join("--tmp-omp-project--");
        let session_id = "01a05dd7-7cbc-7005-818b-73de30e4dc42";
        let path = write_omp_session(
            &session_dir,
            session_id,
            Some("slot title"),
            &[
                serde_json::json!({
                    "type": "session",
                    "version": 3,
                    "id": session_id,
                    "title": "header title",
                    "timestamp": "1970-01-01T00:00:01.000Z",
                    "cwd": "/tmp/omp-project"
                }),
                serde_json::json!({
                    "type": "message",
                    "id": "user1",
                    "timestamp": "1970-01-01T00:00:02.000Z",
                    "message": {
                        "role": "user",
                        "content": "hello",
                        "timestamp": 2000
                    }
                }),
            ],
        );
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();
        let raw = parse_omp_session_file(
            FileScanEntry {
                session_id: session_id.to_string(),
                stat_target: path,
                directory: None,
            },
            mtime,
        )
        .unwrap()
        .unwrap();

        assert_eq!(raw.custom_title.as_deref(), Some("slot title"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn collect_omp_entries_ignores_sidecar_logs() {
        let root = temp_omp_root("sidecar");
        let session_dir = root.join("-git-samzong-Recall");
        let session_id = "01a05dd7-7cbc-7005-818b-73de30e4dc42";
        let path = write_omp_session(
            &session_dir,
            session_id,
            None,
            &[serde_json::json!({
                "type": "session",
                "version": 3,
                "id": session_id,
                "timestamp": "1970-01-01T00:00:01.000Z",
                "cwd": "/tmp/omp-project"
            })],
        );
        let sidecar = session_dir.join(path.file_stem().unwrap());
        fs::create_dir_all(&sidecar).unwrap();
        fs::write(sidecar.join("4.bash-original.log"), "log").unwrap();

        let entries = collect_omp_entries(&[session_dir]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id, session_id);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_omp_session_file_skips_hidden_bash_execution() {
        let root = temp_omp_root("hidden-bash");
        let session_dir = root.join("--tmp-omp-project--");
        let session_id = "01a05dd7-7cbc-7005-818b-73de30e4dc42";
        let path = write_omp_session(
            &session_dir,
            session_id,
            None,
            &[
                serde_json::json!({
                    "type": "session", "version": 3, "id": session_id,
                    "timestamp": "1970-01-01T00:00:01.000Z", "cwd": "/tmp/omp-project"
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
        let raw = parse_omp_session_file(
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
    fn parse_omp_session_file_skips_fork_inherited_usage() {
        let root = temp_omp_root("fork-usage");
        let session_dir = root.join("--tmp-omp-project--");
        let session_id = "01a05dd7-7cbc-7005-818b-73de30e4dc42";
        let path = write_omp_session(
            &session_dir,
            session_id,
            None,
            &[
                serde_json::json!({
                    "type": "session", "version": 3, "id": session_id,
                    "timestamp": "1970-01-01T00:00:03.000Z", "cwd": "/tmp/omp-project",
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

        let parsed = parse_omp_session(&path, 0).unwrap();

        assert_eq!(parsed.messages.len(), 2);
        assert_eq!(parsed.usage_events.len(), 1);
        assert_eq!(parsed.usage_events[0].event_key, "message:child-assistant");
        assert_eq!(parsed.usage_events[0].input_tokens, 5);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_for_sync_skips_unchanged_session_when_usage_state_is_current() {
        let root = temp_omp_root("skip");
        let session_dir = root.join("--tmp-omp-project--");
        let session_id = "01a05dd7-7cbc-7005-818b-73de30e4dc42";
        let path = write_omp_session(
            &session_dir,
            session_id,
            None,
            &[
                serde_json::json!({
                    "type": "session",
                    "version": 3,
                    "id": session_id,
                    "timestamp": "1970-01-01T00:00:01.000Z",
                    "cwd": "/tmp/omp-project"
                }),
                serde_json::json!({
                    "type": "message",
                    "id": "user1",
                    "parentId": null,
                    "timestamp": "1970-01-01T00:00:02.000Z",
                    "message": {
                        "role": "user",
                        "content": "hello omp",
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
                "omp",
                session_id,
                &[],
                USAGE_PARSER_VERSION,
                Some(mtime),
            )
            .unwrap();
        store
            .persist_topology_for_existing_session(
                "omp",
                session_id,
                &crate::db::store::SessionTopologyWrite {
                    thread_role: None,
                    parents: &[],
                    parser_version: Some(METADATA_PARSER_VERSION),
                },
            )
            .unwrap();

        let result = scan_for_sync_impl(&[session_dir], &store, None, true).unwrap();
        assert_eq!(result.sessions.len(), 0);
        assert_eq!(result.stats.skipped_sessions, 1);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_omp_session_maps_parent_session_to_primary_fork() {
        let root = temp_omp_root("parent-session");
        let session_dir = root.join("--tmp-omp-project--");
        let session_id = "01a05dd7-7cbc-7005-818b-73de30e4dc42";
        let path = write_omp_session(
            &session_dir,
            session_id,
            None,
            &[
                serde_json::json!({
                    "type": "session",
                    "version": 3,
                    "id": session_id,
                    "parentSession": "/home/x/.omp/agent/sessions/--proj--/2026-09-01T16-39-58-396Z_019e0000-0000-0000-0000-000000000001.jsonl",
                    "timestamp": "1970-01-01T00:00:01.000Z",
                    "cwd": "/tmp/omp-project"
                }),
                serde_json::json!({
                    "type": "message",
                    "id": "user1",
                    "timestamp": "1970-01-01T00:00:02.000Z",
                    "message": {
                        "role": "user",
                        "content": [{"type": "text", "text": "hello omp"}],
                        "timestamp": 2000
                    }
                }),
            ],
        );
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();
        let entry = FileScanEntry {
            session_id: session_id.to_string(),
            stat_target: path,
            directory: None,
        };

        let raw = parse_omp_session_file(entry, mtime).unwrap().unwrap();

        assert_eq!(raw.thread_role, Some(ThreadRole::Primary));
        assert_eq!(
            raw.parent_links,
            vec![ParentLink {
                relation: ParentRelation::Fork,
                source: "omp".to_string(),
                source_id: "019e0000-0000-0000-0000-000000000001".to_string(),
            }]
        );
        assert_eq!(raw.metadata_parser_version, Some(METADATA_PARSER_VERSION));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_omp_session_drops_unresolvable_parent_session() {
        let root = temp_omp_root("parent-unresolvable");
        let session_dir = root.join("--tmp-omp-project--");
        let session_id = "01a05dd7-7cbc-7005-818b-73de30e4dc43";
        let path = write_omp_session(
            &session_dir,
            session_id,
            None,
            &[
                serde_json::json!({
                    "type": "session",
                    "version": 3,
                    "id": session_id,
                    "parentSession": "not-a-session-path",
                    "timestamp": "1970-01-01T00:00:01.000Z",
                    "cwd": "/tmp/omp-project"
                }),
                serde_json::json!({
                    "type": "message",
                    "id": "user1",
                    "timestamp": "1970-01-01T00:00:02.000Z",
                    "message": {
                        "role": "user",
                        "content": [{"type": "text", "text": "hello omp"}],
                        "timestamp": 2000
                    }
                }),
            ],
        );
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();
        let entry = FileScanEntry {
            session_id: session_id.to_string(),
            stat_target: path,
            directory: None,
        };

        let raw = parse_omp_session_file(entry, mtime).unwrap().unwrap();

        assert_eq!(raw.thread_role, Some(ThreadRole::Primary));
        assert!(raw.parent_links.is_empty());

        let _ = fs::remove_dir_all(&root);
    }
}
