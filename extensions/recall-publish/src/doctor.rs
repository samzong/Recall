use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::protocol::RecallClient;
use crate::scan::environment_dir;

pub const PYTHON_FILES: [(&str, &str); 4] = [
    ("pyproject.toml", include_str!("../python/pyproject.toml")),
    ("uv.lock", include_str!("../python/uv.lock")),
    ("scan.py", include_str!("../python/scan.py")),
    ("README.md", include_str!("../python/README.md")),
];

fn tool_version(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().find(|line| !line.trim().is_empty())?;
    Some(line.trim().to_string())
}

pub fn materialize(env_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(env_dir)
        .with_context(|| format!("failed to create {}", env_dir.display()))?;
    for (name, contents) in PYTHON_FILES {
        let path = env_dir.join(name);
        let current = std::fs::read_to_string(&path).unwrap_or_default();
        if current != contents {
            std::fs::write(&path, contents)
                .with_context(|| format!("failed to write {}", path.display()))?;
        }
    }
    Ok(())
}

pub fn install(env_dir: &Path) -> Result<()> {
    materialize(env_dir)?;
    let status = Command::new("uv")
        .arg("sync")
        .arg("--project")
        .arg(env_dir)
        .arg("--frozen")
        .status()
        .context("failed to run uv; install uv first")?;
    if !status.success() {
        bail!("uv sync failed in {}", env_dir.display());
    }
    Ok(())
}

pub fn report(data_dir: &Path, client: &RecallClient) -> Result<Value> {
    let env_dir = environment_dir(data_dir);
    let recall = client.info().ok();
    let environment = crate::pipeline::environment_versions(&env_dir).ok();
    let checks = json!({
        "recall": {
            "ok": recall.is_some(),
            "protocol_version": recall
                .as_ref()
                .and_then(|info| info.get("protocol_version").cloned())
                .unwrap_or(Value::Null),
        },
        "gitleaks": {
            "ok": tool_version("gitleaks", &["version"]).is_some(),
            "version": tool_version("gitleaks", &["version"]),
        },
        "uv": { "ok": tool_version("uv", &["--version"]).is_some(), "version": tool_version("uv", &["--version"]) },

        "redaction_environment": {
            "ok": environment.is_some(),
            "path": env_dir.display().to_string(),
            "packages": environment.clone().unwrap_or(Value::Null),
        },
    });
    let ready = checks
        .as_object()
        .expect("checks is an object")
        .values()
        .all(|check| check["ok"] == Value::Bool(true));
    Ok(json!({ "ready": ready, "checks": checks }))
}
