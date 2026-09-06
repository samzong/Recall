mod cli_store;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use tracing::debug;
use walkdir::WalkDir;

use crate::adapters::AdapterSyncContext;
use crate::adapters::events;
use crate::adapters::json_util::json_i64;
use crate::adapters::paths::resolve_home_dir;
use crate::adapters::usage::usage_count;
use crate::adapters::{
    RawMessage, RawSession, ResumeCommand, SourceAdapter, SyncScanResult, SyncScanStats,
    first_timestamp, last_timestamp,
};
use crate::types::{
    FileEvidence, FileEvidenceKind, FileOperation, RawSessionEvent, RawUsageEvent, Role,
};

pub(crate) struct CursorAdapter;

const METADATA_PARSER_VERSION: u32 = 2;
const USAGE_PARSER_VERSION: u32 = 3;
const EVENT_PARSER_VERSION: u32 = 6;

#[derive(Debug, Clone, Default)]
struct ComposerMeta {
    name: Option<String>,
    unified_mode: Option<String>,
    directory: Option<String>,
    created_at: Option<i64>,
    last_updated_at: Option<i64>,
}

struct ParsedComposerSession {
    messages: Vec<RawMessage>,
    usage_events: Vec<RawUsageEvent>,
    events: Vec<RawSessionEvent>,
    started_at: i64,
    updated_at: Option<i64>,
    entrypoint: Option<String>,
    directory: Option<String>,
}

#[derive(Debug, Clone)]
struct AgentTranscriptPath {
    session_id: String,
    path: PathBuf,
    directory: Option<String>,
}

impl SourceAdapter for CursorAdapter {
    fn id(&self) -> &str {
        "cursor"
    }

    fn label(&self) -> &str {
        "CUR"
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        cli_store::resume_command(source_id)
    }

    fn start_command(&self, prompt: String) -> Option<ResumeCommand> {
        Some(crate::adapters::prompt_start("agent", prompt))
    }

    fn usage_parser_version(&self) -> Option<u32> {
        Some(USAGE_PARSER_VERSION)
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        scan_cursor_sessions(None, true)
    }

    fn scan_for_sync(
        &self,
        context: &AdapterSyncContext,
        since_ts: Option<i64>,
        include_events: bool,
    ) -> anyhow::Result<Option<SyncScanResult>> {
        let transcript_paths = collect_agent_transcript_paths();
        let mut transcript_meta = transcript_metadata();
        let mut result = if let Some(conn) = open_global_db()? {
            scan_for_sync_conn(
                &conn,
                context,
                since_ts,
                include_events,
                &transcript_paths,
                &mut transcript_meta,
            )?
        } else {
            SyncScanResult {
                sessions: vec![],
                stats: SyncScanStats::default(),
                observations: Vec::new(),
            }
        };
        let mut covered = result
            .sessions
            .iter()
            .map(|session| session.source_id.clone())
            .chain(result.observations.iter().map(|item| item.source_id.clone()))
            .collect::<HashSet<_>>();
        let store_result = cli_store::scan_for_sync(
            context,
            since_ts,
            &covered,
            USAGE_PARSER_VERSION,
            include_events,
        )?;
        covered.extend(store_result.sessions.iter().map(|session| session.source_id.clone()));
        covered.extend(store_result.observations.iter().map(|item| item.source_id.clone()));
        covered.extend(context.session_paths().filter_map(|item| {
            let path = Path::new(item.source_file_path.as_deref()?);
            (path.file_name()?.to_str()? == "store.db" && path.is_file())
                .then(|| item.source_id.clone())
        }));
        result.absorb(store_result);
        result.absorb(scan_transcripts_for_sync(
            context,
            since_ts,
            include_events,
            &transcript_paths,
            &covered,
            &transcript_meta,
        )?);
        Ok(Some(result))
    }
}

fn scan_for_sync_conn(
    conn: &Connection,
    context: &AdapterSyncContext,
    since_ts: Option<i64>,
    include_events: bool,
    transcript_paths: &HashMap<String, AgentTranscriptPath>,
    transcript_meta: &mut HashMap<String, ComposerMeta>,
) -> anyhow::Result<SyncScanResult> {
    debug_assert_eq!(context.source(), "cursor");
    let existing = context.session_meta();
    let existing_paths = context
        .session_paths()
        .map(|path| (path.source_id.clone(), path.source_file_path.clone()))
        .collect::<HashMap<_, _>>();
    let usage_state = context.usage_state();
    let event_state = context.event_state();
    let metadata_state = context.metadata_state();
    let global_mtime = global_db_mtime();
    let lookup = ComposerLookup::load(conn);
    let composer_ids = discover_composer_ids(conn)?;
    let mut sessions = Vec::new();
    let mut stats = SyncScanStats::default();
    let mut observations = Vec::new();

    for composer_id in composer_ids {
        let meta = load_composer_meta(conn, &composer_id, &lookup);
        transcript_meta.insert(composer_id.clone(), meta.clone());
        let source_path_changed = existing.contains_key(&composer_id)
            && transcript_paths
                .get(&composer_id)
                .and_then(|transcript| transcript.path.to_str())
                .is_some_and(|path| {
                    existing_paths.get(&composer_id).and_then(|stored| stored.as_deref())
                        != Some(path)
                });
        let updated_at = meta.last_updated_at.or(meta.created_at);
        if let Some(cutoff) = since_ts
            && updated_at.is_some_and(|ts| ts < cutoff)
            && !source_path_changed
        {
            stats.filtered_sessions += 1;
            continue;
        }

        let source_updated_at = updated_at.or(global_mtime);
        if let Some(old) = existing.get(&composer_id)
            && cli_store::find_store_db(&composer_id).is_none()
            && !existing_paths
                .get(&composer_id)
                .and_then(|path| path.as_deref())
                .is_some_and(|path| path.ends_with("store.db"))
            && old.updated_at == source_updated_at
            && crate::adapters::sync_state::session_state_is_current(
                USAGE_PARSER_VERSION,
                EVENT_PARSER_VERSION,
                usage_state.get(&composer_id).copied(),
                event_state.get(&composer_id).copied(),
                source_updated_at,
                include_events,
            )
            && crate::adapters::sync_state::metadata_state_is_current(
                METADATA_PARSER_VERSION,
                metadata_state.get(&composer_id).copied(),
                source_updated_at,
            )
            && !source_path_changed
        {
            stats.skipped_sessions += 1;
            observations.push(crate::adapters::SourceObservation {
                source_id: composer_id,
                source_file_path: None,
            });
            continue;
        }

        if let Some(raw) =
            build_raw_session(conn, &composer_id, &meta, transcript_paths, include_events)?
        {
            sessions.push(raw);
        }
    }

    Ok(SyncScanResult { sessions, stats, observations })
}

fn transcript_metadata() -> HashMap<String, ComposerMeta> {
    build_agent_cwd_map(resolve_global_state_db_path().as_deref())
        .into_iter()
        .map(|(id, directory)| {
            (id, ComposerMeta { directory: Some(directory), ..Default::default() })
        })
        .collect()
}

fn scan_transcripts_for_sync(
    context: &AdapterSyncContext,
    since_ts: Option<i64>,
    include_events: bool,
    transcripts: &HashMap<String, AgentTranscriptPath>,
    covered: &HashSet<String>,
    transcript_meta: &HashMap<String, ComposerMeta>,
) -> anyhow::Result<SyncScanResult> {
    let entries =
        transcripts.values().filter(|entry| !covered.contains(&entry.session_id)).map(|entry| {
            crate::adapters::file_scan::FileScanEntry {
                session_id: entry.session_id.clone(),
                stat_target: entry.path.clone(),
                directory: entry.directory.clone(),
            }
        });
    crate::adapters::file_scan::run_file_scan_with_options_and_snapshot(
        context,
        since_ts,
        crate::adapters::file_scan::FileScanOptions {
            usage_parser_version: Some(USAGE_PARSER_VERSION),
            event_parser_version: include_events.then_some(EVENT_PARSER_VERSION),
            metadata_parser_version: Some(METADATA_PARSER_VERSION),
        },
        entries,
        |entry| {
            let snapshot = crate::adapters::file_scan::file_metadata_snapshot(&entry.stat_target)?;
            Some(crate::adapters::file_scan::FileScanSnapshot::new(snapshot.mtime_ms()?, snapshot))
        },
        |entry, mtime_ms| {
            load_transcript(
                &entry.stat_target,
                &entry.session_id,
                entry.directory,
                transcript_meta.get(&entry.session_id),
                mtime_ms,
                include_events,
            )
        },
    )
}

fn load_transcript(
    path: &Path,
    session_id: &str,
    directory: Option<String>,
    meta: Option<&ComposerMeta>,
    mtime_ms: i64,
    include_events: bool,
) -> anyhow::Result<Option<RawSession>> {
    let Some(mut raw) = parse_agent_transcript(path, include_events)? else { return Ok(None) };
    for file in raw.events.iter_mut().flat_map(|event| &mut event.files) {
        if file.cwd.is_none() && file.kind != FileEvidenceKind::Command {
            file.cwd = meta.and_then(|meta| meta.directory.clone());
        }
    }
    raw.source_id = session_id.into();
    raw.directory = meta.and_then(|meta| meta.directory.clone()).or(raw.directory).or(directory);
    raw.entrypoint = meta.and_then(|meta| meta.unified_mode.clone()).or(raw.entrypoint);
    raw.started_at = stat_birth_ms(path).unwrap_or(mtime_ms);
    raw.updated_at = Some(mtime_ms);
    Ok(Some(raw))
}

fn scan_cursor_sessions(
    since_ts: Option<i64>,
    include_events: bool,
) -> anyhow::Result<Vec<RawSession>> {
    let transcript_paths = collect_agent_transcript_paths();
    let mut transcript_meta = transcript_metadata();
    let mut sessions = Vec::new();
    let mut covered = HashSet::new();
    if let Some(conn) = open_global_db()? {
        let lookup = ComposerLookup::load(&conn);
        for composer_id in discover_composer_ids(&conn)? {
            let meta = load_composer_meta(&conn, &composer_id, &lookup);
            transcript_meta.insert(composer_id.clone(), meta.clone());
            if since_ts.is_some_and(|cutoff| {
                meta.last_updated_at.or(meta.created_at).is_some_and(|ts| ts < cutoff)
            }) {
                continue;
            }
            if let Some(raw) =
                build_raw_session(&conn, &composer_id, &meta, &transcript_paths, include_events)?
            {
                covered.insert(composer_id);
                sessions.push(raw);
            }
        }
    }
    sessions.extend(cli_store::scan_uncovered(&covered, USAGE_PARSER_VERSION, include_events)?);
    covered.extend(sessions.iter().map(|session| session.source_id.clone()));
    for transcript in transcript_paths.values().filter(|entry| !covered.contains(&entry.session_id))
    {
        let Some(mtime_ms) = stat_mtime_ms(&transcript.path) else { continue };
        if since_ts.is_some_and(|cutoff| mtime_ms < cutoff) {
            continue;
        }
        if let Some(raw) = load_transcript(
            &transcript.path,
            &transcript.session_id,
            transcript.directory.clone(),
            transcript_meta.get(&transcript.session_id),
            mtime_ms,
            include_events,
        )? {
            sessions.push(raw);
        }
    }
    Ok(sessions)
}

