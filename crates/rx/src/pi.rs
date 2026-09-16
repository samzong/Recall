use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::args;
use crate::catalog;
use crate::catalog::openai_base;
use crate::config::Paths;
use crate::file_io;
use crate::launch::EnvLookup;
use crate::opencode;
use crate::provider::{Provider, Setup};

const PI_ENV_CLEAR: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "OPENAI_API_KEY",
    "AZURE_OPENAI_API_KEY",
    "GEMINI_API_KEY",
];

fn global_agent_dir(env: &EnvLookup) -> Result<PathBuf> {
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

pub(crate) fn prepare(
    provider_id: &str,
    provider: &Provider,
    base_url: &str,
    key: &str,
    paths: &Paths,
    env: &EnvLookup,
) -> Result<()> {
    let allow_fetch = env.is_real() || provider.setup == Setup::Generated;
    let document = generated_provider(provider, provider_id, base_url, key, paths, allow_fetch)?;
    if document.get("models").and_then(Value::as_array).is_none_or(|models| models.is_empty()) {
        if provider.setup == Setup::Generated {
            bail!("{} returned no models from {}", provider.name, openai_base(base_url));
        }
        return Ok(());
    }
    let agent_dir = global_agent_dir(env)?;
    merge_provider(&agent_dir.join("models.json"), provider_id, document)
}

pub(crate) fn env_set(env_key: &str, key: &str) -> Vec<(String, String)> {
    let mut env_set = vec![(env_key.to_string(), key.to_string())];
    for name in PI_ENV_CLEAR {
        if *name != env_key {
            env_set.push(((*name).to_string(), String::new()));
        }
    }
    env_set
}

pub(crate) fn args(
    provider_id: &str,
    model: Option<&str>,
    passthrough: &[OsString],
) -> Vec<OsString> {
    let mut args = Vec::new();
    if !args::has_flags(passthrough, &["--models"]) {
        args.push(OsString::from("--models"));
        args.push(OsString::from(format!("{provider_id}/*")));
    }
    if let Some(model) = model.filter(|_| !user_sets_model(passthrough)) {
        args.push(OsString::from("--model"));
        args.push(OsString::from(opencode::prefixed_model(provider_id, model)));
    } else if !args::has_flags(passthrough, &["--provider"]) {
        args.push(OsString::from("--provider"));
        args.push(OsString::from(provider_id));
    }
    args.extend(passthrough.iter().cloned());
    args
}

fn generated_provider(
    provider: &Provider,
    provider_id: &str,
    base_url: &str,
    key: &str,
    paths: &Paths,
    allow_fetch: bool,
) -> Result<Value> {
    let models = match catalog::load_pi_models(paths, provider_id, base_url, key, allow_fetch) {
        Ok(models) => models,
        Err(error) if provider.setup != Setup::Generated => {
            eprintln!("[rx] model catalog skipped: {error:#}");
            Vec::new()
        }
        Err(error) => return Err(error),
    };
    Ok(json!({
        "baseUrl": openai_base(base_url),
        "apiKey": format!("${}", provider.env),
        "api": "openai-responses",
        "authHeader": true,
        "models": models
    }))
}

pub(crate) fn merge_provider(models_path: &Path, provider_id: &str, provider: Value) -> Result<()> {
    let _lock = file_io::lock(&file_io::appended(models_path, ".rx.lock"))?;
    let mut document = if models_path.is_file() {
        let body = fs::read_to_string(models_path)
            .with_context(|| format!("failed to read {}", models_path.display()))?;
        serde_json::from_str(&body).with_context(|| {
            format!("failed to parse {}; fix or remove the file and retry", models_path.display())
        })?
    } else {
        json!({ "providers": {} })
    };
    let Some(root) = document.as_object_mut() else {
        bail!(
            "{} root is not a JSON object; fix or remove the file and retry",
            models_path.display()
        );
    };
    let Some(providers) = root.entry("providers").or_insert_with(|| json!({})).as_object_mut()
    else {
        bail!(
            "{} providers is not a JSON object; fix or remove the file and retry",
            models_path.display()
        );
    };
    providers.insert(provider_id.to_string(), provider);
    file_io::write(models_path, &serde_json::to_vec_pretty(&document)?)
}

fn user_sets_model(passthrough: &[OsString]) -> bool {
    args::before_double_dash(passthrough)
        .iter()
        .any(|arg| arg == "-m" || arg == "--model" || args::os_prefix(arg, "--model="))
}
