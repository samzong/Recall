use std::path::{Path, PathBuf};

use tracing::debug;

use crate::adapters::opencode;
use crate::adapters::{RawSession, ResumeCommand, SourceAdapter, SyncScanResult, SyncScanStats};
use crate::db::store::Store;

pub(crate) struct KiloCodeAdapter;

impl SourceAdapter for KiloCodeAdapter {
    fn id(&self) -> &str {
        "kilo-code"
    }

    fn label(&self) -> &str {
        "KL"
    }

    fn usage_parser_version(&self) -> Option<u32> {
        Some(opencode::USAGE_PARSER_VERSION)
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        Some(ResumeCommand {
            program: "kilo".to_string(),
            args: vec!["--session".to_string(), source_id.to_string()],
        })
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let Some(conn) = open_kilo_db()? else {
            return Ok(vec![]);
        };
        opencode::scan(&conn, true)
    }

    fn scan_for_sync(
        &self,
        store: &Store,
        since_ts: Option<i64>,
        include_events: bool,
    ) -> anyhow::Result<Option<SyncScanResult>> {
        let Some(conn) = open_kilo_db()? else {
            return Ok(Some(SyncScanResult { sessions: vec![], stats: SyncScanStats::default() }));
        };
        Ok(Some(opencode::scan_for_sync_conn(&conn, store, since_ts, "kilo-code", include_events)?))
    }
}

fn open_kilo_db() -> anyhow::Result<Option<rusqlite::Connection>> {
    let Some(db_path) = resolve_kilo_db_path() else {
        debug!("Kilo Code DB not found, skipping");
        return Ok(None);
    };
    opencode::open_readonly(&db_path)
}

fn resolve_kilo_db_path() -> Option<PathBuf> {
    resolve_kilo_db_path_from(
        std::env::var("KILO_DB").ok(),
        std::env::var("XDG_DATA_HOME").ok(),
        dirs::home_dir()?,
    )
}

fn resolve_kilo_db_path_from(
    kilo_db: Option<String>,
    xdg_data_home: Option<String>,
    home: PathBuf,
) -> Option<PathBuf> {
    let data_dir = kilo_data_dir(xdg_data_home.as_deref(), &home);
    let path = match kilo_db.as_deref().map(str::trim).filter(|value| !value.is_empty()) {
        Some(":memory:") => return None,
        Some(raw) => {
            let candidate = Path::new(raw);
            if candidate.is_absolute() { candidate.to_path_buf() } else { data_dir.join(raw) }
        }
        None => data_dir.join("kilo.db"),
    };
    path.exists().then_some(path)
}

fn kilo_data_dir(xdg_data_home: Option<&str>, home: &Path) -> PathBuf {
    match xdg_data_home.map(str::trim).filter(|value| !value.is_empty()) {
        Some(xdg) => PathBuf::from(xdg).join("kilo"),
        None => home.join(".local/share/kilo"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_uses_official_flag() {
        let command = KiloCodeAdapter.resume_command("ses_123").unwrap();
        assert_eq!(command.program, "kilo");
        assert_eq!(command.args, vec!["--session", "ses_123"]);
    }

    #[test]
    fn default_path_is_xdg_share_kilo_db() {
        let empty_home = tempfile::tempdir().unwrap();
        assert!(resolve_kilo_db_path_from(None, None, empty_home.path().to_path_buf()).is_none());

        let root = tempfile::tempdir().unwrap();
        let db = root.path().join(".local/share/kilo/kilo.db");
        fs_write_empty(&db);
        let resolved = resolve_kilo_db_path_from(None, None, root.path().to_path_buf()).unwrap();
        assert_eq!(resolved, db);
    }

    #[test]
    fn xdg_data_home_overrides_default_root() {
        let root = tempfile::tempdir().unwrap();
        let xdg = root.path().join("xdg-data");
        let db = xdg.join("kilo/kilo.db");
        fs_write_empty(&db);
        let resolved = resolve_kilo_db_path_from(
            None,
            Some(xdg.to_string_lossy().into_owned()),
            PathBuf::from("/unused"),
        )
        .unwrap();
        assert_eq!(resolved, db);
    }

    #[test]
    fn kilo_db_absolute_path_wins() {
        let root = tempfile::tempdir().unwrap();
        let db = root.path().join("custom.db");
        fs_write_empty(&db);
        let resolved = resolve_kilo_db_path_from(
            Some(db.to_string_lossy().into_owned()),
            None,
            PathBuf::from("/unused"),
        )
        .unwrap();
        assert_eq!(resolved, db);
    }

    #[test]
    fn kilo_db_relative_path_is_under_data_dir() {
        let root = tempfile::tempdir().unwrap();
        let xdg = root.path().join("xdg-data");
        let db = xdg.join("kilo/alt.db");
        fs_write_empty(&db);
        let resolved = resolve_kilo_db_path_from(
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
            resolve_kilo_db_path_from(Some(":memory:".to_string()), None, PathBuf::from("/tmp"))
                .is_none()
        );
        assert!(
            resolve_kilo_db_path_from(
                Some("/no/such/kilo.db".to_string()),
                None,
                PathBuf::from("/tmp")
            )
            .is_none()
        );
    }

    #[test]
    fn scan_reads_opencode_shaped_kilo_db() {
        let root = tempfile::tempdir().unwrap();
        let db_path = root.path().join("kilo.db");
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
            VALUES ('ses_123', 'seed', '/repo', 100, 200);
            INSERT INTO message (session_id, data, time_created)
            VALUES ('ses_123', '{\"role\":\"user\"}', 110);
            INSERT INTO message (session_id, data, time_created)
            VALUES ('ses_123', '{\"role\":\"assistant\"}', 120);
            INSERT INTO part (message_id, data)
            VALUES (1, '{\"type\":\"text\",\"text\":\"hello kilo\"}');
            INSERT INTO part (message_id, data)
            VALUES (2, '{\"type\":\"tool\",\"tool\":\"bash\",\"state\":{\"status\":\"completed\",\"input\":{\"command\":\"ls\"},\"output\":\"file body\"}}');
            ",
        )
        .unwrap();
        drop(conn);

        let conn = opencode::open_readonly(&db_path).unwrap().unwrap();
        let sessions = opencode::scan(&conn, true).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].source_id, "ses_123");
        assert_eq!(sessions[0].directory.as_deref(), Some("/repo"));
        assert_eq!(sessions[0].custom_title.as_deref(), Some("seed"));
        assert_eq!(sessions[0].metadata_parser_version, Some(opencode::METADATA_PARSER_VERSION));
        assert_eq!(sessions[0].messages.len(), 1);
        assert_eq!(sessions[0].messages[0].content, "hello kilo");
        assert_eq!(sessions[0].events.len(), 2);
        assert_eq!(sessions[0].events[0].kind, "command");
        assert_eq!(sessions[0].events[0].name.as_deref(), Some("bash"));
    }

    fn fs_write_empty(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, []).unwrap();
    }
}
