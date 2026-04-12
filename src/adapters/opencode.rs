use std::collections::HashMap;

use rusqlite::{Connection, OpenFlags, params_from_iter};
use serde_json::Value;
use tracing::debug;

use crate::adapters::{
    RawMessage, RawSession, ResumeCommand, SourceAdapter, SourceScanSummary, SyncScanResult,
    SyncScanStats,
};
use crate::db::store::Store;
use crate::types::Role;

const MAX_SQL_VARS_PER_BATCH: usize = 900;
const PARSED_PART_FILTER_SQL: &str = "
    json_valid(m.data)
    AND json_valid(p.data)
    AND json_extract(m.data, '$.role') IN ('user', 'assistant')
    AND (
        (json_extract(p.data, '$.type') = 'text'
            AND NULLIF(TRIM(CAST(json_extract(p.data, '$.text') AS TEXT)), '') IS NOT NULL)
        OR (json_extract(p.data, '$.type') = 'tool-invocation'
            AND json_type(p.data, '$.input') IS NOT NULL)
        OR (json_extract(p.data, '$.type') = 'tool-result'
            AND json_type(p.data, '$.result') IS NOT NULL)
    )
";

pub struct OpenCodeAdapter;

struct SessionRow {
    id: String,
    directory: String,
    time_created: i64,
    time_updated: Option<i64>,
}

impl SourceAdapter for OpenCodeAdapter {
    fn id(&self) -> &str {
        "opencode"
    }

    fn label(&self) -> &str {
        "OC"
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        Some(ResumeCommand {
            program: "opencode".to_string(),
            args: vec!["--session".to_string(), source_id.to_string()],
        })
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let Some(conn) = open_opencode_db()? else {
            return Ok(vec![]);
        };
        let sessions = load_session_rows(&conn, None)?;
        scan_session_messages(&conn, sessions)
    }

    fn scan_summary(&self) -> anyhow::Result<Option<SourceScanSummary>> {
        let Some(conn) = open_opencode_db()? else {
            return Ok(Some(SourceScanSummary {
                sessions: 0,
                messages: 0,
                oldest_started_at: None,
                newest_started_at: None,
            }));
        };

        let sessions: usize =
            conn.query_row("SELECT COUNT(*) FROM session", [], |row| row.get(0))?;
        let oldest_started_at =
            conn.query_row("SELECT MIN(time_created) FROM session", [], |row| row.get(0))?;
        let newest_started_at =
            conn.query_row("SELECT MAX(time_created) FROM session", [], |row| row.get(0))?;
        let messages = count_total_parsed_messages(&conn)?;

        Ok(Some(SourceScanSummary { sessions, messages, oldest_started_at, newest_started_at }))
    }

    fn scan_for_sync(
        &self,
        store: &Store,
        since_ts: Option<i64>,
    ) -> anyhow::Result<Option<SyncScanResult>> {
        let Some(conn) = open_opencode_db()? else {
            return Ok(Some(SyncScanResult { sessions: vec![], stats: SyncScanStats::default() }));
        };

        Ok(Some(scan_for_sync_conn(&conn, store, since_ts, self.id())?))
    }
}

fn open_opencode_db() -> anyhow::Result<Option<Connection>> {
    let db_path = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("no home dir"))?
        .join(".local/share/opencode/opencode.db");

    if !db_path.exists() {
        debug!("OpenCode DB not found at {}, skipping", db_path.display());
        return Ok(None);
    }

    Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map(Some)
        .map_err(Into::into)
}

fn count_filtered_sessions(conn: &Connection, since_ts: Option<i64>) -> anyhow::Result<u32> {
    let Some(cutoff) = since_ts else {
        return Ok(0);
    };

    conn.query_row(
        "SELECT COUNT(*)
         FROM session
         WHERE COALESCE(time_updated, time_created) < ?1",
        rusqlite::params![cutoff],
        |row| row.get::<_, i64>(0),
    )
    .map(|count| count as u32)
    .map_err(Into::into)
}

