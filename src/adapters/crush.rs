use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde::Deserialize;
use serde_json::Value;
use tracing::{debug, warn};

use crate::adapters::events;
use crate::adapters::opencode;
use crate::adapters::{
    RawMessage, RawSession, ResumeCommand, SourceAdapter, SyncScanResult, SyncScanStats,
};
use crate::db::store::Store;
use crate::types::{ParentLink, ParentRelation, RawSessionEvent, RawUsageEvent, Role, ThreadRole};

const USAGE_PARSER_VERSION: u32 = 1;
const EVENT_PARSER_VERSION: u32 = 1;
const METADATA_PARSER_VERSION: u32 = 1;
const MS_THRESHOLD: i64 = 1_000_000_000_000;

pub(crate) struct CrushAdapter;

#[derive(Debug, Deserialize)]
struct ProjectList {
    #[serde(default)]
    projects: Vec<ProjectRef>,
}

#[derive(Debug, Deserialize)]
struct ProjectRef {
    path: String,
    data_dir: String,
}

struct SessionRow {
    id: String,
    parent_session_id: Option<String>,
    title: String,
    prompt_tokens: i64,
    completion_tokens: i64,
    created_at: i64,
    updated_at: i64,
}

struct MessageRow {
    id: String,
    role: String,
    parts: String,
    model: Option<String>,
    provider: Option<String>,
    created_at: i64,
}

impl SourceAdapter for CrushAdapter {
    fn id(&self) -> &str {
        "crush"
    }

    fn label(&self) -> &str {
        "CR"
    }

    fn usage_parser_version(&self) -> Option<u32> {
        Some(USAGE_PARSER_VERSION)
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        Some(ResumeCommand {
            program: "crush".to_string(),
            args: vec!["--session".to_string(), source_id.to_string()],
        })
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        Ok(scan_projects(&load_projects()?, None, None, true)?.sessions)
    }

    fn scan_for_sync(
        &self,
        store: &Store,
        since_ts: Option<i64>,
        include_events: bool,
    ) -> anyhow::Result<Option<SyncScanResult>> {
        Ok(Some(scan_projects(&load_projects()?, Some(store), since_ts, include_events)?))
    }
}

fn load_projects() -> anyhow::Result<Vec<ProjectRef>> {
    load_projects_from(resolve_projects_json())
}

fn resolve_projects_json() -> Option<PathBuf> {
    resolve_projects_json_from(
        std::env::var("CRUSH_GLOBAL_DATA").ok(),
        std::env::var("XDG_DATA_HOME").ok(),
        dirs::home_dir(),
    )
}

fn resolve_projects_json_from(
    crush_global_data: Option<String>,
    xdg_data_home: Option<String>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(dir) = crush_global_data.as_deref().map(str::trim).filter(|value| !value.is_empty())
    {
        return Some(PathBuf::from(dir).join("projects.json"));
    }
    if let Some(xdg) = xdg_data_home.as_deref().map(str::trim).filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(xdg).join("crush").join("projects.json"));
    }
    Some(home?.join(".local/share/crush/projects.json"))
}

fn load_projects_from(path: Option<PathBuf>) -> anyhow::Result<Vec<ProjectRef>> {
    let Some(path) = path else {
        return Ok(vec![]);
    };
    if !path.exists() {
        debug!("Crush projects.json not found at {}, skipping", path.display());
        return Ok(vec![]);
    }
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) => {
            warn!("failed to read Crush projects.json {}: {err}", path.display());
            return Ok(vec![]);
        }
    };
    match serde_json::from_str::<ProjectList>(&raw) {
        Ok(list) => Ok(list.projects),
        Err(err) => {
            warn!("failed to parse Crush projects.json {}: {err}", path.display());
            Ok(vec![])
        }
    }
}

