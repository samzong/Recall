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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GcReason {
    Keep,
    DisabledSource,
    Orphan,
    Unverifiable,
}

/// Classify a single indexed session for garbage collection. Pure over the
/// injected `exists` probe so every branch is unit-testable without a real FS.
/// Orphan is only reported when the stored path's PARENT still exists; a missing
/// parent means the source root is unavailable, so the row is kept as
/// unverifiable rather than deleted (the offline-root guard).
fn classify(
    source_enabled: bool,
    source_file_path: Option<&str>,
    exists: &impl Fn(&std::path::Path) -> bool,
) -> GcReason {
    if !source_enabled {
        return GcReason::DisabledSource;
    }
    let Some(path_str) = source_file_path else {
        return GcReason::Unverifiable;
    };
    let path = std::path::Path::new(path_str);
    if exists(path) {
        return GcReason::Keep;
    }
    match path.parent() {
        Some(parent) if exists(parent) => GcReason::Orphan,
        _ => GcReason::Unverifiable,
    }
}

pub(crate) fn run_gc(dry_run: bool, yes: bool) -> Result<()> {
    let config = crate::config::AppConfig::load_or_default();
    let store = Store::open()?;
    let exists = |p: &std::path::Path| p.exists();

    // (source, source_id, reason) for every row in the DB.
    let mut plan: Vec<(String, String, GcReason)> = Vec::new();
    for source in store.distinct_session_sources()? {
        let enabled = config.is_source_enabled(&source);
        for sp in store.session_paths_for_source(&source)? {
            let reason = classify(enabled, sp.source_file_path.as_deref(), &exists);
            plan.push((source.clone(), sp.source_id, reason));
        }
    }

    let count = |want: GcReason| plan.iter().filter(|(_, _, r)| *r == want).count();
    let (keep, disabled, orphan, unverifiable) = (
        count(GcReason::Keep),
        count(GcReason::DisabledSource),
        count(GcReason::Orphan),
        count(GcReason::Unverifiable),
    );
    let deletable = disabled + orphan;

    println!("Garbage Collection");
    println!("  Keep            {keep}");
    println!("  Disabled source {disabled}");
    println!("  Orphan          {orphan}");
    println!("  Unverifiable    {unverifiable} (kept: NULL path or source root unavailable)");

    // Up to 10 sample lines per deletable reason.
    for (label, reason) in
        [("disabled source", GcReason::DisabledSource), ("orphan", GcReason::Orphan)]
    {
        let rows: Vec<&(String, String, GcReason)> =
            plan.iter().filter(|(_, _, r)| *r == reason).collect();
        if rows.is_empty() {
            continue;
        }
        println!("  Sample ({label}):");
        for (source, source_id, _) in rows.iter().take(10) {
            println!("    {source}  {source_id}");
        }
        if rows.len() > 10 {
            println!("    ... and {} more", rows.len() - 10);
        }
    }

    if deletable == 0 {
        println!("Nothing to collect.");
        return Ok(());
    }

    if dry_run {
        println!(
            "Dry run: {deletable} session(s) would be deleted. Re-run without --dry-run to apply."
        );
        return Ok(());
    }

    if !yes {
        if !std::io::stdin().is_terminal() {
            anyhow::bail!("gc requires --yes when not attached to a terminal");
        }
        print!("Delete {deletable} indexed session(s)? This cannot be undone [y/N]: ");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y") {
            println!("Aborted.");
            return Ok(());
        }
    }

    let outcome =
        delete_plan(&plan, |source, source_id| store.delete_session_data(source, source_id));

    if outcome.failed > 0 {
        println!(
            "Deleted {} of {deletable} session(s); {} failed.",
            outcome.deleted, outcome.failed
        );
        let first_error = outcome.first_error.expect("failed > 0 implies first_error is populated");
        return Err(first_error.context(format!(
            "gc: {} of {deletable} session delete(s) failed ({} succeeded)",
            outcome.failed, outcome.deleted
        )));
    }

    println!("Deleted {} session(s).", outcome.deleted);
    Ok(())
}

/// Outcome of running [`delete_plan`] over a classified gc plan.
struct GcDeleteOutcome {
    deleted: usize,
    failed: usize,
    first_error: Option<anyhow::Error>,
}

