use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, params_from_iter, types::ValueRef};
use serde_json::Value;
use tracing::{debug, warn};

use crate::adapters::json_util::rfc3339_ms;
use crate::adapters::paths;

use crate::adapters::AdapterSyncContext;
use crate::adapters::events;
use crate::adapters::{
    RawMessage, RawSession, ResumeCommand, SourceAdapter, SyncScanResult, SyncScanStats,
};
use crate::types::{
    FileEvidence, FileEvidenceKind, FileOperation, ParentLink, ParentRelation, RawSessionEvent,
    RawUsageEvent, Role, ThreadRole,
};

#[path = "opencode_v2.rs"]
mod v2;

const MAX_SQL_VARS_PER_BATCH: usize = 900;
pub(crate) const USAGE_PARSER_VERSION: u32 = 2;
pub(crate) const EVENT_PARSER_VERSION: u32 = 7;
pub(crate) const METADATA_PARSER_VERSION: u32 = 2;
const NATIVE_METADATA_PARSER_VERSION: u32 = 3;
const PARSED_PART_FILTER_SQL: &str = "
    json_valid(m.data)
    AND json_valid(p.data)
    AND json_extract(m.data, '$.role') IN ('user', 'assistant')
    AND json_extract(p.data, '$.type') = 'text'
    AND NULLIF(TRIM(CAST(json_extract(p.data, '$.text') AS TEXT)), '') IS NOT NULL
";
const HIDDEN_TRANSCRIPT_FILTER_SQL: &str =
    "AND json_extract(m.data, '$.semantics.transcriptVisibility') IS NOT 'hidden'";
const TIMELINE_USAGE_FILTER_SQL: &str =
    "AND json_extract(data, '$.semantics.kind') IS NOT 'timeline_event'";

#[derive(Clone, Copy, Default)]
pub(crate) struct ScanOptions {
    pub exclude_hidden_transcript: bool,
    pub exclude_timeline_usage: bool,
    zcode_tools: bool,
    native_topology: bool,
}

impl ScanOptions {
    pub(crate) const ZCODE: Self = Self {
        exclude_hidden_transcript: true,
        exclude_timeline_usage: true,
        zcode_tools: true,
        native_topology: false,
    };

    const OPENCODE_NATIVE: Self = Self {
        exclude_hidden_transcript: false,
        exclude_timeline_usage: false,
        zcode_tools: false,
        native_topology: true,
    };

    fn metadata_parser_version(self, native_v2: bool) -> u32 {
        if self.native_topology && native_v2 {
            4
        } else if self.native_topology {
            NATIVE_METADATA_PARSER_VERSION
        } else {
            METADATA_PARSER_VERSION
        }
    }

    fn transcript_sql(self) -> &'static str {
        if self.exclude_hidden_transcript { HIDDEN_TRANSCRIPT_FILTER_SQL } else { "" }
    }

    fn usage_sql(self) -> &'static str {
        if self.exclude_timeline_usage { TIMELINE_USAGE_FILTER_SQL } else { "" }
    }
}

pub(crate) struct OpenCodeAdapter;

struct SessionRow {
    id: String,
    directory: String,
    time_created: i64,
    time_updated: Option<i64>,
    title: Option<String>,
    parent_id: Option<String>,
    v2: bool,
    // For a session present in both schemas, the millisecond timestamp of the
    // last legacy `message` row; V2 rows at or before it are treated as a
    // lossy copy and skipped. `None` means the session has no legacy history.
    legacy_tail: Option<i64>,
}

impl SourceAdapter for OpenCodeAdapter {
    fn id(&self) -> &str {
        "opencode"
    }

    fn label(&self) -> &str {
        "OC"
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        Some(ResumeCommand::new("opencode", &["--session", source_id]))
    }

    fn start_command(&self, prompt: String) -> Option<ResumeCommand> {
        Some(ResumeCommand {
            program: "opencode".to_string(),
            args: vec!["run".to_string(), "-i".to_string(), prompt],
        })
    }

    fn usage_parser_version(&self) -> Option<u32> {
        Some(USAGE_PARSER_VERSION)
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let Some(conn) = open_opencode_db()? else {
            return Ok(vec![]);
        };
        scan_with_options(&conn, true, ScanOptions::OPENCODE_NATIVE)
    }

    fn scan_for_sync(
        &self,
        context: &AdapterSyncContext,
        since_ts: Option<i64>,
        include_events: bool,
    ) -> anyhow::Result<Option<SyncScanResult>> {
        let Some(conn) = open_opencode_db()? else {
            return Ok(Some(SyncScanResult::default()));
        };

        Ok(Some(scan_for_sync_conn_with_options(
            &conn,
            context,
            since_ts,
            include_events,
            ScanOptions::OPENCODE_NATIVE,
        )?))
    }
}

fn open_opencode_db() -> anyhow::Result<Option<Connection>> {
    for db_path in opencode_db_candidates() {
        if let Some(conn) = open_readonly(&db_path)? {
            return Ok(Some(conn));
        }
    }
    Ok(None)
}

fn opencode_db_candidates() -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Some(explicit) = paths::env_path_dir("OPENCODE_SQLITE_DB") {
        out.push(explicit);
    }
    if let Some(home) = dirs::home_dir() {
        out.push(home.join(".local/share/opencode/opencode.db"));
        out.push(home.join(".config/opencode/opencode.db"));
    }
    if let Some(data) = dirs::data_local_dir() {
        out.push(data.join("opencode/opencode.db"));
    }
    if let Some(config) = dirs::config_dir() {
        out.push(config.join("opencode/opencode.db"));
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|path| seen.insert(path.clone()));
    out
}

pub(crate) fn open_readonly(db_path: &Path) -> anyhow::Result<Option<Connection>> {
    if !db_path.exists() {
        debug!("session DB not found at {}, skipping", db_path.display());
        return Ok(None);
    }

    Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map(Some)
        .map_err(Into::into)
}

pub(crate) fn scan(conn: &Connection, include_events: bool) -> anyhow::Result<Vec<RawSession>> {
    scan_with_options(conn, include_events, ScanOptions::default())
}

pub(crate) fn scan_with_options(
    conn: &Connection,
    include_events: bool,
    options: ScanOptions,
) -> anyhow::Result<Vec<RawSession>> {
    let (sessions, _) = load_session_rows(conn, None, None, options)?;
    scan_session_messages(conn, sessions, include_events, options)
}

fn load_session_rows(
    conn: &Connection,
    since_ts: Option<i64>,
    target: Option<&str>,
    options: ScanOptions,
) -> anyhow::Result<(Vec<SessionRow>, u32)> {
    let native_v2 = options.native_topology && has_table(conn, "session_v2")?;
    let legacy = has_table(conn, "session")?;
    // Other adapters share this parser but do not share OpenCode's parent semantics.
    // A missing column is compatible; other schema/query errors still propagate.
    let has_parent_id = if options.native_topology && legacy {
        let mut columns = conn.prepare("PRAGMA table_info(session)")?;
        let names = columns.query_map([], |row| row.get::<_, String>(1))?;
        names.collect::<rusqlite::Result<Vec<_>>>()?.iter().any(|name| name == "parent_id")
    } else {
        false
    };
    let parent_column = if has_parent_id { "parent_id" } else { "NULL AS parent_id" };
    let mut selects = Vec::new();
    if legacy {
        let exclude_v2 = if native_v2 {
            "AND NOT EXISTS (SELECT 1 FROM session_v2 v WHERE v.id = session.id)"
        } else {
            ""
        };
        selects.push(format!(
            "SELECT id, directory, time_created, time_updated, title,
            {parent_column}, 0 AS v2, NULL AS legacy_tail
            FROM session WHERE (?1 IS NULL OR id = ?1) {exclude_v2}"
        ));
    }
    if native_v2 {
        let legacy_blend = if legacy {
            "COALESCE((SELECT l.time_updated FROM session l WHERE l.id = s.id), s.time_created)"
        } else {
            "s.time_created"
        };
        let legacy_tail = if legacy {
            "(SELECT MAX(lm.time_created) FROM message lm WHERE lm.session_id = s.id)"
        } else {
            "NULL"
        };
        selects.push(format!(
            "SELECT s.id, s.directory, s.time_created,
            MAX(COALESCE(s.time_updated, s.time_created),
                COALESCE((SELECT MAX(COALESCE(m.time_updated, m.time_created))
                    FROM session_message m WHERE m.session_id = s.id), s.time_created),
                {legacy_blend}),
            s.title, s.parent_id, 1 AS v2,
            {legacy_tail} AS legacy_tail
            FROM session_v2 s WHERE (?1 IS NULL OR s.id = ?1)"
        ));
    }
    if selects.is_empty() {
        return Ok((Vec::new(), 0));
    }
    let mut stmt = conn.prepare(&selects.join(" UNION ALL "))?;
    let rows = stmt.query_map([target], map_session_row)?;
    let mut sessions = Vec::new();
    for row in rows {
        match row {
            Ok(session) => sessions.push(session),
            Err(err) => debug!("skipping malformed OpenCode session row: {err}"),
        }
    }
    let Some(cutoff) = since_ts else {
        return Ok((sessions, 0));
    };
    let before = sessions.len();
    sessions.retain(|row| row.time_updated.unwrap_or(row.time_created) >= cutoff);
    let filtered = (before - sessions.len()) as u32;
    Ok((sessions, filtered))
}

fn has_table(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [name],
        |row| row.get(0),
    )
}

fn map_session_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        id: row.get(0)?,
        directory: row.get(1)?,
        time_created: sqlite_millis(row, 2)?.unwrap_or(0),
        time_updated: sqlite_millis(row, 3)?,
        title: row.get(4)?,
        parent_id: row.get(5)?,
        v2: row.get(6)?,
        legacy_tail: sqlite_millis(row, 7)?,
    })
}

fn sqlite_millis(row: &rusqlite::Row<'_>, idx: usize) -> rusqlite::Result<Option<i64>> {
    match row.get_ref(idx)? {
        ValueRef::Null => Ok(None),
        ValueRef::Integer(value) => Ok(Some(normalize_opencode_ts(value))),
        ValueRef::Real(value) => Ok(Some(normalize_opencode_ts(value as i64))),
        ValueRef::Text(bytes) => {
            let text = std::str::from_utf8(bytes).unwrap_or("").trim();
            Ok(parse_opencode_text_millis(text))
        }
        _ => Ok(None),
    }
}

