use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::catalog;
use crate::config::Paths;
use crate::launch::{EnvLookup, openai_base};
use crate::provider::{Provider, Setup};

pub(crate) fn config_content(
    provider_id: &str,
    provider: &Provider,
    base_url: &str,
    key: &str,
    paths: &Paths,
    allow_fetch: bool,
) -> Result<String> {
    let models = match catalog::load_opencode_models(paths, provider_id, base_url, key, allow_fetch)
    {
        Ok(models) => models,
        Err(error) if provider.setup != Setup::Generated => {
            eprintln!("[rx] model catalog skipped: {error:#}");
            Default::default()
        }
        Err(error) => return Err(error),
    };
    let mut entry = json!({
        "name": provider.name,
        "options": {
            "baseURL": openai_base(base_url),
            "apiKey": format!("{{env:{}}}", provider.env)
        }
    });
    if provider.setup == Setup::Generated {
        entry["npm"] = json!("@ai-sdk/openai-compatible");
    }
    if !models.is_empty() {
        entry["models"] = json!(models);
    }
    serde_json::to_string(&json!({ "provider": { provider_id: entry } }))
        .context("failed to serialize OPENCODE_CONFIG_CONTENT")
}

pub(crate) fn auth_conflict_note(
    provider: &Provider,
    key: &str,
    env: &EnvLookup,
) -> Option<String> {
    if provider.setup != Setup::OpenRouter {
        return None;
    }
    let path = opencode_auth_path(env).ok()?;
    let stored = fs::read_to_string(&path).ok()?;
    let document: Value = serde_json::from_str(&stored).ok()?;
    let stored_key = document.get("openrouter").and_then(|entry| {
        entry
            .get("key")
            .and_then(Value::as_str)
            .or_else(|| entry.get("apiKey").and_then(Value::as_str))
    });
    if stored_key.is_some_and(|stored_key| stored_key != key) {
        Some(format!(
            "[rx] opencode stored OpenRouter credential at {} differs from rx provider key; remove or update it to avoid auth conflicts",
            path.display()
        ))
    } else {
        None
    }
}

pub(crate) fn prefixed_model(provider_id: &str, model: &str) -> String {
    let prefix = format!("{provider_id}/");
    if model.starts_with(&prefix) { model.to_string() } else { format!("{prefix}{model}") }
}

fn opencode_auth_path(env: &EnvLookup) -> Result<PathBuf> {
    if let Some(dir) = env.get("XDG_DATA_HOME").filter(|value| !value.trim().is_empty()) {
        return Ok(PathBuf::from(dir).join("opencode").join("auth.json"));
    }
    if !env.is_real() {
        bail!("XDG_DATA_HOME is required in an isolated environment");
    }
    let home = dirs::home_dir().context("cannot determine home directory")?;
    Ok(home.join(".local").join("share").join("opencode").join("auth.json"))
}
