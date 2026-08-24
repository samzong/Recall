use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::catalog::{self, ListedModel};
use crate::config::Paths;
use crate::launch::EnvLookup;

const MIN_CONTEXT: i64 = 100_000;
const MAX_CONTEXT: i64 = 1_000_000;
const OPENAI_COMPACT_WINDOW: i64 = 258_000;
const TOOL_SEARCH_UNSUPPORTED_KEY: &str = "tengu_tool_search_unsupported_models";
const RX_SEEDED_DENYLIST_KEY: &str = "rxSeededToolSearchDenylist";
const RX_SEEDED_CATALOG_KEY: &str = "rxSeededCatalog";
const MODEL_OPTIONS_CACHE_KEY: &str = "additionalModelOptionsCache";
const MODEL_ACCESS_CACHE_KEY: &str = "modelAccessCache";
const MODEL_COSTS_CACHE_KEY: &str = "additionalModelCostsCache";
const COMPACT_WINDOWS_CACHE_KEY: &str = "autoCompactWindowsCache";
const TOOL_SEARCH_DENYLIST_MARKER_KEY: &str = "toolSearchDenylist";
const MAX_WRITE_ATTEMPTS: usize = 4;

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
    pub supported_efforts: Vec<String>,
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
    pub max_effort_level: Option<String>,
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
    Seeded { model_count: usize },
    Fallback,
}

pub(crate) fn try_seed_user_catalog(
    paths: &Paths,
    provider_id: &str,
    base_url: &str,
    api_key: &str,
    env: &EnvLookup,
) -> Result<SeedOutcome> {
    let caches = match catalog::load_claude_seed(paths, provider_id, base_url, api_key) {
        Ok(Some(caches)) if !caches.additional_model_options.is_empty() => caches,
        _ => return Ok(SeedOutcome::Fallback),
    };
    let config_path = claude_config_path(env);
    match write_seed(&config_path, &caches) {
        Ok(()) => Ok(SeedOutcome::Seeded { model_count: caches.additional_model_options.len() }),
        Err(error) => {
            eprintln!("[rx] catalog seed skipped: {error:#}");
            Ok(SeedOutcome::Fallback)
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
        let models = apply_fallback_context(provider_id, models);
        let seed = with_provider(provider_id, build_seed(&models));
        if !seed.additional_model_options.is_empty() {
            return seed;
        }
    }
    seed_from_listed(provider_id, fallback)
}

fn apply_fallback_context(provider_id: &str, models: Vec<UserModel>) -> Vec<UserModel> {
    let fallback = catalog::fallback_context(provider_id);
    models
        .into_iter()
        .map(|model| UserModel {
            context_length: Some(model.context_length.unwrap_or(fallback)),
            ..model
        })
        .collect()
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
            supported_efforts: Vec::new(),
            pricing: None,
        })
        .collect()
}

pub(crate) fn parse_user_catalog(body: &str) -> Result<Vec<UserModel>> {
    let value: Value = serde_json::from_str(body).context("catalog response is not JSON")?;
    parse_user_catalog_value(&value)
}

pub(crate) fn parse_user_catalog_value(value: &Value) -> Result<Vec<UserModel>> {
    let Some(data) = value.get("data").and_then(Value::as_array) else {
        bail!("catalog JSON has no data array");
    };
    Ok(data.iter().filter_map(parse_user_model).collect())
}