fn parse_opencode_text_millis(text: &str) -> Option<i64> {
    if let Ok(value) = text.parse::<i64>() {
        return Some(normalize_opencode_ts(value));
    }
    if let Some(timestamp) = rfc3339_ms(Some(&Value::String(text.to_string()))) {
        return Some(timestamp);
    }
    ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f"]
        .into_iter()
        .find_map(|format| chrono::NaiveDateTime::parse_from_str(text, format).ok())
        .map(|timestamp| timestamp.and_utc().timestamp_millis())
}

fn normalize_opencode_ts(value: i64) -> i64 {
    if (1_000_000_000..1_000_000_000_000).contains(&value.abs()) {
        value.saturating_mul(1000)
    } else {
        value
    }
}

fn scan_session_messages(
    conn: &Connection,
    sessions: Vec<SessionRow>,
    include_events: bool,
    options: ScanOptions,
) -> anyhow::Result<Vec<RawSession>> {
    if sessions.is_empty() {
        return Ok(vec![]);
    }

    // Legacy loaders run for pure-legacy sessions and for mixed sessions, where
    // the legacy rows hold history (tokens, patch events) the V2 copy lost.
    let session_ids: Vec<String> = sessions
        .iter()
        .filter(|s| !s.v2 || s.legacy_tail.is_some())
        .map(|s| s.id.clone())
        .collect();
    let v2_ids: Vec<String> = sessions.iter().filter(|s| s.v2).map(|s| s.id.clone()).collect();
    let v2_tails: HashMap<String, i64> = sessions
        .iter()
        .filter(|s| s.v2)
        .filter_map(|s| s.legacy_tail.map(|tail| (s.id.clone(), tail)))
        .collect();
    let mut session_messages: HashMap<String, Vec<RawMessage>> = HashMap::new();
    let mut session_usage_events: HashMap<String, Vec<RawUsageEvent>> = HashMap::new();
    let mut session_events: HashMap<String, Vec<RawSessionEvent>> = HashMap::new();

    for chunk in session_ids.chunks(MAX_SQL_VARS_PER_BATCH) {
        load_message_chunk(conn, chunk, &mut session_messages, options)?;
        load_usage_chunk(conn, chunk, &mut session_usage_events, options)?;
        if include_events {
            load_event_chunk(conn, chunk, &mut session_events, options)?;
        }
    }
    for chunk in v2_ids.chunks(MAX_SQL_VARS_PER_BATCH) {
        let directories =
            sessions.iter().filter(|s| s.v2).map(|s| (s.id.clone(), s.directory.clone())).collect();
        let data = v2::load_chunk(conn, chunk, include_events, &directories, &v2_tails)?;
        for (sid, items) in data.messages {
            session_messages.entry(sid).or_default().extend(items);
        }
        for (sid, items) in data.usage {
            session_usage_events.entry(sid).or_default().extend(items);
        }
        for (sid, items) in data.events {
            session_events.entry(sid).or_default().extend(items);
        }
    }

    let mut raw_sessions = Vec::new();
    for session in sessions {
        let messages = session_messages.remove(&session.id).unwrap_or_default();
        let usage_events = session_usage_events.remove(&session.id).unwrap_or_default();
        let mut events = session_events.remove(&session.id).unwrap_or_default();
        // A mixed session merges legacy events and V2 tail events, each numbered
        // from zero in its own schema; renumber so session_events stays unique.
        for (seq, event) in events.iter_mut().enumerate() {
            event.event_seq = seq as u32;
        }
        for file in events.iter_mut().flat_map(|event| &mut event.files) {
            // Legacy file rows for a mixed session also need the directory fallback.
            if (session.legacy_tail.is_some() || !session.v2)
                && !options.zcode_tools
                && file.kind != FileEvidenceKind::Command
                && file.cwd.is_none()
                && !session.directory.trim().is_empty()
            {
                file.cwd = Some(session.directory.clone());
            }
        }
        if (session.legacy_tail.is_some() || !session.v2)
            && messages.is_empty()
            && usage_events.is_empty()
            && events.is_empty()
        {
            continue;
        }

        let parent = session.parent_id.as_deref().map(str::trim).filter(|id| !id.is_empty());
        let parent = parent.filter(|id| {
            if *id == session.id {
                warn!(session_id = %session.id, "ignoring self-referential OpenCode parent_id");
                false
            } else {
                true
            }
        });
        let parent_links: Vec<_> = parent
            .into_iter()
            .map(|id| ParentLink {
                relation: ParentRelation::Spawn,
                source: "opencode".to_string(),
                source_id: id.to_string(),
            })
            .collect();
        let thread_role = (!parent_links.is_empty()).then_some(ThreadRole::Subagent);
        let mut raw = RawSession::search_only(
            session.id,
            Some(session.directory),
            session.time_created,
            session.time_updated,
            None,
            messages,
        )
        .with_usage(usage_events, USAGE_PARSER_VERSION);
        raw.source_file_path = conn.path().filter(|path| !path.is_empty()).map(str::to_string);
        raw.custom_title =
            session.title.map(|title| title.trim().to_string()).filter(|title| !title.is_empty());
        raw.thread_role = thread_role;
        raw.parent_links = parent_links;
        raw.metadata_parser_version = Some(options.metadata_parser_version(session.v2));
        raw.refresh_session_on_metadata_backfill = true;
        raw_sessions.push(if include_events {
            raw.with_events(events, EVENT_PARSER_VERSION)
        } else {
            raw
        });
    }

    Ok(raw_sessions)
}