fn scan_projects(
    projects: &[ProjectRef],
    store: Option<&Store>,
    since_ts: Option<i64>,
    include_events: bool,
) -> anyhow::Result<SyncScanResult> {
    let existing = match store {
        Some(store) => store.session_meta_map("crush")?,
        None => HashMap::new(),
    };
    let usage_state = match store {
        Some(store) => store.usage_state_meta_map("crush")?,
        None => HashMap::new(),
    };
    let event_state = match store {
        Some(store) if include_events => store.event_state_meta_map("crush")?,
        _ => HashMap::new(),
    };
    let metadata_state = match store {
        Some(store) => store.metadata_state_meta_map("crush")?,
        None => HashMap::new(),
    };

    let mut sessions = Vec::new();
    let mut stats = SyncScanStats::default();

    for project in projects {
        let directory = project.path.trim();
        if directory.is_empty() {
            continue;
        }
        let db_path = crush_db_path(directory, &project.data_dir);
        let Some(conn) = opencode::open_readonly(&db_path)? else {
            continue;
        };
        let rows = match load_session_rows(&conn) {
            Ok(rows) => rows,
            Err(err) => {
                warn!("failed to read Crush sessions from {}: {err}", db_path.display());
                continue;
            }
        };
        for row in rows {
            stats.candidates += 1;
            let started_at = seconds_to_ms(row.created_at);
            let updated_at = seconds_to_ms(row.updated_at);
            if since_ts.is_some_and(|cutoff| updated_at < cutoff) {
                stats.filtered_sessions += 1;
                continue;
            }
            if store.is_some()
                && existing.get(&row.id).is_some_and(|&(old_updated_at, _)| {
                    old_updated_at == Some(updated_at)
                        && crate::adapters::sync_state::session_state_is_current(
                            USAGE_PARSER_VERSION,
                            EVENT_PARSER_VERSION,
                            usage_state.get(&row.id).copied(),
                            event_state.get(&row.id).copied(),
                            Some(updated_at),
                            include_events,
                        )
                        && crate::adapters::sync_state::metadata_state_is_current(
                            METADATA_PARSER_VERSION,
                            metadata_state.get(&row.id).copied(),
                            Some(updated_at),
                        )
                })
            {
                stats.skipped_sessions += 1;
                continue;
            }
            match scan_session(
                &conn,
                &row,
                directory,
                &db_path,
                started_at,
                updated_at,
                include_events,
            ) {
                Ok(Some(raw)) => {
                    stats.parsed += 1;
                    sessions.push(raw);
                }
                Ok(None) => {}
                Err(err) => {
                    warn!("failed to parse Crush session {}: {err}", row.id);
                }
            }
        }
    }

    Ok(SyncScanResult { sessions, stats })
}

fn crush_db_path(project_path: &str, data_dir: &str) -> PathBuf {
    let data_dir = Path::new(data_dir.trim());
    if data_dir.is_absolute() {
        data_dir.join("crush.db")
    } else {
        Path::new(project_path).join(data_dir).join("crush.db")
    }
}

fn load_session_rows(conn: &Connection) -> anyhow::Result<Vec<SessionRow>> {
    let mut stmt = conn.prepare(
        "SELECT s.id, s.parent_session_id, s.title, s.prompt_tokens, s.completion_tokens,
                s.created_at,
                MAX(
                    s.updated_at,
                    COALESCE(
                        (SELECT MAX(m.updated_at) FROM messages m WHERE m.session_id = s.id),
                        s.updated_at
                    )
                )
         FROM sessions s",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(SessionRow {
            id: row.get(0)?,
            parent_session_id: row.get(1)?,
            title: row.get(2)?,
            prompt_tokens: row.get(3)?,
            completion_tokens: row.get(4)?,
            created_at: row.get(5)?,
            updated_at: row.get(6)?,
        })
    })?;
    let mut sessions = Vec::new();
    for row in rows {
        match row {
            Ok(session) => sessions.push(session),
            Err(err) => warn!("skipping malformed Crush session row: {err}"),
        }
    }
    Ok(sessions)
}

