use std::path::PathBuf;

use tracing::debug;

use crate::adapters::opencode;
use crate::adapters::{RawSession, ResumeCommand, SourceAdapter, SyncScanResult, SyncScanStats};
use crate::db::store::Store;

pub(crate) struct ZcodeAdapter;

impl SourceAdapter for ZcodeAdapter {
    fn id(&self) -> &str {
        "zcode"
    }

    fn label(&self) -> &str {
        "ZC"
    }

    fn usage_parser_version(&self) -> Option<u32> {
        Some(opencode::USAGE_PARSER_VERSION)
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        Some(ResumeCommand {
            program: "zcode".to_string(),
            args: vec!["--resume".to_string(), source_id.to_string()],
        })
    }

    fn app_command(&self, _source_id: &str) -> Option<ResumeCommand> {
        open_zcode_app()
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let Some(conn) = open_zcode_db()? else {
            return Ok(vec![]);
        };
        opencode::scan_with_options(&conn, true, opencode::ScanOptions::ZCODE)
    }

    fn scan_for_sync(
        &self,
        store: &Store,
        since_ts: Option<i64>,
        include_events: bool,
    ) -> anyhow::Result<Option<SyncScanResult>> {
        let Some(conn) = open_zcode_db()? else {
            return Ok(Some(SyncScanResult { sessions: vec![], stats: SyncScanStats::default() }));
        };
        Ok(Some(opencode::scan_for_sync_conn_with_options(
            &conn,
            store,
            since_ts,
            "zcode",
            include_events,
            opencode::ScanOptions::ZCODE,
        )?))
    }
}

fn open_zcode_db() -> anyhow::Result<Option<rusqlite::Connection>> {
    let Some(db_path) = resolve_zcode_db_path() else {
        debug!("ZCode DB not found, skipping");
        return Ok(None);
    };
    opencode::open_readonly(&db_path)
}

fn resolve_zcode_db_path() -> Option<PathBuf> {
    resolve_zcode_db_path_from(std::env::var("ZCODE_STORAGE_DIR").ok(), dirs::home_dir()?)
}

fn resolve_zcode_db_path_from(storage_dir: Option<String>, home: PathBuf) -> Option<PathBuf> {
    let root = match storage_dir.as_deref().map(str::trim).filter(|value| !value.is_empty()) {
        Some(raw) => PathBuf::from(raw),
        None => home.join(".zcode"),
    };
    let path = root.join("cli/db/db.sqlite");
    path.exists().then_some(path)
}

#[cfg(target_os = "macos")]
fn open_zcode_app() -> Option<ResumeCommand> {
    Some(ResumeCommand {
        program: "open".to_string(),
        args: vec!["-a".to_string(), "ZCode".to_string()],
    })
}

