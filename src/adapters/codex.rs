use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;
use tracing::debug;
use walkdir::WalkDir;

use crate::adapters::AdapterSyncContext;
use crate::adapters::events;
use crate::adapters::file_scan::{self, FileScanEntry};
use crate::adapters::invocation_probe::{
    InvocationProbeBudget, ProviderInvocationProbe, is_discovery_tool, nonce_matches_input,
    probe_recent_files,
};
use crate::adapters::json_util::{jsonl_indexed, rfc3339_ms};
use crate::adapters::paths::{self, resolve_home_dir};
use crate::adapters::{
    RawMessage, RawSession, ResumeCommand, SourceAdapter, SyncScanResult, SyncScanStats,
    first_timestamp, last_timestamp,
};
use crate::types::{ParentLink, ParentRelation, RawSessionEvent, RawUsageEvent, Role, ThreadRole};

pub(crate) struct CodexAdapter;

const USAGE_PARSER_VERSION: u32 = 6;
const EVENT_PARSER_VERSION: u32 = 4;
const METADATA_PARSER_VERSION: u32 = 1;

impl SourceAdapter for CodexAdapter {
    fn id(&self) -> &str {
        "codex"
    }
    fn label(&self) -> &str {
        "CDX"
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        Some(ResumeCommand {
            program: "codex".to_string(),
            args: vec!["resume".to_string(), source_id.to_string()],
        })
    }

    fn app_command(&self, source_id: &str) -> Option<ResumeCommand> {
        Some(open_url_command(codex_thread_url(source_id)))
    }

    fn usage_parser_version(&self) -> Option<u32> {
        Some(USAGE_PARSER_VERSION)
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let Some(codex_dir) = resolve_codex_dir()? else {
            return Ok(vec![]);
        };
        let sessions_dir = codex_dir.join("sessions");
        let archived_dir = codex_dir.join("archived_sessions");

        let mut sessions = Vec::new();
        for entry in collect_codex_entries(&[&sessions_dir, &archived_dir]) {
            let Some(mtime_ms) = file_scan::stat_mtime_ms(&entry.stat_target) else {
                continue;
            };
            if let Some(raw) = parse_codex_session_for_entry(entry, mtime_ms, true)? {
                sessions.push(raw);
            }
        }
        Ok(sessions)
    }

    fn scan_for_sync(
        &self,
        context: &AdapterSyncContext,
        since_ts: Option<i64>,
        include_events: bool,
    ) -> anyhow::Result<Option<SyncScanResult>> {
        let Some(codex_dir) = resolve_codex_dir()? else {
            return Ok(Some(SyncScanResult {
                sessions: vec![],
                stats: SyncScanStats::default(),
                observations: Vec::new(),
            }));
        };
        let result = scan_for_sync_impl(&codex_dir, context, since_ts, include_events)?;
        Ok(Some(result))
    }
}

fn codex_thread_url(source_id: &str) -> String {
    format!("codex://threads/{source_id}")
}

#[cfg(target_os = "macos")]
fn open_url_command(url: String) -> ResumeCommand {
    ResumeCommand { program: "open".to_string(), args: vec![url] }
}

#[cfg(target_os = "windows")]
fn open_url_command(url: String) -> ResumeCommand {
    ResumeCommand {
        program: "cmd".to_string(),
        args: vec!["/C".to_string(), "start".to_string(), String::new(), url],
    }
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
fn open_url_command(url: String) -> ResumeCommand {
    ResumeCommand { program: "xdg-open".to_string(), args: vec![url] }
}

fn resolve_codex_dir() -> anyhow::Result<Option<PathBuf>> {
    if let Some(dir) = paths::env_path_dir("CODEX_HOME") {
        if dir.is_dir() {
            return Ok(Some(dir));
        }
        debug!("CODEX_HOME not found, skipping Codex");
        return Ok(None);
    }
    resolve_home_dir(".codex", "~/.codex not found, skipping Codex")
}

pub(crate) fn probe_invocation_nonce(
    nonce: &str,
    budget: InvocationProbeBudget,
) -> anyhow::Result<ProviderInvocationProbe> {
    let Some(codex_dir) = resolve_codex_dir()? else {
        return Ok(ProviderInvocationProbe {
            source_ids: Vec::new(),
            files_read: 0,
            bytes_read: 0,
            complete: true,
        });
    };
    Ok(probe_invocation_nonce_in(&codex_dir, nonce, budget))
}

fn probe_invocation_nonce_in(
    codex_dir: &Path,
    nonce: &str,
    budget: InvocationProbeBudget,
) -> ProviderInvocationProbe {
    let sessions_dir = codex_dir.join("sessions");
    let archived_dir = codex_dir.join("archived_sessions");
    probe_recent_files(
        nonce,
        collect_codex_entries(&[&sessions_dir, &archived_dir]),
        budget,
        codex_invocation_input,
    )
}

fn codex_invocation_input(value: &Value, nonce: &str) -> anyhow::Result<bool> {
    let Some(payload) = value.get("payload") else {
        return Ok(false);
    };
    if value.get("type").and_then(Value::as_str) == Some("event_msg")
        && payload.get("type").and_then(Value::as_str) == Some("item_completed")
    {
        let Some(item) = payload.get("item") else {
            return Ok(false);
        };
        return Ok(item.get("type").and_then(Value::as_str) == Some("McpToolCall")
            && item.get("tool").and_then(Value::as_str).is_some_and(is_discovery_tool)
            && item.get("arguments").is_some_and(|input| nonce_matches_input(input, nonce)));
    }
    if value.get("type").and_then(Value::as_str) != Some("response_item") {
        return Ok(false);
    }
    if !matches!(
        payload.get("type").and_then(Value::as_str),
        Some("function_call" | "custom_tool_call")
    ) {
        return Ok(false);
    }
    let Some(name) = payload.get("name").and_then(Value::as_str) else {
        return Ok(false);
    };
    if payload.get("type").and_then(Value::as_str) == Some("custom_tool_call") && name == "exec" {
        return Ok(payload
            .get("input")
            .and_then(Value::as_str)
            .is_some_and(|input| codex_tool_wrapper_matches(input, nonce)));
    }
    if !is_discovery_tool(name) {
        return Ok(false);
    }
    let Some(input) = payload.get("arguments").or_else(|| payload.get("input")) else {
        return Ok(false);
    };
    match input {
        Value::String(input) => {
            let input: Value = serde_json::from_str(input)?;
            Ok(nonce_matches_input(&input, nonce))
        }
        input => Ok(nonce_matches_input(input, nonce)),
    }
}

fn codex_tool_wrapper_matches(input: &str, nonce: &str) -> bool {
    let Some(tool_start) = input.find("tools.") else {
        return false;
    };
    let tool = &input[tool_start + "tools.".len()..];
    let name_end = tool
        .find(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        .unwrap_or(tool.len());
    let name = &tool[..name_end];
    if !is_discovery_tool(name) {
        return false;
    }
    let Some(call) = tool[name_end..].trim_start().strip_prefix('(') else {
        return false;
    };
    let Some(arguments) = call_arguments(call) else {
        return false;
    };
    string_property_matches(arguments, "invocation_nonce", nonce)
}

fn call_arguments(input: &str) -> Option<&str> {
    let mut depth = 1_usize;
    let mut quote = None;
    let mut escaped = false;
    for (index, character) in input.char_indices() {
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == active_quote {
                quote = None;
            }
            continue;
        }
        match character {
            '\'' | '"' | '`' => quote = Some(character),
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&input[..index]);
                }
            }
            _ => {}
        }
    }
    None
}

fn string_property_matches(input: &str, property: &str, expected: &str) -> bool {
    input.match_indices(property).any(|(start, _)| {
        let property_end = start + property.len();
        let before = input[..start].chars().next_back();
        let after = input[property_end..].chars().next();
        if before.is_some_and(|character| character.is_ascii_alphanumeric() || character == '_')
            || after.is_some_and(|character| character.is_ascii_alphanumeric() || character == '_')
        {
            return false;
        }
        let Some(value) = input[property_end..].trim_start().strip_prefix(':') else {
            return false;
        };
        json_string(value.trim_start()).as_deref() == Some(expected)
    })
}

fn json_string(input: &str) -> Option<String> {
    if !input.starts_with('"') {
        return None;
    }
    let mut escaped = false;
    for (index, character) in input.char_indices().skip(1) {
        if escaped {
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == '"' {
            return serde_json::from_str(&input[..=index]).ok();
        }
    }
    None
}

fn scan_for_sync_impl(
    codex_dir: &Path,
    context: &AdapterSyncContext,
    since_ts: Option<i64>,
    include_events: bool,
) -> anyhow::Result<SyncScanResult> {
    let sessions_dir = codex_dir.join("sessions");
    let archived_dir = codex_dir.join("archived_sessions");
    let entries = collect_codex_entries(&[&sessions_dir, &archived_dir]);
    file_scan::run_file_scan_with_options(
        context,
        since_ts,
        file_scan::FileScanOptions {
            usage_parser_version: Some(USAGE_PARSER_VERSION),
            event_parser_version: include_events.then_some(EVENT_PARSER_VERSION),
            metadata_parser_version: include_events.then_some(METADATA_PARSER_VERSION),
        },
        entries,
        |entry, mtime_ms| {
            let Some(session) = parse_codex_session_for_entry(entry, mtime_ms, include_events)?
            else {
                return Ok(None);
            };
            if !include_events && session.messages.is_empty() && session.usage_events.is_empty() {
                return Ok(None);
            }
            Ok(Some(session))
        },
    )
}

fn collect_codex_entries(base_dirs: &[&Path]) -> Vec<FileScanEntry> {
    let mut entries = Vec::new();
    for dir in base_dirs {
        if !dir.exists() {
            continue;
        }
        for walk_entry in WalkDir::new(dir).into_iter().filter_map(|e| e.ok()) {
            let path = walk_entry.path();
            let ext = path.extension().and_then(|e| e.to_str());
            if ext != Some("jsonl") && ext != Some("json") {
                continue;
            }
            if !path.is_file() {
                continue;
            }
            let stem = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) if !s.is_empty() => s,
                _ => continue,
            };
            let Some(session_id) = extract_session_id_from_filename(stem) else {
                continue;
            };
            entries.push(FileScanEntry {
                session_id,
                stat_target: path.to_path_buf(),
                directory: None,
            });
        }
    }
    entries
}

fn parse_codex_session_for_entry(
    entry: FileScanEntry,
    mtime_ms: i64,
    include_events: bool,
) -> anyhow::Result<Option<RawSession>> {
    let source_file_path = entry.stat_target.to_str().map(str::to_string);
    let mut raw = match parse_codex_session_with_options(&entry.stat_target, include_events) {
        Ok(Some(raw)) => raw,
        Ok(None) => return Ok(None),
        Err(e) => {
            debug!("failed to parse codex session {}: {e}", entry.stat_target.display());
            return Ok(None);
        }
    };
    raw.source_id = entry.session_id;
    raw.updated_at = Some(mtime_ms);
    raw.source_file_path = source_file_path;
    Ok(Some(raw))
}

fn extract_session_id_from_filename(stem: &str) -> Option<String> {
    if stem.len() < 37 {
        return None;
    }
    let (prefix, tail) = stem.split_at(stem.len() - 36);
    if !prefix.ends_with('-') {
        return None;
    }
    uuid::Uuid::try_parse(tail).ok().map(|_| tail.to_string())
}

