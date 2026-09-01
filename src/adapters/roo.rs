use std::path::PathBuf;

use tracing::debug;

use crate::adapters::cline;
use crate::adapters::paths;
use crate::adapters::{RawSession, ResumeCommand, SourceAdapter, SyncScanResult};
use crate::db::store::Store;

const EXTENSION_ID: &str = "rooveterinaryinc.roo-cline";

pub(crate) struct RooAdapter;

impl SourceAdapter for RooAdapter {
    fn id(&self) -> &str {
        "roo"
    }

    fn label(&self) -> &str {
        "ROO"
    }

    fn resume_command(&self, _source_id: &str) -> Option<ResumeCommand> {
        None
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        cline::scan_task_dirs(&resolve_tasks_dirs())
    }

    fn scan_for_sync(
        &self,
        store: &Store,
        since_ts: Option<i64>,
        _include_events: bool,
    ) -> anyhow::Result<Option<SyncScanResult>> {
        Ok(Some(cline::scan_task_dirs_for_sync(&resolve_tasks_dirs(), store, since_ts, "roo")?))
    }
}

fn resolve_tasks_dirs() -> Vec<PathBuf> {
    let dirs = paths::vscode_extension_task_dirs(EXTENSION_ID);
    if dirs.is_empty() {
        debug!("Roo tasks directory not found, skipping Roo");
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{schema, store::Store};

    #[test]
    fn adapter_identity() {
        assert_eq!(RooAdapter.id(), "roo");
        assert_eq!(RooAdapter.label(), "ROO");
        assert!(RooAdapter.resume_command("1").is_none());
    }

    #[test]
    fn missing_tasks_dirs_scan_empty() {
        let sessions = cline::scan_task_dirs(&[]).unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn missing_tasks_dirs_sync_empty() {
        schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let result = cline::scan_task_dirs_for_sync(&[], &store, None, "roo").unwrap();
        assert!(result.sessions.is_empty());
        assert_eq!(result.stats.candidates, 0);
    }
}
