use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::select::now_rfc3339;
use crate::workspace::{RECEIPT_FILE, Receipt, Workspace, digest_of};

pub fn repo_url(repo: &str) -> String {
    format!("https://huggingface.co/datasets/{repo}")
}

pub fn hf(env_dir: &Path) -> Command {
    let mut command = Command::new("uv");
    command.arg("run").arg("--project").arg(env_dir).arg("--quiet").arg("hf");
    command
}

pub fn upload(workspace: &Workspace, env_dir: &Path, repo: &str, private: bool) -> Result<Value> {
    let approval = workspace.verify_against_approval()?;
    let mut command = hf(env_dir);
    command.arg("upload").arg(repo).arg(&workspace.root).arg(".").arg("--repo-type").arg("dataset");
    if private {
        command.arg("--private");
    }
    for name in approval.digests.keys() {
        command.arg("--include").arg(name);
    }
    for (name, _) in workspace.local_only_paths() {
        command.arg("--exclude").arg(name);
    }
    let output =
        command.output().context("failed to run hf; run `recall publish doctor --install`")?;
    if !output.status.success() {
        bail!("hf upload failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }

    verify_remote(env_dir, repo, &approval.digests)?;

    let receipt = Receipt {
        scope: workspace.scope.clone(),
        repo: repo.to_string(),
        url: repo_url(repo),
        uploaded_at: now_rfc3339(),
        digests: approval.digests,
    };
    let value = serde_json::to_value(&receipt)?;
    workspace.write_json(RECEIPT_FILE, &value)?;
    Ok(value)
}

fn verify_remote(
    env_dir: &Path,
    repo: &str,
    expected: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    let dir = tempfile::tempdir().context("failed to create a verification directory")?;
    for name in expected.keys() {
        let output = hf(env_dir)
            .arg("download")
            .arg(repo)
            .arg(name)
            .arg("--repo-type")
            .arg("dataset")
            .arg("--local-dir")
            .arg(dir.path())
            .output()
            .context("failed to download the published files for verification")?;
        if !output.status.success() {
            bail!(
                "could not read back `{name}` from {}: {}",
                repo_url(repo),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let bytes = std::fs::read(dir.path().join(name))
            .with_context(|| format!("`{name}` was not downloaded"))?;
        let actual = digest_of(&bytes);
        if Some(&actual) != expected.get(name) {
            bail!("`{name}` on {} does not match the approved bytes", repo_url(repo));
        }
    }
    Ok(())
}

pub fn dry_run(workspace: &Workspace, repo: &str) -> Result<Value> {
    let approval = workspace.verify_against_approval()?;
    Ok(json!({
        "scope": workspace.scope,
        "repo": repo,
        "url": repo_url(repo),
        "approved_at": approval.approved_at,
        "files": approval.digests,
        "excluded": workspace
            .local_only_paths()
            .into_iter()
            .map(|(name, _)| name)
            .collect::<Vec<String>>(),
    }))
}
