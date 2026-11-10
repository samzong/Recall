use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

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
}

#[derive(Deserialize)]
pub struct UsageEvent {
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

pub fn export_usage(source: &str, time: &str) -> Result<Vec<UsageEvent>> {
    let bin = std::env::var_os("RECALL_BIN").map(PathBuf::from).unwrap_or_else(|| "recall".into());
    let args = [
        "export",
        "--project",
        "all",
        "--source",
        source,
        "--time",
        time,
        "--include",
        "metadata,messages,usage",
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
    let mut events = vec![];
    let stdout = child.stdout.take().context("missing recall stdout")?;
    for line in BufReader::new(stdout).lines() {
        let line = line.context("failed to read recall export output")?;
        if line.trim().is_empty() {
            continue;
        }
        let record: Record =
            serde_json::from_str(&line).context("recall export returned invalid JSONL")?;
        let session_id = record.session.and_then(|s| s.id).unwrap_or_default();
        for mut event in record.usage_events {
            event.session_id = session_id.clone();
            events.push(event);
        }
    }
    let status = child.wait().context("failed to wait for recall export")?;
    let stderr = stderr_reader.join().unwrap_or_default();
    if !status.success() {
        bail!("`recall {}` failed: {}", args.join(" "), stderr.trim());
    }
    Ok(events)
}