/// Keyed by payload id so forked rollouts pick the child's meta, not an inherited parent's.
struct CodexMetaTopology {
    meta_id: Option<String>,
    thread_role: Option<ThreadRole>,
    parent_links: Vec<ParentLink>,
}

fn codex_thread_role(payload: &Value) -> Option<ThreadRole> {
    match payload.get("thread_source").and_then(|v| v.as_str()) {
        Some("user") => return Some(ThreadRole::Primary),
        Some("subagent") => return Some(ThreadRole::Subagent),
        _ => {}
    }
    let source = payload.get("source")?;
    if source.get("subagent").is_some() || source.as_str() == Some("subagent") {
        return Some(ThreadRole::Subagent);
    }
    source.as_str().filter(|s| !s.trim().is_empty()).map(|_| ThreadRole::Primary)
}

/// `spawn` ← `parent_thread_id` / `source.subagent.thread_spawn`; `fork` ← `forked_from_id`.
fn codex_parent_links(payload: &Value) -> Vec<ParentLink> {
    let mut links = Vec::new();
    let mut push = |relation: ParentRelation, id: &str| {
        let id = id.trim();
        if id.is_empty() {
            return;
        }
        let link = ParentLink { relation, source: "codex".to_string(), source_id: id.to_string() };
        if !links.contains(&link) {
            links.push(link);
        }
    };
    let spawn_parent = payload.get("parent_thread_id").and_then(|v| v.as_str()).or_else(|| {
        payload
            .get("source")
            .and_then(|s| s.get("subagent"))
            .and_then(|s| s.get("thread_spawn"))
            .and_then(|s| s.get("parent_thread_id"))
            .and_then(|v| v.as_str())
    });
    if let Some(parent) = spawn_parent {
        push(ParentRelation::Spawn, parent);
    }
    if let Some(parent) = payload.get("forked_from_id").and_then(|v| v.as_str()) {
        push(ParentRelation::Fork, parent);
    }
    links
}

#[cfg(test)]
fn parse_codex_session(path: &Path) -> anyhow::Result<Option<RawSession>> {
    parse_codex_session_with_options(path, true)
}

pub(crate) fn parse_codex_session_with_options(
    path: &Path,
    include_events: bool,
) -> anyhow::Result<Option<RawSession>> {
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let fallback_timestamp = file_scan::stat_mtime_ms(path).unwrap_or(0);
    let mut meta_id: Option<String> = None;
    let mut meta_cwd: Option<String> = None;
    let mut meta_timestamp: Option<i64> = None;
    let mut meta_topologies: Vec<CodexMetaTopology> = Vec::new();
    let mut messages = Vec::new();
    let mut usage_events = Vec::new();
    let mut events = Vec::new();
    let mut current_model: Option<String> = None;
    let mut provider: Option<String> = None;
    let mut previous_totals: Option<CodexUsageTotals> = None;
    let mut pending_model_usage_indices: Vec<usize> = Vec::new();
    let mut forked_child_waiting_for_turn_context = false;
    let mut forked_child_inherited_baseline: Option<CodexUsageTotals> = None;
    let mut forked_child_inherited_reported_total: Option<i64> = None;
    let mut last_visible_message_seq: Option<u32> = None;
    let mut user_dedup = CodexUserDedup::default();
    let source_path = path.to_string_lossy().to_string();

    for item in jsonl_indexed(reader.lines()) {
        let (line_index, v) = item?;

        let msg_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");

        let payload = v.get("payload");
        let payload_type =
            payload.and_then(|p| p.get("type")).and_then(|t| t.as_str()).unwrap_or("");
        let is_token_count = msg_type == "event_msg" && payload_type == "token_count";
        let event_has_model = payload.and_then(extract_codex_model).is_some()
            || (is_token_count
                && payload.and_then(|p| p.get("info")).and_then(extract_codex_model).is_some());

        if !pending_model_usage_indices.is_empty()
            && !event_has_model
            && !is_token_count
            && msg_type != "session_meta"
        {
            pending_model_usage_indices.clear();
        }

        if forked_child_waiting_for_turn_context {
            if msg_type == "turn_context" {
                forked_child_waiting_for_turn_context = false;
            } else {
                if is_token_count && let Some(info) = payload.and_then(|p| p.get("info")) {
                    remember_forked_child_inherited_baseline(
                        &mut previous_totals,
                        &mut forked_child_inherited_baseline,
                        &mut forked_child_inherited_reported_total,
                        info,
                    );
                }
                continue;
            }
        }

        match msg_type {
            "session_meta" => {
                if let Some(payload) = payload {
                    meta_id = payload.get("id").and_then(|s| s.as_str()).map(String::from);
                    meta_cwd = payload.get("cwd").and_then(|s| s.as_str()).map(String::from);
                    provider = payload
                        .get("model_provider")
                        .and_then(|s| s.as_str())
                        .filter(|s| !s.trim().is_empty())
                        .map(String::from)
                        .or(provider);
                    if let Some(model) = extract_codex_model(payload) {
                        current_model = Some(model.clone());
                        apply_pending_codex_model(
                            &mut usage_events,
                            &mut pending_model_usage_indices,
                            &model,
                        );
                    }
                    meta_timestamp = rfc3339_ms(payload.get("timestamp"));
                    meta_topologies.push(CodexMetaTopology {
                        meta_id: meta_id.clone(),
                        thread_role: codex_thread_role(payload),
                        parent_links: codex_parent_links(payload),
                    });
                    if payload.get("forked_from_id").and_then(|s| s.as_str()).is_some() {
                        forked_child_waiting_for_turn_context = true;
                        forked_child_inherited_baseline = None;
                        forked_child_inherited_reported_total = None;
                    }
                }
            }
            "turn_context" => {
                if let Some(payload) = payload
                    && let Some(model) = extract_codex_model(payload)
                {
                    current_model = Some(model.clone());
                    apply_pending_codex_model(
                        &mut usage_events,
                        &mut pending_model_usage_indices,
                        &model,
                    );
                }
            }
            "event_msg" => {
                if let Some(payload) = payload {
                    match payload_type {
                        "token_count" => {
                            if let Some(info) = payload.get("info") {
                                let total_usage = info
                                    .get("total_token_usage")
                                    .and_then(CodexUsageTotals::from_usage);
                                if forked_child_matches_inherited_baseline(
                                    forked_child_inherited_baseline,
                                    forked_child_inherited_reported_total,
                                    info,
                                    total_usage,
                                ) {
                                    if let Some(total) = total_usage {
                                        previous_totals = Some(total);
                                    }
                                    forked_child_inherited_baseline = None;
                                    forked_child_inherited_reported_total = None;
                                    continue;
                                }
                                forked_child_inherited_baseline = None;
                                forked_child_inherited_reported_total = None;
                            }
                            if let Some(event) = extract_codex_usage_event(
                                payload,
                                line_index as u32,
                                &mut previous_totals,
                                parse_timestamp(&v).unwrap_or(fallback_timestamp),
                                provider.as_deref().unwrap_or("openai"),
                                current_model.as_deref(),
                                &source_path,
                            ) {
                                if event.model == "unknown" {
                                    pending_model_usage_indices.push(usage_events.len());
                                } else {
                                    current_model = Some(event.model.clone());
                                }
                                usage_events.push(event);
                            }
                        }
                        "user_message" => {
                            if let Some(text) = payload.get("message").and_then(|m| m.as_str())
                                && !text.is_empty()
                            {
                                let ts = parse_timestamp(&v);
                                last_visible_message_seq = push_codex_user(
                                    &mut messages,
                                    &mut user_dedup,
                                    CodexUserStream::EventMsg,
                                    text.to_string(),
                                    ts,
                                );
                            }
                        }
                        "agent_message" => {
                            if let Some(text) = payload.get("message").and_then(|m| m.as_str())
                                && !text.is_empty()
                            {
                                let ts = parse_timestamp(&v);
                                last_visible_message_seq = push_codex_message(
                                    &mut messages,
                                    Role::Assistant,
                                    text.to_string(),
                                    ts,
                                );
                            }
                        }
                        _ => {}
                    }
                }
            }
            "response_item" => {
                if let Some(payload) = v.get("payload") {
                    let timestamp = parse_timestamp(&v);
                    let payload_type = payload.get("type").and_then(|t| t.as_str());
                    let role = payload.get("role").and_then(|r| r.as_str());
                    if payload_type == Some("message") && role == Some("user") {
                        let text = extract_content_array(payload.get("content"));
                        if !text.is_empty() && !is_codex_injected_context(&text) {
                            last_visible_message_seq = push_codex_user(
                                &mut messages,
                                &mut user_dedup,
                                CodexUserStream::ResponseItem,
                                text,
                                timestamp,
                            );
                        }
                    } else if payload_type == Some("message") && role == Some("assistant") {
                        let text = extract_content_array(payload.get("content"));
                        let message_seq = if text.is_empty() {
                            None
                        } else {
                            push_codex_message(&mut messages, Role::Assistant, text, timestamp)
                        };
                        if include_events {
                            collect_codex_content_events(
                                payload.get("content"),
                                timestamp,
                                &source_path,
                                line_index,
                                last_visible_message_seq,
                                message_seq,
                                &mut events,
                            );
                        }
                        if codex_content_has_visible_text(payload.get("content")) {
                            last_visible_message_seq = message_seq;
                        }
                    } else if include_events
                        && payload_type == Some("message")
                        && matches!(role, Some("developer" | "system"))
                    {
                        collect_codex_meta_event(
                            payload,
                            timestamp,
                            &source_path,
                            line_index,
                            &mut events,
                        );
                    } else if include_events {
                        collect_codex_response_item_event(
                            payload,
                            timestamp,
                            &source_path,
                            line_index,
                            last_visible_message_seq,
                            &mut events,
                        );
                    }
                }
            }
            _ => {}
        }
    }

    if messages.is_empty() && usage_events.is_empty() && events.is_empty() {
        return Ok(None);
    }

    let source_id = meta_id.unwrap_or_else(|| {
        path.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown").to_string()
    });

    let started_at =
        first_timestamp(meta_timestamp, &messages, &usage_events, &events).unwrap_or(0);

    // Prefer the metadata record whose payload id matches the source id derived
    // from the rollout filename so inherited parent metadata in forked rollouts
    // never overwrites the child identity; fall back to the last processed
    // record (the child's own, since inherited parent metadata is skipped).
    let filename_id =
        path.file_stem().and_then(|s| s.to_str()).and_then(extract_session_id_from_filename);
    let (thread_role, parent_links) = meta_topologies
        .iter()
        .find(|topo| topo.meta_id.is_some() && topo.meta_id.as_deref() == filename_id.as_deref())
        .or_else(|| meta_topologies.last())
        .map(|topo| (topo.thread_role, topo.parent_links.clone()))
        .unwrap_or((None, Vec::new()));

    Ok(Some(RawSession {
        source_id,
        directory: meta_cwd,
        started_at,
        updated_at: last_timestamp(None, &messages, &usage_events, &events),
        entrypoint: None,
        messages,
        usage_events,
        usage_parser_version: Some(USAGE_PARSER_VERSION),
        events,
        event_parser_version: include_events.then_some(EVENT_PARSER_VERSION),
        source_file_path: None,
        custom_title: None,
        summary: None,
        duration_minutes: None,
        thread_role,
        parent_links,
        metadata_parser_version: Some(METADATA_PARSER_VERSION),
        refresh_session_on_metadata_backfill: false,
    }))
}