fn load_event_chunk(
    conn: &Connection,
    session_ids: &[String],
    session_events: &mut HashMap<String, Vec<RawSessionEvent>>,
    options: ScanOptions,
) -> anyhow::Result<()> {
    let placeholders = std::iter::repeat_n("?", session_ids.len()).collect::<Vec<_>>().join(", ");
    let sql = format!(
        "SELECT m.session_id, CAST(p.id AS TEXT), p.data, m.time_created, CAST(m.id AS TEXT), m.data
         FROM message m
         JOIN part p ON p.message_id = m.id
         WHERE m.session_id IN ({placeholders})
           AND json_valid(m.data)
           AND json_valid(p.data)
           AND json_extract(m.data, '$.role') = 'assistant'
           AND json_extract(p.data, '$.type') IN ('tool-invocation', 'tool-result', 'tool', 'patch')
         ORDER BY m.time_created, p.id"
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(session_ids.iter()), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            sqlite_millis(row, 3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
        ))
    })?;
    let mut rows = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    rows.sort_by_key(|row| row.3);

    for row in rows {
        let (session_id, part_id, part_data, timestamp, message_id, message_data) = row;
        let message: Value = serde_json::from_str(&message_data)?;
        let cwd = message
            .pointer("/path/cwd")
            .and_then(Value::as_str)
            .filter(|cwd| !cwd.trim().is_empty());
        let events = session_events.entry(session_id).or_default();
        let part_events = parse_part_events(
            &part_id,
            &part_data,
            timestamp,
            events.len() as u32,
            conn.path().filter(|path| !path.is_empty()),
            &message_id,
            cwd,
            options,
        );
        events.extend(part_events);
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn parse_part_events(
    part_id: &str,
    part_data: &str,
    timestamp: Option<i64>,
    event_seq: u32,
    source_path: Option<&str>,
    message_id: &str,
    cwd: Option<&str>,
    options: ScanOptions,
) -> Vec<RawSessionEvent> {
    let Some(part) = serde_json::from_str::<Value>(part_data).ok() else {
        return Vec::new();
    };
    let context = |event_seq, source_event_id| events::EventContext {
        event_seq,
        timestamp,
        source_path: source_path.map(str::to_string),
        source_event_id: Some(source_event_id),
        message_seq: None,
        parser_version: EVENT_PARSER_VERSION,
    };
    let mut parsed = match part.get("type").and_then(Value::as_str) {
        Some("tool-invocation") => {
            let name = part.get("toolName").and_then(Value::as_str).unwrap_or("tool").to_string();
            vec![events::tool_call_event(
                context(event_seq, part_id.to_string()),
                name,
                part.get("input"),
            )]
        }
        Some("tool-result") => {
            let name = part.get("toolName").and_then(Value::as_str).map(String::from);
            let summary = part.get("result").map(|result| result.to_string());
            vec![events::tool_result_event(context(event_seq, part_id.to_string()), name, summary)]
        }
        Some("tool") => {
            let name = opencode_tool_name(&part);
            let Some(state) = part.get("state") else {
                return Vec::new();
            };
            let status = match state.get("status").and_then(Value::as_str) {
                Some("completed") => Some("success"),
                Some("error") => Some("error"),
                _ => None,
            };
            let mut part_events = Vec::new();
            if let Some(input) = state.get("input") {
                part_events.push(events::tool_call_event(
                    context(event_seq, format!("{part_id}:input")),
                    name.clone(),
                    Some(input),
                ));
            }
            let output = state.get("output").or_else(|| state.get("error"));
            if output.is_some() || status.is_some() {
                let mut event = events::tool_result_event(
                    context(event_seq + part_events.len() as u32, format!("{part_id}:output")),
                    Some(name),
                    output.map(display_json_value),
                );
                event.status = status.map(str::to_string);
                part_events.push(event);
            }
            part_events
        }
        Some("patch") => {
            let files = patch_files(&part)
                .into_iter()
                .map(|path| FileEvidence {
                    path,
                    operation: FileOperation::Write,
                    kind: FileEvidenceKind::Observation,
                    cwd: cwd.map(str::to_string),
                    target: None,
                })
                .collect::<Vec<_>>();
            vec![RawSessionEvent {
                target: files.first().map(|file| file.path.clone()),
                files,
                name: Some("patch".to_string()),
                ..context(event_seq, part_id.to_string()).event("file_write", "assistant")
            }]
        }
        _ => Vec::new(),
    };
    let call_id = match part.get("type").and_then(Value::as_str) {
        Some("tool") => part.get("callID"),
        Some("tool-invocation" | "tool-result") => part.get("toolCallId"),
        _ => None,
    }
    .and_then(Value::as_str)
    .filter(|id| !id.trim().is_empty());
    let attrs = serde_json::json!({"part_id": part_id, "message_id": message_id, "part": part});
    for event in &mut parsed {
        event.attrs_json = Some(attrs.to_string());
        event.tool_call_id = call_id.map(str::to_string);
        if options.zcode_tools
            && event.actor == "tool"
            && part.pointer("/state/status").and_then(Value::as_str) == Some("completed")
            && let Some(observation) = part.pointer("/state/metadata/readFileState")
            && observation.get("schemaVersion").and_then(Value::as_u64) == Some(1)
            && observation.get("tool").and_then(Value::as_str) == event.name.as_deref()
            && let Some(operation) = match event.name.as_deref() {
                Some("Read") => Some(FileOperation::Read),
                Some("Write" | "Edit") => Some(FileOperation::Write),
                _ => None,
            }
            && let Some(path) = observation.get("path").and_then(Value::as_str)
            && Path::new(path).is_absolute()
        {
            event.files.push(FileEvidence {
                path: path.to_string(),
                operation,
                kind: FileEvidenceKind::Observation,
                cwd: None,
                target: None,
            });
        }
        if matches!(part.get("type").and_then(Value::as_str), Some("tool" | "tool-invocation"))
            && event.actor == "assistant"
        {
            let input = part.get("input").or_else(|| part.pointer("/state/input"));
            if event.name.as_deref() == Some("bash")
                || (options.zcode_tools && event.name.as_deref() == Some("Bash"))
            {
                event.kind = "command".to_string();
                event.target = input
                    .and_then(|input| input.get("command"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if let Some(command) = event.target.as_deref() {
                    let base =
                        cwd.filter(|cwd| !options.zcode_tools && Path::new(cwd).is_absolute());
                    let shell_cwd = match input.and_then(|input| input.get("workdir")) {
                        _ if options.zcode_tools => None,
                        Some(Value::String(path)) if Path::new(path).is_absolute() => {
                            Some(PathBuf::from(path))
                        }
                        Some(Value::String(path)) => base.map(|base| Path::new(base).join(path)),
                        Some(_) => None,
                        None => base.map(PathBuf::from),
                    };
                    let (files, status) = events::shell_file_evidence(
                        command,
                        shell_cwd.as_deref().and_then(Path::to_str),
                    );
                    event.files = files;
                    event.command_evidence_status = Some(status);
                }
            }
            let selection = match event.name.as_deref() {
                Some("Read") if options.zcode_tools => Some((FileOperation::Read, "file_path")),
                Some("Write" | "Edit") if options.zcode_tools => {
                    Some((FileOperation::Write, "file_path"))
                }
                Some("read") => Some((FileOperation::Read, "filePath")),
                Some("write" | "edit") => Some((FileOperation::Write, "filePath")),
                Some("readFile") => Some((FileOperation::Read, "path")),
                _ => None,
            };
            if let Some((operation, key)) = selection
                && let Some(path) = input
                    .and_then(|input| input.get(key))
                    .and_then(Value::as_str)
                    .filter(|path| !path.trim().is_empty())
            {
                event.files.push(FileEvidence::call(
                    path.to_string(),
                    operation,
                    cwd.filter(|_| !options.zcode_tools).map(str::to_string),
                ));
            }
            if event.name.as_deref() == Some("apply_patch")
                && let Some(patch) =
                    input.and_then(|input| input.get("patchText")).and_then(Value::as_str)
            {
                event.files = events::patch_file_evidence(patch);
                for file in &mut event.files {
                    file.cwd = cwd.map(str::to_string);
                }
            }
        }
    }
    parsed
}

fn load_message_chunk(
    conn: &Connection,
    session_ids: &[String],
    session_messages: &mut HashMap<String, Vec<RawMessage>>,
    options: ScanOptions,
) -> anyhow::Result<()> {
    let placeholders = std::iter::repeat_n("?", session_ids.len()).collect::<Vec<_>>().join(", ");
    let transcript_sql = options.transcript_sql();
    let sql = format!(
        "SELECT m.session_id, json_extract(m.data, '$.role') AS role, p.data, m.time_created
         FROM message m
         JOIN part p ON p.message_id = m.id
         WHERE m.session_id IN ({placeholders})
           AND {PARSED_PART_FILTER_SQL}
           {transcript_sql}
         ORDER BY m.time_created, p.id"
    );

    let mut stmt = conn.prepare(&sql)?;
    let msg_rows = stmt.query_map(params_from_iter(session_ids.iter()), |row| {
        let session_id: String = row.get(0)?;
        let role: Option<String> = row.get(1)?;
        let part_data: String = row.get(2)?;
        let timestamp = sqlite_millis(row, 3)?;
        Ok((session_id, role, part_data, timestamp))
    })?;
    let mut msg_rows = msg_rows.collect::<rusqlite::Result<Vec<_>>>()?;
    msg_rows.sort_by_key(|row| row.3);

    for row in msg_rows {
        let (session_id, role_str, part_data, timestamp) = row;
        let Some(role) = parse_role(role_str.as_deref()) else {
            continue;
        };
        let Some(content) = parse_part_content(&part_data) else {
            continue;
        };

        session_messages.entry(session_id).or_default().push(RawMessage {
            role,
            content,
            timestamp,
        });
    }

    Ok(())
}

fn load_usage_chunk(
    conn: &Connection,
    session_ids: &[String],
    session_usage_events: &mut HashMap<String, Vec<RawUsageEvent>>,
    options: ScanOptions,
) -> anyhow::Result<()> {
    let placeholders = std::iter::repeat_n("?", session_ids.len()).collect::<Vec<_>>().join(", ");
    let usage_sql = options.usage_sql();
    let sql = format!(
        "SELECT CAST(id AS TEXT), session_id, data, time_created
         FROM message
         WHERE session_id IN ({placeholders})
           AND json_valid(data)
           AND json_extract(data, '$.role') = 'assistant'
           AND json_type(data, '$.tokens') = 'object'
           {usage_sql}
         ORDER BY time_created, id"
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(session_ids.iter()), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            sqlite_millis(row, 3)?.unwrap_or(0),
        ))
    })?;
    let mut rows = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    rows.sort_by_key(|row| row.3);

    for row in rows {
        let (message_id, session_id, data, timestamp) = row;
        let events = session_usage_events.entry(session_id).or_default();
        if let Some(event) = parse_usage_event(&message_id, &data, timestamp, events.len() as u32) {
            events.push(event);
        }
    }

    Ok(())
}

fn parse_usage_event(
    message_id: &str,
    message_data: &str,
    timestamp: i64,
    event_seq: u32,
) -> Option<RawUsageEvent> {
    let message: Value = serde_json::from_str(message_data).ok()?;
    let tokens = message.get("tokens")?;
    let provider = message
        .get("providerID")
        .and_then(|provider| provider.as_str())
        .filter(|provider| !provider.trim().is_empty())
        .unwrap_or("unknown");
    let model = message
        .get("modelID")
        .and_then(|model| model.as_str())
        .filter(|model| !model.trim().is_empty())
        .unwrap_or("unknown");

    Some(RawUsageEvent {
        model: model.to_string(),
        provider: provider.to_string(),
        input_tokens: token_count(tokens, "input"),
        output_tokens: token_count(tokens, "output"),
        cache_read_tokens: cache_token_count(tokens, "read"),
        cache_write_tokens: cache_token_count(tokens, "write"),
        reasoning_tokens: token_count(tokens, "reasoning"),
        raw_usage_json: Some(
            serde_json::json!({
                "providerID": provider,
                "modelID": model,
                "tokens": tokens,
            })
            .to_string(),
        ),
        ..RawUsageEvent::observed(
            format!("message:{message_id}"),
            event_seq,
            timestamp,
            USAGE_PARSER_VERSION,
        )
    })
}

fn token_count(tokens: &Value, key: &str) -> i64 {
    tokens.get(key).and_then(|value| value.as_i64()).unwrap_or(0).max(0)
}

fn cache_token_count(tokens: &Value, key: &str) -> i64 {
    tokens
        .get("cache")
        .and_then(|cache| cache.get(key))
        .and_then(|value| value.as_i64())
        .unwrap_or(0)
        .max(0)
}

fn load_message_counts(
    conn: &Connection,
    session_ids: &[String],
    options: ScanOptions,
) -> anyhow::Result<HashMap<String, u32>> {
    let mut counts = HashMap::new();

    for chunk in session_ids.chunks(MAX_SQL_VARS_PER_BATCH) {
        let placeholders = std::iter::repeat_n("?", chunk.len()).collect::<Vec<_>>().join(", ");
        let transcript_sql = options.transcript_sql();
        let sql = format!(
            "SELECT m.session_id, COUNT(*)
             FROM message m
             JOIN part p ON p.message_id = m.id
             WHERE m.session_id IN ({placeholders})
               AND {PARSED_PART_FILTER_SQL}
               {transcript_sql}
             GROUP BY m.session_id"
        );

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(chunk.iter()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u32))
        })?;

        for row in rows {
            let (session_id, count) = row?;
            counts.insert(session_id, count);
        }
    }

    Ok(counts)
}

fn parse_role(role: Option<&str>) -> Option<Role> {
    match role {
        Some("user") => Some(Role::User),
        Some("assistant") => Some(Role::Assistant),
        _ => None,
    }
}

fn parse_part_content(part_data: &str) -> Option<String> {
    let part: Value = serde_json::from_str(part_data).ok()?;
    let part_type = part.get("type").and_then(|t| t.as_str()).unwrap_or("");

    match part_type {
        "text" => part
            .get("text")
            .and_then(|t| t.as_str())
            .and_then(|text| if text.trim().is_empty() { None } else { Some(text.to_string()) }),
        _ => None,
    }
}

fn opencode_tool_name(part: &Value) -> String {
    part.get("tool")
        .or_else(|| part.get("toolName"))
        .and_then(|name| name.as_str())
        .unwrap_or("tool")
        .to_string()
}

