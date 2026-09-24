use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use tracing::debug;
use walkdir::WalkDir;

use crate::adapters::file_scan::{self, FileScanEntry, FileScanOptions};
use crate::adapters::json_util::rfc3339_ms;
use crate::adapters::paths;
use crate::adapters::{
    AdapterSyncContext, RawMessage, RawSession, RawUsageEvent, ResumeCommand, SourceAdapter,
    SyncScanResult,
};
use crate::types::{Role, TokenSource};

pub(crate) struct DevinAdapter;

const USAGE_PARSER_VERSION: u32 = 2;

impl SourceAdapter for DevinAdapter {
    fn id(&self) -> &str {
        "devin"
    }
    fn label(&self) -> &str {
        "DVN"
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        Some(ResumeCommand::new("devin", &["--resume", source_id]))
    }

    fn start_command(&self, prompt: String) -> Option<ResumeCommand> {
        Some(ResumeCommand::new("devin", &["--", &prompt]))
    }

    fn usage_parser_version(&self) -> Option<u32> {
        Some(USAGE_PARSER_VERSION)
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let Some(dir) = transcripts_dir() else {
            return Ok(vec![]);
        };
        Ok(collect_entries(&dir)
            .into_iter()
            .filter_map(|entry| {
                let mtime_ms = file_scan::stat_mtime_ms(&entry.stat_target)?;
                parse_entry(&entry, mtime_ms)
            })
            .collect())
    }

    fn scan_for_sync(
        &self,
        context: &AdapterSyncContext,
        since_ts: Option<i64>,
        _include_events: bool,
    ) -> anyhow::Result<Option<SyncScanResult>> {
        let Some(dir) = transcripts_dir() else {
            return Ok(Some(SyncScanResult::default()));
        };
        let result = file_scan::run_file_scan_with_options(
            context,
            since_ts,
            FileScanOptions {
                usage_parser_version: Some(USAGE_PARSER_VERSION),
                ..Default::default()
            },
            collect_entries(&dir),
            |entry, mtime_ms| Ok(parse_entry(&entry, mtime_ms)),
        )?;
        Ok(Some(result))
    }
}

fn transcripts_dir() -> Option<PathBuf> {
    let root = match paths::env_path_dir("DEVIN_HOME") {
        Some(dir) => dir,
        None => std::env::var_os("XDG_DATA_HOME")
            .filter(|xdg| !xdg.is_empty())
            .map(|xdg| PathBuf::from(xdg).join("devin"))
            .filter(|dir| dir.is_dir())
            .or_else(|| dirs::home_dir().map(|home| home.join(".local/share/devin")))?,
    };
    paths::existing_dir(root.join("cli/transcripts"))
}

fn collect_entries(transcripts_dir: &Path) -> Vec<FileScanEntry> {
    WalkDir::new(transcripts_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter_map(|walk_entry| {
            let path = walk_entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") || !path.is_file() {
                return None;
            }
            let stem = path.file_stem()?.to_str().filter(|s| !s.is_empty())?;
            Some(FileScanEntry {
                session_id: stem.to_string(),
                stat_target: path.to_path_buf(),
                directory: None,
            })
        })
        .collect()
}

fn parse_entry(entry: &FileScanEntry, mtime_ms: i64) -> Option<RawSession> {
    parse_transcript(&entry.stat_target, &entry.session_id, mtime_ms)
        .inspect_err(|e| {
            debug!("failed to parse devin session {}: {e}", entry.stat_target.display())
        })
        .ok()
        .flatten()
}

