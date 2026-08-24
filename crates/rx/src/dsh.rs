use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_yaml::{Mapping, Value};

use crate::catalog;
use crate::config::Paths;
use crate::launch::{EnvLookup, openai_base};
use crate::provider::{Provider, Setup};

pub(crate) const PROFILE: &str = "dsh-tui";
pub(crate) const CLI_PACKAGE: &str = "@deepseek-ai/dsh";
pub(crate) const PLUGIN_PACKAGE: &str = "@deepseek-harness-tui/dsh-tui";
const OFFICIAL_DEEPSEEK: &str = "https://api.deepseek.com";
const OFFICIAL_DEEPSEEK_ROUTE: &str = "deepseek-official";

pub(crate) fn npm_install_cmd() -> String {
    format!("npm install -g {CLI_PACKAGE} {PLUGIN_PACKAGE}")
}

pub(crate) fn install_hint() -> String {
    format!("{}\n  {}", npm_install_cmd(), profile_hint())
}

pub(crate) fn profile_hint() -> String {
    format!("dsh plugin --profile {PROFILE} add {PLUGIN_PACKAGE}")
}

pub(crate) fn home(env: &EnvLookup) -> Option<PathBuf> {
    if let Some(dir) = env.get("DSH_HOME").filter(|value| !value.trim().is_empty()) {
        return Some(expand_home(dir, env));
    }
    if env.is_real() {
        return dirs::home_dir().map(|home| home.join(".dsh"));
    }
    env.get("HOME")
        .filter(|value| !value.trim().is_empty())
        .map(|home| PathBuf::from(home).join(".dsh"))
}

pub(crate) fn profile_ready(env: &EnvLookup) -> bool {
    let Some(root) = home(env) else {
        return false;
    };
    let path = root.join("profiles").join(PROFILE).join("package.json");
    fs::read_to_string(path).is_ok_and(|body| body.contains(PLUGIN_PACKAGE))
}

pub(crate) fn official_deepseek(provider_id: &str) -> bool {
    provider_id == "deepseek"
}

pub(crate) fn args(passthrough: &[String], patch: Option<&Path>) -> Vec<String> {
    if passthrough.first().is_some_and(|arg| arg == "plugin") {
        return passthrough.to_vec();
    }
    let mut args = Vec::new();
    if passthrough.first().is_some_and(|arg| arg == "web") {
        args.push("web".to_string());
        push_patch(&mut args, patch);
        args.extend(passthrough.iter().skip(1).cloned());
        return args;
    }
    if !has_profile(passthrough) {
        args.push("--profile".to_string());
        args.push(PROFILE.to_string());
    }
    push_patch(&mut args, patch);
    args.extend(passthrough.iter().cloned());
    args
}

pub(crate) fn env_set(provider_id: &str, provider: &Provider, key: &str) -> Vec<(String, String)> {
    let mut env_set = vec![(provider.env.clone(), key.to_string())];
    if official_deepseek(provider_id) && provider.endpoint != OFFICIAL_DEEPSEEK {
        env_set.push(("DEEPSEEK_BASE_URL".to_string(), provider.endpoint.clone()));
    }
    env_set
}

pub(crate) fn prepare(
    provider_id: &str,
    provider: &Provider,
    base_url: &str,
    key: &str,
    model: Option<&str>,
    paths: &Paths,
    env: &EnvLookup,
) -> Result<PathBuf> {
    let model = model.filter(|value| !value.is_empty());
    let models = if official_deepseek(provider_id) {
        Vec::new()
    } else {
        load_models(provider_id, provider, base_url, key, model, paths, env)?
    };
    let dir = paths.dir.join("dsh");
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let settings_path = dir.join("settings.yaml");
    write_settings_overlay(&settings_path, provider_id, provider, base_url, model, &models, env)?;
    let patch_path = dir.join("launch.cordis.yml");
    write_launch_patch(&patch_path, &settings_path, official_deepseek(provider_id))?;
    Ok(patch_path)
}

