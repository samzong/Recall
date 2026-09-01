use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, Row};
use serde_json::Value;
use tracing::{debug, warn};

use crate::adapters::events;
use crate::adapters::opencode;
use crate::adapters::{
    RawMessage, RawSession, ResumeCommand, SourceAdapter, SyncScanResult, SyncScanStats,
    first_timestamp, last_timestamp,
};
use crate::db::store::Store;
use crate::types::{ParentLink, ParentRelation, RawSessionEvent, RawUsageEvent, Role, ThreadRole};

const SOURCE: &str = "goose";
const USAGE_PARSER_VERSION: u32 = 1;
const EVENT_PARSER_VERSION: u32 = 1;
const METADATA_PARSER_VERSION: u32 = 2;
const MS_THRESHOLD: i64 = 10_000_000_000;

pub(crate) struct GooseAdapter;

struct SessionRow {
    id: String,
    working_dir: String,
    name: String,
    session_type: String,
    parent_session_id: Option<String>,
    provider_name: Option<String>,
    model_config_json: Option<String>,
    created_at: Option<i64>,
    updated_at: Option<i64>,
    message_updated_at: Option<i64>,
}

impl SessionRow {
    fn freshness(&self) -> Option<i64> {
        match (self.updated_at, self.message_updated_at) {
            (Some(session), Some(message)) => Some(session.max(message)),
            (Some(session), None) => Some(session),
            (None, Some(message)) => Some(message),
            (None, None) => None,
        }
    }
}

struct MessageRow {
    message_id: Option<String>,
    role: String,
    content_json: String,
    created_timestamp: Option<i64>,
    metadata_json: Option<String>,
}

struct UsageRow {
    id: i64,
    created_timestamp: Option<i64>,
    model: Option<String>,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
}

impl SourceAdapter for GooseAdapter {
    fn id(&self) -> &str {
        SOURCE
    }

    fn label(&self) -> &str {
        "GS"
    }

    fn usage_parser_version(&self) -> Option<u32> {
        Some(USAGE_PARSER_VERSION)
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        Some(ResumeCommand {
            program: "goose".to_string(),
            args: vec![
                "session".to_string(),
                "--resume".to_string(),
                "--session-id".to_string(),
                source_id.to_string(),
            ],
        })
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        Ok(scan_db(open_goose_db()?, None, None, true)?.sessions)
    }

    fn scan_for_sync(
        &self,
        store: &Store,
        since_ts: Option<i64>,
        include_events: bool,
    ) -> anyhow::Result<Option<SyncScanResult>> {
        Ok(Some(scan_db(open_goose_db()?, Some(store), since_ts, include_events)?))
    }

    fn prune(&self, store: &Store) -> anyhow::Result<()> {
        let Some((conn, _)) = open_goose_db()? else {
            return Ok(());
        };
        if !has_table(&conn, "sessions") {
            return Ok(());
        }
        let live = load_all_session_ids(&conn)?;
        for source_id in store.session_meta_map(SOURCE)?.keys() {
            if !live.contains(source_id) {
                store.delete_session_data(SOURCE, source_id)?;
            }
        }
        Ok(())
    }
}

fn open_goose_db() -> anyhow::Result<Option<(Connection, PathBuf)>> {
    let Some(db_path) = resolve_db_path() else {
        return Ok(None);
    };
    let Some(conn) = opencode::open_readonly(&db_path)? else {
        return Ok(None);
    };
    Ok(Some((conn, db_path)))
}

fn resolve_db_path() -> Option<PathBuf> {
    resolve_db_path_from(
        std::env::var("GOOSE_PATH_ROOT").ok(),
        std::env::var("XDG_DATA_HOME").ok(),
        dirs::home_dir(),
        std::env::var("APPDATA").ok(),
    )
}

fn resolve_db_path_from(
    goose_path_root: Option<String>,
    xdg_data_home: Option<String>,
    home: Option<PathBuf>,
    appdata: Option<String>,
) -> Option<PathBuf> {
    if let Some(root) = goose_path_root.as_deref().map(str::trim).filter(|value| !value.is_empty())
    {
        let root = PathBuf::from(root);
        if root.is_absolute() {
            let path = root.join("data/sessions/sessions.db");
            return path.exists().then_some(path);
        }
    }
    let mut candidates = Vec::new();
    if let Some(home) = home {
        let xdg = xdg_data_home
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"));
        candidates.push(xdg.join("goose/sessions/sessions.db"));
        candidates.push(home.join("Library/Application Support/Block/goose/sessions/sessions.db"));
    }
    if let Some(appdata) = appdata.as_deref().map(str::trim).filter(|value| !value.is_empty()) {
        candidates.push(PathBuf::from(appdata).join("Block/goose/data/sessions/sessions.db"));
    }
    candidates.into_iter().find(|path| path.exists())
}