fn build_raw_session(
    conn: &Connection,
    composer_id: &str,
    meta: &ComposerMeta,
    transcript_paths: &HashMap<String, AgentTranscriptPath>,
    include_events: bool,
) -> anyhow::Result<Option<RawSession>> {
    let Some(parsed) = parse_composer_session(conn, composer_id, meta, include_events)? else {
        return Ok(None);
    };

    let mut session = RawSession::search_only(
        composer_id.to_string(),
        parsed.directory.or(meta.directory.clone()),
        parsed.started_at,
        parsed.updated_at,
        parsed.entrypoint.or(meta.unified_mode.clone()),
        parsed.messages,
    )
    .with_usage(parsed.usage_events, USAGE_PARSER_VERSION);
    if include_events {
        session = session.with_events(parsed.events, EVENT_PARSER_VERSION);
    }
    session.metadata_parser_version = Some(METADATA_PARSER_VERSION);
    session.refresh_session_on_metadata_backfill = true;
    session.source_file_path = transcript_paths
        .get(composer_id)
        .and_then(|transcript| transcript.path.to_str())
        .map(str::to_string);
    Ok(Some(session))
}

fn parse_composer_session(
    conn: &Connection,
    composer_id: &str,
    meta: &ComposerMeta,
    include_events: bool,
) -> anyhow::Result<Option<ParsedComposerSession>> {
    let Some(raw_json) = read_disk_kv(conn, &format!("composerData:{composer_id}")) else {
        return Ok(None);
    };
    let data: Value = match serde_json::from_str(&raw_json) {
        Ok(value) => value,
        Err(err) => {
            debug!("failed to parse composerData for {composer_id}: {err}");
            return Ok(None);
        }
    };

    let headers = data
        .get("fullConversationHeadersOnly")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    let conversation_map = data.get("conversationMap").and_then(|value| value.as_object());

    let mut messages = Vec::new();
    let mut has_tool_records = false;
    let mut usage_events = Vec::new();
    let mut bubble_usage_events = Vec::new();
    let mut session_events = Vec::new();
    let source_path = format!("composer:{composer_id}");

    for (index, header) in headers.iter().enumerate() {
        let bubble_id = header.get("bubbleId").and_then(|value| value.as_str());
        let header_type = header.get("type").and_then(|value| value.as_i64());
        let role = bubble_role(header_type);
        let Some(role) = role else {
            continue;
        };

        let stored_bubble =
            bubble_id.and_then(|bubble_id| load_bubble(conn, composer_id, bubble_id));
        let bubble = stored_bubble
            .as_ref()
            .or_else(|| conversation_map.and_then(|map| bubble_id.and_then(|id| map.get(id))));
        has_tool_records |=
            bubble.and_then(|bubble| bubble.get("toolFormerData")).is_some_and(Value::is_object);
        let content = bubble.map(|bubble| render_bubble_content(bubble, &role)).unwrap_or_default();

        let timestamp = bubble.and_then(|value| json_i64(value.get("createdAt"))).or_else(|| {
            conversation_map
                .and_then(|map| bubble_id.and_then(|id| map.get(id)))
                .and_then(|value| json_i64(value.get("createdAt")))
        });

        if !content.is_empty() {
            messages.push(RawMessage { role: role.clone(), content, timestamp });
        }
        let message_seq = messages.len().checked_sub(1).map(|seq| seq as u32);

        if include_events
            && matches!(role, Role::Assistant)
            && let Some(bubble) = bubble
        {
            collect_bubble_tool_events(
                bubble,
                bubble_id,
                &source_path,
                timestamp,
                message_seq,
                &mut session_events,
            );
        }

        if let Some(bubble) = bubble
            && let Some(event) = extract_bubble_usage_event(
                composer_id,
                bubble_id.unwrap_or("unknown"),
                index as u32,
                message_seq,
                timestamp.unwrap_or(0),
                bubble,
                &data,
            )
        {
            bubble_usage_events.push(event);
        }
    }

    for file in session_events.iter_mut().flat_map(|event| &mut event.files) {
        if file.cwd.is_none() && file.kind != FileEvidenceKind::Command {
            file.cwd.clone_from(&meta.directory);
        }
    }

    if !bubble_usage_events.is_empty() {
        usage_events.extend(bubble_usage_events);
    } else if let Some(event) = extract_session_usage_event(composer_id, &data, meta) {
        usage_events.push(event);
    }

    if messages.is_empty()
        && usage_events.is_empty()
        && session_events.is_empty()
        && !has_tool_records
    {
        return Ok(None);
    }

    let started_at = first_timestamp(
        json_i64(data.get("createdAt")).or(meta.created_at),
        &messages,
        &usage_events,
        &session_events,
    )
    .unwrap_or(0);
    let updated_at = last_timestamp(
        json_i64(data.get("lastUpdatedAt"))
            .or(json_i64(data.get("conversationCheckpointLastUpdatedAt")))
            .or(meta.last_updated_at),
        &messages,
        &usage_events,
        &session_events,
    );

    Ok(Some(ParsedComposerSession {
        messages,
        usage_events,
        events: session_events,
        started_at,
        updated_at,
        entrypoint: data
            .get("unifiedMode")
            .and_then(|value| value.as_str())
            .map(str::to_string)
            .or_else(|| meta.unified_mode.clone()),
        directory: meta.directory.clone(),
    }))
}

fn discover_composer_ids(conn: &Connection) -> anyhow::Result<BTreeSet<String>> {
    let mut ids = BTreeSet::new();

    if let Some(raw) = read_item_value(conn, "composer.composerHeaders")
        && let Ok(value) = serde_json::from_str::<Value>(&raw)
        && let Some(items) = value.get("allComposers").and_then(|value| value.as_array())
    {
        for item in items {
            if let Some(id) = item.get("composerId").and_then(|value| value.as_str()) {
                ids.insert(id.to_string());
            }
        }
    }

    let mut stmt = conn.prepare("SELECT key FROM cursorDiskKV WHERE key LIKE 'composerData:%'")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    for row in rows {
        let key = row?;
        if let Some(id) = key.strip_prefix("composerData:") {
            ids.insert(id.to_string());
        }
    }

    if let Some(workspace_dir) = resolve_workspace_storage_dir() {
        for entry in fs::read_dir(workspace_dir)? {
            let entry = entry?;
            let db_path = entry.path().join("state.vscdb");
            if !db_path.exists() {
                continue;
            }
            if let Ok(workspace_conn) = Connection::open_with_flags(
                &db_path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            ) && let Some(raw) = read_item_value(&workspace_conn, "composer.composerData")
                && let Ok(value) = serde_json::from_str::<Value>(&raw)
            {
                collect_composer_ids_from_workspace_data(&value, &mut ids);
            }
        }
    }

    for transcript in collect_agent_transcript_paths().values() {
        ids.insert(transcript.session_id.clone());
    }

    Ok(ids)
}

fn collect_composer_ids_from_workspace_data(value: &Value, ids: &mut BTreeSet<String>) {
    if let Some(items) = value.get("allComposers").and_then(|value| value.as_array()) {
        for item in items {
            if let Some(id) = item.get("composerId").and_then(|value| value.as_str()) {
                ids.insert(id.to_string());
            }
        }
    }
    for key in ["selectedComposerIds", "lastFocusedComposerIds"] {
        if let Some(items) = value.get(key).and_then(|value| value.as_array()) {
            for item in items {
                if let Some(id) = item.as_str() {
                    ids.insert(id.to_string());
                }
            }
        }
    }
}

/// `composer.composerHeaders` is one document describing every composer, and
/// the agent cwd map is a second full-database read. Both were rebuilt once per
/// composer id, which made metadata lookup quadratic in the number of sessions.
/// Building them once per scan is what keeps the Cursor scan linear.
struct ComposerLookup {
    headers: HashMap<String, ComposerMeta>,
    agent_cwd: HashMap<String, String>,
}

impl ComposerLookup {
    fn load(conn: &Connection) -> Self {
        let mut headers = HashMap::new();
        if let Some(raw) = read_item_value(conn, "composer.composerHeaders")
            && let Ok(value) = serde_json::from_str::<Value>(&raw)
            && let Some(items) = value.get("allComposers").and_then(|value| value.as_array())
        {
            for item in items {
                let Some(composer_id) = item.get("composerId").and_then(|value| value.as_str())
                else {
                    continue;
                };
                headers.insert(
                    composer_id.to_string(),
                    ComposerMeta {
                        name: item.get("name").and_then(|value| value.as_str()).map(str::to_string),
                        unified_mode: item
                            .get("unifiedMode")
                            .and_then(|value| value.as_str())
                            .map(str::to_string),
                        directory: workspace_path_from_identifier(item.get("workspaceIdentifier")),
                        created_at: json_i64(item.get("createdAt")),
                        last_updated_at: json_i64(item.get("lastUpdatedAt")),
                    },
                );
            }
        }

        Self { headers, agent_cwd: build_agent_cwd_map(resolve_global_state_db_path().as_deref()) }
    }
}

fn load_composer_meta(
    conn: &Connection,
    composer_id: &str,
    lookup: &ComposerLookup,
) -> ComposerMeta {
    let mut meta = lookup.headers.get(composer_id).cloned().unwrap_or(ComposerMeta {
        name: None,
        unified_mode: None,
        directory: None,
        created_at: None,
        last_updated_at: None,
    });

    if let Some(raw) = read_disk_kv(conn, &format!("composerData:{composer_id}"))
        && let Ok(data) = serde_json::from_str::<Value>(&raw)
    {
        if meta.name.is_none() {
            meta.name = data.get("name").and_then(|value| value.as_str()).map(str::to_string);
        }
        if meta.unified_mode.is_none() {
            meta.unified_mode =
                data.get("unifiedMode").and_then(|value| value.as_str()).map(str::to_string);
        }
        if meta.created_at.is_none() {
            meta.created_at = json_i64(data.get("createdAt"));
        }
        if meta.last_updated_at.is_none() {
            meta.last_updated_at = json_i64(data.get("lastUpdatedAt"))
                .or(json_i64(data.get("conversationCheckpointLastUpdatedAt")));
        }
    }

    if meta.directory.is_none()
        && let Some(path) = lookup.agent_cwd.get(composer_id)
    {
        meta.directory = Some(path.clone());
    }

    meta
}