fn scan_session(
    conn: &Connection,
    session: &SessionRow,
    directory: &str,
    db_path: &Path,
    started_at: i64,
    updated_at: i64,
    include_events: bool,
) -> anyhow::Result<Option<RawSession>> {
    let messages_rows = load_message_rows(conn, &session.id)?;
    let mut messages = Vec::new();
    let mut events = Vec::new();
    let mut model = None;
    let mut provider = None;

    for row in messages_rows {
        let timestamp = seconds_to_ms(row.created_at);
        if let Some(value) = row.model.as_deref().map(str::trim).filter(|value| !value.is_empty()) {
            model = Some(value.to_string());
        }
        if let Some(value) =
            row.provider.as_deref().map(str::trim).filter(|value| !value.is_empty())
        {
            provider = Some(value.to_string());
        }
        let Some(role) = parse_role(&row.role) else {
            continue;
        };
        let parts = match serde_json::from_str::<Vec<Value>>(&row.parts) {
            Ok(parts) => parts,
            Err(err) => {
                warn!("skipping Crush message {} with invalid parts: {err}", row.id);
                continue;
            }
        };
        if let Some(content) = extract_text(&parts) {
            messages.push(RawMessage { role, content, timestamp: Some(timestamp) });
        }
        if include_events {
            for part in &parts {
                if let Some(event) =
                    parse_tool_event(&row.id, part, Some(timestamp), events.len() as u32)
                {
                    events.push(event);
                }
            }
        }
    }

    let usage_events = session_usage(session, started_at, model.as_deref(), provider.as_deref());
    if messages.is_empty() && usage_events.is_empty() && events.is_empty() {
        return Ok(None);
    }

    let mut raw = RawSession::search_only(
        session.id.clone(),
        Some(directory.to_string()),
        started_at,
        Some(updated_at),
        None,
        messages,
    )
    .with_usage(usage_events, USAGE_PARSER_VERSION);
    raw.custom_title = Some(session.title.trim().to_string()).filter(|title| !title.is_empty());
    raw.metadata_parser_version = Some(METADATA_PARSER_VERSION);
    raw.source_file_path = db_path.to_str().map(str::to_string);
    if let Some(parent) =
        session.parent_session_id.as_deref().map(str::trim).filter(|value| !value.is_empty())
    {
        raw.thread_role = Some(ThreadRole::Subagent);
        raw.parent_links = vec![ParentLink {
            relation: ParentRelation::Spawn,
            source: "crush".to_string(),
            source_id: parent.to_string(),
        }];
    }
    Ok(Some(if include_events { raw.with_events(events, EVENT_PARSER_VERSION) } else { raw }))
}

fn load_message_rows(conn: &Connection, session_id: &str) -> anyhow::Result<Vec<MessageRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, role, parts, model, provider, created_at
         FROM messages
         WHERE session_id = ?1
         ORDER BY created_at ASC, rowid ASC",
    )?;
    let rows = stmt.query_map(rusqlite::params![session_id], |row| {
        Ok(MessageRow {
            id: row.get(0)?,
            role: row.get(1)?,
            parts: row.get(2)?,
            model: row.get(3)?,
            provider: row.get(4)?,
            created_at: row.get(5)?,
        })
    })?;
    let mut messages = Vec::new();
    for row in rows {
        match row {
            Ok(message) => messages.push(message),
            Err(err) => warn!("skipping malformed Crush message row: {err}"),
        }
    }
    Ok(messages)
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
    let text = part.pointer("/data/text").or_else(|| part.get("text")).and_then(Value::as_str)?;
    if text.trim().is_empty() { None } else { Some(text) }
}

fn parse_tool_event(
    message_id: &str,
    part: &Value,
    timestamp: Option<i64>,
    event_seq: u32,
) -> Option<RawSessionEvent> {
    if part.get("type").and_then(Value::as_str) != Some("tool_call") {
        return None;
    }
    let data = part.get("data").unwrap_or(part);
    let name = data.get("name").and_then(Value::as_str).filter(|name| !name.is_empty())?;
    let input = data.get("input").and_then(Value::as_str);
    let tool_id = data.get("id").and_then(Value::as_str).unwrap_or(message_id);
    Some(events::tool_call_event_from_text(
        events::EventContext {
            event_seq,
            timestamp,
            source_path: None,
            source_event_id: Some(tool_id.to_string()),
            message_seq: None,
            parser_version: EVENT_PARSER_VERSION,
        },
        name.to_string(),
        input,
    ))
}

