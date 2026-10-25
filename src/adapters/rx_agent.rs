use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

use anyhow::ensure;
use tracing::warn;

use crate::adapters::file_scan::{self, FileScanEntry, FileScanOptions};
use crate::adapters::json_util::{json_i64, jsonl_indexed, rfc3339_ms};
use crate::adapters::pi_session::{self, EVENT_PARSER_VERSION, Format, USAGE_PARSER_VERSION};
use crate::adapters::{
    AdapterSyncContext, RawSession, ResumeCommand, SourceAdapter, SyncScanResult,
};

const METADATA_PARSER_VERSION: u32 = 1;

pub(crate) struct RxAgentAdapter;

impl SourceAdapter for RxAgentAdapter {
    fn id(&self) -> &str {
        "rx-agent"
    }

    fn label(&self) -> &str {
        "rx-agent"
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        Some(ResumeCommand::new("rx-agent", &["--resume", source_id]))
    }

    fn usage_parser_version(&self) -> Option<u32> {
        Some(USAGE_PARSER_VERSION)
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let mut sessions = Vec::new();
        for entry in entries()? {
            if let Some(mtime) = file_scan::stat_mtime_ms(&entry.stat_target)
                && let Some(raw) = parse_entry(entry, mtime, true)?
            {
                sessions.push(raw);
            }
        }
        Ok(sessions)
    }

    fn scan_for_sync(
        &self,
        context: &AdapterSyncContext,
        since_ts: Option<i64>,
        include_events: bool,
    ) -> anyhow::Result<Option<SyncScanResult>> {
        Ok(Some(file_scan::run_file_scan_with_options(
            context,
            since_ts,
            FileScanOptions {
                stream_sessions: true,
                usage_parser_version: Some(USAGE_PARSER_VERSION),
                event_parser_version: include_events.then_some(EVENT_PARSER_VERSION),
                metadata_parser_version: Some(METADATA_PARSER_VERSION),
            },
            entries()?,
            |entry, mtime| parse_entry(entry, mtime, include_events),
        )?))
    }
}

fn entries() -> anyhow::Result<Vec<FileScanEntry>> {
    let home = std::env::var_os("RX_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".rx")))
        .ok_or_else(|| anyhow::anyhow!("no home dir"))?;
    Ok(pi_session::collect_entries(&[home.join("sessions")], Format::Pi))
}

fn parse_entry(
    entry: FileScanEntry,
    mtime: i64,
    include_events: bool,
) -> anyhow::Result<Option<RawSession>> {
    match parse_session(&entry, mtime, include_events) {
        Ok(raw) => Ok(raw),
        Err(error) => {
            warn!("failed to parse rx-agent session {}: {error}", entry.stat_target.display());
            Ok(None)
        }
    }
}

pub(crate) fn parse_session(
    entry: &FileScanEntry,
    mtime: i64,
    include_events: bool,
) -> anyhow::Result<Option<RawSession>> {
    let mut reader = BufReader::new(fs::File::open(&entry.stat_target)?);
    let lines = std::iter::from_fn(move || {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => None,
            Ok(_) if !line.ends_with('\n') => None,
            Ok(_) => Some(Ok(line)),
            Err(error) => Some(Err(error)),
        }
    });
    let mut records = jsonl_indexed(lines);
    let Some(header) = records.next().transpose()? else {
        return Ok(None);
    };
    let (line, header) = header;
    ensure!(line == 0 && header["type"] == "session", "missing session header");
    ensure!(header["v"].as_u64() == Some(1), "unsupported session log version");
    let id = header["id"].as_str().filter(|id| uuid::Uuid::try_parse(id).is_ok());
    let cwd = header["cwd"].as_str().filter(|cwd| !cwd.is_empty());
    let started_at = rfc3339_ms(header.get("createdAt"));
    ensure!(id.is_some() && cwd.is_some() && started_at.is_some(), "invalid session header");
    ensure!(id == Some(entry.session_id.as_str()), "session id does not match filename");
    let path = entry.stat_target.to_string_lossy().to_string();
    let mut raw = RawSession::search_only(
        id.unwrap().to_string(),
        cwd.map(str::to_string),
        started_at.unwrap(),
        Some(mtime),
        None,
        Vec::new(),
    );
    for record in records {
        let (line, record) = record?;
        if record["type"] != "message" {
            continue;
        }
        let message = &record["message"];
        let timestamp = json_i64(message.get("timestamp"))
            .or_else(|| rfc3339_ms(record.get("at")))
            .unwrap_or(raw.started_at);
        if include_events {
            pi_session::extract_events(
                &record,
                message,
                line,
                timestamp,
                (&path, raw.messages.len().checked_sub(1).map(|seq| seq as u32)),
                cwd,
                Format::Pi,
                &mut raw.events,
            );
        }
        pi_session::parse_message(
            &record,
            message,
            line as u32,
            timestamp,
            None,
            None,
            &path,
            None,
            &mut raw.messages,
            &mut raw.usage_events,
        );
    }
    raw.usage_parser_version = Some(USAGE_PARSER_VERSION);
    raw.event_parser_version = include_events.then_some(EVENT_PARSER_VERSION);
    raw.metadata_parser_version = Some(METADATA_PARSER_VERSION);
    raw.source_file_path = Some(path);
    raw.thread_role = Some(crate::types::ThreadRole::Primary);
    Ok((!raw.messages.is_empty() || !raw.usage_events.is_empty() || !raw.events.is_empty())
        .then_some(raw))
}
