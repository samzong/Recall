use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_yaml::{Mapping, Value};

use crate::catalog;
use crate::config::Paths;
use crate::launch::{EnvLookup, openai_base};
use crate::provider::{ModelProtocol, Provider, ReasoningControl, Setup};

pub(crate) const PROFILE: &str = "dsh-tui";
pub(crate) const CLI_PACKAGE: &str = "@deepseek-ai/dsh";
pub(crate) const PLUGIN_PACKAGE: &str = "@deepseek-harness-tui/dsh-tui";
pub(crate) const PLUGIN_SPEC: &str = "@deepseek-harness-tui/dsh-tui@latest";
const OFFICIAL_DEEPSEEK: &str = "https://api.deepseek.com";
const OFFICIAL_DEEPSEEK_ROUTE: &str = "deepseek-official";

#[derive(Debug, Clone, PartialEq, Eq)]
struct DshModel {
    id: String,
    reasoning: Option<ReasoningControl>,
}

struct RouteContext<'a> {
    provider_id: &'a str,
    provider: &'a Provider,
    base_url: &'a str,
    protocol: ModelProtocol,
    env: &'a EnvLookup,
}

pub(crate) fn npm_install_cmd() -> String {
    format!("npm install -g --legacy-peer-deps {CLI_PACKAGE} {PLUGIN_PACKAGE}")
}

pub(crate) fn install_hint() -> String {
    format!("{}\n  {}", npm_install_cmd(), profile_hint())
}

pub(crate) fn profile_hint() -> String {
    format!("dsh plugin --profile {PROFILE} add -w --ignore-scripts {PLUGIN_SPEC}")
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
    let profile = root.join("profiles").join(PROFILE);
    let Ok(body) = fs::read_to_string(profile.join("package.json")) else {
        return false;
    };
    let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&body) else {
        return false;
    };
    let dependency = manifest["dependencies"].get(PLUGIN_PACKAGE).is_some();
    let bundle = manifest["dsh"]["profile"]["bundles"]
        .as_array()
        .is_some_and(|bundles| bundles.iter().any(|bundle| bundle == PLUGIN_PACKAGE));
    let package = profile.join("node_modules").join(PLUGIN_PACKAGE).join("package.json");
    let installed = fs::read_to_string(package)
        .ok()
        .and_then(|body| serde_json::from_str::<serde_json::Value>(&body).ok())
        .is_some_and(|manifest| manifest["name"] == PLUGIN_PACKAGE);
    dependency && bundle && installed
}

pub(crate) fn official_deepseek(provider_id: &str) -> bool {
    provider_id == "deepseek"
}