fn scan_db(
    opened: Option<(Connection, PathBuf)>,
    store: Option<&Store>,
    since_ts: Option<i64>,
    include_events: bool,
) -> anyhow::Result<SyncScanResult> {
    let Some((conn, db_path)) = opened else {
        return Ok(SyncScanResult { sessions: Vec::new(), stats: SyncScanStats::default() });
    };
    if !has_table(&conn, "sessions") || !has_table(&conn, "messages") {
        debug!("Goose sessions.db missing required tables, skipping");
        return Ok(SyncScanResult { sessions: Vec::new(), stats: SyncScanStats::default() });
    }

    let existing = match store {
        Some(store) => store.session_meta_map(SOURCE)?,
        None => HashMap::new(),
    };
    let usage_state = match store {
        Some(store) => store.usage_state_meta_map(SOURCE)?,
        None => HashMap::new(),
    };
    let event_state = match store {
        Some(store) if include_events => store.event_state_meta_map(SOURCE)?,
        _ => HashMap::new(),
    };
    let metadata_state = match store {
        Some(store) => store.metadata_state_meta_map(SOURCE)?,
        None => HashMap::new(),
    };

    let rows = load_session_rows(&conn)?;
    let mut sessions = Vec::new();
    let mut stats = SyncScanStats::default();

    for row in rows {
        stats.candidates += 1;
        if !include_session_type(&row.session_type) {
            stats.rejected_before_parse += 1;
            continue;
        }
        let freshness = row.freshness();
        if since_ts.is_some_and(|cutoff| freshness.is_some_and(|updated_at| updated_at < cutoff)) {
            stats.filtered_sessions += 1;
            continue;
        }
        if store.is_some()
            && existing.get(&row.id).is_some_and(|&(old_updated_at, _)| {
                old_updated_at == freshness
                    && crate::adapters::sync_state::session_state_is_current(
                        USAGE_PARSER_VERSION,
                        EVENT_PARSER_VERSION,
                        usage_state.get(&row.id).copied(),
                        event_state.get(&row.id).copied(),
                        freshness,
                        include_events,
                    )
                    && crate::adapters::sync_state::metadata_state_is_current(
                        METADATA_PARSER_VERSION,
                        metadata_state.get(&row.id).copied(),
                        freshness,
                    )
            })
        {
            stats.skipped_sessions += 1;
            continue;
        }
        match scan_session(&conn, &row, &db_path, include_events) {
            Ok(Some(raw)) => {
                stats.parsed += 1;
                sessions.push(raw);
            }
            Ok(None) => {}
            Err(err) => warn!("failed to parse Goose session {}: {err}", row.id),
        }
    }

    Ok(SyncScanResult { sessions, stats })
}

fn include_session_type(session_type: &str) -> bool {
    matches!(session_type, "user" | "scheduled" | "acp" | "sub_agent")
}

fn load_session_rows(conn: &Connection) -> anyhow::Result<Vec<SessionRow>> {
    let columns = table_columns(conn, "sessions")?;
    let sql = format!(
        "SELECT id, working_dir, created_at, updated_at, {}, {}, {}, {}, {},
                (SELECT MAX(created_timestamp) FROM messages WHERE session_id = sessions.id)
         FROM sessions",
        col_or_lit(&columns, "name", "''"),
        col_or_lit(&columns, "session_type", "'user'"),
        col_or_null(&columns, "parent_session_id"),
        col_or_null(&columns, "provider_name"),
        col_or_null(&columns, "model_config_json"),
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        Ok(SessionRow {
            id: row.get(0)?,
            working_dir: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            created_at: sql_timestamp(row, 2),
            updated_at: sql_timestamp(row, 3),
            name: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
            session_type: row.get::<_, Option<String>>(5)?.unwrap_or_else(|| "user".to_string()),
            parent_session_id: row.get(6)?,
            provider_name: row.get(7)?,
            model_config_json: row.get(8)?,
            message_updated_at: sql_timestamp(row, 9),
        })
    })?;
    let mut sessions = Vec::new();
    for row in rows {
        match row {
            Ok(session) => sessions.push(session),
            Err(err) => warn!("skipping malformed Goose session row: {err}"),
        }
    }
    Ok(sessions)
}

fn load_all_session_ids(conn: &Connection) -> anyhow::Result<HashSet<String>> {
    let mut stmt = conn.prepare("SELECT id FROM sessions")?;
    let rows = stmt.query_map([], |row| row.get(0))?;
    let mut ids = HashSet::new();
    for row in rows {
        match row {
            Ok(id) => {
                ids.insert(id);
            }
            Err(err) => warn!("skipping malformed Goose session id: {err}"),
        }
    }
    Ok(ids)
}

