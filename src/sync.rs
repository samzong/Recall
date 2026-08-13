use std::collections::{HashMap, HashSet};

use anyhow::Result;
use tracing::info;

use crate::adapters;
use crate::config::AppConfig;
use crate::db::store::{
    EventSessionStateMeta, MetadataSessionStateMeta, SessionPath, SessionTopologyWrite, Store,
    UsageSessionStateMeta,
};
use crate::project_scope::{ProjectScope, SessionScopeFields};
use crate::query::resolve_source_filter;
use crate::repo_identity::{RepoIdentity, RepoIdentityCache};
use crate::semantic;
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

pub(crate) fn run_cli(
    force: bool,
    verbose: bool,
    source_filter: Option<&str>,
    project_filter: Option<&str>,
) -> Result<()> {
    let labels = adapters::source_labels();
    let sources = resolve_source_filter(source_filter, &labels)?;
    let scope = Store::open()?.resolve_scope(project_filter, None)?.announce();
    run_sync_job_inner(SyncRunOptions {
        force,
        verbose,
        emit: true,
        usage_only: false,
        backfill_events: false,
        sources,
        scope,
    })?;
    semantic::ensure_background_worker(false)?;
    Ok(())
}

pub(crate) fn run_usage_sync_job() -> Result<()> {
    run_sync_job_inner(SyncRunOptions {
        force: false,
        verbose: false,
        emit: false,
        usage_only: true,
        backfill_events: false,
        sources: None,
        scope: ProjectScope::Global,
    })
}

pub(crate) fn run_dashboard_sync_job() -> Result<()> {
    run_sync_job_inner(SyncRunOptions {
        force: false,
        verbose: false,
        emit: false,
        usage_only: true,
        backfill_events: true,
        sources: None,
        scope: ProjectScope::Global,
    })
}

pub(crate) fn run_background_worker(sync_first: bool) -> Result<()> {
    semantic::run_background_worker(sync_first, || {
        run_sync_job_inner(SyncRunOptions {
            force: false,
            verbose: false,
            emit: false,
            usage_only: false,
            backfill_events: false,
            sources: None,
            scope: ProjectScope::Global,
        })
    })
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

struct ExistingState {
    meta: HashMap<String, (Option<i64>, u32)>,
    paths: HashMap<String, SessionPath>,
    imported_ids: HashSet<String>,
    usage_meta: HashMap<String, UsageSessionStateMeta>,
    event_meta: HashMap<String, EventSessionStateMeta>,
    metadata_meta: HashMap<String, MetadataSessionStateMeta>,
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
        self.meta.insert(session.source_id.clone(), (session.updated_at, session.message_count));
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
        if let Some(parser_version) = usage_parser_version {
            self.usage_meta.insert(
                session.source_id.clone(),
                UsageSessionStateMeta { parser_version, source_updated_at: session.updated_at },
            );
        }
        if let Some(parser_version) = event_parser_version {
            self.event_meta.insert(
                session.source_id.clone(),
                EventSessionStateMeta { parser_version, source_updated_at: session.updated_at },
            );
        }
        if let Some(parser_version) = metadata_parser_version {
            self.metadata_meta.insert(
                session.source_id.clone(),
                MetadataSessionStateMeta { parser_version, source_updated_at: session.updated_at },
            );
        }
    }
}

pub(crate) fn run_sync_job_inner(options: SyncRunOptions) -> Result<()> {
    let available_adapters = adapters::all_adapters();
    let config = AppConfig::load()?;
    SyncJob::new(options, Store::open()?, config, &available_adapters)?.run(&available_adapters)
}