pub(crate) fn args(passthrough: &[OsString], patch: Option<&Path>) -> Vec<OsString> {
    if passthrough.first().is_some_and(|arg| arg == "plugin") {
        return passthrough.to_vec();
    }
    let mut args = Vec::new();
    if passthrough.first().is_some_and(|arg| arg == "web") {
        args.push(OsString::from("web"));
        push_patch(&mut args, patch);
        args.extend(passthrough.iter().skip(1).cloned());
        return args;
    }
    if !has_profile(passthrough) {
        args.push(OsString::from("--profile"));
        args.push(OsString::from(PROFILE));
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
    let protocol = crate::provider::dsh_protocol(provider_id, base_url);
    let context = RouteContext { provider_id, provider, base_url, protocol, env };
    let models = if official_deepseek(provider_id) {
        Vec::new()
    } else {
        load_models(&context, key, model, paths)?
    };
    let dir = paths.dir.join("dsh");
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let settings_path = dir.join("settings.yaml");
    write_settings_overlay(&settings_path, &context, model, &models)?;
    let patch_path = dir.join("launch.cordis.yml");
    write_launch_patch(&patch_path, &settings_path, official_deepseek(provider_id))?;
    Ok(patch_path)
}

fn load_models(
    context: &RouteContext<'_>,
    key: &str,
    model: Option<&str>,
    paths: &Paths,
) -> Result<Vec<DshModel>> {
    let allow_fetch = context.env.is_real() || context.provider.setup == Setup::Generated;
    let mut models = Vec::new();
    match catalog::load_pi_models(paths, context.provider_id, context.base_url, key, allow_fetch) {
        Ok(values) => {
            for value in values {
                push_model_id(&mut models, &value);
            }
        }
        Err(error) if context.provider.setup != Setup::Generated => {
            eprintln!("[rx] model catalog skipped: {error:#}");
        }
        Err(error) => return Err(error),
    }
    if let Some(model) = model.filter(|id| !models.iter().any(|existing| existing == *id)) {
        models.insert(0, model.to_string());
    }
    if models.is_empty() {
        bail!(
            "dsh could not load models for '{}'; run: rx providers models update {}",
            context.provider_id,
            context.provider_id
        );
    }
    Ok(models
        .into_iter()
        .map(|id| DshModel {
            reasoning: crate::provider::reasoning_control(
                context.provider_id,
                context.base_url,
                &id,
                context.protocol,
            )
            .cloned(),
            id,
        })
        .collect())
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
    context: &RouteContext<'_>,
    model: Option<&str>,
    models: &[DshModel],
) -> Result<()> {
    let mut document = load_user_settings(context.env)?;
    let root = as_mapping(&mut document)?;
    root.insert("llm-pi-ai".into(), llm_pi_ai_section(context, models));
    root.insert("agent-default-model".into(), default_model_section(context.provider_id, model));
    if crate::launch::yolo_enabled(context.env) {
        let mut permission = Mapping::new();
        permission.insert("defaultPreset".into(), Value::String("danger-full-access".into()));
        root.insert("permission".into(), Value::Mapping(permission));
    }
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

fn llm_pi_ai_section(context: &RouteContext<'_>, models: &[DshModel]) -> Value {
    let mut providers = Mapping::new();
    if !official_deepseek(context.provider_id) && !models.is_empty() {
        let mut route = Mapping::new();
        route.insert("apiKeyEnv".into(), Value::String(context.provider.env.clone()));
        route.insert("api".into(), Value::String(context.protocol.as_str().to_string()));
        route.insert("baseURL".into(), Value::String(openai_base(context.base_url)));
        let entries = models
            .iter()
            .map(|model| {
                let mut entry = Mapping::new();
                entry.insert("id".into(), Value::String(model.id.clone()));
                if let Some(reasoning) = &model.reasoning {
                    entry.insert("reasoningEfforts".into(), reasoning_efforts(reasoning));
                }
                Value::Mapping(entry)
            })
            .collect();
        route.insert("models".into(), Value::Sequence(entries));
        providers.insert(Value::String(context.provider_id.to_string()), Value::Mapping(route));
    }
    let mut section = Mapping::new();
    section.insert("providers".into(), Value::Mapping(providers));
    Value::Mapping(section)
}

fn reasoning_efforts(control: &ReasoningControl) -> Value {
    match control {
        ReasoningControl::Fixed => Value::Bool(false),
        ReasoningControl::Effort { levels } => Value::Mapping(
            levels
                .iter()
                .map(|(level, wire)| {
                    (
                        Value::String(level.as_str().to_string()),
                        wire.as_ref().map_or(Value::Null, |wire| Value::String(wire.clone())),
                    )
                })
                .collect(),
        ),
    }
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

fn push_patch(args: &mut Vec<OsString>, patch: Option<&Path>) {
    if let Some(path) = patch {
        args.push(OsString::from("--patch"));
        args.push(path.as_os_str().to_os_string());
    }
}

fn has_profile(args: &[OsString]) -> bool {
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--" || arg == "web" || arg == "plugin" {
            return false;
        }
        if arg == "--profile" || crate::args::os_prefix(arg, "--profile=") {
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
            format!(
                "npm install -g --legacy-peer-deps {CLI_PACKAGE} {PLUGIN_PACKAGE}\n  {}",
                profile_hint()
            )
        );
    }

    fn os(argv: &[&str]) -> Vec<OsString> {
        argv.iter().map(|arg| OsString::from(*arg)).collect()
    }

    #[test]
    fn default_args_boot_tui_profile() {
        assert_eq!(args(&[], None), os(&["--profile", PROFILE]));
        assert_eq!(args(&os(&["--resume"]), None), os(&["--profile", PROFILE, "--resume"]));
    }

    #[test]
    fn keeps_explicit_profile_and_web() {
        assert_eq!(
            args(&os(&["--profile", "headless", "job"]), None),
            os(&["--profile", "headless", "job"])
        );
        assert_eq!(args(&os(&["web", "--port", "8080"]), None), os(&["web", "--port", "8080"]));
        assert_eq!(
            args(&os(&["plugin", "--profile", PROFILE]), None),
            os(&["plugin", "--profile", PROFILE])
        );
    }

    #[test]
    fn patch_follows_web_subcommand() {
        let patch = PathBuf::from("/tmp/rx.cordis.yml");
        assert_eq!(
            args(&os(&["web"]), Some(&patch)),
            os(&["web", "--patch", "/tmp/rx.cordis.yml"])
        );
        assert_eq!(
            args(&[], Some(&patch)),
            os(&["--profile", PROFILE, "--patch", "/tmp/rx.cordis.yml"])
        );
    }

    #[test]
    fn profile_readiness_requires_activated_installed_bundle() {
        let root = tempfile::tempdir().unwrap();
        let profile = root.path().join("profiles").join(PROFILE);
        let package = profile.join("node_modules").join(PLUGIN_PACKAGE);
        fs::create_dir_all(&package).unwrap();
        fs::write(package.join("package.json"), format!(r#"{{"name":"{PLUGIN_PACKAGE}"}}"#))
            .unwrap();
        fs::write(
            profile.join("package.json"),
            format!(
                r#"{{"dependencies":{{"{PLUGIN_PACKAGE}":"^0.9.3"}},"dsh":{{"profile":{{"bundles":["@deepseek-ai/dsh-base"]}}}}}}"#
            ),
        )
        .unwrap();
        let env = EnvLookup::isolated(std::collections::HashMap::from([(
            "DSH_HOME".to_string(),
            root.path().display().to_string(),
        )]));
        assert!(!profile_ready(&env));
        fs::write(
            profile.join("package.json"),
            format!(
                r#"{{"dependencies":{{"{PLUGIN_PACKAGE}":"^0.9.3"}},"dsh":{{"profile":{{"bundles":["@deepseek-ai/dsh-base","{PLUGIN_PACKAGE}"]}}}}}}"#
            ),
        )
        .unwrap();
        assert!(profile_ready(&env));
    }

    #[test]
    fn tokener_reasoning_capabilities_project_to_dsh_models() {
        let provider = crate::provider::find("tokener").unwrap();
        let protocol = crate::provider::dsh_protocol("tokener", &provider.endpoint);
        let env = EnvLookup::isolated(std::collections::HashMap::new());
        let context = RouteContext {
            provider_id: "tokener",
            provider,
            base_url: &provider.endpoint,
            protocol,
            env: &env,
        };
        assert_eq!(protocol, ModelProtocol::OpenAiResponses);
        let models = ["gpt-5.6-sol", "glm-5.2", "kimi-k3"]
            .into_iter()
            .map(|id| DshModel {
                id: id.to_string(),
                reasoning: crate::provider::reasoning_control(
                    "tokener",
                    &provider.endpoint,
                    id,
                    protocol,
                )
                .cloned(),
            })
            .collect::<Vec<_>>();
        let section = llm_pi_ai_section(&context, &models);
        assert_eq!(section["providers"]["tokener"]["api"], "openai-responses");
        let entries = section["providers"]["tokener"]["models"].as_sequence().unwrap();
        assert_eq!(entries[0]["reasoningEfforts"]["off"], "none");
        assert_eq!(entries[0]["reasoningEfforts"]["low"], "low");
        assert_eq!(entries[0]["reasoningEfforts"]["medium"], "medium");
        assert_eq!(entries[0]["reasoningEfforts"]["high"], "high");
        assert_eq!(entries[0]["reasoningEfforts"]["xhigh"], "xhigh");
        assert_eq!(entries[0]["reasoningEfforts"]["max"], "max");
        assert_eq!(entries[1]["reasoningEfforts"]["off"], "none");
        assert_eq!(entries[1]["reasoningEfforts"]["low"], "low");
        assert_eq!(entries[1]["reasoningEfforts"]["medium"], "medium");
        assert!(entries[1]["reasoningEfforts"].get("high").is_none());
        assert!(entries[2].get("reasoningEfforts").is_none());
        assert_eq!(
            crate::provider::dsh_protocol("tokener", "https://proxy.example.com/v1"),
            ModelProtocol::OpenAiCompletions
        );
        assert!(
            crate::provider::reasoning_control(
                "tokener",
                "https://proxy.example.com/v1",
                "gpt-5.6-sol",
                ModelProtocol::OpenAiResponses,
            )
            .is_none()
        );
    }
}
