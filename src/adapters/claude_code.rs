use std::collections::{HashMap, HashSet};
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
    RawMessage, RawSession, ResumeCommand, SourceAdapter, SyncScanResult, first_timestamp,
};
use crate::types::{
    EvidenceVisibility, FileEvidence, FileEvidenceKind, FileOperation, ParentLink, ParentRelation,
    RawSessionEvent, RawUsageEvent, Role, ThreadRole,
};

pub(crate) struct ClaudeCodeAdapter;

const USAGE_PARSER_VERSION: u32 = 6;
const EVENT_PARSER_VERSION: u32 = 5;
const METADATA_PARSER_VERSION: u32 = 3;

impl SourceAdapter for ClaudeCodeAdapter {
    fn id(&self) -> &str {
        "claude-code"
    }
    fn label(&self) -> &str {
        "CC"
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        Some(ResumeCommand {
            program: "claude".to_string(),
            args: vec!["--resume".to_string(), source_id.to_string()],
        })
    }

    fn start_command(&self, prompt: String) -> Option<ResumeCommand> {
        Some(crate::adapters::prompt_start("claude", prompt))
    }

    fn usage_parser_version(&self) -> Option<u32> {
        Some(USAGE_PARSER_VERSION)
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        scan_claude_dirs(&resolve_claude_dirs()?)
    }

    fn scan_for_sync(
        &self,
        context: &AdapterSyncContext,
        since_ts: Option<i64>,
        include_events: bool,
    ) -> anyhow::Result<Option<SyncScanResult>> {
        Ok(Some(scan_claude_dirs_for_sync(
            &resolve_claude_dirs()?,
            context,
            since_ts,
            include_events,
        )?))
    }
}

fn scan_claude_dirs(claude_dirs: &[PathBuf]) -> anyhow::Result<Vec<RawSession>> {
    let mut sessions = Vec::new();
    let mut claimed = HashSet::new();
    for claude_dir in claude_dirs {
        let mut indexes = load_session_indexes(claude_dir);
        let mut entries = collect_project_entries(claude_dir, &mut indexes);
        entries.extend(collect_transcript_entries(claude_dir));
        claim_session_entries(&mut entries, &mut claimed);
        for entry in entries {
            let Some(mtime_ms) = file_scan::stat_mtime_ms(&entry.stat_target) else {
                continue;
            };
            if let Some(raw) = parse_claude_session_file(entry, mtime_ms, &indexes, true)? {
                sessions.push(raw);
            }
        }
    }
    Ok(sessions)
}

fn scan_claude_dirs_for_sync(
    claude_dirs: &[PathBuf],
    context: &AdapterSyncContext,
    since_ts: Option<i64>,
    include_events: bool,
) -> anyhow::Result<SyncScanResult> {
    let mut combined = SyncScanResult::default();
    let mut claimed = HashSet::new();
    for claude_dir in claude_dirs {
        combined.absorb(scan_for_sync_claimed(
            claude_dir,
            context,
            since_ts,
            include_events,
            &mut claimed,
        )?);
    }
    Ok(combined)
}

fn claim_session_entries(entries: &mut Vec<FileScanEntry>, claimed: &mut HashSet<String>) {
    entries.retain(|entry| claimed.insert(entry.session_id.clone()));
}

struct SessionMeta {
    cwd: Option<String>,
    started_at: Option<i64>,
    entrypoint: Option<String>,
}

#[derive(Default)]
struct SessionIndexes {
    live: HashMap<String, SessionMeta>,
    project_summaries: HashMap<String, String>,
}

fn load_session_indexes(claude_dir: &Path) -> SessionIndexes {
    SessionIndexes { live: load_session_index(claude_dir), project_summaries: HashMap::new() }
}

fn resolve_claude_dirs() -> anyhow::Result<Vec<PathBuf>> {
    if let Some(dir) = paths::env_path_dir("CLAUDE_CONFIG_DIR") {
        let Some(dir) = paths::existing_dir(dir) else {
            debug!("CLAUDE_CONFIG_DIR not found, skipping Claude Code");
            return Ok(Vec::new());
        };
        return Ok(vec![dir]);
    }
    let mut dirs = Vec::new();
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty())
        && let Some(dir) = paths::existing_dir(PathBuf::from(xdg).join("claude-code"))
    {
        dirs.push(dir);
    }
    if let Some(dir) = resolve_home_dir(".claude", "~/.claude not found, skipping Claude Code")?
        && !dirs.iter().any(|existing| existing == &dir)
    {
        dirs.push(dir);
    }
    Ok(dirs)
}

pub(crate) fn probe_invocation_nonce(
    nonce: &str,
    budget: InvocationProbeBudget,
) -> anyhow::Result<ProviderInvocationProbe> {
    let claude_dirs = resolve_claude_dirs()?;
    Ok(probe_invocation_nonce_in(&claude_dirs, nonce, budget))
}

fn probe_invocation_nonce_in(
    claude_dirs: &[PathBuf],
    nonce: &str,
    budget: InvocationProbeBudget,
) -> ProviderInvocationProbe {
    let entries =
        claude_dirs.iter().flat_map(|claude_dir| collect_invocation_entries(claude_dir)).collect();
    probe_recent_files(nonce, entries, budget, claude_invocation_input)
}