struct SyncJob {
    store: Store,
    options: SyncRunOptions,
    config: AppConfig,
    labels: Vec<(String, String)>,
    since_ts: Option<i64>,
    path_excluder: Option<globset::GlobSet>,
    repo_cache: RepoIdentityCache,
    stats: SyncStats,
    adapter_runs: Vec<AdapterRun>,
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
        Ok(Self {
            store,
            options,
            config,
            labels,
            since_ts,
            path_excluder,
            repo_cache: RepoIdentityCache::default(),
            stats: SyncStats::default(),
            adapter_runs: Vec::new(),
        })
    }

    fn run(&mut self, available_adapters: &[Box<dyn adapters::SourceAdapter>]) -> Result<()> {
        for adapter in available_adapters {
            self.sync_adapter(adapter.as_ref())?;
        }
        self.report_progress()
    }

    fn sync_adapter(&mut self, adapter: &dyn adapters::SourceAdapter) -> Result<()> {
        let source_id = adapter.id();
        let label = adapter.label();

        if self.options.usage_only
            && !adapters::adapter_supports_usage_dashboard(adapter, self.options.backfill_events)
        {
            return Ok(());
        }

        if let Some(sources) = &self.options.sources
            && !sources.iter().any(|id| id == source_id)
        {
            return Ok(());
        }

        if !self.config.is_source_enabled(source_id) {
            if self.options.verbose {
                println!("Skipping {label} (filtered)");
            }
            return Ok(());
        }

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

        let Some((raw_sessions, scan)) =
            self.scan_sessions(adapter, source_id, label, &mut purged_excluded_ids)?
        else {
            return Ok(());
        };

        let mut existing = self.load_existing_state(source_id)?;
        for raw in raw_sessions {
            self.process_raw_session(source_id, raw, &mut existing, &mut purged_excluded_ids)?;
        }

        self.adapter_runs.push(AdapterRun {
            label: label.to_string(),
            scan,
            out_of_scope: self.stats.out_of_scope - out_of_scope_before,
            touched: self.stats.touched() - touched_before,
            elapsed_ms: started.elapsed().as_millis(),
        });

        info!("{label} done");
        Ok(())
    }

    fn scan_sessions(
        &mut self,
        adapter: &dyn adapters::SourceAdapter,
        source_id: &str,
        label: &str,
        purged_excluded_ids: &mut HashSet<String>,
    ) -> Result<Option<(Vec<adapters::RawSession>, adapters::SyncScanStats)>> {
        if self.options.verbose {
            println!("Scanning {label}...");
        }
        // Prune deletes every row whose source file disappeared, which a
        // scoped run must not do outside its scope. Adapters cannot prune by
        // scope, so a scoped sync leaves it to global runs.
        if matches!(self.options.scope, ProjectScope::Global)
            && let Err(e) = adapter.prune(&self.store)
            && self.options.emit
        {
            eprintln!("Error pruning {label}: {e}");
        }
        let include_events = !self.options.usage_only || self.options.backfill_events;
        let optimized = if self.options.force {
            None
        } else {
            match adapter.scan_for_sync(&self.store, self.since_ts, include_events) {
                Ok(scan) => scan,
                Err(e) => {
                    if self.options.emit {
                        eprintln!("Error scanning {label}: {e}");
                    }
                    return Ok(None);
                }
            }
        };
        let (raw_sessions, scan_stats) = match optimized {
            Some(scan) => (scan.sessions, scan.stats),
            None => {
                let raw_sessions = match adapter.scan() {
                    Ok(s) => s,
                    Err(e) => {
                        if self.options.emit {
                            eprintln!("Error scanning {label}: {e}");
                        }
                        return Ok(None);
                    }
                };
                // A full scan parses everything it finds; there is no
                // candidate stage to account for separately.
                let parsed = raw_sessions.len() as u32;
                (
                    raw_sessions,
                    adapters::SyncScanStats { candidates: parsed, parsed, ..Default::default() },
                )
            }
        };
        self.stats.skipped += scan_stats.skipped_sessions;
        self.stats.filtered_out += scan_stats.filtered_sessions;
        if let Some(matcher) = &self.path_excluder {
            let n = delete_excluded_sessions_for_source(
                &self.store,
                source_id,
                matcher,
                &self.options.scope,
                purged_excluded_ids,
            )?;
            self.stats.excluded_out += n;
        }
        if self.options.verbose {
            println!("  Found {} sessions", raw_sessions.len());
        }
        Ok(Some((raw_sessions, scan_stats)))
    }

    fn load_existing_state(&mut self, source_id: &str) -> Result<ExistingState> {
        let meta = self.store.session_meta_map(source_id)?;
        let mut paths = HashMap::new();
        // Backfilling identity for every session of a source is global
        // maintenance; a scoped run only writes identity for the sessions it
        // actually processes.
        let backfill_identity = matches!(self.options.scope, ProjectScope::Global);
        for mut path in self.store.session_paths_for_source(source_id)? {
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
            paths.insert(path.source_id.clone(), path);
        }
        let imported_ids = self.store.imported_source_ids(source_id)?;
        let usage_meta = self.store.usage_state_meta_map(source_id)?;
        let event_meta = if self.options.usage_only && !self.options.backfill_events {
            Default::default()
        } else {
            self.store.event_state_meta_map(source_id)?
        };
        let metadata_meta = if self.options.usage_only {
            Default::default()
        } else {
            self.store.metadata_state_meta_map(source_id)?
        };
        Ok(ExistingState { meta, paths, imported_ids, usage_meta, event_meta, metadata_meta })
    }

    fn process_raw_session(
        &mut self,
        source_id: &str,
        raw: adapters::RawSession,
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
            if existing.remove(&raw_source_id) {
                self.store.delete_session_data(source_id, &raw_source_id)?;
            }
            if purged_excluded_ids.insert(raw_source_id) {
                self.stats.excluded_out += 1;
            }
            return Ok(());
        }

        // Path evidence drives exclusions, so backfill it after scope/exclusion
        // checks even when content falls outside the configured sync window.
        if let Some(source_file_path) = raw.source_file_path.as_deref()
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
        let usage_backfill_needed = raw.usage_parser_version.is_some_and(|version| {
            !crate::adapters::sync_state::usage_state_is_current(
                version,
                existing.usage_meta.get(&raw_source_id).copied(),
                raw.updated_at,
            )
        });
        let event_backfill_needed = (self.options.backfill_events || !self.options.usage_only)
            && raw.event_parser_version.is_some_and(|version| {
                !crate::adapters::sync_state::event_state_is_current(
                    version,
                    existing.event_meta.get(&raw_source_id).copied(),
                    raw.updated_at,
                )
            });
        let metadata_parser_version = raw.metadata_parser_version;
        let metadata_backfill_needed = !self.options.usage_only
            && metadata_parser_version.is_some_and(|version| {
                !crate::adapters::sync_state::metadata_state_is_current(
                    version,
                    existing.metadata_meta.get(&raw_source_id).copied(),
                    raw.updated_at,
                )
            });

        match existing.meta.get(&raw_source_id).copied() {
            Some((old_updated_at, old_msg_count)) => {
                let was_imported = existing.imported_ids.remove(&raw_source_id);
                let metadata_changed = existing.paths.get(&raw_source_id).is_some_and(|old| {
                    raw_session_metadata_changed(&raw, repo_identity.as_ref(), old)
                });
                let content_changed = old_msg_count != msg_count
                    || metadata_changed
                    || (raw.updated_at.is_some() && raw.updated_at != old_updated_at);
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
                        self.stats.skipped += 1;
                        return Ok(());
                    }
                    ExistingSessionAction::BackfillOnly(plan) => {
                        self.apply_backfill(
                            source_id,
                            &raw_source_id,
                            &raw,
                            plan,
                            was_imported,
                            existing,
                        )?;
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
            }
            None => {
                self.stats.new_sessions += 1;
            }
        }

        let session_uuid = uuid::Uuid::new_v4().to_string();
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
        raw: &adapters::RawSession,
        plan: BackfillPlan,
        was_imported: bool,
        existing: &mut ExistingState,
    ) -> Result<()> {
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
                UsageSessionStateMeta { parser_version, source_updated_at: raw.updated_at },
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
                EventSessionStateMeta { parser_version, source_updated_at: raw.updated_at },
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
                    MetadataSessionStateMeta { parser_version, source_updated_at: raw.updated_at },
                );
                reprocessed = true;
            }
        }
        if raw.custom_title.is_some() || raw.summary.is_some() || raw.duration_minutes.is_some() {
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
        let runs: Vec<&AdapterRun> = self.adapter_runs.iter().collect();
        if runs.is_empty() {
            return;
        }

        println!();
        println!(
            "{:<6} {:>10} {:>14} {:>8} {:>8} {:>9} {:>8}",
            "Source", "candidates", "pre-parse rej", "parsed", "scoped", "touched", "ms"
        );
        for run in &runs {
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
        let total = |f: fn(&AdapterRun) -> u32| runs.iter().map(|run| f(run)).sum::<u32>();
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
            if filtered_out > 0 {
                print!(", {filtered_out} outside configured time scope");
            }
            if out_of_scope > 0 {
                print!(", {out_of_scope} outside project scope");
            }
            if excluded_out > 0 {
                print!(", {excluded_out} excluded by excluded_paths");
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
            if self.options.force {
                println!("Reprocessed {touched} sessions, {total_messages} messages");
            } else if touched == 0 {
                println!("Up to date.");
            } else {
                println!(
                    "{new_sessions} new, {updated_sessions} updated, {total_messages} messages"
                );
            }
        }

        Ok(())
    }
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
    if usage_only {
        let needs_usage = usage_backfill_needed;
        let needs_events = backfill_events && event_backfill_needed;
        return if needs_usage || needs_events {
            ExistingSessionAction::BackfillOnly(BackfillPlan {
                usage: needs_usage,
                events: needs_events,
                metadata: false,
            })
        } else {
            ExistingSessionAction::Skip
        };
    }

    if !content_changed && !force {
        return if usage_backfill_needed || event_backfill_needed || metadata_backfill_needed {
            ExistingSessionAction::BackfillOnly(BackfillPlan {
                usage: usage_backfill_needed,
                events: event_backfill_needed,
                metadata: metadata_backfill_needed,
            })
        } else {
            ExistingSessionAction::Skip
        };
    }

    ExistingSessionAction::RefreshSession
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
mod tests {
    use std::collections::HashSet;

    use crate::adapters::{RawMessage, RawSession, ResumeCommand, SourceAdapter};
    use crate::config::AppConfig;
    use crate::db::{
        schema,
        store::{SessionPath, Store},
    };
    use crate::project_scope::ProjectScope;
    use crate::types::{Role, Session};

    use super::{
        BackfillPlan, ExistingSessionAction, SyncJob, SyncRunOptions,
        decide_existing_session_action, delete_excluded_sessions_for_source,
        raw_session_metadata_changed,
    };

    struct StaticAdapter {
        updated_at: i64,
        messages: &'static [&'static str],
        source_file_path: Option<&'static str>,
        optimized: bool,
    }

    impl SourceAdapter for StaticAdapter {
        fn id(&self) -> &str {
            "test"
        }

        fn label(&self) -> &str {
            "Test"
        }

        fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
            let messages = self
                .messages
                .iter()
                .enumerate()
                .map(|(seq, content)| RawMessage {
                    role: Role::User,
                    content: (*content).to_string(),
                    timestamp: Some(self.updated_at + seq as i64),
                })
                .collect();
            let mut raw =
                RawSession::search_only("raw1", None, 1_000, Some(self.updated_at), None, messages);
            raw.source_file_path = self.source_file_path.map(str::to_string);
            Ok(vec![raw])
        }

        fn scan_for_sync(
            &self,
            _store: &Store,
            _since_ts: Option<i64>,
            _include_events: bool,
        ) -> anyhow::Result<Option<crate::adapters::SyncScanResult>> {
            if !self.optimized {
                return Ok(None);
            }
            Ok(Some(crate::adapters::SyncScanResult {
                sessions: self.scan()?,
                stats: Default::default(),
            }))
        }

        fn resume_command(&self, _source_id: &str) -> Option<ResumeCommand> {
            None
        }
    }

    fn matcher(pattern: &str) -> globset::GlobSet {
        let mut builder = globset::GlobSetBuilder::new();
        builder.add(globset::Glob::new(pattern).unwrap());
        builder.build().unwrap()
    }

    fn session(id: &str, source: &str, source_id: &str) -> Session {
        Session {
            id: id.to_string(),
            source: source.to_string(),
            source_id: source_id.to_string(),
            title: "t".to_string(),
            directory: None,
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
    fn usage_only_never_refreshes_existing_session() {
        assert_eq!(
            decide_existing_session_action(true, false, false, true, true, true, true),
            ExistingSessionAction::BackfillOnly(BackfillPlan {
                usage: true,
                events: false,
                metadata: false
            })
        );
        assert_eq!(
            decide_existing_session_action(true, false, false, true, false, true, true),
            ExistingSessionAction::Skip
        );
    }

    #[test]
    fn usage_only_can_backfill_events_without_refresh() {
        assert_eq!(
            decide_existing_session_action(true, true, false, true, false, true, false),
            ExistingSessionAction::BackfillOnly(BackfillPlan {
                usage: false,
                events: true,
                metadata: false
            })
        );
        assert_eq!(
            decide_existing_session_action(true, true, false, true, true, true, false),
            ExistingSessionAction::BackfillOnly(BackfillPlan {
                usage: true,
                events: true,
                metadata: false
            })
        );
    }

    #[test]
    fn full_sync_refreshes_changed_existing_session() {
        assert_eq!(
            decide_existing_session_action(false, false, false, true, true, true, false),
            ExistingSessionAction::RefreshSession
        );
    }

    #[test]
    fn full_sync_backfills_unchanged_existing_session_in_place() {
        assert_eq!(
            decide_existing_session_action(false, false, false, false, true, true, false),
            ExistingSessionAction::BackfillOnly(BackfillPlan {
                usage: true,
                events: true,
                metadata: false
            })
        );
        assert_eq!(
            decide_existing_session_action(false, false, false, false, false, false, false),
            ExistingSessionAction::Skip
        );
    }

    #[test]
    fn full_sync_backfills_metadata_only_when_topology_parser_advances() {
        assert_eq!(
            decide_existing_session_action(false, false, false, false, false, false, true),
            ExistingSessionAction::BackfillOnly(BackfillPlan {
                usage: false,
                events: false,
                metadata: true
            })
        );
        assert_eq!(
            decide_existing_session_action(true, false, false, false, false, false, true),
            ExistingSessionAction::Skip
        );
    }

    #[test]
    fn full_sync_treats_new_session_metadata_as_changed() {
        let raw = RawSession::search_only(
            "raw1",
            Some("/Users/x/git/samzong/Recall".to_string()),
            0,
            Some(1),
            None,
            vec![],
        );
        let missing = SessionPath {
            source_id: "raw1".to_string(),
            directory: None,
            source_file_path: None,
            repo_remote: None,
            repo_slug: None,
            repo_name: None,
        };
        let same = SessionPath {
            source_id: "raw1".to_string(),
            directory: Some("/Users/x/git/samzong/Recall".to_string()),
            source_file_path: None,
            repo_remote: Some("github.com/samzong/Recall".to_string()),
            repo_slug: None,
            repo_name: None,
        };
        assert!(raw_session_metadata_changed(&raw, None, &missing));
        assert!(!raw_session_metadata_changed(&raw, None, &same));

        let mut raw_with_path = RawSession::search_only("raw1", None, 0, Some(1), None, vec![]);
        raw_with_path.source_file_path = Some("/tmp/session.jsonl".to_string());
        assert!(raw_session_metadata_changed(&raw_with_path, None, &missing));
    }

    struct TwoProjectAdapter;

    impl SourceAdapter for TwoProjectAdapter {
        fn id(&self) -> &str {
            "test"
        }

        fn label(&self) -> &str {
            "Test"
        }

        fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
            let message = |content: &str| RawMessage {
                role: Role::User,
                content: content.to_string(),
                timestamp: Some(1_000),
            };
            Ok(vec![
                RawSession::search_only(
                    "inside",
                    Some("/repo/root/nested".to_string()),
                    1_000,
                    Some(2_000),
                    None,
                    vec![message("inside")],
                ),
                RawSession::search_only(
                    "outside",
                    Some("/elsewhere".to_string()),
                    1_000,
                    Some(2_000),
                    None,
                    vec![message("outside")],
                ),
            ])
        }

        fn resume_command(&self, _source_id: &str) -> Option<ResumeCommand> {
            None
        }
    }

    fn scoped_job(scope: ProjectScope) -> (SyncJob, Vec<Box<dyn SourceAdapter>>) {
        schema::register_sqlite_vec();
        let adapters: Vec<Box<dyn SourceAdapter>> = vec![Box::new(TwoProjectAdapter)];
        let job = SyncJob::new(
            SyncRunOptions {
                force: false,
                verbose: false,
                emit: false,
                usage_only: false,
                backfill_events: false,
                sources: None,
                scope,
            },
            Store::open_in_memory().unwrap(),
            AppConfig::default(),
            &adapters,
        )
        .unwrap();
        (job, adapters)
    }

    fn synced_source_ids(job: &SyncJob) -> Vec<String> {
        let mut ids = job
            .store
            .session_paths_for_source("test")
            .unwrap()
            .into_iter()
            .map(|path| path.source_id)
            .collect::<Vec<_>>();
        ids.sort();
        ids
    }

    #[test]
    fn scoped_sync_writes_only_sessions_inside_the_scope() {
        let (mut job, adapters) = scoped_job(ProjectScope::Directory("/repo/root".to_string()));

        job.run(&adapters).unwrap();

        assert_eq!(synced_source_ids(&job), vec!["inside".to_string()]);
        assert_eq!(job.stats.out_of_scope, 1);
    }

    #[test]
    fn scoped_sync_leaves_sessions_outside_the_scope_untouched() {
        let (mut job, adapters) = scoped_job(ProjectScope::Directory("/repo/root".to_string()));
        let mut existing = session("s-outside", "test", "outside");
        existing.directory = Some("/elsewhere".to_string());
        existing.message_count = 7;
        job.store.insert_session(&existing).unwrap();

        job.run(&adapters).unwrap();

        assert_eq!(job.store.session_meta("test", "outside").unwrap(), Some((Some(1), 7)));
    }

    #[test]
    fn repository_scope_falls_back_to_local_root_when_identity_is_unknown() {
        let (mut job, adapters) = scoped_job(ProjectScope::Repository {
            filter: crate::db::search::RepoFilter::Remote("github.com/samzong/Recall".to_string()),
            local_root: Some("/repo/root".to_string()),
        });

        job.run(&adapters).unwrap();

        assert_eq!(synced_source_ids(&job), vec!["inside".to_string()]);
    }

    #[test]
    fn sync_job_refreshes_changed_session_through_adapter_seam() {
        schema::register_sqlite_vec();
        let initial: Vec<Box<dyn SourceAdapter>> = vec![Box::new(StaticAdapter {
            updated_at: 2_000,
            messages: &["first"],
            source_file_path: None,
            optimized: false,
        })];
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
            Store::open_in_memory().unwrap(),
            AppConfig::default(),
            &initial,
        )
        .unwrap();

        job.run(&initial).unwrap();
        assert_eq!(job.store.session_meta("test", "raw1").unwrap(), Some((Some(2_000), 1)));

        let updated: Vec<Box<dyn SourceAdapter>> = vec![Box::new(StaticAdapter {
            updated_at: 3_000,
            messages: &["first", "second"],
            source_file_path: None,
            optimized: false,
        })];
        job.run(&updated).unwrap();

        assert_eq!(job.store.session_meta("test", "raw1").unwrap(), Some((Some(3_000), 2)));
        let session = job.store.list_recent_sessions(1).unwrap().pop().unwrap();
        let messages = job.store.get_messages(&session.id).unwrap();
        assert_eq!(
            messages.iter().map(|message| message.content.as_str()).collect::<Vec<_>>(),
            ["first", "second"]
        );
    }

    #[test]
    fn source_path_backfill_runs_after_scope_and_before_time_filter() {
        schema::register_sqlite_vec();
        let adapters: Vec<Box<dyn SourceAdapter>> = vec![Box::new(StaticAdapter {
            updated_at: 2_000,
            messages: &[],
            source_file_path: Some("/tmp/session.jsonl"),
            optimized: true,
        })];
        let mut config = AppConfig::default();
        config.sync_window = crate::config::SyncWindow::Today;
        let mut global_job = SyncJob::new(
            SyncRunOptions {
                force: false,
                verbose: false,
                emit: false,
                usage_only: false,
                backfill_events: false,
                sources: None,
                scope: ProjectScope::Global,
            },
            Store::open_in_memory().unwrap(),
            config,
            &adapters,
        )
        .unwrap();
        global_job.store.insert_session(&session("global", "test", "raw1")).unwrap();

        global_job.run(&adapters).unwrap();

        assert_eq!(
            global_job.store.session_paths_for_source("test").unwrap()[0]
                .source_file_path
                .as_deref(),
            Some("/tmp/session.jsonl")
        );

        let mut scoped_job = SyncJob::new(
            SyncRunOptions {
                force: false,
                verbose: false,
                emit: false,
                usage_only: false,
                backfill_events: false,
                sources: None,
                scope: ProjectScope::Directory("/repo/root".to_string()),
            },
            Store::open_in_memory().unwrap(),
            AppConfig::default(),
            &adapters,
        )
        .unwrap();
        scoped_job.store.insert_session(&session("scoped", "test", "raw1")).unwrap();

        scoped_job.run(&adapters).unwrap();

        assert_eq!(
            scoped_job.store.session_paths_for_source("test").unwrap()[0].source_file_path,
            None
        );
    }

    #[test]
    fn delete_excluded_sessions_for_source_uses_persisted_source_file_path() {
        schema::register_sqlite_vec();
        let matcher = matcher("**/observer-sessions");
        let store = Store::open_in_memory().unwrap();
        store.insert_session(&session("id-1", "claude-code", "s1")).unwrap();
        store
            .update_session_fields(
                "claude-code",
                "s1",
                None,
                None,
                None,
                Some("/tmp/observer-sessions/session.jsonl"),
            )
            .unwrap();

        let mut deleted = HashSet::new();
        let count = delete_excluded_sessions_for_source(
            &store,
            "claude-code",
            &matcher,
            &ProjectScope::Global,
            &mut deleted,
        )
        .unwrap();

        assert_eq!(count, 1);
        assert!(deleted.contains("s1"));
        assert!(store.session_paths_for_source("claude-code").unwrap().is_empty());
    }

    #[test]
    fn excluded_source_file_path_blocks_fresh_and_force_sync() {
        for force in [false, true] {
            schema::register_sqlite_vec();
            let adapters: Vec<Box<dyn SourceAdapter>> = vec![Box::new(StaticAdapter {
                updated_at: 2_000,
                messages: &[],
                source_file_path: Some("/tmp/private-sessions/session.jsonl"),
                optimized: true,
            })];
            let mut config = AppConfig::default();
            config.excluded_paths = vec!["**/private-sessions".to_string()];
            let mut job = SyncJob::new(
                SyncRunOptions {
                    force,
                    verbose: false,
                    emit: false,
                    usage_only: false,
                    backfill_events: false,
                    sources: None,
                    scope: ProjectScope::Global,
                },
                Store::open_in_memory().unwrap(),
                config,
                &adapters,
            )
            .unwrap();

            job.run(&adapters).unwrap();

            assert!(job.store.session_paths_for_source("test").unwrap().is_empty());
            assert_eq!(job.stats.excluded_out, 1);
        }
    }
}
