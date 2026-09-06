use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use tracing::warn;

use crate::adapters::sync_state::{
    event_state_is_current_for_mtime, metadata_state_is_current_for_mtime,
    usage_state_is_current_for_mtime,
};
use crate::adapters::{
    AdapterSyncContext, RawSession, SourceObservation, SyncScanResult, SyncScanStats,
};

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct FileScanOptions {
    pub(crate) usage_parser_version: Option<u32>,
    pub(crate) event_parser_version: Option<u32>,
    pub(crate) metadata_parser_version: Option<u32>,
}

#[derive(Clone)]
pub(crate) struct FileScanEntry {
    pub(crate) session_id: String,
    pub(crate) stat_target: PathBuf,
    pub(crate) directory: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct FileScanSnapshot<T> {
    effective_mtime_ms: i64,
    fingerprint: T,
}

impl<T> FileScanSnapshot<T> {
    pub(crate) fn new(effective_mtime_ms: i64, fingerprint: T) -> Self {
        Self { effective_mtime_ms, fingerprint }
    }

    pub(crate) fn effective_mtime_ms(&self) -> i64 {
        self.effective_mtime_ms
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct FileMetadataSnapshot {
    modified: SystemTime,
    len: u64,
}

impl FileMetadataSnapshot {
    pub(crate) fn mtime_ms(&self) -> Option<i64> {
        let duration = self.modified.duration_since(UNIX_EPOCH).ok()?;
        Some(duration.as_millis() as i64)
    }
}

pub(crate) fn run_file_scan_with_options<I, F>(
    context: &AdapterSyncContext,
    since_ts: Option<i64>,
    options: FileScanOptions,
    entries: I,
    parse_fn: F,
) -> Result<SyncScanResult>
where
    I: IntoIterator<Item = FileScanEntry>,
    F: Fn(FileScanEntry, i64) -> Result<Option<RawSession>>,
{
    run_file_scan_with_options_and_mtime(
        context,
        since_ts,
        options,
        entries,
        |entry| stat_mtime_ms(&entry.stat_target),
        parse_fn,
    )
}

pub(crate) fn run_file_scan_with_options_and_mtime<I, F, M>(
    context: &AdapterSyncContext,
    since_ts: Option<i64>,
    options: FileScanOptions,
    entries: I,
    mtime_fn: M,
    parse_fn: F,
) -> Result<SyncScanResult>
where
    I: IntoIterator<Item = FileScanEntry>,
    F: Fn(FileScanEntry, i64) -> Result<Option<RawSession>>,
    M: Fn(&FileScanEntry) -> Option<i64>,
{
    run_file_scan_with_observations(
        context,
        since_ts,
        options,
        entries,
        |entry| mtime_fn(entry).map(|mtime_ms| FileScanSnapshot::new(mtime_ms, ())),
        |entry, mtime_ms, _| Ok((parse_fn(entry, mtime_ms)?, true)),
    )
}

pub(crate) fn run_file_scan_with_options_and_snapshot<I, F, S, T>(
    context: &AdapterSyncContext,
    since_ts: Option<i64>,
    options: FileScanOptions,
    entries: I,
    snapshot_fn: S,
    parse_fn: F,
) -> Result<SyncScanResult>
where
    I: IntoIterator<Item = FileScanEntry>,
    F: Fn(FileScanEntry, i64) -> Result<Option<RawSession>>,
    S: Fn(&FileScanEntry) -> Option<FileScanSnapshot<T>>,
    T: PartialEq,
{
    let snapshot_fn = &snapshot_fn;
    run_file_scan_with_observations(
        context,
        since_ts,
        options,
        entries,
        |entry| snapshot_fn(entry),
        |entry, mtime_ms, before| {
            let revalidate_entry = entry.clone();
            let raw = parse_fn(entry, mtime_ms)?;
            let stable = snapshot_fn(&revalidate_entry).as_ref() == Some(&before);
            if !stable {
                warn!(
                    "skipping unstable {} session {}: source files changed while parsing ({})",
                    context.source(),
                    revalidate_entry.session_id,
                    revalidate_entry.stat_target.display()
                );
            }
            Ok((raw, stable))
        },
    )
}

fn run_file_scan_with_observations<I, F, S, T>(
    context: &AdapterSyncContext,
    since_ts: Option<i64>,
    options: FileScanOptions,
    entries: I,
    snapshot_fn: S,
    parse_fn: F,
) -> Result<SyncScanResult>
where
    I: IntoIterator<Item = FileScanEntry>,
    F: Fn(FileScanEntry, i64, FileScanSnapshot<T>) -> Result<(Option<RawSession>, bool)>,
    S: Fn(&FileScanEntry) -> Option<FileScanSnapshot<T>>,
{
    let existing = context.session_meta();
    let usage_state = context.usage_state();
    let event_state = context.event_state();
    let metadata_state = context.metadata_state();
    let mut sessions = Vec::new();
    let mut observations = Vec::new();
    let mut stats = SyncScanStats::default();

    for entry in entries {
        stats.candidates += 1;
        let Some(snapshot) = snapshot_fn(&entry) else {
            stats.rejected_before_parse += 1;
            continue;
        };
        let mtime_ms = snapshot.effective_mtime_ms();
        let observation = existing.contains_key(&entry.session_id).then(|| SourceObservation {
            source_id: entry.session_id.clone(),
            source_file_path: entry.stat_target.to_str().map(str::to_string),
        });

        if let Some(cutoff) = since_ts
            && mtime_ms < cutoff
        {
            observations.extend(observation);
            stats.filtered_sessions += 1;
            stats.rejected_before_parse += 1;
            continue;
        }

        if let Some(old) = existing.get(&entry.session_id)
            && old.updated_at == Some(mtime_ms)
            && usage_state_is_current_for_mtime(
                options.usage_parser_version,
                usage_state.get(&entry.session_id).copied(),
                mtime_ms,
            )
            && event_state_is_current_for_mtime(
                options.event_parser_version,
                event_state.get(&entry.session_id).copied(),
                mtime_ms,
            )
            && metadata_state_is_current_for_mtime(
                options.metadata_parser_version,
                metadata_state.get(&entry.session_id).copied(),
                mtime_ms,
            )
        {
            observations.extend(observation);
            stats.skipped_sessions += 1;
            stats.rejected_before_parse += 1;
            continue;
        }

        stats.parsed += 1;
        let (raw, stable) = parse_fn(entry, mtime_ms, snapshot)?;
        if !stable {
            stats.unstable_sessions += 1;
            continue;
        }
        if let Some(raw) = raw {
            observations.extend(observation);
            sessions.push(raw);
        }
    }

    Ok(SyncScanResult { sessions, stats, observations })
}

pub(crate) fn stat_mtime_ms(path: &Path) -> Option<i64> {
    file_metadata_snapshot(path)?.mtime_ms()
}

pub(crate) fn file_metadata_snapshot(path: &Path) -> Option<FileMetadataSnapshot> {
    let meta = fs::metadata(path).ok()?;
    Some(FileMetadataSnapshot { modified: meta.modified().ok()?, len: meta.len() })
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

    fn sync_context(store: &Store) -> AdapterSyncContext {
        AdapterSyncContext::from_store_for_test(store, "test-source").unwrap()
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
            repo_remote: None,
            repo_slug: None,
            repo_name: None,
            started_at: 0,
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

    fn temp_file_with_mtime(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("recall-filescan-{}-{}", name, uuid::Uuid::new_v4()));
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "dummy").unwrap();
        path
    }

    fn stub_raw_session(source_id: &str, mtime_ms: i64) -> RawSession {
        RawSession::search_only(
            source_id,
            None,
            mtime_ms,
            Some(mtime_ms),
            None,
            vec![RawMessage {
                role: Role::User,
                content: "hi".to_string(),
                timestamp: Some(mtime_ms),
            }],
        )
    }

    #[test]
    fn empty_input_returns_empty_result() {
        let context = AdapterSyncContext::empty_for_test("test-source");
        let result = run_file_scan_with_options(
            &context,
            None,
            FileScanOptions::default(),
            Vec::<FileScanEntry>::new(),
            |_, _| panic!("parse should not be called"),
        )
        .unwrap();
        assert_eq!(result.sessions.len(), 0);
        assert!(result.observations.is_empty());
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

        let result = run_file_scan_with_options(
            &sync_context(&store),
            None,
            FileScanOptions::default(),
            vec![entry],
            |entry, mtime_ms| Ok(Some(stub_raw_session(&entry.session_id, mtime_ms))),
        )
        .unwrap();

        assert_eq!(result.sessions.len(), 1);
        assert!(result.observations.is_empty());
        assert_eq!(result.sessions[0].source_id, "sess-new");
        assert_eq!(result.stats.skipped_sessions, 0);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn changed_during_parse_is_rejected() {
        let store = setup_store();
        let path = temp_file_with_mtime("changed-during-parse");
        let entry = FileScanEntry {
            session_id: "sess-changing".to_string(),
            stat_target: path.clone(),
            directory: None,
        };
        store.insert_session(&make_session("s1", "sess-changing", Some(0), 1)).unwrap();

        let result = run_file_scan_with_options_and_snapshot(
            &sync_context(&store),
            None,
            FileScanOptions::default(),
            vec![entry],
            |entry| {
                let fingerprint = file_metadata_snapshot(&entry.stat_target)?;
                Some(FileScanSnapshot::new(fingerprint.mtime_ms()?, fingerprint))
            },
            |entry, mtime_ms| {
                fs::write(&entry.stat_target, "changed while parsing with a different length")?;
                Ok(Some(stub_raw_session(&entry.session_id, mtime_ms)))
            },
        )
        .unwrap();

        assert!(result.sessions.is_empty());
        assert!(result.observations.is_empty());
        assert_eq!(result.stats.unstable_sessions, 1);

        let entry = FileScanEntry {
            session_id: "sess-changing".to_string(),
            stat_target: path.clone(),
            directory: None,
        };
        let retry = run_file_scan_with_options_and_snapshot(
            &sync_context(&store),
            None,
            FileScanOptions::default(),
            vec![entry],
            |entry| {
                let fingerprint = file_metadata_snapshot(&entry.stat_target)?;
                Some(FileScanSnapshot::new(fingerprint.mtime_ms()?, fingerprint))
            },
            |entry, mtime_ms| Ok(Some(stub_raw_session(&entry.session_id, mtime_ms))),
        )
        .unwrap();

        assert_eq!(retry.sessions.len(), 1);
        assert_eq!(retry.observations.len(), 1);
        assert_eq!(retry.stats.unstable_sessions, 0);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn matching_mtime_skip_returns_observation_without_parsing() {
        let store = setup_store();
        let path = temp_file_with_mtime("skip");
        let mtime_ms = stat_mtime_ms(&path).unwrap();
        store.insert_session(&make_session("s1", "sess-skip", Some(mtime_ms), 1)).unwrap();

        let entry = FileScanEntry {
            session_id: "sess-skip".to_string(),
            stat_target: path.clone(),
            directory: None,
        };

        let result = run_file_scan_with_options(
            &sync_context(&store),
            None,
            FileScanOptions::default(),
            vec![entry],
            |_, _| panic!("parse should not be called for skipped entry"),
        )
        .unwrap();

        assert_eq!(result.sessions.len(), 0);
        assert_eq!(result.stats.skipped_sessions, 1);
        assert_eq!(
            result.observations,
            vec![SourceObservation {
                source_id: "sess-skip".to_string(),
                source_file_path: path.to_str().map(str::to_string),
            }]
        );
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn parse_rejection_does_not_return_observation() {
        let store = setup_store();
        let path = temp_file_with_mtime("parse-rejection");
        store.insert_session(&make_session("s1", "sess-rejected", Some(0), 1)).unwrap();
        let entry = FileScanEntry {
            session_id: "sess-rejected".to_string(),
            stat_target: path.clone(),
            directory: None,
        };

        let result = run_file_scan_with_options(
            &sync_context(&store),
            None,
            FileScanOptions::default(),
            vec![entry],
            |_, _| Ok(None),
        )
        .unwrap();

        assert!(result.sessions.is_empty());
        assert!(result.observations.is_empty());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn matching_mtime_reparses_until_usage_state_is_current() {
        let store = setup_store();
        let path = temp_file_with_mtime("usage-backfill");
        let mtime_ms = stat_mtime_ms(&path).unwrap();
        store.insert_session(&make_session("s1", "sess-usage", Some(mtime_ms), 1)).unwrap();

        let entry = FileScanEntry {
            session_id: "sess-usage".to_string(),
            stat_target: path.clone(),
            directory: None,
        };
        let result = run_file_scan_with_options(
            &sync_context(&store),
            None,
            FileScanOptions {
                usage_parser_version: Some(1),
                event_parser_version: None,
                metadata_parser_version: None,
            },
            vec![entry],
            |entry, mtime_ms| Ok(Some(stub_raw_session(&entry.session_id, mtime_ms))),
        )
        .unwrap();
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.stats.skipped_sessions, 0);

        store
            .persist_usage_events_for_existing_session(
                "test-source",
                "sess-usage",
                &[],
                1,
                Some(mtime_ms),
            )
            .unwrap();
        let entry = FileScanEntry {
            session_id: "sess-usage".to_string(),
            stat_target: path.clone(),
            directory: None,
        };
        let result = run_file_scan_with_options(
            &sync_context(&store),
            None,
            FileScanOptions {
                usage_parser_version: Some(1),
                event_parser_version: None,
                metadata_parser_version: None,
            },
            vec![entry],
            |_, _| panic!("current usage state should skip parsing"),
        )
        .unwrap();
        assert_eq!(result.sessions.len(), 0);
        assert_eq!(result.stats.skipped_sessions, 1);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn matching_mtime_reparses_until_event_state_is_current() {
        let store = setup_store();
        let path = temp_file_with_mtime("event-backfill");
        let mtime_ms = stat_mtime_ms(&path).unwrap();
        store.insert_session(&make_session("s1", "sess-event", Some(mtime_ms), 1)).unwrap();

        let entry = FileScanEntry {
            session_id: "sess-event".to_string(),
            stat_target: path.clone(),
            directory: None,
        };
        let result = run_file_scan_with_options(
            &sync_context(&store),
            None,
            FileScanOptions {
                usage_parser_version: None,
                event_parser_version: Some(1),
                metadata_parser_version: None,
            },
            vec![entry],
            |entry, mtime_ms| Ok(Some(stub_raw_session(&entry.session_id, mtime_ms))),
        )
        .unwrap();
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.stats.skipped_sessions, 0);

        store
            .persist_session_events_for_existing_session(
                "test-source",
                "sess-event",
                &[],
                1,
                Some(mtime_ms),
            )
            .unwrap();
        let entry = FileScanEntry {
            session_id: "sess-event".to_string(),
            stat_target: path.clone(),
            directory: None,
        };
        let result = run_file_scan_with_options(
            &sync_context(&store),
            None,
            FileScanOptions {
                usage_parser_version: None,
                event_parser_version: Some(1),
                metadata_parser_version: None,
            },
            vec![entry],
            |_, _| panic!("current event state should skip parsing"),
        )
        .unwrap();
        assert_eq!(result.sessions.len(), 0);
        assert_eq!(result.stats.skipped_sessions, 1);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn matching_mtime_reparses_until_metadata_state_is_current() {
        use crate::db::store::SessionTopologyWrite;
        let store = setup_store();
        let path = temp_file_with_mtime("metadata-backfill");
        let mtime_ms = stat_mtime_ms(&path).unwrap();
        store.insert_session(&make_session("s1", "sess-meta", Some(mtime_ms), 1)).unwrap();

        // An unchanged file whose topology parser version is stale must reparse,
        // even though usage and events are not tracked here.
        let entry = FileScanEntry {
            session_id: "sess-meta".to_string(),
            stat_target: path.clone(),
            directory: None,
        };
        let result = run_file_scan_with_options(
            &sync_context(&store),
            None,
            FileScanOptions {
                usage_parser_version: None,
                event_parser_version: None,
                metadata_parser_version: Some(1),
            },
            vec![entry],
            |entry, mtime_ms| Ok(Some(stub_raw_session(&entry.session_id, mtime_ms))),
        )
        .unwrap();
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.stats.skipped_sessions, 0);

        store
            .persist_topology_for_existing_session(
                "test-source",
                "sess-meta",
                &SessionTopologyWrite { thread_role: None, parents: &[], parser_version: Some(1) },
            )
            .unwrap();
        let entry = FileScanEntry {
            session_id: "sess-meta".to_string(),
            stat_target: path.clone(),
            directory: None,
        };
        let result = run_file_scan_with_options(
            &sync_context(&store),
            None,
            FileScanOptions {
                usage_parser_version: None,
                event_parser_version: None,
                metadata_parser_version: Some(1),
            },
            vec![entry],
            |_, _| panic!("current metadata state should skip parsing"),
        )
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

        let result = run_file_scan_with_options(
            &sync_context(&store),
            None,
            FileScanOptions::default(),
            vec![entry],
            |entry, mtime_ms| {
                assert_eq!(mtime_ms, actual_mtime);
                Ok(Some(stub_raw_session(&entry.session_id, mtime_ms)))
            },
        )
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
        store.insert_session(&make_session("s1", "sess-old", Some(mtime_ms), 1)).unwrap();

        let entry = FileScanEntry {
            session_id: "sess-old".to_string(),
            stat_target: path.clone(),
            directory: None,
        };

        let result = run_file_scan_with_options(
            &sync_context(&store),
            Some(future_cutoff),
            FileScanOptions::default(),
            vec![entry],
            |_, _| panic!("parse should not be called for filtered entry"),
        )
        .unwrap();

        assert_eq!(result.sessions.len(), 0);
        assert_eq!(result.stats.filtered_sessions, 1);
        assert_eq!(result.observations.len(), 1);
        assert_eq!(result.observations[0].source_id, "sess-old");
        assert_eq!(result.observations[0].source_file_path.as_deref(), path.to_str());
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

        let result = run_file_scan_with_options(
            &sync_context(&store),
            None,
            FileScanOptions::default(),
            vec![entry],
            |_, _| panic!("parse should not be called for missing stat target"),
        )
        .unwrap();

        assert_eq!(result.sessions.len(), 0);
        assert_eq!(result.stats.skipped_sessions, 0);
        assert_eq!(result.stats.filtered_sessions, 0);
    }
}