fn collect_codex_response_item_event(
    payload: &Value,
    timestamp: Option<i64>,
    source_path: &str,
    line_index: usize,
    message_seq: Option<u32>,
    events_out: &mut Vec<RawSessionEvent>,
) {
    let Some(payload_type) = payload.get("type").and_then(|t| t.as_str()) else {
        return;
    };
    let source_event_id = Some(line_index.to_string());
    let tool_call_id = codex_tool_call_id(payload);

    if payload_type.ends_with("_output") {
        let mut event = events::tool_result_event(
            events::EventContext {
                event_seq: events_out.len() as u32,
                timestamp,
                source_path: Some(source_path.to_string()),
                source_event_id,
                message_seq,
                parser_version: EVENT_PARSER_VERSION,
            },
            payload.get("name").and_then(|name| name.as_str()).map(String::from),
            codex_output_summary(payload),
        );
        event.status = payload.get("status").and_then(|status| status.as_str()).map(String::from);
        event.tool_call_id = tool_call_id;
        events_out.push(event);
        return;
    }

    if payload_type.ends_with("_call") {
        let name = codex_call_name(payload_type, payload);
        let status = payload.get("status").and_then(|status| status.as_str()).map(String::from);
        if let Some(Value::String(text)) = codex_call_args(payload) {
            let patch_targets = events::patch_file_targets(text);
            if !patch_targets.is_empty() {
                for target in patch_targets {
                    let mut event = events::file_write_event(
                        events::EventContext {
                            event_seq: events_out.len() as u32,
                            timestamp,
                            source_path: Some(source_path.to_string()),
                            source_event_id: source_event_id.clone(),
                            message_seq,
                            parser_version: EVENT_PARSER_VERSION,
                        },
                        name.clone(),
                        target,
                    );
                    event.status = status.clone();
                    event.tool_call_id = tool_call_id.clone();
                    events_out.push(event);
                }
                return;
            }
        }
        let mut event = match codex_call_args(payload) {
            Some(Value::String(text)) => events::tool_call_event_from_text(
                events::EventContext {
                    event_seq: events_out.len() as u32,
                    timestamp,
                    source_path: Some(source_path.to_string()),
                    source_event_id: source_event_id.clone(),
                    message_seq,
                    parser_version: EVENT_PARSER_VERSION,
                },
                name,
                Some(text),
            ),
            args => events::tool_call_event(
                events::EventContext {
                    event_seq: events_out.len() as u32,
                    timestamp,
                    source_path: Some(source_path.to_string()),
                    source_event_id,
                    message_seq,
                    parser_version: EVENT_PARSER_VERSION,
                },
                name,
                args,
            ),
        };
        event.status = status;
        event.tool_call_id = tool_call_id;
        events_out.push(event);
    }
}

