mod backfill;
mod entry;
mod persist;
mod reconcile;
mod scope;
mod single_session;
pub(crate) use entry::{
    run_background_worker, run_cli, run_dashboard_sync_job_quiet, run_sync_job_inner,
    run_usage_sync_job, run_usage_sync_job_with_progress, scan_remote_scope,
};
#[cfg(any(test, feature = "bench"))]
pub(crate) use persist::persist_raw_session_for_conformance;
#[cfg(test)]
use persist::{decide_existing_session_action, raw_session_metadata_changed};
use scope::{delete_excluded_sessions_for_source, path_matches_scope, paths_match_excluded};
pub(crate) use single_session::{SessionSyncFormat, run as run_single_session};

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use tracing::info;

use crate::adapters;
use crate::config::AppConfig;
use crate::db::store::{IndexedSessionMeta, ParserStateMeta, SessionPath, Store};
use crate::project_scope::ProjectScope;
use crate::repo_identity::RepoIdentityCache;
use crate::sync_progress::SyncProgress;
use crate::types::Session;

#[derive(Debug, Clone)]
pub(crate) struct SyncRunOptions {
    pub(crate) force: bool,
    pub(crate) verbose: bool,
    pub(crate) emit: bool,
    pub(crate) usage_only: bool,
    pub(crate) backfill_events: bool,
    pub(crate) sources: Option<Vec<String>>,
    /// System jobs must pass `Global` explicitly: the background worker is a
    /// child process that inherits the caller's directory, so an inferred
    /// scope would silently shrink global maintenance.
    pub(crate) scope: ProjectScope,
    pub(crate) target_session: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct BackfillPlan {
    pub(super) usage: bool,
    pub(super) events: bool,
    pub(super) metadata: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ExistingSessionAction {
    Skip,
    BackfillOnly(BackfillPlan),
    RefreshSession,
}

#[derive(Default)]
pub(super) struct SyncStats {
    pub(super) new_sessions: u32,
    pub(super) updated_sessions: u32,
    pub(super) reprocessed_sessions: u32,
    pub(super) total_messages: u32,
    pub(super) skipped: u32,
    pub(super) filtered_out: u32,
    pub(super) excluded_out: u32,
    pub(super) out_of_scope: u32,
}

/// Per-adapter accounting for `--verbose`. Without it a sync reports only
/// totals, which cannot show whether a candidate was rejected before its
/// transcript was read — the only rejection that saves work.
pub(super) struct AdapterRun {
    pub(super) label: String,
    pub(super) scan: adapters::SyncScanStats,
    pub(super) out_of_scope: u32,
    pub(super) touched: u32,
    pub(super) elapsed_ms: u128,
}

impl SyncStats {
    pub(super) fn touched(&self) -> u32 {
        self.new_sessions + self.updated_sessions + self.reprocessed_sessions
    }
}

#[derive(Default)]
pub(super) struct EventBackfillReport {
    pub(super) dry_run: bool,
    pub(super) scanned: u32,
    pub(super) missing_original: u32,
    pub(super) unknown_original: u32,
    pub(super) unstable: u32,
    pub(super) no_events: u32,
    pub(super) unsupported_sessions: u32,
    pub(super) failed_writes: u32,
    pub(super) disabled: Vec<String>,
    pub(super) unsupported_sources: Vec<String>,
    pub(super) unavailable: Vec<String>,
}

pub(super) struct ExistingState {
    pub(super) meta: HashMap<String, IndexedSessionMeta>,
    pub(super) paths: HashMap<String, SessionPath>,
    pub(super) imported_ids: HashSet<String>,
    pub(super) usage_meta: HashMap<String, ParserStateMeta>,
    pub(super) event_meta: HashMap<String, ParserStateMeta>,
    pub(super) metadata_meta: HashMap<String, ParserStateMeta>,
}

impl ExistingState {
    pub(super) fn remove(&mut self, source_id: &str) -> bool {
        if self.meta.remove(source_id).is_some() {
            self.paths.remove(source_id);
            self.usage_meta.remove(source_id);
            self.event_meta.remove(source_id);
            self.metadata_meta.remove(source_id);
            true
        } else {
            false
        }
    }

    pub(super) fn record_replaced(
        &mut self,
        session: &Session,
        usage_parser_version: Option<u32>,
        event_parser_version: Option<u32>,
        metadata_parser_version: Option<u32>,
    ) {
        self.meta.insert(
            session.source_id.clone(),
            IndexedSessionMeta {
                id: session.id.clone(),
                directory: session.directory.clone(),
                custom_title: session.custom_title.clone(),
                started_at: session.started_at,
                updated_at: session.updated_at,
                message_count: session.message_count,
            },
        );
        self.paths.insert(
            session.source_id.clone(),
            SessionPath {
                source_id: session.source_id.clone(),
                directory: session.directory.clone(),
                source_file_path: session.source_file_path.clone(),
                repo_remote: session.repo_remote.clone(),
                repo_slug: session.repo_slug.clone(),
                repo_name: session.repo_name.clone(),
            },
        );
        for (states, version) in [
            (&mut self.usage_meta, usage_parser_version),
            (&mut self.event_meta, event_parser_version),
            (&mut self.metadata_meta, metadata_parser_version),
        ] {
            if let Some(parser_version) = version {
                states.insert(
                    session.source_id.clone(),
                    ParserStateMeta { parser_version, source_updated_at: session.updated_at },
                );
            }
        }
    }
}

pub(super) struct SyncJob {
    pub(super) store: Store,
    pub(super) event_backfill: Option<EventBackfillReport>,
    pub(super) options: SyncRunOptions,
    pub(super) config: AppConfig,
    pub(super) host: Option<crate::host::Host>,
    pub(super) labels: Vec<(String, String)>,
    pub(super) since_ts: Option<i64>,
    pub(super) path_excluder: Option<globset::GlobSet>,
    pub(super) repo_cache: RepoIdentityCache,
    pub(super) stats: SyncStats,
    pub(super) adapter_runs: Vec<AdapterRun>,
    pub(super) progress: SyncProgress,
    pub(super) started: std::time::Instant,
}

impl SyncJob {
    fn new(
        options: SyncRunOptions,
        store: Store,
        mut config: AppConfig,
        available_adapters: &[Box<dyn adapters::SourceAdapter>],
    ) -> Result<Self> {
        let labels: Vec<_> = available_adapters
            .iter()
            .map(|adapter| (adapter.id().to_string(), adapter.label().to_string()))
            .collect();
        config.normalize_sources(&labels);
        let since_ts = if options.usage_only || options.target_session.is_some() {
            None
        } else {
            config.sync_window.to_since_cutoff()
        };
        let path_excluder = config.build_path_excluder()?;
        let mut job = Self {
            store,
            event_backfill: None,
            options,
            config,
            host: None,
            labels,
            since_ts,
            path_excluder,
            repo_cache: RepoIdentityCache::default(),
            stats: SyncStats::default(),
            adapter_runs: Vec::new(),
            progress: SyncProgress::disabled(),
            started: std::time::Instant::now(),
        };
        if job.options.emit && !job.options.verbose {
            let selected = available_adapters
                .iter()
                .filter(|adapter| job.is_selected(adapter.as_ref()))
                .count();
            job.progress = SyncProgress::for_terminal(selected);
        }
        Ok(job)
    }

    fn passes_filters(&self, adapter: &dyn adapters::SourceAdapter) -> bool {
        if self.options.usage_only
            && !adapters::adapter_supports_usage_dashboard(adapter, self.options.backfill_events)
        {
            return false;
        }
        self.options
            .sources
            .as_ref()
            .is_none_or(|sources| sources.iter().any(|id| id == adapter.id()))
    }

    fn is_selected(&self, adapter: &dyn adapters::SourceAdapter) -> bool {
        self.passes_filters(adapter) && self.config.is_source_enabled(adapter.id())
    }

    fn run_with(
        &mut self,
        available_adapters: &[Box<dyn adapters::SourceAdapter>],
        mut on_source: Option<&mut dyn FnMut(&str)>,
    ) -> Result<()> {
        for adapter in available_adapters {
            self.sync_adapter(adapter.as_ref(), &mut on_source)?;
        }
        self.progress.finish();
        self.report_progress()
    }

    fn sync_adapter(
        &mut self,
        adapter: &dyn adapters::SourceAdapter,
        on_source: &mut Option<&mut dyn FnMut(&str)>,
    ) -> Result<()> {
        if self.event_backfill.is_some() {
            return self.sync_event_backfill_adapter(adapter);
        }
        let source_id = adapter.id();
        let label = adapter.label();

        if !self.passes_filters(adapter) {
            return Ok(());
        }

        if !self.config.is_source_enabled(source_id) {
            if self.options.verbose {
                println!("Skipping {label} (filtered)");
            }
            return Ok(());
        }

        if let Some(on_source) = on_source.as_mut() {
            on_source(source_id);
        }

        self.progress.begin_source(label);
        let started = std::time::Instant::now();
        let touched_before = self.stats.touched();
        let out_of_scope_before = self.stats.out_of_scope;

        let single_session = self.options.target_session.is_some();
        let mut purged_excluded_ids = HashSet::new();
        if !single_session && let Some(matcher) = &self.path_excluder {
            let n = delete_excluded_sessions_for_source(
                &self.store,
                source_id,
                matcher,
                &self.options.scope,
                self.options.target_session.as_deref(),
                &mut purged_excluded_ids,
            )?;
            self.stats.excluded_out += n;
        }

        let context = self.load_adapter_sync_context(source_id)?;
        let (mut existing, repo_backfill) =
            self.resolve_existing_state_parts(context.clone_parts())?;
        let settings = self.scan_settings(label);
        let mut streamed = 0;
        let mut sink_error = None;
        let scan = {
            let mut sink = |raw| {
                if sink_error.is_some() {
                    anyhow::bail!("{label} sync stopped after a write failure");
                }
                self.progress.indexing(label, streamed, None);
                match self.process_raw_session(
                    source_id,
                    raw,
                    &mut existing,
                    &mut purged_excluded_ids,
                ) {
                    Ok(()) => {
                        streamed += 1;
                        Ok(())
                    }
                    Err(error) => {
                        sink_error = Some(error);
                        anyhow::bail!("{label} sync stopped after a write failure")
                    }
                }
            };
            scan_adapter(adapter, &context.with_session_sink(&mut sink), settings)
        };
        if let Some(error) = sink_error {
            return Err(error);
        }
        let Some(scan_result) = self.finish_scan(label, scan, streamed)? else {
            if streamed > 0 {
                self.record_source_run(
                    label,
                    started,
                    touched_before,
                    out_of_scope_before,
                    streamed,
                    Default::default(),
                );
            }
            return Ok(());
        };
        let adapters::SyncScanOutput {
            scan: adapters::SyncScanResult { sessions: raw_sessions, stats: scan, observations },
            reconcile,
        } = scan_result;

        let found = streamed + raw_sessions.len();
        for (done, raw) in (streamed..).zip(raw_sessions) {
            self.progress.indexing(label, done, Some(found));
            self.process_raw_session(source_id, raw, &mut existing, &mut purged_excluded_ids)?;
        }
        self.write_repo_backfill(source_id, repo_backfill, &existing)?;
        self.apply_source_observations(source_id, observations, &mut existing)?;
        if let Some(matcher) = &self.path_excluder {
            let n = delete_excluded_sessions_for_source(
                &self.store,
                source_id,
                matcher,
                &self.options.scope,
                self.options.target_session.as_deref(),
                &mut purged_excluded_ids,
            )?;
            self.stats.excluded_out += n;
        }
        for source_id in &purged_excluded_ids {
            existing.remove(source_id);
        }
        if !single_session {
            self.reconcile_source(source_id, label, reconcile, &mut existing)?;
        }

        self.record_source_run(label, started, touched_before, out_of_scope_before, found, scan);
        info!("{label} done");
        Ok(())
    }

    fn record_source_run(
        &mut self,
        label: &str,
        started: std::time::Instant,
        touched_before: u32,
        out_of_scope_before: u32,
        found: usize,
        scan: adapters::SyncScanStats,
    ) {
        let touched = self.stats.touched() - touched_before;
        let elapsed_ms = started.elapsed().as_millis();
        self.progress.end_source(label, found, touched, scan.unstable_sessions, elapsed_ms);
        self.adapter_runs.push(AdapterRun {
            label: label.to_string(),
            scan,
            out_of_scope: self.stats.out_of_scope - out_of_scope_before,
            touched,
            elapsed_ms,
        });
    }

    fn scan_sessions(
        &mut self,
        adapter: &dyn adapters::SourceAdapter,
        label: &str,
        context: &adapters::AdapterSyncContext,
    ) -> Result<Option<adapters::SyncScanOutput>> {
        let scan = scan_adapter(adapter, context, self.scan_settings(label));
        self.finish_scan(label, scan, 0)
    }

    fn scan_settings(&self, label: &str) -> ScanSettings {
        if self.options.verbose {
            println!("Scanning {label}...");
        }
        ScanSettings {
            since_ts: self.since_ts,
            include_events: !self.options.usage_only || self.options.backfill_events,
            force: self.options.force,
        }
    }

    pub(super) fn finish_scan(
        &mut self,
        label: &str,
        scan: Result<adapters::SyncScanOutput>,
        streamed: usize,
    ) -> Result<Option<adapters::SyncScanOutput>> {
        let scan_result = match scan {
            Ok(scan) => scan,
            Err(error) if self.options.target_session.is_some() => return Err(error),
            Err(error) => {
                if self.options.emit {
                    eprintln!("Error scanning {label}: {error}");
                }
                return Ok(None);
            }
        };
        let found = streamed + scan_result.scan.sessions.len();
        if let Some(target) = &self.options.target_session {
            anyhow::ensure!(
                scan_result.scan.stats.candidates > 0,
                "no {label} session found for {target}"
            );
            anyhow::ensure!(
                found > 0 || scan_result.scan.stats.skipped_sessions > 0,
                "{label} session {target} holds no readable session"
            );
        }
        self.stats.skipped += scan_result.scan.stats.skipped_sessions;
        self.stats.filtered_out += scan_result.scan.stats.filtered_sessions;
        if self.options.verbose {
            println!("  Found {found} sessions");
        }
        Ok(Some(scan_result))
    }
}

#[derive(Clone, Copy)]
pub(super) struct ScanSettings {
    since_ts: Option<i64>,
    include_events: bool,
    force: bool,
}

pub(super) fn scan_adapter(
    adapter: &dyn adapters::SourceAdapter,
    context: &adapters::AdapterSyncContext,
    settings: ScanSettings,
) -> Result<adapters::SyncScanOutput> {
    let optimized = adapter.scan_for_sync_output(
        context,
        settings.since_ts,
        settings.include_events,
        settings.force,
    )?;
    if let Some(scan) = optimized {
        return Ok(scan);
    }
    let sessions = adapter.scan()?;
    let parsed = sessions.len() as u32;
    Ok(adapters::SyncScanOutput {
        scan: adapters::SyncScanResult {
            sessions,
            stats: adapters::SyncScanStats { candidates: parsed, parsed, ..Default::default() },
            observations: Vec::new(),
        },
        reconcile: None,
    })
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
mod tests;