fn collect_invocation_entries(claude_dir: &Path) -> Vec<FileScanEntry> {
    let mut entries = Vec::new();
    for root in [claude_dir.join("projects"), claude_dir.join("transcripts")] {
        if !root.exists() {
            continue;
        }
        for entry in WalkDir::new(root).into_iter().filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl")
                || !path.is_file()
            {
                continue;
            }
            let Some(session_id) = transcript_source_id(path) else {
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

fn claude_invocation_input(value: &Value, nonce: &str) -> anyhow::Result<bool> {
    if value.get("type").and_then(Value::as_str) != Some("assistant") {
        return Ok(false);
    }
    let Some(content) =
        value.get("message").and_then(|message| message.get("content")).and_then(Value::as_array)
    else {
        return Ok(false);
    };
    Ok(content.iter().any(|item| {
        item.get("type").and_then(Value::as_str) == Some("tool_use")
            && item.get("name").and_then(Value::as_str).is_some_and(is_discovery_tool)
            && item.get("input").is_some_and(|input| nonce_matches_input(input, nonce))
    }))
}

fn transcript_source_id(path: &Path) -> Option<String> {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
fn scan_for_sync_impl(
    claude_dir: &Path,
    context: &AdapterSyncContext,
    since_ts: Option<i64>,
    include_events: bool,
) -> anyhow::Result<SyncScanResult> {
    scan_for_sync_claimed(claude_dir, context, since_ts, include_events, &mut HashSet::new())
}

fn scan_for_sync_claimed(
    claude_dir: &Path,
    context: &AdapterSyncContext,
    since_ts: Option<i64>,
    include_events: bool,
    claimed: &mut HashSet<String>,
) -> anyhow::Result<SyncScanResult> {
    let mut indexes = load_session_indexes(claude_dir);
    let mut entries = collect_project_entries(claude_dir, &mut indexes);
    entries.extend(collect_transcript_entries(claude_dir));
    claim_session_entries(&mut entries, claimed);

    file_scan::run_file_scan_with_options(
        context,
        since_ts,
        file_scan::FileScanOptions {
            usage_parser_version: Some(USAGE_PARSER_VERSION),
            event_parser_version: include_events.then_some(EVENT_PARSER_VERSION),
            metadata_parser_version: include_events.then_some(METADATA_PARSER_VERSION),
        },
        entries,
        |entry, mtime_ms| parse_claude_session_file(entry, mtime_ms, &indexes, include_events),
    )
}

fn load_session_index(claude_dir: &Path) -> HashMap<String, SessionMeta> {
    let sessions_dir = claude_dir.join("sessions");
    let mut index = HashMap::new();
    if !sessions_dir.exists() {
        return index;
    }

    let entries = match fs::read_dir(&sessions_dir) {
        Ok(e) => e,
        Err(e) => {
            debug!("cannot read ~/.claude/sessions: {e}");
            return index;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let v: Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(session_id) = v.get("sessionId").and_then(|s| s.as_str()) {
            let meta = SessionMeta {
                cwd: v.get("cwd").and_then(|s| s.as_str()).map(|s| s.to_string()),
                started_at: v.get("startedAt").and_then(|s| s.as_i64()),
                entrypoint: v.get("entrypoint").and_then(|s| s.as_str()).map(|s| s.to_string()),
            };
            index.insert(session_id.to_string(), meta);
        }
    }
    index
}

fn collect_project_entries(claude_dir: &Path, indexes: &mut SessionIndexes) -> Vec<FileScanEntry> {
    let projects_dir = claude_dir.join("projects");
    if !projects_dir.exists() {
        return vec![];
    }

    let mut entries = Vec::new();

    let project_dirs = match fs::read_dir(&projects_dir) {
        Ok(e) => e,
        Err(e) => {
            debug!("cannot read ~/.claude/projects: {e}");
            return vec![];
        }
    };

    for project_entry in project_dirs.flatten() {
        let project_path = project_entry.path();
        if !project_path.is_dir() {
            continue;
        }

        let dir_name = project_entry.file_name().to_string_lossy().to_string();
        let directory_hint = project_key_to_path(&dir_name);
        merge_project_session_summaries(&project_path, &mut indexes.project_summaries);

        for file_entry in WalkDir::new(&project_path).into_iter().filter_map(|e| e.ok()) {
            let file_path = file_entry.path();
            if file_path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            if !file_path.is_file() {
                continue;
            }
            let Some(session_id) = transcript_source_id(file_path) else {
                continue;
            };

            let meta_cwd = indexes.live.get(&session_id).and_then(|m| m.cwd.clone());
            let directory = meta_cwd.or_else(|| Some(directory_hint.clone()));

            entries.push(FileScanEntry {
                session_id,
                stat_target: file_path.to_path_buf(),
                directory,
            });
        }
    }

    entries
}

fn merge_project_session_summaries(
    project_path: &Path,
    project_summaries: &mut HashMap<String, String>,
) {
    let index_path = project_path.join("sessions-index.json");
    let content = match fs::read_to_string(&index_path) {
        Ok(content) => content,
        Err(_) => return,
    };
    let v: Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(err) => {
            debug!("failed to parse {}: {err}", index_path.display());
            return;
        }
    };
    let Some(entries) = v.get("entries").and_then(|entries| entries.as_array()) else {
        return;
    };
    for entry in entries {
        let Some(session_id) = entry.get("sessionId").and_then(|id| id.as_str()) else {
            continue;
        };
        let Some(summary) = entry
            .get("summary")
            .and_then(|summary| summary.as_str())
            .map(str::trim)
            .filter(|summary| !summary.is_empty())
        else {
            continue;
        };
        project_summaries.insert(session_id.to_string(), summary.to_string());
    }
}

fn collect_transcript_entries(claude_dir: &Path) -> Vec<FileScanEntry> {
    let transcripts_dir = claude_dir.join("transcripts");
    if !transcripts_dir.exists() {
        return vec![];
    }

    let mut entries = Vec::new();

    for entry in WalkDir::new(&transcripts_dir).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        if !path.is_file() {
            continue;
        }
        let Some(session_id) = transcript_source_id(path) else {
            continue;
        };

        entries.push(FileScanEntry {
            session_id,
            stat_target: path.to_path_buf(),
            directory: None,
        });
    }

    entries
}

fn parse_claude_session_file(
    entry: FileScanEntry,
    mtime_ms: i64,
    indexes: &SessionIndexes,
    include_events: bool,
) -> anyhow::Result<Option<RawSession>> {
    let mut parsed = match parse_conversation_jsonl(&entry.stat_target, mtime_ms, include_events) {
        Ok(parsed) => parsed,
        Err(e) => {
            debug!("failed to parse {}: {e}", entry.stat_target.display());
            return Ok(None);
        }
    };

    if parsed.messages.is_empty() && parsed.usage_events.is_empty() && parsed.events.is_empty() {
        return Ok(None);
    }

    let meta = indexes.live.get(&entry.session_id);
    let fallback_cwd = meta.and_then(|m| m.cwd.as_deref()).or(entry.directory.as_deref());
    for file in parsed.events.iter_mut().flat_map(|event| &mut event.files) {
        if file.cwd.is_none() {
            file.cwd = fallback_cwd.map(str::to_string);
        }
    }
    let started_at = first_timestamp(
        meta.and_then(|m| m.started_at),
        &parsed.messages,
        &parsed.usage_events,
        &parsed.events,
    )
    .unwrap_or(0);
    let directory =
        meta.and_then(|m| m.cwd.clone()).or_else(|| parsed.cwd.clone()).or(entry.directory);
    let entrypoint = meta.and_then(|m| m.entrypoint.clone());
    let source_file_path = entry.stat_target.to_str().map(|s| s.to_string());
    let duration_minutes = match (parsed.first_ts, parsed.last_ts) {
        (Some(first), Some(last)) if last >= first => Some(((last - first) / 60_000) as u32),
        _ => None,
    };
    let summary =
        parsed.summary.or_else(|| indexes.project_summaries.get(&entry.session_id).cloned());
    let (thread_role, parent_links) =
        claude_topology(&entry.stat_target, &entry.session_id, parsed.session_id.as_deref());
    Ok(Some(RawSession {
        source_id: entry.session_id,
        directory,
        started_at,
        updated_at: Some(mtime_ms),
        entrypoint,
        messages: parsed.messages,
        usage_events: parsed.usage_events,
        usage_parser_version: Some(USAGE_PARSER_VERSION),
        events: parsed.events,
        event_parser_version: include_events.then_some(EVENT_PARSER_VERSION),
        source_file_path,
        custom_title: parsed.custom_title,
        summary,
        duration_minutes,
        thread_role,
        parent_links,
        metadata_parser_version: Some(METADATA_PARSER_VERSION),
        refresh_session_on_metadata_backfill: true,
    }))
}

#[cfg(test)]
pub(crate) fn parse_conformance_fixture(claude_dir: &Path) -> anyhow::Result<Option<RawSession>> {
    let mut indexes = load_session_indexes(claude_dir);
    let Some(entry) = collect_project_entries(claude_dir, &mut indexes).into_iter().next() else {
        return Ok(None);
    };
    let Some(mtime_ms) = file_scan::stat_mtime_ms(&entry.stat_target) else {
        return Ok(None);
    };
    parse_claude_session_file(entry, mtime_ms, &indexes, true)
}

pub(crate) struct ParsedConversation {
    pub(crate) messages: Vec<RawMessage>,
    pub(crate) usage_events: Vec<RawUsageEvent>,
    pub(crate) events: Vec<RawSessionEvent>,
    cwd: Option<String>,
    custom_title: Option<String>,
    summary: Option<String>,
    first_ts: Option<i64>,
    last_ts: Option<i64>,
    session_id: Option<String>,
}

pub(crate) fn parse_conversation_jsonl(
    path: &Path,
    fallback_timestamp: i64,
    include_events: bool,
) -> anyhow::Result<ParsedConversation> {
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut messages = Vec::new();
    let mut usage_events: Vec<RawUsageEvent> = Vec::new();
    let mut events = Vec::new();
    let mut usage_index: HashMap<String, usize> = HashMap::new();
    let mut cwd: Option<String> = None;
    let mut effective_cwd: Option<String> = None;
    let mut custom_title: Option<String> = None;
    let mut summary: Option<String> = None;
    let mut first_ts: Option<i64> = None;
    let mut last_ts: Option<i64> = None;
    let mut session_id: Option<String> = None;
    let mut last_visible_message_seq: Option<u32> = None;
    let source_path = path.to_string_lossy().to_string();

    for item in jsonl_indexed(reader.lines()) {
        let (line_index, v) = item?;

        if cwd.is_none()
            && let Some(c) = v.get("cwd").and_then(|s| s.as_str())
            && !c.is_empty()
        {
            cwd = Some(c.to_string());
        }

        if session_id.is_none()
            && let Some(sid) = v.get("sessionId").and_then(|s| s.as_str())
            && !sid.is_empty()
        {
            session_id = Some(sid.to_string());
        }

        let msg_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");

        if matches!(msg_type, "custom-title" | "ai-title" | "title") {
            let title = v
                .get("customTitle")
                .or_else(|| v.get("title"))
                .and_then(|t| t.as_str())
                .map(str::trim)
                .filter(|title| !title.is_empty());
            if let Some(title) = title {
                custom_title = Some(title.to_string());
            }
            continue;
        }
        if msg_type == "summary"
            && summary.is_none()
            && let Some(s) = v.get("summary").and_then(|t| t.as_str())
        {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                summary = Some(trimmed.to_string());
            }
            continue;
        }

        match msg_type {
            "user" | "assistant" => {}
            _ => continue,
        }

        let is_compact_summary =
            v.get("isCompactSummary").and_then(Value::as_bool).unwrap_or(false);
        let is_sidechain = v.get("isSidechain").and_then(Value::as_bool).unwrap_or(false);
        let is_meta = v.get("isMeta").and_then(Value::as_bool);
        let is_machinery = is_compact_summary || is_sidechain || is_meta == Some(true);

        let role = if msg_type == "user" { Role::User } else { Role::Assistant };

        let message = match v.get("message") {
            Some(m) => m,
            None => continue,
        };

        let text = claude_visible_content(message.get("content"));
        let timestamp = rfc3339_ms(v.get("timestamp"));

        let message_seq =
            if !is_machinery && !text.is_empty() { Some(messages.len() as u32) } else { None };

        if role == Role::Assistant
            && let Some(event) = extract_claude_usage_event(
                &v,
                message,
                timestamp.unwrap_or(fallback_timestamp),
                line_index as u32,
                message_seq,
                &source_path,
            )
        {
            if let Some(existing_index) = usage_index.get(&event.event_key).copied() {
                merge_claude_usage_event(&mut usage_events[existing_index], event);
            } else {
                usage_index.insert(event.event_key.clone(), usage_events.len());
                usage_events.push(event);
            }
        }

        if !is_machinery
            && let Some(value) = v.get("cwd").and_then(Value::as_str).filter(|cwd| !cwd.is_empty())
        {
            effective_cwd = Some(value.to_string());
        }

        if include_events {
            collect_claude_content_events(
                message.get("content"),
                ClaudeContentEventContext {
                    role: role.clone(),
                    timestamp,
                    source_path: &source_path,
                    line_index,
                    prior_message_seq: if is_machinery { None } else { last_visible_message_seq },
                    current_message_seq: message_seq,
                    is_meta,
                    cwd: v
                        .get("cwd")
                        .and_then(Value::as_str)
                        .filter(|value| !value.trim().is_empty())
                        .or(effective_cwd.as_deref()),
                    visibility: is_machinery.then_some(EvidenceVisibility::Hidden),
                },
                &mut events,
            );
        }

        if is_compact_summary || is_meta == Some(true) {
            if include_events {
                collect_claude_meta_event(
                    message.get("content"),
                    role,
                    timestamp,
                    &source_path,
                    line_index,
                    &mut events,
                );
            }
            continue;
        }

        if is_sidechain {
            continue;
        }

        if let Some(ts) = timestamp {
            if first_ts.is_none_or(|f| ts < f) {
                first_ts = Some(ts);
            }
            if last_ts.is_none_or(|l| ts > l) {
                last_ts = Some(ts);
            }
        }

        if !text.is_empty() {
            messages.push(RawMessage { role, content: text, timestamp });
        }
        if claude_content_has_visible_text(message.get("content")) {
            last_visible_message_seq = message_seq;
        }
    }

    Ok(ParsedConversation {
        messages,
        usage_events,
        events,
        cwd,
        custom_title,
        summary,
        first_ts,
        last_ts,
        session_id,
    })
}

/// Subagent path → role subagent; parent id is the transcript's own `sessionId` field.
/// Claude stores subagents under `…/<parent>/subagents/<agent>.jsonl`.
fn claude_topology(
    path: &Path,
    own_source_id: &str,
    parent_session_id: Option<&str>,
) -> (Option<ThreadRole>, Vec<ParentLink>) {
    if !path.components().any(|c| c.as_os_str() == "subagents") {
        return (Some(ThreadRole::Primary), Vec::new());
    }
    let parents = parent_session_id
        .filter(|parent| !parent.is_empty() && *parent != own_source_id)
        .map(|parent| {
            vec![ParentLink {
                relation: ParentRelation::Spawn,
                source: "claude-code".to_string(),
                source_id: parent.to_string(),
            }]
        })
        .unwrap_or_default();
    (Some(ThreadRole::Subagent), parents)
}

struct ClaudeContentEventContext<'a> {
    role: Role,
    timestamp: Option<i64>,
    source_path: &'a str,
    line_index: usize,
    prior_message_seq: Option<u32>,
    current_message_seq: Option<u32>,
    is_meta: Option<bool>,
    cwd: Option<&'a str>,
    visibility: Option<EvidenceVisibility>,
}

fn collect_claude_content_events(
    content: Option<&Value>,
    context: ClaudeContentEventContext<'_>,
    events_out: &mut Vec<RawSessionEvent>,
) {
    let Some(Value::Array(arr)) = content else {
        return;
    };
    let mut message_seq = context.prior_message_seq;
    for (item_index, item) in arr.iter().enumerate() {
        match item.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if item.get("text").and_then(Value::as_str).is_some_and(|text| !text.is_empty()) {
                    message_seq = context.current_message_seq;
                }
            }
            Some("tool_use") if context.role == Role::Assistant => {
                let name = item.get("name").and_then(|n| n.as_str()).unwrap_or("tool").to_string();
                let mut event = events::tool_call_event(
                    events::EventContext {
                        event_seq: events_out.len() as u32,
                        timestamp: context.timestamp,
                        source_path: Some(context.source_path.to_string()),
                        source_event_id: Some(format!("{}:{item_index}", context.line_index)),
                        message_seq,
                        parser_version: EVENT_PARSER_VERSION,
                    },
                    name,
                    item.get("input"),
                );
                let operation = match event.name.as_deref() {
                    Some("Read") => Some(FileOperation::Read),
                    Some("Edit" | "Write" | "MultiEdit") => Some(FileOperation::Write),
                    _ => None,
                };
                if let Some(operation) = operation
                    && let Some(path) = item
                        .pointer("/input/file_path")
                        .and_then(Value::as_str)
                        .filter(|path| !path.trim().is_empty())
                {
                    event.files.push(FileEvidence {
                        path: path.to_string(),
                        operation,
                        kind: FileEvidenceKind::Call,
                        cwd: context.cwd.map(str::to_string),
                        target: None,
                    });
                }
                event.attrs_json = Some(item.to_string());
                event.tool_call_id = claude_tool_call_id(item.get("id"));
                event.is_meta = context.is_meta;
                event.visibility = context.visibility;
                events_out.push(event);
            }
            Some("tool_result") => {
                let summary = item.get("content").map(|content| match content {
                    Value::String(text) => text.to_string(),
                    other => other.to_string(),
                });
                let mut event = events::tool_result_event(
                    events::EventContext {
                        event_seq: events_out.len() as u32,
                        timestamp: context.timestamp,
                        source_path: Some(context.source_path.to_string()),
                        source_event_id: Some(format!("{}:{item_index}", context.line_index)),
                        message_seq,
                        parser_version: EVENT_PARSER_VERSION,
                    },
                    item.get("name").and_then(Value::as_str).map(String::from),
                    summary,
                );
                event.status = item
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .map(|is_error| if is_error { "error" } else { "success" }.to_string());
                event.attrs_json = Some(item.to_string());
                event.tool_call_id = claude_tool_call_id(item.get("tool_use_id"));
                event.is_meta = context.is_meta;
                event.visibility = context.visibility;
                events_out.push(event);
            }
            _ => {}
        }
    }
}