fn load_models(
    provider_id: &str,
    provider: &Provider,
    base_url: &str,
    key: &str,
    model: Option<&str>,
    paths: &Paths,
    env: &EnvLookup,
) -> Result<Vec<String>> {
    let allow_fetch = env.is_real() || provider.setup == Setup::Generated;
    let mut models = Vec::new();
    match catalog::load_pi_models(paths, provider_id, base_url, key, allow_fetch) {
        Ok(values) => {
            for value in values {
                push_model_id(&mut models, &value);
            }
        }
        Err(error) if provider.setup != Setup::Generated => {
            eprintln!("[rx] model catalog skipped: {error:#}");
        }
        Err(error) => return Err(error),
    }
    if let Some(model) = model.filter(|id| !models.iter().any(|existing| existing == *id)) {
        models.insert(0, model.to_string());
    }
    if models.is_empty() {
        bail!(
            "dsh could not load models for '{provider_id}'; run: rx providers models update {provider_id}"
        );
    }
    Ok(models)
}

fn push_model_id(models: &mut Vec<String>, value: &serde_json::Value) {
    let Some(id) = value.get("id").and_then(serde_json::Value::as_str).filter(|id| !id.is_empty())
    else {
        return;
    };
    if !models.iter().any(|existing| existing == id) {
        models.push(id.to_string());
    }
}

fn write_settings_overlay(
    path: &Path,
    provider_id: &str,
    provider: &Provider,
    base_url: &str,
    model: Option<&str>,
    models: &[String],
    env: &EnvLookup,
) -> Result<()> {
    let mut document = load_user_settings(env)?;
    let root = as_mapping(&mut document)?;
    root.insert("llm-pi-ai".into(), llm_pi_ai_section(provider_id, provider, base_url, models));
    root.insert("agent-default-model".into(), default_model_section(provider_id, model));
    write_yaml_atomic(path, &document)
}