fn parse_user_model(entry: &Value) -> Option<UserModel> {
    let id = entry.get("id")?.as_str()?.to_string();
    let name = entry
        .get("name")
        .or_else(|| entry.get("display_name"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let context_length = entry
        .get("context_length")
        .or_else(|| entry.get("max_input_tokens"))
        .and_then(Value::as_i64);
    let canonical_slug = entry.get("canonical_slug").and_then(Value::as_str).map(str::to_string);
    let supported_efforts = entry
        .get("reasoning")
        .and_then(|value| value.get("supported_efforts"))
        .and_then(Value::as_array)
        .map(|items| {
            items.iter().filter_map(|item| item.as_str().map(str::to_string)).collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let pricing = entry.get("pricing").map(|pricing| Pricing {
        prompt: pricing.get("prompt").and_then(Value::as_str).map(str::to_string),
        completion: pricing.get("completion").and_then(Value::as_str).map(str::to_string),
        input_cache_read: pricing
            .get("input_cache_read")
            .and_then(Value::as_str)
            .map(str::to_string),
        input_cache_write: pricing
            .get("input_cache_write")
            .and_then(Value::as_str)
            .map(str::to_string),
        web_search: pricing.get("web_search").and_then(Value::as_str).map(str::to_string),
    });
    Some(UserModel { id, name, context_length, canonical_slug, supported_efforts, pricing })
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
        let max_effort = max_effort_level(&model.supported_efforts);
        for api_name in &api_names {
            auto_compact_windows.insert(api_name.clone(), compact_window);
            model_access.push(ModelAccess {
                api_name: api_name.clone(),
                max_effort_level: max_effort.clone(),
            });
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

pub(crate) fn user_passes_settings(passthrough: &[String]) -> bool {
    passthrough.iter().any(|arg| {
        arg == "--settings"
            || arg == "--setting-sources"
            || arg.starts_with("--settings=")
            || arg.starts_with("--setting-sources=")
    })
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

fn write_seed(path: &Path, caches: &SeedCaches) -> Result<()> {
    write_seed_with_hook(path, caches, |_| {})
}

fn write_seed_with_hook<F>(path: &Path, caches: &SeedCaches, mut after_read: F) -> Result<()>
where
    F: FnMut(usize),
{
    let parent =
        path.parent().ok_or_else(|| anyhow::anyhow!("path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let mut lock_path = path.as_os_str().to_os_string();
    lock_path.push(".rx.lock");
    let lock_path = PathBuf::from(lock_path);
    // The stable sidecar must outlive each lock holder. Removing it after
    // unlock could split existing waiters and new writers across two inodes.
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("failed to open {}", lock_path.display()))?;
    lock.lock_exclusive().with_context(|| format!("failed to lock {}", lock_path.display()))?;
    for attempt in 0..MAX_WRITE_ATTEMPTS {
        let (mut document, snapshot) = read_config_document(path)?;
        after_read(attempt);
        merge_seed(&mut document, caches);
        if write_config_document(path, &document, snapshot.as_deref())? {
            return Ok(());
        }
    }
    bail!("{} changed repeatedly while seeding catalog", path.display())
}

#[cfg(test)]
pub(crate) fn write_seed_for_test(path: &Path, caches: &SeedCaches) -> Result<()> {
    write_seed(path, caches)
}

#[cfg(test)]
pub(crate) fn write_seed_with_hook_for_test<F>(
    path: &Path,
    caches: &SeedCaches,
    after_read: F,
) -> Result<()>
where
    F: FnMut(usize),
{
    write_seed_with_hook(path, caches, after_read)
}

fn read_config_document(path: &Path) -> Result<(Value, Option<Vec<u8>>)> {
    match read_config_bytes(path)? {
        Some(contents) => {
            let value: Value = serde_json::from_slice(&contents)
                .with_context(|| format!("failed to parse {}", path.display()))?;
            if !value.is_object() {
                bail!("{} root is not a JSON object", path.display());
            }
            Ok((value, Some(contents)))
        }
        None => Ok((json!({}), None)),
    }
}

fn read_config_bytes(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

fn merge_seed(document: &mut Value, caches: &SeedCaches) {
    let object = document.as_object_mut().expect("claude config root is an object");
    let mut catalog_marker =
        object.get(RX_SEEDED_CATALOG_KEY).and_then(Value::as_object).cloned().unwrap_or_default();
    merge_tool_search_denylist(object, caches, &mut catalog_marker);
    merge_model_options(object, caches, &mut catalog_marker);
    merge_model_access(object, caches, &mut catalog_marker);
    merge_costs(object, caches, &mut catalog_marker);
    merge_compact_windows(object, caches, &mut catalog_marker);
    catalog_marker.insert("provider_id".to_string(), json!(caches.provider_id));
    catalog_marker.insert("version".to_string(), json!(1));
    object.insert(RX_SEEDED_CATALOG_KEY.to_string(), Value::Object(catalog_marker));
}

fn model_option_values(caches: &SeedCaches) -> Vec<Value> {
    caches
        .additional_model_options
        .iter()
        .map(|option| {
            json!({
                "value": option.value,
                "label": option.label,
                "description": option.description,
            })
        })
        .collect()
}

fn model_access_values(caches: &SeedCaches) -> Vec<Value> {
    caches
        .model_access
        .iter()
        .map(|access| {
            let mut entry = json!({ "apiName": access.api_name, "entitled": true });
            if let Some(level) = &access.max_effort_level {
                entry["maxEffortLevel"] = json!(level);
            }
            entry
        })
        .collect()
}

fn merge_tool_search_denylist(
    object: &mut serde_json::Map<String, Value>,
    caches: &SeedCaches,
    marker: &mut serde_json::Map<String, Value>,
) {
    let previous_marker = marker
        .get(TOOL_SEARCH_DENYLIST_MARKER_KEY)
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_else(|| {
            string_array(object.get(RX_SEEDED_DENYLIST_KEY))
                .into_iter()
                .map(|identity| {
                    let payload = Value::String(identity.clone());
                    (identity, owned_marker(&payload))
                })
                .collect()
        });
    let existing = string_array(
        object
            .get("cachedGrowthBookFeatures")
            .and_then(|value| value.get(TOOL_SEARCH_UNSUPPORTED_KEY)),
    );
    let managed: HashSet<_> = previous_marker.keys().cloned().collect();
    let mut occupied = HashSet::new();
    let mut reconciled = Vec::new();
    let mut next_marker = serde_json::Map::new();
    for identity in existing {
        if managed.contains(&identity) {
            continue;
        }
        occupied.insert(identity.clone());
        reconciled.push(identity);
    }

    let desired = BASE_TOOL_SEARCH_DENY
        .iter()
        .map(|identity| (*identity).to_string())
        .chain(caches.tool_search_denylist.iter().cloned());
    for identity in desired {
        if occupied.insert(identity.clone()) {
            let payload = Value::String(identity.clone());
            next_marker.insert(identity.clone(), owned_marker(&payload));
            reconciled.push(identity);
        }
    }
    reconciled.sort();
    reconciled.dedup();

    if !object.get("cachedGrowthBookFeatures").is_some_and(Value::is_object) {
        object.insert("cachedGrowthBookFeatures".to_string(), json!({}));
    }
    object
        .get_mut("cachedGrowthBookFeatures")
        .and_then(Value::as_object_mut)
        .expect("normalized cachedGrowthBookFeatures to an object")
        .insert(TOOL_SEARCH_UNSUPPORTED_KEY.to_string(), json!(reconciled));
    object.insert(
        "cachedGrowthBookFeaturesAt".to_string(),
        json!(SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis()),
    );
    let mut legacy_owned = next_marker.keys().cloned().collect::<Vec<_>>();
    legacy_owned.sort();
    object.insert(RX_SEEDED_DENYLIST_KEY.to_string(), json!(legacy_owned));
    marker.insert(TOOL_SEARCH_DENYLIST_MARKER_KEY.to_string(), Value::Object(next_marker));
}

fn merge_model_options(
    object: &mut serde_json::Map<String, Value>,
    caches: &SeedCaches,
    marker: &mut serde_json::Map<String, Value>,
) {
    reconcile_array_cache(
        object,
        marker,
        MODEL_OPTIONS_CACHE_KEY,
        "value",
        model_option_values(caches),
    );
}

fn merge_model_access(
    object: &mut serde_json::Map<String, Value>,
    caches: &SeedCaches,
    marker: &mut serde_json::Map<String, Value>,
) {
    reconcile_array_cache(
        object,
        marker,
        MODEL_ACCESS_CACHE_KEY,
        "apiName",
        model_access_values(caches),
    );
}

fn merge_costs(
    object: &mut serde_json::Map<String, Value>,
    caches: &SeedCaches,
    marker: &mut serde_json::Map<String, Value>,
) {
    reconcile_object_cache(object, marker, MODEL_COSTS_CACHE_KEY, &caches.additional_model_costs);
}

fn merge_compact_windows(
    object: &mut serde_json::Map<String, Value>,
    caches: &SeedCaches,
    marker: &mut serde_json::Map<String, Value>,
) {
    let desired = caches
        .auto_compact_windows
        .iter()
        .map(|(key, value)| (key.clone(), json!(value)))
        .collect();
    reconcile_object_cache(object, marker, COMPACT_WINDOWS_CACHE_KEY, &desired);
}

fn reconcile_array_cache(
    object: &mut serde_json::Map<String, Value>,
    marker: &mut serde_json::Map<String, Value>,
    cache_key: &str,
    identity_key: &str,
    desired: Vec<Value>,
) {
    let previous_marker =
        marker.get(cache_key).and_then(Value::as_object).cloned().unwrap_or_default();
    let existing = object.get(cache_key).and_then(Value::as_array).cloned().unwrap_or_default();
    let managed: HashSet<_> = previous_marker.keys().cloned().collect();
    let mut occupied = HashSet::new();
    let mut reconciled = Vec::new();
    let mut next_marker = serde_json::Map::new();
    for entry in existing {
        let Some(identity) = entry.get(identity_key).and_then(Value::as_str).map(str::to_string)
        else {
            reconciled.push(entry);
            continue;
        };
        if managed.contains(&identity) {
            continue;
        }
        occupied.insert(identity);
        reconciled.push(entry);
    }

    for entry in desired {
        let Some(identity) = entry.get(identity_key).and_then(Value::as_str).map(str::to_string)
        else {
            continue;
        };
        if occupied.insert(identity.clone()) {
            next_marker.insert(identity, owned_marker(&entry));
            reconciled.push(entry);
        }
    }

    object.insert(cache_key.to_string(), Value::Array(reconciled));
    marker.insert(cache_key.to_string(), Value::Object(next_marker));
}

fn reconcile_object_cache(
    object: &mut serde_json::Map<String, Value>,
    marker: &mut serde_json::Map<String, Value>,
    cache_key: &str,
    desired: &BTreeMap<String, Value>,
) {
    let previous_marker =
        marker.get(cache_key).and_then(Value::as_object).cloned().unwrap_or_default();
    let mut reconciled =
        object.get(cache_key).and_then(Value::as_object).cloned().unwrap_or_default();
    let mut next_marker = serde_json::Map::new();
    for key in previous_marker.keys() {
        reconciled.remove(key);
    }

    for (key, payload) in desired {
        if !reconciled.contains_key(key) {
            reconciled.insert(key.clone(), payload.clone());
            next_marker.insert(key.clone(), owned_marker(payload));
        }
    }

    object.insert(cache_key.to_string(), Value::Object(reconciled));
    marker.insert(cache_key.to_string(), Value::Object(next_marker));
}

fn owned_marker(payload: &Value) -> Value {
    json!({ "state": "owned", "payload": payload })
}

fn string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items.iter().filter_map(|item| item.as_str().map(str::to_string)).collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn write_config_document(path: &Path, document: &Value, expected: Option<&[u8]>) -> Result<bool> {
    let parent =
        path.parent().ok_or_else(|| anyhow::anyhow!("path has no parent: {}", path.display()))?;
    let contents =
        serde_json::to_string_pretty(document).context("failed to serialize .claude.json")?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("failed to create temporary file in {}", parent.display()))?;
    temp.write_all(contents.as_bytes())
        .with_context(|| format!("failed to write temporary file in {}", parent.display()))?;
    temp.as_file()
        .sync_all()
        .with_context(|| format!("failed to sync temporary file in {}", parent.display()))?;
    if read_config_bytes(path)?.as_deref() != expected {
        return Ok(false);
    }
    temp.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(true)
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

fn max_effort_level(supported: &[String]) -> Option<String> {
    const LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
    let supported: HashSet<&str> = supported.iter().map(String::as_str).collect();
    LEVELS.iter().rev().find(|level| supported.contains(*level)).map(|level| (*level).to_string())
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