fn claude_tool_call_id(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::trim).filter(|id| !id.is_empty()).map(String::from)
}

fn claude_content_has_visible_text(content: Option<&Value>) -> bool {
    match content {
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(items)) => items.iter().any(|item| {
            item.get("type").and_then(Value::as_str) == Some("text")
                && item.get("text").and_then(Value::as_str).is_some_and(|text| !text.is_empty())
        }),
        _ => false,
    }
}

fn collect_claude_meta_event(
    content: Option<&Value>,
    role: Role,
    timestamp: Option<i64>,
    source_path: &str,
    line_index: usize,
    events_out: &mut Vec<RawSessionEvent>,
) {
    let summary = claude_visible_content(content);
    if summary.is_empty() {
        return;
    }
    events_out.push(RawSessionEvent {
        files: Vec::new(),
        event_seq: events_out.len() as u32,
        timestamp,
        kind: "message".to_string(),
        actor: role.as_str().to_string(),
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

fn claude_visible_content(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn extract_claude_usage_event(
    row: &Value,
    message: &Value,
    timestamp: i64,
    event_seq: u32,
    message_seq: Option<u32>,
    source_path: &str,
) -> Option<RawUsageEvent> {
    let usage = message.get("usage")?;
    let input_tokens = usage.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0).max(0);
    let output_tokens = usage.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0).max(0);
    let cache_read_tokens =
        usage.get("cache_read_input_tokens").and_then(|v| v.as_i64()).unwrap_or(0).max(0);
    let cache_write_tokens =
        usage.get("cache_creation_input_tokens").and_then(|v| v.as_i64()).unwrap_or(0).max(0);

    let model = message.get("model").and_then(|v| v.as_str()).filter(|v| !v.trim().is_empty())?;

    let request_id = row.get("requestId").and_then(|v| v.as_str());
    let message_id = message.get("id").and_then(|v| v.as_str());
    let event_key = match (request_id, message_id) {
        (Some(request_id), Some(message_id)) => format!("assistant:{request_id}:{message_id}"),
        (Some(request_id), None) => format!("assistant:{request_id}:line:{event_seq}"),
        (None, Some(message_id)) => format!("assistant:{message_id}:line:{event_seq}"),
        (None, None) => format!("line:{event_seq}"),
    };

    Some(RawUsageEvent {
        message_seq,
        model: model.to_string(),
        provider: "anthropic".to_string(),
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_write_tokens,
        source_path: Some(source_path.to_string()),
        raw_usage_json: Some(usage.to_string()),
        ..RawUsageEvent::observed(event_key, event_seq, timestamp, USAGE_PARSER_VERSION)
    })
}

fn merge_claude_usage_event(existing: &mut RawUsageEvent, next: RawUsageEvent) {
    existing.input_tokens = existing.input_tokens.max(next.input_tokens);
    existing.output_tokens = existing.output_tokens.max(next.output_tokens);
    existing.cache_read_tokens = existing.cache_read_tokens.max(next.cache_read_tokens);
    existing.cache_write_tokens = existing.cache_write_tokens.max(next.cache_write_tokens);
    existing.reasoning_tokens = existing.reasoning_tokens.max(next.reasoning_tokens);
    existing.timestamp = existing.timestamp.max(next.timestamp);
    existing.raw_usage_json = next.raw_usage_json;
}

fn project_key_to_path(key: &str) -> String {
    let key = key.strip_prefix('-').unwrap_or(key);
    let mut result = String::with_capacity(key.len() + 1);
    result.push('/');
    let mut chars = key.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '-' {
            if chars.peek() == Some(&'-') {
                chars.next();
                result.push_str("/.");
            } else {
                result.push('/');
            }
        } else {
            result.push(c);
        }
    }
    result
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

    fn temp_claude_root(label: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("recall-cc-test-{}-{}", label, uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_user_jsonl(project_dir: &Path, session_id: &str, text: &str) -> PathBuf {
        fs::create_dir_all(project_dir).unwrap();
        let path = project_dir.join(format!("{session_id}.jsonl"));
        let line = serde_json::json!({
            "type": "user",
            "message": {"content": text},
            "timestamp": "2026-04-13T10:00:00Z"
        });
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "{line}").unwrap();
        path
    }

    #[test]
    fn parse_claude_session_file_extracts_structured_tool_events() {
        let root = temp_claude_root("events");
        let project = root.join("projects").join("-tmp-foo");
        fs::create_dir_all(&project).unwrap();
        let path = project.join("tool-session.jsonl");
        let large_content = "汉🦀".repeat(3000);
        let cases = [
            ("Read", serde_json::json!({"file_path": "src/main.rs"}), Some(false)),
            (
                "Edit",
                serde_json::json!({"file_path": "src/main.rs", "old_string": "before", "new_string": "after"}),
                Some(true),
            ),
            (
                "Write",
                serde_json::json!({"file_path": "src/other.rs", "content": large_content}),
                None,
            ),
            (
                "MultiEdit",
                serde_json::json!({"file_path": "src/main.rs", "edits": [
                    {"old_string": "before", "new_string": "after"},
                    {"old_string": "first", "new_string": "second"}
                ]}),
                None,
            ),
        ];
        let mut f = fs::File::create(&path).unwrap();
        let mut expected_calls = Vec::new();
        let mut expected_results = Vec::new();
        for (index, (name, input, is_error)) in cases.iter().enumerate() {
            let call_id = format!("tool-{index}");
            let call = serde_json::json!({
                "type": "tool_use", "id": call_id, "name": name, "input": input
            });
            let mut assistant = serde_json::json!({
                "type": "assistant", "isMeta": false,
                "timestamp": "2026-04-13T10:00:00Z",
                "message": {"content": [
                    {"type": "text", "text": format!("Operation {index}")}, call.clone()
                ]}
            });
            if index == 1 {
                assistant["cwd"] = serde_json::json!("/tmp/target-worktree");
            }
            let mut result = serde_json::json!({
                "type": "tool_result", "tool_use_id": call_id, "content": large_content
            });
            if let Some(value) = is_error {
                result["is_error"] = serde_json::json!(value);
            } else if index == 3 {
                result["is_error"] = serde_json::json!("false");
            }
            let user = serde_json::json!({
                "type": "user", "timestamp": "2026-04-13T10:00:01Z",
                "message": {"content": [result.clone()]}
            });
            writeln!(f, "{assistant}").unwrap();
            writeln!(f, "{user}").unwrap();
            expected_calls.push(call);
            expected_results.push(result);
        }
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();
        let entry = FileScanEntry {
            session_id: "tool-session".to_string(),
            stat_target: path.clone(),
            directory: Some("/tmp/origin".to_string()),
        };
        let indexes = SessionIndexes {
            live: HashMap::from([(
                "tool-session".to_string(),
                SessionMeta {
                    cwd: Some("/tmp/session-origin".to_string()),
                    started_at: None,
                    entrypoint: None,
                },
            )]),
            project_summaries: HashMap::new(),
        };
        let raw = parse_claude_session_file(entry, mtime, &indexes, true).unwrap().unwrap();

        assert_eq!(raw.events.len(), cases.len() * 2);
        assert_eq!(raw.directory.as_deref(), Some("/tmp/session-origin"));
        for (index, (name, input, is_error)) in cases.iter().enumerate() {
            let call = &raw.events[index * 2];
            let result = &raw.events[index * 2 + 1];
            assert_eq!(call.name.as_deref(), Some(*name));
            assert_eq!(call.files.len(), 1);
            assert_eq!(call.files[0].path, input["file_path"].as_str().unwrap());
            assert_eq!(call.files[0].kind, FileEvidenceKind::Call);
            assert_eq!(
                call.files[0].operation,
                if index == 0 { FileOperation::Read } else { FileOperation::Write }
            );
            assert_eq!(
                call.files[0].cwd.as_deref(),
                Some(if index == 0 { "/tmp/session-origin" } else { "/tmp/target-worktree" })
            );
            assert_eq!(call.is_meta, Some(false));
            assert_eq!(call.message_seq, Some(index as u32));
            assert_eq!(result.message_seq, call.message_seq);
            assert_eq!(call.tool_call_id, result.tool_call_id);
            assert_eq!(call.source_event_id, Some(format!("{}:1", index * 2)));
            assert_eq!(result.source_event_id, Some(format!("{}:0", index * 2 + 1)));
            assert_eq!(
                result.status.as_deref(),
                is_error.map(|value| if value { "error" } else { "success" })
            );
            assert!(result.files.is_empty());
            assert_eq!(
                serde_json::from_str::<Value>(call.attrs_json.as_deref().unwrap()).unwrap(),
                expected_calls[index]
            );
            assert_eq!(
                serde_json::from_str::<Value>(result.attrs_json.as_deref().unwrap()).unwrap(),
                expected_results[index]
            );
        }
        assert_eq!(raw.event_parser_version, Some(EVENT_PARSER_VERSION));

        let entry = FileScanEntry {
            session_id: "tool-session".to_string(),
            stat_target: path,
            directory: Some("/tmp/foo".to_string()),
        };
        let raw = parse_claude_session_file(entry, mtime, &indexes, false).unwrap().unwrap();
        assert!(raw.events.is_empty());
        assert_eq!(raw.event_parser_version, None);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_claude_session_preserves_relationships_meta_and_source_order_anchors() {
        let root = temp_claude_root("event-relationships");
        let project = root.join("projects").join("-tmp-foo");
        fs::create_dir_all(&project).unwrap();
        let path = project.join("event-relationships.jsonl");
        let lines = [
            serde_json::json!({
                "type": "user",
                "timestamp": "2026-04-13T10:00:00Z",
                "message": {"content": "Inspect the repository"}
            }),
            serde_json::json!({
                "type": "assistant",
                "timestamp": "2026-04-13T10:00:01Z",
                "message": {
                    "content": [
                        {
                            "type": "tool_use",
                            "id": "tool-before",
                            "name": "Read",
                            "input": {"path": "Cargo.toml"}
                        },
                        {"type": "text", "text": "I found the manifest."},
                        {
                            "type": "tool_use",
                            "id": "tool-after",
                            "name": "Read",
                            "input": {"path": "src/lib.rs"}
                        },
                        {
                            "type": "tool_use",
                            "id": "   ",
                            "name": "Read",
                            "input": {"path": "src/main.rs"}
                        },
                        {
                            "type": "tool_use",
                            "id": 42,
                            "name": "Read",
                            "input": {"path": "src/cli.rs"}
                        }
                    ]
                }
            }),
            serde_json::json!({
                "type": "user",
                "timestamp": "2026-04-13T10:00:02Z",
                "message": {
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "tool-after",
                            "name": "Read result",
                            "content": "library body"
                        },
                        {
                            "type": "tool_result",
                            "tool_use_id": "tool-before",
                            "content": "manifest body"
                        },
                        {"type": "tool_result", "tool_use_id": "", "content": "empty id"},
                        {"type": "tool_result", "tool_use_id": false, "content": "wrong id"}
                    ]
                }
            }),
            serde_json::json!({
                "type": "assistant",
                "isMeta": false,
                "timestamp": "2026-04-13T10:00:03Z",
                "message": {
                    "content": [
                        {"type": "text", "text": "Continue."},
                        {
                            "type": "tool_use",
                            "id": "tool-explicit-false",
                            "name": "Glob",
                            "input": {"pattern": "src/**/*.rs"}
                        }
                    ]
                }
            }),
            serde_json::json!({
                "type": "assistant",
                "isMeta": true,
                "timestamp": "2026-04-13T10:00:04Z",
                "message": {"content": [{"type": "text", "text": "汉".repeat(5000)}]}
            }),
            serde_json::json!({
                "type": "user",
                "isCompactSummary": true,
                "timestamp": "2026-04-13T10:00:05Z",
                "message": {"content": "Compacted context"}
            }),
            serde_json::json!({
                "type": "assistant",
                "isSidechain": true,
                "timestamp": "2026-04-13T10:00:06Z",
                "cwd": "/tmp/sidechain",
                "message": {"content": [
                    {"type": "text", "text": "Hidden sidechain"},
                    {"type": "tool_use", "id": "hidden-edit", "name": "Edit", "input": {"file_path": "src/lib.rs", "old_string": "a", "new_string": "b"}}
                ]}
            }),
            serde_json::json!({
                "type": "assistant",
                "isMeta": true,
                "isSidechain": true,
                "timestamp": "2026-04-13T10:00:07Z",
                "message": {"content": "Explicit sidechain metadata"}
            }),
        ];
        let mut file = fs::File::create(&path).unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }

        let parsed = parse_conversation_jsonl(&path, 0, true).unwrap();

        assert_eq!(parsed.messages.len(), 3);
        assert!(parsed.messages.iter().all(|message| message.content != "Hidden sidechain"));
        assert_eq!(parsed.usage_events.len(), 0);
        assert_eq!(parsed.events.len(), 13);
        assert_eq!(parsed.events[0].message_seq, Some(0));
        assert_eq!(parsed.events[0].tool_call_id.as_deref(), Some("tool-before"));
        assert_eq!(parsed.events[1].message_seq, Some(1));
        assert_eq!(parsed.events[1].tool_call_id.as_deref(), Some("tool-after"));
        assert_eq!(parsed.events[2].tool_call_id, None);
        assert_eq!(parsed.events[3].tool_call_id, None);
        assert_eq!(parsed.events[4].name.as_deref(), Some("Read result"));
        assert_eq!(parsed.events[4].tool_call_id.as_deref(), Some("tool-after"));
        assert_eq!(parsed.events[5].name, None);
        assert_eq!(parsed.events[5].tool_call_id.as_deref(), Some("tool-before"));
        assert_eq!(parsed.events[6].tool_call_id, None);
        assert_eq!(parsed.events[7].tool_call_id, None);
        assert_eq!(parsed.events[8].source_event_id.as_deref(), Some("3:1"));
        assert_eq!(parsed.events[8].message_seq, Some(2));
        assert_eq!(parsed.events[8].is_meta, Some(false));
        assert_eq!(parsed.events[9].source_event_id.as_deref(), Some("4"));
        assert_eq!(parsed.events[9].is_meta, Some(true));
        assert_eq!(parsed.events[9].visibility, None);
        let summary = parsed.events[9].summary.as_deref().unwrap();
        assert!(summary.ends_with('…'));
        assert!(summary.len() <= 4099);
        assert_eq!(parsed.events[10].source_event_id.as_deref(), Some("5"));
        assert_eq!(parsed.events[10].summary.as_deref(), Some("Compacted context"));
        assert_eq!(parsed.events[10].is_meta, Some(true));
        assert_eq!(parsed.events[11].visibility, Some(EvidenceVisibility::Hidden));
        assert_eq!(parsed.events[11].message_seq, None);
        assert_eq!(parsed.events[11].tool_call_id.as_deref(), Some("hidden-edit"));
        assert_eq!(parsed.events[11].files[0].cwd.as_deref(), Some("/tmp/sidechain"));
        assert_eq!(parsed.events[12].summary.as_deref(), Some("Explicit sidechain metadata"));
        assert_eq!(parsed.events[12].is_meta, Some(true));
        assert_eq!(parsed.events[12].visibility, None);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_claude_session_file_keeps_meta_event_only_transcript() {
        let root = temp_claude_root("meta-event-only");
        let project = root.join("projects").join("-tmp-foo");
        fs::create_dir_all(&project).unwrap();
        let path = project.join("meta-event-only.jsonl");
        let line = serde_json::json!({
            "type": "assistant",
            "isCompactSummary": true,
            "timestamp": "2026-04-13T10:00:00Z",
            "message": {"content": "Compacted context"}
        });
        let mut file = fs::File::create(&path).unwrap();
        writeln!(file, "{line}").unwrap();
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();
        let entry = FileScanEntry {
            session_id: "meta-event-only".to_string(),
            stat_target: path,
            directory: Some("/tmp/foo".to_string()),
        };

        let raw = parse_claude_session_file(entry, mtime, &SessionIndexes::default(), true)
            .unwrap()
            .unwrap();

        assert!(raw.messages.is_empty());
        assert!(raw.usage_events.is_empty());
        assert_eq!(raw.events.len(), 1);
        assert_eq!(raw.started_at, 1_776_074_400_000);

        let _ = fs::remove_dir_all(&root);
    }

    fn write_usage_jsonl(project_dir: &Path, session_id: &str) -> PathBuf {
        fs::create_dir_all(project_dir).unwrap();
        let path = project_dir.join(format!("{session_id}.jsonl"));
        let first = serde_json::json!({
            "type": "assistant",
            "requestId": "req-1",
            "timestamp": "2026-04-13T10:00:00Z",
            "message": {
                "id": "msg-1",
                "model": "claude-sonnet-4-5",
                "content": [{"type": "text", "text": "partial"}],
                "usage": {
                    "input_tokens": 100,
                    "output_tokens": 20,
                    "cache_read_input_tokens": 50,
                    "cache_creation_input_tokens": 5
                }
            }
        });
        let second = serde_json::json!({
            "type": "assistant",
            "requestId": "req-1",
            "timestamp": "2026-04-13T10:00:02Z",
            "message": {
                "id": "msg-1",
                "model": "claude-sonnet-4-5",
                "content": [{"type": "text", "text": "complete"}],
                "usage": {
                    "input_tokens": 100,
                    "output_tokens": 30,
                    "cache_read_input_tokens": 50,
                    "cache_creation_input_tokens": 5
                }
            }
        });
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "{first}").unwrap();
        writeln!(f, "{second}").unwrap();
        path
    }

    fn write_usage_only_jsonl(project_dir: &Path, session_id: &str) -> PathBuf {
        fs::create_dir_all(project_dir).unwrap();
        let path = project_dir.join(format!("{session_id}.jsonl"));
        let line = serde_json::json!({
            "type": "assistant",
            "requestId": "req-usage-only",
            "timestamp": "2026-04-13T10:00:00Z",
            "message": {
                "id": "msg-usage-only",
                "model": "claude-opus-4-7",
                "content": [],
                "usage": {
                    "input_tokens": 10,
                    "output_tokens": 20,
                    "cache_read_input_tokens": 30,
                    "cache_creation_input_tokens": 40
                }
            }
        });
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "{line}").unwrap();
        path
    }

    fn make_existing_session(source_id: &str, updated_at: i64, message_count: u32) -> Session {
        Session {
            id: format!("internal-{source_id}"),
            source: "claude-code".to_string(),
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
    fn parse_claude_session_file_sets_updated_at_to_mtime() {
        let root = temp_claude_root("parse");
        let project = root.join("projects").join("-tmp-foo");
        let path = write_user_jsonl(&project, "abc-123", "hello");
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();

        let entry = FileScanEntry {
            session_id: "abc-123".to_string(),
            stat_target: path.clone(),
            directory: Some("/tmp/foo".to_string()),
        };
        let indexes = SessionIndexes::default();
        let raw = parse_claude_session_file(entry, mtime, &indexes, true).unwrap().unwrap();

        assert_eq!(raw.source_id, "abc-123");
        assert_eq!(raw.updated_at, Some(mtime));
        assert_eq!(raw.directory.as_deref(), Some("/tmp/foo"));
        assert_eq!(raw.messages.len(), 1);
        assert_eq!(raw.messages[0].content, "hello");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_claude_session_file_uses_jsonl_cwd_between_session_index_and_entry_hint() {
        let root = temp_claude_root("cwd");
        let project = root.join("projects").join("-tmp-encoded");
        fs::create_dir_all(&project).unwrap();
        let path = project.join("cwd-session.jsonl");
        let line = serde_json::json!({
            "type": "user",
            "cwd": "/tmp/from-jsonl",
            "message": {"content": "hello"},
            "timestamp": "2026-04-13T10:00:00Z"
        });
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "{line}").unwrap();
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();

        let entry = FileScanEntry {
            session_id: "cwd-session".to_string(),
            stat_target: path.clone(),
            directory: Some("/tmp/from-entry".to_string()),
        };
        let indexes = SessionIndexes::default();
        let raw = parse_claude_session_file(entry, mtime, &indexes, true).unwrap().unwrap();
        assert_eq!(raw.directory.as_deref(), Some("/tmp/from-jsonl"));
        assert_eq!(raw.duration_minutes, Some(0));
        assert_eq!(raw.source_file_path.as_deref(), path.to_str());

        let entry = FileScanEntry {
            session_id: "cwd-session".to_string(),
            stat_target: path,
            directory: Some("/tmp/from-entry".to_string()),
        };
        let indexes = SessionIndexes {
            live: HashMap::from([(
                "cwd-session".to_string(),
                SessionMeta {
                    cwd: Some("/tmp/from-index".to_string()),
                    started_at: None,
                    entrypoint: None,
                },
            )]),
            project_summaries: HashMap::new(),
        };
        let raw = parse_claude_session_file(entry, mtime, &indexes, true).unwrap().unwrap();
        assert_eq!(raw.directory.as_deref(), Some("/tmp/from-index"));
        assert_eq!(raw.started_at, 1_776_074_400_000);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_claude_session_file_extracts_title_summary_and_duration() {
        let root = temp_claude_root("metadata");
        let project = root.join("projects").join("-tmp-meta");
        fs::create_dir_all(&project).unwrap();
        let path = project.join("meta-session.jsonl");
        let lines = [
            serde_json::json!({
                "type": "user",
                "message": {"content": "start"},
                "timestamp": "2026-04-13T10:00:00Z"
            }),
            serde_json::json!({
                "type": "custom-title",
                "customTitle": "First title"
            }),
            serde_json::json!({
                "type": "custom-title",
                "customTitle": "Final title"
            }),
            serde_json::json!({
                "type": "custom-title",
                "customTitle": "   "
            }),
            serde_json::json!({
                "type": "summary",
                "summary": "   "
            }),
            serde_json::json!({
                "type": "summary",
                "summary": "First summary"
            }),
            serde_json::json!({
                "type": "summary",
                "summary": "Second summary"
            }),
            serde_json::json!({
                "type": "assistant",
                "message": {"content": "done"},
                "timestamp": "2026-04-13T10:02:00Z"
            }),
        ];
        let mut f = fs::File::create(&path).unwrap();
        for line in lines {
            writeln!(f, "{line}").unwrap();
        }
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();

        let entry = FileScanEntry {
            session_id: "meta-session".to_string(),
            stat_target: path,
            directory: Some("/tmp/meta".to_string()),
        };
        let indexes = SessionIndexes::default();
        let raw = parse_claude_session_file(entry, mtime, &indexes, true).unwrap().unwrap();

        assert_eq!(raw.custom_title.as_deref(), Some("Final title"));
        assert_eq!(raw.summary.as_deref(), Some("First summary"));
        assert_eq!(raw.duration_minutes, Some(2));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_claude_session_prefers_ai_title_when_present() {
        let root = temp_claude_root("ai-title");
        let project = root.join("projects").join("-tmp-ai");
        fs::create_dir_all(&project).unwrap();
        let path = project.join("ai-session.jsonl");
        let lines = [
            serde_json::json!({"type":"user","message":{"content":"start"},"timestamp":"2026-04-13T10:00:00Z"}),
            serde_json::json!({"type":"ai-title","title":"Named by Claude"}),
            serde_json::json!({"type":"assistant","message":{"content":"done"},"timestamp":"2026-04-13T10:00:01Z"}),
        ];
        let mut file = fs::File::create(&path).unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();
        let raw = parse_claude_session_file(
            FileScanEntry {
                session_id: "ai-session".to_string(),
                stat_target: path,
                directory: None,
            },
            mtime,
            &SessionIndexes::default(),
            false,
        )
        .unwrap()
        .unwrap();
        assert_eq!(raw.custom_title.as_deref(), Some("Named by Claude"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_for_sync_uses_project_sessions_index_summary() {
        let root = temp_claude_root("project-summary");
        let project = root.join("projects").join("-tmp-index");
        let _path = write_user_jsonl(&project, "index-session", "hello");
        let index = serde_json::json!({
            "version": 1,
            "entries": [
                {
                    "sessionId": "index-session",
                    "summary": "Project index summary"
                }
            ]
        });
        fs::write(project.join("sessions-index.json"), index.to_string()).unwrap();

        let store = setup_store();
        let result = scan_for_sync_impl(
            &root,
            &AdapterSyncContext::from_store_for_test(&store, "claude-code").unwrap(),
            None,
            true,
        )
        .unwrap();

        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].summary.as_deref(), Some("Project index summary"));
        assert_eq!(result.sessions[0].started_at, 1_776_074_400_000);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_claude_session_file_extracts_deduped_usage() {
        let root = temp_claude_root("usage");
        let project = root.join("projects").join("-tmp-foo");
        let path = write_usage_jsonl(&project, "usage-session");
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();

        let entry = FileScanEntry {
            session_id: "usage-session".to_string(),
            stat_target: path,
            directory: Some("/tmp/foo".to_string()),
        };
        let indexes = SessionIndexes::default();
        let raw = parse_claude_session_file(entry, mtime, &indexes, true).unwrap().unwrap();

        assert_eq!(raw.usage_events.len(), 1);
        let event = &raw.usage_events[0];
        assert_eq!(event.event_key, "assistant:req-1:msg-1");
        assert_eq!(event.model, "claude-sonnet-4-5");
        assert_eq!(event.provider, "anthropic");
        assert_eq!(event.input_tokens, 100);
        assert_eq!(event.output_tokens, 30);
        assert_eq!(event.cache_read_tokens, 50);
        assert_eq!(event.cache_write_tokens, 5);
        assert_eq!(event.token_source, crate::types::TokenSource::Observed);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_claude_session_file_keeps_usage_without_searchable_messages() {
        let root = temp_claude_root("usage-only");
        let project = root.join("projects").join("-tmp-foo");
        let path = write_usage_only_jsonl(&project, "usage-only-session");
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();

        let entry = FileScanEntry {
            session_id: "usage-only-session".to_string(),
            stat_target: path,
            directory: Some("/tmp/foo".to_string()),
        };
        let indexes = SessionIndexes::default();
        let raw = parse_claude_session_file(entry, mtime, &indexes, true).unwrap().unwrap();

        assert!(raw.messages.is_empty());
        assert_eq!(raw.started_at, 1_776_074_400_000);
        assert_eq!(raw.usage_events.len(), 1);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_claude_session_file_keeps_zero_token_usage_events() {
        let root = temp_claude_root("zero-usage");
        let project = root.join("projects").join("-tmp-foo");
        fs::create_dir_all(&project).unwrap();
        let path = project.join("zero-session.jsonl");
        let line = serde_json::json!({
            "type": "assistant",
            "requestId": "req-zero",
            "timestamp": "2026-04-13T10:00:00Z",
            "message": {
                "id": "msg-zero",
                "model": "gpt-5.5",
                "content": [{"type": "text", "text": "zero"}],
                "usage": {
                    "input_tokens": 0,
                    "output_tokens": 0
                }
            }
        });
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "{line}").unwrap();
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();

        let entry = FileScanEntry {
            session_id: "zero-session".to_string(),
            stat_target: path,
            directory: Some("/tmp/foo".to_string()),
        };
        let indexes = SessionIndexes::default();
        let raw = parse_claude_session_file(entry, mtime, &indexes, true).unwrap().unwrap();

        assert_eq!(raw.usage_events.len(), 1);
        assert_eq!(raw.usage_events[0].model, "gpt-5.5");
        assert_eq!(raw.usage_events[0].input_tokens, 0);
        assert_eq!(raw.usage_events[0].output_tokens, 0);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn collect_project_entries_walks_nested_projects() {
        let root = temp_claude_root("collect");
        let p1 = root.join("projects").join("-tmp-foo");
        let p2 = root.join("projects").join("-tmp-bar");
        let nested = p1.join("parent-session").join("subagents");
        write_user_jsonl(&p1, "sess-1", "a");
        write_user_jsonl(&p2, "sess-2", "b");
        write_user_jsonl(&nested, "agent-a123", "nested");

        let mut indexes = SessionIndexes::default();
        let entries = collect_project_entries(&root, &mut indexes);
        assert_eq!(entries.len(), 3);
        let ids: Vec<_> = entries.iter().map(|e| e.session_id.clone()).collect();
        assert!(ids.contains(&"sess-1".to_string()));
        assert!(ids.contains(&"sess-2".to_string()));
        assert!(ids.contains(&"agent-a123".to_string()));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_for_sync_skips_unchanged_session() {
        let root = temp_claude_root("skip");
        let project = root.join("projects").join("-tmp-proj");
        let path = write_user_jsonl(&project, "sess-skip", "hello");
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();

        let store = setup_store();
        store.insert_session(&make_existing_session("sess-skip", mtime, 1)).unwrap();
        store
            .persist_usage_events_for_existing_session(
                "claude-code",
                "sess-skip",
                &[],
                USAGE_PARSER_VERSION,
                Some(mtime),
            )
            .unwrap();
        store
            .persist_session_events_for_existing_session(
                "claude-code",
                "sess-skip",
                &[],
                EVENT_PARSER_VERSION,
                Some(mtime),
            )
            .unwrap();
        store
            .persist_topology_for_existing_session(
                "claude-code",
                "sess-skip",
                &crate::db::store::SessionTopologyWrite {
                    thread_role: None,
                    parents: &[],
                    parser_version: Some(METADATA_PARSER_VERSION),
                },
            )
            .unwrap();

        let result = scan_for_sync_impl(
            &root,
            &AdapterSyncContext::from_store_for_test(&store, "claude-code").unwrap(),
            None,
            true,
        )
        .unwrap();
        assert_eq!(result.sessions.len(), 0);
        assert_eq!(result.stats.skipped_sessions, 1);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_for_sync_reparses_unchanged_session_for_ai_title_backfill() {
        let root = temp_claude_root("ai-title-backfill");
        let project = root.join("projects").join("-tmp-proj");
        let path = write_user_jsonl(&project, "sess-ai-title", "hello");
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "{}", serde_json::json!({"type":"ai-title","title":"Named by Claude"}))
            .unwrap();
        drop(file);
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();

        let store = setup_store();
        store.insert_session(&make_existing_session("sess-ai-title", mtime, 1)).unwrap();
        store
            .persist_usage_events_for_existing_session(
                "claude-code",
                "sess-ai-title",
                &[],
                USAGE_PARSER_VERSION,
                Some(mtime),
            )
            .unwrap();
        store
            .persist_session_events_for_existing_session(
                "claude-code",
                "sess-ai-title",
                &[],
                EVENT_PARSER_VERSION,
                Some(mtime),
            )
            .unwrap();
        store
            .persist_topology_for_existing_session(
                "claude-code",
                "sess-ai-title",
                &crate::db::store::SessionTopologyWrite {
                    thread_role: None,
                    parents: &[],
                    parser_version: Some(1),
                },
            )
            .unwrap();

        let result = scan_for_sync_impl(
            &root,
            &AdapterSyncContext::from_store_for_test(&store, "claude-code").unwrap(),
            None,
            true,
        )
        .unwrap();
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].custom_title.as_deref(), Some("Named by Claude"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_for_sync_reparses_unchanged_session_with_stale_event_parser() {
        let root = temp_claude_root("event-parser-backfill");
        let project = root.join("projects").join("-tmp-proj");
        let path = write_user_jsonl(&project, "sess-event-backfill", "hello");
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();

        let store = setup_store();
        store.insert_session(&make_existing_session("sess-event-backfill", mtime, 1)).unwrap();
        store
            .persist_usage_events_for_existing_session(
                "claude-code",
                "sess-event-backfill",
                &[],
                USAGE_PARSER_VERSION,
                Some(mtime),
            )
            .unwrap();
        store
            .persist_session_events_for_existing_session(
                "claude-code",
                "sess-event-backfill",
                &[],
                EVENT_PARSER_VERSION - 1,
                Some(mtime),
            )
            .unwrap();
        store
            .persist_topology_for_existing_session(
                "claude-code",
                "sess-event-backfill",
                &crate::db::store::SessionTopologyWrite {
                    thread_role: None,
                    parents: &[],
                    parser_version: Some(METADATA_PARSER_VERSION),
                },
            )
            .unwrap();

        let result = scan_for_sync_impl(
            &root,
            &AdapterSyncContext::from_store_for_test(&store, "claude-code").unwrap(),
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
    fn scan_for_sync_reparses_when_mtime_diverges() {
        let root = temp_claude_root("mismatch");
        let project = root.join("projects").join("-tmp-proj");
        let path = write_user_jsonl(&project, "sess-stale", "hi");
        let actual_mtime = file_scan::stat_mtime_ms(&path).unwrap();

        let store = setup_store();
        store
            .insert_session(&make_existing_session("sess-stale", actual_mtime - 1_000, 1))
            .unwrap();

        let result = scan_for_sync_impl(
            &root,
            &AdapterSyncContext::from_store_for_test(&store, "claude-code").unwrap(),
            None,
            true,
        )
        .unwrap();
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].source_id, "sess-stale");
        assert_eq!(result.sessions[0].updated_at, Some(actual_mtime));
        assert_eq!(result.stats.skipped_sessions, 0);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_for_sync_picks_up_new_session() {
        let root = temp_claude_root("new");
        let project = root.join("projects").join("-tmp-proj");
        write_user_jsonl(&project, "sess-fresh", "fresh");

        let store = setup_store();

        let result = scan_for_sync_impl(
            &root,
            &AdapterSyncContext::from_store_for_test(&store, "claude-code").unwrap(),
            None,
            true,
        )
        .unwrap();
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].source_id, "sess-fresh");
        assert_eq!(result.stats.skipped_sessions, 0);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn invocation_probe_accepts_only_discovery_tool_input_and_deduplicates_a_session() {
        let root = temp_claude_root("invocation-probe");
        let project = root.join("projects").join("-tmp-probe");
        let path = write_user_jsonl(&project, "claude-session", "normal nonce-claude reference");
        let call = serde_json::json!({
            "type": "assistant",
            "message": {
                "content": [{
                    "type": "tool_use",
                    "name": "mcp__recall__list_recent_sessions",
                    "input": {"invocation_nonce": "nonce-claude"}
                }]
            }
        });
        let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
        writeln!(file, "{call}").unwrap();
        writeln!(file, "{call}").unwrap();

        let result = probe_invocation_nonce_in(
            std::slice::from_ref(&root),
            "nonce-claude",
            InvocationProbeBudget::default(),
        );
        assert!(result.complete);
        assert_eq!(result.source_ids, vec!["claude-session".to_string()]);

        let normal = probe_invocation_nonce_in(
            std::slice::from_ref(&root),
            "normal nonce-claude reference",
            InvocationProbeBudget::default(),
        );
        assert!(normal.complete);
        assert!(normal.source_ids.is_empty());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn invocation_probe_reports_multiple_source_sessions_without_guessing() {
        let root = temp_claude_root("invocation-probe-multiple");
        let project = root.join("projects").join("-tmp-probe");
        for session_id in ["claude-one", "claude-two"] {
            let path = write_user_jsonl(&project, session_id, "ordinary");
            let call = serde_json::json!({
                "type": "assistant",
                "message": {
                    "content": [{
                        "type": "tool_use",
                        "name": "mcp__recall__search_sessions",
                        "input": {"query": "history", "invocation_nonce": "nonce-shared"}
                    }]
                }
            });
            let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
            writeln!(file, "{call}").unwrap();
        }

        let result = probe_invocation_nonce_in(
            std::slice::from_ref(&root),
            "nonce-shared",
            InvocationProbeBudget::default(),
        );
        assert!(result.complete);
        assert_eq!(result.source_ids.len(), 2);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn invocation_probe_shares_budget_across_roots() {
        let first = temp_claude_root("invocation-probe-budget-first");
        let second = temp_claude_root("invocation-probe-budget-second");
        for (root, prefix) in [(&first, "first"), (&second, "second")] {
            let project = root.join("projects").join("-tmp-probe");
            for index in 0..40 {
                write_user_jsonl(&project, &format!("{prefix}-{index}"), "ordinary");
            }
        }

        let result = probe_invocation_nonce_in(
            &[first.clone(), second.clone()],
            "absent-nonce",
            InvocationProbeBudget::default(),
        );
        assert!(result.files_read <= 64);
        assert!(!result.complete);

        let _ = fs::remove_dir_all(first);
        let _ = fs::remove_dir_all(second);
    }

    #[test]
    fn duplicate_session_ids_prefer_the_first_claude_root() {
        let preferred = temp_claude_root("duplicate-preferred");
        let fallback = temp_claude_root("duplicate-fallback");
        write_user_jsonl(
            &preferred.join("projects").join("-tmp-probe"),
            "shared-session",
            "preferred",
        );
        write_user_jsonl(
            &fallback.join("projects").join("-tmp-probe"),
            "shared-session",
            "fallback",
        );
        let roots = [preferred.clone(), fallback.clone()];

        let sessions = scan_claude_dirs(&roots).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].messages[0].content, "preferred");

        let store = setup_store();
        let result = scan_claude_dirs_for_sync(
            &roots,
            &AdapterSyncContext::from_store_for_test(&store, "claude-code").unwrap(),
            None,
            true,
        )
        .unwrap();
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].messages[0].content, "preferred");

        let _ = fs::remove_dir_all(preferred);
        let _ = fs::remove_dir_all(fallback);
    }

    #[test]
    fn project_key_to_path_decodes_dashes() {
        assert_eq!(project_key_to_path("-tmp-foo"), "/tmp/foo");
        assert_eq!(
            project_key_to_path("-Users-x-git-samzong-Recall"),
            "/Users/x/git/samzong/Recall"
        );
    }

    #[test]
    fn machinery_turn_keeps_usage_but_drops_message() {
        let dir = std::env::temp_dir().join(format!("recall-mach-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("sess.jsonl");
        let mut f = fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"{{"type":"user","message":{{"role":"user","content":"hi"}},"timestamp":"2026-05-20T10:00:00Z"}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","isSidechain":true,"message":{{"role":"assistant","content":"sub-agent work","usage":{{"input_tokens":100,"output_tokens":50}},"model":"claude-x"}},"timestamp":"2026-05-20T10:00:01Z"}}"#
        )
        .unwrap();

        let parsed = parse_conversation_jsonl(&path, 0, true).unwrap();

        assert!(
            parsed.messages.iter().all(|m| m.content != "sub-agent work"),
            "machinery turn must not be a stored message"
        );
        assert!(
            !parsed.usage_events.is_empty(),
            "usage event from the machinery turn must be preserved"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn claude_topology_classifies_primary_and_subagent() {
        let primary = Path::new("/home/x/.claude/projects/proj/parent-uuid.jsonl");
        assert_eq!(
            claude_topology(primary, "parent-uuid", Some("parent-uuid")),
            (Some(ThreadRole::Primary), Vec::new())
        );

        let sub = Path::new("/home/x/.claude/projects/proj/parent-uuid/subagents/agent-abc.jsonl");
        assert_eq!(
            claude_topology(sub, "agent-abc", Some("parent-uuid")),
            (
                Some(ThreadRole::Subagent),
                vec![ParentLink {
                    relation: ParentRelation::Spawn,
                    source: "claude-code".to_string(),
                    source_id: "parent-uuid".to_string(),
                }]
            )
        );

        // A subagent without a resolvable parent, or a self-referential parent,
        // stays a subagent with no invented link.
        assert_eq!(
            claude_topology(sub, "agent-abc", None),
            (Some(ThreadRole::Subagent), Vec::new())
        );
        assert_eq!(
            claude_topology(sub, "agent-abc", Some("agent-abc")),
            (Some(ThreadRole::Subagent), Vec::new())
        );
    }
}
