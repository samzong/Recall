use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};

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

pub fn resolve_scope(cwd: &Path) -> Result<ResolvedScope> {
    let git_dir = git_value(cwd, &["rev-parse", "--absolute-git-dir"])?
        .ok_or_else(|| ScopeError("not a git repository; run from a repo with origin".into()))?;
    let root = git_value(cwd, &["rev-parse", "--show-toplevel"])?
        .ok_or_else(|| ScopeError("not a git repository; run from a repo with origin".into()))?;
    let remote = git_value(Path::new(&root), &["config", "--get", "remote.origin.url"])?
        .ok_or_else(|| {
            ScopeError("git remote origin is missing; refusing local:sha256 scope".into())
        })?;
    let project = normalize_git_remote(&remote)
        .ok_or_else(|| ScopeError(format!("git remote origin is not a network URL: {remote}")))?;
    let id =
        read_bound_scope_id(Path::new(&git_dir)).unwrap_or_else(|| bounded_scope("git", &project));
    Ok(ResolvedScope { id, project })
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

pub fn normalize_git_remote(remote: &str) -> Option<String> {
    let value = remote.trim();
    if value.is_empty() {
        return None;
    }
    if !value.contains("://") {
        return normalize_scp_remote(value);
    }
    let (scheme, rest) = value.split_once("://")?;
    if !matches!(scheme, "http" | "https" | "ssh" | "git") {
        return None;
    }
    let rest = rest.trim_start_matches('/');
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let authority = authority.rsplit('@').next()?.trim();
    if authority.is_empty() {
        return None;
    }
    let host = authority.to_ascii_lowercase();
    let path = normalize_path(path);
    if path.is_empty() {
        return None;
    }
    Some(format!("{host}/{path}"))
}

fn normalize_scp_remote(value: &str) -> Option<String> {
    let rest = match value.split_once('@') {
        Some((user, rest)) if !user.is_empty() && !user.contains('/') => rest,
        None => value,
        Some(_) => return None,
    };
    let (host, path) = rest.split_once(':')?;
    if host.is_empty() || host.contains('/') {
        return None;
    }
    let path = normalize_path(path);
    if path.is_empty() {
        return None;
    }
    Some(format!("{}/{path}", host.to_ascii_lowercase()))
}

fn normalize_path(path: &str) -> String {
    let mut normalized = path
        .replace('\\', "/")
        .split('/')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("/");
    if let Some(stripped) = normalized.strip_suffix(".git") {
        normalized = stripped.to_string();
    }
    normalized.trim_end_matches('/').to_string()
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
    use super::{bounded_scope, normalize_git_remote};

    #[test]
    fn normalizes_scp_and_https_remotes() {
        assert_eq!(
            normalize_git_remote("git@github.com:samzong/Recall.git").as_deref(),
            Some("github.com/samzong/Recall")
        );
        assert_eq!(
            normalize_git_remote("https://USER:token@GitHub.com/samzong/Recall.git").as_deref(),
            Some("github.com/samzong/Recall")
        );
        assert_eq!(
            normalize_git_remote("ssh://git@github.com/samzong/Recall.git").as_deref(),
            Some("github.com/samzong/Recall")
        );
        assert_eq!(
            normalize_git_remote("https://github.com:443/samzong/Recall.git").as_deref(),
            Some("github.com:443/samzong/Recall")
        );
    }

    #[test]
    fn rejects_file_and_empty_remotes() {
        assert_eq!(normalize_git_remote("file:///tmp/repo.git"), None);
        assert_eq!(normalize_git_remote(""), None);
        assert_eq!(normalize_git_remote("   "), None);
    }

    #[test]
    fn hashes_overlong_scope_ids() {
        let long = "x".repeat(300);
        let scope = bounded_scope("git", &long);
        assert!(scope.starts_with("git:sha256:"));
        assert!(scope.len() < 256);
    }
}