fn scan_session(
    conn: &Connection,
    session: &SessionRow,
    db_path: &Path,
    include_events: bool,
) -> anyhow::Result<Option<RawSession>> {
    let message_rows = load_message_rows(conn, &session.id)?;
    let mut messages = Vec::new();
    let mut events = Vec::new();

    for row in message_rows {
        if !is_user_visible(row.metadata_json.as_deref()) {
            continue;
        }
        let Some(role) = parse_role(&row.role) else {
            continue;
        };
        let parts = match serde_json::from_str::<Vec<Value>>(&row.content_json) {
            Ok(parts) => parts,
            Err(err) => {
                warn!("skipping Goose message in {} with invalid content_json: {err}", session.id);
                continue;
            }
        };
        let timestamp = row.created_timestamp;
        if let Some(content) = extract_text(&parts) {
            messages.push(RawMessage { role, content, timestamp });
        }
        if include_events {
            for part in &parts {
                if let Some(event) = parse_tool_event(
                    part,
                    timestamp,
                    events.len() as u32,
                    row.message_id.as_deref(),
                ) {
                    events.push(event);
                }
            }
        }
    }

    let usage_events = load_usage_events(conn, session, db_path)?;
    if messages.is_empty() && usage_events.is_empty() && events.is_empty() {
        return Ok(None);
    }

    let started_at =
        first_timestamp(session.created_at, &messages, &usage_events, &events).unwrap_or(0);
    let updated_at =
        session.freshness().or_else(|| last_timestamp(None, &messages, &usage_events, &events));
    let directory = Some(session.working_dir.trim().to_string()).filter(|value| !value.is_empty());

    let mut raw = RawSession::search_only(
        session.id.clone(),
        directory,
        started_at,
        updated_at,
        None,
        messages,
    )
    .with_usage(usage_events, USAGE_PARSER_VERSION);
    raw.custom_title = Some(session.name.trim().to_string()).filter(|title| !title.is_empty());
    raw.metadata_parser_version = Some(METADATA_PARSER_VERSION);
    raw.source_file_path = db_path.to_str().map(str::to_string);
    if let Some(parent) =
        session.parent_session_id.as_deref().map(str::trim).filter(|value| !value.is_empty())
    {
        raw.thread_role = Some(ThreadRole::Subagent);
        raw.parent_links = vec![ParentLink {
            relation: ParentRelation::Spawn,
            source: SOURCE.to_string(),
            source_id: parent.to_string(),
        }];
    }
    Ok(Some(if include_events { raw.with_events(events, EVENT_PARSER_VERSION) } else { raw }))
}

fn load_message_rows(conn: &Connection, session_id: &str) -> anyhow::Result<Vec<MessageRow>> {
    let columns = table_columns(conn, "messages")?;
    let order =
        if columns.contains("id") { "created_timestamp, id" } else { "created_timestamp, rowid" };
    let sql = format!(
        "SELECT {}, role, content_json, created_timestamp, {}
         FROM messages
         WHERE session_id = ?1
         ORDER BY {order}",
        col_or_null(&columns, "message_id"),
        col_or_null(&columns, "metadata_json"),
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params![session_id], |row| {
        Ok(MessageRow {
            message_id: row.get(0)?,
            role: row.get(1)?,
            content_json: row.get(2)?,
            created_timestamp: sql_timestamp(row, 3),
            metadata_json: row.get(4)?,
        })
    })?;
    let mut messages = Vec::new();
    for row in rows {
        match row {
            Ok(message) => messages.push(message),
            Err(err) => warn!("skipping malformed Goose message row: {err}"),
        }
    }
    Ok(messages)
}

fn load_usage_events(
    conn: &Connection,
    session: &SessionRow,
    db_path: &Path,
) -> anyhow::Result<Vec<RawUsageEvent>> {
    if !has_table(conn, "usage_ledger") {
        return Ok(Vec::new());
    }
    let columns = table_columns(conn, "usage_ledger")?;
    let sql = format!(
        "SELECT {}, created_timestamp, {}, {}, {}, {}, {}
         FROM usage_ledger
         WHERE session_id = ?1
         ORDER BY created_timestamp, {}",
        col_or_null(&columns, "id"),
        col_or_null(&columns, "model"),
        col_or_zero(&columns, "input_tokens"),
        col_or_zero(&columns, "output_tokens"),
        col_or_zero(&columns, "cache_read_tokens"),
        col_or_zero(&columns, "cache_write_tokens"),
        if columns.contains("id") { "id" } else { "rowid" },
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params![session.id], |row| {
        Ok(UsageRow {
            id: row.get::<_, Option<i64>>(0)?.unwrap_or(0),
            created_timestamp: sql_timestamp(row, 1),
            model: row.get(2)?,
            input_tokens: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
            output_tokens: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
            cache_read_tokens: row.get::<_, Option<i64>>(5)?.unwrap_or(0),
            cache_write_tokens: row.get::<_, Option<i64>>(6)?.unwrap_or(0),
        })
    })?;
    let fallback_model = model_from_config(session.model_config_json.as_deref());
    let provider = session
        .provider_name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("unknown");
    let source_path = db_path.to_str().map(str::to_string);
    let mut events = Vec::new();
    for row in rows {
        let row = match row {
            Ok(row) => row,
            Err(err) => {
                warn!("skipping malformed Goose usage row in {}: {err}", session.id);
                continue;
            }
        };
        if row.input_tokens <= 0
            && row.output_tokens <= 0
            && row.cache_read_tokens <= 0
            && row.cache_write_tokens <= 0
        {
            continue;
        }
        let model = row
            .model
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .or(fallback_model.as_deref())
            .unwrap_or("unknown");
        let timestamp =
            row.created_timestamp.or(session.updated_at).or(session.created_at).unwrap_or(0);
        let mut event = RawUsageEvent::observed(
            format!("ledger:{}", row.id),
            events.len() as u32,
            timestamp,
            USAGE_PARSER_VERSION,
        );
        event.model = model.to_string();
        event.provider = provider.to_string();
        event.input_tokens = row.input_tokens.max(0);
        event.output_tokens = row.output_tokens.max(0);
        event.cache_read_tokens = row.cache_read_tokens.max(0);
        event.cache_write_tokens = row.cache_write_tokens.max(0);
        event.source_path = source_path.clone();
        event.raw_usage_json = Some(
            serde_json::json!({
                "id": row.id,
                "input_tokens": row.input_tokens,
                "output_tokens": row.output_tokens,
                "cache_read_tokens": row.cache_read_tokens,
                "cache_write_tokens": row.cache_write_tokens,
            })
            .to_string(),
        );
        events.push(event);
    }
    Ok(events)
}

