use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const UNKNOWN_SCOPE: &str = "Recall could not determine the current repository; make sure Git responds in this directory and retry";
const MAX_SCOPE_LENGTH: usize = 256;
const WORKSPACE_STATE_SCHEMA: &str = "powercontext.codex-workspace.v1";

#[derive(Debug)]
pub struct ScopeError(pub String);

impl std::fmt::Display for ScopeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ScopeError {}

pub struct ResolvedScope {
    pub id: String,
    pub project: String,
}

pub fn resolve_scope(cwd: &Path, scope: &serde_json::Value) -> Result<ResolvedScope> {
    if scope.is_null() {
        return Err(ScopeError(UNKNOWN_SCOPE.into()).into());
    }
    if scope["kind"] == "global" {
        return Err(ScopeError(
            "not a git repository; run backfill from a Git checkout with a network origin".into(),
        )
        .into());
    }
    let project = scope.get("repository")
        .and_then(|repository| repository.get("remote"))
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ScopeError("git remote origin has no core repository identity; configure origin with a host-qualified network repository URL and retry".into()))?;
    let git_dir = git_value(cwd, &["rev-parse", "--absolute-git-dir"])
        .map_err(|_| ScopeError(UNKNOWN_SCOPE.into()))?
        .ok_or_else(|| ScopeError(UNKNOWN_SCOPE.into()))?;
    let id =
        read_bound_scope_id(Path::new(&git_dir)).unwrap_or_else(|| bounded_scope("git", project));
    Ok(ResolvedScope { id, project: project.to_string() })
}

fn read_bound_scope_id(git_dir: &Path) -> Option<String> {
    #[derive(Deserialize)]
    struct WorkspaceState {
        schema: String,
        scope_id: String,
    }

    let bytes = fs::read(git_dir.join("powercontext/codex-workspace.json")).ok()?;
    let state: WorkspaceState = serde_json::from_slice(&bytes).ok()?;
    if state.schema != WORKSPACE_STATE_SCHEMA
        || state.scope_id.is_empty()
        || state.scope_id.trim() != state.scope_id
        || state.scope_id.chars().count() > MAX_SCOPE_LENGTH
    {
        return None;
    }
    Some(state.scope_id)
}

fn bounded_scope(prefix: &str, value: &str) -> String {
    let candidate = format!("{prefix}:{value}");
    if candidate.chars().count() <= MAX_SCOPE_LENGTH {
        return candidate;
    }
    format!("{prefix}:sha256:{:x}", Sha256::digest(value.as_bytes()))
}

fn git_value(cwd: &Path, args: &[&str]) -> Result<Option<String>> {
    let output =
        Command::new("git").args(args).current_dir(cwd).output().context("failed to run git")?;
    if !output.status.success() {
        return Ok(None);
    }
    let value = String::from_utf8(output.stdout)
        .context("git output was not valid UTF-8")?
        .trim()
        .to_string();
    if value.is_empty() { Ok(None) } else { Ok(Some(value)) }
}

pub fn scope_failure(error: &anyhow::Error) -> bool {
    error.downcast_ref::<ScopeError>().is_some()
}

#[cfg(test)]
mod tests {
    use super::bounded_scope;

    #[test]
    fn hashes_overlong_scope_ids() {
        let long = "x".repeat(300);
        let scope = bounded_scope("git", &long);
        assert!(scope.starts_with("git:sha256:"));
        assert!(scope.len() < 256);
    }
}
