use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde_json::Value;
use tracing::warn;

use crate::adapters::json_util::{json_i64, rfc3339_ms};
use crate::adapters::{
    AdapterSyncContext, RawMessage, RawSession, RawUsageEvent, ResumeCommand, SourceAdapter,
    SyncScanResult, SyncScanStats,
};
use crate::adapters::{opencode, paths, sync_state};
use crate::types::Role;

pub(crate) struct DevinAdapter;

const USAGE_PARSER_VERSION: u32 = 3;

struct SessionRow {
    id: String,
    working_directory: String,
    model: String,
    title: Option<String>,
    created_at: i64,
    updated_at: i64,
}

struct MessageNode {
    node_id: i64,
    parent_node_id: Option<i64>,
    message: Value,
    created_at: i64,
}

impl SourceAdapter for DevinAdapter {
    fn id(&self) -> &str {
        "devin"
    }
    fn label(&self) -> &str {
        "DVN"
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        Some(ResumeCommand::new("devin", &["--resume", source_id]))
    }

    fn start_command(&self, prompt: String) -> Option<ResumeCommand> {
        Some(ResumeCommand::new("devin", &["--", &prompt]))
    }

    fn usage_parser_version(&self) -> Option<u32> {
        Some(USAGE_PARSER_VERSION)
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let Some((conn, db_path)) = open_sessions_db()? else {
            return Ok(vec![]);
        };
        Ok(scan_db(&conn, &db_path, None, None)?.sessions)
    }

    fn scan_for_sync(
        &self,
        context: &AdapterSyncContext,
        since_ts: Option<i64>,
        _include_events: bool,
    ) -> anyhow::Result<Option<SyncScanResult>> {
        let Some((conn, db_path)) = open_sessions_db()? else {
            return Ok(Some(SyncScanResult::default()));
        };
        Ok(Some(scan_db(&conn, &db_path, Some(context), since_ts)?))
    }
}

fn sessions_db_path() -> Option<PathBuf> {
    let root = match paths::env_path_dir("DEVIN_HOME") {
        Some(dir) => dir,
        None => std::env::var_os("XDG_DATA_HOME")
            .filter(|xdg| !xdg.is_empty())
            .map(|xdg| PathBuf::from(xdg).join("devin"))
            .filter(|dir| dir.is_dir())
            .or_else(|| dirs::home_dir().map(|home| home.join(".local/share/devin")))?,
    };
    Some(root.join("cli").join("sessions.db"))
}

fn open_sessions_db() -> anyhow::Result<Option<(Connection, PathBuf)>> {
    let Some(db_path) = sessions_db_path() else {
        return Ok(None);
    };
    Ok(opencode::open_readonly(&db_path)?.map(|conn| (conn, db_path)))
}

fn scan_db(
    conn: &Connection,
    db_path: &Path,
    context: Option<&AdapterSyncContext>,
    since_ts: Option<i64>,
) -> anyhow::Result<SyncScanResult> {
    let mut stats = SyncScanStats::default();
    let mut sessions = Vec::new();
    let rows = match load_session_rows(conn, context.and_then(AdapterSyncContext::target_source_id))
    {
        Ok(rows) => rows,
        Err(err) => {
            warn!("failed to read Devin sessions from {}: {err}", db_path.display());
            return Ok(SyncScanResult { sessions, stats, observations: Vec::new() });
        }
    };
    for row in rows {
        stats.candidates += 1;
        if since_ts.is_some_and(|cutoff| row.updated_at < cutoff) {
            stats.filtered_sessions += 1;
            continue;
        }
        if context.is_some_and(|context| session_is_current(context, &row)) {
            stats.skipped_sessions += 1;
            continue;
        }
        match scan_session(conn, &row, db_path) {
            Ok(Some(raw)) => {
                stats.parsed += 1;
                sessions.push(raw);
            }
            Ok(None) => {}
            Err(err) => warn!("failed to parse Devin session {}: {err}", row.id),
        }
    }
    Ok(SyncScanResult { sessions, stats, observations: Vec::new() })
}

fn session_is_current(context: &AdapterSyncContext, row: &SessionRow) -> bool {
    let updated_at = Some(row.updated_at);
    context.session_meta().get(&row.id).is_some_and(|old| {
        old.updated_at == updated_at
            && sync_state::parser_state_is_current(
                USAGE_PARSER_VERSION,
                context.usage_state().get(&row.id).copied(),
                updated_at,
            )
    })
}

