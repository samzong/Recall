mod store;

pub(crate) use store::write_seed;
#[cfg(test)]
pub(crate) use store::write_seed_with_hook;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::args;
use crate::catalog::{self, ListedModel};
use crate::config::Paths;
use crate::launch::EnvLookup;

const MIN_CONTEXT: i64 = 100_000;
const MAX_CONTEXT: i64 = 1_000_000;
const OPENAI_COMPACT_WINDOW: i64 = 258_000;
const BASE_TOOL_SEARCH_DENY: &[&str] = &["claude-3-5-haiku", "claude-3-haiku"];

const SETTINGS_CLEAR_ENV: &[(&str, &str)] = &[
    ("ANTHROPIC_API_KEY", ""),
    ("ANTHROPIC_AUTH_TOKEN", ""),
    ("ANTHROPIC_AWS_BASE_URL", ""),
    ("ANTHROPIC_BEDROCK_BASE_URL", ""),
    ("ANTHROPIC_BEDROCK_MANTLE_BASE_URL", ""),
    ("ANTHROPIC_FOUNDRY_BASE_URL", ""),
    ("ANTHROPIC_GOOGLE_CLOUD_BASE_URL", ""),
    ("ANTHROPIC_UNIX_SOCKET", ""),
    ("ANTHROPIC_VERTEX_BASE_URL", ""),
    ("CLAUDE_CODE_OAUTH_TOKEN", ""),
    ("CLAUDE_CODE_USE_ANTHROPIC_AWS", ""),
    ("CLAUDE_CODE_USE_ANTHROPIC_GOOGLE_CLOUD", ""),
    ("CLAUDE_CODE_USE_BEDROCK", ""),
    ("CLAUDE_CODE_USE_FOUNDRY", ""),
    ("CLAUDE_CODE_USE_GATEWAY", ""),
    ("CLAUDE_CODE_USE_MANTLE", ""),
    ("CLAUDE_CODE_USE_VERTEX", ""),
    ("ENABLE_TOOL_SEARCH", "false"),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UserModel {
    pub id: String,
    pub name: Option<String>,
    pub context_length: Option<i64>,
    pub canonical_slug: Option<String>,
    pub pricing: Option<Pricing>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pricing {
    pub prompt: Option<String>,
    pub completion: Option<String>,
    pub input_cache_read: Option<String>,
    pub input_cache_write: Option<String>,
    pub web_search: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ModelOption {
    pub value: String,
    pub label: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ModelAccess {
    pub api_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub(crate) struct SeedCaches {
    #[serde(default)]
    pub provider_id: String,
    pub additional_model_options: Vec<ModelOption>,
    pub model_access: Vec<ModelAccess>,
    pub tool_search_denylist: Vec<String>,
    pub auto_compact_windows: BTreeMap<String, i64>,
    pub additional_model_costs: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SeedOutcome {
    Seeded,
    Fallback,
}

pub(crate) fn try_seed_user_catalog(
    paths: &Paths,
    provider_id: &str,
    base_url: &str,
    api_key: &str,
    env: &EnvLookup,
) -> SeedOutcome {
    let caches = match catalog::load_claude_seed(paths, provider_id, base_url, api_key) {
        Ok(Some(caches)) if !caches.additional_model_options.is_empty() => caches,
        _ => return SeedOutcome::Fallback,
    };
    let config_path = claude_config_path(env);
    match write_seed(&config_path, &caches) {
        Ok(()) => SeedOutcome::Seeded,
        Err(error) => {
            eprintln!("[rx] catalog seed skipped: {error:#}");
            SeedOutcome::Fallback
        }
    }
}

pub(crate) fn seed_from_listed(provider_id: &str, models: &[ListedModel]) -> SeedCaches {
    with_provider(provider_id, build_seed(&from_listed_models(provider_id, models)))
}

pub(crate) fn seed_from_openai_body(
    provider_id: &str,
    body: &str,
    fallback: &[ListedModel],
) -> SeedCaches {
    if let Ok(models) = parse_user_catalog(body)
        && !models.is_empty()
    {
        let fallback_context = catalog::fallback_context(provider_id);
        let models = models
            .into_iter()
            .map(|model| UserModel {
                context_length: Some(model.context_length.unwrap_or(fallback_context)),
                ..model
            })
            .collect::<Vec<_>>();
        let seed = with_provider(provider_id, build_seed(&models));
        if !seed.additional_model_options.is_empty() {
            return seed;
        }
    }
    seed_from_listed(provider_id, fallback)
}

fn with_provider(provider_id: &str, mut caches: SeedCaches) -> SeedCaches {
    caches.provider_id = provider_id.to_string();
    caches
}

fn from_listed_models(provider_id: &str, models: &[ListedModel]) -> Vec<UserModel> {
    models
        .iter()
        .map(|model| UserModel {
            id: model.id.clone(),
            name: model.name.clone(),
            context_length: Some(
                model.context_length.unwrap_or(catalog::fallback_context(provider_id)),
            ),
            canonical_slug: None,
            pricing: None,
        })
        .collect()
}

fn parse_user_catalog(body: &str) -> Result<Vec<UserModel>> {
    let value: Value = serde_json::from_str(body).context("catalog response is not JSON")?;
    let Some(data) = value.get("data").and_then(Value::as_array) else {
        bail!("catalog JSON has no data array");
    };
    Ok(data.iter().filter_map(parse_user_model).collect())
}

fn parse_user_model(entry: &Value) -> Option<UserModel> {
    let ListedModel { id, name, context_length } = catalog::parse_listed_model(entry)?;
    let canonical_slug = entry.get("canonical_slug").and_then(Value::as_str).map(str::to_string);
    let pricing = entry.get("pricing").map(|pricing| {
        let field = |key| pricing.get(key).and_then(Value::as_str).map(str::to_string);
        Pricing {
            prompt: field("prompt"),
            completion: field("completion"),
            input_cache_read: field("input_cache_read"),
            input_cache_write: field("input_cache_write"),
            web_search: field("web_search"),
        }
    });
    Some(UserModel { id, name, context_length, canonical_slug, pricing })
}

pub(crate) fn build_seed(models: &[UserModel]) -> SeedCaches {
    let mut additional_model_options = Vec::new();
    let mut model_access = Vec::new();
    let mut auto_compact_windows = BTreeMap::new();
    let mut additional_model_costs = BTreeMap::new();
    let mut tool_search_denylist = BTreeSet::new();

    for model in models {
        let Some(context) = model.context_length.filter(|value| *value >= MIN_CONTEXT) else {
            continue;
        };
        let capped = context.min(MAX_CONTEXT);
        let stripped = strip_anthropic_prefix(&model.id);
        let picker_value = model_picker_id(model);
        let api_names: Vec<String> = [stripped.clone(), picker_value.clone()]
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        additional_model_options.push(ModelOption {
            value: picker_value.clone(),
            label: model.name.clone().unwrap_or_else(|| stripped.clone()),
            description: context_label(capped),
        });
        let compact_window = auto_compact_window(&stripped, capped);
        for api_name in &api_names {
            auto_compact_windows.insert(api_name.clone(), compact_window);
            model_access.push(ModelAccess { api_name: api_name.clone() });
        }
        if let Some(costs) = model_costs(model) {
            for key in cost_keys(&stripped, &picker_value, model.canonical_slug.as_deref()) {
                additional_model_costs.insert(key, costs.clone());
            }
        }
        if !is_claude_id(&stripped) {
            for api_name in api_names {
                tool_search_denylist.insert(api_name);
            }
        }
    }

    let mut denylist =
        BASE_TOOL_SEARCH_DENY.iter().map(|value| (*value).to_string()).collect::<Vec<_>>();
    denylist.extend(tool_search_denylist);
    denylist.sort();
    denylist.dedup();

    SeedCaches {
        provider_id: String::new(),
        additional_model_options,
        model_access,
        tool_search_denylist: denylist,
        auto_compact_windows,
        additional_model_costs,
    }
}

pub(crate) fn claude_settings_json(base_url: &str, seeded: bool, api_key_env: &str) -> String {
    let mut env = BTreeMap::new();
    for (key, value) in SETTINGS_CLEAR_ENV {
        env.insert((*key).to_string(), (*value).to_string());
    }
    env.insert("ANTHROPIC_BASE_URL".to_string(), catalog::anthropic_base(base_url));
    if seeded {
        env.insert("ENABLE_TOOL_SEARCH".to_string(), "true".to_string());
        env.insert("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY".to_string(), "0".to_string());
        env.insert("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".to_string(), "1".to_string());
        env.insert("CLAUDE_CODE_MAX_CONTEXT_TOKENS".to_string(), MAX_CONTEXT.to_string());
        env.insert("DISABLE_TELEMETRY".to_string(), "1".to_string());
        env.insert("DISABLE_GROWTHBOOK".to_string(), "0".to_string());
        env.insert("CLAUDE_CODE_GB_DISK_CACHE_WHEN_TELEMETRY_OFF".to_string(), "1".to_string());
    }
    serde_json::to_string(&json!({
        "apiKeyHelper": format!("printf %s \"${api_key_env}\""),
        "env": env,
    }))
    .expect("settings json serializes")
}

pub(crate) fn user_passes_settings(passthrough: &[std::ffi::OsString]) -> bool {
    args::has_flags(passthrough, &["--settings", "--setting-sources"])
}

fn claude_config_path(env: &EnvLookup) -> PathBuf {
    if let Some(dir) = env.get("CLAUDE_CONFIG_DIR").filter(|value| !value.trim().is_empty()) {
        PathBuf::from(dir).join(".claude.json")
    } else {
        dirs::home_dir()
            .map(|home| home.join(".claude.json"))
            .unwrap_or_else(|| PathBuf::from(".claude.json"))
    }
}

fn strip_anthropic_prefix(id: &str) -> String {
    id.strip_prefix("anthropic/").unwrap_or(id).to_string()
}

fn is_claude_id(id: &str) -> bool {
    id.starts_with("claude-")
}

fn model_picker_id(model: &UserModel) -> String {
    let stripped = strip_anthropic_prefix(&model.id);
    match model.context_length {
        Some(context) if context > MAX_CONTEXT => format!("{stripped}[1m]"),
        _ => stripped,
    }
}

fn context_label(context: i64) -> String {
    if context == MAX_CONTEXT {
        "1M context".to_string()
    } else {
        format!("{}K context", context.div_euclid(1000))
    }
}

fn auto_compact_window(id: &str, context: i64) -> i64 {
    let window = if id.starts_with("openai/") { OPENAI_COMPACT_WINDOW } else { context };
    context.min(window)
}

fn cost_keys(stripped: &str, picker: &str, canonical: Option<&str>) -> Vec<String> {
    [Some(stripped), Some(picker), canonical]
        .into_iter()
        .flatten()
        .map(str::to_string)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn model_costs(model: &UserModel) -> Option<Value> {
    let pricing = model.pricing.as_ref()?;
    let input = token_cost(&pricing.prompt)?;
    let output = token_cost(&pricing.completion)?;
    let web_search = pricing
        .web_search
        .as_deref()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value >= 0.0)
        .unwrap_or(0.0);
    Some(json!({
        "inputTokens": input,
        "outputTokens": output,
        "promptCacheWriteTokens": token_cost(&pricing.input_cache_write).unwrap_or(input),
        "promptCacheReadTokens": token_cost(&pricing.input_cache_read).unwrap_or(input),
        "webSearchRequests": web_search,
    }))
}

fn token_cost(raw: &Option<String>) -> Option<f64> {
    let value = raw.as_deref()?.trim();
    if value.is_empty() {
        return None;
    }
    let parsed = value.parse::<f64>().ok()?;
    if !parsed.is_finite() || parsed < 0.0 {
        return None;
    }
    Some(((parsed * 1_000_000.0) * 1e12).round() / 1e12)
}