fn workspace_path_from_identifier(value: Option<&Value>) -> Option<String> {
    let uri = value?.get("uri")?;
    uri.get("fsPath")
        .and_then(|value| value.as_str())
        .filter(|path| !path.trim().is_empty())
        .map(str::to_string)
}

fn load_bubble(conn: &Connection, composer_id: &str, bubble_id: &str) -> Option<Value> {
    let raw = read_disk_kv(conn, &format!("bubbleId:{composer_id}:{bubble_id}"))?;
    serde_json::from_str(&raw).ok()
}

fn render_bubble_content(bubble: &Value, role: &Role) -> String {
    let mut parts = Vec::new();
    if let Some(text) = non_empty_str(bubble.get("text").or_else(|| bubble.get("rawText"))) {
        let normalized = if matches!(role, Role::User) {
            strip_user_query_envelope(text).trim().to_string()
        } else {
            text.trim().to_string()
        };
        if !normalized.is_empty() {
            parts.push(normalized);
        }
    }

    if let Some(blocks) = bubble.get("codeBlocks").and_then(|value| value.as_array()) {
        for block in blocks {
            if let Some(content) = block.get("content").and_then(|value| value.as_str()) {
                parts.push(format!("[code_block] {content}"));
            }
        }
    }

    parts.join("\n")
}

fn cursor_tool_call(
    context: events::EventContext,
    name: String,
    args: Option<&Value>,
) -> RawSessionEvent {
    let decoded =
        args.and_then(Value::as_str).and_then(|text| serde_json::from_str::<Value>(text).ok());
    let args = decoded.as_ref().or(args);
    let mut event = if let Some(text) = args.and_then(Value::as_str) {
        let mut event = events::tool_call_event_from_text(context, name.clone(), Some(text));
        event.attrs_json = args.map(Value::to_string);
        event
    } else {
        events::tool_call_event(context, name.clone(), args)
    };
    if matches!(name.as_str(), "Shell" | "run_terminal_command_v2") {
        let cwd_key = if name == "Shell" { "working_directory" } else { "cwd" };
        let native_cwd = args.and_then(|args| args.get(cwd_key));
        let cwd = native_cwd
            .and_then(Value::as_str)
            .filter(|path| Path::new(path).is_absolute() && !path.contains(['$', '`']));
        let (files, mut status) =
            match args.and_then(|args| args.get("command")).and_then(Value::as_str) {
                Some(command) => events::shell_file_evidence(command, cwd),
                None => (Vec::new(), crate::types::CommandEvidenceStatus::Unsupported),
            };
        if native_cwd.is_some()
            && cwd.is_none()
            && status != crate::types::CommandEvidenceStatus::LimitExceeded
        {
            status = crate::types::CommandEvidenceStatus::Unsupported;
        }
        event.files = files;
        event.command_evidence_status = Some(status);
        return event;
    }
    if name == "ApplyPatch" {
        event.files =
            args.and_then(Value::as_str).map(events::patch_file_evidence).unwrap_or_default();
        event.target = event.files.first().map(|file| file.path.clone());
        if !event.files.is_empty() {
            event.kind = "file_write".into();
        }
        return event;
    }
    let operation = match name.as_str() {
        "StrReplace" | "Write" | "edit_file" | "edit_file_v2" | "write_file" => {
            FileOperation::Write
        }
        "Read" | "ReadFile" | "read_file" | "read_file_v2" => FileOperation::Read,
        "Delete" | "delete_file" => FileOperation::Delete,
        _ => return event,
    };
    let Some(args) = args else { return event };
    let path = ["path", "file_path", "relativeWorkspacePath", "targetFile"]
        .iter()
        .find_map(|key| args.get(key).and_then(Value::as_str))
        .filter(|path| !path.trim().is_empty());
    if let Some(path) = path {
        event.kind =
            if operation == FileOperation::Read { "file_read" } else { "file_write" }.into();
        event.target = Some(path.into());
        event.files.push(FileEvidence {
            path: path.into(),
            operation,
            kind: FileEvidenceKind::Call,
            cwd: ["cwd", "workingDirectory", "working_directory", "workdir"]
                .iter()
                .find_map(|key| args.get(key).and_then(Value::as_str))
                .map(str::to_string),
            target: None,
        });
    }
    event
}

fn cursor_tool_result(
    context: events::EventContext,
    name: Option<String>,
    result: Option<&Value>,
) -> RawSessionEvent {
    let decoded =
        result.and_then(Value::as_str).and_then(|text| serde_json::from_str::<Value>(text).ok());
    let result = decoded.as_ref().or(result);
    let mut event = events::tool_result_event(context, name, result.and_then(render_json_fragment));
    event.attrs_json = result.map(Value::to_string);
    event
}

fn collect_bubble_tool_events(
    bubble: &Value,
    bubble_id: Option<&str>,
    source_path: &str,
    timestamp: Option<i64>,
    message_seq: Option<u32>,
    events_out: &mut Vec<RawSessionEvent>,
) {
    let Some(tool_data) = bubble.get("toolFormerData") else { return };
    let name = tool_data.get("name").and_then(Value::as_str).unwrap_or("tool").to_string();
    let source_id = cursor_native_id(bubble_id);
    let native_id = cursor_native_id(tool_data.get("toolCallId").and_then(Value::as_str))
        .or_else(|| source_id.clone());
    let context = |event_seq| events::EventContext {
        event_seq,
        timestamp,
        source_path: Some(source_path.into()),
        source_event_id: source_id.clone(),
        message_seq,
        parser_version: EVENT_PARSER_VERSION,
    };
    let mut call = cursor_tool_call(
        context(events_out.len() as u32),
        name.clone(),
        tool_data.get("params").or_else(|| tool_data.get("rawArgs")),
    );
    call.tool_call_id = native_id.clone();
    events_out.push(call);
    if let Some(result) = tool_data.get("result") {
        let mut event =
            cursor_tool_result(context(events_out.len() as u32), Some(name), Some(result));
        event.tool_call_id = native_id;
        event.status = tool_data.get("status").and_then(Value::as_str).map(str::to_string);
        events_out.push(event);
    }
}

fn cursor_native_id(value: Option<&str>) -> Option<String> {
    value.map(str::trim).filter(|id| !id.is_empty() && *id != "unknown").map(String::from)
}

fn extract_bubble_usage_event(
    composer_id: &str,
    bubble_id: &str,
    event_seq: u32,
    message_seq: Option<u32>,
    timestamp: i64,
    bubble: &Value,
    composer_data: &Value,
) -> Option<RawUsageEvent> {
    let token_count = bubble.get("tokenCount")?;
    let input_tokens = usage_count(token_count, &["inputTokens"]);
    let output_tokens = usage_count(token_count, &["outputTokens"]);
    if input_tokens == 0 && output_tokens == 0 {
        return None;
    }

    let model = model_from_composer(composer_data);
    Some(RawUsageEvent {
        message_seq,
        model: model.clone(),
        provider: infer_cursor_provider(&model),
        input_tokens,
        output_tokens,
        source_path: Some(format!("composer:{composer_id}")),
        raw_usage_json: Some(token_count.to_string()),
        ..RawUsageEvent::observed(
            format!("bubble:{bubble_id}"),
            event_seq,
            timestamp,
            USAGE_PARSER_VERSION,
        )
    })
}

fn extract_session_usage_event(
    composer_id: &str,
    composer_data: &Value,
    meta: &ComposerMeta,
) -> Option<RawUsageEvent> {
    if let Some(breakdown) = composer_data.get("promptTokenBreakdown") {
        let total_used = json_i64(breakdown.get("totalUsedTokens")).unwrap_or(0).max(0);
        if total_used == 0 {
            return None;
        }
        let (input_tokens, cache_read_tokens) = map_context_breakdown(breakdown, total_used);
        return Some(build_session_usage_event(
            composer_id,
            composer_data,
            meta,
            input_tokens,
            cache_read_tokens,
            breakdown,
        ));
    }

    let total_used = json_i64(composer_data.get("contextTokensUsed")).unwrap_or(0).max(0);
    if total_used == 0 {
        return None;
    }
    Some(build_session_usage_event(composer_id, composer_data, meta, total_used, 0, &Value::Null))
}

fn map_context_breakdown(breakdown: &Value, total_used: i64) -> (i64, i64) {
    let mut conversation_tokens = 0;
    let mut prompt_tokens = 0;
    if let Some(categories) = breakdown.get("categories").and_then(|value| value.as_array()) {
        for category in categories {
            let id = category.get("id").and_then(|value| value.as_str()).unwrap_or("");
            let estimated = json_i64(category.get("estimatedTokens")).unwrap_or(0).max(0);
            match id {
                "conversation" | "summarized_conversation" => conversation_tokens += estimated,
                _ => prompt_tokens += estimated,
            }
        }
    }
    let categorized = conversation_tokens + prompt_tokens;
    if categorized < total_used {
        prompt_tokens += total_used - categorized;
    }
    (prompt_tokens, conversation_tokens)
}

fn build_session_usage_event(
    composer_id: &str,
    composer_data: &Value,
    meta: &ComposerMeta,
    input_tokens: i64,
    cache_read_tokens: i64,
    breakdown: &Value,
) -> RawUsageEvent {
    let model = model_from_composer(composer_data);
    let timestamp = meta
        .last_updated_at
        .or(meta.created_at)
        .or_else(|| json_i64(composer_data.get("lastUpdatedAt")))
        .unwrap_or(0);
    RawUsageEvent {
        model: model.clone(),
        provider: infer_cursor_provider(&model),
        input_tokens,
        cache_read_tokens,
        source_path: Some(format!("composer:{composer_id}")),
        raw_usage_json: if breakdown.is_null() { None } else { Some(breakdown.to_string()) },
        ..RawUsageEvent::derived(
            "session:prompt-token-breakdown".to_string(),
            0,
            timestamp,
            USAGE_PARSER_VERSION,
        )
    }
}

fn model_from_composer(composer_data: &Value) -> String {
    composer_data
        .get("modelConfig")
        .and_then(|value| value.get("modelName"))
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "unknown".to_string())
}

fn infer_cursor_provider(model: &str) -> String {
    let lower = model.to_lowercase();
    if lower.starts_with("claude") {
        "anthropic".to_string()
    } else if lower.starts_with("gpt") || lower.starts_with("o1") || lower.starts_with("o3") {
        "openai".to_string()
    } else if lower.starts_with("gemini") {
        "google".to_string()
    } else {
        "cursor".to_string()
    }
}

fn bubble_role(header_type: Option<i64>) -> Option<Role> {
    match header_type {
        Some(1) => Some(Role::User),
        Some(2) => Some(Role::Assistant),
        _ => None,
    }
}