fn load_session_rows(conn: &Connection, target: Option<&str>) -> anyhow::Result<Vec<SessionRow>> {
    let mut stmt = conn.prepare(
        "SELECT s.id, s.working_directory, s.model, s.title, s.created_at,
                MAX(s.last_activity_at,
                    COALESCE((SELECT MAX(n.created_at) FROM message_nodes n
                              WHERE n.session_id = s.id), 0))
         FROM sessions s
         WHERE s.hidden = 0 AND (?1 IS NULL OR s.id = ?1)
         ORDER BY s.created_at, s.id",
    )?;
    let rows = stmt.query_map([target], |row| {
        Ok(SessionRow {
            id: row.get(0)?,
            working_directory: row.get(1)?,
            model: row.get(2)?,
            title: row.get(3)?,
            created_at: row.get(4)?,
            updated_at: row.get::<_, i64>(5)?.saturating_mul(1000),
        })
    })?;
    let mut sessions = Vec::new();
    for row in rows {
        match row {
            Ok(session) => sessions.push(session),
            Err(err) => warn!("skipping malformed Devin session row: {err}"),
        }
    }
    Ok(sessions)
}

fn load_nodes(conn: &Connection, session_id: &str) -> anyhow::Result<Vec<MessageNode>> {
    let mut stmt = conn.prepare(
        "SELECT node_id, parent_node_id, chat_message, created_at
         FROM message_nodes WHERE session_id = ?1 ORDER BY node_id",
    )?;
    let rows = stmt.query_map(rusqlite::params![session_id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, Option<i64>>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
        ))
    })?;
    let mut nodes = Vec::new();
    for row in rows {
        let (node_id, parent_node_id, chat_message, created_at) = row?;
        let message = match serde_json::from_str::<Value>(&chat_message) {
            Ok(message) => message,
            Err(err) => {
                warn!("skipping Devin message node {node_id} in session {session_id}: {err}");
                continue;
            }
        };
        nodes.push(MessageNode { node_id, parent_node_id, message, created_at });
    }
    Ok(nodes)
}

fn scan_session(
    conn: &Connection,
    session: &SessionRow,
    db_path: &Path,
) -> anyhow::Result<Option<RawSession>> {
    let nodes = load_nodes(conn, &session.id)?;
    let mut roots: HashMap<i64, i64> = HashMap::with_capacity(nodes.len());
    for node in &nodes {
        let root = node
            .parent_node_id
            .and_then(|parent| roots.get(&parent).copied())
            .unwrap_or(node.node_id);
        roots.insert(node.node_id, root);
    }
    let conversation_roots: HashSet<i64> = nodes
        .iter()
        .filter(|node| is_real_user_input(&node.message))
        .map(|node| roots[&node.node_id])
        .collect();

    let source_path = db_path.to_str().map(str::to_string);
    let mut messages = Vec::new();
    let mut seen_ids: HashSet<&str> = HashSet::new();
    for node in &nodes {
        if !conversation_roots.contains(&roots[&node.node_id]) {
            continue;
        }
        let message_id = node.message.get("message_id").and_then(Value::as_str).unwrap_or_default();
        if !seen_ids.insert(message_id) {
            continue;
        }
        let timestamp = Some(node_timestamp_ms(node));
        match node.message.get("role").and_then(Value::as_str) {
            Some("user") if is_real_user_input(&node.message) => {
                let content = user_text(&node.message);
                if content.trim().is_empty() {
                    continue;
                }
                messages.push(RawMessage { role: Role::User, content, timestamp });
            }
            Some("assistant") => {
                let content =
                    node.message.get("content").and_then(Value::as_str).unwrap_or_default();
                if content.trim().is_empty() {
                    continue;
                }
                messages.push(RawMessage {
                    role: Role::Assistant,
                    content: content.to_string(),
                    timestamp,
                });
            }
            _ => {}
        }
    }

    let usage_events = session_usage(&nodes, session, source_path.as_deref());
    if messages.is_empty() && usage_events.is_empty() {
        return Ok(None);
    }

    let mut raw = RawSession::search_only(
        session.id.clone(),
        Some(session.working_directory.clone()),
        session.created_at.saturating_mul(1000),
        Some(session.updated_at),
        None,
        messages,
    )
    .with_usage(usage_events, USAGE_PARSER_VERSION);
    raw.custom_title = session
        .title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_string);
    raw.source_file_path = source_path;
    raw.duration_minutes = match (raw.messages.first(), raw.messages.last()) {
        (Some(first), Some(last)) => match (first.timestamp, last.timestamp) {
            (Some(first), Some(last)) if last >= first => Some(((last - first) / 60_000) as u32),
            _ => None,
        },
        _ => None,
    };
    Ok(Some(raw))
}

