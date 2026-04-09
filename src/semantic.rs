use std::fs::{File, OpenOptions};
use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::Result;
use fs2::FileExt;

use crate::db::store::Store;
use crate::embedding::EmbeddingProvider;

const SESSION_EMBED_BATCH: usize = 64;
const BACKGROUND_JOB: &str = "pipeline";

pub struct SemanticRunOutcome {
    pub title: String,
    pub units_done: u64,
    pub units_total: u64,
}

pub fn ensure_background_worker(sync_first: bool) -> Result<()> {
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

pub fn run_background_worker<F>(sync_first: bool, mut sync_fn: F) -> Result<()>
where
    F: FnMut() -> Result<()>,
{
    let Some(_lock) = try_acquire_worker_lock()? else {
        return Ok(());
    };

    let store = Store::open()?;

    if sync_first {
        store.set_background_job_state(BACKGROUND_JOB, "sync", Some("Incremental sync"))?;
        if let Err(err) = sync_fn() {
            let message = format!("Sync failed: {err:#}");
            store.set_background_job_state(BACKGROUND_JOB, "error", Some(&message))?;
            return Err(err);
        }
    }

    let provider = match EmbeddingProvider::new(false) {
        Ok(provider) => provider,
        Err(err) => {
            let message = format!("Semantic unavailable: {err:#}");
            store.set_background_job_state(BACKGROUND_JOB, "error", Some(&message))?;
            return Err(err);
        }
    };

    while let Some(outcome) = process_next_session(&store, &provider)? {
        let detail = format!("{} ({}/{})", outcome.title, outcome.units_done, outcome.units_total);
        store.set_background_job_state(BACKGROUND_JOB, "semantic", Some(&detail))?;
    }

    store.clear_background_job_state(BACKGROUND_JOB)?;
    Ok(())
}

fn process_next_session(
    store: &Store,
    provider: &EmbeddingProvider,
) -> Result<Option<SemanticRunOutcome>> {
    let Some(job) = store.claim_next_session_embedding_job()? else {
        return Ok(None);
    };

    store.set_background_job_state(BACKGROUND_JOB, "semantic", Some(&job.title))?;
    let result = process_session(store, provider, &job);
    match result {
        Ok(outcome) => {
            store.complete_session_embedding(&job.session_id)?;
            Ok(Some(outcome))
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
) -> Result<SemanticRunOutcome> {
    let pending = store.pending_embeddable_messages(&job.session_id)?;
    if pending.is_empty() {
        let units_done = store.embedded_message_count(&job.session_id)?;
        return Ok(SemanticRunOutcome {
            title: job.title.clone(),
            units_done,
            units_total: job.units_total,
        });
    }

    let mut units_done = store.embedded_message_count(&job.session_id)?;

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
        let detail = format!("{} ({}/{})", job.title, units_done, job.units_total);
        store.set_background_job_state(BACKGROUND_JOB, "semantic", Some(&detail))?;
    }

    Ok(SemanticRunOutcome { title: job.title.clone(), units_done, units_total: job.units_total })
}

fn build_embedding_text(title: &str, content: &str) -> String {
    let text = format!("{title}: {content}");
    if text.chars().count() > 500 { text.chars().take(500).collect() } else { text }
}

fn try_acquire_worker_lock() -> Result<Option<File>> {
    let path = worker_lock_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file =
        OpenOptions::new().create(true).truncate(false).read(true).write(true).open(path)?;
    match file.try_lock_exclusive() {
        Ok(()) => {
            file.set_len(0)?;
            writeln!(file, "{}", std::process::id())?;
            Ok(Some(file))
        }
        Err(_) => Ok(None),
    }
}

fn worker_lock_path() -> Result<std::path::PathBuf> {
    let dir = dirs::data_dir().ok_or_else(|| anyhow::anyhow!("cannot determine data directory"))?;
    Ok(dir.join("recall").join("background-worker.lock"))
}
