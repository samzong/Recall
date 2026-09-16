use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::{BASE_TOOL_SEARCH_DENY, SeedCaches};
use crate::file_io;

const TOOL_SEARCH_UNSUPPORTED_KEY: &str = "tengu_tool_search_unsupported_models";
const RX_SEEDED_DENYLIST_KEY: &str = "rxSeededToolSearchDenylist";
const RX_SEEDED_CATALOG_KEY: &str = "rxSeededCatalog";
const MODEL_OPTIONS_CACHE_KEY: &str = "additionalModelOptionsCache";
const MODEL_ACCESS_CACHE_KEY: &str = "modelAccessCache";
const MODEL_COSTS_CACHE_KEY: &str = "additionalModelCostsCache";
const COMPACT_WINDOWS_CACHE_KEY: &str = "autoCompactWindowsCache";
const TOOL_SEARCH_DENYLIST_MARKER_KEY: &str = "toolSearchDenylist";
const MAX_WRITE_ATTEMPTS: usize = 4;

pub(crate) fn write_seed(path: &Path, caches: &SeedCaches) -> Result<()> {
    write_seed_with_hook(path, caches, |_| {})
}

pub(crate) fn write_seed_with_hook<F>(
    path: &Path,
    caches: &SeedCaches,
    mut after_read: F,
) -> Result<()>
where
    F: FnMut(usize),
{
    let _lock = file_io::lock(&file_io::appended(path, ".rx.lock"))?;
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

fn read_config_document(path: &Path) -> Result<(Value, Option<Vec<u8>>)> {
    match file_io::read_optional(path)? {
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

fn merge_seed(document: &mut Value, caches: &SeedCaches) {
    let object = document.as_object_mut().expect("claude config root is an object");
    let mut catalog_marker =
        object.get(RX_SEEDED_CATALOG_KEY).and_then(Value::as_object).cloned().unwrap_or_default();
    merge_tool_search_denylist(object, caches, &mut catalog_marker);
    reconcile_array_cache(
        object,
        &mut catalog_marker,
        MODEL_OPTIONS_CACHE_KEY,
        "value",
        model_option_values(caches),
    );
    reconcile_array_cache(
        object,
        &mut catalog_marker,
        MODEL_ACCESS_CACHE_KEY,
        "apiName",
        model_access_values(caches),
    );
    reconcile_object_cache(
        object,
        &mut catalog_marker,
        MODEL_COSTS_CACHE_KEY,
        &caches.additional_model_costs,
    );
    merge_compact_windows(object, caches, &mut catalog_marker);
    catalog_marker.insert("provider_id".to_string(), json!(caches.provider_id));
    catalog_marker.insert("version".to_string(), json!(1));
    object.insert(RX_SEEDED_CATALOG_KEY.to_string(), Value::Object(catalog_marker));
}

fn model_option_values(caches: &SeedCaches) -> Vec<Value> {
    caches.additional_model_options.iter().map(|option| json!(option)).collect()
}

fn model_access_values(caches: &SeedCaches) -> Vec<Value> {
    caches
        .model_access
        .iter()
        .map(|access| json!({ "apiName": access.api_name, "entitled": true }))
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
    let desired = BASE_TOOL_SEARCH_DENY
        .iter()
        .map(|identity| (*identity).to_string())
        .chain(caches.tool_search_denylist.iter().cloned());
    let (mut reconciled, next_marker) = reconcile_entries(
        existing.into_iter().map(Value::String).collect(),
        desired.map(Value::String).collect(),
        Some(&Value::Object(previous_marker)),
        Value::as_str,
    );
    reconciled.sort_by(|left, right| left.as_str().cmp(&right.as_str()));
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
    object
        .insert(RX_SEEDED_DENYLIST_KEY.to_string(), json!(next_marker.keys().collect::<Vec<_>>()));
    marker.insert(TOOL_SEARCH_DENYLIST_MARKER_KEY.to_string(), Value::Object(next_marker));
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
    let (entries, next_marker) = reconcile_entries(
        object.get(cache_key).and_then(Value::as_array).cloned().unwrap_or_default(),
        desired,
        marker.get(cache_key),
        |entry| entry.get(identity_key).and_then(Value::as_str),
    );
    object.insert(cache_key.to_string(), Value::Array(entries));
    marker.insert(cache_key.to_string(), Value::Object(next_marker));
}

fn reconcile_entries(
    mut existing: Vec<Value>,
    desired: Vec<Value>,
    marker: Option<&Value>,
    identity: impl Fn(&Value) -> Option<&str>,
) -> (Vec<Value>, serde_json::Map<String, Value>) {
    let managed = marker.and_then(Value::as_object);
    existing.retain(|entry| {
        identity(entry).is_none_or(|id| !managed.is_some_and(|map| map.contains_key(id)))
    });
    let mut occupied: HashSet<String> =
        existing.iter().filter_map(&identity).map(str::to_string).collect();
    let mut next_marker = serde_json::Map::new();
    for entry in desired {
        if let Some(id) = identity(&entry).filter(|id| occupied.insert(id.to_string())) {
            next_marker.insert(id.to_string(), owned_marker(&entry));
            existing.push(entry);
        }
    }
    (existing, next_marker)
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
    let contents =
        serde_json::to_vec_pretty(document).context("failed to serialize .claude.json")?;
    let temp = file_io::stage(path, &contents)?;
    if file_io::read_optional(path)?.as_deref() != expected {
        return Ok(false);
    }
    file_io::persist(temp, path)?;
    Ok(true)
}
