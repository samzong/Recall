use std::io::{IsTerminal, Write};

use anyhow::Result;

use crate::db::store::Store;
use crate::semantic;

pub(crate) fn run_reset(yes: bool) -> Result<()> {
    if !yes {
        if !std::io::stdin().is_terminal() {
            anyhow::bail!("reset requires --yes when not attached to a terminal");
        }
        print!("Delete ALL indexed Recall data? This cannot be undone [y/N]: ");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y") {
            println!("Aborted.");
            return Ok(());
        }
    }
    Store::open()?.reset_all_data()?;
    println!("All indexed data cleared.");
    Ok(())
}

pub(crate) fn run_vacuum() -> Result<()> {
    let path = Store::db_path()?;
    let before = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    Store::open()?.vacuum()?;
    let after = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    println!(
        "Vacuumed {} -> {} ({} reclaimed)",
        crate::utils::humanize_bytes(before),
        crate::utils::humanize_bytes(after),
        crate::utils::humanize_bytes(before.saturating_sub(after)),
    );
    Ok(())
}

pub(crate) fn run_reembed(yes: bool) -> Result<()> {
    if !yes {
        if !std::io::stdin().is_terminal() {
            anyhow::bail!("reembed requires --yes when not attached to a terminal");
        }
        print!("Rebuild all semantic embeddings? Type 'reembed' to confirm: ");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if answer.trim() != "reembed" {
            println!("Aborted.");
            return Ok(());
        }
    }
    let cleared = Store::open()?.clear_semantic_queue()?;
    println!(
        "Cleared embeddings for {cleared} session(s). FTS search stays available; \
         the background worker rebuilds vectors on the next `recall` or `recall sync`."
    );
    Ok(())
}

pub(crate) fn run_worker_status() -> Result<()> {
    let held = semantic::worker_lock_is_held()?;
    let pid = semantic::worker_lock_pid()?;
    let store = Store::open()?;
    let progress = store.semantic_progress().unwrap_or_default();
    let worker = store.background_job_status("pipeline").unwrap_or_default();

    println!("Background Worker");
    match (held, pid) {
        (true, Some(pid)) => println!("  State       running (pid {pid})"),
        (true, None) => println!("  State       running (pid unknown)"),
        (false, _) => println!("  State       not running"),
    }
    if let Some(phase) = worker.phase {
        if held {
            println!("  Phase       {phase}");
        } else {
            println!("  Phase       {phase} (stale)");
        }
    }
    println!(
        "  Progress    {} done, {} pending, {} failed",
        progress.done_sessions,
        progress.pending_sessions + progress.processing_sessions,
        progress.failed_sessions
    );
    Ok(())
}

pub(crate) fn run_worker_stop(clear_queue: bool) -> Result<()> {
    let mut worker_was_running = false;
    if semantic::worker_lock_is_held()? {
        worker_was_running = true;
        match semantic::worker_lock_pid()? {
            Some(pid) => {
                // Re-verify the lock is still held immediately before signaling
                // to narrow the pid-reuse TOCTOU window (see signal_worker).
                if semantic::worker_lock_is_held()? {
                    signal_worker(pid)?;
                    if wait_for_worker_exit()? {
                        println!("Stopped background worker (pid {pid}).");
                    } else {
                        println!(
                            "Signaled worker (pid {pid}) but the lock is still held after 5s; \
                             proceeding."
                        );
                    }
                }
            }
            None => {
                println!("Worker lock is held but no pid is recorded; leaving it in place.");
            }
        }
    } else {
        println!("No background worker is running.");
        remove_stale_lock()?;
    }

    // Only reset/clear queue state once the worker is confirmed not running:
    // it was never running (worker_was_running false), or a fresh lock probe
    // now reports not held. A timed-out wait or a held-but-no-pid worker both
    // still hold the lock here, so the probe correctly keeps the block gated.
    if worker_was_running && semantic::worker_lock_is_held()? {
        println!(
            "Worker still running; queue not modified. Re-run `recall worker stop` after it exits."
        );
        return Ok(());
    }

    if clear_queue {
        let cleared = Store::open()?.clear_semantic_queue()?;
        println!(
            "Cleared embeddings for {cleared} session(s); the background worker rebuilds \
             vectors on the next `recall` or `recall sync`."
        );
    } else {
        let requeued = Store::open()?.reset_inflight_embedding_jobs()?;
        if requeued > 0 {
            println!("Re-queued {requeued} in-flight session(s) as pending.");
        }
    }
    Ok(())
}

fn remove_stale_lock() -> Result<()> {
    let path = semantic::worker_lock_path()?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

fn wait_for_worker_exit() -> Result<bool> {
    for _ in 0..50 {
        if !semantic::worker_lock_is_held()? {
            return Ok(true);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Ok(false)
}

#[cfg(unix)]
fn signal_worker(pid: u32) -> Result<()> {
    // Safest achievable stop with an fs2 flock + pidfile: caller has just
    // re-verified the lock is held. Residual pid-reuse window (holder dies and
    // the OS reuses `pid` between that check and this kill) cannot be closed
    // without a pidfd; documented as out of scope. Never signal self/pid 0;
    // treat ESRCH (already gone) as success.
    if pid == 0 || pid == std::process::id() {
        anyhow::bail!("refusing to signal invalid worker pid {pid}");
    }
    let result = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    if result == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(err.into())
}

#[cfg(not(unix))]
fn signal_worker(_pid: u32) -> Result<()> {
    anyhow::bail!("worker stop is only supported on Unix")
}