/// Delete every `DisabledSource`/`Orphan` row in `plan` via the injected `delete`
/// callback. Each row is deleted independently (the store commits per-row), so a
/// failure on one row must not abort the rest: it is recorded and the loop
/// continues, keeping partial cleanup instead of stopping silently mid-gc. Every
/// per-row failure is also printed immediately so it isn't swallowed even when
/// the caller only inspects the aggregate outcome.
fn delete_plan(
    plan: &[(String, String, GcReason)],
    mut delete: impl FnMut(&str, &str) -> Result<()>,
) -> GcDeleteOutcome {
    let mut deleted = 0usize;
    let mut failed = 0usize;
    let mut first_error: Option<anyhow::Error> = None;
    for (source, source_id, reason) in plan {
        if !matches!(reason, GcReason::DisabledSource | GcReason::Orphan) {
            continue;
        }
        match delete(source, source_id) {
            Ok(()) => deleted += 1,
            Err(err) => {
                eprintln!("gc: failed to delete {source}/{source_id}: {err:#}");
                failed += 1;
                if first_error.is_none() {
                    first_error = Some(err);
                }
            }
        }
    }
    GcDeleteOutcome { deleted, failed, first_error }
}

pub(crate) fn run_config_show() -> Result<()> {
    let path = crate::config::config_path()?;
    let config = crate::config::AppConfig::load_or_default();
    println!("# {}", path.display());
    println!("{}", serde_json::to_string_pretty(&config)?);
    Ok(())
}

pub(crate) fn run_config_edit() -> Result<()> {
    if !std::io::stdin().is_terminal() {
        anyhow::bail!("config edit requires an interactive terminal");
    }
    let path = crate::config::config_path()?;
    if !path.exists() {
        crate::config::AppConfig::default().save()?;
        println!("Created default config at {}", path.display());
    }
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
    // Command::status() inherits parent stdio, so a full-screen TUI editor works.
    let status = std::process::Command::new(&editor).arg(&path).status()?;
    if !status.success() {
        anyhow::bail!("editor `{editor}` exited with a non-zero status");
    }
    match crate::config::AppConfig::load() {
        Ok(_) => {
            println!("Config saved and valid.");
            Ok(())
        }
        Err(err) => anyhow::bail!("config is invalid after edit: {err}"),
    }
}

pub(crate) fn run_config_doctor() -> Result<()> {
    let path = crate::config::config_path()?;
    let raw = match std::fs::read_to_string(&path) {
        Ok(content) => Some(content),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(err.into()),
    };
    let mode = config_file_mode(&path);
    let labels = crate::adapters::source_labels();
    let report = crate::config::evaluate_config(raw.as_deref(), mode, &labels);

    println!("Config Doctor");
    println!("  Path         {}", path.display());
    for check in &report.checks {
        let tag = match check.level {
            crate::config::DoctorLevel::Info => "info",
            crate::config::DoctorLevel::Warn => "warn",
            crate::config::DoctorLevel::Error => "FAIL",
        };
        println!("  [{tag}] {:<12} {}", check.label, check.detail);
    }
    println!("  [info] {:<12} {}", "embedding", crate::embedding::availability_summary());

    if report.has_errors() {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(unix)]
fn config_file_mode(path: &std::path::Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).ok().map(|meta| meta.permissions().mode())
}

#[cfg(not(unix))]
fn config_file_mode(_path: &std::path::Path) -> Option<u32> {
    None
}

