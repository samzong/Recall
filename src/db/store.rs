use anyhow::Result;
use rusqlite::Connection;

use crate::types::Session;

pub(crate) const SESSION_COLUMNS: &str = "id, source, source_id, title, directory, repo_remote, repo_slug, repo_name, started_at, updated_at, message_count, entrypoint, custom_title, summary, duration_minutes, source_file_path, is_import";

pub(crate) fn session_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        id: row.get(0)?,
        source: row.get(1)?,
        source_id: row.get(2)?,
        title: row.get(3)?,
        directory: row.get(4)?,
        repo_remote: row.get(5)?,
        repo_slug: row.get(6)?,
        repo_name: row.get(7)?,
        started_at: row.get(8)?,
        updated_at: row.get(9)?,
        message_count: row.get(10)?,
        entrypoint: row.get(11)?,
        custom_title: row.get(12)?,
        summary: row.get(13)?,
        duration_minutes: row.get::<_, Option<i64>>(14)?.map(|v| v as u32),
        source_file_path: row.get(15)?,
        is_import: row.get(16)?,
    })
}

pub(crate) struct Store {
    pub(crate) conn: Connection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectDirectory {
    pub(crate) directory: String,
    pub(crate) sessions: u64,
    pub(crate) last_seen: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionPath {
    pub(crate) source_id: String,
    pub(crate) directory: Option<String>,
    pub(crate) source_file_path: Option<String>,
    pub(crate) repo_remote: Option<String>,
    pub(crate) repo_slug: Option<String>,
    pub(crate) repo_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionListSort {
    Newest,
    Oldest,
    Updated,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct UsageSessionStateMeta {
    pub(crate) parser_version: u32,
    pub(crate) source_updated_at: Option<i64>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct EventSessionStateMeta {
    pub(crate) parser_version: u32,
    pub(crate) source_updated_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub(crate) struct SkillAuditEventRow {
    pub(crate) session_id: String,
    #[allow(dead_code)] // selected from session_events.source
    pub(crate) source: String,
    pub(crate) timestamp: Option<i64>,
    pub(crate) name: Option<String>,
    pub(crate) target: Option<String>,
    pub(crate) attrs_json: Option<String>,
}

impl Store {
    pub(crate) fn open() -> Result<Self> {
        let data_dir = dirs::data_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot determine data directory"))?
            .join("recall");
        std::fs::create_dir_all(&data_dir)?;
        let db_path = data_dir.join("recall.db");
        let conn = Connection::open(&db_path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA busy_timeout=5000;
             PRAGMA foreign_keys=ON;",
        )?;
        crate::db::schema::init(&conn)?;
        Ok(Store { conn })
    }

    #[cfg(test)]
    pub(crate) fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON;")?;
        crate::db::schema::init(&conn)?;
        Ok(Store { conn })
    }
}

#[cfg(test)]
mod exclusion_tests {
    use super::*;
    use crate::types::Session;

    fn sess(id: &str, dir: Option<&str>) -> Session {
        Session {
            id: id.to_string(),
            source: "claude-code".to_string(),
            source_id: format!("src-{id}"),
            title: "t".to_string(),
            directory: dir.map(String::from),
            repo_remote: None,
            repo_slug: None,
            repo_name: None,
            started_at: 0,
            updated_at: Some(1),
            message_count: 0,
            entrypoint: None,
            custom_title: None,
            summary: None,
            duration_minutes: None,
            source_file_path: None,
            is_import: false,
        }
    }

    #[test]
    fn imported_source_ids_and_clear_import_marker_round_trip() {
        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let mut imported = sess("a", None);
        imported.is_import = true;
        store.insert_session(&imported).unwrap();
        store.insert_session(&sess("b", None)).unwrap();

        let ids = store.imported_source_ids("claude-code").unwrap();
        assert_eq!(ids, std::collections::HashSet::from(["src-a".to_string()]));

        store.clear_import_marker("claude-code", "src-a").unwrap();
        assert!(store.imported_source_ids("claude-code").unwrap().is_empty());
        let sessions = store.list_recent_sessions(10).unwrap();
        assert!(sessions.iter().all(|s| !s.is_import));
    }

    #[test]
    fn list_recent_sessions_orders_by_latest_activity() {
        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let mut older_started_recently_active = sess("a", None);
        older_started_recently_active.started_at = 100;
        older_started_recently_active.updated_at = Some(1000);
        let mut newer_started_stale = sess("b", None);
        newer_started_stale.started_at = 900;
        newer_started_stale.updated_at = Some(900);
        store.insert_session(&newer_started_stale).unwrap();
        store.insert_session(&older_started_recently_active).unwrap();

        let sessions = store.list_recent_sessions(10).unwrap();

        assert_eq!(sessions[0].source_id, "src-a");
        assert_eq!(sessions[1].source_id, "src-b");
    }

    #[test]
    fn session_paths_for_source_round_trips_then_delete_clears() {
        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        store.insert_session(&sess("a", Some("/home/u/proj"))).unwrap();
        store.insert_session(&sess("b", None)).unwrap();
        store
            .update_session_fields(
                "claude-code",
                "src-b",
                None,
                None,
                None,
                Some("/home/u/.claude-mem/observer-sessions/session.jsonl"),
            )
            .unwrap();

        let paths = store.session_paths_for_source("claude-code").unwrap();
        assert_eq!(paths.len(), 2);
        let observer = paths.iter().find(|path| path.source_id == "src-b").unwrap();
        assert_eq!(
            observer.source_file_path.as_deref(),
            Some("/home/u/.claude-mem/observer-sessions/session.jsonl")
        );

        store.delete_session_data("claude-code", "src-b").unwrap();
        let after = store.session_paths_for_source("claude-code").unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].source_id, "src-a");
    }

    #[test]
    fn update_session_fields_does_not_clear_metadata() {
        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let mut session = sess("a", None);
        session.custom_title = Some("Keep title".to_string());
        session.summary = Some("Keep summary".to_string());
        session.duration_minutes = Some(9);
        store.insert_session(&session).unwrap();

        store
            .update_session_fields(
                "claude-code",
                "src-a",
                None,
                None,
                None,
                Some("/home/u/observer-sessions/session.jsonl"),
            )
            .unwrap();

        let paths = store.session_paths_for_source("claude-code").unwrap();
        assert_eq!(
            paths[0].source_file_path.as_deref(),
            Some("/home/u/observer-sessions/session.jsonl")
        );
        let sessions = store.list_sessions_by_ids(&["a".to_string()]).unwrap();
        assert_eq!(sessions[0].custom_title.as_deref(), Some("Keep title"));
        assert_eq!(sessions[0].summary.as_deref(), Some("Keep summary"));
        assert_eq!(sessions[0].duration_minutes, Some(9));
    }

    #[test]
    fn update_session_fields_preserves_unset_fields() {
        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let mut session = sess("a", None);
        session.summary = Some("Keep summary".to_string());
        session.duration_minutes = Some(9);
        store.insert_session(&session).unwrap();

        store
            .update_session_fields("claude-code", "src-a", Some("New title"), None, None, None)
            .unwrap();

        let sessions = store.list_sessions_by_ids(&["a".to_string()]).unwrap();
        assert_eq!(sessions[0].title, "New title");
        assert_eq!(sessions[0].custom_title.as_deref(), Some("New title"));
        assert_eq!(sessions[0].summary.as_deref(), Some("Keep summary"));
        assert_eq!(sessions[0].duration_minutes, Some(9));
    }
}
