use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde_json::{Value, json};

use crate::args;
use crate::catalog;
use crate::config::Paths;
use crate::launch::{EnvLookup, openai_base};
use crate::opencode;
use crate::provider::{Provider, Setup};

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
    let recall_dir = recall_pi_dir(paths);
    fs::create_dir_all(&recall_dir)
        .with_context(|| format!("failed to create {}", recall_dir.display()))?;
    write_json_atomic(&recall_dir.join(format!("{provider_id}-provider.json")), &document)?;
    let agent_dir = global_agent_dir(env)?;
    fs::create_dir_all(&agent_dir)
        .with_context(|| format!("failed to create {}", agent_dir.display()))?;
    merge_provider(&agent_dir.join("models.json"), provider_id, document)
}

pub(crate) fn env_set(env_key: &str, key: &str) -> Vec<(String, String)> {
    let mut env_set = vec![(env_key.to_string(), key.to_string())];
    for name in PI_ENV_CLEAR {
        env_set.push(((*name).to_string(), String::new()));
    }
    env_set
}

pub(crate) fn args(provider_id: &str, model: Option<&str>, passthrough: &[String]) -> Vec<String> {
    let mut args = Vec::new();
    if !user_sets_models_flag(passthrough) {
        args.push("--models".to_string());
        args.push(format!("{provider_id}/*"));
    }
    if let Some(model) = model.filter(|_| !user_sets_model(passthrough)) {
        args.push("--model".to_string());
        args.push(opencode::prefixed_model(provider_id, model));
    } else if !user_sets_provider(passthrough) {
        args.push("--provider".to_string());
        args.push(provider_id.to_string());
    }
    args.extend(passthrough.iter().cloned());
    args
}

fn user_sets_models_flag(passthrough: &[String]) -> bool {
    args::before_double_dash(passthrough)
        .iter()
        .any(|arg| arg == "--models" || arg.starts_with("--models="))
}

fn user_sets_provider(passthrough: &[String]) -> bool {
    args::before_double_dash(passthrough)
        .iter()
        .any(|arg| arg == "--provider" || arg.starts_with("--provider="))
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
    let _lock = exclusive_sidecar(models_path)?;
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
    if let Some(providers) = root.get_mut("providers").and_then(Value::as_object_mut) {
        providers.insert(provider_id.to_string(), provider);
    } else {
        root.insert("providers".to_string(), json!({ provider_id: provider }));
    }
    write_json_atomic(models_path, &document)
}

fn exclusive_sidecar(path: &Path) -> Result<fs::File> {
    let parent = path.parent().context("json file has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let mut lock_path = path.as_os_str().to_os_string();
    lock_path.push(".rx.lock");
    let lock_path = PathBuf::from(lock_path);
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("failed to open {}", lock_path.display()))?;
    lock.lock_exclusive().with_context(|| format!("failed to lock {}", lock_path.display()))?;
    Ok(lock)
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
    args::before_double_dash(passthrough)
        .iter()
        .any(|arg| arg == "-m" || arg == "--model" || arg.starts_with("--model="))
}
