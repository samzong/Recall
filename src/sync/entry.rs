use super::{EventBackfillReport, SyncJob, SyncRunOptions};
use crate::adapters;
use crate::config::AppConfig;
use crate::db::store::Store;
use crate::project_scope::ProjectScope;
use crate::query::resolve_source_filter;
use crate::sync_progress::{format_bytes, format_elapsed};
use crate::{semantic, utils};
use anyhow::Result;

pub(crate) fn run_cli(
    force: bool,
    verbose: bool,
    source_filter: Option<&str>,
    project_filter: Option<&str>,
    backfill_events: bool,
    dry_run: bool,
) -> Result<()> {
    if backfill_events {
        let run = || {
            let store = if dry_run {
                Store::open_event_preview_at(&Store::default_db_path()?)?
            } else {
                Store::open()?
            };
            let labels = adapters::source_labels();
            let sources = resolve_source_filter(source_filter, &labels)?;
            let scope = store.resolve_scope(project_filter, None)?.announce();
            let available = adapters::all_adapters();
            let mut job = SyncJob::new(
                SyncRunOptions {
                    force,
                    verbose,
                    emit: true,
                    usage_only: true,
                    backfill_events: true,
                    sources,
                    scope,
                    target_session: None,
                },
                store,
                AppConfig::load()?,
                &available,
            )?;
            job.event_backfill = Some(EventBackfillReport { dry_run, ..Default::default() });
            job.run_with(&available, None)
        };
        return if dry_run { run() } else { run_with_sync_lock(run) };
    }
    run_with_sync_lock(|| {
        let labels = adapters::source_labels();
        let sources = resolve_source_filter(source_filter, &labels)?;
        let scope = Store::open()?.resolve_scope(project_filter, None)?.announce();
        run_sync_job_with(
            SyncRunOptions {
                force,
                verbose,
                emit: true,
                usage_only: false,
                backfill_events: false,
                sources,
                scope,
                target_session: None,
            },
            None,
        )?;
        compact_database_if_bloated()
    })?;
    semantic::ensure_background_worker(false)?;
    Ok(())
}

fn compact_database_if_bloated() -> Result<()> {
    let store = Store::open()?;
    let Some(plan) = store.compaction_plan()? else {
        return Ok(());
    };
    let db_path = Store::default_db_path()?;
    let available = fs2::available_space(db_path.parent().unwrap_or(&db_path))?;
    if available < plan.required_disk_bytes {
        eprintln!(
            "Compaction skipped: reclaiming {} needs {} of free disk, {} available.",
            format_bytes(plan.reclaimable_bytes),
            format_bytes(plan.required_disk_bytes),
            format_bytes(available)
        );
        return Ok(());
    }
    eprintln!(
        "Compacting database to reclaim {} (one-time)...",
        format_bytes(plan.reclaimable_bytes)
    );
    let started = std::time::Instant::now();
    match store.vacuum() {
        Ok(()) => {
            eprintln!("Database compacted in {}.", format_elapsed(started.elapsed().as_millis()))
        }
        Err(err) => eprintln!("Compaction skipped ({err}); it will be retried on the next sync."),
    }
    Ok(())
}

fn usage_sync_options() -> SyncRunOptions {
    SyncRunOptions {
        force: false,
        verbose: false,
        emit: false,
        usage_only: true,
        backfill_events: false,
        sources: None,
        scope: ProjectScope::Global,
        target_session: None,
    }
}

pub(crate) fn run_usage_sync_job() -> Result<()> {
    run_sync_job_inner(usage_sync_options())
}

pub(crate) fn run_usage_sync_job_with_progress(on_source: &mut dyn FnMut(&str)) -> Result<()> {
    run_with_sync_lock(|| run_sync_job_with(usage_sync_options(), Some(on_source)))
}

pub(crate) fn run_dashboard_sync_job() -> Result<()> {
    run_sync_job_inner(SyncRunOptions { backfill_events: true, ..usage_sync_options() })
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
            target_session: None,
        })
    })
}

pub(crate) fn run_sync_job_inner(options: SyncRunOptions) -> Result<()> {
    run_with_sync_lock(|| run_sync_job_with(options, None))
}

pub(crate) fn scan_remote_scope(scope: ProjectScope) -> Result<()> {
    run_sync_job_with(
        SyncRunOptions {
            force: false,
            verbose: false,
            emit: false,
            usage_only: false,
            backfill_events: false,
            sources: None,
            scope,
            target_session: None,
        },
        None,
    )
}

fn run_with_sync_lock<T>(run: impl FnOnce() -> Result<T>) -> Result<T> {
    let _lock = utils::acquire_sync_lock()?;
    run()
}

fn run_sync_job_with(
    options: SyncRunOptions,
    on_source: Option<&mut dyn FnMut(&str)>,
) -> Result<()> {
    let available_adapters = adapters::all_adapters();
    let config = AppConfig::load()?;
    let mut job = SyncJob::new(options, Store::open()?, config, &available_adapters)?;
    job.host = crate::remote::load_settings()?.map(|settings| settings.host);
    job.run_with(&available_adapters, on_source)
}