fn count_total_parsed_messages(conn: &Connection) -> anyhow::Result<usize> {
    let sql = format!(
        "SELECT COUNT(*)
         FROM message m
         JOIN part p ON p.message_id = m.id
         WHERE {PARSED_PART_FILTER_SQL}"
    );
    conn.query_row(&sql, [], |row| row.get(0)).map_err(Into::into)
}

fn load_session_rows(conn: &Connection, since_ts: Option<i64>) -> anyhow::Result<Vec<SessionRow>> {
    let sql = if since_ts.is_some() {
        "SELECT id, directory, time_created, time_updated
         FROM session
         WHERE COALESCE(time_updated, time_created) >= ?1"
    } else {
        "SELECT id, directory, time_created, time_updated FROM session"
    };

    let mut stmt = conn.prepare(sql)?;
    let rows = if let Some(cutoff) = since_ts {
        stmt.query_map(rusqlite::params![cutoff], map_session_row)?
    } else {
        stmt.query_map([], map_session_row)?
    };

    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

fn map_session_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        id: row.get(0)?,
        directory: row.get(1)?,
        time_created: row.get(2)?,
        time_updated: row.get(3)?,
    })
}

fn scan_session_messages(
    conn: &Connection,
    sessions: Vec<SessionRow>,
) -> anyhow::Result<Vec<RawSession>> {
    if sessions.is_empty() {
        return Ok(vec![]);
    }

    let session_ids: Vec<String> = sessions.iter().map(|session| session.id.clone()).collect();
    let mut session_messages: HashMap<String, Vec<RawMessage>> = HashMap::new();

    for chunk in session_ids.chunks(MAX_SQL_VARS_PER_BATCH) {
        load_message_chunk(conn, chunk, &mut session_messages)?;
    }

    let mut raw_sessions = Vec::new();
    for session in sessions {
        let messages = session_messages.remove(&session.id).unwrap_or_default();
        if messages.is_empty() {
            continue;
        }

        raw_sessions.push(RawSession {
            source_id: session.id,
            directory: Some(session.directory),
            started_at: session.time_created,
            updated_at: session.time_updated,
            entrypoint: None,
            messages,
        });
    }

    Ok(raw_sessions)
}

