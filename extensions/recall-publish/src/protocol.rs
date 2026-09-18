use std::path::PathBuf;
use std::process::{Command, Output};

use anyhow::{Context, Result, anyhow};

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

    pub fn info(&self) -> Result<serde_json::Value> {
        let args = vec!["info".to_string(), "--format".to_string(), "json".to_string()];
        let stdout = self.capture("info", &args)?;
        serde_json::from_str(&stdout).context("recall info did not return JSON")
    }

    pub fn export_scope(&self, project: &str, source: Option<&str>) -> Result<String> {
        let mut args = vec![
            "export".to_string(),
            "--project".to_string(),
            project.to_string(),
            "--time".to_string(),
            "all".to_string(),
            "--limit".to_string(),
            "0".to_string(),
            "--include".to_string(),
            "metadata,messages,usage,events".to_string(),
        ];
        if let Some(source) = source {
            args.push("--source".to_string());
            args.push(source.to_string());
        }
        self.capture("export", &args)
    }

    pub fn export_ids(&self, ids: &[String]) -> Result<String> {
        let mut args = vec![
            "session".to_string(),
            "export".to_string(),
            "--format".to_string(),
            "jsonl".to_string(),
            "--include".to_string(),
            "metadata,messages,usage,events".to_string(),
        ];
        for id in ids {
            args.push("--id".to_string());
            args.push(id.clone());
        }
        self.capture("session export", &args)
    }

    fn capture(&self, action: &str, args: &[String]) -> Result<String> {
        let output: Output = Command::new(&self.bin)
            .args(args)
            .output()
            .with_context(|| format!("failed to run `{}`", self.label(args)))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let mut details = stderr.trim().to_string();
            if details.is_empty() {
                details = format!("exit status {}", output.status);
            }
            return Err(anyhow!("recall {action} failed: {details}"));
        }
        String::from_utf8(output.stdout)
            .with_context(|| format!("recall {action} output was not valid UTF-8"))
    }

    fn label(&self, args: &[String]) -> String {
        let mut parts = vec![self.bin.display().to_string()];
        parts.extend(args.iter().cloned());
        parts.join(" ")
    }
}
