use anyhow::{Result, bail};

use super::{SyncJob, SyncRunOptions};
use crate::adapters;
use crate::config::AppConfig;
use crate::db::store::Store;
use crate::project_scope::ProjectScope;
use crate::query::resolve_source_id;
use crate::utils;

#[derive(Clone, Copy, PartialEq, Eq, Debug, clap::ValueEnum)]
pub(crate) enum SessionSyncFormat {
    Text,
    Json,
}

pub(crate) fn run(source: Option<&str>, native_id: &str, format: SessionSyncFormat) -> Result<()> {
    let native_id = native_id.trim();
    if native_id.is_empty() {
        bail!("--session needs a native session id");
    }
    let config = AppConfig::load()?;
    let source = resolve_source_id(
        source.ok_or_else(|| anyhow::anyhow!("--session requires --source"))?,
        &adapters::source_labels(),
    )?;
    anyhow::ensure!(config.is_source_enabled(&source), "{source} is disabled");

    let Some(_lock) = utils::try_acquire_sync_lock()? else {
        bail!("another sync holds the index lock; retry after it finishes");
    };

    let available = adapters::all_adapters();
    let mut job = SyncJob::new(
        SyncRunOptions {
            force: false,
            verbose: false,
            emit: false,
            usage_only: false,
            backfill_events: false,
            sources: Some(vec![source.clone()]),
            scope: ProjectScope::Global,
            target_session: Some(native_id.to_string()),
        },
        Store::open()?,
        config,
        &available,
    )?;
    job.host = crate::remote::load_settings()?.map(|settings| settings.host);

    job.run_with(&available, None)?;

    if job.stats.excluded_out > 0 {
        bail!(
            "{source} session {native_id} matches excluded_paths and is not indexed; any existing index entry for it was removed"
        );
    }
    let Some(session) = job.store.get_native_session(&source, native_id)? else {
        bail!("{source} session {native_id} could not be indexed; the index is unchanged");
    };
    let status = if job.stats.new_sessions > 0 {
        "created"
    } else if job.stats.updated_sessions + job.stats.reprocessed_sessions > 0 {
        "updated"
    } else {
        "unchanged"
    };

    match format {
        SessionSyncFormat::Json => println!(
            "{}",
            serde_json::json!({
                "protocol_version": crate::PROTOCOL_VERSION,
                "source": source,
                "source_session_id": native_id,
                "session_id": session.id,
                "message_count": session.message_count,
                "status": status,
            })
        ),
        SessionSyncFormat::Text => println!(
            "{source} {native_id} -> {} ({status}, {} messages)",
            session.id, session.message_count
        ),
    }
    Ok(())
}