fn load_message_chunk(
    conn: &Connection,
    session_ids: &[String],
    session_messages: &mut HashMap<String, Vec<RawMessage>>,
) -> anyhow::Result<()> {
    let placeholders = std::iter::repeat_n("?", session_ids.len()).collect::<Vec<_>>().join(", ");
    let sql = format!(
        "SELECT m.session_id, json_extract(m.data, '$.role') AS role, p.data, m.time_created
         FROM message m
         JOIN part p ON p.message_id = m.id
         WHERE m.session_id IN ({placeholders})
           AND {PARSED_PART_FILTER_SQL}
         ORDER BY m.time_created, p.id"
    );

    let mut stmt = conn.prepare(&sql)?;
    let msg_rows = stmt.query_map(params_from_iter(session_ids.iter()), |row| {
        let session_id: String = row.get(0)?;
        let role: Option<String> = row.get(1)?;
        let part_data: String = row.get(2)?;
        let timestamp: Option<i64> = row.get(3)?;
        Ok((session_id, role, part_data, timestamp))
    })?;

    for row in msg_rows {
        let (session_id, role_str, part_data, timestamp) = row?;
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

fn load_message_counts(
    conn: &Connection,
    session_ids: &[String],
) -> anyhow::Result<HashMap<String, u32>> {
    let mut counts = HashMap::new();

    for chunk in session_ids.chunks(MAX_SQL_VARS_PER_BATCH) {
        let placeholders = std::iter::repeat_n("?", chunk.len()).collect::<Vec<_>>().join(", ");
        let sql = format!(
            "SELECT m.session_id, COUNT(*)
             FROM message m
             JOIN part p ON p.message_id = m.id
             WHERE m.session_id IN ({placeholders})
               AND {PARSED_PART_FILTER_SQL}
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
        "tool-invocation" | "tool-result" => {
            let name = part.get("toolName").and_then(|n| n.as_str()).unwrap_or("tool");
            if let Some(input) = part.get("input") {
                Some(format!("[{name}] {input}"))
            } else {
                part.get("result").map(|result| format!("[{name}] {result}"))
            }
        }
        _ => None,
    }
}

fn scan_for_sync_conn(
    conn: &Connection,
    store: &Store,
    since_ts: Option<i64>,
    source_id: &str,
) -> anyhow::Result<SyncScanResult> {
    let filtered_sessions = count_filtered_sessions(conn, since_ts)?;
    let sessions = load_session_rows(conn, since_ts)?;
    let existing = store.session_meta_map(source_id)?;
    let current_counts = load_message_counts(
        conn,
        &sessions.iter().map(|session| session.id.clone()).collect::<Vec<_>>(),
    )?;

    let mut stats = SyncScanStats { filtered_sessions, ..Default::default() };
    let mut candidates = Vec::new();

    for session in sessions {
        if let Some(&(old_updated_at, old_message_count)) = existing.get(&session.id) {
            let current_message_count = current_counts.get(&session.id).copied().unwrap_or(0);
            if session.time_updated == old_updated_at && current_message_count == old_message_count
            {
                stats.skipped_sessions += 1;
                continue;
            }
        }
        candidates.push(session);
    }

    let sessions = scan_session_messages(conn, candidates)?;
    Ok(SyncScanResult { sessions, stats })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::db::{schema, store::Store};
    use crate::types::Session;

    fn make_session(
        id: &str,
        source_id: &str,
        updated_at: Option<i64>,
        message_count: u32,
    ) -> Session {
        Session {
            id: id.to_string(),
            source: "opencode".to_string(),
            source_id: source_id.to_string(),
            title: "existing".to_string(),
            directory: Some("/tmp/project".to_string()),
            started_at: 100,
            updated_at,
            message_count,
            entrypoint: None,
        }
    }

    fn setup_store() -> Store {
        schema::register_sqlite_vec();
        Store::open_in_memory().unwrap()
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

    #[test]
    fn incremental_scan_skips_sessions_with_matching_updated_at_and_message_count() {
        let (path, conn) = setup_opencode_db();
        insert_session_with_message(&conn, "s1", 200, 100, "hello");

        let store = setup_store();
        store.insert_session(&make_session("local-s1", "s1", Some(200), 1)).unwrap();

        let result = scan_for_sync_conn(&conn, &store, None, "opencode").unwrap();
        assert!(result.sessions.is_empty());
        assert_eq!(result.stats.skipped_sessions, 1);
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

        let result = scan_for_sync_conn(&conn, &store, None, "opencode").unwrap();
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

        let result = scan_for_sync_conn(&conn, &store, None, "opencode").unwrap();
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
        let result = scan_for_sync_conn(&conn, &store, Some(200), "opencode").unwrap();

        assert_eq!(result.stats.filtered_sessions, 1);
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].source_id, "new");
        drop(conn);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn summary_reports_counts_without_full_scan() {
        let (path, conn) = setup_opencode_db();
        insert_session_with_message(&conn, "s1", 220, 100, "hello");
        insert_session_with_message(&conn, "s2", 250, 200, "world");

        let summary = SourceScanSummary {
            sessions: conn.query_row("SELECT COUNT(*) FROM session", [], |row| row.get(0)).unwrap(),
            messages: count_total_parsed_messages(&conn).unwrap(),
            oldest_started_at: conn
                .query_row("SELECT MIN(time_created) FROM session", [], |row| row.get(0))
                .unwrap(),
            newest_started_at: conn
                .query_row("SELECT MAX(time_created) FROM session", [], |row| row.get(0))
                .unwrap(),
        };

        assert_eq!(summary.sessions, 2);
        assert_eq!(summary.messages, 2);
        assert_eq!(summary.oldest_started_at, Some(100));
        assert_eq!(summary.newest_started_at, Some(200));
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
        let result = scan_for_sync_conn(&conn, &store, None, "opencode").unwrap();

        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].source_id, "good");
        drop(conn);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn parse_part_content_preserves_non_blank_whitespace() {
        let parsed = parse_part_content("{\"type\":\"text\",\"text\":\"  hello  \"}");
        assert_eq!(parsed.as_deref(), Some("  hello  "));
        assert_eq!(parse_part_content("{\"type\":\"text\",\"text\":\"   \"}"), None);
    }
}
