use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde_json::Value;
use tracing::{debug, warn};
use walkdir::WalkDir;

use crate::adapters::AdapterSyncContext;
use crate::adapters::file_scan::{self, FileScanEntry};
use crate::adapters::json_util::json_i64;
use crate::adapters::paths;
use crate::adapters::{
    RawSession, ResumeCommand, SourceAdapter, SyncScanOutput, SyncScanResult, SyncScanStats,
};

#[cfg(test)]
mod tests;
mod wire;

use wire::parse_kimi_wire;

pub(crate) struct KimiCodeAdapter;

pub(super) const USAGE_PARSER_VERSION: u32 = 1;
pub(super) const EVENT_PARSER_VERSION: u32 = 1;

fn kimi_scan_options(include_events: bool) -> file_scan::FileScanOptions {
    file_scan::FileScanOptions {
        stream_sessions: true,
        usage_parser_version: Some(USAGE_PARSER_VERSION),
        event_parser_version: include_events.then_some(EVENT_PARSER_VERSION),
        ..Default::default()
    }
}

impl SourceAdapter for KimiCodeAdapter {
    fn id(&self) -> &str {
        "kimi-code"
    }
    fn label(&self) -> &str {
        "KC"
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        Some(ResumeCommand::new("kimi", &["--session", source_id]))
    }

    fn usage_parser_version(&self) -> Option<u32> {
        Some(USAGE_PARSER_VERSION)
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        scan_kimi_dirs(&resolve_kimi_dirs()?)
    }

    fn scan_for_sync(
        &self,
        context: &AdapterSyncContext,
        since_ts: Option<i64>,
        include_events: bool,
    ) -> anyhow::Result<Option<SyncScanResult>> {
        Ok(Some(scan_kimi_dirs_for_sync(&resolve_kimi_dirs()?, context, since_ts, include_events)?))
    }

    fn scan_for_sync_output(
        &self,
        context: &AdapterSyncContext,
        since_ts: Option<i64>,
        include_events: bool,
        force: bool,
    ) -> anyhow::Result<Option<SyncScanOutput>> {
        let scan = if force {
            let sessions = scan_kimi_dirs_with_context(&resolve_kimi_dirs()?, Some(context))?;
            let parsed = sessions.len() as u32;
            SyncScanResult {
                sessions,
                stats: SyncScanStats { candidates: parsed, parsed, ..Default::default() },
                ..Default::default()
            }
        } else {
            scan_kimi_dirs_for_sync(&resolve_kimi_dirs()?, context, since_ts, include_events)?
        };
        Ok(Some(SyncScanOutput { scan, reconcile: None }))
    }
}

fn scan_kimi_dirs(sessions_dirs: &[PathBuf]) -> anyhow::Result<Vec<RawSession>> {
    scan_kimi_dirs_with_context(sessions_dirs, None)
}

fn scan_kimi_dirs_with_context(
    sessions_dirs: &[PathBuf],
    context: Option<&AdapterSyncContext>,
) -> anyhow::Result<Vec<RawSession>> {
    let mut sessions = Vec::new();
    let mut claimed = HashSet::new();
    for sessions_dir in sessions_dirs {
        sessions.extend(scan_kimi_session_entries_with_parser(
            claim_session_entries(
                collect_session_entries_with_context(sessions_dir, context),
                &mut claimed,
            ),
            |entry, mtime_ms| parse_kimi_session_file(entry, mtime_ms, true),
        )?);
    }
    Ok(sessions)
}

fn scan_kimi_dirs_for_sync(
    sessions_dirs: &[PathBuf],
    context: &AdapterSyncContext,
    since_ts: Option<i64>,
    include_events: bool,
) -> anyhow::Result<SyncScanResult> {
    let mut combined = SyncScanResult::default();
    let mut claimed = HashSet::new();
    for sessions_dir in sessions_dirs {
        combined.absorb(file_scan::run_file_scan_with_options_and_snapshot(
            context,
            since_ts,
            kimi_scan_options(include_events),
            claim_session_entries(
                collect_session_entries_with_context(sessions_dir, Some(context)),
                &mut claimed,
            ),
            kimi_session_snapshot,
            |entry, mtime_ms| parse_kimi_session_file(entry, mtime_ms, include_events),
        )?);
    }
    Ok(combined)
}

fn claim_session_entries(
    entries: Vec<FileScanEntry>,
    claimed: &mut HashSet<String>,
) -> Vec<FileScanEntry> {
    entries.into_iter().filter(|entry| claimed.insert(entry.session_id.clone())).collect()
}

