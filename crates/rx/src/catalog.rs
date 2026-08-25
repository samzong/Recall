use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::claude_catalog;
use crate::config::Paths;

pub(crate) const DEFAULT_CONTEXT_WINDOW: i64 = 200_000;
const CACHE_TTL: Duration = Duration::from_secs(60 * 60);
const CATALOG_FORMAT: u32 = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ListedModel {
    pub id: String,
    pub name: Option<String>,
    pub context_length: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct CacheMeta {
    fetched_at: u64,
    endpoint: String,
    format: u32,
}

pub(crate) fn anthropic_base(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    trimmed.strip_suffix("/v1").unwrap_or(trimmed).to_string()
}

pub(crate) fn openai_base(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    if has_version_suffix(trimmed) { trimmed.to_string() } else { format!("{trimmed}/v1") }
}

fn has_version_suffix(url: &str) -> bool {
    url.rsplit_once('/').is_some_and(|(_, segment)| {
        let Some(digits) = segment.strip_prefix('v') else {
            return false;
        };
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
    })
}

pub(crate) fn fetch_get(url: &str, headers: &[(&str, String)]) -> Result<String> {
    let (status, body) = fetch(url, headers)?;
    if !(200..300).contains(&status) {
        bail!("HTTP {status}: {}", truncate(&body, 300));
    }
    Ok(body)
}

pub(crate) fn parse_openai_models(body: &str) -> Result<Vec<ListedModel>> {
    let value: Value =
        serde_json::from_str(body).context("provider models response is not JSON")?;
    let Some(rows) = value.get("data").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    Ok(rows.iter().filter_map(parse_listed_model).collect())
}

pub(crate) fn synthesize_codex_catalog(models: &[ListedModel]) -> Value {
    json!({
        "models": models.iter().map(|model| {
            let name = model.name.as_deref().unwrap_or(model.id.as_str());
            let context = model.context_length.unwrap_or(DEFAULT_CONTEXT_WINDOW);
            json!({
                "slug": model.id,
                "display_name": name,
                "description": name,
                "supported_reasoning_levels": [],
                "shell_type": "shell_command",
                "visibility": "list",
                "supported_in_api": true,
                "priority": 1,
                "upgrade": null,
                "support_verbosity": false,
                "default_verbosity": null,
                "apply_patch_tool_type": null,
                "truncation_policy": { "mode": "bytes", "limit": 10000 },
                "context_window": context,
                "max_context_window": context,
                "experimental_supported_tools": [],
                "base_instructions": "",
            })
        }).collect::<Vec<_>>()
    })
}

pub(crate) fn prepare_codex_catalog(
    paths: &Paths,
    provider_id: &str,
    base_url: &str,
    key: &str,
) -> Result<Option<PathBuf>> {
    ensure(paths, provider_id, base_url, key, true)?;
    let path = artifact_path(paths, provider_id, "json");
    if catalog_has_models(&path) { Ok(Some(path)) } else { Ok(None) }
}

pub(crate) fn update_models(
    paths: &Paths,
    provider_id: &str,
    base_url: &str,
    key: &str,
) -> Result<usize> {
    refresh(paths, provider_id, key, &openai_base(base_url))
}

pub(crate) fn load_opencode_models(
    paths: &Paths,
    provider_id: &str,
    base_url: &str,
    key: &str,
    allow_fetch: bool,
) -> Result<BTreeMap<String, Value>> {
    ensure(paths, provider_id, base_url, key, allow_fetch)?;
    read_json_object(artifact_path(paths, provider_id, "opencode.json"))
}

pub(crate) fn load_pi_models(
    paths: &Paths,
    provider_id: &str,
    base_url: &str,
    key: &str,
    allow_fetch: bool,
) -> Result<Vec<Value>> {
    ensure(paths, provider_id, base_url, key, allow_fetch)?;
    read_json_array(artifact_path(paths, provider_id, "pi.json"))
}

pub(crate) fn load_claude_seed(
    paths: &Paths,
    provider_id: &str,
    base_url: &str,
    key: &str,
) -> Result<Option<claude_catalog::SeedCaches>> {
    ensure(paths, provider_id, base_url, key, true)?;
    let path = artifact_path(paths, provider_id, "claude.json");
    if !path.is_file() {
        return Ok(None);
    }
    let body =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let caches = serde_json::from_str(&body)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(Some(caches))
}

fn ensure(
    paths: &Paths,
    provider_id: &str,
    base_url: &str,
    key: &str,
    allow_fetch: bool,
) -> Result<()> {
    let endpoint = openai_base(base_url);
    if is_fresh(paths, provider_id, &endpoint) {
        return Ok(());
    }
    if !allow_fetch {
        return Ok(());
    }
    match refresh(paths, provider_id, key, &endpoint) {
        Ok(_) => Ok(()),
        Err(error) if has_stale_catalog(paths, provider_id, &endpoint) => {
            eprintln!("[rx] catalog refresh failed; using cached models: {error:#}");
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn refresh(paths: &Paths, provider_id: &str, key: &str, endpoint: &str) -> Result<usize> {
    let url = format!("{endpoint}/models");
    let body = match fetch_get(&url, &[("Authorization", format!("Bearer {key}"))]) {
        Ok(body) => body,
        Err(error) => bail!("{provider_id}: {error:#}"),
    };
    let models = fill_missing_context(provider_id, &parse_openai_models(&body)?);
    if models.is_empty() {
        bail!("{} returned no models from {endpoint}", provider_id);
    }
    write_artifacts(paths, provider_id, &models, &body)?;
    write_json_atomic(
        &artifact_path(paths, provider_id, "meta.json"),
        &serde_json::to_value(CacheMeta {
            fetched_at: now_secs(),
            endpoint: endpoint.to_string(),
            format: CATALOG_FORMAT,
        })
        .context("failed to serialize catalog meta")?,
    )?;
    Ok(models.len())
}

fn write_artifacts(
    paths: &Paths,
    provider_id: &str,
    models: &[ListedModel],
    body: &str,
) -> Result<()> {
    let dir = catalogs_dir(paths);
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let files = [
        (artifact_path(paths, provider_id, "json"), synthesize_codex_catalog(models)),
        (
            artifact_path(paths, provider_id, "claude.json"),
            serde_json::to_value(claude_catalog::seed_from_openai_body(provider_id, body, models))
                .context("failed to serialize claude catalog")?,
        ),
        (
            artifact_path(paths, provider_id, "opencode.json"),
            Value::Object(
                models
                    .iter()
                    .map(|model| (model.id.clone(), json!({ "name": model.id })))
                    .collect(),
            ),
        ),
        (
            artifact_path(paths, provider_id, "pi.json"),
            Value::Array(models.iter().map(|model| json!({ "id": model.id })).collect()),
        ),
    ];
    let mut staged = Vec::with_capacity(files.len());
    for (path, document) in files {
        let temp = stage_json(&path, &document)?;
        staged.push((path, temp));
    }
    for (path, temp) in staged {
        persist_json(temp, &path)?;
    }
    Ok(())
}

fn is_fresh(paths: &Paths, provider_id: &str, endpoint: &str) -> bool {
    let Some(meta) = complete_cache_meta(paths, provider_id, endpoint) else {
        return false;
    };
    now_secs().saturating_sub(meta.fetched_at) < CACHE_TTL.as_secs()
}

fn has_stale_catalog(paths: &Paths, provider_id: &str, endpoint: &str) -> bool {
    complete_cache_meta(paths, provider_id, endpoint).is_some()
        && catalog_has_models(&artifact_path(paths, provider_id, "json"))
}

fn complete_cache_meta(paths: &Paths, provider_id: &str, endpoint: &str) -> Option<CacheMeta> {
    let meta = read_meta(paths, provider_id)?;
    if meta.endpoint != endpoint || meta.format != CATALOG_FORMAT {
        return None;
    }
    ["json", "claude.json", "opencode.json", "pi.json"]
        .into_iter()
        .all(|suffix| artifact_path(paths, provider_id, suffix).is_file())
        .then_some(meta)
}

fn read_meta(paths: &Paths, provider_id: &str) -> Option<CacheMeta> {
    let body = fs::read_to_string(artifact_path(paths, provider_id, "meta.json")).ok()?;
    serde_json::from_str(&body).ok()
}

fn catalog_has_models(path: &Path) -> bool {
    let Ok(body) = fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<Value>(&body) else {
        return false;
    };
    value.get("models").and_then(Value::as_array).is_some_and(|models| !models.is_empty())
}

fn read_json_object(path: PathBuf) -> Result<BTreeMap<String, Value>> {
    if !path.is_file() {
        return Ok(BTreeMap::new());
    }
    let body =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&body).with_context(|| format!("failed to parse {}", path.display()))
}

fn read_json_array(path: PathBuf) -> Result<Vec<Value>> {
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let body =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&body).with_context(|| format!("failed to parse {}", path.display()))
}

fn catalogs_dir(paths: &Paths) -> PathBuf {
    paths.dir.join("catalogs")
}

fn artifact_path(paths: &Paths, provider_id: &str, suffix: &str) -> PathBuf {
    if suffix == "json" {
        catalogs_dir(paths).join(format!("{provider_id}.json"))
    } else {
        catalogs_dir(paths).join(format!("{provider_id}.{suffix}"))
    }
}

fn write_json_atomic(path: &Path, document: &Value) -> Result<()> {
    persist_json(stage_json(path, document)?, path)
}

fn stage_json(path: &Path, document: &Value) -> Result<tempfile::NamedTempFile> {
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
    Ok(temp)
}

fn persist_json(temp: tempfile::NamedTempFile, path: &Path) -> Result<()> {
    temp.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

pub(crate) fn fallback_context(provider_id: &str) -> i64 {
    crate::provider::find(provider_id)
        .and_then(|provider| provider.default_context)
        .unwrap_or(DEFAULT_CONTEXT_WINDOW)
}

fn fill_missing_context(provider_id: &str, models: &[ListedModel]) -> Vec<ListedModel> {
    let fallback = fallback_context(provider_id);
    models
        .iter()
        .map(|model| ListedModel {
            id: model.id.clone(),
            name: model.name.clone(),
            context_length: Some(model.context_length.unwrap_or(fallback)),
        })
        .collect()
}

fn parse_listed_model(row: &Value) -> Option<ListedModel> {
    let id = row.get("id")?.as_str()?.to_string();
    let name = row
        .get("name")
        .or_else(|| row.get("display_name"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let context_length =
        row.get("context_length").or_else(|| row.get("max_input_tokens")).and_then(Value::as_i64);
    Some(ListedModel { id, name, context_length })
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn fetch(url: &str, headers: &[(&str, String)]) -> Result<(u16, String)> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .http_status_as_error(false)
        .build()
        .into();
    let mut request = agent.get(url);
    if !headers.iter().any(|(name, _)| name.eq_ignore_ascii_case("User-Agent")) {
        request = request.header("User-Agent", format!("rx/{}", crate::RELEASE_VERSION));
    }
    for (name, value) in headers {
        request = request.header(*name, value);
    }
    let mut response = request.call().with_context(|| format!("GET {url}"))?;
    let status = response.status().as_u16();
    let body = response.body_mut().read_to_string().context("failed to read response body")?;
    Ok((status, body))
}

fn truncate(value: &str, max: usize) -> String {
    let trimmed = value.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    let cut: String = trimmed.chars().take(max).collect();
    format!("{cut}...")
}
