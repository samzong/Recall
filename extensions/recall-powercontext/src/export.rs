use std::io::{BufRead, BufReader, Read, Seek, stdin};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::session::{ExportRecord, parse_export_line};

#[derive(Clone, Debug)]
pub struct RecallClient {
    bin: PathBuf,
}

impl RecallClient {
    pub fn from_env() -> Self {
        let bin = std::env::var_os("RECALL_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("recall"));
        Self { bin }
    }

    pub fn current_scope(&self, cwd: &Path) -> Result<serde_json::Value> {
        let output = Command::new(&self.bin)
            .args(["info", "--format", "json"])
            .current_dir(cwd)
            .output()
            .context("failed to run recall info --format json")?;
        if !output.status.success() {
            bail!("Recall info failed: {}", String::from_utf8_lossy(&output.stderr).trim());
        }
        let info: serde_json::Value =
            serde_json::from_slice(&output.stdout).context("Recall info did not return JSON")?;
        if info["protocol_version"] != 2
            || !info.get("scope").is_some_and(|scope| scope.is_object() || scope.is_null())
        {
            bail!("Recall info must return a protocol 2 scope; rebuild or upgrade Recall");
        }
        Ok(info["scope"].clone())
    }

    pub fn export_sessions(
        &self,
        cwd: &Path,
        project: &str,
        time: Option<&str>,
    ) -> Result<ScopedExport> {
        let args = export_args(project, time);
        let mut output = tempfile::tempfile().context("failed to create Recall export buffer")?;
        let mut errors = tempfile::tempfile().context("failed to create Recall error buffer")?;
        let stdout = output.try_clone().context("failed to clone Recall export buffer")?;
        let stderr = errors.try_clone().context("failed to clone Recall error buffer")?;
        let status = Command::new(&self.bin)
            .args(&args)
            .current_dir(cwd)
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .status()
            .with_context(|| format!("failed to run {}", command_label(&self.bin, &args)))?;
        errors.rewind().context("failed to rewind Recall error buffer")?;
        let mut bytes = Vec::new();
        errors.read_to_end(&mut bytes).context("failed to read Recall error output")?;
        let detail = String::from_utf8_lossy(&bytes);
        if !status.success() {
            let detail = detail.trim();
            let command = command_label(&self.bin, &args);
            if detail.is_empty() {
                bail!("Recall command failed while running `{command}`: {status}");
            }
            bail!("Recall command failed while running `{command}`: {status}\n{detail}");
        }
        for line in detail.lines() {
            eprintln!("{line}");
        }
        output.rewind().context("failed to rewind Recall export buffer")?;
        let mut reader = BufReader::new(output);
        let mut line = String::new();
        reader.read_line(&mut line).context("failed to read Recall scope record")?;
        let header: serde_json::Value = serde_json::from_str(&line)
            .context("failed to parse Recall export scope record on line 1")?;
        if header["record_type"] != "scope" || header["protocol_version"] != 2 {
            bail!("Recall export must return a protocol 2 scope record; rebuild or upgrade Recall");
        }
        Ok(ScopedExport { reader })
    }
}

pub struct ScopedExport {
    reader: BufReader<std::fs::File>,
}

impl ScopedExport {
    pub fn visit_sessions(
        &mut self,
        visit: &mut impl FnMut(ExportRecord) -> Result<()>,
    ) -> Result<()> {
        visit_export_jsonl(&mut self.reader, visit)
    }
}

fn export_args(project: &str, time: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "export".to_string(),
        "--with-scope".to_string(),
        "--project".to_string(),
        project.to_string(),
        "--limit".to_string(),
        "0".to_string(),
        "--include".to_string(),
        "metadata,messages".to_string(),
    ];
    if let Some(time) = time {
        args.push("--time".to_string());
        args.push(time.to_string());
    }
    args
}

pub fn normalize_time_selector(time: Option<&str>) -> Result<Option<String>> {
    match time.map(|value| value.trim().to_ascii_lowercase()).as_deref() {
        None | Some("") | Some("all") => Ok(None),
        Some(time) if matches!(time, "today" | "7d" | "week" | "30d" | "month") => {
            Ok(Some(time.to_string()))
        }
        Some(time) => {
            bail!("unknown time range: {time}; expected today, 7d, week, 30d, month, or all")
        }
    }
}

pub fn visit_stdin_sessions(visit: &mut impl FnMut(ExportRecord) -> Result<()>) -> Result<()> {
    visit_export_jsonl(stdin().lock(), visit)
}

fn visit_export_jsonl(
    reader: impl BufRead,
    visit: &mut impl FnMut(ExportRecord) -> Result<()>,
) -> Result<()> {
    for (index, line) in reader.lines().enumerate() {
        let line_no = index + 1;
        let line = line.with_context(|| format!("failed to read export JSONL line {line_no}"))?;
        if !line.trim().is_empty() {
            if index == 0 {
                let header: serde_json::Value = serde_json::from_str(&line)
                    .with_context(|| format!("failed to parse export JSONL line {line_no}"))?;
                if header["record_type"] == "scope" {
                    if header["protocol_version"] != 2 || !header["scope"].is_object() {
                        bail!("invalid Recall export scope record on line {line_no}");
                    }
                    continue;
                }
            }
            visit(parse_export_line(&line, line_no)?)?;
        }
    }
    Ok(())
}

fn command_label(bin: &Path, args: &[String]) -> String {
    let mut parts = Vec::with_capacity(args.len() + 1);
    parts.push(bin.display().to_string());
    parts.extend(args.iter().cloned());
    parts.join(" ")
}