fn codex_tool_call_id(payload: &Value) -> Option<String> {
    payload
        .get("call_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(String::from)
}

fn codex_call_name(payload_type: &str, payload: &Value) -> String {
    payload.get("name").and_then(|name| name.as_str()).map(String::from).unwrap_or_else(|| {
        match payload_type {
            "function_call" | "custom_tool_call" => "tool".to_string(),
            _ => payload_type.strip_suffix("_call").unwrap_or(payload_type).to_string(),
        }
    })
}

fn codex_call_args(payload: &Value) -> Option<&Value> {
    payload
        .get("arguments")
        .or_else(|| payload.get("input"))
        .or_else(|| payload.get("action"))
        .or_else(|| payload.get("revised_prompt"))
}

fn codex_output_summary(payload: &Value) -> Option<String> {
    ["output", "result", "content", "tools"]
        .iter()
        .find_map(|key| payload.get(*key))
        .map(codex_value_summary)
}

fn codex_value_summary(value: &Value) -> String {
    value.as_str().map(String::from).unwrap_or_else(|| value.to_string())
}

fn collect_codex_content_events(
    content: Option<&Value>,
    timestamp: Option<i64>,
    source_path: &str,
    line_index: usize,
    prior_message_seq: Option<u32>,
    current_message_seq: Option<u32>,
    events_out: &mut Vec<RawSessionEvent>,
) {
    let Some(Value::Array(arr)) = content else {
        return;
    };
    let mut message_seq = prior_message_seq;
    for (item_index, item) in arr.iter().enumerate() {
        match item.get("type").and_then(|t| t.as_str()) {
            Some("text" | "output_text") => {
                if item.get("text").and_then(Value::as_str).is_some_and(|text| !text.is_empty()) {
                    message_seq = current_message_seq;
                }
            }
            Some("function_call") => {
                let name = item.get("name").and_then(|n| n.as_str()).unwrap_or("tool").to_string();
                let args = item.get("arguments").and_then(|a| a.as_str());
                let mut event = events::tool_call_event_from_text(
                    events::EventContext {
                        event_seq: events_out.len() as u32,
                        timestamp,
                        source_path: Some(source_path.to_string()),
                        source_event_id: Some(format!("{line_index}:{item_index}")),
                        message_seq,
                        parser_version: EVENT_PARSER_VERSION,
                    },
                    name,
                    args,
                );
                event.tool_call_id = codex_tool_call_id(item);
                events_out.push(event);
            }
            Some("function_call_output") => {
                let output = item.get("output").and_then(|o| o.as_str()).map(String::from);
                let mut event = events::tool_result_event(
                    events::EventContext {
                        event_seq: events_out.len() as u32,
                        timestamp,
                        source_path: Some(source_path.to_string()),
                        source_event_id: Some(format!("{line_index}:{item_index}")),
                        message_seq,
                        parser_version: EVENT_PARSER_VERSION,
                    },
                    None,
                    output,
                );
                event.tool_call_id = codex_tool_call_id(item);
                events_out.push(event);
            }
            _ => {}
        }
    }
}

fn codex_content_has_visible_text(content: Option<&Value>) -> bool {
    let Some(Value::Array(items)) = content else {
        return false;
    };
    items.iter().any(|item| {
        matches!(
            item.get("type").and_then(Value::as_str),
            Some("text" | "output_text" | "input_text")
        ) && item.get("text").and_then(Value::as_str).is_some_and(|text| !text.is_empty())
    })
}

fn collect_codex_meta_event(
    payload: &Value,
    timestamp: Option<i64>,
    source_path: &str,
    line_index: usize,
    events_out: &mut Vec<RawSessionEvent>,
) {
    let Some(actor) = payload.get("role").and_then(Value::as_str) else {
        return;
    };
    let summary = codex_visible_content(payload.get("content"));
    if summary.is_empty() {
        return;
    }
    events_out.push(RawSessionEvent {
        event_seq: events_out.len() as u32,
        timestamp,
        kind: "message".to_string(),
        actor: actor.to_string(),
        name: None,
        status: None,
        target: None,
        message_seq: None,
        summary: Some(events::bounded_summary(summary)),
        source_path: Some(source_path.to_string()),
        source_event_id: Some(line_index.to_string()),
        tool_call_id: None,
        is_meta: Some(true),
        visibility: None,
        attrs_json: None,
        parser_version: EVENT_PARSER_VERSION,
    });
}

fn codex_visible_content(content: Option<&Value>) -> String {
    let Some(Value::Array(items)) = content else {
        return String::new();
    };
    items
        .iter()
        .filter(|item| {
            matches!(
                item.get("type").and_then(Value::as_str),
                Some("text" | "output_text" | "input_text")
            )
        })
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn extract_content_array(content: Option<&Value>) -> String {
    match content {
        Some(Value::Array(arr)) => {
            let mut parts = Vec::new();
            for item in arr {
                match item.get("type").and_then(|t| t.as_str()) {
                    Some("text" | "output_text" | "input_text") => {
                        if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                            parts.push(text.to_string());
                        }
                    }
                    Some("function_call") => {
                        let name = item.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                        if let Some(args) = item.get("arguments").and_then(|a| a.as_str()) {
                            parts.push(format!("[{name}] {args}"));
                        }
                    }
                    Some("function_call_output") => {
                        if let Some(output) = item.get("output").and_then(|o| o.as_str()) {
                            parts.push(output.to_string());
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CodexUsageTotals {
    input: i64,
    output: i64,
    cached: i64,
    reasoning: i64,
}

impl CodexUsageTotals {
    fn from_usage(value: &Value) -> Option<Self> {
        let input = value.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0).max(0);
        let output = value.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0).max(0);
        let cached = value
            .get("cached_input_tokens")
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
            .max(value.get("cache_read_input_tokens").and_then(|v| v.as_i64()).unwrap_or(0))
            .max(0);
        let reasoning =
            value.get("reasoning_output_tokens").and_then(|v| v.as_i64()).unwrap_or(0).max(0);

        if input == 0 && output == 0 && cached == 0 && reasoning == 0 {
            return None;
        }

        Some(Self { input, output, cached, reasoning })
    }

    fn delta_from(self, previous: Self) -> Option<Self> {
        if self.input < previous.input
            || self.output < previous.output
            || self.cached < previous.cached
            || self.reasoning < previous.reasoning
        {
            return None;
        }

        Some(Self {
            input: self.input - previous.input,
            output: self.output - previous.output,
            cached: self.cached - previous.cached,
            reasoning: self.reasoning - previous.reasoning,
        })
    }

    fn saturating_add(self, other: Self) -> Self {
        Self {
            input: self.input.saturating_add(other.input),
            output: self.output.saturating_add(other.output),
            cached: self.cached.saturating_add(other.cached),
            reasoning: self.reasoning.saturating_add(other.reasoning),
        }
    }

    fn total(self) -> i64 {
        self.input
            .saturating_add(self.output)
            .saturating_add(self.cached)
            .saturating_add(self.reasoning)
    }

    fn looks_like_stale_regression(self, previous: Self, last: Self) -> bool {
        let previous_total = previous.total();
        let current_total = self.total();
        let last_total = last.total();

        if previous_total <= 0 || current_total <= 0 || last_total <= 0 {
            return false;
        }

        current_total.saturating_mul(100) >= previous_total.saturating_mul(98)
            || current_total.saturating_add(last_total.saturating_mul(2)) >= previous_total
    }
}

fn reported_total_tokens(usage: &Value) -> Option<i64> {
    usage.get("total_tokens").and_then(|v| v.as_i64()).filter(|total| *total >= 0)
}

fn remember_forked_child_inherited_baseline(
    previous_totals: &mut Option<CodexUsageTotals>,
    inherited_baseline: &mut Option<CodexUsageTotals>,
    inherited_reported_total: &mut Option<i64>,
    info: &Value,
) {
    let Some(total_usage) = info.get("total_token_usage") else {
        return;
    };
    let Some(total) = CodexUsageTotals::from_usage(total_usage) else {
        return;
    };

    *previous_totals = Some(total);
    *inherited_baseline = Some(total);
    *inherited_reported_total = reported_total_tokens(total_usage);
}

fn forked_child_matches_inherited_baseline(
    inherited_baseline: Option<CodexUsageTotals>,
    inherited_reported_total: Option<i64>,
    info: &Value,
    total_usage: Option<CodexUsageTotals>,
) -> bool {
    if let (Some(total_usage), Some(baseline)) =
        (info.get("total_token_usage"), inherited_reported_total)
        && reported_total_tokens(total_usage) == Some(baseline)
    {
        return true;
    }

    if let (Some(total), Some(baseline)) = (total_usage, inherited_baseline) {
        return total == baseline;
    }

    false
}

fn extract_codex_usage_event(
    payload: &Value,
    event_seq: u32,
    previous_totals: &mut Option<CodexUsageTotals>,
    timestamp: i64,
    provider: &str,
    current_model: Option<&str>,
    source_path: &str,
) -> Option<RawUsageEvent> {
    let info = payload.get("info")?;
    let total_usage = info.get("total_token_usage").and_then(CodexUsageTotals::from_usage);
    let last_usage = info.get("last_token_usage").and_then(CodexUsageTotals::from_usage);

    let (tokens, next_totals) = match (total_usage, last_usage, *previous_totals) {
        (Some(total), Some(last), Some(previous)) => {
            if total == previous {
                return None;
            }
            if total.delta_from(previous).is_none()
                && total.looks_like_stale_regression(previous, last)
            {
                return None;
            }
            (last, Some(total))
        }
        (Some(total), Some(last), None) => (last, Some(total)),
        (Some(total), None, Some(previous)) => {
            if total == previous {
                return None;
            }
            let delta = total.delta_from(previous)?;
            (delta, Some(total))
        }
        (Some(total), None, None) => (total, Some(total)),
        (None, Some(last), Some(previous)) => (last, Some(previous.saturating_add(last))),
        (None, Some(last), None) => (last, None),
        (None, None, _) => return None,
    };

    let cache_read_tokens = tokens.cached.min(tokens.input).max(0);
    let input_tokens = tokens.input.saturating_sub(cache_read_tokens).max(0);
    let reasoning_tokens = tokens.reasoning.max(0);
    let output_tokens = tokens.output.saturating_sub(reasoning_tokens).max(0);
    if input_tokens == 0 && output_tokens == 0 && cache_read_tokens == 0 && reasoning_tokens == 0 {
        return None;
    }

    *previous_totals = next_totals;

    let model = extract_codex_model(payload)
        .or_else(|| extract_codex_model(info))
        .or_else(|| current_model.map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string());

    Some(RawUsageEvent {
        model,
        provider: provider.to_string(),
        input_tokens,
        output_tokens,
        cache_read_tokens,
        reasoning_tokens,
        source_path: Some(source_path.to_string()),
        raw_usage_json: Some(info.to_string()),
        ..RawUsageEvent::derived(
            format!("token_count:{event_seq}"),
            event_seq,
            timestamp,
            USAGE_PARSER_VERSION,
        )
    })
}

fn extract_codex_model(value: &Value) -> Option<String> {
    value
        .get("model")
        .or_else(|| value.get("model_name"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .or_else(|| {
            value
                .get("model_info")
                .and_then(|info| info.get("slug"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
        })
}

fn apply_pending_codex_model(
    usage_events: &mut [RawUsageEvent],
    pending_indices: &mut Vec<usize>,
    model: &str,
) {
    for index in pending_indices.drain(..) {
        if let Some(event) = usage_events.get_mut(index)
            && event.model == "unknown"
        {
            event.model = model.to_string();
        }
    }
}

fn parse_timestamp(v: &Value) -> Option<i64> {
    rfc3339_ms(v.get("timestamp"))
}

const CODEX_INJECTED_CONTEXT_PREFIXES: &[&str] =
    &["# AGENTS.md instructions", "<environment_context>", "<user_instructions>", "<INSTRUCTIONS>"];

fn is_codex_injected_context(text: &str) -> bool {
    text.lines().any(|line| {
        let trimmed = line.trim_start();
        CODEX_INJECTED_CONTEXT_PREFIXES.iter().any(|prefix| trimmed.starts_with(prefix))
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CodexUserStream {
    EventMsg,
    ResponseItem,
}

#[derive(Default)]
struct CodexUserDedup {
    last_seq: Option<u32>,
    streams_seen: u8,
}

fn push_codex_user(
    messages: &mut Vec<RawMessage>,
    dedup: &mut CodexUserDedup,
    stream: CodexUserStream,
    content: String,
    timestamp: Option<i64>,
) -> Option<u32> {
    let stream_bit = match stream {
        CodexUserStream::EventMsg => 1,
        CodexUserStream::ResponseItem => 2,
    };
    if let Some(seq) = dedup.last_seq
        && seq as usize + 1 == messages.len()
        && messages
            .get(seq as usize)
            .is_some_and(|message| message.role == Role::User && message.content == content)
        && dedup.streams_seen & stream_bit == 0
    {
        dedup.streams_seen |= stream_bit;
        return Some(seq);
    }
    let seq = push_codex_message(messages, Role::User, content.clone(), timestamp);
    dedup.last_seq = seq;
    dedup.streams_seen = stream_bit;
    seq
}

fn push_codex_message(
    messages: &mut Vec<RawMessage>,
    role: Role,
    content: String,
    timestamp: Option<i64>,
) -> Option<u32> {
    if role == Role::Assistant
        && messages.last().is_some_and(|m| m.role == Role::Assistant && m.content == content)
    {
        return messages.len().checked_sub(1).map(|seq| seq as u32);
    }
    let seq = messages.len() as u32;
    messages.push(RawMessage { role, content, timestamp });
    Some(seq)
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

    #[test]
    fn codex_app_command_opens_thread_deeplink() {
        let command = CodexAdapter.app_command("019e6d8d-588b-7fd2-a326-c525469ed120").unwrap();

        assert!(
            command
                .args
                .iter()
                .any(|arg| arg == "codex://threads/019e6d8d-588b-7fd2-a326-c525469ed120")
        );
    }

    fn temp_codex_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "recall-cdx-test-{}-{}",
            label,
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_codex_rollout(sessions_dir: &Path, session_uuid: &str, text: &str) -> PathBuf {
        fs::create_dir_all(sessions_dir).unwrap();
        let filename = format!("rollout-2026-04-13T10-00-00-{session_uuid}.jsonl");
        let path = sessions_dir.join(filename);
        let meta = serde_json::json!({
            "type": "session_meta",
            "payload": {
                "id": session_uuid,
                "timestamp": "2026-04-13T10:00:00Z",
                "cwd": "/tmp/foo"
            }
        });
        let msg = serde_json::json!({
            "type": "event_msg",
            "timestamp": "2026-04-13T10:00:30Z",
            "payload": {
                "type": "user_message",
                "message": text
            }
        });
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "{meta}").unwrap();
        writeln!(f, "{msg}").unwrap();
        path
    }

    fn write_codex_event_only_rollout(sessions_dir: &Path, session_uuid: &str) -> PathBuf {
        fs::create_dir_all(sessions_dir).unwrap();
        let filename = format!("rollout-2026-04-13T10-00-00-{session_uuid}.jsonl");
        let path = sessions_dir.join(filename);
        let meta = serde_json::json!({
            "type": "session_meta",
            "timestamp": "2026-04-13T10:00:00Z",
            "payload": {
                "id": session_uuid,
                "timestamp": "2026-04-13T10:00:00Z",
                "cwd": "/tmp/foo"
            }
        });
        let web_search = serde_json::json!({
            "type": "response_item",
            "timestamp": "2026-04-13T10:00:01Z",
            "payload": {
                "type": "web_search_call",
                "status": "completed",
                "call_id": "web_123",
                "action": {
                    "type": "search",
                    "query": "rust sqlite json_extract"
                }
            }
        });
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "{meta}").unwrap();
        writeln!(f, "{web_search}").unwrap();
        path
    }

    #[test]
    fn parse_codex_session_extracts_structured_tool_events() {
        let root = temp_codex_root("events");
        let sessions_dir = root.join("sessions");
        fs::create_dir_all(&sessions_dir).unwrap();
        let uuid = "019a4c01-e8f4-7270-bdab-7f19273b2391";
        let path = sessions_dir.join(format!("rollout-2026-04-13T10-00-00-{uuid}.jsonl"));
        let meta = serde_json::json!({
            "type": "session_meta",
            "timestamp": "2026-04-13T10:00:00Z",
            "payload": {
                "id": uuid,
                "timestamp": "2026-04-13T10:00:00Z",
                "cwd": "/tmp/foo"
            }
        });
        let tool_call = serde_json::json!({
            "type": "response_item",
            "timestamp": "2026-04-13T10:00:01Z",
            "payload": {
                "type": "function_call",
                "name": "exec_command",
                "arguments": "{\"cmd\":\"sed -n '1,220p' CLAUDE.md\",\"workdir\":\"/tmp/foo\",\"yield_time_ms\":1000}",
                "call_id": "call_123"
            }
        });
        let tool_result = serde_json::json!({
            "type": "response_item",
            "timestamp": "2026-04-13T10:00:02Z",
            "payload": {
                "type": "function_call_output",
                "call_id": "call_123",
                "output": "file body"
            }
        });
        let custom_call = serde_json::json!({
            "type": "response_item",
            "timestamp": "2026-04-13T10:00:03Z",
            "payload": {
                "type": "custom_tool_call",
                "status": "completed",
                "call_id": "call_patch",
                "name": "apply_patch",
                "input": "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-a\n+b\n*** Add File: docs/new.md\n*** End Patch"
            }
        });
        let custom_result = serde_json::json!({
            "type": "response_item",
            "timestamp": "2026-04-13T10:00:04Z",
            "payload": {
                "type": "custom_tool_call_output",
                "call_id": "call_patch",
                "output": "{\"output\":\"Success\"}"
            }
        });
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "{meta}").unwrap();
        writeln!(f, "{tool_call}").unwrap();
        writeln!(f, "{tool_result}").unwrap();
        writeln!(f, "{custom_call}").unwrap();
        writeln!(f, "{custom_result}").unwrap();

        let raw = parse_codex_session(&path).unwrap().unwrap();

        assert_eq!(raw.events.len(), 5);
        assert_eq!(raw.events[0].kind, "command");
        assert_eq!(raw.events[0].name.as_deref(), Some("exec_command"));
        assert_eq!(raw.events[0].target.as_deref(), Some("sed -n '1,220p' CLAUDE.md"));
        assert_eq!(raw.events[0].source_event_id.as_deref(), Some("1"));
        assert_eq!(raw.events[0].tool_call_id.as_deref(), Some("call_123"));
        assert_eq!(raw.events[1].kind, "tool_result");
        assert_eq!(raw.events[1].source_event_id.as_deref(), Some("2"));
        assert_eq!(raw.events[1].tool_call_id.as_deref(), Some("call_123"));
        assert_eq!(raw.events[2].kind, "file_write");
        assert_eq!(raw.events[2].name.as_deref(), Some("apply_patch"));
        assert_eq!(raw.events[2].status.as_deref(), Some("completed"));
        assert_eq!(raw.events[2].target.as_deref(), Some("src/lib.rs"));
        assert_eq!(raw.events[2].source_event_id.as_deref(), Some("3"));
        assert_eq!(raw.events[2].tool_call_id.as_deref(), Some("call_patch"));
        assert_eq!(raw.events[3].kind, "file_write");
        assert_eq!(raw.events[3].name.as_deref(), Some("apply_patch"));
        assert_eq!(raw.events[3].target.as_deref(), Some("docs/new.md"));
        assert_eq!(raw.events[3].event_seq, 3);
        assert_eq!(raw.events[3].source_event_id.as_deref(), Some("3"));
        assert_eq!(raw.events[3].tool_call_id.as_deref(), Some("call_patch"));
        assert_eq!(raw.events[4].kind, "tool_result");
        assert_eq!(raw.events[4].source_event_id.as_deref(), Some("4"));
        assert_eq!(raw.events[4].tool_call_id.as_deref(), Some("call_patch"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_codex_session_extracts_array_shell_command_target() {
        let root = temp_codex_root("array-command-events");
        let sessions_dir = root.join("sessions");
        fs::create_dir_all(&sessions_dir).unwrap();
        let uuid = "019a4c01-e8f4-7270-bdab-7f19273b2392";
        let path = sessions_dir.join(format!("rollout-2026-04-13T10-00-00-{uuid}.jsonl"));
        let meta = serde_json::json!({
            "type": "session_meta",
            "timestamp": "2026-04-13T10:00:00Z",
            "payload": {
                "id": uuid,
                "timestamp": "2026-04-13T10:00:00Z",
                "cwd": "/tmp/foo"
            }
        });
        let tool_call = serde_json::json!({
            "type": "response_item",
            "timestamp": "2026-04-13T10:00:01Z",
            "payload": {
                "type": "function_call",
                "name": "shell",
                "arguments": "{\"command\":[\"bash\",\"-lc\",\"cd /tmp/foo && git status -sb\"]}",
                "call_id": "call_shell"
            }
        });
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "{meta}").unwrap();
        writeln!(f, "{tool_call}").unwrap();

        let raw = parse_codex_session(&path).unwrap().unwrap();

        assert_eq!(raw.events[0].kind, "command");
        assert_eq!(raw.events[0].target.as_deref(), Some("cd /tmp/foo && git status -sb"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_codex_session_does_not_reference_missing_message_for_empty_tool_content() {
        let root = temp_codex_root("empty-tool-content");
        let sessions_dir = root.join("sessions");
        fs::create_dir_all(&sessions_dir).unwrap();
        let uuid = "019a4c01-e8f4-7270-bdab-7f19273b2393";
        let path = sessions_dir.join(format!("rollout-2026-04-13T10-00-00-{uuid}.jsonl"));
        let meta = serde_json::json!({
            "type": "session_meta",
            "timestamp": "2026-04-13T10:00:00Z",
            "payload": {
                "id": uuid,
                "timestamp": "2026-04-13T10:00:00Z",
                "cwd": "/tmp/foo"
            }
        });
        let tool_call = serde_json::json!({
            "type": "response_item",
            "timestamp": "2026-04-13T10:00:01Z",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [
                    {
                        "type": "function_call",
                        "name": "shell",
                        "call_id": "call_empty"
                    }
                ]
            }
        });
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "{meta}").unwrap();
        writeln!(f, "{tool_call}").unwrap();

        let raw = parse_codex_session(&path).unwrap().unwrap();

        assert!(raw.messages.is_empty());
        assert_eq!(raw.events[0].kind, "command");
        assert_eq!(raw.events[0].message_seq, None);
        assert_eq!(raw.events[0].source_event_id.as_deref(), Some("1:0"));
        assert_eq!(raw.events[0].tool_call_id.as_deref(), Some("call_empty"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_codex_session_indexes_response_item_users_and_dedups_event_msg() {
        let root = temp_codex_root("response-user");
        let sessions_dir = root.join("sessions");
        fs::create_dir_all(&sessions_dir).unwrap();
        let uuid = "019a4c01-e8f4-7270-bdab-7f19273b2399";
        let path = sessions_dir.join(format!("rollout-2026-04-13T10-00-00-{uuid}.jsonl"));
        let lines = [
            serde_json::json!({
                "type": "session_meta",
                "timestamp": "2026-04-13T10:00:00Z",
                "payload": {"id": uuid, "cwd": "/tmp/foo"}
            }),
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-04-13T10:00:01Z",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "fix the parser"}]
                }
            }),
            serde_json::json!({
                "type": "event_msg",
                "timestamp": "2026-04-13T10:00:01Z",
                "payload": {"type": "user_message", "message": "fix the parser"}
            }),
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-04-13T10:00:02Z",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "ok"}]
                }
            }),
        ];
        let mut file = fs::File::create(&path).unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
        let raw = parse_codex_session(&path).unwrap().unwrap();
        assert_eq!(raw.messages.len(), 2);
        assert_eq!(raw.messages[0].role, Role::User);
        assert_eq!(raw.messages[0].content, "fix the parser");
        assert_eq!(raw.messages[1].content, "ok");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_codex_session_drops_injected_context_response_items() {
        let root = temp_codex_root("injected-context");
        let sessions_dir = root.join("sessions");
        fs::create_dir_all(&sessions_dir).unwrap();
        let uuid = "019a4c01-e8f4-7270-bdab-7f19273b2401";
        let path = sessions_dir.join(format!("rollout-2026-04-13T10-00-00-{uuid}.jsonl"));
        let injected = [
            "# AGENTS.md instructions for /tmp/foo\n\nalways run make check",
            "<environment_context>\n  <cwd>/tmp/foo</cwd>\n</environment_context>",
            "<user_instructions>\nbe terse\n</user_instructions>",
            "wrapper\n<environment_context>\n  <cwd>/tmp/foo</cwd>\n</environment_context>",
        ];
        let mut lines = vec![serde_json::json!({
            "type": "session_meta",
            "timestamp": "2026-04-13T10:00:00Z",
            "payload": {"id": uuid, "cwd": "/tmp/foo"}
        })];
        for text in injected {
            lines.push(serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-04-13T10:00:01Z",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": text}]
                }
            }));
        }
        lines.push(serde_json::json!({
            "type": "response_item",
            "timestamp": "2026-04-13T10:00:02Z",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "fix the parser"}]
            }
        }));
        let body = lines.iter().map(|line| line.to_string()).collect::<Vec<_>>().join("\n") + "\n";
        fs::write(&path, body).unwrap();

        let raw = parse_codex_session(&path).unwrap().unwrap();
        let users: Vec<&str> = raw
            .messages
            .iter()
            .filter(|message| message.role == Role::User)
            .map(|message| message.content.as_str())
            .collect();
        assert_eq!(users, vec!["fix the parser"]);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_codex_session_keeps_repeated_user_turns() {
        let root = temp_codex_root("repeat-user");
        let sessions_dir = root.join("sessions");
        fs::create_dir_all(&sessions_dir).unwrap();
        let uuid = "019a4c01-e8f4-7270-bdab-7f19273b2400";
        let path = sessions_dir.join(format!("rollout-2026-04-13T10-00-00-{uuid}.jsonl"));
        let mut lines = vec![serde_json::json!({
            "type": "session_meta",
            "timestamp": "2026-04-13T10:00:00Z",
            "payload": {"id": uuid, "cwd": "/tmp/foo"}
        })];
        for turn in 0..3 {
            lines.push(serde_json::json!({
                "type": "response_item",
                "timestamp": format!("2026-04-13T10:0{turn}:01Z"),
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "continue"}]
                }
            }));
            lines.push(serde_json::json!({
                "type": "event_msg",
                "timestamp": format!("2026-04-13T10:0{turn}:01Z"),
                "payload": {"type": "user_message", "message": "continue"}
            }));
            lines.push(serde_json::json!({
                "type": "response_item",
                "timestamp": format!("2026-04-13T10:0{turn}:02Z"),
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": format!("step {turn}")}]
                }
            }));
        }
        let mut file = fs::File::create(&path).unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
        let raw = parse_codex_session(&path).unwrap().unwrap();
        let users: Vec<&str> = raw
            .messages
            .iter()
            .filter(|message| message.role == Role::User)
            .map(|message| message.content.as_str())
            .collect();
        assert_eq!(users, vec!["continue", "continue", "continue"]);
        assert_eq!(raw.messages.len(), 6);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_codex_session_keeps_same_text_from_separate_stream_turns() {
        let root = temp_codex_root("separate-stream-turns");
        let sessions_dir = root.join("sessions");
        fs::create_dir_all(&sessions_dir).unwrap();
        let uuid = "019a4c01-e8f4-7270-bdab-7f19273b2402";
        let path = sessions_dir.join(format!("rollout-2026-04-13T10-00-00-{uuid}.jsonl"));
        let lines = [
            serde_json::json!({
                "type": "session_meta",
                "timestamp": "2026-04-13T10:00:00Z",
                "payload": {"id": uuid, "cwd": "/tmp/foo"}
            }),
            serde_json::json!({
                "type": "event_msg",
                "timestamp": "2026-04-13T10:00:01Z",
                "payload": {"type": "user_message", "message": "continue"}
            }),
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-04-13T10:00:02Z",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "first"}]
                }
            }),
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-04-13T10:01:01Z",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "continue"}]
                }
            }),
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-04-13T10:01:02Z",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "second"}]
                }
            }),
        ];
        let mut file = fs::File::create(&path).unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }

        let raw = parse_codex_session(&path).unwrap().unwrap();
        let users: Vec<&str> = raw
            .messages
            .iter()
            .filter(|message| message.role == Role::User)
            .map(|message| message.content.as_str())
            .collect();
        assert_eq!(users, vec!["continue", "continue"]);
        assert_eq!(raw.messages.len(), 4);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_codex_session_preserves_event_relationships_and_source_order_anchors() {
        let root = temp_codex_root("event-relationships");
        let sessions_dir = root.join("sessions");
        fs::create_dir_all(&sessions_dir).unwrap();
        let uuid = "019a4c01-e8f4-7270-bdab-7f19273b2395";
        let path = sessions_dir.join(format!("rollout-2026-04-13T10-00-00-{uuid}.jsonl"));
        let lines = [
            serde_json::json!({
                "type": "session_meta",
                "timestamp": "2026-04-13T10:00:00Z",
                "payload": {"id": uuid, "cwd": "/tmp/foo"}
            }),
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-04-13T10:00:01Z",
                "payload": {
                    "type": "function_call",
                    "name": "shell",
                    "arguments": "{\"command\":\"pwd\"}",
                    "call_id": "call_start"
                }
            }),
            serde_json::json!({
                "type": "event_msg",
                "timestamp": "2026-04-13T10:00:02Z",
                "payload": {"type": "user_message", "message": "Inspect the repository"}
            }),
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-04-13T10:00:03Z",
                "payload": {
                    "type": "function_call_output",
                    "call_id": "call_start",
                    "output": "/tmp/foo"
                }
            }),
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-04-13T10:00:04Z",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [
                        {
                            "type": "function_call",
                            "name": "read_file",
                            "arguments": "{\"path\":\"Cargo.toml\"}",
                            "call_id": "call_before"
                        },
                        {"type": "output_text", "text": "I found the manifest."},
                        {
                            "type": "function_call",
                            "name": "read_file",
                            "arguments": "{\"path\":\"src/lib.rs\"}",
                            "call_id": "call_after"
                        },
                        {
                            "type": "function_call_output",
                            "call_id": "call_after",
                            "output": "library body"
                        }
                    ]
                }
            }),
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-04-13T10:00:05Z",
                "payload": {
                    "type": "function_call",
                    "name": "shell",
                    "arguments": "{\"command\":\"git status\"}"
                }
            }),
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-04-13T10:00:06Z",
                "payload": {
                    "type": "function_call",
                    "name": "shell",
                    "arguments": "{\"command\":\"git diff\"}",
                    "call_id": "   "
                }
            }),
            serde_json::json!({
                "type": "response_item",
                "timestamp": "2026-04-13T10:00:07Z",
                "payload": {
                    "type": "message",
                    "role": "developer",
                    "content": [{"type": "input_text", "text": "汉".repeat(5000)}]
                }
            }),
        ];
        let mut file = fs::File::create(&path).unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }

        let raw = parse_codex_session(&path).unwrap().unwrap();

        assert_eq!(raw.messages.len(), 2);
        assert_eq!(raw.usage_events.len(), 0);
        assert_eq!(raw.events.len(), 8);
        assert_eq!(raw.events[0].message_seq, None);
        assert_eq!(raw.events[0].source_event_id.as_deref(), Some("1"));
        assert_eq!(raw.events[0].tool_call_id.as_deref(), Some("call_start"));
        assert_eq!(raw.events[1].message_seq, Some(0));
        assert_eq!(raw.events[1].source_event_id.as_deref(), Some("3"));
        assert_eq!(raw.events[1].tool_call_id.as_deref(), Some("call_start"));
        assert_eq!(raw.events[2].message_seq, Some(0));
        assert_eq!(raw.events[2].source_event_id.as_deref(), Some("4:0"));
        assert_eq!(raw.events[2].tool_call_id.as_deref(), Some("call_before"));
        assert_eq!(raw.events[3].message_seq, Some(1));
        assert_eq!(raw.events[3].tool_call_id.as_deref(), Some("call_after"));
        assert_eq!(raw.events[4].message_seq, Some(1));
        assert_eq!(raw.events[4].tool_call_id.as_deref(), Some("call_after"));
        assert_eq!(raw.events[5].source_event_id.as_deref(), Some("5"));
        assert_eq!(raw.events[5].tool_call_id, None);
        assert_eq!(raw.events[6].source_event_id.as_deref(), Some("6"));
        assert_eq!(raw.events[6].tool_call_id, None);
        assert_eq!(raw.events[7].kind, "message");
        assert_eq!(raw.events[7].actor, "developer");
        assert_eq!(raw.events[7].message_seq, None);
        assert_eq!(raw.events[7].is_meta, Some(true));
        assert_eq!(raw.events[7].visibility, None);
        let summary = raw.events[7].summary.as_deref().unwrap();
        assert!(summary.ends_with('…'));
        assert!(summary.len() <= 4099);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_codex_session_without_events_ignores_event_only_lines() {
        let root = temp_codex_root("no-events");
        let sessions_dir = root.join("sessions");
        let uuid = "019a4c01-e8f4-7270-bdab-7f19273b2394";
        let path = write_codex_event_only_rollout(&sessions_dir, uuid);

        assert!(parse_codex_session_with_options(&path, false).unwrap().is_none());

        let raw = parse_codex_session_with_options(&path, true).unwrap().unwrap();
        assert_eq!(raw.event_parser_version, Some(EVENT_PARSER_VERSION));
        assert!(raw.events.iter().any(|event| event.kind == "search"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_codex_session_extracts_non_function_response_item_events() {
        let root = temp_codex_root("nonfunction-events");
        let sessions_dir = root.join("sessions");
        fs::create_dir_all(&sessions_dir).unwrap();
        let uuid = "019a4c01-e8f4-7270-bdab-7f19273b2392";
        let path = sessions_dir.join(format!("rollout-2026-04-13T10-00-00-{uuid}.jsonl"));
        let meta = serde_json::json!({
            "type": "session_meta",
            "timestamp": "2026-04-13T10:00:00Z",
            "payload": {
                "id": uuid,
                "timestamp": "2026-04-13T10:00:00Z",
                "cwd": "/tmp/foo"
            }
        });
        let web_search = serde_json::json!({
            "type": "response_item",
            "timestamp": "2026-04-13T10:00:01Z",
            "payload": {
                "type": "web_search_call",
                "status": "completed",
                "call_id": "web_123",
                "action": {
                    "type": "search",
                    "query": "rust sqlite json_extract"
                }
            }
        });
        let tool_search = serde_json::json!({
            "type": "response_item",
            "timestamp": "2026-04-13T10:00:02Z",
            "payload": {
                "type": "tool_search_call",
                "status": "completed",
                "call_id": "tool_search_123",
                "arguments": {
                    "query": "serena initial_instructions",
                    "limit": 5
                }
            }
        });
        let tool_search_output = serde_json::json!({
            "type": "response_item",
            "timestamp": "2026-04-13T10:00:03Z",
            "payload": {
                "type": "tool_search_output",
                "call_id": "tool_search_123",
                "tools": [
                    {"name": "mcp__serena__initial_instructions"}
                ]
            }
        });
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "{meta}").unwrap();
        writeln!(f, "{web_search}").unwrap();
        writeln!(f, "{tool_search}").unwrap();
        writeln!(f, "{tool_search_output}").unwrap();

        let raw = parse_codex_session(&path).unwrap().unwrap();

        assert_eq!(raw.events.len(), 3);
        assert_eq!(raw.events[0].kind, "search");
        assert_eq!(raw.events[0].name.as_deref(), Some("web_search"));
        assert_eq!(raw.events[0].target.as_deref(), Some("rust sqlite json_extract"));
        assert_eq!(raw.events[0].status.as_deref(), Some("completed"));
        assert_eq!(raw.events[0].source_event_id.as_deref(), Some("1"));
        assert_eq!(raw.events[0].tool_call_id.as_deref(), Some("web_123"));
        assert_eq!(raw.events[1].kind, "search");
        assert_eq!(raw.events[1].name.as_deref(), Some("tool_search"));
        assert_eq!(raw.events[1].target.as_deref(), Some("serena initial_instructions"));
        assert_eq!(raw.events[1].source_event_id.as_deref(), Some("2"));
        assert_eq!(raw.events[1].tool_call_id.as_deref(), Some("tool_search_123"));
        assert_eq!(raw.events[2].kind, "tool_result");
        assert_eq!(raw.events[2].source_event_id.as_deref(), Some("3"));
        assert_eq!(raw.events[2].tool_call_id.as_deref(), Some("tool_search_123"));
        assert!(raw.events[2].summary.as_deref().unwrap_or("").contains("mcp__serena"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_codex_session_ignores_metadata_only_rollout() {
        let root = temp_codex_root("metadata-only");
        let sessions_dir = root.join("sessions");
        fs::create_dir_all(&sessions_dir).unwrap();
        let uuid = "019a4c01-e8f4-7270-bdab-7f19273b2393";
        let path = sessions_dir.join(format!("rollout-2026-04-13T10-00-00-{uuid}.jsonl"));
        let meta = serde_json::json!({
            "type": "session_meta",
            "timestamp": "2026-04-13T10:00:00Z",
            "payload": {
                "id": uuid,
                "timestamp": "2026-04-13T10:00:00Z",
                "cwd": "/tmp/foo",
                "model": "gpt-5.5"
            }
        });
        let turn = serde_json::json!({
            "type": "turn_context",
            "timestamp": "2026-04-13T10:00:01Z",
            "payload": {
                "cwd": "/tmp/foo",
                "model": "gpt-5.5"
            }
        });
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "{meta}").unwrap();
        writeln!(f, "{turn}").unwrap();

        assert!(parse_codex_session(&path).unwrap().is_none());

        let _ = fs::remove_dir_all(&root);
    }

    fn write_codex_usage_rollout(sessions_dir: &Path, session_uuid: &str) -> PathBuf {
        fs::create_dir_all(sessions_dir).unwrap();
        let filename = format!("rollout-2026-04-13T10-00-00-{session_uuid}.jsonl");
        let path = sessions_dir.join(filename);
        let meta = serde_json::json!({
            "type": "session_meta",
            "payload": {
                "id": session_uuid,
                "timestamp": "2026-04-13T10:00:00Z",
                "cwd": "/tmp/foo",
                "model_provider": "openai"
            }
        });
        let usage1 = serde_json::json!({
            "type": "event_msg",
            "timestamp": "2026-04-13T10:00:01Z",
            "payload": {
                "type": "token_count",
                "info": {
                    "total_token_usage": {
                        "input_tokens": 10,
                        "cached_input_tokens": 2,
                        "output_tokens": 3,
                        "reasoning_output_tokens": 1
                    },
                    "last_token_usage": {
                        "input_tokens": 10,
                        "cached_input_tokens": 2,
                        "output_tokens": 3,
                        "reasoning_output_tokens": 1
                    }
                }
            }
        });
        let turn_context = serde_json::json!({
            "type": "turn_context",
            "payload": {"model": "gpt-5-codex"}
        });
        let usage2 = serde_json::json!({
            "type": "event_msg",
            "timestamp": "2026-04-13T10:00:02Z",
            "payload": {
                "type": "token_count",
                "info": {
                    "total_token_usage": {
                        "input_tokens": 15,
                        "cached_input_tokens": 3,
                        "output_tokens": 5,
                        "reasoning_output_tokens": 1
                    },
                    "last_token_usage": {
                        "input_tokens": 5,
                        "cached_input_tokens": 1,
                        "output_tokens": 2,
                        "reasoning_output_tokens": 0
                    }
                }
            }
        });
        let msg = serde_json::json!({
            "type": "event_msg",
            "timestamp": "2026-04-13T10:00:30Z",
            "payload": {
                "type": "user_message",
                "message": "hello"
            }
        });
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "{meta}").unwrap();
        writeln!(f, "{usage1}").unwrap();
        writeln!(f, "{turn_context}").unwrap();
        writeln!(f, "{usage2}").unwrap();
        writeln!(f, "{msg}").unwrap();
        path
    }

    fn write_codex_usage_only_rollout(sessions_dir: &Path, session_uuid: &str) -> PathBuf {
        fs::create_dir_all(sessions_dir).unwrap();
        let filename = format!("rollout-2026-04-13T10-00-00-{session_uuid}.jsonl");
        let path = sessions_dir.join(filename);
        let meta = serde_json::json!({
            "type": "session_meta",
            "payload": {
                "id": session_uuid,
                "timestamp": "2026-04-13T10:00:00Z",
                "cwd": "/tmp/foo",
                "model_provider": "openai",
                "model": "gpt-5.5"
            }
        });
        let usage = serde_json::json!({
            "type": "event_msg",
            "timestamp": "2026-04-13T10:00:01Z",
            "payload": {
                "type": "token_count",
                "info": {
                    "last_token_usage": {
                        "input_tokens": 10,
                        "cached_input_tokens": 2,
                        "output_tokens": 3,
                        "reasoning_output_tokens": 1
                    }
                }
            }
        });
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "{meta}").unwrap();
        writeln!(f, "{usage}").unwrap();
        path
    }

    fn make_existing_session(source_id: &str, updated_at: i64, message_count: u32) -> Session {
        Session {
            id: format!("internal-{source_id}"),
            source: "codex".to_string(),
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
    fn extract_session_id_from_filename_happy_path() {
        let stem = "rollout-2025-11-04T07-16-24-019a4c01-e8f4-7270-bdab-7f19273b237e";
        assert_eq!(
            extract_session_id_from_filename(stem),
            Some("019a4c01-e8f4-7270-bdab-7f19273b237e".to_string())
        );
    }

    #[test]
    fn extract_session_id_from_filename_rejects_non_uuid_tail() {
        assert_eq!(extract_session_id_from_filename("short"), None);
        assert_eq!(extract_session_id_from_filename("rollout-no-uuid-at-end"), None);
        let non_hex_tail = "rollout-xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx";
        assert_eq!(extract_session_id_from_filename(non_hex_tail), None);
        let no_separator = "rolloutX019a4c01-e8f4-7270-bdab-7f19273b237e";
        assert_eq!(extract_session_id_from_filename(no_separator), None);
        let bad_dash_layout = "rollout-019a4c01Xe8f4-7270-bdab-7f19273b237e";
        assert_eq!(extract_session_id_from_filename(bad_dash_layout), None);
    }

    #[test]
    fn parse_codex_session_extracts_derived_usage_events() {
        let root = temp_codex_root("usage");
        let sessions_dir = root.join("sessions");
        let uuid = "019a4c01-e8f4-7270-bdab-7f19273b237e";
        let path = write_codex_usage_rollout(&sessions_dir, uuid);

        let raw = parse_codex_session(&path).unwrap().unwrap();

        assert_eq!(raw.usage_events.len(), 2);
        assert_eq!(raw.usage_events[0].model, "gpt-5-codex");
        assert_eq!(raw.usage_events[0].provider, "openai");
        assert_eq!(raw.usage_events[0].input_tokens, 8);
        assert_eq!(raw.usage_events[0].cache_read_tokens, 2);
        assert_eq!(raw.usage_events[0].output_tokens, 2);
        assert_eq!(raw.usage_events[0].reasoning_tokens, 1);
        assert_eq!(raw.usage_events[0].token_source, crate::types::TokenSource::Derived);

        assert_eq!(raw.usage_events[1].input_tokens, 4);
        assert_eq!(raw.usage_events[1].cache_read_tokens, 1);
        assert_eq!(raw.usage_events[1].output_tokens, 2);
        assert_eq!(raw.usage_events[1].reasoning_tokens, 0);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_codex_session_keeps_usage_without_searchable_messages() {
        let root = temp_codex_root("usage-only");
        let sessions_dir = root.join("sessions");
        let uuid = "019a4c01-e8f4-7270-bdab-7f19273b2380";
        let path = write_codex_usage_only_rollout(&sessions_dir, uuid);

        let raw = parse_codex_session(&path).unwrap().unwrap();

        assert!(raw.messages.is_empty());
        assert_eq!(raw.started_at, 1_776_074_400_000);
        assert_eq!(raw.usage_events.len(), 1);
        assert_eq!(raw.usage_events[0].model, "gpt-5.5");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_codex_session_skips_forked_child_inherited_usage_prefix() {
        let root = temp_codex_root("forked-child");
        let sessions_dir = root.join("sessions");
        fs::create_dir_all(&sessions_dir).unwrap();
        let uuid = "019a4c01-e8f4-7270-bdab-7f19273b2381";
        let path = sessions_dir.join(format!("rollout-2026-05-05T21-51-57-{uuid}.jsonl"));
        let mut f = fs::File::create(&path).unwrap();
        for line in [
            r#"{"timestamp":"2026-05-05T21:51:57.991Z","type":"session_meta","payload":{"id":"child-session","forked_from_id":"parent-session","source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent-session","depth":1}}},"model_provider":"openai","agent_nickname":"worker","cwd":"/repo-child"}}"#,
            r#"{"timestamp":"2026-05-05T21:51:57.992Z","type":"session_meta","payload":{"id":"parent-session","source":"interactive","model_provider":"azure","agent_nickname":"parent","cwd":"/repo-parent"}}"#,
            r#"{"timestamp":"2026-05-05T21:51:57.994Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":116000,"cached_input_tokens":114000,"output_tokens":1000,"total_tokens":117000},"last_token_usage":{"input_tokens":73000,"cached_input_tokens":72000,"output_tokens":500,"total_tokens":73500}}}}"#,
            r#"{"timestamp":"2026-05-05T21:51:58.947Z","type":"turn_context","payload":{"model":"gpt-5.5","cwd":"/repo-child"}}"#,
            r#"{"timestamp":"2026-05-05T21:51:58.948Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":116000,"cached_input_tokens":114000,"output_tokens":1000,"total_tokens":117000},"last_token_usage":{"input_tokens":73000,"cached_input_tokens":72000,"output_tokens":500,"total_tokens":73500}}}}"#,
            r#"{"timestamp":"2026-05-05T21:51:59.253Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":117500,"cached_input_tokens":115000,"output_tokens":1200,"reasoning_output_tokens":50,"total_tokens":118700},"last_token_usage":{"input_tokens":1500,"cached_input_tokens":1000,"output_tokens":200,"reasoning_output_tokens":50,"total_tokens":1700}}}}"#,
        ] {
            writeln!(f, "{line}").unwrap();
        }

        let raw = parse_codex_session(&path).unwrap().unwrap();

        assert_eq!(raw.directory.as_deref(), Some("/repo-child"));
        assert_eq!(raw.usage_events.len(), 1);
        assert_eq!(raw.usage_events[0].model, "gpt-5.5");
        assert_eq!(raw.usage_events[0].provider, "openai");
        assert_eq!(raw.usage_events[0].input_tokens, 500);
        assert_eq!(raw.usage_events[0].cache_read_tokens, 1000);
        assert_eq!(raw.usage_events[0].output_tokens, 150);
        assert_eq!(raw.usage_events[0].reasoning_tokens, 50);

        assert_eq!(raw.thread_role, Some(ThreadRole::Subagent));
        assert_eq!(
            raw.parent_links,
            vec![
                ParentLink {
                    relation: ParentRelation::Spawn,
                    source: "codex".to_string(),
                    source_id: "parent-session".to_string(),
                },
                ParentLink {
                    relation: ParentRelation::Fork,
                    source: "codex".to_string(),
                    source_id: "parent-session".to_string(),
                },
            ]
        );
        assert_eq!(raw.metadata_parser_version, Some(METADATA_PARSER_VERSION));

        let _ = fs::remove_dir_all(&root);
    }

    fn write_codex_topology_rollout(sessions_dir: &Path, uuid: &str, payload: Value) -> PathBuf {
        fs::create_dir_all(sessions_dir).unwrap();
        let path = sessions_dir.join(format!("rollout-2026-04-13T10-00-00-{uuid}.jsonl"));
        let meta = serde_json::json!({ "type": "session_meta", "payload": payload });
        // A turn_context clears the forked-child inheritance guard so the message
        // that follows is retained, matching real forked rollouts.
        let turn_context = serde_json::json!({
            "type": "turn_context",
            "timestamp": "2026-04-13T10:00:29Z",
            "payload": { "model": "gpt-5.5", "cwd": "/repo" }
        });
        let msg = serde_json::json!({
            "type": "event_msg",
            "timestamp": "2026-04-13T10:00:30Z",
            "payload": { "type": "user_message", "message": "hi" }
        });
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "{meta}").unwrap();
        writeln!(f, "{turn_context}").unwrap();
        writeln!(f, "{msg}").unwrap();
        path
    }

    #[test]
    fn parse_codex_session_classifies_user_source_as_primary_without_parents() {
        let root = temp_codex_root("primary-topology");
        let uuid = "019a4c01-e8f4-7270-bdab-7f19273b2401";
        let path = write_codex_topology_rollout(
            &root.join("sessions"),
            uuid,
            serde_json::json!({
                "id": uuid,
                "thread_source": "user",
                "source": "interactive",
                "cwd": "/repo"
            }),
        );

        let raw = parse_codex_session(&path).unwrap().unwrap();

        assert_eq!(raw.thread_role, Some(ThreadRole::Primary));
        assert!(raw.parent_links.is_empty());
        assert_eq!(raw.metadata_parser_version, Some(METADATA_PARSER_VERSION));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_codex_session_keeps_user_fork_primary_with_fork_link() {
        let root = temp_codex_root("fork-primary");
        let uuid = "019a4c01-e8f4-7270-bdab-7f19273b2402";
        let path = write_codex_topology_rollout(
            &root.join("sessions"),
            uuid,
            serde_json::json!({
                "id": uuid,
                "thread_source": "user",
                "forked_from_id": "parent-abc",
                "cwd": "/repo"
            }),
        );

        let raw = parse_codex_session(&path).unwrap().unwrap();

        assert_eq!(raw.thread_role, Some(ThreadRole::Primary));
        assert_eq!(
            raw.parent_links,
            vec![ParentLink {
                relation: ParentRelation::Fork,
                source: "codex".to_string(),
                source_id: "parent-abc".to_string(),
            }]
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_codex_session_keeps_legacy_subagent_without_parent() {
        let root = temp_codex_root("legacy-subagent");
        let uuid = "019a4c01-e8f4-7270-bdab-7f19273b2403";
        let path = write_codex_topology_rollout(
            &root.join("sessions"),
            uuid,
            serde_json::json!({ "id": uuid, "thread_source": "subagent", "cwd": "/repo" }),
        );

        let raw = parse_codex_session(&path).unwrap().unwrap();

        assert_eq!(raw.thread_role, Some(ThreadRole::Subagent));
        assert!(raw.parent_links.is_empty());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_codex_session_leaves_role_unknown_without_markers() {
        let root = temp_codex_root("unknown-topology");
        let uuid = "019a4c01-e8f4-7270-bdab-7f19273b2404";
        let path = write_codex_topology_rollout(
            &root.join("sessions"),
            uuid,
            serde_json::json!({ "id": uuid, "cwd": "/repo" }),
        );

        let raw = parse_codex_session(&path).unwrap().unwrap();

        assert_eq!(raw.thread_role, None);
        assert!(raw.parent_links.is_empty());
        // Even unknown-role sessions get a metadata parser version so topology
        // backfill runs for them once a classifier improves.
        assert_eq!(raw.metadata_parser_version, Some(METADATA_PARSER_VERSION));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_codex_session_keeps_model_less_usage_unknown_after_plain_event() {
        let root = temp_codex_root("unknown-model");
        let sessions_dir = root.join("sessions");
        fs::create_dir_all(&sessions_dir).unwrap();
        let uuid = "019a4c01-e8f4-7270-bdab-7f19273b2382";
        let path = sessions_dir.join(format!("rollout-2026-04-13T10-00-00-{uuid}.jsonl"));
        let mut f = fs::File::create(&path).unwrap();
        for line in [
            r#"{"timestamp":"2026-04-13T10:00:01Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3,"reasoning_output_tokens":1},"last_token_usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":3,"reasoning_output_tokens":1}}}}"#,
            r#"{"timestamp":"2026-04-13T10:00:02Z","type":"event_msg","payload":{"type":"user_message","message":"plain event fixes prior model as unknown"}}"#,
            r#"{"timestamp":"2026-04-13T10:00:03Z","type":"turn_context","payload":{"model":"gpt-5-codex"}}"#,
            r#"{"timestamp":"2026-04-13T10:00:04Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":15,"cached_input_tokens":3,"output_tokens":5,"reasoning_output_tokens":1},"last_token_usage":{"input_tokens":5,"cached_input_tokens":1,"output_tokens":2,"reasoning_output_tokens":0}}}}"#,
        ] {
            writeln!(f, "{line}").unwrap();
        }

        let raw = parse_codex_session(&path).unwrap().unwrap();

        assert_eq!(raw.usage_events.len(), 2);
        assert_eq!(raw.usage_events[0].model, "unknown");
        assert_eq!(raw.usage_events[1].model, "gpt-5-codex");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_for_sync_skips_unchanged_session() {
        let root = temp_codex_root("skip");
        let sessions_dir = root.join("sessions");
        let uuid = "019a4c01-e8f4-7270-bdab-7f19273b237e";
        let path = write_codex_rollout(&sessions_dir, uuid, "hello");
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();

        let store = setup_store();
        store.insert_session(&make_existing_session(uuid, mtime, 1)).unwrap();
        store
            .persist_usage_events_for_existing_session(
                "codex",
                uuid,
                &[],
                USAGE_PARSER_VERSION,
                Some(mtime),
            )
            .unwrap();
        store
            .persist_session_events_for_existing_session(
                "codex",
                uuid,
                &[],
                EVENT_PARSER_VERSION,
                Some(mtime),
            )
            .unwrap();
        store
            .persist_topology_for_existing_session(
                "codex",
                uuid,
                &crate::db::store::SessionTopologyWrite {
                    thread_role: None,
                    parents: &[],
                    parser_version: Some(METADATA_PARSER_VERSION),
                },
            )
            .unwrap();

        let result = scan_for_sync_impl(
            &root,
            &AdapterSyncContext::from_store_for_test(&store, "codex").unwrap(),
            None,
            true,
        )
        .unwrap();
        assert_eq!(result.sessions.len(), 0);
        assert_eq!(result.stats.skipped_sessions, 1);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_for_sync_reparses_unchanged_session_with_stale_event_parser() {
        let root = temp_codex_root("event-parser-backfill");
        let sessions_dir = root.join("sessions");
        let uuid = "019a4c01-e8f4-7270-bdab-7f19273b237d";
        let path = write_codex_rollout(&sessions_dir, uuid, "hello");
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();

        let store = setup_store();
        store.insert_session(&make_existing_session(uuid, mtime, 1)).unwrap();
        store
            .persist_usage_events_for_existing_session(
                "codex",
                uuid,
                &[],
                USAGE_PARSER_VERSION,
                Some(mtime),
            )
            .unwrap();
        store
            .persist_session_events_for_existing_session(
                "codex",
                uuid,
                &[],
                EVENT_PARSER_VERSION - 1,
                Some(mtime),
            )
            .unwrap();
        store
            .persist_topology_for_existing_session(
                "codex",
                uuid,
                &crate::db::store::SessionTopologyWrite {
                    thread_role: None,
                    parents: &[],
                    parser_version: Some(METADATA_PARSER_VERSION),
                },
            )
            .unwrap();

        let result = scan_for_sync_impl(
            &root,
            &AdapterSyncContext::from_store_for_test(&store, "codex").unwrap(),
            None,
            true,
        )
        .unwrap();

        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.stats.skipped_sessions, 0);
        assert_eq!(result.sessions[0].event_parser_version, Some(EVENT_PARSER_VERSION));
        assert_eq!(result.sessions[0].messages.len(), 1);
        assert_eq!(result.sessions[0].messages[0].content, "hello");
        assert!(result.sessions[0].usage_events.is_empty());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn usage_only_scan_skips_unchanged_session_without_event_state() {
        let root = temp_codex_root("usage-only-skip");
        let sessions_dir = root.join("sessions");
        let uuid = "019a4c01-e8f4-7270-bdab-7f19273b237e";
        let path = write_codex_rollout(&sessions_dir, uuid, "hello");
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();

        let store = setup_store();
        store.insert_session(&make_existing_session(uuid, mtime, 1)).unwrap();
        store
            .persist_usage_events_for_existing_session(
                "codex",
                uuid,
                &[],
                USAGE_PARSER_VERSION,
                Some(mtime),
            )
            .unwrap();

        let result = scan_for_sync_impl(
            &root,
            &AdapterSyncContext::from_store_for_test(&store, "codex").unwrap(),
            None,
            false,
        )
        .unwrap();
        assert_eq!(result.sessions.len(), 0);
        assert_eq!(result.stats.skipped_sessions, 1);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn usage_only_scan_skips_event_only_session() {
        let root = temp_codex_root("usage-only-event-only");
        let sessions_dir = root.join("sessions");
        let uuid = "019a4c01-e8f4-7270-bdab-7f19273b2381";
        write_codex_event_only_rollout(&sessions_dir, uuid);
        let store = setup_store();

        let usage_result = scan_for_sync_impl(
            &root,
            &AdapterSyncContext::from_store_for_test(&store, "codex").unwrap(),
            None,
            false,
        )
        .unwrap();
        assert!(usage_result.sessions.is_empty());

        let full_result = scan_for_sync_impl(
            &root,
            &AdapterSyncContext::from_store_for_test(&store, "codex").unwrap(),
            None,
            true,
        )
        .unwrap();
        assert_eq!(full_result.sessions.len(), 1);
        assert!(full_result.sessions[0].messages.is_empty());
        assert!(full_result.sessions[0].usage_events.is_empty());
        assert!(full_result.sessions[0].events.iter().any(|event| event.kind == "search"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_for_sync_reparses_when_mtime_changes() {
        let root = temp_codex_root("mismatch");
        let sessions_dir = root.join("sessions");
        let uuid = "019a4c01-e8f4-7270-bdab-7f19273b237e";
        let path = write_codex_rollout(&sessions_dir, uuid, "hi");
        let actual_mtime = file_scan::stat_mtime_ms(&path).unwrap();

        let store = setup_store();
        store.insert_session(&make_existing_session(uuid, actual_mtime - 1_000, 1)).unwrap();

        let result = scan_for_sync_impl(
            &root,
            &AdapterSyncContext::from_store_for_test(&store, "codex").unwrap(),
            None,
            true,
        )
        .unwrap();
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].source_id, uuid);
        assert_eq!(result.sessions[0].updated_at, Some(actual_mtime));
        assert_eq!(result.stats.skipped_sessions, 0);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_for_sync_keeps_going_when_one_file_is_unreadable() {
        use std::io::Write as _;

        let root = temp_codex_root("unreadable");
        let sessions_dir = root.join("sessions");
        fs::create_dir_all(&sessions_dir).unwrap();

        let good_uuid = "019a4c01-e8f4-7270-bdab-7f19273b237e";
        write_codex_rollout(&sessions_dir, good_uuid, "still here");

        let bad_uuid = "019a4c02-e8f4-7270-bdab-7f19273b237e";
        let bad_path = sessions_dir.join(format!("rollout-2026-04-13T10-00-00-{bad_uuid}.jsonl"));
        let mut f = fs::File::create(&bad_path).unwrap();
        f.write_all(&[0xFF, 0xFE, 0xFD, 0xFC]).unwrap();

        let store = setup_store();

        let result = scan_for_sync_impl(
            &root,
            &AdapterSyncContext::from_store_for_test(&store, "codex").unwrap(),
            None,
            true,
        )
        .unwrap();
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].source_id, good_uuid);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_for_sync_picks_up_new_session() {
        let root = temp_codex_root("new");
        let sessions_dir = root.join("archived_sessions");
        let uuid = "019a4c01-e8f4-7270-bdab-7f19273b237e";
        let path = write_codex_rollout(&sessions_dir, uuid, "fresh");

        let store = setup_store();

        let result = scan_for_sync_impl(
            &root,
            &AdapterSyncContext::from_store_for_test(&store, "codex").unwrap(),
            None,
            true,
        )
        .unwrap();
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].source_id, uuid);
        assert_eq!(result.sessions[0].source_file_path.as_deref(), path.to_str());
        assert_eq!(result.stats.skipped_sessions, 0);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn invocation_probe_accepts_only_discovery_tool_input_and_deduplicates_a_session() {
        let root = temp_codex_root("invocation-probe");
        let sessions_dir = root.join("sessions");
        let uuid = "019c9c4f-a462-7cc1-99a5-4ab521648c91";
        let path = write_codex_rollout(&sessions_dir, uuid, "normal nonce-codex reference");
        let call = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "name": "mcp__recall__search_sessions",
                "arguments": serde_json::json!({
                    "query": "history",
                    "invocation_nonce": "nonce-codex"
                }).to_string()
            }
        });
        let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
        writeln!(file, "{call}").unwrap();
        writeln!(file, "{call}").unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type": "event_msg",
                "payload": {
                    "type": "item_completed",
                    "item": {
                        "type": "McpToolCall",
                        "server": "recall",
                        "tool": "search_sessions",
                        "arguments": {
                            "query": "history",
                            "invocation_nonce": "nonce-codex"
                        }
                    }
                }
            })
        )
        .unwrap();

        let result =
            probe_invocation_nonce_in(&root, "nonce-codex", InvocationProbeBudget::default());
        assert!(result.complete);
        assert_eq!(result.source_ids, vec![uuid.to_string()]);

        let normal = probe_invocation_nonce_in(
            &root,
            "normal nonce-codex reference",
            InvocationProbeBudget::default(),
        );
        assert!(normal.complete);
        assert!(normal.source_ids.is_empty());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn invocation_probe_rejects_cross_session_nonce_reuse() {
        let root = temp_codex_root("invocation-probe-multiple");
        let sessions_dir = root.join("sessions");
        for uuid in ["019c9c4f-a462-7cc1-99a5-4ab521648c91", "019c9c4f-a462-7cc1-99a5-4ab521648c92"]
        {
            let path = write_codex_rollout(&sessions_dir, uuid, "ordinary");
            let call = serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "custom_tool_call",
                    "name": "mcp__recall__list_recent_sessions",
                    "input": {"invocation_nonce": "nonce-shared"}
                }
            });
            let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
            writeln!(file, "{call}").unwrap();
        }

        let result =
            probe_invocation_nonce_in(&root, "nonce-shared", InvocationProbeBudget::default());
        assert!(result.complete);
        assert_eq!(result.source_ids.len(), 2);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn invocation_probe_accepts_codex_tool_wrapper_before_completion_event() {
        let root = temp_codex_root("invocation-probe-wrapper");
        let sessions_dir = root.join("sessions");
        let uuid = "019c9c4f-a462-7cc1-99a5-4ab521648c91";
        let path = write_codex_rollout(&sessions_dir, uuid, "ordinary");
        let call = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "custom_tool_call",
                "name": "exec",
                "input": "const r = await tools.mcp__recall__search_sessions({\n  query: \"history\",\n  invocation_nonce: \"nonce-wrapper\"\n});\ntext(r);"
            }
        });
        let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
        writeln!(file, "{call}").unwrap();

        let result =
            probe_invocation_nonce_in(&root, "nonce-wrapper", InvocationProbeBudget::default());
        assert!(result.complete);
        assert_eq!(result.source_ids, vec![uuid.to_string()]);
        assert!(!codex_tool_wrapper_matches(
            "const r = await tools.exec_command({cmd: `tools.mcp__recall__search_sessions({invocation_nonce: \\\"nonce-wrapper\\\"})`});",
            "nonce-wrapper"
        ));
        assert!(!codex_tool_wrapper_matches(
            "const r = await tools.mcp__recall__search_sessions({query: \"history\"}); text(\"nonce-wrapper\");",
            "nonce-wrapper"
        ));

        let _ = fs::remove_dir_all(&root);
    }
}