fn parse_agent_transcript(path: &Path, include_events: bool) -> anyhow::Result<Option<RawSession>> {
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut messages = Vec::new();
    let mut has_tool_records = false;
    let mut session_events = Vec::new();
    let mut last_visible_message_seq: Option<u32> = None;
    let source_path = path.display().to_string();

    for (line_index, line) in reader.lines().enumerate() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let role = match v.get("role").and_then(|r| r.as_str()) {
            Some("user") => Role::User,
            Some("assistant") => Role::Assistant,
            _ => continue,
        };
        let content_array =
            v.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_array());
        let Some(items) = content_array else {
            continue;
        };
        has_tool_records |= items.iter().any(|item| {
            matches!(
                item.get("type").and_then(Value::as_str),
                Some("tool_use" | "tool-call" | "tool_result" | "tool-result")
            )
        });
        let is_user = matches!(role, Role::User);
        let content = render_transcript_content_items(items, is_user);
        let current_message_seq =
            if content.is_empty() { None } else { Some(messages.len() as u32) };
        if include_events {
            collect_transcript_content_events(
                items,
                &source_path,
                &line_index.to_string(),
                last_visible_message_seq,
                current_message_seq,
                &mut session_events,
            );
        }
        if content.is_empty() {
            continue;
        }
        messages.push(RawMessage { role, content, timestamp: None });
        if transcript_content_has_visible_text(items) {
            last_visible_message_seq = current_message_seq;
        }
    }

    if messages.is_empty() && session_events.is_empty() && !has_tool_records {
        return Ok(None);
    }

    let mut session =
        RawSession::search_only(String::new(), None, 0, None, Some("agent".to_string()), messages)
            .with_usage(Vec::new(), USAGE_PARSER_VERSION);
    session.source_file_path = Some(source_path);
    session.metadata_parser_version = Some(METADATA_PARSER_VERSION);
    session.refresh_session_on_metadata_backfill = true;
    if include_events {
        session = session.with_events(session_events, EVENT_PARSER_VERSION);
    }
    Ok(Some(session))
}

#[cfg(test)]
pub(crate) fn parse_conformance_fixture(
    path: &Path,
    source_id: &str,
) -> anyhow::Result<Option<RawSession>> {
    let mut raw = parse_agent_transcript(path, true)?;
    if let Some(session) = raw.as_mut() {
        session.source_id = source_id.to_string();
    }
    Ok(raw)
}

fn collect_transcript_content_events(
    items: &[Value],
    source_path: &str,
    record_id: &str,
    prior_message_seq: Option<u32>,
    current_message_seq: Option<u32>,
    events_out: &mut Vec<RawSessionEvent>,
) {
    let mut message_seq = prior_message_seq;
    for (item_index, item) in items.iter().enumerate() {
        let context = events::EventContext {
            event_seq: events_out.len() as u32,
            timestamp: None,
            source_path: Some(source_path.into()),
            source_event_id: Some(format!("{record_id}:{item_index}")),
            message_seq,
            parser_version: EVENT_PARSER_VERSION,
        };
        let mut event = match item.get("type").and_then(Value::as_str) {
            Some("text") => {
                if item
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| !text.trim().is_empty())
                {
                    message_seq = current_message_seq;
                }
                continue;
            }
            Some("tool_use" | "tool-call") => {
                let name = item
                    .get("name")
                    .or_else(|| item.get("toolName"))
                    .and_then(Value::as_str)
                    .unwrap_or("tool");
                let mut event = cursor_tool_call(
                    context,
                    name.into(),
                    item.get("input").or_else(|| item.get("args")),
                );
                event.tool_call_id = cursor_native_id(
                    item.get("id").or_else(|| item.get("toolCallId")).and_then(Value::as_str),
                );
                event
            }
            Some("tool_result" | "tool-result") => {
                let mut event = cursor_tool_result(
                    context,
                    item.get("toolName").and_then(Value::as_str).map(str::to_string),
                    item.get("content").or_else(|| item.get("result")),
                );
                event.tool_call_id = cursor_native_id(
                    item.get("tool_use_id")
                        .or_else(|| item.get("toolCallId"))
                        .and_then(Value::as_str),
                );
                event.status = item
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .map(|failed| if failed { "error" } else { "success" }.into());
                event
            }
            _ => continue,
        };
        event.attrs_json = Some(item.to_string());
        events_out.push(event);
    }
}

fn transcript_content_has_visible_text(items: &[Value]) -> bool {
    items.iter().any(|item| {
        item.get("type").and_then(Value::as_str) == Some("text")
            && item.get("text").and_then(Value::as_str).is_some_and(|text| !text.trim().is_empty())
    })
}

fn render_transcript_content_items(items: &[Value], is_user: bool) -> String {
    let mut parts = Vec::new();
    for item in items {
        if item.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        let Some(text) = item.get("text").and_then(Value::as_str) else { continue };
        let normalized = if is_user { strip_user_query_envelope(text) } else { text };
        let trimmed = normalized.trim();
        if !trimmed.is_empty() {
            parts.push(trimmed.to_string());
        }
    }
    parts.join("\n")
}

fn strip_user_query_envelope(text: &str) -> &str {
    const OPEN: &str = "<user_query>";
    const CLOSE: &str = "</user_query>";
    let trimmed = text.trim();
    if let Some(inner) = trimmed.strip_prefix(OPEN).and_then(|s| s.strip_suffix(CLOSE)) {
        inner
    } else {
        text
    }
}

fn collect_agent_transcript_paths() -> HashMap<String, AgentTranscriptPath> {
    let Some(projects_dir) = resolve_projects_dir().ok().flatten() else {
        return HashMap::new();
    };
    collect_agent_transcript_paths_from_dir(&projects_dir)
        .into_iter()
        .map(|transcript| (transcript.session_id.clone(), transcript))
        .collect()
}

fn collect_agent_transcript_paths_from_dir(projects_dir: &Path) -> Vec<AgentTranscriptPath> {
    let mut entries = Vec::new();
    for walk_entry in WalkDir::new(projects_dir).into_iter().filter_map(|e| e.ok()) {
        let path = walk_entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if uuid::Uuid::try_parse(stem).is_err() {
            continue;
        }
        let parent_name = path.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str());
        if parent_name != Some(stem) {
            continue;
        }
        let grandparent_name = path
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str());
        if grandparent_name != Some("agent-transcripts") {
            continue;
        }
        entries.push(AgentTranscriptPath {
            session_id: stem.to_string(),
            path: path.to_path_buf(),
            directory: agent_transcript_directory_from_path(path),
        });
    }
    entries
}

fn agent_transcript_directory_from_path(path: &Path) -> Option<String> {
    let project_key = path
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())?;
    cursor_project_key_to_existing_path(project_key)
}

fn cursor_project_key_to_existing_path(key: &str) -> Option<String> {
    let parts: Vec<&str> = key.split('-').filter(|part| !part.is_empty()).collect();
    if parts.is_empty() {
        return None;
    }

    let mut matches = Vec::new();
    collect_cursor_project_key_matches(Path::new("/"), &parts, 0, &mut matches);
    matches.sort();
    matches.dedup();
    if matches.len() == 1 { Some(matches.remove(0).to_string_lossy().to_string()) } else { None }
}

fn collect_cursor_project_key_matches(
    current: &Path,
    parts: &[&str],
    index: usize,
    matches: &mut Vec<PathBuf>,
) {
    if index == parts.len() {
        if current.is_dir() {
            matches.push(current.to_path_buf());
        }
        return;
    }

    for end in (index + 1..=parts.len()).rev() {
        let component = parts[index..end].join("-");
        let next = current.join(component);
        if next.is_dir() {
            collect_cursor_project_key_matches(&next, parts, end, matches);
        }
    }
}

fn resolve_projects_dir() -> anyhow::Result<Option<PathBuf>> {
    resolve_home_dir(".cursor/projects", "~/.cursor/projects not found, skipping Cursor")
}

fn global_state_db_candidate() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("Cursor/User/globalStorage/state.vscdb"))
}

fn resolve_global_state_db_path() -> Option<PathBuf> {
    global_state_db_candidate().filter(|path| path.exists())
}

fn resolve_workspace_storage_dir() -> Option<PathBuf> {
    let dir = dirs::config_dir()?.join("Cursor/User/workspaceStorage");
    if dir.exists() { Some(dir) } else { None }
}

fn open_global_db() -> anyhow::Result<Option<Connection>> {
    let Some(path) = resolve_global_state_db_path() else {
        debug!("Cursor global state DB not found, skipping composer sessions");
        return Ok(None);
    };
    Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map(Some)
    .map_err(Into::into)
}

fn global_db_mtime() -> Option<i64> {
    resolve_global_state_db_path().and_then(|path| stat_mtime_ms(&path))
}

fn build_agent_cwd_map(db_path: Option<&Path>) -> HashMap<String, String> {
    let Some(db_path) = db_path else {
        return HashMap::new();
    };
    match read_agent_cwd_map(db_path) {
        Ok(map) => map,
        Err(err) => {
            debug!("cursor cwd map unavailable from {}: {err}", db_path.display());
            HashMap::new()
        }
    }
}

fn read_agent_cwd_map(db_path: &Path) -> anyhow::Result<HashMap<String, String>> {
    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let mut project_to_path: HashMap<String, String> = HashMap::new();
    if let Some(projects_json) = read_item_value(&conn, "glass.localAgentProjects.v1")
        && let Ok(projects) = serde_json::from_str::<Value>(&projects_json)
        && let Some(arr) = projects.as_array()
    {
        for item in arr {
            let project_id = match item.get("id").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let fs_path = item
                .get("workspace")
                .and_then(|w| w.get("uri"))
                .and_then(|u| u.get("fsPath"))
                .and_then(|p| p.as_str());
            if let Some(fs_path) = fs_path {
                project_to_path.insert(project_id, fs_path.to_string());
            }
        }
    }

    let mut session_to_path: HashMap<String, String> = HashMap::new();
    if let Some(membership_json) = read_item_value(&conn, "glass.localAgentProjectMembership.v1")
        && let Ok(membership) = serde_json::from_str::<Value>(&membership_json)
        && let Some(obj) = membership.as_object()
    {
        for (session_id, project_val) in obj {
            let Some(project_id) = project_val.as_str() else {
                continue;
            };
            if let Some(path) = project_to_path.get(project_id) {
                session_to_path.insert(session_id.clone(), path.clone());
            }
        }
    }
    Ok(session_to_path)
}

fn read_item_value(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM ItemTable WHERE key = ?1", [key], |row| {
        row.get::<_, String>(0)
    })
    .ok()
}

