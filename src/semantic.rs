use std::process::{Command, Stdio};

use anyhow::Result;

use crate::config::AppConfig;
use crate::db::store::Store;
use crate::embedding::EmbeddingProvider;
use crate::utils::try_acquire_worker_lock;

const SESSION_EMBED_BATCH: usize = 8;
const BACKGROUND_JOB: &str = "pipeline";

pub(crate) fn ensure_background_worker(sync_first: bool) -> Result<()> {
    if !sync_first {
        let store = Store::open_at(&Store::default_db_path()?)?;
        if !store.trigram_backfill_pending()? {
            if !AppConfig::load()?.semantic_enabled {
                return Ok(());
            }
            let progress = store.semantic_progress()?;
            if progress.pending_sessions + progress.processing_sessions == 0 {
                return Ok(());
            }
        }
    }
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.arg("__background-worker");
    if sync_first {
        cmd.arg("--sync-first");
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let _ = cmd.spawn()?;
    Ok(())
}

pub(crate) fn run_background_worker<F>(sync_first: bool, mut sync_fn: F) -> Result<()>
where
    F: FnMut() -> Result<()>,
{
    let Some(_lock) = try_acquire_worker_lock()? else {
        return Ok(());
    };

    let store = Store::open_at(&Store::default_db_path()?)?;

    loop {
        let pending = {
            let _sync_lock = crate::utils::acquire_sync_lock()?;
            store.backfill_trigram_batch()?
        };
        if !pending {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    {
        let _sync_lock = crate::utils::acquire_sync_lock()?;
        store.finish_trigram_backfill()?;
    }

    if sync_first {
        store.set_background_job_state(BACKGROUND_JOB, "sync", Some("Incremental sync"))?;
        if let Err(err) = sync_fn() {
            let message = format!("Sync failed: {err:#}");
            store.set_background_job_state(BACKGROUND_JOB, "error", Some(&message))?;
            return Err(err);
        }
    }

    if !AppConfig::load()?.semantic_enabled {
        store.clear_background_job_state(BACKGROUND_JOB)?;
        return Ok(());
    }

    store.requeue_processing_jobs()?;

    let provider = match EmbeddingProvider::new(false) {
        Ok(provider) => provider,
        Err(err) => {
            let message = format!("Semantic unavailable: {err:#}");
            store.set_background_job_state(BACKGROUND_JOB, "error", Some(&message))?;
            return Err(err);
        }
    };
    store.set_background_job_state(
        BACKGROUND_JOB,
        "semantic",
        Some(&format!("starting on {}", provider.device_name())),
    )?;

    loop {
        if !AppConfig::load()?.semantic_enabled {
            break;
        }
        if !process_next_session(&store, &provider)? {
            break;
        }
    }

    store.clear_background_job_state(BACKGROUND_JOB)?;
    Ok(())
}

fn process_next_session(store: &Store, provider: &EmbeddingProvider) -> Result<bool> {
    let Some(job) = store.claim_next_session_embedding_job()? else {
        return Ok(false);
    };

    match process_session(store, provider, &job) {
        Ok(()) => {
            store.complete_session_embedding(&job.session_id)?;
            Ok(true)
        }
        Err(err) => {
            let message = format!("{err:#}");
            store.fail_session_embedding(&job.session_id, &message)?;
            store.set_background_job_state(BACKGROUND_JOB, "error", Some(&message))?;
            Err(err)
        }
    }
}

fn process_session(
    store: &Store,
    provider: &EmbeddingProvider,
    job: &crate::types::SemanticSessionJob,
) -> Result<()> {
    let pending = store.pending_embeddable_messages(&job.session_id)?;
    let mut units_done = store.embedded_message_count(&job.session_id)?;
    if pending.is_empty() {
        return Ok(());
    }

    let device = provider.device_name();
    for chunk in pending.chunks(SESSION_EMBED_BATCH) {
        let texts: Vec<String> =
            chunk.iter().map(|(_, content)| build_embedding_text(&job.title, content)).collect();
        let embeddings = provider.embed_documents(&texts)?;
        let items: Vec<(i64, &[f32])> = chunk
            .iter()
            .zip(embeddings.iter())
            .map(|((message_id, _), embedding)| (*message_id, embedding.as_slice()))
            .collect();
        store.upsert_embeddings(&items)?;
        units_done += chunk.len() as u64;
        store.update_session_embedding_progress(&job.session_id, units_done)?;
        let detail = format!("{} ({}/{}) • {device}", job.title, units_done, job.units_total);
        store.set_background_job_state(BACKGROUND_JOB, "semantic", Some(&detail))?;
    }

    Ok(())
}

pub(crate) fn build_embedding_text(title: &str, content: &str) -> String {
    let text = format!("{title}: {content}");
    if text.chars().count() > 500 { text.chars().take(500).collect() } else { text }
}

#[derive(clap::Subcommand)]
pub(crate) enum SemanticCommands {
    #[command(about = "Enable semantic indexing and search, and start indexing queued sessions")]
    Enable,
    #[command(about = "Disable semantic indexing and search; full-text search keeps working")]
    Disable,
    #[command(about = "Show the semantic setting and indexing queue")]
    Status,
}

pub(crate) fn run_semantic_command(command: SemanticCommands) -> Result<()> {
    match command {
        SemanticCommands::Enable => {
            let mut config = AppConfig::load()?;
            config.semantic_enabled = true;
            config.save()?;
            let pending = Store::open()
                .and_then(|store| store.semantic_progress())
                .map(|progress| progress.pending_sessions + progress.processing_sessions)
                .unwrap_or(0);
            println!("Semantic search enabled.");
            println!("{pending} sessions queued; indexing runs in the background.");
            ensure_background_worker(false)?;
        }
        SemanticCommands::Disable => {
            let mut config = AppConfig::load()?;
            config.semantic_enabled = false;
            config.save()?;
            println!("Semantic search disabled; sync and full-text search keep working.");
            println!(
                "A running indexer stops after its current session; queued sessions backfill when re-enabled."
            );
        }
        SemanticCommands::Status => {
            let config = AppConfig::load()?;
            let progress =
                Store::open().and_then(|store| store.semantic_progress()).unwrap_or_default();
            if config.semantic_enabled {
                println!("Semantic search: enabled");
            } else {
                println!("Semantic search: disabled (full-text search only)");
            }
            println!(
                "Index queue: {} done, {} pending, {} failed",
                progress.done_sessions,
                progress.pending_sessions + progress.processing_sessions,
                progress.failed_sessions
            );
        }
    }
    Ok(())
}
