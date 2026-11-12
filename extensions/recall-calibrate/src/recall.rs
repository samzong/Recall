use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use chrono::{Local, TimeZone};
use serde::Deserialize;

const USAGE_OWNERSHIP_REQUIRED: &str =
    "Recall does not support usage ownership export; rebuild or upgrade Recall";

#[derive(Deserialize)]
struct Record {
    #[serde(default)]
    session: Option<Session>,
    #[serde(default)]
    usage_events: Vec<UsageEvent>,
}

#[derive(Deserialize)]
struct Session {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    source: Option<String>,
}

#[derive(Deserialize)]
pub struct UsageEvent {
    is_counted: Option<bool>,
    #[serde(default)]
    pub timestamp: i64,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub input_tokens: i64,
    #[serde(default)]
    pub output_tokens: i64,
    #[serde(default)]
    pub reasoning_tokens: i64,
    #[serde(default)]
    pub cache_read_tokens: i64,
    #[serde(default)]
    pub cache_write_tokens: i64,
    #[serde(default)]
    pub source_path: Option<String>,
    #[serde(default)]
    pub raw_usage_json: Option<String>,
    #[serde(skip)]
    pub session_id: String,
}

pub fn export_usage(sources: &str, time: &str) -> Result<BTreeMap<String, Vec<UsageEvent>>> {
    let cutoff = usage_cutoff(time)?;
    let bin = std::env::var_os("RECALL_BIN").map(PathBuf::from).unwrap_or_else(|| "recall".into());
    let args = [
        "export",
        "--project",
        "all",
        "--source",
        sources,
        "--time",
        "all",
        "--include",
        "metadata,messages,usage,usage-ownership",
        "--messages",
        "none",
        "--limit",
        "0",
    ];
    let mut child = Command::new(&bin)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to run `{} {}`", bin.display(), args.join(" ")))?;
    let mut stderr = child.stderr.take().context("missing recall stderr")?;
    let stderr_reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });
    let stdout = child.stdout.take().context("missing recall stdout")?;
    let events = read_usage(BufReader::new(stdout), cutoff);
    if events.is_err() {
        let _ = child.kill();
    }
    let status = child.wait().context("failed to wait for recall export")?;
    let stderr = stderr_reader.join().unwrap_or_default();
    let events = events?;
    if !status.success() {
        if stderr.contains("unknown include field: usage-ownership") {
            bail!(USAGE_OWNERSHIP_REQUIRED);
        }
        bail!("`recall {}` failed: {}", args.join(" "), stderr.trim());
    }
    Ok(events)
}

fn usage_cutoff(time: &str) -> Result<Option<i64>> {
    let now = Local::now();
    match time.to_lowercase().as_str() {
        "today" => Ok(now
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .and_then(|start| Local.from_local_datetime(&start).earliest())
            .map(|start| start.timestamp_millis())),
        "7d" | "week" => Ok(Some(now.timestamp_millis() - 7 * 24 * 3600 * 1000)),
        "30d" | "month" => Ok(Some(now.timestamp_millis() - 30 * 24 * 3600 * 1000)),
        "all" => Ok(None),
        _ => bail!("unknown time range: {time}; expected today, 7d, week, 30d, month, or all"),
    }
}

fn read_usage(
    reader: impl BufRead,
    cutoff: Option<i64>,
) -> Result<BTreeMap<String, Vec<UsageEvent>>> {
    let mut events: BTreeMap<String, Vec<UsageEvent>> = BTreeMap::new();
    for line in reader.lines() {
        let line = line.context("failed to read recall export output")?;
        if line.trim().is_empty() {
            continue;
        }
        let record: Record =
            serde_json::from_str(&line).context("recall export returned invalid JSONL")?;
        let Some(session) = record.session else { continue };
        let session_id = session.id.unwrap_or_default();
        let Some(source) = session.source else { continue };
        for mut event in record.usage_events {
            let is_counted = event.is_counted.context(USAGE_OWNERSHIP_REQUIRED)?;
            if !is_counted || cutoff.is_some_and(|cutoff| event.timestamp < cutoff) {
                continue;
            }
            event.session_id = session_id.clone();
            events.entry(source.clone()).or_default().push(event);
        }
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_consumer_uses_core_ownership_and_event_time() {
        let record = serde_json::json!({
            "session": { "id": "old-session", "source": "codex", "started_at": 1 },
            "usage_events": [
                { "timestamp": 20, "input_tokens": 11, "is_counted": true },
                { "timestamp": 20, "input_tokens": 11, "is_counted": false },
                { "timestamp": 1, "input_tokens": 12, "is_counted": true }
            ]
        });
        let text = record.to_string();
        let events = read_usage(text.as_bytes(), Some(10)).unwrap();
        assert_eq!(events["codex"].len(), 1);
        assert_eq!(events["codex"][0].input_tokens, 11);
        assert_eq!(events["codex"][0].session_id, "old-session");
    }

    #[test]
    fn usage_consumer_rejects_unknown_ownership() {
        let record = serde_json::json!({
            "session": { "id": "session", "source": "codex" },
            "usage_events": [{ "timestamp": 20 }]
        });
        assert!(read_usage(record.to_string().as_bytes(), None).is_err());
    }
}