#[cfg(test)]
mod gc_tests {
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};

    use super::{GcReason, classify};

    fn probe(existing: &[&str]) -> impl Fn(&Path) -> bool {
        let set: HashSet<PathBuf> = existing.iter().map(PathBuf::from).collect();
        move |p: &Path| set.contains(p)
    }

    #[test]
    fn disabled_source_is_bucketed_before_any_path_check() {
        // Source disabled: reason is DisabledSource even though the file exists.
        let exists = probe(&["/root/s.jsonl", "/root"]);
        assert_eq!(classify(false, Some("/root/s.jsonl"), &exists), GcReason::DisabledSource);
    }

    #[test]
    fn enabled_source_with_present_file_is_kept() {
        let exists = probe(&["/root/s.jsonl", "/root"]);
        assert_eq!(classify(true, Some("/root/s.jsonl"), &exists), GcReason::Keep);
    }

    #[test]
    fn enabled_source_with_missing_file_but_present_root_is_orphan() {
        let exists = probe(&["/root"]); // parent exists, file does not
        assert_eq!(classify(true, Some("/root/gone.jsonl"), &exists), GcReason::Orphan);
    }

    #[test]
    fn null_path_row_is_unverifiable_and_safe_kept() {
        let exists = probe(&[]);
        assert_eq!(classify(true, None, &exists), GcReason::Unverifiable);
    }

    #[test]
    fn missing_root_row_is_unverifiable_not_orphan() {
        // Neither the file nor its parent dir exist (unmounted drive / renamed root).
        let exists = probe(&[]);
        assert_eq!(classify(true, Some("/mnt/usb/s.jsonl"), &exists), GcReason::Unverifiable);
    }

    #[test]
    fn orphan_and_disabled_source_rows_are_deleted_via_store() {
        use crate::db::store::Store;
        use crate::types::Session;

        crate::db::schema::register_sqlite_vec();
        let dir = std::env::temp_dir().join(format!("recall-gc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let present = dir.join("present.jsonl");
        std::fs::write(&present, b"{}").unwrap();
        let missing = dir.join("missing.jsonl"); // never created

        let store = Store::open_in_memory().unwrap();
        let mk = |id: &str, source: &str, path: &std::path::Path| Session {
            id: id.to_string(),
            source: source.to_string(),
            source_id: format!("src-{id}"),
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
            source_file_path: Some(path.to_str().unwrap().to_string()),
            is_import: false,
        };
        // "claude-code" is enabled: keep (file present) + orphan (file missing).
        store.insert_session(&mk("keep", "claude-code", &present)).unwrap();
        store.insert_session(&mk("orphan", "claude-code", &missing)).unwrap();
        // "codex" is disabled entirely: its row must be deleted via the
        // DisabledSource branch even though the file itself still exists.
        store.insert_session(&mk("disabled", "codex", &present)).unwrap();

        let exists = |p: &std::path::Path| p.exists();
        let enabled_for = |source: &str| source != "codex";
        for source in ["claude-code", "codex"] {
            for sp in store.session_paths_for_source(source).unwrap() {
                let reason =
                    super::classify(enabled_for(source), sp.source_file_path.as_deref(), &exists);
                if matches!(reason, super::GcReason::Orphan | super::GcReason::DisabledSource) {
                    store.delete_session_data(source, &sp.source_id).unwrap();
                }
            }
        }

        let remaining_claude_code = store.session_paths_for_source("claude-code").unwrap();
        assert_eq!(remaining_claude_code.len(), 1);
        assert_eq!(remaining_claude_code[0].source_id, "src-keep");

        let remaining_codex = store.session_paths_for_source("codex").unwrap();
        assert!(remaining_codex.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn partial_delete_failure_continues_and_reports_first_error() {
        // Store-level errors mid-gc-loop can't be forced through real SQLite
        // (deleting a row that doesn't exist is a silent no-op, not an Err), so
        // this drives `delete_plan` directly with an injected failing callback
        // -- the same dependency-injection seam `classify` already uses for
        // its `exists` probe.
        let plan = vec![
            ("claude-code".to_string(), "a".to_string(), GcReason::Orphan),
            ("claude-code".to_string(), "b".to_string(), GcReason::DisabledSource),
            ("claude-code".to_string(), "c".to_string(), GcReason::Orphan),
            ("claude-code".to_string(), "keep".to_string(), GcReason::Keep),
        ];
        let outcome = super::delete_plan(&plan, |_source, source_id| {
            if source_id == "b" {
                anyhow::bail!("simulated failure for {source_id}");
            }
            Ok(())
        });
        assert_eq!(outcome.deleted, 2, "a and c succeed despite b failing");
        assert_eq!(outcome.failed, 1);
        assert!(outcome.first_error.is_some());
        assert!(outcome.first_error.unwrap().to_string().contains("simulated failure for b"));
    }
}
