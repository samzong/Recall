use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::Result;

use crate::adapters::{RawSession, SyncScanResult, SyncScanStats};
use crate::db::store::Store;

pub struct FileScanEntry {
    pub session_id: String,
    pub stat_target: PathBuf,
    pub directory: Option<String>,
}

pub fn run_file_scan<I, F>(
    store: &Store,
    source_id: &str,
    since_ts: Option<i64>,
    entries: I,
    parse_fn: F,
) -> Result<SyncScanResult>
where
    I: IntoIterator<Item = FileScanEntry>,
    F: Fn(FileScanEntry, i64) -> Result<Option<RawSession>>,
{
    let existing = store.session_meta_map(source_id)?;
    let mut sessions = Vec::new();
    let mut stats = SyncScanStats::default();

    for entry in entries {
        let Some(mtime_ms) = stat_mtime_ms(&entry.stat_target) else {
            continue;
        };

        if let Some(cutoff) = since_ts
            && mtime_ms < cutoff
        {
            stats.filtered_sessions += 1;
            continue;
        }

        if let Some((old_updated_at, _)) = existing.get(&entry.session_id)
            && *old_updated_at == Some(mtime_ms)
        {
            stats.skipped_sessions += 1;
            continue;
        }

        if let Some(raw) = parse_fn(entry, mtime_ms)? {
            sessions.push(raw);
        }
    }

    Ok(SyncScanResult { sessions, stats })
}

pub fn stat_mtime_ms(path: &Path) -> Option<i64> {
    let meta = fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    let duration = mtime.duration_since(UNIX_EPOCH).ok()?;
    Some(duration.as_millis() as i64)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    use crate::adapters::{RawMessage, RawSession};
    use crate::db::{schema, store::Store};
    use crate::types::{Role, Session};

    fn setup_store() -> Store {
        schema::register_sqlite_vec();
        Store::open_in_memory().unwrap()
    }

    fn make_session(
        id: &str,
        source_id: &str,
        updated_at: Option<i64>,
        message_count: u32,
    ) -> Session {
        Session {
            id: id.to_string(),
            source: "test-source".to_string(),
            source_id: source_id.to_string(),
            title: "existing".to_string(),
            directory: None,
            started_at: 0,
            updated_at,
            message_count,
            entrypoint: None,
        }
    }

    fn temp_file_with_mtime(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("recall-filescan-{}-{}", name, uuid::Uuid::new_v4()));
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "dummy").unwrap();
        path
    }

    fn stub_raw_session(source_id: &str, mtime_ms: i64) -> RawSession {
        RawSession {
            source_id: source_id.to_string(),
            directory: None,
            started_at: mtime_ms,
            updated_at: Some(mtime_ms),
            entrypoint: None,
            messages: vec![RawMessage {
                role: Role::User,
                content: "hi".to_string(),
                timestamp: Some(mtime_ms),
            }],
        }
    }

    #[test]
    fn empty_input_returns_empty_result() {
        let store = setup_store();
        let result =
            run_file_scan(&store, "test-source", None, Vec::<FileScanEntry>::new(), |_, _| {
                panic!("parse should not be called")
            })
            .unwrap();
        assert_eq!(result.sessions.len(), 0);
        assert_eq!(result.stats.skipped_sessions, 0);
        assert_eq!(result.stats.filtered_sessions, 0);
    }

    #[test]
    fn new_entry_triggers_parse_fn() {
        let store = setup_store();
        let path = temp_file_with_mtime("new");
        let entry = FileScanEntry {
            session_id: "sess-new".to_string(),
            stat_target: path.clone(),
            directory: None,
        };

        let result = run_file_scan(&store, "test-source", None, vec![entry], |entry, mtime_ms| {
            Ok(Some(stub_raw_session(&entry.session_id, mtime_ms)))
        })
        .unwrap();

        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].source_id, "sess-new");
        assert_eq!(result.stats.skipped_sessions, 0);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn matching_mtime_skips_without_parsing() {
        let store = setup_store();
        let path = temp_file_with_mtime("skip");
        let mtime_ms = stat_mtime_ms(&path).unwrap();
        store.insert_session(&make_session("s1", "sess-skip", Some(mtime_ms), 1)).unwrap();

        let entry = FileScanEntry {
            session_id: "sess-skip".to_string(),
            stat_target: path.clone(),
            directory: None,
        };

        let result = run_file_scan(&store, "test-source", None, vec![entry], |_, _| {
            panic!("parse should not be called for skipped entry")
        })
        .unwrap();

        assert_eq!(result.sessions.len(), 0);
        assert_eq!(result.stats.skipped_sessions, 1);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn mtime_mismatch_triggers_reparse() {
        let store = setup_store();
        let path = temp_file_with_mtime("mismatch");
        let actual_mtime = stat_mtime_ms(&path).unwrap();
        let stale_mtime = actual_mtime - 1_000;
        store.insert_session(&make_session("s2", "sess-stale", Some(stale_mtime), 1)).unwrap();

        let entry = FileScanEntry {
            session_id: "sess-stale".to_string(),
            stat_target: path.clone(),
            directory: None,
        };

        let result = run_file_scan(&store, "test-source", None, vec![entry], |entry, mtime_ms| {
            assert_eq!(mtime_ms, actual_mtime);
            Ok(Some(stub_raw_session(&entry.session_id, mtime_ms)))
        })
        .unwrap();

        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.stats.skipped_sessions, 0);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn since_ts_filters_old_entries() {
        let store = setup_store();
        let path = temp_file_with_mtime("old");
        let mtime_ms = stat_mtime_ms(&path).unwrap();
        let future_cutoff = mtime_ms + 10_000_000;

        let entry = FileScanEntry {
            session_id: "sess-old".to_string(),
            stat_target: path.clone(),
            directory: None,
        };

        let result =
            run_file_scan(&store, "test-source", Some(future_cutoff), vec![entry], |_, _| {
                panic!("parse should not be called for filtered entry")
            })
            .unwrap();

        assert_eq!(result.sessions.len(), 0);
        assert_eq!(result.stats.filtered_sessions, 1);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn missing_stat_target_is_skipped_silently() {
        let store = setup_store();
        let bogus =
            std::env::temp_dir().join(format!("recall-filescan-bogus-{}", uuid::Uuid::new_v4()));
        let entry = FileScanEntry {
            session_id: "sess-missing".to_string(),
            stat_target: bogus,
            directory: None,
        };

        let result = run_file_scan(&store, "test-source", None, vec![entry], |_, _| {
            panic!("parse should not be called for missing stat target")
        })
        .unwrap();

        assert_eq!(result.sessions.len(), 0);
        assert_eq!(result.stats.skipped_sessions, 0);
        assert_eq!(result.stats.filtered_sessions, 0);
    }
}
