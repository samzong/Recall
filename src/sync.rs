mod entry;
pub(crate) use entry::{
    run_background_worker, run_cli, run_dashboard_sync_job, run_sync_job_inner, run_usage_sync_job,
    run_usage_sync_job_with_progress, scan_remote_scope,
};

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use tracing::info;

use crate::adapters;
use crate::config::AppConfig;
use crate::db::store::{
    IndexedSessionMeta, ParserStateMeta, SessionPath, SessionTopologyWrite, Store,
};
use crate::project_scope::{ProjectScope, SessionScopeFields};
use crate::repo_identity::{RepoIdentity, RepoIdentityCache};
use crate::sync_progress::{SyncProgress, format_elapsed};
use crate::types::{Message, Role, Session};
use crate::utils;

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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BackfillPlan {
    usage: bool,
    events: bool,
    metadata: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExistingSessionAction {
    Skip,
    BackfillOnly(BackfillPlan),
    RefreshSession,
}

#[derive(Default)]
struct SyncStats {
    new_sessions: u32,
    updated_sessions: u32,
    reprocessed_sessions: u32,
    total_messages: u32,
    skipped: u32,
    filtered_out: u32,
    excluded_out: u32,
    out_of_scope: u32,
}

/// Per-adapter accounting for `--verbose`. Without it a sync reports only
/// totals, which cannot show whether a candidate was rejected before its
/// transcript was read — the only rejection that saves work.
struct AdapterRun {
    label: String,
    scan: adapters::SyncScanStats,
    out_of_scope: u32,
    touched: u32,
    elapsed_ms: u128,
}

impl SyncStats {
    fn touched(&self) -> u32 {
        self.new_sessions + self.updated_sessions + self.reprocessed_sessions
    }
}

#[derive(Default)]
struct EventBackfillReport {
    dry_run: bool,
    scanned: u32,
    missing_original: u32,
    unknown_original: u32,
    unstable: u32,
    no_events: u32,
    unsupported_sessions: u32,
    failed_writes: u32,
    disabled: Vec<String>,
    unsupported_sources: Vec<String>,
    unavailable: Vec<String>,
}

struct ExistingState {
    meta: HashMap<String, IndexedSessionMeta>,
    paths: HashMap<String, SessionPath>,
    imported_ids: HashSet<String>,
    usage_meta: HashMap<String, ParserStateMeta>,
    event_meta: HashMap<String, ParserStateMeta>,
    metadata_meta: HashMap<String, ParserStateMeta>,
}

impl ExistingState {
    fn remove(&mut self, source_id: &str) -> bool {
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

    fn record_replaced(
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

struct SyncJob {
    store: Store,
    event_backfill: Option<EventBackfillReport>,
    options: SyncRunOptions,
    config: AppConfig,
    host: Option<crate::host::Host>,
    labels: Vec<(String, String)>,
    since_ts: Option<i64>,
    path_excluder: Option<globset::GlobSet>,
    repo_cache: RepoIdentityCache,
    stats: SyncStats,
    adapter_runs: Vec<AdapterRun>,
    progress: SyncProgress,
    started: std::time::Instant,
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
        let since_ts = if options.usage_only { None } else { config.sync_window.to_since_cutoff() };
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

        let mut purged_excluded_ids = HashSet::new();
        if let Some(matcher) = &self.path_excluder {
            let n = delete_excluded_sessions_for_source(
                &self.store,
                source_id,
                matcher,
                &self.options.scope,
                &mut purged_excluded_ids,
            )?;
            self.stats.excluded_out += n;
        }

        let context = self.load_adapter_sync_context(source_id)?;
        let Some(scan_result) = self.scan_sessions(adapter, label, &context)? else {
            return Ok(());
        };
        let adapters::SyncScanOutput {
            scan: adapters::SyncScanResult { sessions: raw_sessions, stats: scan, observations },
            reconcile,
        } = scan_result;

        let mut existing = self.prepare_existing_state(source_id, context)?;
        let found = raw_sessions.len();
        for (done, raw) in raw_sessions.into_iter().enumerate() {
            self.progress.indexing(label, done, found);
            self.process_raw_session(source_id, raw, &mut existing, &mut purged_excluded_ids)?;
        }
        self.apply_source_observations(source_id, observations, &mut existing)?;
        if let Some(matcher) = &self.path_excluder {
            let n = delete_excluded_sessions_for_source(
                &self.store,
                source_id,
                matcher,
                &self.options.scope,
                &mut purged_excluded_ids,
            )?;
            self.stats.excluded_out += n;
        }
        for source_id in &purged_excluded_ids {
            existing.remove(source_id);
        }
        self.reconcile_source(source_id, label, reconcile, &mut existing)?;

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

        info!("{label} done");
        Ok(())
    }

    fn sync_event_backfill_adapter(&mut self, adapter: &dyn adapters::SourceAdapter) -> Result<()> {
        let source = adapter.id();
        if self
            .options
            .sources
            .as_ref()
            .is_some_and(|sources| !sources.iter().any(|id| id == source))
        {
            return Ok(());
        }
        if !self.config.is_source_enabled(source) {
            self.event_backfill.as_mut().unwrap().disabled.push(source.to_string());
            return Ok(());
        }
        if !adapters::source_supports_event_backfill(source) {
            self.event_backfill.as_mut().unwrap().unsupported_sources.push(source.to_string());
            return Ok(());
        }
        let context = self.load_adapter_sync_context(source)?;
        let paths = context
            .session_paths()
            .filter(|path| {
                path_matches_scope(&self.options.scope, path, &mut self.repo_cache)
                    && !self.path_excluder.as_ref().is_some_and(|matcher| {
                        paths_match_excluded(
                            path.directory.as_deref(),
                            path.source_file_path.as_deref(),
                            matcher,
                        )
                    })
            })
            .cloned()
            .collect::<Vec<_>>();
        let scan = self.scan_sessions(adapter, adapter.label(), &context)?;
        let Some(scan) = scan else {
            self.event_backfill.as_mut().unwrap().unavailable.push(source.to_string());
            self.event_backfill.as_mut().unwrap().unknown_original += paths.len() as u32;
            return Ok(());
        };
        let observed = scan
            .scan
            .sessions
            .iter()
            .map(|raw| raw.source_id.as_str())
            .chain(scan.scan.observations.iter().map(|observation| observation.source_id.as_str()))
            .collect::<HashSet<_>>();
        for path in &paths {
            if observed.contains(path.source_id.as_str()) {
                continue;
            }
            let missing_record = matches!(&scan.reconcile, Some(adapters::ReconcilePlan::CompleteLiveSet(live)) if !live.contains(&path.source_id));
            let missing_path = path
                .source_file_path
                .as_deref()
                .is_some_and(|path| matches!(std::path::Path::new(path).try_exists(), Ok(false)));
            if missing_record || missing_path {
                self.event_backfill.as_mut().unwrap().missing_original += 1;
            } else {
                self.event_backfill.as_mut().unwrap().unknown_original += 1;
            }
        }
        let report = self.event_backfill.as_mut().unwrap();
        report.scanned += scan.scan.stats.parsed;
        report.unstable += scan.scan.stats.unstable_sessions;
        if matches!(
            scan.reconcile,
            Some(
                adapters::ReconcilePlan::PartialInventory(_)
                    | adapters::ReconcilePlan::UnavailableInventory(_)
            )
        ) {
            report.unavailable.push(source.to_string());
        }
        let mut existing = self.prepare_existing_state(source, context)?;
        for raw in scan.scan.sessions {
            if let Err(error) =
                self.process_raw_session(source, raw, &mut existing, &mut HashSet::new())
            {
                self.event_backfill.as_mut().unwrap().failed_writes += 1;
                if self.options.emit {
                    eprintln!("Event backfill failed for {source}: {error}");
                }
            }
        }
        Ok(())
    }

    fn scan_sessions(
        &mut self,
        adapter: &dyn adapters::SourceAdapter,
        label: &str,
        context: &adapters::AdapterSyncContext,
    ) -> Result<Option<adapters::SyncScanOutput>> {
        if self.options.verbose {
            println!("Scanning {label}...");
        }
        let include_events = !self.options.usage_only || self.options.backfill_events;
        let scan = adapter
            .scan_for_sync_output(context, self.since_ts, include_events, self.options.force)
            .and_then(|optimized| match optimized {
                Some(scan) => Ok(scan),
                None => adapter.scan().map(|sessions| {
                    let parsed = sessions.len() as u32;
                    adapters::SyncScanOutput {
                        scan: adapters::SyncScanResult {
                            sessions,
                            stats: adapters::SyncScanStats {
                                candidates: parsed,
                                parsed,
                                ..Default::default()
                            },
                            observations: Vec::new(),
                        },
                        reconcile: None,
                    }
                }),
            });
        let scan_result = match scan {
            Ok(scan) => scan,
            Err(error) => {
                if self.options.emit {
                    eprintln!("Error scanning {label}: {error}");
                }
                return Ok(None);
            }
        };
        self.stats.skipped += scan_result.scan.stats.skipped_sessions;
        self.stats.filtered_out += scan_result.scan.stats.filtered_sessions;
        if self.options.verbose {
            println!("  Found {} sessions", scan_result.scan.sessions.len());
        }
        Ok(Some(scan_result))
    }

    fn apply_source_observations(
        &mut self,
        source_id: &str,
        observations: Vec<adapters::SourceObservation>,
        existing: &mut ExistingState,
    ) -> Result<()> {
        for observation in observations {
            let Some(stored) = existing.paths.get_mut(&observation.source_id) else {
                continue;
            };
            if !path_matches_scope(&self.options.scope, stored, &mut self.repo_cache) {
                continue;
            }
            if let Some(source_file_path) = observation.source_file_path.as_deref()
                && stored.source_file_path.as_deref() != Some(source_file_path)
            {
                self.store.update_session_fields(
                    source_id,
                    &observation.source_id,
                    None,
                    None,
                    None,
                    Some(source_file_path),
                )?;
                stored.source_file_path = Some(source_file_path.to_string());
            }
            if existing.imported_ids.remove(&observation.source_id) {
                self.store.clear_import_marker(source_id, &observation.source_id)?;
            }
            if let Some(host) = &self.host {
                host.observe(&self.store.conn, source_id, &observation.source_id)?;
            }
        }
        Ok(())
    }

    fn reconcile_source(
        &mut self,
        source_id: &str,
        label: &str,
        reconcile: Option<adapters::ReconcilePlan>,
        existing: &mut ExistingState,
    ) -> Result<()> {
        if !matches!(self.options.scope, ProjectScope::Global) {
            return Ok(());
        }
        let Some(reconcile) = reconcile else {
            return Ok(());
        };
        match reconcile {
            adapters::ReconcilePlan::PartialInventory(issues) => {
                self.report_incomplete_inventory(label, "partial", &issues);
            }
            adapters::ReconcilePlan::UnavailableInventory(issues) => {
                self.report_incomplete_inventory(label, "unavailable", &issues);
            }
            adapters::ReconcilePlan::CompleteLiveSet(live) => {
                let stale = existing
                    .meta
                    .keys()
                    .filter(|id| !live.contains(*id))
                    .cloned()
                    .collect::<Vec<_>>();
                for source_id_to_delete in stale {
                    self.store.delete_session_data(source_id, &source_id_to_delete)?;
                    existing.remove(&source_id_to_delete);
                }
            }
            adapters::ReconcilePlan::ExactTombstones(source_ids) => {
                for source_id_to_delete in source_ids {
                    if existing.remove(&source_id_to_delete) {
                        self.store.delete_session_data(source_id, &source_id_to_delete)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn report_incomplete_inventory(
        &self,
        label: &str,
        state: &str,
        issues: &[adapters::InventoryIssue],
    ) {
        if !self.options.emit {
            return;
        }
        if let Some(issue) = issues.first() {
            eprintln!(
                "Reconciliation skipped for {label}: inventory {state} at {} ({:?}; {} issue(s)).",
                issue.path.display(),
                issue.category,
                issues.len()
            );
        } else {
            eprintln!("Reconciliation skipped for {label}: inventory {state}.");
        }
    }

    fn load_adapter_sync_context(&self, source_id: &str) -> Result<adapters::AdapterSyncContext> {
        Ok(adapters::AdapterSyncContext::new(
            source_id.to_string(),
            self.store.session_meta_map(source_id)?,
            self.store
                .session_paths_for_source(source_id)?
                .into_iter()
                .map(|path| (path.source_id.clone(), path))
                .collect(),
            self.store.imported_source_ids(source_id)?,
            self.store.usage_state_meta_map(source_id)?,
            self.store.event_state_meta_map(source_id)?,
            self.store.metadata_state_meta_map(source_id)?,
        ))
    }

    fn prepare_existing_state(
        &mut self,
        source_id: &str,
        context: adapters::AdapterSyncContext,
    ) -> Result<ExistingState> {
        let adapters::AdapterSyncContextParts {
            session_meta: meta,
            session_paths: mut paths,
            imported_ids,
            usage_state: usage_meta,
            event_state: event_meta,
            metadata_state: metadata_meta,
        } = context.into_parts();
        let backfill_identity =
            self.event_backfill.is_none() && matches!(self.options.scope, ProjectScope::Global);
        for path in paths.values_mut() {
            if backfill_identity
                && path.directory.is_some()
                && (path.repo_remote.is_none()
                    || path.repo_slug.is_none()
                    || path.repo_name.is_none())
            {
                let repo_identity = self.repo_cache.resolve(path.directory.as_deref());
                if let Some(repo) = repo_identity.as_ref() {
                    self.store.update_session_repo_identity(source_id, &path.source_id, repo)?;
                    path.repo_remote = Some(repo.remote.clone());
                    path.repo_slug = Some(repo.slug.clone());
                    path.repo_name = Some(repo.name.clone());
                }
            }
        }
        Ok(ExistingState { meta, paths, imported_ids, usage_meta, event_meta, metadata_meta })
    }

    fn process_raw_session(
        &mut self,
        source_id: &str,
        mut raw: adapters::RawSession,
        existing: &mut ExistingState,
        purged_excluded_ids: &mut HashSet<String>,
    ) -> Result<()> {
        let raw_source_id = raw.source_id.clone();

        // Runs before every write and delete below, so a scoped sync can never
        // touch a session outside its scope.
        let repo_identity = self.repo_cache.resolve(raw.directory.as_deref());
        if !self
            .options
            .scope
            .matches(SessionScopeFields::new(raw.directory.as_deref(), repo_identity.as_ref()))
        {
            self.stats.out_of_scope += 1;
            return Ok(());
        }

        if let Some(matcher) = &self.path_excluder
            && paths_match_excluded(
                raw.directory.as_deref(),
                raw.source_file_path.as_deref(),
                matcher,
            )
        {
            if self.event_backfill.is_none() && existing.remove(&raw_source_id) {
                self.store.delete_session_data(source_id, &raw_source_id)?;
            }
            if purged_excluded_ids.insert(raw_source_id) {
                self.stats.excluded_out += 1;
            }
            return Ok(());
        }

        if self.event_backfill.is_none()
            && let Some(source_file_path) = raw.source_file_path.as_deref()
            && let Some(stored) = existing.paths.get_mut(&raw_source_id)
            && stored.source_file_path.as_deref() != Some(source_file_path)
        {
            self.store.update_session_fields(
                source_id,
                &raw_source_id,
                None,
                None,
                None,
                Some(source_file_path),
            )?;
            stored.source_file_path = Some(source_file_path.to_string());
        }

        if let Some(cutoff) = self.since_ts {
            let ts = raw.updated_at.unwrap_or(raw.started_at);
            if ts < cutoff {
                self.stats.filtered_out += 1;
                return Ok(());
            }
        }

        for event in &mut raw.events {
            for file in &mut event.files {
                file.target = self.repo_cache.resolve_file(&file.path, file.cwd.as_deref());
            }
        }

        if let Some(report) = &mut self.event_backfill {
            let Some(version) = raw.event_parser_version else {
                report.unsupported_sessions += 1;
                return Ok(());
            };
            if raw.events.is_empty() {
                report.no_events += 1;
                return Ok(());
            }
            if existing.meta.contains_key(&raw_source_id) {
                if !self.options.force
                    && crate::adapters::sync_state::parser_state_is_current(
                        version,
                        existing.event_meta.get(&raw_source_id).copied(),
                        raw.updated_at,
                    )
                {
                    self.stats.skipped += 1;
                    return Ok(());
                }
                if report.dry_run {
                    self.stats.reprocessed_sessions += 1;
                    return Ok(());
                }
                return self.apply_backfill(
                    source_id,
                    &raw_source_id,
                    &mut raw,
                    BackfillPlan { usage: false, events: true, metadata: false },
                    false,
                    existing,
                );
            }
            if report.dry_run {
                self.stats.new_sessions += 1;
                return Ok(());
            }
        }

        let existing_repo_fields = existing.paths.get(&raw_source_id).filter(|old| {
            repo_identity.is_none() && old.directory.as_deref() == raw.directory.as_deref()
        });
        let (repo_remote, repo_slug, repo_name) = match repo_identity.as_ref() {
            Some(repo) => {
                (Some(repo.remote.clone()), Some(repo.slug.clone()), Some(repo.name.clone()))
            }
            None => existing_repo_fields
                .map(|old| (old.repo_remote.clone(), old.repo_slug.clone(), old.repo_name.clone()))
                .unwrap_or((None, None, None)),
        };
        let msg_count = raw.messages.len() as u32;
        let needs_backfill = |version: Option<u32>, state: Option<&ParserStateMeta>| {
            version.is_some_and(|version| {
                !crate::adapters::sync_state::parser_state_is_current(
                    version,
                    state.copied(),
                    raw.updated_at,
                )
            })
        };
        let usage_backfill_needed =
            needs_backfill(raw.usage_parser_version, existing.usage_meta.get(&raw_source_id));
        let event_backfill_needed = (self.options.backfill_events || !self.options.usage_only)
            && needs_backfill(raw.event_parser_version, existing.event_meta.get(&raw_source_id));
        let metadata_parser_version = raw.metadata_parser_version;
        let metadata_backfill_needed = !self.options.usage_only
            && needs_backfill(metadata_parser_version, existing.metadata_meta.get(&raw_source_id));

        let session_uuid = match existing.meta.get(&raw_source_id).cloned() {
            Some(old) => {
                let was_imported = existing.imported_ids.remove(&raw_source_id);
                let metadata_changed = existing.paths.get(&raw_source_id).is_some_and(|old| {
                    raw_session_metadata_changed(&raw, repo_identity.as_ref(), old)
                });
                let content_changed = old.message_count != msg_count
                    || metadata_changed
                    || (raw.updated_at.is_some() && raw.updated_at != old.updated_at)
                    || (raw.refresh_session_on_metadata_backfill && metadata_backfill_needed);
                match decide_existing_session_action(
                    self.options.usage_only,
                    self.options.backfill_events,
                    self.options.force,
                    content_changed,
                    usage_backfill_needed,
                    event_backfill_needed,
                    metadata_backfill_needed,
                ) {
                    ExistingSessionAction::Skip => {
                        if was_imported {
                            self.store.clear_import_marker(source_id, &raw_source_id)?;
                        }
                        if let Some(host) = &self.host {
                            host.observe(&self.store.conn, source_id, &raw_source_id)?;
                        }
                        self.stats.skipped += 1;
                        return Ok(());
                    }
                    ExistingSessionAction::BackfillOnly(plan) => {
                        self.apply_backfill(
                            source_id,
                            &raw_source_id,
                            &mut raw,
                            plan,
                            was_imported,
                            existing,
                        )?;
                        if let Some(host) = &self.host {
                            host.observe(&self.store.conn, source_id, &raw_source_id)?;
                        }
                        return Ok(());
                    }
                    ExistingSessionAction::RefreshSession => {}
                }
                existing.usage_meta.remove(&raw_source_id);
                existing.event_meta.remove(&raw_source_id);
                existing.metadata_meta.remove(&raw_source_id);
                if content_changed {
                    self.stats.updated_sessions += 1;
                } else {
                    self.stats.reprocessed_sessions += 1;
                }
                old.id
            }
            None => {
                self.stats.new_sessions += 1;
                uuid::Uuid::new_v4().to_string()
            }
        };

        let title = raw
            .custom_title
            .clone()
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| generate_title(&raw.messages));

        let session = Session {
            id: session_uuid.clone(),
            source: source_id.to_string(),
            source_id: raw.source_id,
            title,
            directory: raw.directory,
            repo_remote,
            repo_slug,
            repo_name,
            started_at: raw.started_at,
            updated_at: raw.updated_at,
            message_count: msg_count,
            entrypoint: raw.entrypoint,
            custom_title: raw.custom_title,
            summary: raw.summary,
            duration_minutes: raw.duration_minutes,
            source_file_path: raw.source_file_path,
            is_import: false,
            locations: Vec::new(),
            alternative_versions: 0,
        };

        let messages: Vec<Message> = raw
            .messages
            .into_iter()
            .enumerate()
            .map(|(i, m)| Message {
                session_id: session_uuid.clone(),
                role: m.role,
                content: m.content,
                timestamp: m.timestamp,
                seq: i as u32,
            })
            .collect();

        let persist_events = !self.options.usage_only || self.options.backfill_events;
        let (events, event_parser_version) = if persist_events {
            (raw.events, raw.event_parser_version)
        } else {
            (Vec::new(), None)
        };

        let topology = SessionTopologyWrite {
            thread_role: raw.thread_role,
            parents: &raw.parent_links,
            parser_version: metadata_parser_version,
        };
        self.store.replace_session_with_usage_and_events_with_topology(
            source_id,
            &raw_source_id,
            &session,
            &messages,
            &raw.usage_events,
            raw.usage_parser_version,
            &events,
            event_parser_version,
            &topology,
        )?;
        if let Some(host) = &self.host {
            host.observe(&self.store.conn, source_id, &raw_source_id)?;
        }
        existing.record_replaced(
            &session,
            raw.usage_parser_version,
            event_parser_version,
            metadata_parser_version,
        );
        self.stats.total_messages += msg_count;
        Ok(())
    }

    fn apply_backfill(
        &mut self,
        source_id: &str,
        raw_source_id: &str,
        raw: &mut adapters::RawSession,
        plan: BackfillPlan,
        was_imported: bool,
        existing: &mut ExistingState,
    ) -> Result<()> {
        if (plan.usage && raw.usage_events.iter().any(|event| event.message_seq.is_some()))
            || (plan.events && raw.events.iter().any(|event| event.message_seq.is_some()))
        {
            let stored = existing
                .meta
                .get(raw_source_id)
                .map(|session| self.store.get_messages(&session.id))
                .transpose()?;
            let same_messages = stored.as_ref().is_some_and(|stored| {
                stored.len() == raw.messages.len()
                    && stored.iter().zip(&raw.messages).enumerate().all(
                        |(index, (stored, parsed))| {
                            stored.seq == index as u32
                                && stored.role == parsed.role
                                && stored.content == parsed.content
                                && stored.timestamp == parsed.timestamp
                        },
                    )
            });
            if !same_messages {
                for event in &mut raw.events {
                    event.message_seq = None;
                }
                for event in &mut raw.usage_events {
                    event.message_seq = None;
                }
            }
        }
        let mut reprocessed = false;
        if plan.usage
            && let Some(parser_version) = raw.usage_parser_version
            && self.store.persist_usage_events_for_existing_session(
                source_id,
                raw_source_id,
                &raw.usage_events,
                parser_version,
                raw.updated_at,
            )?
        {
            existing.usage_meta.insert(
                raw_source_id.to_string(),
                ParserStateMeta { parser_version, source_updated_at: raw.updated_at },
            );
            reprocessed = true;
        }
        if plan.events
            && let Some(parser_version) = raw.event_parser_version
            && self.store.persist_session_events_for_existing_session(
                source_id,
                raw_source_id,
                &raw.events,
                parser_version,
                raw.updated_at,
            )?
        {
            existing.event_meta.insert(
                raw_source_id.to_string(),
                ParserStateMeta { parser_version, source_updated_at: raw.updated_at },
            );
            reprocessed = true;
        }
        if plan.metadata
            && let Some(parser_version) = raw.metadata_parser_version
        {
            let topology = SessionTopologyWrite {
                thread_role: raw.thread_role,
                parents: &raw.parent_links,
                parser_version: Some(parser_version),
            };
            if self.store.persist_topology_for_existing_session(
                source_id,
                raw_source_id,
                &topology,
            )? {
                existing.metadata_meta.insert(
                    raw_source_id.to_string(),
                    ParserStateMeta { parser_version, source_updated_at: raw.updated_at },
                );
                reprocessed = true;
            }
        }
        if self.event_backfill.is_none()
            && (raw.custom_title.is_some()
                || raw.summary.is_some()
                || raw.duration_minutes.is_some())
        {
            self.store.update_session_fields(
                source_id,
                raw_source_id,
                raw.custom_title.as_deref(),
                raw.summary.as_deref(),
                raw.duration_minutes,
                None,
            )?;
        }
        if was_imported {
            self.store.clear_import_marker(source_id, raw_source_id)?;
        }
        if reprocessed {
            self.stats.reprocessed_sessions += 1;
        }
        Ok(())
    }

    /// The evidence a scan-level optimisation has to move: how many candidates
    /// each adapter considered, how many it rejected without reading the
    /// transcript, and how many transcripts it actually parsed.
    fn report_adapter_breakdown(&self) {
        let runs = &self.adapter_runs;
        if runs.is_empty() {
            return;
        }

        println!();
        println!(
            "{:<6} {:>10} {:>14} {:>8} {:>8} {:>9} {:>8}",
            "Source", "candidates", "pre-parse rej", "parsed", "scoped", "touched", "ms"
        );
        for run in runs {
            println!(
                "{:<6} {:>10} {:>14} {:>8} {:>8} {:>9} {:>8}",
                run.label,
                run.scan.candidates,
                run.scan.rejected_before_parse,
                run.scan.parsed,
                run.out_of_scope,
                run.touched,
                run.elapsed_ms
            );
        }
        let total = |f: fn(&AdapterRun) -> u32| runs.iter().map(f).sum::<u32>();
        println!(
            "{:<6} {:>10} {:>14} {:>8} {:>8} {:>9} {:>8}",
            "total",
            total(|run| run.scan.candidates),
            total(|run| run.scan.rejected_before_parse),
            total(|run| run.scan.parsed),
            total(|run| run.out_of_scope),
            total(|run| run.touched),
            runs.iter().map(|run| run.elapsed_ms).sum::<u128>()
        );
    }

    fn report_progress(&self) -> Result<()> {
        if let Some(report) = &self.event_backfill {
            if self.options.emit {
                println!(
                    "Event backfill {}: scanned={}, new={}, updated={}, unchanged={}, excluded={}, out_of_scope={}, no_events={}, unsupported_sessions={}, missing_original={}, unknown_original={}, unstable={}, failed_writes={}, parse_failures=unknown",
                    if report.dry_run { "preview" } else { "finished" },
                    report.scanned,
                    self.stats.new_sessions,
                    self.stats.reprocessed_sessions,
                    self.stats.skipped,
                    self.stats.excluded_out,
                    self.stats.out_of_scope,
                    report.no_events,
                    report.unsupported_sessions,
                    report.missing_original,
                    report.unknown_original,
                    report.unstable,
                    report.failed_writes
                );
                for (label, sources) in [
                    ("disabled", &report.disabled),
                    ("unsupported", &report.unsupported_sources),
                    ("unavailable", &report.unavailable),
                ] {
                    if !sources.is_empty() {
                        println!("{label}: {}", sources.join(", "));
                    }
                }
            }
            anyhow::ensure!(
                report.failed_writes == 0,
                "event backfill could not persist {} sessions",
                report.failed_writes
            );
            return Ok(());
        }

        let SyncStats {
            new_sessions,
            updated_sessions,
            reprocessed_sessions,
            total_messages,
            skipped,
            filtered_out,
            excluded_out,
            out_of_scope,
        } = self.stats;
        let touched = self.stats.touched();

        if self.options.verbose {
            println!();
            if self.options.force {
                print!(
                    "Force sync: {new_sessions} new, {updated_sessions} updated, {reprocessed_sessions} reprocessed, {total_messages} messages"
                );
            } else {
                print!(
                    "Sync: {new_sessions} new, {updated_sessions} updated, {skipped} unchanged, {total_messages} messages"
                );
            }
            for (count, label) in [
                (filtered_out, "outside configured time scope"),
                (out_of_scope, "outside project scope"),
                (excluded_out, "excluded by excluded_paths"),
            ] {
                if count > 0 {
                    print!(", {count} {label}");
                }
            }
            println!();
            self.report_adapter_breakdown();
            println!(
                "Settings: sources [{}], time scope [{}]",
                self.labels
                    .iter()
                    .filter(|(id, _)| self.config.is_source_enabled(id))
                    .map(|(_, label)| label.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                self.config.sync_window.label()
            );
            let progress = self.store.semantic_progress()?;
            if progress.total_sessions > 0 {
                println!(
                    "Semantic queue: {}/{} done, {} pending, {} failed",
                    progress.done_sessions,
                    progress.total_sessions,
                    progress.pending_sessions + progress.processing_sessions,
                    progress.failed_sessions
                );
            }
        } else if self.options.emit {
            let elapsed = format_elapsed(self.started.elapsed().as_millis());
            if self.options.force {
                println!("Reprocessed {touched} sessions, {total_messages} messages in {elapsed}");
            } else if touched == 0 {
                println!("Up to date ({elapsed}).");
            } else if reprocessed_sessions > 0 {
                println!(
                    "{new_sessions} new, {updated_sessions} updated, {reprocessed_sessions} backfilled, {total_messages} messages in {elapsed}"
                );
            } else {
                println!(
                    "{new_sessions} new, {updated_sessions} updated, {total_messages} messages in {elapsed}"
                );
            }
        }

        Ok(())
    }
}

#[cfg(any(test, feature = "bench"))]
pub(crate) fn persist_raw_session_for_conformance(
    store: Store,
    source: &str,
    raw: adapters::RawSession,
) -> Result<Store> {
    let mut job = SyncJob::new(
        SyncRunOptions {
            force: false,
            verbose: false,
            emit: false,
            usage_only: false,
            backfill_events: false,
            sources: None,
            scope: ProjectScope::Global,
        },
        store,
        AppConfig::default(),
        &[],
    )?;
    let context = job.load_adapter_sync_context(source)?;
    let mut existing = job.prepare_existing_state(source, context)?;
    job.process_raw_session(source, raw, &mut existing, &mut HashSet::new())?;
    Ok(job.store)
}

#[allow(clippy::too_many_arguments)]
fn decide_existing_session_action(
    usage_only: bool,
    backfill_events: bool,
    force: bool,
    content_changed: bool,
    usage_backfill_needed: bool,
    event_backfill_needed: bool,
    metadata_backfill_needed: bool,
) -> ExistingSessionAction {
    if !usage_only && (content_changed || force) {
        return ExistingSessionAction::RefreshSession;
    }
    let plan = BackfillPlan {
        usage: usage_backfill_needed,
        events: event_backfill_needed && (!usage_only || backfill_events),
        metadata: metadata_backfill_needed && !usage_only,
    };
    if plan.usage || plan.events || plan.metadata {
        ExistingSessionAction::BackfillOnly(plan)
    } else {
        ExistingSessionAction::Skip
    }
}

fn raw_session_metadata_changed(
    raw: &adapters::RawSession,
    repo_identity: Option<&RepoIdentity>,
    old: &SessionPath,
) -> bool {
    let repo_changed = repo_identity.is_some_and(|repo| {
        old.repo_remote.as_deref() != Some(repo.remote.as_str())
            || old.repo_slug.as_deref() != Some(repo.slug.as_str())
            || old.repo_name.as_deref() != Some(repo.name.as_str())
    });
    raw.directory.as_deref().is_some_and(|directory| old.directory.as_deref() != Some(directory))
        || raw
            .source_file_path
            .as_deref()
            .is_some_and(|path| old.source_file_path.as_deref() != Some(path))
        || repo_changed
}

fn generate_title(messages: &[adapters::RawMessage]) -> String {
    let user_contents: Vec<&str> =
        messages.iter().filter(|m| m.role == Role::User).map(|m| m.content.as_str()).collect();
    utils::title_from_user_messages(&user_contents)
}

fn delete_excluded_sessions_for_source(
    store: &Store,
    source_id: &str,
    matcher: &globset::GlobSet,
    scope: &ProjectScope,
    deleted: &mut HashSet<String>,
) -> Result<u32> {
    let mut count = 0;
    for path in store.session_paths_for_source(source_id)? {
        if !scope.matches(SessionScopeFields {
            directory: path.directory.as_deref(),
            repo_remote: path.repo_remote.as_deref(),
            repo_slug: path.repo_slug.as_deref(),
            repo_name: path.repo_name.as_deref(),
        }) {
            continue;
        }
        if paths_match_excluded(
            path.directory.as_deref(),
            path.source_file_path.as_deref(),
            matcher,
        ) {
            let source_id_to_delete = path.source_id;
            store.delete_session_data(source_id, &source_id_to_delete)?;
            if deleted.insert(source_id_to_delete) {
                count += 1;
            }
        }
    }
    Ok(count)
}

fn path_matches_scope(
    scope: &ProjectScope,
    path: &SessionPath,
    cache: &mut RepoIdentityCache,
) -> bool {
    let repo = cache.resolve(path.directory.as_deref());
    scope.matches(SessionScopeFields {
        directory: path.directory.as_deref(),
        repo_remote: path
            .repo_remote
            .as_deref()
            .or_else(|| repo.as_ref().map(|repo| repo.remote.as_str())),
        repo_slug: path
            .repo_slug
            .as_deref()
            .or_else(|| repo.as_ref().map(|repo| repo.slug.as_str())),
        repo_name: path
            .repo_name
            .as_deref()
            .or_else(|| repo.as_ref().map(|repo| repo.name.as_str())),
    })
}

fn paths_match_excluded(
    directory: Option<&str>,
    source_file_path: Option<&str>,
    matcher: &globset::GlobSet,
) -> bool {
    directory.is_some_and(|path| matcher.is_match(path))
        || source_file_path.is_some_and(|path| path_or_ancestor_matches(path, matcher))
}

fn path_or_ancestor_matches(path: &str, matcher: &globset::GlobSet) -> bool {
    let path = std::path::Path::new(path);
    path.ancestors().any(|candidate| matcher.is_match(candidate))
}

#[cfg(test)]
mod tests;