fn session_usage(
    session: &SessionRow,
    timestamp: i64,
    model: Option<&str>,
    provider: Option<&str>,
) -> Vec<RawUsageEvent> {
    if session.prompt_tokens <= 0 && session.completion_tokens <= 0 {
        return Vec::new();
    }
    let mut event = RawUsageEvent::observed(
        format!("session:{}", session.id),
        0,
        timestamp,
        USAGE_PARSER_VERSION,
    );
    event.model = model.unwrap_or("unknown").to_string();
    event.provider = provider.unwrap_or("unknown").to_string();
    event.input_tokens = session.prompt_tokens.max(0);
    event.output_tokens = session.completion_tokens.max(0);
    event.raw_usage_json = Some(
        serde_json::json!({
            "prompt_tokens": session.prompt_tokens,
            "completion_tokens": session.completion_tokens,
        })
        .to_string(),
    );
    vec![event]
}

fn seconds_to_ms(timestamp: i64) -> i64 {
    if timestamp.abs() >= MS_THRESHOLD { timestamp } else { timestamp.saturating_mul(1000) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{schema, store::Store};
    use crate::types::Session;

    fn make_session(source_id: &str, updated_at: Option<i64>, message_count: u32) -> Session {
        Session {
            id: format!("local-{source_id}"),
            source: "crush".to_string(),
            source_id: source_id.to_string(),
            title: "existing".to_string(),
            directory: Some("/tmp/project".to_string()),
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

    fn write_projects(root: &Path, projects: &[(&str, &str)]) -> PathBuf {
        let path = root.join("projects.json");
        let entries: Vec<Value> = projects
            .iter()
            .map(|(project, data_dir)| {
                serde_json::json!({
                    "path": project,
                    "data_dir": data_dir,
                })
            })
            .collect();
        std::fs::write(&path, serde_json::json!({ "projects": entries }).to_string()).unwrap();
        path
    }

    fn setup_crush_db(dir: &Path) -> Connection {
        std::fs::create_dir_all(dir).unwrap();
        let conn = Connection::open(dir.join("crush.db")).unwrap();
        conn.execute_batch(
            "
            CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                parent_session_id TEXT,
                title TEXT NOT NULL,
                message_count INTEGER NOT NULL DEFAULT 0,
                prompt_tokens INTEGER NOT NULL DEFAULT 0,
                completion_tokens INTEGER NOT NULL DEFAULT 0,
                cost REAL NOT NULL DEFAULT 0.0,
                updated_at INTEGER NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE messages (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                parts TEXT NOT NULL DEFAULT '[]',
                model TEXT,
                provider TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            ",
        )
        .unwrap();
        conn
    }

    struct SessionSpec<'a> {
        id: &'a str,
        title: &'a str,
        created_at: i64,
        updated_at: i64,
        prompt_tokens: i64,
        completion_tokens: i64,
        parent: Option<&'a str>,
    }

    struct MessageSpec<'a> {
        id: &'a str,
        session_id: &'a str,
        role: &'a str,
        parts: &'a str,
        created_at: i64,
        updated_at: Option<i64>,
        model: Option<&'a str>,
        provider: Option<&'a str>,
    }

    fn insert_session(conn: &Connection, spec: &SessionSpec<'_>) {
        conn.execute(
            "INSERT INTO sessions
             (id, parent_session_id, title, prompt_tokens, completion_tokens, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                spec.id,
                spec.parent,
                spec.title,
                spec.prompt_tokens,
                spec.completion_tokens,
                spec.created_at,
                spec.updated_at
            ],
        )
        .unwrap();
    }

    fn insert_message(conn: &Connection, spec: &MessageSpec<'_>) {
        conn.execute(
            "INSERT INTO messages
             (id, session_id, role, parts, model, provider, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                spec.id,
                spec.session_id,
                spec.role,
                spec.parts,
                spec.model,
                spec.provider,
                spec.created_at,
                spec.updated_at.unwrap_or(spec.created_at)
            ],
        )
        .unwrap();
    }

    #[test]
    fn resume_uses_official_flag() {
        let command = CrushAdapter.resume_command("39959662-e5f0-471f-8c30-28cd8f55b50f").unwrap();
        assert_eq!(command.program, "crush");
        assert_eq!(command.args, vec!["--session", "39959662-e5f0-471f-8c30-28cd8f55b50f"]);
    }

    #[test]
    fn default_projects_json_is_xdg_share() {
        let home = tempfile::tempdir().unwrap();
        let resolved =
            resolve_projects_json_from(None, None, Some(home.path().to_path_buf())).unwrap();
        assert_eq!(resolved, home.path().join(".local/share/crush/projects.json"));
    }

    #[test]
    fn crush_global_data_wins() {
        let resolved = resolve_projects_json_from(
            Some("/tmp/crush-data".to_string()),
            Some("/unused".to_string()),
            Some(PathBuf::from("/unused-home")),
        )
        .unwrap();
        assert_eq!(resolved, PathBuf::from("/tmp/crush-data/projects.json"));
    }

    #[test]
    fn seconds_are_converted_to_milliseconds() {
        assert_eq!(seconds_to_ms(1788279318), 1_788_279_318_000);
        assert_eq!(seconds_to_ms(1_788_279_318_000), 1_788_279_318_000);
    }

    #[test]
    fn scan_reads_text_title_usage_and_tool_events() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("repo");
        let data_dir = project.join(".crush");
        let conn = setup_crush_db(&data_dir);
        insert_session(
            &conn,
            &SessionSpec {
                id: "ses-1",
                title: "seed title",
                created_at: 1788279318,
                updated_at: 1788279401,
                prompt_tokens: 19923,
                completion_tokens: 161,
                parent: None,
            },
        );
        insert_message(
            &conn,
            &MessageSpec {
                id: "m1",
                session_id: "ses-1",
                role: "user",
                parts: r#"[{"type":"text","data":{"text":"write hello-crush.txt"}},{"type":"finish","data":{"reason":"stop"}}]"#,
                created_at: 1788279318,
                updated_at: None,
                model: None,
                provider: None,
            },
        );
        insert_message(
            &conn,
            &MessageSpec {
                id: "m2",
                session_id: "ses-1",
                role: "assistant",
                parts: r#"[{"type":"tool_call","data":{"id":"call-1","name":"write","input":"{\"file_path\":\"/repo/hello-crush.txt\"}"}},{"type":"finish","data":{"reason":"end_turn"}}]"#,
                created_at: 1788279356,
                updated_at: None,
                model: Some("auto"),
                provider: Some("aihubmix"),
            },
        );
        insert_message(
            &conn,
            &MessageSpec {
                id: "m3",
                session_id: "ses-1",
                role: "assistant",
                parts: "[]",
                created_at: 1788279401,
                updated_at: None,
                model: Some("auto"),
                provider: Some("aihubmix"),
            },
        );
        drop(conn);

        let projects = vec![ProjectRef {
            path: project.to_string_lossy().into_owned(),
            data_dir: data_dir.to_string_lossy().into_owned(),
        }];
        let result = scan_projects(&projects, None, None, true).unwrap();
        assert_eq!(result.sessions.len(), 1);
        let raw = &result.sessions[0];
        assert_eq!(raw.source_id, "ses-1");
        assert_eq!(raw.directory.as_deref(), Some(project.to_str().unwrap()));
        assert_eq!(raw.custom_title.as_deref(), Some("seed title"));
        assert_eq!(raw.started_at, 1_788_279_318_000);
        assert_eq!(raw.updated_at, Some(1_788_279_401_000));
        assert_eq!(raw.metadata_parser_version, Some(METADATA_PARSER_VERSION));
        assert_eq!(raw.messages.len(), 1);
        assert_eq!(raw.messages[0].content, "write hello-crush.txt");
        assert_eq!(raw.usage_events.len(), 1);
        assert_eq!(raw.usage_events[0].input_tokens, 19923);
        assert_eq!(raw.usage_events[0].output_tokens, 161);
        assert_eq!(raw.usage_events[0].provider, "aihubmix");
        assert_eq!(raw.events.len(), 1);
        assert_eq!(raw.events[0].name.as_deref(), Some("write"));
        assert_eq!(raw.events[0].target.as_deref(), Some("/repo/hello-crush.txt"));
        assert!(raw.thread_role.is_none());
    }

    #[test]
    fn scan_links_child_sessions_and_skips_blank_titles() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("repo");
        let data_dir = project.join(".crush");
        let conn = setup_crush_db(&data_dir);
        insert_session(
            &conn,
            &SessionSpec {
                id: "child",
                title: "   ",
                created_at: 100,
                updated_at: 200,
                prompt_tokens: 0,
                completion_tokens: 0,
                parent: Some("parent"),
            },
        );
        insert_message(
            &conn,
            &MessageSpec {
                id: "m1",
                session_id: "child",
                role: "user",
                parts: r#"[{"type":"text","data":{"text":"hello"}}]"#,
                created_at: 100,
                updated_at: None,
                model: None,
                provider: None,
            },
        );
        drop(conn);

        let result = scan_projects(
            &[ProjectRef {
                path: project.to_string_lossy().into_owned(),
                data_dir: data_dir.to_string_lossy().into_owned(),
            }],
            None,
            None,
            false,
        )
        .unwrap();
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].custom_title, None);
        assert_eq!(result.sessions[0].thread_role, Some(ThreadRole::Subagent));
        assert_eq!(result.sessions[0].parent_links[0].source_id, "parent");
        assert!(result.sessions[0].events.is_empty());
    }

    #[test]
    fn missing_registry_is_empty() {
        let projects = load_projects_from(Some(PathBuf::from("/no/such/projects.json"))).unwrap();
        assert!(projects.is_empty());
        let result = scan_projects(&projects, None, None, true).unwrap();
        assert!(result.sessions.is_empty());
    }

    #[test]
    fn incremental_scan_skips_current_metadata() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("repo");
        let data_dir = project.join(".crush");
        let conn = setup_crush_db(&data_dir);
        insert_session(
            &conn,
            &SessionSpec {
                id: "ses-1",
                title: "seed",
                created_at: 100,
                updated_at: 200,
                prompt_tokens: 1,
                completion_tokens: 1,
                parent: None,
            },
        );
        insert_message(
            &conn,
            &MessageSpec {
                id: "m1",
                session_id: "ses-1",
                role: "user",
                parts: r#"[{"type":"text","data":{"text":"hello"}}]"#,
                created_at: 100,
                updated_at: None,
                model: None,
                provider: None,
            },
        );
        drop(conn);

        let store = setup_store();
        store.insert_session(&make_session("ses-1", Some(200_000), 1)).unwrap();
        store
            .persist_usage_events_for_existing_session(
                "crush",
                "ses-1",
                &[],
                USAGE_PARSER_VERSION,
                Some(200_000),
            )
            .unwrap();
        store
            .persist_session_events_for_existing_session(
                "crush",
                "ses-1",
                &[],
                EVENT_PARSER_VERSION,
                Some(200_000),
            )
            .unwrap();
        store
            .persist_topology_for_existing_session(
                "crush",
                "ses-1",
                &crate::db::store::SessionTopologyWrite {
                    thread_role: None,
                    parents: &[],
                    parser_version: Some(METADATA_PARSER_VERSION),
                },
            )
            .unwrap();

        let result = scan_projects(
            &[ProjectRef {
                path: project.to_string_lossy().into_owned(),
                data_dir: data_dir.to_string_lossy().into_owned(),
            }],
            Some(&store),
            None,
            true,
        )
        .unwrap();
        assert!(result.sessions.is_empty());
        assert_eq!(result.stats.skipped_sessions, 1);
    }

    #[test]
    fn incremental_scan_observes_message_only_updates() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("repo");
        let data_dir = project.join(".crush");
        let conn = setup_crush_db(&data_dir);
        insert_session(
            &conn,
            &SessionSpec {
                id: "ses-1",
                title: "seed",
                created_at: 100,
                updated_at: 200,
                prompt_tokens: 1,
                completion_tokens: 1,
                parent: None,
            },
        );
        insert_message(
            &conn,
            &MessageSpec {
                id: "m1",
                session_id: "ses-1",
                role: "user",
                parts: r#"[{"type":"text","data":{"text":"hello"}}]"#,
                created_at: 100,
                updated_at: Some(300),
                model: None,
                provider: None,
            },
        );
        drop(conn);

        let store = setup_store();
        store.insert_session(&make_session("ses-1", Some(200_000), 1)).unwrap();
        store
            .persist_usage_events_for_existing_session(
                "crush",
                "ses-1",
                &[],
                USAGE_PARSER_VERSION,
                Some(200_000),
            )
            .unwrap();
        store
            .persist_session_events_for_existing_session(
                "crush",
                "ses-1",
                &[],
                EVENT_PARSER_VERSION,
                Some(200_000),
            )
            .unwrap();
        store
            .persist_topology_for_existing_session(
                "crush",
                "ses-1",
                &crate::db::store::SessionTopologyWrite {
                    thread_role: None,
                    parents: &[],
                    parser_version: Some(METADATA_PARSER_VERSION),
                },
            )
            .unwrap();

        let result = scan_projects(
            &[ProjectRef {
                path: project.to_string_lossy().into_owned(),
                data_dir: data_dir.to_string_lossy().into_owned(),
            }],
            Some(&store),
            None,
            true,
        )
        .unwrap();
        assert_eq!(result.stats.skipped_sessions, 0);
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].updated_at, Some(300_000));
    }

    #[test]
    fn messages_with_equal_timestamps_keep_insertion_order() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("repo");
        let data_dir = project.join(".crush");
        let conn = setup_crush_db(&data_dir);
        insert_session(
            &conn,
            &SessionSpec {
                id: "ses-1",
                title: "seed",
                created_at: 1788279271,
                updated_at: 1788279271,
                prompt_tokens: 0,
                completion_tokens: 0,
                parent: None,
            },
        );
        insert_message(
            &conn,
            &MessageSpec {
                id: "zzzzzzzz-e5f0-471f-8c30-28cd8f55b50f",
                session_id: "ses-1",
                role: "user",
                parts: r#"[{"type":"text","data":{"text":"first"}}]"#,
                created_at: 1788279271,
                updated_at: None,
                model: None,
                provider: None,
            },
        );
        insert_message(
            &conn,
            &MessageSpec {
                id: "00000000-b5f7-4e2d-b051-47f3230a7cec",
                session_id: "ses-1",
                role: "assistant",
                parts: r#"[{"type":"text","data":{"text":"second"}}]"#,
                created_at: 1788279271,
                updated_at: None,
                model: None,
                provider: None,
            },
        );
        drop(conn);

        let result = scan_projects(
            &[ProjectRef {
                path: project.to_string_lossy().into_owned(),
                data_dir: data_dir.to_string_lossy().into_owned(),
            }],
            None,
            None,
            false,
        )
        .unwrap();
        assert_eq!(result.sessions[0].messages.len(), 2);
        assert_eq!(result.sessions[0].messages[0].role, Role::User);
        assert_eq!(result.sessions[0].messages[0].content, "first");
        assert_eq!(result.sessions[0].messages[1].role, Role::Assistant);
        assert_eq!(result.sessions[0].messages[1].content, "second");
    }

    #[test]
    fn load_projects_reads_registry() {
        let root = tempfile::tempdir().unwrap();
        let path = write_projects(root.path(), &[("/repo", "/repo/.crush")]);
        let projects = load_projects_from(Some(path)).unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].path, "/repo");
        assert_eq!(projects[0].data_dir, "/repo/.crush");
    }
}