fn node_timestamp_ms(node: &MessageNode) -> i64 {
    rfc3339_ms(node.message.pointer("/metadata/created_at"))
        .unwrap_or_else(|| node.created_at.saturating_mul(1000))
}

fn is_real_user_input(message: &Value) -> bool {
    if message.get("role").and_then(Value::as_str) != Some("user") {
        return false;
    }
    let Some(metadata) = message.get("metadata") else {
        return false;
    };
    if metadata.get("is_user_input").and_then(Value::as_bool) != Some(true) {
        return false;
    }
    metadata
        .get("extensions")
        .and_then(|extensions| extensions.get("subagent/handoff"))
        .and_then(Value::as_bool)
        != Some(true)
}

fn user_text(message: &Value) -> String {
    let content = message.get("content").and_then(Value::as_str).unwrap_or_default();
    let Some(blocks) = message
        .get("metadata")
        .and_then(|metadata| metadata.get("extensions"))
        .and_then(|extensions| extensions.get("chisel/acp-content-blocks"))
        .and_then(Value::as_array)
    else {
        return content.to_string();
    };
    let rendered = blocks.iter().filter_map(block_text).collect::<Vec<_>>().join(" ");
    if rendered.is_empty() { content.to_string() } else { rendered }
}

fn block_text(block: &Value) -> Option<String> {
    let text = match block.get("type").and_then(Value::as_str)? {
        "text" => block.get("text").and_then(Value::as_str),
        "resource_link" => block.get("name").and_then(Value::as_str),
        _ => None,
    }?;
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn session_usage(
    nodes: &[MessageNode],
    session: &SessionRow,
    source_path: Option<&str>,
) -> Vec<RawUsageEvent> {
    let mut events = Vec::new();
    let mut seen_ids: HashSet<&str> = HashSet::new();
    for node in nodes {
        if node.message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(metrics) = node
            .message
            .get("metadata")
            .and_then(|metadata| metadata.get("metrics"))
            .filter(|metrics| metrics.is_object())
        else {
            continue;
        };
        let message_id = node.message.get("message_id").and_then(Value::as_str).unwrap_or_default();
        if !seen_ids.insert(message_id) {
            continue;
        }
        let input = json_i64(metrics.get("input_tokens")).unwrap_or(0).max(0);
        let output = json_i64(metrics.get("output_tokens")).unwrap_or(0).max(0);
        let cache_read = json_i64(metrics.get("cache_read_tokens")).unwrap_or(0).max(0);
        let cache_write = json_i64(metrics.get("cache_creation_tokens")).unwrap_or(0).max(0);
        if input == 0 && output == 0 && cache_read == 0 && cache_write == 0 {
            continue;
        }
        let seq = events.len() as u32;
        let mut event = RawUsageEvent::observed(
            message_id.to_string(),
            seq,
            node_timestamp_ms(node),
            USAGE_PARSER_VERSION,
        );
        event.model = node
            .message
            .pointer("/metadata/generation_model")
            .and_then(Value::as_str)
            .filter(|model| !model.trim().is_empty())
            .unwrap_or(&session.model)
            .to_string();
        event.provider = "devin".to_string();
        event.input_tokens = input;
        event.output_tokens = output;
        event.cache_read_tokens = cache_read;
        event.cache_write_tokens = cache_write;
        event.raw_usage_json = Some(metrics.to_string());
        event.source_path = source_path.map(str::to_string);
        events.push(event);
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::test_support::{seed_empty_usage_state, store as setup_store};
    use crate::types::Session;
    use rusqlite::params;

    fn setup_db(dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let db_path = dir.join("sessions.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                working_directory TEXT NOT NULL,
                backend_type TEXT NOT NULL DEFAULT 'cli',
                model TEXT NOT NULL,
                agent_mode TEXT NOT NULL DEFAULT 'cli',
                created_at INTEGER NOT NULL,
                last_activity_at INTEGER NOT NULL,
                title TEXT,
                main_chain_id INTEGER,
                shell_last_seen_index INTEGER DEFAULT 0,
                cogs_json TEXT,
                workspace_dirs TEXT,
                hidden INTEGER NOT NULL DEFAULT 0,
                metadata TEXT
            );
            CREATE TABLE message_nodes (
                row_id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                node_id INTEGER NOT NULL,
                parent_node_id INTEGER,
                chat_message TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                metadata TEXT,
                UNIQUE(session_id, node_id)
            );",
        )
        .unwrap();
        db_path
    }

    fn insert_session(conn: &Connection, id: &str, hidden: i64) {
        conn.execute(
            "INSERT INTO sessions
             (id, working_directory, model, created_at, last_activity_at, hidden)
             VALUES (?1, '/repo', 'swe-2-high', 1000, 2000, ?2)",
            params![id, hidden],
        )
        .unwrap();
    }

    fn insert_node(
        conn: &Connection,
        session_id: &str,
        node_id: i64,
        parent_node_id: Option<i64>,
        message: &Value,
        created_at: i64,
    ) {
        conn.execute(
            "INSERT INTO message_nodes
             (session_id, node_id, parent_node_id, chat_message, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![session_id, node_id, parent_node_id, message.to_string(), created_at],
        )
        .unwrap();
    }

    fn user_message(id: &str, content: &str) -> Value {
        serde_json::json!({
            "message_id": id,
            "role": "user",
            "content": content,
            "metadata": {"is_user_input": true, "created_at": "2026-09-24T08:00:00Z"},
        })
    }

    fn assistant_message(id: &str, content: &str, metrics: Option<Value>) -> Value {
        let mut message = serde_json::json!({
            "message_id": id,
            "role": "assistant",
            "content": content,
            "metadata": {"created_at": "2026-09-24T08:00:05Z"},
        });
        if let Some(metrics) = metrics {
            message["metadata"]["metrics"] = metrics;
        }
        message
    }

    fn contents(raw: &RawSession) -> Vec<(&str, &str)> {
        raw.messages.iter().map(|m| (m.role.as_str(), m.content.as_str())).collect()
    }

    #[test]
    fn forest_dedupe_keeps_pre_compaction_messages() {
        let root = tempfile::tempdir().unwrap();
        let db_path = setup_db(root.path());
        let conn = Connection::open(&db_path).unwrap();
        insert_session(&conn, "s1", 0);
        conn.execute("UPDATE sessions SET main_chain_id = 10 WHERE id = 's1'", []).unwrap();
        insert_node(
            &conn,
            "s1",
            1,
            None,
            &serde_json::json!({"message_id": "sys1", "role": "system", "content": "prefix"}),
            1000,
        );
        insert_node(&conn, "s1", 2, Some(1), &user_message("u1", "first question"), 1001);
        insert_node(&conn, "s1", 3, Some(2), &assistant_message("a1", "answer one", None), 1002);
        insert_node(
            &conn,
            "s1",
            4,
            Some(3),
            &assistant_message("a2", "pre-compaction only", None),
            1003,
        );
        insert_node(
            &conn,
            "s1",
            10,
            None,
            &serde_json::json!({"message_id": "sys2", "role": "system", "content": "prefix v2"}),
            1100,
        );
        insert_node(&conn, "s1", 11, Some(10), &user_message("u1", "first question"), 1101);
        insert_node(&conn, "s1", 12, Some(11), &assistant_message("a3", "answer two", None), 1102);
        insert_node(&conn, "s1", 13, Some(12), &user_message("u2", "second question"), 1103);
        insert_node(&conn, "s1", 14, Some(13), &assistant_message("a4", "reply two", None), 1104);

        let result = scan_db(&conn, &db_path, None, None).unwrap();
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(
            contents(&result.sessions[0]),
            [
                ("user", "first question"),
                ("assistant", "answer one"),
                ("assistant", "pre-compaction only"),
                ("assistant", "answer two"),
                ("user", "second question"),
                ("assistant", "reply two"),
            ]
        );
    }

    #[test]
    fn subagent_and_summarizer_roots_count_usage_not_messages() {
        let root = tempfile::tempdir().unwrap();
        let db_path = setup_db(root.path());
        let conn = Connection::open(&db_path).unwrap();
        insert_session(&conn, "s1", 0);
        insert_node(&conn, "s1", 1, None, &user_message("u1", "real question"), 1000);
        insert_node(
            &conn,
            "s1",
            2,
            Some(1),
            &assistant_message(
                "a1",
                "main answer",
                Some(serde_json::json!({"input_tokens": 10, "output_tokens": 5})),
            ),
            1001,
        );
        let mut handoff = user_message("h1", "handoff brief");
        handoff["metadata"]["extensions"] = serde_json::json!({"subagent/handoff": true});
        insert_node(&conn, "s1", 20, None, &handoff, 1100);
        insert_node(
            &conn,
            "s1",
            21,
            Some(20),
            &assistant_message(
                "a2",
                "sidekick answer",
                Some(serde_json::json!({"input_tokens": 20, "output_tokens": 6})),
            ),
            1101,
        );
        let mut summarizer = user_message("sum1", "summarize this");
        summarizer["metadata"]["is_user_input"] = Value::Bool(false);
        insert_node(&conn, "s1", 30, None, &summarizer, 1200);
        insert_node(
            &conn,
            "s1",
            31,
            Some(30),
            &assistant_message(
                "a3",
                "summary",
                Some(serde_json::json!({"input_tokens": 30, "output_tokens": 7})),
            ),
            1201,
        );

        let result = scan_db(&conn, &db_path, None, None).unwrap();
        let raw = &result.sessions[0];
        assert_eq!(contents(raw), [("user", "real question"), ("assistant", "main answer")]);
        assert_eq!(raw.usage_events.len(), 3);
        let keys: Vec<_> = raw.usage_events.iter().map(|e| e.event_key.as_str()).collect();
        assert_eq!(keys, ["a1", "a2", "a3"]);
        let total_input: i64 = raw.usage_events.iter().map(|e| e.input_tokens).sum();
        assert_eq!(total_input, 60);
    }

    #[test]
    fn acp_content_blocks_render_typed_text() {
        let root = tempfile::tempdir().unwrap();
        let db_path = setup_db(root.path());
        let conn = Connection::open(&db_path).unwrap();
        insert_session(&conn, "s1", 0);
        let mut slash =
            user_message("u1", "**回复语言**：中文\n\n# Simplify\n\nReduce maintenance");
        slash["metadata"]["extensions"] = serde_json::json!({"chisel/acp-content-blocks": [{"type": "text", "text": "/simplify "}]});
        insert_node(&conn, "s1", 1, None, &slash, 1000);
        insert_node(&conn, "s1", 2, Some(1), &assistant_message("a1", "ok", None), 1001);
        let mut linked =
            user_message("u2", "[README.md](file:///repo/README.md)  这次改动太啰嗦了");
        linked["metadata"]["extensions"] = serde_json::json!({"chisel/acp-content-blocks": [
            {"type": "resource_link", "name": "README.md", "uri": "file:///repo/README.md"},
            {"type": "text", "text": "  这次改动太啰嗦了"},
        ]});
        insert_node(&conn, "s1", 3, Some(2), &linked, 1002);
        insert_node(&conn, "s1", 4, Some(3), &assistant_message("a2", "done", None), 1003);

        let result = scan_db(&conn, &db_path, None, None).unwrap();
        assert_eq!(
            contents(&result.sessions[0]),
            [
                ("user", "/simplify"),
                ("assistant", "ok"),
                ("user", "README.md 这次改动太啰嗦了"),
                ("assistant", "done"),
            ]
        );
    }

    #[test]
    fn usage_dedupes_message_ids_and_maps_tokens() {
        let root = tempfile::tempdir().unwrap();
        let db_path = setup_db(root.path());
        let conn = Connection::open(&db_path).unwrap();
        insert_session(&conn, "s1", 0);
        insert_node(&conn, "s1", 1, None, &user_message("u1", "hi"), 1000);
        let mut a1 = assistant_message(
            "a1",
            "answer",
            Some(serde_json::json!({
                "input_tokens": 100, "output_tokens": 20,
                "cache_read_tokens": 30, "cache_creation_tokens": null,
                "tpot_ms": 10.5,
            })),
        );
        a1["metadata"]["generation_model"] = Value::String("swe-1-6-slow".to_string());
        insert_node(&conn, "s1", 2, Some(1), &a1, 1001);
        let mut a1_copy = a1.clone();
        a1_copy["metadata"]["metrics"]["tpot_ms"] = serde_json::json!(99.9);
        insert_node(&conn, "s1", 10, None, &user_message("u2", "again"), 1100);
        insert_node(&conn, "s1", 11, Some(10), &a1_copy, 1101);
        insert_node(
            &conn,
            "s1",
            12,
            Some(11),
            &assistant_message(
                "a2",
                "next",
                Some(serde_json::json!({"input_tokens": 5, "output_tokens": 1})),
            ),
            1102,
        );
        insert_node(
            &conn,
            "s1",
            13,
            Some(12),
            &assistant_message(
                "a3",
                "free",
                Some(
                    serde_json::json!({"input_tokens": 0, "output_tokens": 0, "cache_read_tokens": null}),
                ),
            ),
            1103,
        );

        let result = scan_db(&conn, &db_path, None, None).unwrap();
        let events = &result.sessions[0].usage_events;
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_key, "a1");
        assert_eq!(events[0].event_seq, 0);
        assert_eq!(events[0].model, "swe-1-6-slow");
        assert_eq!(events[0].provider, "devin");
        assert_eq!(events[0].input_tokens, 100);
        assert_eq!(events[0].output_tokens, 20);
        assert_eq!(events[0].cache_read_tokens, 30);
        assert_eq!(events[0].cache_write_tokens, 0);
        assert_eq!(events[0].token_source, crate::types::TokenSource::Observed);
        assert!(events[0].raw_usage_json.as_deref().unwrap().contains("input_tokens"));
        assert_eq!(events[0].source_path, result.sessions[0].source_file_path);
        assert_eq!(events[1].event_key, "a2");
        assert_eq!(events[1].event_seq, 1);
        assert_eq!(events[1].model, "swe-2-high");
        assert_eq!(events[1].input_tokens, 5);
    }

    #[test]
    fn hidden_sessions_skipped_and_timestamps_are_ms() {
        let root = tempfile::tempdir().unwrap();
        let db_path = setup_db(root.path());
        let conn = Connection::open(&db_path).unwrap();
        for (id, hidden) in [("s1", 0), ("s2", 1)] {
            insert_session(&conn, id, hidden);
            insert_node(&conn, id, 1, None, &user_message(&format!("u-{id}"), "hi"), 1000);
            insert_node(
                &conn,
                id,
                2,
                Some(1),
                &assistant_message(&format!("a-{id}"), "hey", None),
                1001,
            );
        }

        let result = scan_db(&conn, &db_path, None, None).unwrap();
        assert_eq!(result.sessions.len(), 1);
        let raw = &result.sessions[0];
        assert_eq!(raw.source_id, "s1");
        assert_eq!(raw.started_at, 1_000_000);
        assert_eq!(raw.updated_at, Some(2_000_000));
        assert_eq!(raw.directory.as_deref(), Some("/repo"));
        assert_eq!(raw.messages[0].timestamp, Some(1_790_236_800_000));
    }

    #[test]
    fn second_scan_skips_unchanged_session() {
        let root = tempfile::tempdir().unwrap();
        let db_path = setup_db(root.path());
        let conn = Connection::open(&db_path).unwrap();
        insert_session(&conn, "s1", 0);
        insert_node(&conn, "s1", 1, None, &user_message("u1", "hi"), 1000);
        insert_node(&conn, "s1", 2, Some(1), &assistant_message("a1", "hey", None), 1001);

        let first = scan_db(&conn, &db_path, None, None).unwrap();
        assert_eq!(first.sessions.len(), 1);
        let updated_at = first.sessions[0].updated_at.unwrap();

        let store = setup_store();
        store
            .insert_session(&Session {
                source: "devin".to_string(),
                source_id: "s1".to_string(),
                title: "existing".to_string(),
                directory: Some("/repo".to_string()),
                started_at: 1_000_000,
                updated_at: Some(updated_at),
                message_count: 2,
                ..crate::types::test_support::session("local-s1")
            })
            .unwrap();
        seed_empty_usage_state(&store, "devin", "s1", USAGE_PARSER_VERSION, Some(updated_at));

        let context = AdapterSyncContext::from_store_for_test(&store, "devin").unwrap();
        let second = scan_db(&conn, &db_path, Some(&context), None).unwrap();
        assert!(second.sessions.is_empty());
        assert_eq!(second.stats.candidates, 1);
        assert_eq!(second.stats.skipped_sessions, 1);
    }

    #[test]
    fn target_source_id_restricts_scan() {
        let root = tempfile::tempdir().unwrap();
        let db_path = setup_db(root.path());
        let conn = Connection::open(&db_path).unwrap();
        for id in ["s1", "s2"] {
            insert_session(&conn, id, 0);
            insert_node(&conn, id, 1, None, &user_message(&format!("u-{id}"), "hi"), 1000);
        }
        let context = AdapterSyncContext::empty_for_test("devin").restricted_to("s1");
        let result = scan_db(&conn, &db_path, Some(&context), None).unwrap();
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].source_id, "s1");
        assert_eq!(result.stats.candidates, 1);
    }
}