fn resolve_kimi_dirs() -> anyhow::Result<Vec<PathBuf>> {
    if let Some(home) = paths::env_path_dir("KIMI_CODE_HOME") {
        let Some(dir) = paths::existing_dir(home.join("sessions")) else {
            debug!("KIMI_CODE_HOME/sessions not found, skipping Kimi Code");
            return Ok(Vec::new());
        };
        return Ok(vec![dir]);
    }
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home dir"))?;
    let mut dirs = Vec::new();
    for relative in [".kimi-code/sessions", ".kimi/sessions"] {
        if let Some(dir) = paths::existing_dir(home.join(relative)) {
            dirs.push(dir);
        }
    }
    if dirs.is_empty() {
        debug!("~/.kimi-code/sessions not found, skipping Kimi Code");
    }
    Ok(dirs)
}

fn scan_kimi_session_entries_with_parser<F>(
    entries: Vec<FileScanEntry>,
    parse_fn: F,
) -> anyhow::Result<Vec<RawSession>>
where
    F: Fn(FileScanEntry, i64) -> anyhow::Result<Option<RawSession>>,
{
    let mut sessions = Vec::new();
    for entry in entries {
        let Some(snapshot) = kimi_session_snapshot(&entry) else {
            continue;
        };
        let raw = parse_fn(entry.clone(), snapshot.effective_mtime_ms())?;
        if kimi_session_snapshot(&entry).as_ref() != Some(&snapshot) {
            debug!(
                "skipping unstable Kimi Code session {}: source files changed while parsing ({})",
                entry.session_id,
                entry.stat_target.display()
            );
            continue;
        }
        if let Some(raw) = raw {
            sessions.push(raw);
        }
    }
    Ok(sessions)
}

#[derive(Clone, Default)]
pub(super) struct StateMeta {
    pub(super) id: Option<String>,
    pub(super) cwd: Option<String>,
    pub(super) created_at: Option<i64>,
    pub(super) title: Option<String>,
    pub(super) is_custom_title: bool,
}

fn parse_state_json(content: &str) -> anyhow::Result<StateMeta> {
    let v: Value = serde_json::from_str(content)?;
    anyhow::ensure!(v.is_object(), "state must be a JSON object");
    Ok(StateMeta {
        id: v.get("id").and_then(|s| s.as_str()).map(str::to_string),
        cwd: v.get("cwd").and_then(|s| s.as_str()).map(str::to_string),
        created_at: json_i64(v.get("createdAt")),
        title: v.get("title").and_then(|s| s.as_str()).map(str::to_string),
        is_custom_title: v.get("isCustomTitle").and_then(|b| b.as_bool()).unwrap_or(false),
    })
}

fn read_state_json(path: &Path) -> anyhow::Result<StateMeta> {
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    parse_state_json(&content).with_context(|| format!("failed to parse {}", path.display()))
}

#[cfg(test)]
fn collect_session_entries(sessions_dir: &Path) -> Vec<FileScanEntry> {
    collect_session_entries_with_context(sessions_dir, None)
}

fn collect_session_entries_with_context(
    sessions_dir: &Path,
    context: Option<&AdapterSyncContext>,
) -> Vec<FileScanEntry> {
    let indexed_paths: HashMap<_, _> = context
        .into_iter()
        .flat_map(AdapterSyncContext::session_paths)
        .filter_map(|session| Some((session.source_file_path.as_deref()?, session)))
        .collect();
    let mut entries = Vec::new();
    if !sessions_dir.exists() {
        return entries;
    }

    for dir_entry in WalkDir::new(sessions_dir).max_depth(3).into_iter().filter_map(|e| e.ok()) {
        let path = dir_entry.path();
        if !path.is_dir() {
            continue;
        }
        let session_dir = path;
        let dir_name = session_dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !dir_name.starts_with("session_") {
            continue;
        }
        let wire_path = session_dir.join("agents").join("main").join("wire.jsonl");
        if !wire_path.is_file() {
            continue;
        }

        let meta = match read_state_json(&session_dir.join("state.json")) {
            Ok(meta) => meta,
            Err(error) => {
                warn!("{error}");
                let indexed = wire_path.to_str().and_then(|path| indexed_paths.get(path));
                StateMeta {
                    id: indexed.map(|session| session.source_id.clone()),
                    cwd: indexed.and_then(|session| session.directory.clone()),
                    ..Default::default()
                }
            }
        };
        let session_id = meta.id.clone().unwrap_or_else(|| dir_name.to_string());
        entries.push(FileScanEntry {
            session_id,
            stat_target: wire_path,
            directory: meta.cwd.clone(),
        });
    }

    entries
}

fn parse_kimi_session_file(
    entry: FileScanEntry,
    mtime_ms: i64,
    include_events: bool,
) -> anyhow::Result<Option<RawSession>> {
    match parse_kimi_session_file_impl(&entry, mtime_ms, include_events) {
        Ok(raw) => Ok(raw),
        Err(e) => {
            warn!("failed to parse {}: {e}", entry.stat_target.display());
            Ok(None)
        }
    }
}

