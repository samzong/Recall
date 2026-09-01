use std::collections::HashSet;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use tracing::debug;

use crate::adapters::opencode;
use crate::adapters::{RawSession, ResumeCommand, SourceAdapter, SyncScanResult, SyncScanStats};
use crate::db::store::Store;

pub(crate) struct MimoCodeAdapter;

impl SourceAdapter for MimoCodeAdapter {
    fn id(&self) -> &str {
        "mimo-code"
    }

    fn label(&self) -> &str {
        "MM"
    }

    fn usage_parser_version(&self) -> Option<u32> {
        Some(opencode::USAGE_PARSER_VERSION)
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        Some(ResumeCommand {
            program: "mimo".to_string(),
            args: vec!["--session".to_string(), source_id.to_string()],
        })
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let Some(conn) = open_mimo_db()? else {
            return Ok(vec![]);
        };
        scan_conn(&conn, true)
    }

    fn scan_for_sync(
        &self,
        store: &Store,
        since_ts: Option<i64>,
        include_events: bool,
    ) -> anyhow::Result<Option<SyncScanResult>> {
        let Some(conn) = open_mimo_db()? else {
            return Ok(Some(SyncScanResult { sessions: vec![], stats: SyncScanStats::default() }));
        };
        let mut result =
            opencode::scan_for_sync_conn(&conn, store, since_ts, "mimo-code", include_events)?;
        result.sessions = drop_imported(&conn, result.sessions);
        Ok(Some(result))
    }
}

fn open_mimo_db() -> anyhow::Result<Option<Connection>> {
    let Some(db_path) = resolve_mimo_db_path() else {
        debug!("MiMo Code DB not found, skipping");
        return Ok(None);
    };
    opencode::open_readonly(&db_path)
}

fn resolve_mimo_db_path() -> Option<PathBuf> {
    resolve_mimo_db_path_from(
        std::env::var("MIMOCODE_DB").ok(),
        std::env::var("XDG_DATA_HOME").ok(),
        dirs::home_dir()?,
    )
}

fn resolve_mimo_db_path_from(
    mimo_db: Option<String>,
    xdg_data_home: Option<String>,
    home: PathBuf,
) -> Option<PathBuf> {
    let data_dir = mimo_data_dir(xdg_data_home.as_deref(), &home);
    let path = match mimo_db.as_deref().map(str::trim).filter(|value| !value.is_empty()) {
        Some(":memory:") => return None,
        Some(raw) => {
            let candidate = Path::new(raw);
            if candidate.is_absolute() { candidate.to_path_buf() } else { data_dir.join(raw) }
        }
        None => data_dir.join("mimocode.db"),
    };
    path.exists().then_some(path)
}

fn mimo_data_dir(xdg_data_home: Option<&str>, home: &Path) -> PathBuf {
    match xdg_data_home.map(str::trim).filter(|value| !value.is_empty()) {
        Some(xdg) => PathBuf::from(xdg).join("mimocode"),
        None => home.join(".local/share/mimocode"),
    }
}

fn scan_conn(conn: &Connection, include_events: bool) -> anyhow::Result<Vec<RawSession>> {
    Ok(drop_imported(conn, opencode::scan(conn, include_events)?))
}

fn drop_imported(conn: &Connection, sessions: Vec<RawSession>) -> Vec<RawSession> {
    let imported = imported_session_ids(conn);
    if imported.is_empty() {
        return sessions;
    }
    sessions.into_iter().filter(|session| !imported.contains(&session.source_id)).collect()
}