fn parse_role(role: &str) -> Option<Role> {
    match role {
        "user" => Some(Role::User),
        "assistant" => Some(Role::Assistant),
        _ => None,
    }
}

fn extract_text(parts: &[Value]) -> Option<String> {
    let texts: Vec<&str> = parts.iter().filter_map(part_text).collect();
    if texts.is_empty() { None } else { Some(texts.join("\n")) }
}

fn part_text(part: &Value) -> Option<&str> {
    if part.get("type").and_then(Value::as_str) != Some("text") {
        return None;
    }
    let text = part.get("text").and_then(Value::as_str)?;
    if text.trim().is_empty() { None } else { Some(text) }
}

fn parse_tool_event(
    part: &Value,
    timestamp: Option<i64>,
    event_seq: u32,
    message_id: Option<&str>,
) -> Option<RawSessionEvent> {
    let content_type = part.get("type").and_then(Value::as_str)?;
    let source_event_id = part.get("id").and_then(Value::as_str).or(message_id).map(str::to_string);
    let context = events::EventContext {
        event_seq,
        timestamp,
        source_path: None,
        source_event_id,
        message_seq: None,
        parser_version: EVENT_PARSER_VERSION,
    };
    match content_type {
        "toolRequest" => {
            let value = part.get("toolCall")?.get("value")?;
            let name = value.get("name").and_then(Value::as_str).filter(|name| !name.is_empty())?;
            Some(events::tool_call_event(context, name.to_string(), value.get("arguments")))
        }
        "toolResponse" => Some(events::tool_result_event(context, None, None)),
        _ => None,
    }
}

fn is_user_visible(metadata_json: Option<&str>) -> bool {
    let Some(raw) = metadata_json.map(str::trim).filter(|value| !value.is_empty()) else {
        return true;
    };
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return true;
    };
    value.get("userVisible").and_then(Value::as_bool).unwrap_or(true)
}

fn model_from_config(model_config_json: Option<&str>) -> Option<String> {
    let raw = model_config_json.map(str::trim).filter(|value| !value.is_empty())?;
    let value = serde_json::from_str::<Value>(raw).ok()?;
    value
        .get("model_name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn sql_timestamp(row: &Row<'_>, idx: usize) -> Option<i64> {
    match row.get::<_, SqlValue>(idx) {
        Ok(SqlValue::Integer(value)) => Some(normalize_ts(value)),
        Ok(SqlValue::Real(value)) => Some(normalize_ts(value as i64)),
        Ok(SqlValue::Text(value)) => parse_session_timestamp(&value),
        _ => None,
    }
}

fn normalize_ts(timestamp: i64) -> i64 {
    if timestamp.abs() > MS_THRESHOLD { timestamp } else { timestamp.saturating_mul(1000) }
}

fn parse_session_timestamp(raw: &str) -> Option<i64> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(value) = raw.parse::<i64>() {
        return Some(normalize_ts(value));
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Some(dt.timestamp_millis());
    }
    const NAIVE_FMTS: &[&str] =
        &["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M:%S%.f"];
    for fmt in NAIVE_FMTS {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(raw, fmt) {
            return Some(naive.and_utc().timestamp_millis());
        }
    }
    const OFFSET_FMTS: &[&str] = &["%Y-%m-%d %H:%M:%S%z", "%Y-%m-%d %H:%M:%S%.f%z"];
    for fmt in OFFSET_FMTS {
        if let Ok(dt) = chrono::DateTime::parse_from_str(raw, fmt) {
            return Some(dt.timestamp_millis());
        }
    }
    None
}

fn has_table(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [name],
        |row| row.get::<_, i64>(0),
    )
    .is_ok()
}

fn table_columns(conn: &Connection, table: &str) -> anyhow::Result<HashSet<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    let mut columns = HashSet::new();
    for row in rows {
        columns.insert(row?);
    }
    Ok(columns)
}

fn col_or_null(columns: &HashSet<String>, name: &str) -> String {
    if columns.contains(name) { name.to_string() } else { format!("NULL AS {name}") }
}

fn col_or_zero(columns: &HashSet<String>, name: &str) -> String {
    if columns.contains(name) { name.to_string() } else { format!("0 AS {name}") }
}

