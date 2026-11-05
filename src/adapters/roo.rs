use std::path::PathBuf;

use tracing::debug;

use crate::adapters::AdapterSyncContext;
use crate::adapters::cline;
use crate::adapters::paths;
use crate::adapters::{RawSession, ResumeCommand, SourceAdapter, SyncScanResult};

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
        context: &AdapterSyncContext,
        since_ts: Option<i64>,
        include_events: bool,
    ) -> anyhow::Result<Option<SyncScanResult>> {
        Ok(Some(cline::scan_task_dirs_for_sync(
            &resolve_tasks_dirs(),
            context,
            since_ts,
            include_events,
        )?))
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

    #[test]
    fn adapter_identity() {
        assert_eq!(RooAdapter.id(), "roo");
        assert_eq!(RooAdapter.label(), "ROO");
        assert!(RooAdapter.resume_command("1").is_none());
    }
}