fn imported_session_ids(conn: &Connection) -> HashSet<String> {
    let mut ids = HashSet::new();
    for sql in ["SELECT session_id FROM external_import", "SELECT session_id FROM claude_import"] {
        let Ok(mut stmt) = conn.prepare(sql) else {
            continue;
        };
        let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(0)) else {
            continue;
        };
        ids.extend(rows.flatten());
    }
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_uses_official_flag() {
        let command = MimoCodeAdapter.resume_command("ses_123").unwrap();
        assert_eq!(command.program, "mimo");
        assert_eq!(command.args, vec!["--session", "ses_123"]);
    }

    #[test]
    fn default_path_is_xdg_share_mimocode_db() {
        let empty_home = tempfile::tempdir().unwrap();
        assert!(resolve_mimo_db_path_from(None, None, empty_home.path().to_path_buf()).is_none());

        let root = tempfile::tempdir().unwrap();
        let db = root.path().join(".local/share/mimocode/mimocode.db");
        fs_write_empty(&db);
        let resolved = resolve_mimo_db_path_from(None, None, root.path().to_path_buf()).unwrap();
        assert_eq!(resolved, db);
    }

    #[test]
    fn xdg_data_home_overrides_default_root() {
        let root = tempfile::tempdir().unwrap();
        let xdg = root.path().join("xdg-data");
        let db = xdg.join("mimocode/mimocode.db");
        fs_write_empty(&db);
        let resolved = resolve_mimo_db_path_from(
            None,
            Some(xdg.to_string_lossy().into_owned()),
            PathBuf::from("/unused"),
        )
        .unwrap();
        assert_eq!(resolved, db);
    }

    #[test]
    fn mimo_db_absolute_path_wins() {
        let root = tempfile::tempdir().unwrap();
        let db = root.path().join("custom.db");
        fs_write_empty(&db);
        let resolved = resolve_mimo_db_path_from(
            Some(db.to_string_lossy().into_owned()),
            None,
            PathBuf::from("/unused"),
        )
        .unwrap();
        assert_eq!(resolved, db);
    }

    #[test]
    fn mimo_db_relative_path_is_under_data_dir() {
        let root = tempfile::tempdir().unwrap();
        let xdg = root.path().join("xdg-data");
        let db = xdg.join("mimocode/alt.db");
        fs_write_empty(&db);
        let resolved = resolve_mimo_db_path_from(
            Some("alt.db".to_string()),
            Some(xdg.to_string_lossy().into_owned()),
            PathBuf::from("/unused"),
        )
        .unwrap();
        assert_eq!(resolved, db);
    }

    #[test]
    fn memory_and_missing_paths_are_skipped() {
        assert!(
            resolve_mimo_db_path_from(Some(":memory:".to_string()), None, PathBuf::from("/tmp"))
                .is_none()
        );
        assert!(
            resolve_mimo_db_path_from(
                Some("/no/such/mimocode.db".to_string()),
                None,
                PathBuf::from("/tmp")
            )
            .is_none()
        );
    }

    #[test]
    fn scan_reads_opencode_shaped_native_session() {
        let (_root, db_path) = seed_db(
            "
            INSERT INTO session (id, title, directory, time_created, time_updated)
            VALUES ('ses_native', 'seed', '/repo', 100, 200);
            INSERT INTO message (session_id, data, time_created)
            VALUES ('ses_native', '{\"role\":\"user\"}', 110);
            INSERT INTO message (session_id, data, time_created)
            VALUES ('ses_native', '{\"role\":\"assistant\"}', 120);
            INSERT INTO part (message_id, data)
            VALUES (1, '{\"type\":\"text\",\"text\":\"hello mimo\"}');
            INSERT INTO part (message_id, data)
            VALUES (2, '{\"type\":\"tool\",\"tool\":\"bash\",\"state\":{\"status\":\"completed\",\"input\":{\"command\":\"ls\"},\"output\":\"file body\"}}');
            ",
        );
        let conn = opencode::open_readonly(&db_path).unwrap().unwrap();
        let sessions = scan_conn(&conn, true).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].source_id, "ses_native");
        assert_eq!(sessions[0].directory.as_deref(), Some("/repo"));
        assert_eq!(sessions[0].custom_title.as_deref(), Some("seed"));
        assert_eq!(sessions[0].metadata_parser_version, Some(opencode::METADATA_PARSER_VERSION));
        assert_eq!(sessions[0].messages.len(), 1);
        assert_eq!(sessions[0].messages[0].content, "hello mimo");
        assert_eq!(sessions[0].events.len(), 2);
        assert_eq!(sessions[0].events[0].kind, "command");
        assert_eq!(sessions[0].events[0].name.as_deref(), Some("bash"));
    }

    #[test]
    fn scan_without_import_tables_keeps_sessions() {
        let root = tempfile::tempdir().unwrap();
        let db_path = root.path().join("mimocode.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
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
                session_id TEXT,
                data TEXT,
                time_created INTEGER
            );
            CREATE TABLE part (
                id INTEGER PRIMARY KEY,
                message_id INTEGER,
                data TEXT
            );
            INSERT INTO session (id, title, directory, time_created, time_updated)
            VALUES ('ses_native', 'seed', '/repo', 100, 200);
            INSERT INTO message (session_id, data, time_created)
            VALUES ('ses_native', '{\"role\":\"user\"}', 110);
            INSERT INTO part (message_id, data)
            VALUES (1, '{\"type\":\"text\",\"text\":\"no import tables\"}');
            ",
        )
        .unwrap();
        drop(conn);
        let conn = opencode::open_readonly(&db_path).unwrap().unwrap();
        let sessions = scan_conn(&conn, false).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].messages[0].content, "no import tables");
    }

    #[test]
    fn scan_skips_claude_and_external_imports() {
        let (_root, db_path) = seed_db(
            "
            INSERT INTO session (id, title, directory, time_created, time_updated)
            VALUES
                ('ses_native', 'native', '/repo', 100, 200),
                ('ses_cc', 'from claude', '/repo', 100, 200),
                ('ses_ext', 'from external', '/repo', 100, 200);
            INSERT INTO message (session_id, data, time_created)
            VALUES
                ('ses_native', '{\"role\":\"user\"}', 110),
                ('ses_cc', '{\"role\":\"user\"}', 110),
                ('ses_ext', '{\"role\":\"user\"}', 110);
            INSERT INTO part (message_id, data)
            VALUES
                (1, '{\"type\":\"text\",\"text\":\"native text\"}'),
                (2, '{\"type\":\"text\",\"text\":\"claude text\"}'),
                (3, '{\"type\":\"text\",\"text\":\"external text\"}');
            INSERT INTO claude_import (source_uuid, session_id, source_path, source_mtime, time_imported)
            VALUES ('uuid-cc', 'ses_cc', '/tmp/cc.jsonl', 1, 1);
            INSERT INTO external_import (source, source_key, session_id, source_path, source_mtime, time_imported)
            VALUES ('cc', 'key-ext', 'ses_ext', '/tmp/ext.jsonl', 1, 1);
            ",
        );
        let conn = opencode::open_readonly(&db_path).unwrap().unwrap();
        let sessions = scan_conn(&conn, true).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].source_id, "ses_native");
        assert_eq!(sessions[0].messages[0].content, "native text");
    }

    fn seed_db(extra_sql: &str) -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let db_path = root.path().join("mimocode.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(&format!(
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
                session_id TEXT,
                data TEXT,
                time_created INTEGER
            );
            CREATE TABLE part (
                id INTEGER PRIMARY KEY,
                message_id INTEGER,
                data TEXT
            );
            CREATE TABLE claude_import (
                source_uuid TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                source_path TEXT NOT NULL,
                source_mtime INTEGER NOT NULL,
                time_imported INTEGER NOT NULL
            );
            CREATE TABLE external_import (
                source TEXT NOT NULL,
                source_key TEXT NOT NULL,
                session_id TEXT NOT NULL,
                source_path TEXT NOT NULL,
                source_mtime INTEGER NOT NULL,
                time_imported INTEGER NOT NULL,
                PRIMARY KEY (source, source_key)
            );
            {extra_sql}
            "
        ))
        .unwrap();
        drop(conn);
        (root, db_path)
    }

    fn fs_write_empty(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, []).unwrap();
    }
}