fn col_or_lit(columns: &HashSet<String>, name: &str, lit: &str) -> String {
    if columns.contains(name) { name.to_string() } else { format!("{lit} AS {name}") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{schema, store::Store};
    use crate::types::Session;

    fn make_session(source_id: &str, updated_at: Option<i64>, message_count: u32) -> Session {
        Session {
            id: format!("local-{source_id}"),
            source: SOURCE.to_string(),
            source_id: source_id.to_string(),
            title: "existing".to_string(),
            directory: Some("/repo".to_string()),
            repo_remote: None,
            repo_slug: None,
            repo_name: None,
            started_at: 100,
            updated_at,
            message_count,
            entrypoint: None,
            custom_title: None,
            summary: None,
            duration_minutes: None,
            source_file_path: None,
            is_import: false,
        }
    }

    fn setup_store() -> Store {
        schema::register_sqlite_vec();
        Store::open_in_memory().unwrap()
    }

    fn write_empty(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, []).unwrap();
    }

    fn setup_goose_db(path: &Path) -> Connection {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "
            CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL DEFAULT '',
                session_type TEXT NOT NULL DEFAULT 'user',
                working_dir TEXT NOT NULL,
                created_at TIMESTAMP,
                updated_at TIMESTAMP,
                provider_name TEXT,
                model_config_json TEXT,
                parent_session_id TEXT
            );
            CREATE TABLE messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                message_id TEXT,
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content_json TEXT NOT NULL,
                created_timestamp INTEGER NOT NULL,
                metadata_json TEXT
            );
            CREATE TABLE usage_ledger (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                created_timestamp INTEGER NOT NULL,
                model TEXT,
                input_tokens INTEGER,
                output_tokens INTEGER,
                cache_read_tokens INTEGER,
                cache_write_tokens INTEGER
            );
            ",
        )
        .unwrap();
        conn
    }

    struct SessionSpec<'a> {
        id: &'a str,
        name: &'a str,
        session_type: &'a str,
        working_dir: &'a str,
        created_at: &'a str,
        updated_at: &'a str,
        parent: Option<&'a str>,
        provider: Option<&'a str>,
        model_config: Option<&'a str>,
    }

    impl Default for SessionSpec<'static> {
        fn default() -> Self {
            Self {
                id: "s1",
                name: "",
                session_type: "user",
                working_dir: "/repo",
                created_at: "100",
                updated_at: "200",
                parent: None,
                provider: None,
                model_config: None,
            }
        }
    }

    fn insert_session(conn: &Connection, spec: &SessionSpec<'_>) {
        conn.execute(
            "INSERT INTO sessions (
                id, name, session_type, working_dir, created_at, updated_at,
                parent_session_id, provider_name, model_config_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                spec.id,
                spec.name,
                spec.session_type,
                spec.working_dir,
                spec.created_at,
                spec.updated_at,
                spec.parent,
                spec.provider,
                spec.model_config
            ],
        )
        .unwrap();
    }

    fn insert_message(
        conn: &Connection,
        session_id: &str,
        role: &str,
        content_json: &str,
        created_timestamp: i64,
        metadata_json: Option<&str>,
    ) {
        conn.execute(
            "INSERT INTO messages (session_id, role, content_json, created_timestamp, metadata_json)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![session_id, role, content_json, created_timestamp, metadata_json],
        )
        .unwrap();
    }

    #[test]
    fn resume_uses_official_flags() {
        let command = GooseAdapter.resume_command("20250310_2").unwrap();
        assert_eq!(command.program, "goose");
        assert_eq!(command.args, vec!["session", "--resume", "--session-id", "20250310_2"]);
    }

    #[test]
    fn goose_path_root_wins_when_absolute_and_present() {
        let root = tempfile::tempdir().unwrap();
        let db = root.path().join("data/sessions/sessions.db");
        write_empty(&db);
        let unused = root.path().join("unused-home");
        write_empty(&unused.join("Library/Application Support/Block/goose/sessions/sessions.db"));
        let resolved = resolve_db_path_from(
            Some(root.path().to_string_lossy().into_owned()),
            None,
            Some(unused),
            None,
        )
        .unwrap();
        assert_eq!(resolved, db);
    }

    #[test]
    fn absolute_goose_path_root_does_not_fall_back() {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        write_empty(&home.path().join(".local/share/goose/sessions/sessions.db"));
        write_empty(
            &home.path().join("Library/Application Support/Block/goose/sessions/sessions.db"),
        );
        assert!(
            resolve_db_path_from(
                Some(root.path().to_string_lossy().into_owned()),
                None,
                Some(home.path().to_path_buf()),
                None,
            )
            .is_none()
        );
    }

    #[test]
    fn relative_goose_path_root_is_ignored() {
        let home = tempfile::tempdir().unwrap();
        let db = home.path().join(".local/share/goose/sessions/sessions.db");
        write_empty(&db);
        let resolved = resolve_db_path_from(
            Some("relative/root".to_string()),
            None,
            Some(home.path().to_path_buf()),
            None,
        )
        .unwrap();
        assert_eq!(resolved, db);
    }

    #[test]
    fn xdg_share_is_preferred_over_app_support() {
        let home = tempfile::tempdir().unwrap();
        let app_support =
            home.path().join("Library/Application Support/Block/goose/sessions/sessions.db");
        let xdg = home.path().join(".local/share/goose/sessions/sessions.db");
        write_empty(&app_support);
        write_empty(&xdg);
        let resolved =
            resolve_db_path_from(None, None, Some(home.path().to_path_buf()), None).unwrap();
        assert_eq!(resolved, xdg);
    }

    #[test]
    fn app_support_is_used_when_xdg_is_missing() {
        let home = tempfile::tempdir().unwrap();
        let app_support =
            home.path().join("Library/Application Support/Block/goose/sessions/sessions.db");
        write_empty(&app_support);
        let resolved =
            resolve_db_path_from(None, None, Some(home.path().to_path_buf()), None).unwrap();
        assert_eq!(resolved, app_support);
    }

    #[test]
    fn xdg_data_home_overrides_default_share() {
        let root = tempfile::tempdir().unwrap();
        let xdg = root.path().join("xdg-data");
        let db = xdg.join("goose/sessions/sessions.db");
        write_empty(&db);
        let resolved = resolve_db_path_from(
            None,
            Some(xdg.to_string_lossy().into_owned()),
            Some(PathBuf::from("/unused")),
            None,
        )
        .unwrap();
        assert_eq!(resolved, db);
    }

    #[test]
    fn appdata_block_goose_is_used_when_present() {
        let appdata = tempfile::tempdir().unwrap();
        let db = appdata.path().join("Block/goose/data/sessions/sessions.db");
        write_empty(&db);
        let resolved = resolve_db_path_from(
            None,
            None,
            Some(PathBuf::from("/no/such/home")),
            Some(appdata.path().to_string_lossy().into_owned()),
        )
        .unwrap();
        assert_eq!(resolved, db);
    }

    #[test]
    fn missing_db_is_skipped() {
        assert!(
            resolve_db_path_from(None, None, Some(PathBuf::from("/no/such/home")), None).is_none()
        );
        let result = scan_db(None, None, None, true).unwrap();
        assert!(result.sessions.is_empty());
    }

    #[test]
    fn scan_extracts_text_usage_events_and_skips_hidden() {
        let root = tempfile::tempdir().unwrap();
        let db_path = root.path().join("sessions.db");
        let conn = setup_goose_db(&db_path);
        insert_session(
            &conn,
            &SessionSpec {
                id: "20250310_2",
                name: "seed title",
                created_at: "1741615822",
                updated_at: "1741615900",
                provider: Some("openrouter"),
                model_config: Some(r#"{"model_name":"gpt-4o"}"#),
                ..SessionSpec::default()
            },
        );
        insert_message(
            &conn,
            "20250310_2",
            "user",
            r#"[{"type":"text","text":"write hello"}]"#,
            1_741_615_822,
            None,
        );
        insert_message(
            &conn,
            "20250310_2",
            "assistant",
            r#"[{"type":"text","text":"done"},{"type":"toolRequest","id":"tool123","toolCall":{"status":"success","value":{"name":"developer__text_editor","arguments":{"path":"/repo/hello.txt"}}}}]"#,
            1_741_615_823,
            None,
        );
        insert_message(
            &conn,
            "20250310_2",
            "assistant",
            r#"[{"type":"text","text":"hidden"}]"#,
            1_741_615_824,
            Some(r#"{"userVisible":false}"#),
        );
        conn.execute(
            "INSERT INTO usage_ledger (
                session_id, created_timestamp, model, input_tokens, output_tokens,
                cache_read_tokens, cache_write_tokens
             ) VALUES ('20250310_2', 1741615823, 'gpt-4o', 12, 4, 2, 1)",
            [],
        )
        .unwrap();
        drop(conn);

        let opened = opencode::open_readonly(&db_path).unwrap().map(|conn| (conn, db_path.clone()));
        let result = scan_db(opened, None, None, true).unwrap();
        assert_eq!(result.sessions.len(), 1);
        let raw = &result.sessions[0];
        assert_eq!(raw.source_id, "20250310_2");
        assert_eq!(raw.directory.as_deref(), Some("/repo"));
        assert_eq!(raw.custom_title.as_deref(), Some("seed title"));
        assert_eq!(raw.started_at, 1_741_615_822_000);
        assert_eq!(raw.updated_at, Some(1_741_615_900_000));
        assert_eq!(raw.messages.len(), 2);
        assert_eq!(raw.messages[0].content, "write hello");
        assert_eq!(raw.messages[1].content, "done");
        assert_eq!(raw.messages[0].timestamp, Some(1_741_615_822_000));
        assert_eq!(raw.usage_events.len(), 1);
        assert_eq!(raw.usage_events[0].model, "gpt-4o");
        assert_eq!(raw.usage_events[0].provider, "openrouter");
        assert_eq!(raw.usage_events[0].input_tokens, 12);
        assert_eq!(raw.usage_events[0].output_tokens, 4);
        assert_eq!(raw.usage_events[0].cache_read_tokens, 2);
        assert_eq!(raw.usage_events[0].cache_write_tokens, 1);
        assert_eq!(raw.events.len(), 1);
        assert_eq!(raw.events[0].name.as_deref(), Some("developer__text_editor"));
        assert_eq!(raw.events[0].target.as_deref(), Some("/repo/hello.txt"));
        assert!(raw.thread_role.is_none());
    }

    #[test]
    fn scan_converts_millisecond_message_timestamps() {
        let root = tempfile::tempdir().unwrap();
        let db_path = root.path().join("sessions.db");
        let conn = setup_goose_db(&db_path);
        insert_session(
            &conn,
            &SessionSpec {
                created_at: "1741615822000",
                updated_at: "1741615823000",
                ..SessionSpec::default()
            },
        );
        insert_message(
            &conn,
            "s1",
            "user",
            r#"[{"type":"text","text":"hi"}]"#,
            1_741_615_822_000,
            None,
        );
        drop(conn);

        let opened = opencode::open_readonly(&db_path).unwrap().map(|conn| (conn, db_path.clone()));
        let raw = &scan_db(opened, None, None, false).unwrap().sessions[0];
        assert_eq!(raw.started_at, 1_741_615_822_000);
        assert_eq!(raw.updated_at, Some(1_741_615_823_000));
        assert_eq!(raw.messages[0].timestamp, Some(1_741_615_822_000));
    }

    #[test]
    fn scan_skips_internal_session_types() {
        let root = tempfile::tempdir().unwrap();
        let db_path = root.path().join("sessions.db");
        let conn = setup_goose_db(&db_path);
        for (id, session_type) in [("h1", "hidden"), ("t1", "terminal"), ("g1", "gateway")] {
            insert_session(
                &conn,
                &SessionSpec { id, name: id, session_type, ..SessionSpec::default() },
            );
            insert_message(&conn, id, "user", r#"[{"type":"text","text":"nope"}]"#, 100, None);
        }
        insert_session(
            &conn,
            &SessionSpec { id: "u1", name: "keep", session_type: "acp", ..SessionSpec::default() },
        );
        insert_message(&conn, "u1", "user", r#"[{"type":"text","text":"keep"}]"#, 100, None);
        drop(conn);

        let opened = opencode::open_readonly(&db_path).unwrap().map(|conn| (conn, db_path.clone()));
        let result = scan_db(opened, None, None, false).unwrap();
        assert_eq!(result.stats.rejected_before_parse, 3);
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].source_id, "u1");
    }

    #[test]
    fn scan_links_subagent_parent() {
        let root = tempfile::tempdir().unwrap();
        let db_path = root.path().join("sessions.db");
        let conn = setup_goose_db(&db_path);
        insert_session(
            &conn,
            &SessionSpec {
                id: "child",
                name: "   ",
                session_type: "sub_agent",
                parent: Some("parent"),
                ..SessionSpec::default()
            },
        );
        insert_message(&conn, "child", "user", r#"[{"type":"text","text":"hello"}]"#, 100, None);
        drop(conn);

        let opened = opencode::open_readonly(&db_path).unwrap().map(|conn| (conn, db_path.clone()));
        let raw = &scan_db(opened, None, None, false).unwrap().sessions[0];
        assert_eq!(raw.custom_title, None);
        assert_eq!(raw.thread_role, Some(ThreadRole::Subagent));
        assert_eq!(raw.parent_links[0].source, SOURCE);
        assert_eq!(raw.parent_links[0].source_id, "parent");
        assert_eq!(raw.parent_links[0].relation, ParentRelation::Spawn);
    }

    #[test]
    fn scan_without_usage_ledger_has_no_usage() {
        let root = tempfile::tempdir().unwrap();
        let db_path = root.path().join("sessions.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "
            CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                working_dir TEXT NOT NULL,
                created_at TIMESTAMP,
                updated_at TIMESTAMP
            );
            CREATE TABLE messages (
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content_json TEXT NOT NULL,
                created_timestamp INTEGER NOT NULL
            );
            INSERT INTO sessions (id, working_dir, created_at, updated_at)
            VALUES ('s1', '/repo', 100, 200);
            INSERT INTO messages (session_id, role, content_json, created_timestamp)
            VALUES ('s1', 'user', '[{\"type\":\"text\",\"text\":\"hi\"}]', 100);
            ",
        )
        .unwrap();
        drop(conn);

        let opened = opencode::open_readonly(&db_path).unwrap().map(|conn| (conn, db_path.clone()));
        let raw = &scan_db(opened, None, None, true).unwrap().sessions[0];
        assert!(raw.usage_events.is_empty());
        assert_eq!(raw.messages[0].content, "hi");
    }

    #[test]
    fn incremental_scan_skips_current_metadata() {
        let root = tempfile::tempdir().unwrap();
        let db_path = root.path().join("sessions.db");
        let conn = setup_goose_db(&db_path);
        insert_session(&conn, &SessionSpec { name: "seed", ..SessionSpec::default() });
        insert_message(&conn, "s1", "user", r#"[{"type":"text","text":"hello"}]"#, 100, None);
        drop(conn);

        let store = setup_store();
        store.insert_session(&make_session("s1", Some(200_000), 1)).unwrap();
        store
            .persist_usage_events_for_existing_session(
                SOURCE,
                "s1",
                &[],
                USAGE_PARSER_VERSION,
                Some(200_000),
            )
            .unwrap();
        store
            .persist_session_events_for_existing_session(
                SOURCE,
                "s1",
                &[],
                EVENT_PARSER_VERSION,
                Some(200_000),
            )
            .unwrap();
        store
            .persist_topology_for_existing_session(
                SOURCE,
                "s1",
                &crate::db::store::SessionTopologyWrite {
                    thread_role: None,
                    parents: &[],
                    parser_version: Some(METADATA_PARSER_VERSION),
                },
            )
            .unwrap();

        let opened = opencode::open_readonly(&db_path).unwrap().map(|conn| (conn, db_path.clone()));
        let result = scan_db(opened, Some(&store), None, true).unwrap();
        assert_eq!(result.stats.skipped_sessions, 1);
        assert!(result.sessions.is_empty());
    }

    #[test]
    fn incremental_scan_reprocesses_when_messages_change_without_session_updated_at() {
        let root = tempfile::tempdir().unwrap();
        let db_path = root.path().join("sessions.db");
        let conn = setup_goose_db(&db_path);
        insert_session(&conn, &SessionSpec { name: "seed", ..SessionSpec::default() });
        insert_message(&conn, "s1", "user", r#"[{"type":"text","text":"hello"}]"#, 100, None);
        insert_message(&conn, "s1", "user", r#"[{"type":"text","text":"edited"}]"#, 400, None);
        drop(conn);

        let store = setup_store();
        store.insert_session(&make_session("s1", Some(200_000), 1)).unwrap();
        store
            .persist_usage_events_for_existing_session(
                SOURCE,
                "s1",
                &[],
                USAGE_PARSER_VERSION,
                Some(200_000),
            )
            .unwrap();
        store
            .persist_session_events_for_existing_session(
                SOURCE,
                "s1",
                &[],
                EVENT_PARSER_VERSION,
                Some(200_000),
            )
            .unwrap();
        store
            .persist_topology_for_existing_session(
                SOURCE,
                "s1",
                &crate::db::store::SessionTopologyWrite {
                    thread_role: None,
                    parents: &[],
                    parser_version: Some(METADATA_PARSER_VERSION),
                },
            )
            .unwrap();

        let opened = opencode::open_readonly(&db_path).unwrap().map(|conn| (conn, db_path.clone()));
        let result = scan_db(opened, Some(&store), None, true).unwrap();
        assert_eq!(result.stats.skipped_sessions, 0);
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].updated_at, Some(400_000));
        assert_eq!(result.sessions[0].messages.len(), 2);
    }

    #[test]
    fn since_ts_uses_message_freshness() {
        let root = tempfile::tempdir().unwrap();
        let db_path = root.path().join("sessions.db");
        let conn = setup_goose_db(&db_path);
        insert_session(&conn, &SessionSpec { name: "seed", ..SessionSpec::default() });
        insert_message(&conn, "s1", "user", r#"[{"type":"text","text":"edited"}]"#, 400, None);
        drop(conn);

        let opened = opencode::open_readonly(&db_path).unwrap().map(|conn| (conn, db_path.clone()));
        let result = scan_db(opened, None, Some(300_000), false).unwrap();
        assert_eq!(result.stats.filtered_sessions, 0);
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].updated_at, Some(400_000));
    }

    #[test]
    fn tool_only_session_uses_event_timestamps() {
        let root = tempfile::tempdir().unwrap();
        let db_path = root.path().join("sessions.db");
        let conn = setup_goose_db(&db_path);
        insert_session(
            &conn,
            &SessionSpec { created_at: "", updated_at: "", ..SessionSpec::default() },
        );
        insert_message(
            &conn,
            "s1",
            "assistant",
            r#"[{"type":"toolRequest","id":"tool123","toolCall":{"status":"success","value":{"name":"developer__text_editor","arguments":{"path":"/repo/hello.txt"}}}}]"#,
            400,
            None,
        );
        drop(conn);

        let opened = opencode::open_readonly(&db_path).unwrap().map(|conn| (conn, db_path.clone()));
        let raw = &scan_db(opened, None, None, true).unwrap().sessions[0];
        assert!(raw.messages.is_empty());
        assert_eq!(raw.events.len(), 1);
        assert_eq!(raw.started_at, 400_000);
        assert_eq!(raw.updated_at, Some(400_000));
    }

    #[test]
    fn prune_deletes_sessions_missing_from_goose() {
        let root = tempfile::tempdir().unwrap();
        let db_path = root.path().join("sessions.db");
        let conn = setup_goose_db(&db_path);
        insert_session(&conn, &SessionSpec { id: "keep", name: "keep", ..SessionSpec::default() });
        drop(conn);

        let store = setup_store();
        store.insert_session(&make_session("keep", Some(200_000), 1)).unwrap();
        store.insert_session(&make_session("gone", Some(200_000), 1)).unwrap();

        let opened = opencode::open_readonly(&db_path).unwrap().unwrap();
        let live = load_all_session_ids(&opened).unwrap();
        for source_id in store.session_meta_map(SOURCE).unwrap().keys() {
            if !live.contains(source_id) {
                store.delete_session_data(SOURCE, source_id).unwrap();
            }
        }
        let remaining = store.session_meta_map(SOURCE).unwrap();
        assert!(remaining.contains_key("keep"));
        assert!(!remaining.contains_key("gone"));
    }

    #[test]
    fn rfc3339_and_sqlite_timestamps_parse() {
        assert_eq!(
            parse_session_timestamp("2025-03-10T14:30:22Z"),
            Some(
                chrono::DateTime::parse_from_rfc3339("2025-03-10T14:30:22Z")
                    .unwrap()
                    .timestamp_millis()
            )
        );
        assert_eq!(
            parse_session_timestamp("2025-03-10 14:30:22"),
            Some(
                chrono::NaiveDateTime::parse_from_str("2025-03-10 14:30:22", "%Y-%m-%d %H:%M:%S")
                    .unwrap()
                    .and_utc()
                    .timestamp_millis()
            )
        );
        assert_eq!(parse_session_timestamp("1741615822"), Some(1_741_615_822_000));
    }
}
