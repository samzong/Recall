use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde_json::{Value, json};

use crate::launch::{EnvLookup, anthropic_base};

const MIN_CONTEXT: i64 = 100_000;
const MAX_CONTEXT: i64 = 1_000_000;
const OPENAI_COMPACT_WINDOW: i64 = 258_000;
const TOOL_SEARCH_UNSUPPORTED_KEY: &str = "tengu_tool_search_unsupported_models";
const RX_SEEDED_DENYLIST_KEY: &str = "rxSeededToolSearchDenylist";
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelOption {
    pub value: String,
    pub label: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelAccess {
    pub api_name: String,
    pub max_effort_level: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct SeedCaches {
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
    base_url: &str,
    api_key: &str,
    env: &EnvLookup,
) -> Result<SeedOutcome> {
    let models = match fetch_user_catalog(base_url, api_key) {
        Ok(models) if !models.is_empty() => models,
        Ok(_) => return Ok(SeedOutcome::Fallback),
        Err(_) => return Ok(SeedOutcome::Fallback),
    };
    let caches = build_seed(&models);
    if caches.additional_model_options.is_empty() {
        return Ok(SeedOutcome::Fallback);
    }
    let config_path = claude_config_path(env);
    // Seeding is an optional enhancement: any local failure must degrade to
    // Fallback instead of blocking the launch.
    match write_seed(&config_path, &caches) {
        Ok(()) => Ok(SeedOutcome::Seeded { model_count: caches.additional_model_options.len() }),
        Err(error) => {
            eprintln!("[rx] catalog seed skipped: {error:#}");
            Ok(SeedOutcome::Fallback)
        }
    }
}

pub(crate) fn fetch_user_catalog(base_url: &str, api_key: &str) -> Result<Vec<UserModel>> {
    let url = format!("{}/v1/models/user?limit=1000", anthropic_base(base_url));
    let body = crate::catalog::fetch_get(&url, &[("Authorization", format!("Bearer {api_key}"))])?;
    parse_user_catalog(&body)
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
    let name = entry.get("name").and_then(Value::as_str).map(str::to_string);
    let context_length = entry.get("context_length").and_then(Value::as_i64);
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
    env.insert("ANTHROPIC_BASE_URL".to_string(), anthropic_base(base_url));
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
    let previous_rx_deny = string_array(object.get(RX_SEEDED_DENYLIST_KEY));
    let previous_gb_deny = string_array(
        object
            .get("cachedGrowthBookFeatures")
            .and_then(|value| value.get(TOOL_SEARCH_UNSUPPORTED_KEY)),
    );
    // A wrong-typed cache (e.g. null) must be replaced, not unwrapped: this is
    // best-effort seeding and a panic here would block the launch.
    if !object.get("cachedGrowthBookFeatures").is_some_and(Value::is_object) {
        object.insert("cachedGrowthBookFeatures".to_string(), json!({}));
    }
    let growthbook = object
        .get_mut("cachedGrowthBookFeatures")
        .and_then(Value::as_object_mut)
        .expect("normalized cachedGrowthBookFeatures to an object");
    let preserved = previous_gb_deny
        .into_iter()
        .filter(|entry| !previous_rx_deny.contains(entry))
        .collect::<Vec<_>>();
    let mut denylist = BASE_TOOL_SEARCH_DENY
        .iter()
        .map(|value| (*value).to_string())
        .chain(preserved)
        .chain(caches.tool_search_denylist.iter().cloned())
        .collect::<Vec<_>>();
    denylist.sort();
    denylist.dedup();

    growthbook.insert(TOOL_SEARCH_UNSUPPORTED_KEY.to_string(), json!(denylist));
    object.insert(
        "cachedGrowthBookFeaturesAt".to_string(),
        json!(SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis()),
    );
    object.insert(RX_SEEDED_DENYLIST_KEY.to_string(), json!(caches.tool_search_denylist));

    merge_model_options(object, caches);
    merge_model_access(object, caches);
    merge_costs(object, caches);
    merge_compact_windows(object, caches);
}

fn merge_model_options(object: &mut serde_json::Map<String, Value>, caches: &SeedCaches) {
    let existing = model_options(object.get("additionalModelOptionsCache"));
    let existing_values: HashSet<String> = existing
        .iter()
        .filter_map(|entry| entry.get("value").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    let mut merged = existing;
    for option in &caches.additional_model_options {
        if existing_values.contains(&option.value) {
            continue;
        }
        merged.push(json!({
            "value": option.value,
            "label": option.label,
            "description": option.description,
        }));
    }
    object.insert("additionalModelOptionsCache".to_string(), Value::Array(merged));
}

fn merge_model_access(object: &mut serde_json::Map<String, Value>, caches: &SeedCaches) {
    let existing = model_access_entries(object.get("modelAccessCache"));
    let existing_names: HashSet<String> = existing
        .iter()
        .filter_map(|entry| entry.get("apiName").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    let mut merged = existing;
    for access in &caches.model_access {
        if existing_names.contains(&access.api_name) {
            continue;
        }
        let mut entry = json!({ "apiName": access.api_name, "entitled": true });
        if let Some(level) = &access.max_effort_level {
            entry["maxEffortLevel"] = json!(level);
        }
        merged.push(entry);
    }
    object.insert("modelAccessCache".to_string(), Value::Array(merged));
}

fn merge_costs(object: &mut serde_json::Map<String, Value>, caches: &SeedCaches) {
    let mut merged = object
        .get("additionalModelCostsCache")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for (key, value) in &caches.additional_model_costs {
        merged.entry(key.clone()).or_insert_with(|| value.clone());
    }
    object.insert("additionalModelCostsCache".to_string(), Value::Object(merged));
}

fn merge_compact_windows(object: &mut serde_json::Map<String, Value>, caches: &SeedCaches) {
    let mut merged = object
        .get("autoCompactWindowsCache")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for (key, value) in &caches.auto_compact_windows {
        merged.entry(key.clone()).or_insert(json!(value));
    }
    object.insert("autoCompactWindowsCache".to_string(), Value::Object(merged));
}

fn model_options(value: Option<&Value>) -> Vec<Value> {
    value
        .and_then(Value::as_array)
        .map(|items| items.iter().filter(|item| item.get("value").is_some()).cloned().collect())
        .unwrap_or_default()
}

fn model_access_entries(value: Option<&Value>) -> Vec<Value> {
    value
        .and_then(Value::as_array)
        .map(|items| items.iter().filter(|item| item.get("apiName").is_some()).cloned().collect())
        .unwrap_or_default()
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