pub(crate) fn read_content_evidence(
    composer_id: &str,
    bubble_id: &str,
    call_id: Option<&str>,
    attrs: &str,
    before: bool,
    remaining: &mut usize,
) -> anyhow::Result<String> {
    let path = global_state_db_candidate().ok_or_else(|| anyhow::anyhow!("source_unverified"))?;
    fs::metadata(&path).map_err(|error| {
        anyhow::anyhow!(if error.kind() == std::io::ErrorKind::NotFound {
            "source_missing"
        } else {
            "source_unverified"
        })
    })?;
    let canonical = path.canonicalize()?;
    anyhow::ensure!(
        canonical.parent()
            == Some(
                path.parent()
                    .ok_or_else(|| anyhow::anyhow!("source_unverified"))?
                    .canonicalize()?
                    .as_path()
            ),
        "source_unverified"
    );
    let conn = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let tx = conn.unchecked_transaction()?;
    content_evidence_from_conn(&tx, composer_id, bubble_id, call_id, attrs, before, remaining)
}

fn content_evidence_from_conn(
    conn: &Connection,
    composer_id: &str,
    bubble_id: &str,
    call_id: Option<&str>,
    attrs: &str,
    before: bool,
    remaining: &mut usize,
) -> anyhow::Result<String> {
    use sha2::{Digest, Sha256};
    anyhow::ensure!(
        uuid::Uuid::parse_str(composer_id).is_ok() && uuid::Uuid::parse_str(bubble_id).is_ok(),
        "source_unverified"
    );
    let mut read = |key: &str| -> anyhow::Result<String> {
        use rusqlite::OptionalExtension;
        let length: Option<usize> = conn
            .query_row(
                "SELECT length(CAST(value AS BLOB)) FROM cursorDiskKV WHERE key = ?1",
                [key],
                |row| row.get(0),
            )
            .optional()?;
        let length = length.ok_or_else(|| anyhow::anyhow!("source_missing"))?;
        anyhow::ensure!(length <= *remaining, "evidence_budget_exceeded");
        *remaining -= length;
        let bytes: Vec<u8> = conn.query_row(
            "SELECT CAST(value AS BLOB) FROM cursorDiskKV WHERE key = ?1",
            [key],
            |row| row.get(0),
        )?;
        Ok(String::from_utf8(bytes)?)
    };
    let composer: Value = serde_json::from_str(&read(&format!("composerData:{composer_id}"))?)?;
    let owns_bubble = composer
        .get("fullConversationHeadersOnly")
        .and_then(Value::as_array)
        .is_some_and(|headers| {
            headers
                .iter()
                .any(|header| header.get("bubbleId").and_then(Value::as_str) == Some(bubble_id))
        });
    anyhow::ensure!(owns_bubble, "source_changed");
    let key = format!("bubbleId:{composer_id}:{bubble_id}");
    let stored: bool =
        conn.query_row("SELECT EXISTS(SELECT 1 FROM cursorDiskKV WHERE key=?1)", [&key], |row| {
            row.get(0)
        })?;
    let bubble: Value = if stored {
        serde_json::from_str(&read(&key)?)?
    } else {
        composer
            .get("conversationMap")
            .and_then(|map| map.get(bubble_id))
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("source_missing"))?
    };
    let tool = bubble.get("toolFormerData").ok_or_else(|| anyhow::anyhow!("source_changed"))?;
    let native_call_id = cursor_native_id(tool.get("toolCallId").and_then(Value::as_str))
        .unwrap_or_else(|| bubble_id.into());
    anyhow::ensure!(call_id == Some(native_call_id.as_str()), "source_changed");
    let result = tool.get("result").ok_or_else(|| anyhow::anyhow!("source_changed"))?;
    let decoded = result.as_str().map(serde_json::from_str::<Value>).transpose()?;
    let result = decoded.as_ref().unwrap_or(result);
    let expected: Value = serde_json::from_str(attrs)?;
    anyhow::ensure!(*result == expected, "source_changed");
    let content_id = result
        .get(if before { "beforeContentId" } else { "afterContentId" })
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("content_reference_not_recorded"))?;
    let digest = content_id
        .strip_prefix("composer.content.")
        .filter(|digest| digest.len() == 64 && digest.bytes().all(|ch| ch.is_ascii_hexdigit()))
        .ok_or_else(|| anyhow::anyhow!("source_unverified"))?;
    let content = read(content_id)?;
    anyhow::ensure!(
        format!("{:x}", Sha256::digest(content.as_bytes())) == digest,
        "source_changed"
    );
    Ok(content)
}

fn read_disk_kv(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM cursorDiskKV WHERE key = ?1", [key], |row| {
        row.get::<_, String>(0)
    })
    .ok()
}

fn stat_birth_ms(path: &Path) -> Option<i64> {
    let meta = fs::metadata(path).ok()?;
    let created = meta.created().ok()?;
    let duration = created.duration_since(UNIX_EPOCH).ok()?;
    Some(duration.as_millis() as i64)
}

fn stat_mtime_ms(path: &Path) -> Option<i64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    Some(modified.duration_since(UNIX_EPOCH).ok()?.as_millis() as i64)
}

fn non_empty_str(value: Option<&Value>) -> Option<&str> {
    value.and_then(|value| value.as_str()).filter(|value| !value.trim().is_empty())
}