#[cfg(not(target_os = "macos"))]
fn open_zcode_app() -> Option<ResumeCommand> {
    None
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn resume_uses_official_flag() {
        let command = ZcodeAdapter.resume_command("sess_123").unwrap();
        assert_eq!(command.program, "zcode");
        assert_eq!(command.args, vec!["--resume", "sess_123"]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn app_command_opens_desktop_app() {
        let command = ZcodeAdapter.app_command("sess_123").unwrap();
        assert_eq!(command.program, "open");
        assert_eq!(command.args, vec!["-a", "ZCode"]);
    }

    #[test]
    fn default_path_is_home_zcode_cli_db() {
        let empty_home = tempfile::tempdir().unwrap();
        assert!(resolve_zcode_db_path_from(None, empty_home.path().to_path_buf()).is_none());

        let root = tempfile::tempdir().unwrap();
        let db = root.path().join(".zcode/cli/db/db.sqlite");
        fs_write_empty(&db);
        let resolved = resolve_zcode_db_path_from(None, root.path().to_path_buf()).unwrap();
        assert_eq!(resolved, db);
    }

    #[test]
    fn storage_dir_overrides_home_root() {
        let root = tempfile::tempdir().unwrap();
        let storage = root.path().join("custom-zcode");
        let db = storage.join("cli/db/db.sqlite");
        fs_write_empty(&db);
        let resolved = resolve_zcode_db_path_from(
            Some(storage.to_string_lossy().into_owned()),
            PathBuf::from("/unused"),
        )
        .unwrap();
        assert_eq!(resolved, db);
    }

    #[test]
    fn blank_storage_dir_falls_back_to_home() {
        let root = tempfile::tempdir().unwrap();
        let db = root.path().join(".zcode/cli/db/db.sqlite");
        fs_write_empty(&db);
        let resolved =
            resolve_zcode_db_path_from(Some("   ".to_string()), root.path().to_path_buf()).unwrap();
        assert_eq!(resolved, db);
    }

    #[test]
    fn missing_storage_dir_is_skipped() {
        assert!(
            resolve_zcode_db_path_from(Some("/no/such/zcode".to_string()), PathBuf::from("/tmp"))
                .is_none()
        );
    }

    #[test]
    fn scan_reads_opencode_shaped_zcode_db() {
        let root = tempfile::tempdir().unwrap();
        let db_path = root.path().join("db.sqlite");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "
            CREATE TABLE session (
                id TEXT PRIMARY KEY,
                parent_id TEXT,
                directory TEXT,
                title TEXT,
                time_created INTEGER,
                time_updated INTEGER,
                task_type TEXT
            );
            CREATE TABLE message (
                id TEXT PRIMARY KEY,
                session_id TEXT,
                time_created INTEGER,
                data TEXT,
                sequence INTEGER
            );
            CREATE TABLE part (
                id TEXT PRIMARY KEY,
                message_id TEXT,
                session_id TEXT,
                time_created INTEGER,
                data TEXT,
                sequence INTEGER
            );
            INSERT INTO session (id, parent_id, directory, title, time_created, time_updated, task_type)
            VALUES ('sess_123', NULL, '/repo', 'seed', 100, 200, 'interactive');
            INSERT INTO message (id, session_id, time_created, data, sequence)
            VALUES ('msg_user', 'sess_123', 110, '{\"role\":\"user\"}', 1);
            INSERT INTO message (id, session_id, time_created, data, sequence)
            VALUES (
                'msg_hidden',
                'sess_123',
                115,
                '{\"role\":\"user\",\"semantics\":{\"origin\":\"system\",\"kind\":\"fork_notice\",\"transcriptVisibility\":\"hidden\"}}',
                2
            );
            INSERT INTO message (id, session_id, time_created, data, sequence)
            VALUES (
                'msg_asst',
                'sess_123',
                120,
                '{\"role\":\"assistant\",\"providerID\":\"builtin:zai\",\"modelID\":\"GLM-5.3\",\"tokens\":{\"input\":10,\"output\":4,\"reasoning\":2,\"cache\":{\"read\":1,\"write\":0}}}',
                3
            );
            INSERT INTO message (id, session_id, time_created, data, sequence)
            VALUES (
                'msg_timeline',
                'sess_123',
                125,
                '{\"role\":\"assistant\",\"semantics\":{\"kind\":\"timeline_event\"},\"providerID\":\"builtin:zai\",\"modelID\":\"GLM-5.3\",\"tokens\":{\"input\":0,\"output\":0,\"reasoning\":0,\"cache\":{\"read\":0,\"write\":0}}}',
                4
            );
            INSERT INTO part (id, message_id, session_id, time_created, data, sequence)
            VALUES ('part_user', 'msg_user', 'sess_123', 110, '{\"type\":\"text\",\"text\":\"hello zcode\"}', 0);
            INSERT INTO part (id, message_id, session_id, time_created, data, sequence)
            VALUES ('part_hidden', 'msg_hidden', 'sess_123', 115, '{\"type\":\"text\",\"text\":\"internal fork notice\"}', 0);
            INSERT INTO part (id, message_id, session_id, time_created, data, sequence)
            VALUES ('part_think', 'msg_asst', 'sess_123', 120, '{\"type\":\"reasoning\",\"text\":\"hidden\"}', 1);
            INSERT INTO part (id, message_id, session_id, time_created, data, sequence)
            VALUES ('part_text', 'msg_asst', 'sess_123', 121, '{\"type\":\"text\",\"text\":\"ready\"}', 2);
            INSERT INTO part (id, message_id, session_id, time_created, data, sequence)
            VALUES (
                'part_tool',
                'msg_asst',
                'sess_123',
                122,
                '{\"type\":\"tool\",\"tool\":\"Bash\",\"state\":{\"status\":\"completed\",\"input\":{\"command\":\"ls\"},\"output\":\"file body\"}}',
                3
            );
            ",
        )
        .unwrap();
        drop(conn);

        let conn = opencode::open_readonly(&db_path).unwrap().unwrap();
        let sessions =
            opencode::scan_with_options(&conn, true, opencode::ScanOptions::ZCODE).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].source_id, "sess_123");
        assert_eq!(sessions[0].directory.as_deref(), Some("/repo"));
        assert_eq!(sessions[0].custom_title.as_deref(), Some("seed"));
        assert_eq!(sessions[0].metadata_parser_version, Some(opencode::METADATA_PARSER_VERSION));
        assert_eq!(sessions[0].messages.len(), 2);
        assert_eq!(sessions[0].messages[0].content, "hello zcode");
        assert_eq!(sessions[0].messages[1].content, "ready");
        assert!(
            sessions[0].messages.iter().all(|message| message.content != "internal fork notice")
        );
        assert_eq!(sessions[0].usage_events.len(), 1);
        assert_eq!(sessions[0].usage_events[0].model, "GLM-5.3");
        assert_eq!(sessions[0].usage_events[0].provider, "builtin:zai");
        assert_eq!(sessions[0].usage_events[0].input_tokens, 10);
        assert_eq!(sessions[0].usage_events[0].output_tokens, 4);
        assert_eq!(sessions[0].events.len(), 2);
        assert_eq!(sessions[0].events[0].name.as_deref(), Some("Bash"));
    }

    fn fs_write_empty(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, []).unwrap();
    }
}