fn parse_kimi_session_file_impl(
    entry: &FileScanEntry,
    mtime_ms: i64,
    include_events: bool,
) -> anyhow::Result<Option<RawSession>> {
    let wire_path = &entry.stat_target;
    let Some(session_dir) = session_dir_for_wire(wire_path) else {
        return Ok(None);
    };
    let meta = read_state_json(&session_dir.join("state.json"));
    let usage_available = meta.is_ok();
    let meta = meta.unwrap_or_else(|error| {
        warn!("{error}");
        StateMeta::default()
    });
    let session_id = meta.id.clone().unwrap_or_else(|| entry.session_id.clone());

    let file = fs::File::open(wire_path)?;
    let reader = BufReader::new(file);
    let source_path = wire_path.to_str().map(str::to_string);
    let mut session = parse_kimi_wire(
        meta,
        session_id,
        reader.lines(),
        mtime_ms,
        entry.directory.clone(),
        source_path,
        include_events,
    )?;
    if !usage_available && let Some(session) = session.as_mut() {
        session.usage_parser_version = None;
        session.usage_events.clear();
    }
    Ok(session)
}

#[cfg(test)]
pub(crate) fn parse_conformance_fixture(sessions_dir: &Path) -> anyhow::Result<Option<RawSession>> {
    let Some(entry) = collect_session_entries(sessions_dir).into_iter().next() else {
        return Ok(None);
    };
    let Some(snapshot) = kimi_session_snapshot(&entry) else {
        return Ok(None);
    };
    parse_kimi_session_file_impl(&entry, snapshot.effective_mtime_ms(), true)
}

fn session_dir_for_wire(wire_path: &Path) -> Option<&Path> {
    wire_path.parent().and_then(Path::parent).and_then(Path::parent)
}

#[derive(Debug, PartialEq, Eq)]
struct KimiSessionSnapshot {
    state: Option<file_scan::FileMetadataSnapshot>,
    wire: file_scan::FileMetadataSnapshot,
}

fn kimi_session_snapshot(
    entry: &FileScanEntry,
) -> Option<file_scan::FileScanSnapshot<KimiSessionSnapshot>> {
    let session_dir = session_dir_for_wire(&entry.stat_target)?;
    let state = file_scan::file_metadata_snapshot(&session_dir.join("state.json"));
    let wire = file_scan::file_metadata_snapshot(&entry.stat_target)?;
    let effective_mtime_ms = wire
        .mtime_ms()?
        .max(state.as_ref().and_then(file_scan::FileMetadataSnapshot::mtime_ms).unwrap_or(0));
    Some(file_scan::FileScanSnapshot::new(effective_mtime_ms, KimiSessionSnapshot { state, wire }))
}

#[cfg(test)]
pub(crate) fn parse_kimi_session(
    state_json: &str,
    wire_jsonl: &str,
    session_id: &str,
) -> Option<RawSession> {
    let meta = parse_state_json(state_json).ok()?;
    let lines = wire_jsonl.lines().map(|line| Ok(line.to_string()));
    parse_kimi_wire(meta, session_id.to_string(), lines, 0, None, None, true).unwrap()
}

#[cfg(test)]
mod degradation_tests {
    use super::*;

    #[test]
    fn corrupt_state_preserves_wire_transcript() {
        for content in [
            Some(b"{".as_slice()),
            Some(b"[]".as_slice()),
            Some([0xff].as_slice()),
            None,
            Some(b"directory".as_slice()),
        ] {
            let root = tempfile::tempdir().unwrap();
            let session_dir = root.path().join("wd_repo/session_abc");
            let wire_dir = session_dir.join("agents/main");
            fs::create_dir_all(&wire_dir).unwrap();
            fs::write(wire_dir.join("wire.jsonl"), super::tests::fixture_wire()).unwrap();
            match content {
                Some(b"directory") => fs::create_dir(session_dir.join("state.json")).unwrap(),
                Some(content) => fs::write(session_dir.join("state.json"), content).unwrap(),
                None => {}
            }
            let sessions = scan_kimi_dirs(&[root.path().to_path_buf()]).unwrap();
            assert_eq!(sessions.len(), 1);
            let session = &sessions[0];
            assert_eq!(session.source_id, "session_abc");
            assert_eq!(session.messages.len(), 2);
            assert_eq!(session.messages[0].content, "fix the bug");
            assert_eq!(session.directory, None);
            assert_eq!(session.started_at, 1700000001001);
            assert_eq!(session.events.len(), 2);
            assert_eq!(session.usage_parser_version, None);
            assert!(session.usage_events.is_empty());
        }
    }
}
