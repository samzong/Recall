use std::fs::{File, OpenOptions};
use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::Result;
use fs2::FileExt;

use crate::db::store::Store;
use crate::embedding::EmbeddingProvider;

const SESSION_EMBED_BATCH: usize = 8;
const BACKGROUND_JOB: &str = "pipeline";

pub(crate) fn ensure_background_worker(sync_first: bool) -> Result<()> {
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
    store.set_background_job_state(
        BACKGROUND_JOB,
        "semantic",
        Some(&format!("starting on {}", provider.device_name())),
    )?;

    while process_next_session(&store, &provider)? {}

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

pub(crate) fn worker_lock_path() -> Result<std::path::PathBuf> {
    let dir = dirs::data_dir().ok_or_else(|| anyhow::anyhow!("cannot determine data directory"))?;
    Ok(dir.join("recall").join("background-worker.lock"))
}

pub(crate) fn worker_lock_pid() -> Result<Option<u32>> {
    lock_pid_at(&worker_lock_path()?)
}

pub(crate) fn worker_lock_is_held() -> Result<bool> {
    lock_is_held_at(&worker_lock_path()?)
}

fn lock_pid_at(path: &std::path::Path) -> Result<Option<u32>> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    Ok(contents.lines().next().and_then(|line| line.trim().parse::<u32>().ok()))
}

fn lock_is_held_at(path: &std::path::Path) -> Result<bool> {
    let file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err.into()),
    };
    // A live worker holds an exclusive flock on this file for its lifetime.
    // If we can take the lock, no live holder exists (stale/absent pidfile).
    // `drop(file)` releases our probe lock. Any lock error means contended -> held.
    match file.try_lock_exclusive() {
        Ok(()) => Ok(false),
        Err(_) => Ok(true),
    }
}

#[cfg(test)]
mod worker_lock_tests {
    use std::fs::OpenOptions;

    use fs2::FileExt;

    use super::{lock_is_held_at, lock_pid_at};

    #[test]
    fn test_should_report_not_held_when_lock_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("background-worker.lock");
        assert!(!lock_is_held_at(&path).unwrap());
    }

    #[test]
    fn test_should_report_not_held_when_file_exists_but_unlocked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("background-worker.lock");
        std::fs::write(&path, "4242\n").unwrap();
        assert!(!lock_is_held_at(&path).unwrap());
    }

    #[test]
    fn test_should_report_held_while_exclusive_flock_is_active() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("background-worker.lock");
        std::fs::write(&path, "4242\n").unwrap();

        let holder = OpenOptions::new().read(true).write(true).open(&path).unwrap();
        holder.try_lock_exclusive().unwrap();
        assert!(lock_is_held_at(&path).unwrap());

        drop(holder);
        assert!(!lock_is_held_at(&path).unwrap());
    }

    #[test]
    fn test_should_read_pid_from_first_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("background-worker.lock");
        std::fs::write(&path, "12345\n").unwrap();
        assert_eq!(lock_pid_at(&path).unwrap(), Some(12345));
    }

    #[test]
    fn test_should_return_none_pid_when_missing_empty_or_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.lock");
        assert_eq!(lock_pid_at(&missing).unwrap(), None);

        let empty = dir.path().join("empty.lock");
        std::fs::write(&empty, "").unwrap();
        assert_eq!(lock_pid_at(&empty).unwrap(), None);

        let garbage = dir.path().join("garbage.lock");
        std::fs::write(&garbage, "not-a-pid\n").unwrap();
        assert_eq!(lock_pid_at(&garbage).unwrap(), None);
    }
}