fn patch_files(part: &Value) -> Vec<String> {
    part.get("files")
        .and_then(|files| files.as_array())
        .into_iter()
        .flatten()
        .filter_map(|file| file.as_str())
        .filter(|file| !file.trim().is_empty())
        .map(str::to_string)
        .collect()
}

fn display_json_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.to_string(),
        other => other.to_string(),
    }
}

pub(crate) fn scan_for_sync_conn(
    conn: &Connection,
    context: &AdapterSyncContext,
    since_ts: Option<i64>,
    include_events: bool,
) -> anyhow::Result<SyncScanResult> {
    scan_for_sync_conn_with_options(conn, context, since_ts, include_events, ScanOptions::default())
}

pub(crate) fn scan_for_sync_conn_with_options(
    conn: &Connection,
    context: &AdapterSyncContext,
    since_ts: Option<i64>,
    include_events: bool,
    options: ScanOptions,
) -> anyhow::Result<SyncScanResult> {
    let (sessions, filtered_sessions) =
        load_session_rows(conn, since_ts, context.target_source_id(), options)?;
    let existing = context.session_meta();
    let usage_state = context.usage_state();
    let event_state = context.event_state();
    let metadata_state = context.metadata_state();
    // Mixed sessions count both their legacy history and their V2 tail.
    let legacy_ids: Vec<String> = sessions
        .iter()
        .filter(|s| !s.v2 || s.legacy_tail.is_some())
        .map(|s| s.id.clone())
        .collect();
    let mut current_counts = load_message_counts(conn, &legacy_ids, options)?;
    let v2_ids = sessions.iter().filter(|s| s.v2).map(|s| s.id.clone()).collect::<Vec<_>>();
    let v2_tails: HashMap<String, i64> = sessions
        .iter()
        .filter(|s| s.v2)
        .filter_map(|s| s.legacy_tail.map(|tail| (s.id.clone(), tail)))
        .collect();
    for chunk in v2_ids.chunks(MAX_SQL_VARS_PER_BATCH) {
        for (sid, count) in v2::message_counts(conn, chunk, &v2_tails)? {
            *current_counts.entry(sid).or_insert(0) += count;
        }
    }

    let mut stats = SyncScanStats {
        candidates: sessions.len() as u32,
        filtered_sessions,
        ..Default::default()
    };
    let mut candidates = Vec::new();

    for session in sessions {
        if let Some(old) = existing.get(&session.id) {
            let current_message_count = current_counts.get(&session.id).copied().unwrap_or(0);
            if session.time_updated == old.updated_at
                && current_message_count == old.message_count
                && crate::adapters::sync_state::session_state_is_current(
                    USAGE_PARSER_VERSION,
                    EVENT_PARSER_VERSION,
                    usage_state.get(&session.id).copied(),
                    event_state.get(&session.id).copied(),
                    session.time_updated,
                    include_events,
                )
                && crate::adapters::sync_state::parser_state_is_current(
                    options.metadata_parser_version(session.v2),
                    metadata_state.get(&session.id).copied(),
                    session.time_updated,
                )
            {
                stats.skipped_sessions += 1;
                continue;
            }
        }
        candidates.push(session);
    }

    let sessions = scan_session_messages(conn, candidates, include_events, options)?;
    stats.parsed = sessions.len() as u32;
    Ok(SyncScanResult { sessions, stats, observations: Vec::new() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::test_support::{
        seed_empty_event_state, seed_empty_metadata_state, seed_empty_usage_state,
        store as setup_store,
    };
    use crate::db::store::Store;
    use crate::types::Session;
    use std::path::PathBuf;

    fn make_session(
        id: &str,
        source_id: &str,
        updated_at: Option<i64>,
        message_count: u32,
    ) -> Session {
        Session {
            source: "opencode".to_string(),
            source_id: source_id.to_string(),
            title: "existing".to_string(),
            directory: Some("/tmp/project".to_string()),
            started_at: 100,
            updated_at,
            message_count,
            ..crate::types::test_support::session(id)
        }
    }

    fn setup_opencode_db() -> (PathBuf, Connection) {
        let path =
            std::env::temp_dir().join(format!("recall-opencode-test-{}.db", uuid::Uuid::new_v4()));
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "
            CREATE TABLE session (
                id TEXT PRIMARY KEY,
                title TEXT,
                directory TEXT,
                time_created INTEGER,
                time_updated INTEGER
            );
            CREATE TABLE message (
                id INTEGER PRIMARY KEY,
                session_id TEXT NOT NULL,
                data TEXT NOT NULL,
                time_created INTEGER
            );
            CREATE TABLE part (
                id INTEGER PRIMARY KEY,
                message_id INTEGER NOT NULL,
                data TEXT NOT NULL
            );
            ",
        )
        .unwrap();
        (path, conn)
    }

    fn insert_session_with_message(
        conn: &Connection,
        id: &str,
        updated_at: i64,
        time_created: i64,
        text: &str,
    ) {
        conn.execute(
            "INSERT INTO session (id, title, directory, time_created, time_updated)
             VALUES (?1, 'Test', '/tmp/project', ?2, ?3)",
            rusqlite::params![id, time_created, updated_at],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message (session_id, data, time_created)
             VALUES (?1, '{\"role\":\"user\"}', ?2)",
            rusqlite::params![id, time_created + 10],
        )
        .unwrap();
        let message_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO part (id, message_id, data)
             VALUES (NULL, ?1, ?2)",
            rusqlite::params![message_id, format!("{{\"type\":\"text\",\"text\":\"{text}\"}}")],
        )
        .unwrap();
    }

    fn mark_usage_current(store: &Store, source_id: &str, updated_at: Option<i64>) {
        seed_empty_usage_state(store, "opencode", source_id, USAGE_PARSER_VERSION, updated_at);
    }

    fn add_v2_tables(conn: &Connection) {
        conn.execute_batch(
            "CREATE TABLE session_v2 (id TEXT PRIMARY KEY, directory TEXT, title TEXT,
             parent_id TEXT, time_created INTEGER, time_updated INTEGER);
             CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT,
             type TEXT, seq INTEGER, data TEXT, time_created INTEGER, time_updated INTEGER);",
        )
        .unwrap();
    }

    fn insert_v2_user(conn: &Connection, id: &str, parent: Option<&str>, text: &str) {
        insert_v2_user_at(conn, id, parent, text, 100);
    }

    // V2 `session_message.time_created` mirrors the legacy `message.time_created`
    // for migrated copies and advances for messages written after the upgrade.
    fn insert_v2_user_at(
        conn: &Connection,
        id: &str,
        parent: Option<&str>,
        text: &str,
        time_created: i64,
    ) {
        conn.execute(
            "INSERT INTO session_v2 VALUES (?1, '/tmp/project', 'V2', ?2, 100, 200)",
            rusqlite::params![id, parent],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_message VALUES (?1, ?2, 'user', 1, ?3, ?4, ?4)",
            rusqlite::params![
                format!("msg_{id}"),
                id,
                serde_json::json!({"text":text}).to_string(),
                time_created
            ],
        )
        .unwrap();
    }

    #[test]
    fn v2_only_schema_reads_body_and_parent_in_both_scans() {
        let conn = Connection::open_in_memory().unwrap();
        add_v2_tables(&conn);
        insert_v2_user(&conn, "root", None, "root body");
        insert_v2_user(&conn, "child", Some("root"), "child body");
        let full = scan_with_options(&conn, true, ScanOptions::OPENCODE_NATIVE).unwrap();
        let incremental = scan_for_sync_conn_with_options(
            &conn,
            &AdapterSyncContext::empty_for_test("opencode"),
            None,
            true,
            ScanOptions::OPENCODE_NATIVE,
        )
        .unwrap()
        .sessions;
        for sessions in [full, incremental] {
            assert_eq!(sessions.len(), 2);
            let child = sessions.iter().find(|s| s.source_id == "child").unwrap();
            assert_eq!(child.messages[0].content, "child body");
            assert_eq!(child.thread_role, Some(ThreadRole::Subagent));
            assert_eq!(child.parent_links[0].source_id, "root");
        }
    }

    #[test]
    // A mixed session keeps its legacy history and appends only the V2 rows
    // written after the last legacy message; legacy-only sessions are untouched.
    fn v2_mixed_schema_merges_legacy_history_and_v2_tail() {
        let (path, conn) = setup_opencode_db();
        insert_session_with_message(&conn, "same", 200, 100, "stale legacy");
        insert_session_with_message(&conn, "legacy-only", 200, 100, "legacy body");
        add_v2_tables(&conn);
        // The migrated V2 copy duplicates the legacy message at the same
        // timestamp; only the later row written after the upgrade joins it.
        insert_v2_user_at(&conn, "same", None, "copied legacy", 110);
        conn.execute(
            "INSERT INTO session_message VALUES ('msg_same_new','same','user',2,
             '{\"text\":\"new body\"}',300,300)",
            [],
        )
        .unwrap();
        let rows = scan_with_options(&conn, true, ScanOptions::OPENCODE_NATIVE).unwrap();
        assert_eq!(rows.len(), 2);
        let same = rows.iter().find(|s| s.source_id == "same").unwrap();
        assert_eq!(
            same.messages.iter().map(|m| m.content.as_str()).collect::<Vec<_>>(),
            vec!["stale legacy", "new body"]
        );
        assert_eq!(
            rows.iter().find(|s| s.source_id == "legacy-only").unwrap().messages[0].content,
            "legacy body"
        );
        let shared = scan_with_options(&conn, true, ScanOptions::default()).unwrap();
        assert_eq!(
            shared.iter().find(|s| s.source_id == "same").unwrap().messages[0].content,
            "stale legacy"
        );
        drop(conn);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn v2_assistant_text_usage_order_and_repeat_counts_match() {
        let conn = Connection::open_in_memory().unwrap();
        add_v2_tables(&conn);
        insert_v2_user(&conn, "s", None, "question");
        let data = serde_json::json!({"model":{"id":"model","providerID":"provider"},
            "tokens":{"input":11,"output":7,"reasoning":3,"cache":{"read":2,"write":1}},
            "content":[{"type":"reasoning","text":"private"},
                       {"type":"text","text":"first"},{"type":"text","text":"second"}]});
        conn.execute(
            "INSERT INTO session_message VALUES ('assistant', 's', 'assistant', 2, ?1, 90, 250)",
            [data.to_string()],
        )
        .unwrap();
        conn.execute("INSERT INTO session_message VALUES ('synthetic','s','synthetic',3,'{\"text\":\"machine\"}',110,110)", []).unwrap();
        let raw =
            scan_with_options(&conn, true, ScanOptions::OPENCODE_NATIVE).unwrap().pop().unwrap();
        assert_eq!(
            raw.messages.iter().map(|m| m.content.as_str()).collect::<Vec<_>>(),
            vec!["question", "first", "second"]
        );
        assert_eq!(raw.updated_at, Some(250));
        assert_eq!(raw.usage_events.len(), 1);
        assert_eq!(raw.usage_events[0].input_tokens, 11);
        assert_eq!(raw.usage_events[0].provider, "provider");
        let store =
            crate::sync::persist_raw_session_for_conformance(setup_store(), "opencode", raw)
                .unwrap();
        let repeat = scan_for_sync_conn_with_options(
            &conn,
            &AdapterSyncContext::from_store_for_test(&store, "opencode").unwrap(),
            None,
            true,
            ScanOptions::OPENCODE_NATIVE,
        )
        .unwrap();
        assert!(repeat.sessions.is_empty());
        assert_eq!(repeat.stats.skipped_sessions, 1);
    }

    #[test]
    // An empty V2 copy still leaves the legacy history readable; malformed V2
    // rows in other sessions are skipped without losing valid ones.
    fn v2_empty_copy_keeps_legacy_and_malformed_messages_are_skipped() {
        let (path, conn) = setup_opencode_db();
        insert_session_with_message(&conn, "empty", 200, 100, "stale history");
        add_v2_tables(&conn);
        insert_v2_user(&conn, "empty", None, "copy");
        conn.execute("DELETE FROM session_message", []).unwrap();
        insert_v2_user(&conn, "valid", None, "keep me");
        conn.execute(
            "INSERT INTO session_message VALUES ('bad','valid','user',2,'{broken',100,100)",
            [],
        )
        .unwrap();
        let rows = scan_with_options(&conn, true, ScanOptions::OPENCODE_NATIVE).unwrap();
        assert_eq!(rows.len(), 2);
        let empty = rows.iter().find(|r| r.source_id == "empty").unwrap();
        assert_eq!(empty.messages.len(), 1);
        assert_eq!(empty.messages[0].content, "stale history");
        assert_eq!(
            rows.iter().find(|r| r.source_id == "valid").unwrap().messages[0].content,
            "keep me"
        );
        drop(conn);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    // Regression for the upgraded-database case: the migrated V2 copy drops
    // tokens and patch parts, so the legacy rows must stay readable and only
    // the rows written after the last legacy message come from V2.
    fn v2_mixed_session_preserves_legacy_tokens_and_patch_events() {
        let (path, conn) = setup_opencode_db();
        conn.execute(
            "INSERT INTO session (id, title, directory, time_created, time_updated)
             VALUES ('s', 'Mixed', '/tmp/project', 100, 500)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message (session_id, data, time_created)
             VALUES ('s', ?1, 110)",
            [r#"{"role":"assistant","modelID":"m","providerID":"p",
                "tokens":{"input":100,"output":20},"path":{"cwd":"/tmp/project"}}"#],
        )
        .unwrap();
        let message_id = conn.last_insert_rowid();
        for part in [
            serde_json::json!({"type":"text","text":"legacy answer"}),
            serde_json::json!({"type":"patch","hash":"h1","files":["src/a.rs"]}),
        ] {
            conn.execute(
                "INSERT INTO part (id, message_id, data) VALUES (NULL, ?1, ?2)",
                rusqlite::params![message_id, part.to_string()],
            )
            .unwrap();
        }

        add_v2_tables(&conn);
        conn.execute(
            "INSERT INTO session_v2 VALUES ('s', '/tmp/project', 'Mixed', NULL, 100, 500)",
            [],
        )
        .unwrap();
        // Lossy migrated copy: same message, no tokens, no patch part.
        conn.execute(
            "INSERT INTO session_message VALUES ('copy','s','assistant',1,?1,110,110)",
            [serde_json::json!({"content":[{"type":"text","text":"legacy answer"}]}).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_message VALUES ('tail','s','user',2,?1,400,400)",
            [serde_json::json!({"text":"after upgrade"}).to_string()],
        )
        .unwrap();

        let raw =
            scan_with_options(&conn, true, ScanOptions::OPENCODE_NATIVE).unwrap().pop().unwrap();
        assert_eq!(
            raw.messages.iter().map(|m| m.content.as_str()).collect::<Vec<_>>(),
            vec!["legacy answer", "after upgrade"]
        );
        assert_eq!(raw.usage_events.len(), 1);
        assert_eq!(raw.usage_events[0].input_tokens, 100);
        let files: Vec<_> = raw.events.iter().flat_map(|event| event.files.iter()).collect();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "src/a.rs");
        drop(conn);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    // Legacy and V2 events are each numbered from zero in their own schema; a
    // merged mixed session must renumber before persisting or the
    // UNIQUE(session_id, event_seq) constraint aborts the transaction.
    fn v2_mixed_session_events_persist_with_unique_event_seq() {
        let (path, conn) = setup_opencode_db();
        conn.execute(
            "INSERT INTO session (id, title, directory, time_created, time_updated)
             VALUES ('s', 'Mixed', '/tmp/project', 100, 500)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message (session_id, data, time_created)
             VALUES ('s', ?1, 110)",
            [r#"{"role":"assistant","path":{"cwd":"/tmp/project"}}"#],
        )
        .unwrap();
        let message_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO part (id, message_id, data) VALUES (NULL, ?1, ?2)",
            rusqlite::params![
                message_id,
                serde_json::json!({"type":"patch","hash":"h1","files":["src/a.rs"]}).to_string()
            ],
        )
        .unwrap();

        add_v2_tables(&conn);
        conn.execute(
            "INSERT INTO session_v2 VALUES ('s', '/tmp/project', 'Mixed', NULL, 100, 500)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_message VALUES ('tail','s','assistant',2,?1,400,400)",
            [serde_json::json!({"content":[{"type":"tool","name":"bash","id":"c1","state":{"status":"completed","input":{"command":"ls"}}}]})
                .to_string()],
        )
        .unwrap();

        let raw =
            scan_with_options(&conn, true, ScanOptions::OPENCODE_NATIVE).unwrap().pop().unwrap();
        // The merged list must carry strictly increasing event_seq values.
        let seqs: Vec<u32> = raw.events.iter().map(|event| event.event_seq).collect();
        assert_eq!(seqs, (0..seqs.len() as u32).collect::<Vec<_>>());
        let store =
            crate::sync::persist_raw_session_for_conformance(setup_store(), "opencode", raw)
                .unwrap();
        let count: i64 =
            store.conn.query_row("SELECT COUNT(*) FROM session_events", [], |r| r.get(0)).unwrap();
        assert!(count >= 2, "expected both legacy and V2 events persisted");
        drop(conn);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn v2_metadata_three_backfill_and_same_count_edit_refresh() {
        let (path, conn) = setup_opencode_db();
        insert_session_with_message(&conn, "s", 200, 100, "old body");
        let mut old =
            scan_with_options(&conn, true, ScanOptions::OPENCODE_NATIVE).unwrap().pop().unwrap();
        old.metadata_parser_version = Some(3);
        let store =
            crate::sync::persist_raw_session_for_conformance(setup_store(), "opencode", old)
                .unwrap();
        add_v2_tables(&conn);
        insert_v2_user_at(&conn, "s", None, "new body", 300);
        let mut result = scan_for_sync_conn_with_options(
            &conn,
            &AdapterSyncContext::from_store_for_test(&store, "opencode").unwrap(),
            None,
            true,
            ScanOptions::OPENCODE_NATIVE,
        )
        .unwrap();
        assert_eq!(result.sessions.len(), 1);
        let raw = result.sessions.pop().unwrap();
        assert_eq!(raw.metadata_parser_version, Some(4));
        assert_eq!(
            raw.messages.iter().map(|m| m.content.as_str()).collect::<Vec<_>>(),
            vec!["old body", "new body"]
        );
        let store =
            crate::sync::persist_raw_session_for_conformance(store, "opencode", raw).unwrap();
        conn.execute("UPDATE session_message SET data='{\"text\":\"edited body\"}',time_updated=400 WHERE id='msg_s'", []).unwrap();
        let changed = scan_for_sync_conn_with_options(
            &conn,
            &AdapterSyncContext::from_store_for_test(&store, "opencode").unwrap(),
            Some(350),
            true,
            ScanOptions::OPENCODE_NATIVE,
        )
        .unwrap();
        assert_eq!(changed.sessions.len(), 1);
        assert_eq!(changed.sessions[0].messages[1].content, "edited body");
        assert_eq!(changed.sessions[0].updated_at, Some(400));
        drop(conn);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn v2_tools_preserve_calls_and_results_separately_from_text() {
        let conn = Connection::open_in_memory().unwrap();
        add_v2_tables(&conn);
        insert_v2_user(&conn, "s", None, "question");
        let data = serde_json::json!({"content":[{"type":"tool","id":"call1","name":"read",
            "state":{"status":"completed","input":{"filePath":"src/main.rs"},
                "content":[{"type":"text","text":"tool output"}]}},
            {"type":"text","text":"answer"}]});
        conn.execute(
            "INSERT INTO session_message VALUES ('assistant','s','assistant',2,?1,150,150)",
            [data.to_string()],
        )
        .unwrap();
        let raw =
            scan_with_options(&conn, true, ScanOptions::OPENCODE_NATIVE).unwrap().pop().unwrap();
        assert_eq!(
            raw.messages.iter().map(|m| m.content.as_str()).collect::<Vec<_>>(),
            vec!["question", "answer"]
        );
        assert_eq!(raw.events.len(), 2);
        assert_eq!(raw.events[0].name.as_deref(), Some("read"));
        assert_eq!(raw.events[0].tool_call_id.as_deref(), Some("call1"));
        assert_eq!(raw.events[1].tool_call_id.as_deref(), Some("call1"));
        assert_eq!(raw.events[1].status.as_deref(), Some("success"));
        assert_eq!(raw.events[1].summary.as_deref(), Some("tool output"));
        assert_eq!(raw.events[0].files[0].path, "src/main.rs");
        let without =
            scan_with_options(&conn, false, ScanOptions::OPENCODE_NATIVE).unwrap().pop().unwrap();
        assert!(without.events.is_empty());
        assert_eq!(without.messages.len(), 2);
    }

    #[test]
    fn v2_review_compaction_usage_survives_transcript_projection() {
        let conn = Connection::open_in_memory().unwrap();
        add_v2_tables(&conn);
        insert_v2_user(&conn, "s", None, "body");
        let data = serde_json::json!({"status":"completed","reason":"auto","summary":"machine summary",
            "model":{"id":"compressor","providerID":"provider"},
            "tokens":{"input":12,"output":3,"reasoning":1,"cache":{"read":2,"write":0}}});
        conn.execute(
            "INSERT INTO session_message VALUES ('compaction','s','compaction',2,?1,150,150)",
            [data.to_string()],
        )
        .unwrap();
        let raw =
            scan_with_options(&conn, true, ScanOptions::OPENCODE_NATIVE).unwrap().pop().unwrap();
        assert_eq!(raw.messages.len(), 1);
        assert_eq!(raw.usage_events.len(), 1);
        assert_eq!(raw.usage_events[0].model, "compressor");
        assert_eq!(raw.usage_events[0].input_tokens, 12);
    }

    #[test]
    fn v2_review_tool_paths_follow_location_switch_history() {
        let conn = Connection::open_in_memory().unwrap();
        add_v2_tables(&conn);
        insert_v2_user(&conn, "s", None, "body");
        conn.execute("UPDATE session_v2 SET directory='/new'", []).unwrap();
        let tool = serde_json::json!({"content":[{"type":"tool","id":"read1","name":"read",
            "state":{"status":"completed","input":{"filePath":"src/main.rs"},"content":[{"type":"text","text":"body"}]}}]});
        conn.execute(
            "INSERT INTO session_message VALUES ('before','s','assistant',2,?1,150,150)",
            [tool.to_string()],
        )
        .unwrap();
        let moved = serde_json::json!({"location":{"directory":"/new"},"previous":{"location":{"directory":"/old"}}});
        conn.execute(
            "INSERT INTO session_message VALUES ('move','s','location-switched',3,?1,160,160)",
            [moved.to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_message VALUES ('after','s','assistant',4,?1,170,170)",
            [tool.to_string()],
        )
        .unwrap();
        let raw =
            scan_with_options(&conn, true, ScanOptions::OPENCODE_NATIVE).unwrap().pop().unwrap();
        assert_eq!(raw.events[0].files[0].cwd.as_deref(), Some("/old"));
        assert_eq!(raw.events[2].files[0].cwd.as_deref(), Some("/new"));
    }

    fn mark_event_current(store: &Store, source_id: &str, updated_at: Option<i64>) {
        seed_empty_event_state(store, "opencode", source_id, EVENT_PARSER_VERSION, updated_at);
    }

    fn mark_metadata_current(store: &Store, source_id: &str) {
        seed_empty_metadata_state(store, "opencode", source_id, METADATA_PARSER_VERSION);
    }

    #[test]
    fn native_topology_preserves_parent_identity_and_unknown_roots() {
        let (path, conn) = setup_opencode_db();
        conn.execute("ALTER TABLE session ADD COLUMN parent_id TEXT", []).unwrap();
        for (id, parent) in [
            ("root", None),
            ("empty", Some("")),
            ("blank", Some("  \t")),
            ("self", Some("self")),
            ("child", Some("root")),
            ("orphan", Some("not-indexed")),
        ] {
            insert_session_with_message(&conn, id, 200, 100, id);
            conn.execute(
                "UPDATE session SET parent_id=?1 WHERE id=?2",
                rusqlite::params![parent, id],
            )
            .unwrap();
        }
        let full = scan_with_options(&conn, true, ScanOptions::OPENCODE_NATIVE).unwrap();
        let incremental = scan_for_sync_conn_with_options(
            &conn,
            &AdapterSyncContext::empty_for_test("opencode"),
            None,
            true,
            ScanOptions::OPENCODE_NATIVE,
        )
        .unwrap()
        .sessions;
        for sessions in [full, incremental] {
            assert_eq!(sessions.len(), 6);
            for raw in sessions {
                assert_eq!(raw.messages[0].content, raw.source_id);
                assert_eq!(raw.metadata_parser_version, Some(3));
                let parent = match raw.source_id.as_str() {
                    "child" => Some("root"),
                    "orphan" => Some("not-indexed"),
                    _ => None,
                };
                assert_eq!(raw.thread_role, parent.map(|_| ThreadRole::Subagent));
                assert_eq!(
                    raw.parent_links,
                    parent
                        .into_iter()
                        .map(|id| ParentLink {
                            relation: ParentRelation::Spawn,
                            source: "opencode".to_string(),
                            source_id: id.to_string(),
                        })
                        .collect::<Vec<_>>()
                );
            }
        }
        drop(conn);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn native_metadata_backfill_refreshes_once_without_source_changes() {
        let (path, conn) = setup_opencode_db();
        conn.execute("ALTER TABLE session ADD COLUMN parent_id TEXT", []).unwrap();
        insert_session_with_message(&conn, "child", 200, 100, "unchanged body");
        conn.execute("UPDATE session SET parent_id='not-indexed' WHERE id='child'", []).unwrap();
        let old = scan(&conn, true).unwrap().pop().unwrap();
        assert_eq!(old.metadata_parser_version, Some(2));
        let store =
            crate::sync::persist_raw_session_for_conformance(setup_store(), "opencode", old)
                .unwrap();
        let mut result = scan_for_sync_conn_with_options(
            &conn,
            &AdapterSyncContext::from_store_for_test(&store, "opencode").unwrap(),
            None,
            true,
            ScanOptions::OPENCODE_NATIVE,
        )
        .unwrap();
        assert_eq!(result.stats.skipped_sessions, 0);
        assert_eq!(result.sessions.len(), 1);
        let fresh = result.sessions.pop().unwrap();
        assert!(fresh.refresh_session_on_metadata_backfill);
        assert_eq!(fresh.metadata_parser_version, Some(3));
        assert_eq!(fresh.parent_links[0].source_id, "not-indexed");
        let store =
            crate::sync::persist_raw_session_for_conformance(store, "opencode", fresh).unwrap();
        let repeat = scan_for_sync_conn_with_options(
            &conn,
            &AdapterSyncContext::from_store_for_test(&store, "opencode").unwrap(),
            None,
            true,
            ScanOptions::OPENCODE_NATIVE,
        )
        .unwrap();
        assert_eq!(repeat.stats.skipped_sessions, 1);
        assert!(repeat.sessions.is_empty());
        drop(conn);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn shared_scanners_keep_metadata_two_and_ignore_parent_column() {
        let (path, conn) = setup_opencode_db();
        conn.execute("ALTER TABLE session ADD COLUMN parent_id TEXT", []).unwrap();
        insert_session_with_message(&conn, "same-native-id", 200, 100, "same body");
        conn.execute("UPDATE session SET parent_id='other-id'", []).unwrap();
        for (source, options) in [
            ("kilo-code", ScanOptions::default()),
            ("mimo-code", ScanOptions::default()),
            ("zcode", ScanOptions::ZCODE),
        ] {
            let mut full = scan_with_options(&conn, true, options).unwrap();
            let first = scan_for_sync_conn_with_options(
                &conn,
                &AdapterSyncContext::empty_for_test(source),
                None,
                true,
                options,
            )
            .unwrap();
            assert_eq!(first.sessions.len(), 1);
            for raw in full.iter().chain(first.sessions.iter()) {
                assert_eq!(raw.source_id, "same-native-id");
                assert_eq!(raw.messages[0].content, "same body");
                assert_eq!(raw.metadata_parser_version, Some(2));
                assert_eq!(raw.thread_role, None);
                assert!(raw.parent_links.is_empty());
            }
            let store = crate::sync::persist_raw_session_for_conformance(
                setup_store(),
                source,
                full.pop().unwrap(),
            )
            .unwrap();
            let repeat = scan_for_sync_conn_with_options(
                &conn,
                &AdapterSyncContext::from_store_for_test(&store, source).unwrap(),
                None,
                true,
                options,
            )
            .unwrap();
            assert_eq!(repeat.stats.skipped_sessions, 1);
            assert!(repeat.sessions.is_empty());
        }
        drop(conn);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn single_session_scan_filters_before_reading_messages() {
        let (path, conn) = setup_opencode_db();
        for id in ["target", "other"] {
            insert_session_with_message(&conn, id, 2000, 1000, "hello");
        }
        for (target, count) in [(None, 2), (Some("target"), 1), (Some("missing"), 0)] {
            let context = AdapterSyncContext::empty_for_test("opencode");
            let context = target.map_or_else(
                || AdapterSyncContext::empty_for_test("opencode"),
                |id| context.restricted_to(id),
            );
            let result = scan_for_sync_conn(&conn, &context, None, true).unwrap();
            assert_eq!(result.sessions.len(), count);
            assert_eq!(result.stats.candidates as usize, count);
            assert!(result.sessions.iter().all(|s| target.is_none_or(|id| s.source_id == id)));
        }
        drop(conn);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn scan_parses_sqlite_current_timestamp_defaults() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE session (
                id TEXT PRIMARY KEY,
                title TEXT,
                directory TEXT,
                time_created TEXT DEFAULT CURRENT_TIMESTAMP,
                time_updated TEXT DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE message (
                id INTEGER PRIMARY KEY,
                session_id TEXT NOT NULL,
                data TEXT NOT NULL,
                time_created TEXT DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE part (
                id INTEGER PRIMARY KEY,
                message_id INTEGER NOT NULL,
                data TEXT NOT NULL
            );
            INSERT INTO session (id, title, directory)
            VALUES ('sqlite-ts', 'Test', '/tmp/project');
            INSERT INTO message (session_id, data)
            VALUES ('sqlite-ts', '{"role":"user"}');
            INSERT INTO part (message_id, data)
            VALUES (last_insert_rowid(), '{"type":"text","text":"hello"}');
            "#,
        )
        .unwrap();

        let sessions = scan(&conn, false).unwrap();
        assert_eq!(sessions.len(), 1);
        assert!(sessions[0].started_at > 0);
        assert!(sessions[0].messages[0].timestamp.is_some_and(|timestamp| timestamp > 0));
    }

    #[test]
    fn text_message_timestamps_do_not_abort_the_scan() {
        let path =
            std::env::temp_dir().join(format!("recall-opencode-txt-{}.db", uuid::Uuid::new_v4()));
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "
            CREATE TABLE session (
                id TEXT PRIMARY KEY,
                title TEXT,
                directory TEXT,
                time_created TEXT,
                time_updated TEXT
            );
            CREATE TABLE message (
                id INTEGER PRIMARY KEY,
                session_id TEXT NOT NULL,
                data TEXT NOT NULL,
                time_created TEXT
            );
            CREATE TABLE part (
                id INTEGER PRIMARY KEY,
                message_id INTEGER NOT NULL,
                data TEXT NOT NULL
            );
            ",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session (id, title, directory, time_created, time_updated)
             VALUES ('txt-1', 'Test', '/tmp/project', '2026-04-13T10:00:00Z', '2026-04-13T10:01:00Z')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message (session_id, data, time_created)
             VALUES ('txt-1', '{\"role\":\"user\"}', '2026-04-13T10:00:30Z')",
            [],
        )
        .unwrap();
        let message_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO part (message_id, data) VALUES (?1, '{\"type\":\"text\",\"text\":\"hello\"}')",
            rusqlite::params![message_id],
        )
        .unwrap();

        let sessions = scan(&conn, false).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].messages.len(), 1);
        assert_eq!(
            sessions[0].messages[0].timestamp,
            rfc3339_ms(Some(&Value::String("2026-04-13T10:00:30Z".into())))
        );
        drop(conn);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn since_cutoff_compares_normalized_second_granularity_timestamps() {
        let (path, conn) = setup_opencode_db();
        insert_session_with_message(&conn, "recent", 1_800_000_000, 1_800_000_000, "hello");
        insert_session_with_message(&conn, "old", 1_600_000_000, 1_600_000_000, "hello");

        let cutoff = 1_700_000_000_000;
        let (kept, filtered) =
            load_session_rows(&conn, Some(cutoff), None, ScanOptions::default()).unwrap();
        assert_eq!(kept.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(), vec!["recent"]);
        assert_eq!(filtered, 1);

        drop(conn);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn mixed_timestamp_representations_are_sequenced_after_normalization() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE session (
                id TEXT PRIMARY KEY,
                title TEXT,
                directory TEXT,
                time_created INTEGER,
                time_updated INTEGER
            );
            CREATE TABLE message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                data TEXT NOT NULL,
                time_created INTEGER
            );
            CREATE TABLE part (
                id TEXT PRIMARY KEY,
                message_id TEXT NOT NULL,
                data TEXT NOT NULL
            );
            INSERT INTO session VALUES ('mixed', 'Mixed', '/tmp/project', 1700000000000, 1800000000000);
            INSERT INTO message VALUES ('later', 'mixed', '{"role":"assistant","providerID":"p","modelID":"m","tokens":{"input":3}}', 1800000000);
            INSERT INTO message VALUES ('middle', 'mixed', '{"role":"assistant","providerID":"p","modelID":"m","tokens":{"input":2}}', 1750000000000);
            INSERT INTO message VALUES ('earlier', 'mixed', '{"role":"assistant","providerID":"p","modelID":"m","tokens":{"input":1}}', '2023-11-14 22:13:20');
            INSERT INTO part VALUES ('later-text', 'later', '{"type":"text","text":"later"}');
            INSERT INTO part VALUES ('middle-text', 'middle', '{"type":"text","text":"middle"}');
            INSERT INTO part VALUES ('earlier-text', 'earlier', '{"type":"text","text":"earlier"}');
            INSERT INTO part VALUES ('later-tool', 'later', '{"type":"tool-invocation","toolName":"read","input":{}}');
            INSERT INTO part VALUES ('middle-tool', 'middle', '{"type":"tool-invocation","toolName":"read","input":{}}');
            INSERT INTO part VALUES ('earlier-tool', 'earlier', '{"type":"tool-invocation","toolName":"read","input":{}}');
            "#,
        )
        .unwrap();

        let sessions = scan(&conn, true).unwrap();
        let session = &sessions[0];
        assert_eq!(
            session.messages.iter().map(|message| message.content.as_str()).collect::<Vec<_>>(),
            vec!["earlier", "middle", "later"]
        );
        assert_eq!(
            session.usage_events.iter().map(|event| event.event_key.as_str()).collect::<Vec<_>>(),
            vec!["message:earlier", "message:middle", "message:later"]
        );
        assert_eq!(
            session
                .events
                .iter()
                .filter_map(|event| event.source_event_id.as_deref())
                .collect::<Vec<_>>(),
            vec!["earlier-tool", "middle-tool", "later-tool"]
        );
    }

    #[test]
    fn timestamp_parser_migration_refreshes_existing_messages() {
        let (path, conn) = setup_opencode_db();
        conn.execute(
            "INSERT INTO session (id, title, directory, time_created, time_updated)
             VALUES ('migrate', 'Test', '/tmp/project', 1800000000000, 1800000000000)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message (id, session_id, data, time_created)
             VALUES (1, 'migrate', '{\"role\":\"user\"}', 1800000000)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part (id, message_id, data)
             VALUES (1, 1, '{\"type\":\"text\",\"text\":\"hello\"}')",
            [],
        )
        .unwrap();

        let mut old = RawSession::search_only(
            "migrate",
            Some("/tmp/project".to_string()),
            1_800_000_000_000,
            Some(1_800_000_000_000),
            None,
            vec![RawMessage {
                role: Role::User,
                content: "hello".to_string(),
                timestamp: Some(1_800_000_000),
            }],
        )
        .with_usage(Vec::new(), USAGE_PARSER_VERSION)
        .with_events(Vec::new(), EVENT_PARSER_VERSION);
        old.metadata_parser_version = Some(METADATA_PARSER_VERSION - 1);
        let store =
            crate::sync::persist_raw_session_for_conformance(setup_store(), "opencode", old)
                .unwrap();
        let session = store.get_session_by_source_id("opencode", "migrate").unwrap().unwrap();
        assert_eq!(store.get_messages(&session.id).unwrap()[0].timestamp, Some(1_800_000_000));

        let fresh = scan(&conn, true).unwrap().pop().unwrap();
        assert!(fresh.refresh_session_on_metadata_backfill);
        let store =
            crate::sync::persist_raw_session_for_conformance(store, "opencode", fresh).unwrap();
        let session = store.get_session_by_source_id("opencode", "migrate").unwrap().unwrap();
        assert_eq!(store.get_messages(&session.id).unwrap()[0].timestamp, Some(1_800_000_000_000));

        drop(conn);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn incremental_scan_skips_sessions_with_matching_updated_at_and_message_count() {
        let (path, conn) = setup_opencode_db();
        insert_session_with_message(&conn, "s1", 200, 100, "hello");

        for source in ["opencode", "kilo-code", "mimo-code", "zcode"] {
            let raw = scan(&conn, true).unwrap().pop().unwrap();
            let store =
                crate::sync::persist_raw_session_for_conformance(setup_store(), source, raw)
                    .unwrap();
            let result = scan_for_sync_conn(
                &conn,
                &AdapterSyncContext::from_store_for_test(&store, source).unwrap(),
                None,
                true,
            )
            .unwrap();
            assert!(result.sessions.is_empty());
            assert_eq!(result.stats.skipped_sessions, 1);

            seed_empty_event_state(&store, source, "s1", EVENT_PARSER_VERSION - 1, Some(200));
            let mut result = scan_for_sync_conn(
                &conn,
                &AdapterSyncContext::from_store_for_test(&store, source).unwrap(),
                None,
                true,
            )
            .unwrap();
            assert_eq!(result.sessions.len(), 1);
            assert_eq!(result.stats.skipped_sessions, 0);
            let store = crate::sync::persist_raw_session_for_conformance(
                store,
                source,
                result.sessions.pop().unwrap(),
            )
            .unwrap();
            let result = scan_for_sync_conn(
                &conn,
                &AdapterSyncContext::from_store_for_test(&store, source).unwrap(),
                None,
                true,
            )
            .unwrap();
            assert!(result.sessions.is_empty());
            assert_eq!(result.stats.skipped_sessions, 1);
        }
        drop(conn);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn incremental_scan_resyncs_same_updated_at_when_message_count_changes() {
        let (path, conn) = setup_opencode_db();
        insert_session_with_message(&conn, "s1", 200, 100, "hello");
        insert_session_with_message(&conn, "s1-second", 200, 100, "shadow");
        conn.execute("DELETE FROM session WHERE id = 's1-second'", []).unwrap();
        conn.execute("UPDATE message SET session_id = 's1' WHERE session_id = 's1-second'", [])
            .unwrap();

        let store = setup_store();
        store.insert_session(&make_session("local-s1", "s1", Some(200), 1)).unwrap();

        let result = scan_for_sync_conn(
            &conn,
            &AdapterSyncContext::from_store_for_test(&store, "opencode").unwrap(),
            None,
            true,
        )
        .unwrap();
        assert_eq!(result.stats.skipped_sessions, 0);
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].source_id, "s1");
        assert_eq!(result.sessions[0].messages.len(), 2);
        drop(conn);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn incremental_scan_returns_updated_sessions_only() {
        let (path, conn) = setup_opencode_db();
        insert_session_with_message(&conn, "s1", 220, 100, "hello");
        insert_session_with_message(&conn, "s2", 150, 100, "world");

        let store = setup_store();
        store.insert_session(&make_session("local-s1", "s1", Some(200), 1)).unwrap();
        store.insert_session(&make_session("local-s2", "s2", Some(150), 1)).unwrap();
        mark_usage_current(&store, "s2", Some(150));
        mark_event_current(&store, "s2", Some(150));
        mark_metadata_current(&store, "s2");

        let result = scan_for_sync_conn(
            &conn,
            &AdapterSyncContext::from_store_for_test(&store, "opencode").unwrap(),
            None,
            true,
        )
        .unwrap();
        assert_eq!(result.stats.skipped_sessions, 1);
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].source_id, "s1");
        assert_eq!(result.sessions[0].messages.len(), 1);
        drop(conn);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn incremental_scan_counts_filtered_sessions_for_time_scope() {
        let (path, conn) = setup_opencode_db();
        insert_session_with_message(&conn, "old", 120, 100, "old");
        insert_session_with_message(&conn, "new", 220, 200, "new");

        let store = setup_store();
        let result = scan_for_sync_conn(
            &conn,
            &AdapterSyncContext::from_store_for_test(&store, "opencode").unwrap(),
            Some(200),
            true,
        )
        .unwrap();

        assert_eq!(result.stats.filtered_sessions, 1);
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].source_id, "new");
        drop(conn);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn incremental_scan_tolerates_malformed_json_rows() {
        let (path, conn) = setup_opencode_db();
        insert_session_with_message(&conn, "good", 220, 100, "hello");
        conn.execute(
            "INSERT INTO session (id, title, directory, time_created, time_updated)
             VALUES ('bad', 'Bad', '/tmp/project', 100, 220)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message (session_id, data, time_created)
             VALUES ('bad', '{\"role\":\"user\"}', 110)",
            [],
        )
        .unwrap();
        let message_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO part (id, message_id, data)
             VALUES (NULL, ?1, 'not-json')",
            rusqlite::params![message_id],
        )
        .unwrap();

        let store = setup_store();
        let result = scan_for_sync_conn(
            &conn,
            &AdapterSyncContext::from_store_for_test(&store, "opencode").unwrap(),
            None,
            true,
        )
        .unwrap();

        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].source_id, "good");
        drop(conn);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn load_session_rows_skips_malformed_session_rows() {
        let (path, conn) = setup_opencode_db();
        insert_session_with_message(&conn, "good", 220, 100, "hello");
        conn.execute(
            "INSERT INTO session (id, title, directory, time_created, time_updated)
             VALUES ('bad', 'Bad', NULL, 100, 220)",
            [],
        )
        .unwrap();

        let (sessions, _) = load_session_rows(&conn, None, None, ScanOptions::default()).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "good");
        drop(conn);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn scan_session_messages_extracts_structured_tool_events() {
        let (path, conn) = setup_opencode_db();
        conn.execute(
            "INSERT INTO session (id, title, directory, time_created, time_updated)
             VALUES ('s1', 'Test', '/tmp/project', 100, 200)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message (session_id, data, time_created)
             VALUES ('s1', '{\"role\":\"assistant\"}', 110)",
            [],
        )
        .unwrap();
        let message_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO part (id, message_id, data)
             VALUES (NULL, ?1, ?2)",
            rusqlite::params![
                message_id,
                r#"{"type":"tool-invocation","toolCallId":"legacy-call","toolName":"readFile","input":{"path":"src/main.rs"}}"#
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part (id, message_id, data)
             VALUES (NULL, ?1, ?2)",
            rusqlite::params![
                message_id,
                r#"{"type":"tool-result","toolCallId":"legacy-call","toolName":"readFile","result":"file body"}"#
            ],
        )
        .unwrap();

        let (sessions, _) = load_session_rows(&conn, None, None, ScanOptions::default()).unwrap();
        let raw = scan_session_messages(&conn, sessions, true, ScanOptions::default()).unwrap();

        assert_eq!(raw.len(), 1);
        assert!(raw[0].messages.is_empty());
        assert_eq!(raw[0].custom_title.as_deref(), Some("Test"));
        assert_eq!(raw[0].events.len(), 2);
        assert_eq!(raw[0].events[0].kind, "file_read");
        assert_eq!(raw[0].events[0].name.as_deref(), Some("readFile"));
        assert_eq!(raw[0].events[0].target.as_deref(), Some("src/main.rs"));
        assert_eq!(raw[0].events[1].kind, "tool_result");
        assert_eq!(raw[0].events[0].files[0].operation, FileOperation::Read);
        assert_eq!(raw[0].events[0].files[0].cwd.as_deref(), Some("/tmp/project"));
        assert_eq!(raw[0].events[0].tool_call_id.as_deref(), Some("legacy-call"));
        assert_eq!(raw[0].events[1].tool_call_id, raw[0].events[0].tool_call_id);
        assert!(raw[0].events[1].files.is_empty());
        assert_eq!(raw[0].events[1].status, None);
        drop(conn);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn scan_session_messages_extracts_current_tool_and_patch_events() {
        let (path, conn) = setup_opencode_db();
        conn.execute(
            "INSERT INTO session (id, title, directory, time_created, time_updated)
             VALUES ('s1', 'Test', '/tmp/project', 100, 200)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message (session_id, data, time_created)
             VALUES ('s1', ?1, 110)",
            [r#"{"role":"assistant","path":{"cwd":"/tmp/target-worktree","root":"/tmp/project"}}"#],
        )
        .unwrap();
        let message_id = conn.last_insert_rowid();
        let long_output = "汉🦀".repeat(3000);
        let parts = [
            serde_json::json!({
                "type": "tool", "callID": "read-1", "tool": "read",
                "state": {"status": "completed", "input": {"filePath": "src/main.rs"}, "output": long_output}
            }),
            serde_json::json!({
                "type": "tool", "callID": "edit-1", "tool": "edit",
                "state": {"status": "error", "input": {"filePath": "src/main.rs", "oldString": "before", "newString": "after"}, "error": "oldString not found"}
            }),
            serde_json::json!({
                "type": "tool", "callID": "write-1", "tool": "write",
                "state": {"status": "running", "input": {"filePath": "src/new.rs", "content": "new content"}}
            }),
            serde_json::json!({
                "type": "tool", "callID": "patch-1", "tool": "apply_patch",
                "state": {"status": "completed", "input": {"patchText": "*** Begin Patch\n*** Delete File: old.txt\n*** Add File: new.txt\n+hello\n*** End Patch"}, "output": "applied"}
            }),
            serde_json::json!({"type": "patch", "hash": "abc", "files": ["src/main.rs", " README.md "]}),
        ];
        let mut part_ids = Vec::new();
        for part in &parts {
            conn.execute(
                "INSERT INTO part (id, message_id, data) VALUES (NULL, ?1, ?2)",
                rusqlite::params![message_id, part.to_string()],
            )
            .unwrap();
            part_ids.push(conn.last_insert_rowid().to_string());
        }
        let raw = scan(&conn, true).unwrap();
        assert_eq!(raw.len(), 1);
        let session = &raw[0];
        assert!(session.messages.is_empty());
        assert_eq!(session.directory.as_deref(), Some("/tmp/project"));
        assert_eq!(session.source_file_path.as_deref(), conn.path());
        assert_eq!(session.events.len(), 8);
        let events = &session.events;
        assert_eq!(events[0].files[0].operation, FileOperation::Read);
        assert_eq!(events[0].status, None);
        assert_eq!(events[1].status.as_deref(), Some("success"));
        assert_eq!(events[2].files[0].operation, FileOperation::Write);
        assert_eq!(events[3].status.as_deref(), Some("error"));
        assert_eq!(events[3].summary.as_deref(), Some("oldString not found"));
        assert_eq!(events[4].files[0].path, "src/new.rs");
        assert_eq!(events[4].status, None);
        assert_eq!(
            events[5].files.iter().map(|file| &file.operation).collect::<Vec<_>>(),
            vec![&FileOperation::Delete, &FileOperation::Write]
        );
        assert_eq!(
            events[5].files.iter().map(|file| file.path.as_str()).collect::<Vec<_>>(),
            vec!["old.txt", "new.txt"]
        );
        for (call_index, result_index, part_index) in [(0, 1, 0), (2, 3, 1), (5, 6, 3)] {
            assert_eq!(events[call_index].tool_call_id, events[result_index].tool_call_id);
            assert_eq!(
                events[call_index].tool_call_id.as_deref(),
                parts[part_index]["callID"].as_str()
            );
            assert!(events[result_index].files.is_empty());
            assert_eq!(events[result_index].source_path.as_deref(), conn.path());
            let attrs: Value =
                serde_json::from_str(events[result_index].attrs_json.as_deref().unwrap()).unwrap();
            assert_eq!(attrs["message_id"], message_id.to_string());
            assert_eq!(attrs["part_id"], part_ids[part_index]);
            assert_eq!(attrs["part"], parts[part_index]);
        }
        let observation = &events[7];
        assert_eq!(observation.tool_call_id, None);
        assert_eq!(observation.status, None);
        assert_eq!(
            observation.files.iter().map(|file| file.path.as_str()).collect::<Vec<_>>(),
            vec!["src/main.rs", " README.md "]
        );
        assert!(observation.files.iter().all(|file| file.kind == FileEvidenceKind::Observation));
        for event in events {
            for file in &event.files {
                assert_eq!(file.cwd.as_deref(), Some("/tmp/target-worktree"));
            }
        }
        let shell = serde_json::json!({"type":"tool","tool":"bash","callID":"shell-1","state":{"status":"running","input":{"command":"git restore -- src/lib.rs","workdir":"nested"}}});
        let parsed = parse_part_events(
            "shell-part",
            &shell.to_string(),
            None,
            0,
            conn.path(),
            "message",
            Some("/tmp/target-worktree"),
            ScanOptions::default(),
        );
        assert_eq!(parsed[0].files[0].cwd.as_deref(), Some("/tmp/target-worktree/nested"));
        assert_eq!(parsed[0].files[0].kind, FileEvidenceKind::Command);
        assert_eq!(parsed[0].status, None);
        let attrs: Value =
            serde_json::from_str(observation.attrs_json.as_deref().unwrap()).unwrap();
        assert_eq!(attrs["part"], parts[4]);
        assert_eq!(observation.source_event_id.as_deref(), Some(part_ids[4].as_str()));
        drop(conn);
        let _ = std::fs::remove_file(path);
    }
}
