use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::catalog;
use crate::launch::{EnvLookup, openai_base};
use crate::provider::{Provider, Setup};

pub(crate) fn config_content(
    provider_id: &str,
    provider: &Provider,
    base_url: &str,
    key: &str,
) -> Result<String> {
    let value = match provider.setup {
        Setup::OpenRouter => json!({
            "provider": {
                provider_id: {
                    "options": {
                        "baseURL": openai_base(base_url),
                        "apiKey": format!("{{env:{}}}", provider.env)
                    }
                }
            }
        }),
        Setup::Generated => {
            let models = fetch_model_map(base_url, key)?;
            json!({
                "provider": {
                    provider_id: {
                        "npm": "@ai-sdk/openai-compatible",
                        "name": provider.name,
                        "options": {
                            "baseURL": openai_base(base_url),
                            "apiKey": format!("{{env:{}}}", provider.env)
                        },
                        "models": models
                    }
                }
            })
        }
    };
    serde_json::to_string(&value).context("failed to serialize OPENCODE_CONFIG_CONTENT")
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

pub(crate) fn fetch_model_map(base_url: &str, key: &str) -> Result<BTreeMap<String, Value>> {
    let url = format!("{}/models", openai_base(base_url));
    let body = catalog::fetch_get(&url, &[("Authorization", format!("Bearer {key}"))])?;
    let payload: Value =
        serde_json::from_str(&body).context("provider models response is not JSON")?;
    let Some(rows) = payload.get("data").and_then(Value::as_array) else {
        return Ok(BTreeMap::new());
    };
    Ok(rows
        .iter()
        .filter_map(|row| row.get("id").and_then(Value::as_str))
        .map(|id| (id.to_string(), json!({ "name": id })))
        .collect())
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