fn load_user_settings(env: &EnvLookup) -> Result<Value> {
    let Some(root) = home(env) else {
        return Ok(Value::Mapping(Mapping::new()));
    };
    let path = root.join("settings.yaml");
    match fs::read_to_string(&path) {
        Ok(body) => serde_yaml::from_str(&body)
            .with_context(|| format!("failed to parse {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(Value::Mapping(Mapping::new()))
        }
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

fn llm_pi_ai_section(
    provider_id: &str,
    provider: &Provider,
    base_url: &str,
    models: &[String],
) -> Value {
    let mut providers = Mapping::new();
    if !official_deepseek(provider_id) && !models.is_empty() {
        let mut route = Mapping::new();
        route.insert("apiKeyEnv".into(), Value::String(provider.env.clone()));
        route.insert("api".into(), Value::String("openai-completions".into()));
        route.insert("baseURL".into(), Value::String(openai_base(base_url)));
        let entries = models
            .iter()
            .map(|id| {
                let mut entry = Mapping::new();
                entry.insert("id".into(), Value::String(id.clone()));
                Value::Mapping(entry)
            })
            .collect();
        route.insert("models".into(), Value::Sequence(entries));
        providers.insert(Value::String(provider_id.to_string()), Value::Mapping(route));
    }
    let mut section = Mapping::new();
    section.insert("providers".into(), Value::Mapping(providers));
    Value::Mapping(section)
}

fn default_model_section(provider_id: &str, model: Option<&str>) -> Value {
    let mut section = Mapping::new();
    let route = if official_deepseek(provider_id) { OFFICIAL_DEEPSEEK_ROUTE } else { provider_id };
    section.insert("provider".into(), Value::String(route.to_string()));
    if let Some(model) = model {
        section.insert("model".into(), Value::String(model.to_string()));
    }
    Value::Mapping(section)
}

fn write_launch_patch(path: &Path, settings_path: &Path, official_deepseek: bool) -> Result<()> {
    let mut body = format!(
        "- id: settings\n  config:\n    path: {}\n",
        yaml_string(&settings_path.display().to_string())
    );
    if !official_deepseek {
        body.push_str("- id: llm-deepseek\n  disabled: true\n");
    }
    write_bytes_atomic(path, body.as_bytes())
}

fn as_mapping(value: &mut Value) -> Result<&mut Mapping> {
    if !value.is_mapping() {
        *value = Value::Mapping(Mapping::new());
    }
    value.as_mapping_mut().context("dsh settings overlay is not a mapping")
}

fn write_yaml_atomic(path: &Path, document: &Value) -> Result<()> {
    let payload =
        serde_yaml::to_string(document).context("failed to serialize dsh settings overlay")?;
    write_bytes_atomic(path, payload.as_bytes())
}

fn write_bytes_atomic(path: &Path, payload: &[u8]) -> Result<()> {
    let parent = path.parent().context("dsh launch file has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("failed to create temporary {}", path.display()))?;
    temp.write_all(payload)
        .with_context(|| format!("failed to write temporary {}", path.display()))?;
    temp.as_file()
        .sync_all()
        .with_context(|| format!("failed to sync temporary {}", path.display()))?;
    temp.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

fn push_patch(args: &mut Vec<String>, patch: Option<&Path>) {
    if let Some(path) = patch {
        args.push("--patch".to_string());
        args.push(path.display().to_string());
    }
}

fn has_profile(args: &[String]) -> bool {
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--" || arg == "web" || arg == "plugin" {
            return false;
        }
        if arg == "--profile" || arg.starts_with("--profile=") {
            return true;
        }
        if arg == "--patch" {
            i += 2;
            continue;
        }
        if arg == "--dump-config" || arg == "--dump-default-config" {
            i += 1;
            continue;
        }
        return false;
    }
    false
}

fn expand_home(dir: String, env: &EnvLookup) -> PathBuf {
    let Some(rest) = dir.strip_prefix("~/") else {
        return PathBuf::from(dir);
    };
    let home = if env.is_real() {
        dirs::home_dir()
    } else {
        env.get("HOME").filter(|value| !value.trim().is_empty()).map(PathBuf::from)
    };
    match home {
        Some(home) => home.join(rest),
        None => PathBuf::from(dir),
    }
}

fn yaml_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_hint_uses_official_npm_then_profile() {
        assert_eq!(
            install_hint(),
            format!("npm install -g {CLI_PACKAGE} {PLUGIN_PACKAGE}\n  {}", profile_hint())
        );
    }

    #[test]
    fn default_args_boot_tui_profile() {
        assert_eq!(args(&[], None), vec!["--profile", PROFILE]);
        assert_eq!(args(&["--resume".to_string()], None), vec!["--profile", PROFILE, "--resume"]);
    }

    #[test]
    fn keeps_explicit_profile_and_web() {
        assert_eq!(
            args(&["--profile".to_string(), "headless".to_string(), "job".to_string()], None),
            vec!["--profile", "headless", "job"]
        );
        assert_eq!(
            args(&["web".to_string(), "--port".to_string(), "8080".to_string()], None),
            vec!["web", "--port", "8080"]
        );
        assert_eq!(
            args(&["plugin".to_string(), "--profile".to_string(), PROFILE.to_string()], None),
            vec!["plugin", "--profile", PROFILE]
        );
    }

    #[test]
    fn patch_follows_web_subcommand() {
        let patch = PathBuf::from("/tmp/rx.cordis.yml");
        assert_eq!(
            args(&["web".to_string()], Some(&patch)),
            vec!["web", "--patch", "/tmp/rx.cordis.yml"]
        );
        assert_eq!(
            args(&[], Some(&patch)),
            vec!["--profile", PROFILE, "--patch", "/tmp/rx.cordis.yml"]
        );
    }
}