fn render_json_fragment(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        _ => serde_json::to_string(value).ok(),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use rusqlite::Connection;

    use super::*;
    use crate::db::schema;
    use crate::db::store::Store;
    use crate::types::{Session, TokenSource};

    fn temp_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "recall-cursor-test-{}-{}",
            label,
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_jsonl(path: &Path, lines: &[&str]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut f = fs::File::create(path).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
    }

    fn cursor_project_key_for_test(path: &Path) -> String {
        path.components()
            .filter_map(|component| match component {
                std::path::Component::Normal(part) => part.to_str(),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("-")
    }

    fn seed_global_db(root: &Path, composer_id: &str, bubble_id: &str) -> Connection {
        let db_path = root.join("state.vscdb");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )
        .unwrap();

        let headers = serde_json::json!({
            "allComposers": [{
                "composerId": composer_id,
                "name": "Usage review",
                "unifiedMode": "chat",
                "createdAt": 1_700_000_000_000_i64,
                "lastUpdatedAt": 1_700_000_100_000_i64,
                "workspaceIdentifier": {
                    "uri": { "fsPath": "/Users/x/project" }
                }
            }]
        });
        conn.execute(
            "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
            ["composer.composerHeaders", &headers.to_string()],
        )
        .unwrap();

        let composer_data = serde_json::json!({
            "composerId": composer_id,
            "createdAt": 1_700_000_000_000_i64,
            "lastUpdatedAt": 1_700_000_100_000_i64,
            "unifiedMode": "chat",
            "modelConfig": { "modelName": "claude-sonnet-4" },
            "promptTokenBreakdown": {
                "totalUsedTokens": 1200,
                "categories": [
                    { "id": "conversation", "estimatedTokens": 300 }
                ]
            },
            "fullConversationHeadersOnly": [
                { "bubbleId": bubble_id, "type": 1 },
            ]
        });
        conn.execute(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
            [format!("composerData:{composer_id}"), composer_data.to_string()],
        )
        .unwrap();

        let bubble = serde_json::json!({
            "type": 1,
            "text": "<user_query>\nhello cursor\n</user_query>",
            "createdAt": 1_700_000_000_000_i64,
            "tokenCount": { "inputTokens": 0, "outputTokens": 0 }
        });
        conn.execute(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
            [format!("bubbleId:{composer_id}:{bubble_id}"), bubble.to_string()],
        )
        .unwrap();

        conn
    }

    #[test]
    fn strip_user_query_envelope_strips_wrapper() {
        let text = "<user_query>\nhello world\n</user_query>";
        assert_eq!(strip_user_query_envelope(text).trim(), "hello world");
    }

    #[test]
    fn render_bubble_content_preserves_discussion_and_code() {
        let bubble = serde_json::json!({
            "text": "Investigating usage",
            "codeBlocks": [{"content":"let total = 1;"}],
            "toolFormerData": {
                "name": "grep",
                "rawArgs": "{\"pattern\":\"usage\"}",
                "result": "{\"matches\":1}"
            }
        });
        let rendered = render_bubble_content(&bubble, &Role::Assistant);
        assert_eq!(rendered, "Investigating usage\n[code_block] let total = 1;");
    }

    #[test]
    fn parse_composer_session_extracts_messages_and_usage() {
        let root = temp_root("composer");
        let composer_id = uuid::Uuid::new_v4().to_string();
        let bubble_id = uuid::Uuid::new_v4().to_string();
        let conn = seed_global_db(&root, &composer_id, &bubble_id);
        let meta = load_composer_meta(&conn, &composer_id, &ComposerLookup::load(&conn));
        let parsed = parse_composer_session(&conn, &composer_id, &meta, false).unwrap().unwrap();
        assert_eq!(parsed.messages.len(), 1);
        assert_eq!(parsed.messages[0].content, "hello cursor");
        assert_eq!(parsed.usage_events.len(), 1);
        assert_eq!(parsed.usage_events[0].token_source, TokenSource::Derived);
        assert_eq!(parsed.usage_events[0].input_tokens, 900);
        assert_eq!(parsed.usage_events[0].cache_read_tokens, 300);
        assert_eq!(parsed.usage_events[0].output_tokens, 0);
        assert_eq!(parsed.directory.as_deref(), Some("/Users/x/project"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn build_raw_session_attaches_matching_transcript_path() {
        let root = temp_root("composer-path");
        let composer_id = uuid::Uuid::new_v4().to_string();
        let bubble_id = uuid::Uuid::new_v4().to_string();
        let transcript_path = root.join(format!("{composer_id}.jsonl"));
        let conn = seed_global_db(&root, &composer_id, &bubble_id);
        let meta = load_composer_meta(&conn, &composer_id, &ComposerLookup::load(&conn));
        let transcript_paths = HashMap::from([(
            composer_id.clone(),
            AgentTranscriptPath {
                session_id: composer_id.clone(),
                path: transcript_path.clone(),
                directory: None,
            },
        )]);

        let raw = build_raw_session(&conn, &composer_id, &meta, &transcript_paths, false)
            .unwrap()
            .unwrap();

        assert_eq!(raw.source_file_path.as_deref(), transcript_path.to_str());
        schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let context = AdapterSyncContext::from_store_for_test(&store, "cursor").unwrap();
        conn.execute(
            "DELETE FROM cursorDiskKV WHERE key = ?1",
            [format!("composerData:{composer_id}")],
        )
        .unwrap();
        write_jsonl(
            &transcript_path,
            &[
                r#"{"role":"assistant","message":{"content":[{"type":"text","text":"Retained transcript"}]}}"#,
            ],
        );
        let mut metadata = HashMap::new();
        let scanned =
            scan_for_sync_conn(&conn, &context, None, false, &transcript_paths, &mut metadata)
                .unwrap();
        assert!(scanned.sessions.is_empty());
        let fallback = scan_transcripts_for_sync(
            &context,
            None,
            false,
            &transcript_paths,
            &HashSet::new(),
            &metadata,
        )
        .unwrap();
        assert_eq!(fallback.sessions.len(), 1);
        assert_eq!(fallback.sessions[0].directory.as_deref(), Some("/Users/x/project"));
        assert_eq!(fallback.sessions[0].entrypoint.as_deref(), Some("chat"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn incremental_scan_returns_current_session_when_transcript_path_needs_backfill() {
        schema::register_sqlite_vec();
        let root = temp_root("path-backfill");
        let composer_id = uuid::Uuid::new_v4().to_string();
        let bubble_id = uuid::Uuid::new_v4().to_string();
        let transcript_path = root.join(format!("{composer_id}.jsonl"));
        let conn = seed_global_db(&root, &composer_id, &bubble_id);
        let store = Store::open_in_memory().unwrap();
        let source_updated_at = 1_700_000_100_000_i64;
        store
            .insert_session(&Session {
                id: uuid::Uuid::new_v4().to_string(),
                source: "cursor".to_string(),
                source_id: composer_id.clone(),
                title: "Usage review".to_string(),
                directory: Some("/Users/x/project".to_string()),
                repo_remote: None,
                repo_slug: None,
                repo_name: None,
                started_at: 1_700_000_000_000,
                updated_at: Some(source_updated_at),
                message_count: 1,
                entrypoint: Some("chat".to_string()),
                custom_title: None,
                summary: None,
                duration_minutes: None,
                source_file_path: None,
                is_import: false,
            })
            .unwrap();
        store
            .persist_usage_events_for_existing_session(
                "cursor",
                &composer_id,
                &[],
                USAGE_PARSER_VERSION,
                Some(source_updated_at),
            )
            .unwrap();
        let transcript_paths = HashMap::from([(
            composer_id.clone(),
            AgentTranscriptPath {
                session_id: composer_id,
                path: transcript_path.clone(),
                directory: None,
            },
        )]);

        let result = scan_for_sync_conn(
            &conn,
            &AdapterSyncContext::from_store_for_test(&store, "cursor").unwrap(),
            Some(source_updated_at + 1),
            false,
            &transcript_paths,
            &mut HashMap::new(),
        )
        .unwrap();

        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].source_file_path.as_deref(), transcript_path.to_str());
        assert_eq!(store.session_paths_for_source("cursor").unwrap()[0].source_file_path, None);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn incremental_scan_reparses_current_session_with_stale_event_parser() {
        schema::register_sqlite_vec();
        let root = temp_root("event-parser-backfill");
        let composer_id = uuid::Uuid::new_v4().to_string();
        let bubble_id = uuid::Uuid::new_v4().to_string();
        let conn = seed_global_db(&root, &composer_id, &bubble_id);
        let store = Store::open_in_memory().unwrap();
        let source_updated_at = 1_700_000_100_000_i64;
        store
            .insert_session(&Session {
                id: uuid::Uuid::new_v4().to_string(),
                source: "cursor".to_string(),
                source_id: composer_id.clone(),
                title: "Usage review".to_string(),
                directory: Some("/Users/x/project".to_string()),
                repo_remote: None,
                repo_slug: None,
                repo_name: None,
                started_at: 1_700_000_000_000,
                updated_at: Some(source_updated_at),
                message_count: 1,
                entrypoint: Some("chat".to_string()),
                custom_title: None,
                summary: None,
                duration_minutes: None,
                source_file_path: None,
                is_import: false,
            })
            .unwrap();
        store
            .persist_usage_events_for_existing_session(
                "cursor",
                &composer_id,
                &[],
                USAGE_PARSER_VERSION,
                Some(source_updated_at),
            )
            .unwrap();
        store
            .persist_session_events_for_existing_session(
                "cursor",
                &composer_id,
                &[],
                EVENT_PARSER_VERSION - 1,
                Some(source_updated_at),
            )
            .unwrap();

        let result = scan_for_sync_conn(
            &conn,
            &AdapterSyncContext::from_store_for_test(&store, "cursor").unwrap(),
            None,
            true,
            &HashMap::new(),
            &mut HashMap::new(),
        )
        .unwrap();

        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.stats.skipped_sessions, 0);
        assert_eq!(result.sessions[0].event_parser_version, Some(EVENT_PARSER_VERSION));
        assert_eq!(result.sessions[0].messages.len(), 1);
        assert_eq!(result.sessions[0].messages[0].content, "hello cursor");

        let meta = load_composer_meta(&conn, &composer_id, &ComposerLookup::load(&conn));
        let mut stale =
            build_raw_session(&conn, &composer_id, &meta, &HashMap::new(), true).unwrap().unwrap();
        stale.metadata_parser_version = Some(METADATA_PARSER_VERSION - 1);
        stale.messages[0].content.push_str("\n[tool:Shell] stale payload");
        let store =
            crate::sync::persist_raw_session_for_conformance(store, "cursor", stale).unwrap();
        let mut reparsed = scan_for_sync_conn(
            &conn,
            &AdapterSyncContext::from_store_for_test(&store, "cursor").unwrap(),
            None,
            true,
            &HashMap::new(),
            &mut HashMap::new(),
        )
        .unwrap();
        assert_eq!(reparsed.sessions.len(), 1);
        assert_eq!(reparsed.sessions[0].messages.len(), 1);
        assert!(reparsed.sessions[0].refresh_session_on_metadata_backfill);
        let store = crate::sync::persist_raw_session_for_conformance(
            store,
            "cursor",
            reparsed.sessions.remove(0),
        )
        .unwrap();
        let stored = store.get_session_by_source_id("cursor", &composer_id).unwrap().unwrap();
        assert_eq!(store.get_messages(&stored.id).unwrap()[0].content, "hello cursor");
        let current = scan_for_sync_conn(
            &conn,
            &AdapterSyncContext::from_store_for_test(&store, "cursor").unwrap(),
            None,
            true,
            &HashMap::new(),
            &mut HashMap::new(),
        )
        .unwrap();
        assert!(current.sessions.is_empty());
        assert_eq!(current.stats.skipped_sessions, 1);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn parse_composer_session_prefers_bubble_usage_over_context_breakdown() {
        let root = temp_root("bubble-usage");
        let composer_id = uuid::Uuid::new_v4().to_string();
        let bubble_id = uuid::Uuid::new_v4().to_string();
        let conn = seed_global_db(&root, &composer_id, &bubble_id);
        let composer_data = serde_json::json!({
            "composerId": composer_id,
            "createdAt": 1_700_000_000_000_i64,
            "lastUpdatedAt": 1_700_000_100_000_i64,
            "unifiedMode": "chat",
            "modelConfig": { "modelName": "claude-sonnet-4" },
            "promptTokenBreakdown": {
                "totalUsedTokens": 1200,
                "categories": [{ "id": "conversation", "estimatedTokens": 300 }]
            },
            "fullConversationHeadersOnly": [
                { "bubbleId": bubble_id, "type": 2 },
            ]
        });
        conn.execute(
            "UPDATE cursorDiskKV SET value = ?1 WHERE key = ?2",
            rusqlite::params![composer_data.to_string(), format!("composerData:{composer_id}"),],
        )
        .unwrap();
        let bubble = serde_json::json!({
            "type": 2,
            "text": "assistant reply",
            "createdAt": 1_700_000_050_000_i64,
            "tokenCount": { "inputTokens": 12, "outputTokens": 34 }
        });
        conn.execute(
            "UPDATE cursorDiskKV SET value = ?1 WHERE key = ?2",
            rusqlite::params![bubble.to_string(), format!("bubbleId:{composer_id}:{bubble_id}"),],
        )
        .unwrap();

        let meta = load_composer_meta(&conn, &composer_id, &ComposerLookup::load(&conn));
        let parsed = parse_composer_session(&conn, &composer_id, &meta, false).unwrap().unwrap();
        assert_eq!(parsed.usage_events.len(), 1);
        assert_eq!(parsed.usage_events[0].token_source, TokenSource::Observed);
        assert_eq!(parsed.usage_events[0].input_tokens, 12);
        assert_eq!(parsed.usage_events[0].output_tokens, 34);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn parse_composer_session_extracts_tool_events() {
        let root = temp_root("tool-events");
        let composer_id = uuid::Uuid::new_v4().to_string();
        let ids: Vec<_> = (0..4).map(|_| uuid::Uuid::new_v4().to_string()).collect();
        let conn = seed_global_db(&root, &composer_id, &ids[0]);
        let data = serde_json::json!({
            "createdAt": 1_700_000_000_000_i64,
            "lastUpdatedAt": 1_700_000_100_000_i64,
            "fullConversationHeadersOnly": [
                {"bubbleId":ids[0],"type":2},
                {"bubbleId":ids[1],"type":1},
                {"bubbleId":ids[2],"type":2},
                {"bubbleId":ids[3],"type":2}
            ]
        });
        conn.execute(
            "UPDATE cursorDiskKV SET value=?1 WHERE key=?2",
            rusqlite::params![data.to_string(), format!("composerData:{composer_id}")],
        )
        .unwrap();
        for (index, id) in ids.iter().enumerate() {
            let bubble = if index == 1 {
                serde_json::json!({"text":"Move this file"})
            } else {
                serde_json::json!({
                    "text":if index == 3 {"Applying the next change"} else {""},
                    "createdAt":1_700_000_050_000_i64,
                    "tokenCount":{"inputTokens":12,"outputTokens":34},
                    "toolFormerData":{
                        "name":"run_terminal_command_v2",
                        "params":{"command":"mv old.rs new.rs","cwd":"/native/repo"},
                        "result":{"exitCode":0},
                        "status":"completed"
                    }
                })
            };
            conn.execute(
                "INSERT OR REPLACE INTO cursorDiskKV (key,value) VALUES (?1,?2)",
                rusqlite::params![format!("bubbleId:{composer_id}:{id}"), bubble.to_string()],
            )
            .unwrap();
        }
        let meta = load_composer_meta(&conn, &composer_id, &ComposerLookup::load(&conn));
        let parsed = parse_composer_session(&conn, &composer_id, &meta, true).unwrap().unwrap();
        assert_eq!(
            parsed.messages.iter().map(|message| message.content.as_str()).collect::<Vec<_>>(),
            ["Move this file", "Applying the next change"]
        );
        assert_eq!(parsed.events.len(), 6);
        assert_eq!(parsed.usage_events.len(), 3);
        for (index, anchor) in [None, Some(0), Some(1)].into_iter().enumerate() {
            let call = &parsed.events[index * 2];
            let result = &parsed.events[index * 2 + 1];
            assert_eq!(call.message_seq, anchor);
            assert_eq!(result.message_seq, anchor);
            assert_eq!(parsed.usage_events[index].message_seq, anchor);
            assert_eq!(call.tool_call_id, result.tool_call_id);
            assert_eq!(call.source_event_id, result.source_event_id);
            assert_eq!(call.files.len(), 2);
            assert!(call.files.iter().all(|file| file.kind == FileEvidenceKind::Command
                && file.cwd.as_deref() == Some("/native/repo")));
            assert_eq!(call.status, None);
            assert_eq!(result.status.as_deref(), Some("completed"));
        }
        let no_events = parse_composer_session(&conn, &composer_id, &meta, false).unwrap().unwrap();
        assert_eq!(no_events.messages.len(), 2);
        assert_eq!(no_events.usage_events.len(), 3);
        assert!(no_events.events.is_empty());
        let mut tool_only = data.clone();
        tool_only["fullConversationHeadersOnly"] =
            serde_json::json!([{"bubbleId":ids[0],"type":2}]);
        conn.execute(
            "UPDATE cursorDiskKV SET value=?1 WHERE key=?2",
            rusqlite::params![tool_only.to_string(), format!("composerData:{composer_id}")],
        )
        .unwrap();
        conn.execute(
            "UPDATE cursorDiskKV SET value=json_remove(value,'$.tokenCount') WHERE key=?1",
            [format!("bubbleId:{composer_id}:{}", ids[0])],
        )
        .unwrap();
        let empty = parse_composer_session(&conn, &composer_id, &meta, false).unwrap().unwrap();
        assert!(
            empty.messages.is_empty() && empty.events.is_empty() && empty.usage_events.is_empty()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn composer_tool_events_do_not_invent_missing_bubble_relationships() {
        let bubble = serde_json::json!({
            "toolFormerData": {
                "name": "grep",
                "rawArgs": "{\"pattern\":\"usage\"}",
                "result": "match"
            }
        });
        for bubble_id in [None, Some(""), Some("unknown")] {
            let mut events = Vec::new();
            collect_bubble_tool_events(
                &bubble,
                bubble_id,
                "composer:test",
                None,
                Some(0),
                &mut events,
            );
            assert_eq!(events.len(), 2);
            assert!(events.iter().all(|event| event.source_event_id.is_none()));
            assert!(events.iter().all(|event| event.tool_call_id.is_none()));
        }
    }

    #[test]
    fn composer_file_evidence_decodes_native_arguments_and_keeps_content_ids() {
        for (name, key, operation) in [
            ("edit_file_v2", "relativeWorkspacePath", FileOperation::Write),
            ("read_file_v2", "targetFile", FileOperation::Read),
        ] {
            let args = serde_json::json!({key: "src/file.rs"});
            for params in [args.clone(), Value::String(args.to_string())] {
                let bubble = serde_json::json!({"toolFormerData": {
                    "name": name,
                    "toolCallId": "native-call",
                    "status": "completed",
                    "params": params,
                    "result": "{\"beforeContentId\":\"composer.content.before\",\"afterContentId\":\"composer.content.after\"}"
                }});
                let mut events = Vec::new();
                collect_bubble_tool_events(
                    &bubble,
                    Some("record-id"),
                    "composer:test",
                    None,
                    Some(0),
                    &mut events,
                );
                assert_eq!(events.len(), 2);
                assert_eq!(events[0].files[0].path, "src/file.rs");
                assert_eq!(events[0].files[0].operation, operation);
                assert_eq!(events[0].source_event_id.as_deref(), Some("record-id"));
                assert!(
                    events.iter().all(|event| event.tool_call_id.as_deref() == Some("native-call"))
                );
                let result: Value =
                    serde_json::from_str(events[1].attrs_json.as_deref().unwrap()).unwrap();
                assert_eq!(result["afterContentId"], "composer.content.after");
                assert_eq!(events[1].status.as_deref(), Some("completed"));
            }
        }
        for (name, args, expected_cwd) in [
            (
                "Shell",
                serde_json::json!({"command":"mv old.rs new.rs","working_directory":"/explicit/repo"}),
                Some("/explicit/repo"),
            ),
            ("Shell", serde_json::json!({"command":"mv old.rs new.rs"}), None),
            (
                "run_terminal_command_v2",
                serde_json::json!({"command":"mv old.rs new.rs","cwd":"$WORKSPACE"}),
                None,
            ),
            (
                "run_terminal_command_v2",
                serde_json::json!({"command":"mv old.rs new.rs","cwd":null}),
                None,
            ),
        ] {
            let bubble =
                serde_json::json!({"toolFormerData":{"name":name,"params":args.to_string()}});
            let mut events = Vec::new();
            collect_bubble_tool_events(
                &bubble,
                Some("bubble"),
                "composer:test",
                None,
                None,
                &mut events,
            );
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].files.len(), 2);
            assert!(events[0].files.iter().all(|file| file.kind == FileEvidenceKind::Command
                && file.cwd.as_deref() == expected_cwd));
            assert_eq!(events[0].status, None);
            assert_eq!(
                serde_json::from_str::<Value>(events[0].attrs_json.as_deref().unwrap()).unwrap(),
                args
            );
            if expected_cwd.is_none() {
                assert_eq!(
                    events[0].command_evidence_status,
                    Some(crate::types::CommandEvidenceStatus::Unsupported)
                );
            }
        }
    }

    #[test]
    fn incremental_scan_includes_uncovered_transcripts_without_a_composer_database() {
        schema::register_sqlite_vec();
        let root = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let context = AdapterSyncContext::from_store_for_test(&store, "cursor").unwrap();
        let mut transcripts = HashMap::new();
        for id in ["covered", "orphan"] {
            let path = root.path().join(format!("{id}.jsonl"));
            write_jsonl(
                &path,
                &[
                    r#"{"role":"assistant","message":{"content":[{"type":"tool_use","id":"edit","name":"StrReplace","input":{"path":"src/file.rs","old_string":"old","new_string":"new"}}]}}"#,
                    r#"{"role":"assistant","message":{"content":[{"type":"tool_use","id":"patch","name":"ApplyPatch","input":"*** Begin Patch\n*** Delete File: src/file.rs\n*** Add File: src/file.rs\n+replacement\n*** Update File: old.rs\n*** Move to: new.rs\n@@\n-old\n+new\n*** End Patch"}]}}"#,
                    r#"{"role":"user","message":{"content":[{"type":"tool_result","tool_use_id":"patch","is_error":true,"content":"failed"}]}}"#,
                ],
            );
            transcripts.insert(
                id.into(),
                AgentTranscriptPath {
                    session_id: id.into(),
                    path,
                    directory: Some("/repo".into()),
                },
            );
        }
        let transcript_meta = HashMap::from([(
            "orphan".into(),
            ComposerMeta {
                directory: Some("/metadata/repo".into()),
                unified_mode: Some("chat".into()),
                ..Default::default()
            },
        )]);
        for covered in [HashSet::new(), HashSet::from(["covered".into()])] {
            let result = scan_transcripts_for_sync(
                &context,
                None,
                true,
                &transcripts,
                &covered,
                &transcript_meta,
            )
            .unwrap();
            assert_eq!(result.sessions.len(), 2 - covered.len());
            assert!(result.sessions.iter().all(|session| !covered.contains(&session.source_id)));
            for session in result.sessions {
                let orphan = session.source_id == "orphan";
                assert_eq!(
                    session.directory.as_deref(),
                    Some(if orphan { "/metadata/repo" } else { "/repo" })
                );
                assert_eq!(
                    session.entrypoint.as_deref(),
                    Some(if orphan { "chat" } else { "agent" })
                );
                assert_eq!(session.events[0].files[0].operation, FileOperation::Write);
                assert_eq!(
                    session.events[0].files[0].cwd.as_deref(),
                    orphan.then_some("/metadata/repo")
                );
                assert_eq!(session.events.len(), 3);
                assert_eq!(session.events[0].status, None);
                assert_eq!(
                    session.events[1]
                        .files
                        .iter()
                        .map(|file| (file.path.as_str(), file.operation.clone()))
                        .collect::<Vec<_>>(),
                    [
                        ("src/file.rs", FileOperation::Delete),
                        ("src/file.rs", FileOperation::Write),
                        ("old.rs", FileOperation::MoveFrom),
                        ("new.rs", FileOperation::MoveTo)
                    ]
                );
                assert_eq!(session.events[1].tool_call_id.as_deref(), Some("patch"));
                assert_eq!(session.events[1].status, None);
                assert_eq!(session.events[2].tool_call_id.as_deref(), Some("patch"));
                assert_eq!(session.events[2].status.as_deref(), Some("error"));
                assert!(session.events[2].files.is_empty());
            }
        }
    }

    #[test]
    fn parse_composer_legacy_map_preserves_messages_and_tool_evidence() {
        let root = temp_root("legacy-map");
        let composer_id = uuid::Uuid::new_v4().to_string();
        let bubble_id = uuid::Uuid::new_v4().to_string();
        let conn = seed_global_db(&root, &composer_id, &bubble_id);
        conn.execute(
            "DELETE FROM cursorDiskKV WHERE key = ?1",
            [format!("bubbleId:{composer_id}:{bubble_id}")],
        )
        .unwrap();
        let composer_data = serde_json::json!({
            "composerId": composer_id,
            "createdAt": 1_700_000_000_000_i64,
            "lastUpdatedAt": 1_700_000_100_000_i64,
            "fullConversationHeadersOnly": [{"bubbleId": bubble_id, "type": 2}],
            "conversationMap": {
                bubble_id.clone(): {
                    "text": "Legacy assistant message",
                    "toolFormerData": {
                        "name": "grep",
                        "rawArgs": "{\"pattern\":\"usage\"}"
                    }
                }
            }
        });
        conn.execute(
            "UPDATE cursorDiskKV SET value = ?1 WHERE key = ?2",
            rusqlite::params![composer_data.to_string(), format!("composerData:{composer_id}")],
        )
        .unwrap();

        let meta = load_composer_meta(&conn, &composer_id, &ComposerLookup::load(&conn));
        let parsed = parse_composer_session(&conn, &composer_id, &meta, true).unwrap().unwrap();

        assert_eq!(parsed.messages.len(), 1);
        assert_eq!(parsed.messages[0].content, "Legacy assistant message");
        assert_eq!(parsed.events.len(), 1);
        assert_eq!(parsed.events[0].source_event_id.as_deref(), Some(bubble_id.as_str()));
        assert_eq!(parsed.events[0].tool_call_id.as_deref(), Some(bubble_id.as_str()));
        assert_eq!(parsed.events[0].message_seq, Some(0));
        let mut tool_only = composer_data.clone();
        tool_only["conversationMap"][&bubble_id]["text"] = serde_json::json!("");
        conn.execute(
            "UPDATE cursorDiskKV SET value=?1 WHERE key=?2",
            rusqlite::params![tool_only.to_string(), format!("composerData:{composer_id}")],
        )
        .unwrap();
        let parsed = parse_composer_session(&conn, &composer_id, &meta, true).unwrap().unwrap();
        assert!(parsed.messages.is_empty());
        assert_eq!(parsed.events.len(), 1);
        assert_eq!(parsed.events[0].source_event_id.as_deref(), Some(bubble_id.as_str()));
        assert_eq!(parsed.events[0].message_seq, None);

        tool_only["conversationMap"][&bubble_id]["toolFormerData"] = serde_json::json!({
            "name": "edit_file_v2", "params": {"relativeWorkspacePath": "src/file.rs"}
        });
        conn.execute(
            "UPDATE cursorDiskKV SET value=?1 WHERE key=?2",
            rusqlite::params![tool_only.to_string(), format!("composerData:{composer_id}")],
        )
        .unwrap();
        let parsed = parse_composer_session(&conn, &composer_id, &meta, true).unwrap().unwrap();
        assert_eq!(parsed.events[0].files[0].cwd.as_deref(), Some("/Users/x/project"));
        let unknown = parse_composer_session(&conn, &composer_id, &ComposerMeta::default(), true)
            .unwrap()
            .unwrap();
        assert!(unknown.events[0].files[0].cwd.is_none());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn parse_agent_transcript_extracts_tool_use_events() {
        let root = temp_root("transcript-events");
        let uuid = uuid::Uuid::new_v4().to_string();
        let jsonl_path = root.join(format!("{uuid}.jsonl"));
        write_jsonl(
            &jsonl_path,
            &[
                r#"{"role":"user","message":{"content":[{"type":"text","text":"<user_query>\nhello\n</user_query>"}]}}"#,
                r#"{"role":"assistant","message":{"content":[{"type":"tool_use","id":"tool-before","name":"Glob","input":{"glob_pattern":"*.toml"}},{"type":"text","text":"searching"},{"type":"tool_use","id":"tool-after","name":"Glob","input":{"glob_pattern":"*.rs"}},{"type":"tool_use","id":"","name":"Glob","input":{"glob_pattern":"*.md"}},{"type":"tool_use","id":"unknown","name":"Glob","input":{"glob_pattern":"*.json"}},{"type":"tool_use","id":42,"name":"Glob","input":{"glob_pattern":"*.lock"}}]}}"#,
            ],
        );
        let raw = parse_agent_transcript(&jsonl_path, true).unwrap().unwrap();
        assert_eq!(raw.events.len(), 5);
        assert_eq!(raw.events[0].kind, "search");
        assert_eq!(raw.events[0].name.as_deref(), Some("Glob"));
        assert_eq!(raw.events[0].source_event_id.as_deref(), Some("1:0"));
        assert_eq!(raw.events[0].message_seq, Some(0));
        assert_eq!(raw.events[0].tool_call_id.as_deref(), Some("tool-before"));
        assert_eq!(raw.events[1].source_event_id.as_deref(), Some("1:2"));
        assert_eq!(raw.events[1].message_seq, Some(1));
        assert_eq!(raw.events[1].tool_call_id.as_deref(), Some("tool-after"));
        assert_eq!(raw.events[2].tool_call_id, None);
        assert_eq!(raw.events[3].tool_call_id, None);
        assert_eq!(raw.events[4].tool_call_id, None);
        assert_eq!(raw.event_parser_version, Some(EVENT_PARSER_VERSION));
        assert_eq!(raw.messages[1].content, "searching");
        write_jsonl(
            &jsonl_path,
            &[
                r#"{"role":"assistant","message":{"content":[{"type":"tool_use","id":"shell","name":"Shell","input":{"command":"mv old.rs new.rs"}}]}}"#,
            ],
        );
        let meta =
            ComposerMeta { directory: Some("/native/transcript".into()), ..Default::default() };
        let native =
            load_transcript(&jsonl_path, &uuid, None, Some(&meta), 1, true).unwrap().unwrap();
        assert!(native.messages.is_empty());
        assert_eq!(native.events.len(), 1);
        assert_eq!(native.events[0].message_seq, None);
        assert!(native.events[0].files.iter().all(|file| file.cwd.is_none()));
        let empty = parse_agent_transcript(&jsonl_path, false).unwrap().unwrap();
        assert!(empty.messages.is_empty() && empty.events.is_empty());
        assert!(empty.refresh_session_on_metadata_backfill);
        let unknown = parse_agent_transcript(&jsonl_path, true).unwrap().unwrap();
        assert!(unknown.events[0].files.iter().all(|file| file.cwd.is_none()));
        assert_eq!(
            unknown.events[0].command_evidence_status,
            Some(crate::types::CommandEvidenceStatus::Unsupported)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn parse_agent_transcript_happy_path() {
        let root = temp_root("parse");
        let uuid = uuid::Uuid::new_v4().to_string();
        let jsonl_path = root.join(format!("{uuid}.jsonl"));
        write_jsonl(
            &jsonl_path,
            &[
                r#"{"role":"user","message":{"content":[{"type":"text","text":"<user_query>\nhello\n</user_query>"}]}}"#,
                r#"{"role":"assistant","message":{"content":[{"type":"text","text":"hi"},{"type":"tool_use","name":"Glob","input":{"glob_pattern":"*.rs"}}]}}"#,
            ],
        );
        let raw = parse_agent_transcript(&jsonl_path, true).unwrap().unwrap();
        assert_eq!(raw.messages.len(), 2);
        assert_eq!(raw.messages[0].content, "hello");
        assert_eq!(raw.messages[1].content, "hi");
        assert_eq!(raw.metadata_parser_version, Some(METADATA_PARSER_VERSION));
        assert!(raw.refresh_session_on_metadata_backfill);
        assert_eq!(raw.usage_parser_version, Some(USAGE_PARSER_VERSION));
        assert!(raw.usage_events.is_empty());
        assert_eq!(raw.source_file_path.as_deref(), jsonl_path.to_str());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn agent_transcript_directory_uses_project_folder_when_membership_is_missing() {
        let root = temp_root("agent-directory");
        let workspace = root.join("workspace-parent").join("repo-with-dash");
        fs::create_dir_all(&workspace).unwrap();
        let projects_dir = root.join(".cursor").join("projects");
        let uuid = uuid::Uuid::new_v4().to_string();
        let project_key = cursor_project_key_for_test(&workspace);
        let jsonl_path = projects_dir
            .join(project_key)
            .join("agent-transcripts")
            .join(&uuid)
            .join(format!("{uuid}.jsonl"));
        write_jsonl(
            &jsonl_path,
            &[r#"{"role":"user","message":{"content":[{"type":"text","text":"hello"}]}}"#],
        );

        let entries = collect_agent_transcript_paths_from_dir(&projects_dir);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id, uuid);
        assert_eq!(entries[0].directory.as_deref(), Some(workspace.to_string_lossy().as_ref()));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn infer_cursor_provider_maps_models() {
        assert_eq!(infer_cursor_provider("claude-sonnet-4"), "anthropic");
        assert_eq!(infer_cursor_provider("composer-2.5"), "cursor");
        assert_eq!(infer_cursor_provider("gpt-4.1"), "openai");
    }

    #[test]
    fn content_evidence_verifies_ownership_hash_and_read_budget() {
        use sha2::{Digest, Sha256};
        let root = temp_root("content-evidence");
        let composer = uuid::Uuid::new_v4().to_string();
        let bubble = uuid::Uuid::new_v4().to_string();
        let conn = seed_global_db(&root, &composer, &bubble);
        let content = "原始文件\n";
        let content_id = format!("composer.content.{:x}", Sha256::digest(content.as_bytes()));
        let attrs = serde_json::json!({"beforeContentId":content_id,"afterContentId":content_id})
            .to_string();
        let native = serde_json::json!({"toolFormerData":{"toolCallId":"call-id","result":attrs}});
        conn.execute(
            "UPDATE cursorDiskKV SET value=?1 WHERE key=?2",
            rusqlite::params![native.to_string(), format!("bubbleId:{composer}:{bubble}")],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO cursorDiskKV(key,value) VALUES(?1,?2)",
            rusqlite::params![content_id, content],
        )
        .unwrap();
        assert_eq!(
            content_evidence_from_conn(
                &conn,
                &composer,
                &bubble,
                Some("call-id"),
                &attrs,
                true,
                &mut 65536
            )
            .unwrap(),
            content
        );
        let key = format!("composerData:{composer}");
        let mut mapped: Value = serde_json::from_str(&read_disk_kv(&conn, &key).unwrap()).unwrap();
        mapped["conversationMap"] = serde_json::json!({bubble.clone():native});
        conn.execute(
            "UPDATE cursorDiskKV SET value=?1 WHERE key=?2",
            rusqlite::params![mapped.to_string(), key],
        )
        .unwrap();
        conn.execute(
            "DELETE FROM cursorDiskKV WHERE key=?1",
            [format!("bubbleId:{composer}:{bubble}")],
        )
        .unwrap();
        assert_eq!(
            content_evidence_from_conn(
                &conn,
                &composer,
                &bubble,
                Some("call-id"),
                &attrs,
                true,
                &mut 65536
            )
            .unwrap(),
            content
        );
        let mut orphaned = mapped.clone();
        orphaned["fullConversationHeadersOnly"] = serde_json::json!([]);
        conn.execute(
            "UPDATE cursorDiskKV SET value=?1 WHERE key=?2",
            rusqlite::params![orphaned.to_string(), key],
        )
        .unwrap();
        assert_eq!(
            content_evidence_from_conn(
                &conn,
                &composer,
                &bubble,
                Some("call-id"),
                &attrs,
                true,
                &mut 65536
            )
            .unwrap_err()
            .to_string(),
            "source_changed"
        );
        conn.execute(
            "UPDATE cursorDiskKV SET value=?1 WHERE key=?2",
            rusqlite::params![mapped.to_string(), key],
        )
        .unwrap();
        assert_eq!(
            content_evidence_from_conn(
                &conn,
                &composer,
                &bubble,
                Some("call-id"),
                &attrs,
                true,
                &mut 1
            )
            .unwrap_err()
            .to_string(),
            "evidence_budget_exceeded"
        );
        assert_eq!(
            content_evidence_from_conn(
                &conn,
                &composer,
                &bubble,
                Some("different-call"),
                &attrs,
                false,
                &mut 65536
            )
            .unwrap_err()
            .to_string(),
            "source_changed"
        );
        conn.execute(
            "UPDATE cursorDiskKV SET value=?1 WHERE key=?2",
            rusqlite::params!["changed bytes", content_id],
        )
        .unwrap();
        assert_eq!(
            content_evidence_from_conn(
                &conn,
                &composer,
                &bubble,
                Some("call-id"),
                &attrs,
                false,
                &mut 65536
            )
            .unwrap_err()
            .to_string(),
            "source_changed"
        );
        conn.execute("DELETE FROM cursorDiskKV WHERE key=?1", [&content_id]).unwrap();
        assert_eq!(
            content_evidence_from_conn(
                &conn,
                &composer,
                &bubble,
                Some("call-id"),
                &attrs,
                false,
                &mut 65536
            )
            .unwrap_err()
            .to_string(),
            "source_missing"
        );
        fs::remove_dir_all(root).unwrap();
    }
}