fn parse_transcript(
    path: &Path,
    session_id: &str,
    mtime_ms: i64,
) -> anyhow::Result<Option<RawSession>> {
    let v: Value = serde_json::from_str(&fs::read_to_string(path)?)?;

    let schema_version = v.get("schema_version").and_then(|s| s.as_str()).unwrap_or("");
    if !schema_version.starts_with("ATIF") {
        debug!("Unsupported schema version: {}", schema_version);
        return Ok(None);
    }

    let model_name = v.pointer("/agent/model_name").and_then(|m| m.as_str()).unwrap_or("unknown");
    let source_path = path.to_str().map(str::to_string);

    let mut messages = Vec::new();
    let mut usage_events = Vec::new();
    let mut first_ts: Option<i64> = None;
    let mut last_ts: Option<i64> = None;

    for step in v.get("steps").and_then(|s| s.as_array()).into_iter().flatten() {
        let timestamp = rfc3339_ms(step.get("timestamp"));
        if let Some(ts) = timestamp {
            first_ts = Some(first_ts.map_or(ts, |first| first.min(ts)));
            last_ts = Some(last_ts.map_or(ts, |last| last.max(ts)));
        }

        let role = match step.get("source").and_then(|s| s.as_str()) {
            Some("user") => Some(Role::User),
            Some("agent") => Some(Role::Assistant),
            _ => None,
        };
        let content = step.get("message").and_then(|m| m.as_str()).unwrap_or("");
        if let Some(role) = role
            && !content.is_empty()
        {
            messages.push(RawMessage { role, content: content.to_string(), timestamp });
        }

        let Some(metrics) = step.get("metrics").and_then(|m| m.as_object()) else {
            continue;
        };
        let metric = |key: &str| metrics.get(key).and_then(|v| v.as_i64()).unwrap_or(0);
        let prompt_tokens = metric("prompt_tokens").max(0);
        let completion_tokens = metric("completion_tokens").max(0);
        let cache_read_tokens = metric("cached_tokens").clamp(0, prompt_tokens);
        if prompt_tokens == 0 && completion_tokens == 0 {
            continue;
        }
        let seq = usage_events.len();
        usage_events.push(RawUsageEvent {
            event_key: format!("{session_id}-{seq}"),
            event_seq: seq as u32,
            message_seq: None,
            timestamp: timestamp.unwrap_or(0),
            model: step
                .get("model_name")
                .and_then(|m| m.as_str())
                .unwrap_or(model_name)
                .to_string(),
            provider: "devin".to_string(),
            input_tokens: prompt_tokens - cache_read_tokens,
            output_tokens: completion_tokens,
            cache_read_tokens,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
            token_source: TokenSource::Observed,
            parser_version: USAGE_PARSER_VERSION,
            source_path: source_path.clone(),
            raw_usage_json: None,
        });
    }

    if messages.is_empty() {
        return Ok(None);
    }

    let mut raw = RawSession::search_only(
        session_id,
        working_directory(path, session_id),
        first_ts.unwrap_or(0),
        Some(mtime_ms),
        None,
        messages,
    )
    .with_usage(usage_events, USAGE_PARSER_VERSION);
    raw.source_file_path = source_path;
    raw.summary = Some(format!("Devin session using {model_name}"));
    raw.duration_minutes = match (first_ts, last_ts) {
        (Some(first), Some(last)) if last >= first => Some(((last - first) / 60_000) as u32),
        _ => None,
    };
    Ok(Some(raw))
}

fn working_directory(transcript_path: &Path, session_id: &str) -> Option<String> {
    let db_path = transcript_path.parent()?.parent()?.join("sessions.db");
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    conn.query_row("SELECT working_directory FROM sessions WHERE id = ?", [session_id], |row| {
        row.get::<_, String>(0)
    })
    .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_excludes_cached_from_input_and_uses_step_model() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("s1.json");
        let transcript = serde_json::json!({
            "schema_version": "ATIF-v1.7",
            "session_id": "s1",
            "agent": {"name": "devin", "model_name": "SWE-2 High"},
            "steps": [
                {"timestamp": "2026-09-24T08:00:00Z", "source": "user", "message": "hi"},
                {"timestamp": "2026-09-24T08:00:05Z", "source": "agent", "message": "hello",
                 "model_name": "swe-1-6-slow",
                 "metrics": {"prompt_tokens": 1000, "completion_tokens": 50, "cached_tokens": 970}},
                {"timestamp": "2026-09-24T08:01:00Z", "source": "agent", "message": "done",
                 "metrics": {"prompt_tokens": 200, "completion_tokens": 10, "cached_tokens": 0}}
            ]
        });
        fs::write(&path, transcript.to_string()).unwrap();

        let raw = parse_transcript(&path, "s1", 0).unwrap().unwrap();

        assert_eq!(raw.usage_events.len(), 2);
        let first = &raw.usage_events[0];
        assert_eq!(first.model, "swe-1-6-slow");
        assert_eq!(first.input_tokens, 30);
        assert_eq!(first.cache_read_tokens, 970);
        assert_eq!(first.output_tokens, 50);
        let second = &raw.usage_events[1];
        assert_eq!(second.model, "SWE-2 High");
        assert_eq!(second.input_tokens, 200);
        assert_eq!(second.cache_read_tokens, 0);
    }
}
