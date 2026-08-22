use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::config::Paths;
use crate::launch::{EnvLookup, ProviderSpec, openai_base};
use crate::opencode;

const PI_ENV_CLEAR: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "OPENAI_API_KEY",
    "AZURE_OPENAI_API_KEY",
    "GEMINI_API_KEY",
];

pub(crate) fn global_agent_dir(env: &EnvLookup) -> Result<PathBuf> {
    // PI_CODING_AGENT_DIR is authoritative for pi's configuration location
    // (see src/adapters/pi.rs); providers must land where the launched
    // process will actually read them.
    if let Some(dir) = env
        .get("PI_CODING_AGENT_DIR")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        let expanded = match dir.strip_prefix("~/") {
            Some(rest) => {
                let home = if env.is_real() {
                    dirs::home_dir()
                } else {
                    env.get("HOME").filter(|value| !value.trim().is_empty()).map(PathBuf::from)
                }
                .context("cannot expand PI_CODING_AGENT_DIR without a home directory")?;
                home.join(rest)
            }
            None => PathBuf::from(dir),
        };
        return Ok(expanded);
    }
    if !env.is_real() {
        bail!("PI_CODING_AGENT_DIR is required in an isolated environment");
    }
    let home = dirs::home_dir().context("cannot determine home directory")?;
    Ok(home.join(".pi").join("agent"))
}

pub(crate) fn recall_pi_dir(paths: &Paths) -> PathBuf {
    paths.dir.join("pi")
}

pub(crate) fn prepare(
    spec: &ProviderSpec,
    base_url: &str,
    key: &str,
    paths: &Paths,
    env: &EnvLookup,
) -> Result<()> {
    if spec.id != "tokener" {
        return Ok(());
    }
    let provider = tokener_provider(spec, base_url, key)?;
    let recall_dir = recall_pi_dir(paths);
    fs::create_dir_all(&recall_dir)
        .with_context(|| format!("failed to create {}", recall_dir.display()))?;
    write_json_atomic(&recall_dir.join("tokener-provider.json"), &provider)?;
    let agent_dir = global_agent_dir(env)?;
    fs::create_dir_all(&agent_dir)
        .with_context(|| format!("failed to create {}", agent_dir.display()))?;
    merge_provider(&agent_dir.join("models.json"), "tokener", provider)
}

pub(crate) fn env_set(spec: &ProviderSpec, key: &str) -> Vec<(String, String)> {
    let mut env_set = vec![(spec.env_key.to_string(), key.to_string())];
    for name in PI_ENV_CLEAR {
        env_set.push(((*name).to_string(), String::new()));
    }
    env_set
}

pub(crate) fn args(
    spec: &ProviderSpec,
    model: Option<&str>,
    passthrough: &[String],
) -> Vec<String> {
    let mut args = Vec::new();
    if !user_sets_models_flag(passthrough) {
        args.push("--models".to_string());
        args.push(format!("{}/*", spec.id));
    }
    if let Some(model) = model.filter(|_| !user_sets_model(passthrough)) {
        args.push("--model".to_string());
        args.push(opencode::prefixed_model(spec.id, model));
    } else if !user_sets_provider(passthrough) {
        args.push("--provider".to_string());
        args.push(spec.id.to_string());
    }
    args.extend(passthrough.iter().cloned());
    args
}

fn user_sets_models_flag(passthrough: &[String]) -> bool {
    passthrough.iter().any(|arg| arg == "--models" || arg.starts_with("--models="))
}

fn user_sets_provider(passthrough: &[String]) -> bool {
    passthrough.iter().any(|arg| arg == "--provider" || arg.starts_with("--provider="))
}

fn tokener_provider(spec: &ProviderSpec, base_url: &str, key: &str) -> Result<Value> {
    let models: Vec<Value> =
        opencode::fetch_model_map(base_url, key)?.keys().map(|id| json!({ "id": id })).collect();
    if models.is_empty() {
        bail!("tokener returned no models from {}", openai_base(base_url));
    }
    Ok(json!({
        "baseUrl": openai_base(base_url),
        "apiKey": format!("${}", spec.env_key),
        "api": "openai-responses",
        "authHeader": true,
        "models": models
    }))
}

pub(crate) fn merge_provider(models_path: &Path, provider_id: &str, provider: Value) -> Result<()> {
    let mut document = if models_path.is_file() {
        let body = fs::read_to_string(models_path)
            .with_context(|| format!("failed to read {}", models_path.display()))?;
        serde_json::from_str(&body).with_context(|| {
            format!("failed to parse {}; fix or remove the file and retry", models_path.display())
        })?
    } else {
        json!({ "providers": {} })
    };
    let Some(providers) = document.get_mut("providers").and_then(Value::as_object_mut) else {
        document["providers"] = json!({});
        document["providers"][provider_id] = provider;
        return write_json_atomic(models_path, &document);
    };
    providers.insert(provider_id.to_string(), provider);
    write_json_atomic(models_path, &document)
}

fn write_json_atomic(path: &Path, document: &Value) -> Result<()> {
    let parent = path.parent().context("json file has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let payload = serde_json::to_string_pretty(document).context("failed to serialize json")?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("failed to create temporary {}", path.display()))?;
    temp.write_all(payload.as_bytes())
        .with_context(|| format!("failed to write temporary {}", path.display()))?;
    temp.as_file()
        .sync_all()
        .with_context(|| format!("failed to sync temporary {}", path.display()))?;
    temp.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

fn user_sets_model(passthrough: &[String]) -> bool {
    passthrough.iter().any(|arg| arg == "-m" || arg == "--model" || arg.starts_with("--model="))
}
