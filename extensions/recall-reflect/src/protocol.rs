use std::path::PathBuf;
use std::process::{Command, Output};

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

use crate::model::{ReflectFilters, ReflectScopeKind, SourceMessage, SourceSession};

#[derive(Clone, Debug)]
pub struct ReflectArgs {
    pub scope_kind: ReflectScopeKind,
    pub source: Option<String>,
    pub time: Option<String>,
    pub project: Option<String>,
    pub repo: Option<String>,
}

impl Default for ReflectArgs {
    fn default() -> Self {
        Self {
            scope_kind: ReflectScopeKind::Project,
            source: None,
            time: None,
            project: None,
            repo: None,
        }
    }
}

impl ReflectArgs {
    pub fn filters(&self) -> ReflectFilters {
        ReflectFilters {
            scope_kind: self.scope_kind,
            sources: self.source.as_ref().map(|source| vec![source.clone()]),
            time_range: self.time.clone().unwrap_or_else(|| "All".to_string()),
            directory: self.project.clone(),
            repo: self.repo.clone(),
        }
    }
}

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

    pub fn sync(&self, args: &ReflectArgs) -> Result<()> {
        let mut command_args =
            vec!["sync".to_string(), "--project".to_string(), scope_selector(args).to_string()];
        push_optional_arg(&mut command_args, "--source", args.source.as_deref());

        let output = self.run(&command_args)?;
        ensure_success("sync", &command_args, output).map(|_| ())
    }

    pub fn export_sessions(&self, args: &ReflectArgs) -> Result<Vec<SourceSession>> {
        let mut command_args = vec![
            "export".to_string(),
            "--limit".to_string(),
            "0".to_string(),
            "--include".to_string(),
            "metadata,messages".to_string(),
        ];
        command_args.push("--project".to_string());
        command_args.push(scope_selector(args).to_string());
        push_optional_arg(&mut command_args, "--source", args.source.as_deref());
        push_optional_arg(&mut command_args, "--time", args.time.as_deref());

        let output = self.run(&command_args)?;
        let stdout = ensure_success("export", &command_args, output)?;
        parse_jsonl(&stdout)
    }

    fn run(&self, args: &[String]) -> Result<Output> {
        Command::new(&self.bin).args(args).output().with_context(|| {
            format!("failed to spawn Recall command `{}`", command_label(&self.bin, args))
        })
    }
}

/// Recall infers a project scope from the current directory when no selector
/// is given, so reflect always passes one — and passes the same one to sync and
/// export, or it would refresh a different set of sessions than it reports on.
/// `all` is the explicit global scope that personal reflection needs.
fn scope_selector(args: &ReflectArgs) -> &str {
    args.project.as_deref().or(args.repo.as_deref()).unwrap_or("all")
}

fn push_optional_arg(args: &mut Vec<String>, flag: &str, value: Option<&str>) {
    if let Some(value) = value {
        args.push(flag.to_string());
        args.push(value.to_string());
    }
}

fn ensure_success(action: &str, args: &[String], output: Output) -> Result<String> {
    if output.status.success() {
        return String::from_utf8(output.stdout)
            .with_context(|| format!("Recall {action} output was not valid UTF-8"));
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut details = stderr.trim().to_string();
    if details.is_empty() {
        details = stdout.trim().to_string();
    }
    if details.is_empty() {
        details = format!("exit status {}", output.status);
    }

    Err(anyhow!(
        "Recall command failed while running `{}`: {}",
        command_label(PathBuf::from("recall").as_path(), args),
        details
    ))
}

fn command_label(bin: &std::path::Path, args: &[String]) -> String {
    let mut parts = Vec::with_capacity(args.len() + 1);
    parts.push(bin.display().to_string());
    parts.extend(args.iter().cloned());
    parts.join(" ")
}

fn parse_jsonl(stdout: &str) -> Result<Vec<SourceSession>> {
    stdout
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(idx, line)| {
            let record: ExportRecord = serde_json::from_str(line).with_context(|| {
                format!("failed to parse Recall export JSONL record {}", idx + 1)
            })?;
            Ok(record.into_source_session())
        })
        .collect()
}

#[derive(Debug, Deserialize)]
struct ExportRecord {
    session: ExportSession,
    #[serde(default)]
    messages: Vec<ExportMessage>,
}

#[derive(Debug, Deserialize)]
struct ExportSession {
    id: String,
    source: String,
    title: String,
    directory: Option<String>,
    started_at: Option<i64>,
    updated_at: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct ExportMessage {
    seq: u32,
    role: String,
    timestamp: Option<i64>,
    content: String,
}

impl ExportRecord {
    fn into_source_session(self) -> SourceSession {
        SourceSession {
            id: self.session.id,
            source: self.session.source,
            title: self.session.title,
            directory: self.session.directory,
            started_at: self.session.started_at,
            updated_at: self.session.updated_at,
            messages: self.messages.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<ExportMessage> for SourceMessage {
    fn from(message: ExportMessage) -> Self {
        Self {
            role: message.role,
            content: message.content,
            seq: message.seq,
            timestamp: message.timestamp,
        }
    }
}
