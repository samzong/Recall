use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, IsTerminal, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use toml_edit::{DocumentMut, Item, Table, value};

use crate::catalog::{self, ListedModel};
use crate::file_io::{self, appended as appended_path, read_optional as read_bytes};
use crate::launch::ProviderTarget;

const MARKER_VERSION: u32 = 1;
const LEASE_VERSION: u32 = 2;
const MAX_WRITE_ATTEMPTS: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnedCatalog {
    version: u32,
    provider: OwnedProvider,
    models: BTreeMap<String, OwnedModel>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
enum CatalogMarker {
    Leased { version: u32, catalogs: BTreeMap<String, OwnedCatalog> },
    Legacy(OwnedCatalog),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct OwnedProvider {
    alias: String,
    base_url: String,
    api_key_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct OwnedModel {
    provider: String,
    model: String,
    max_context_size: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,
}

pub(super) fn seed_catalog(
    config_path: &Path,
    provider_alias: &str,
    target: &ProviderTarget,
    models: &[ListedModel],
    allow_prompt: bool,
) -> Result<File> {
    let parent = config_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("path has no parent: {}", config_path.display()))?;
    let _lock = file_io::lock(&appended_path(config_path, ".rx.lock"))?;
    let marker_path = appended_path(config_path, ".rx-catalog.json");
    let desired = desired_catalog(provider_alias, target, models);
    let lease_name = catalog_lease_name(&desired)?;
    let mut migration_approved = false;
    for _ in 0..MAX_WRITE_ATTEMPTS {
        let snapshot = read_bytes(config_path)?;
        let mut document = read_document(config_path, snapshot.as_deref())?;
        let mut previous = Vec::new();
        let mut active = BTreeMap::new();
        match read_marker(&marker_path)? {
            Some(CatalogMarker::Legacy(catalog)) => {
                validate_catalog(&catalog)?;
                if !migration_approved {
                    confirm_migration(allow_prompt)?;
                    migration_approved = true;
                }
                previous.push(catalog);
            }
            Some(CatalogMarker::Leased { version, catalogs }) => {
                if version != LEASE_VERSION {
                    bail!("unsupported Kimi catalog marker version {version}");
                }
                for (name, catalog) in catalogs {
                    validate_catalog(&catalog)?;
                    if name != catalog_lease_name(&catalog)? {
                        bail!("invalid Kimi catalog lease identity");
                    }
                    if super::lease::is_active(&parent.join(&name))? {
                        active.insert(name, catalog.clone());
                    }
                    previous.push(catalog);
                }
            }
            None => {}
        }
        reconcile(&mut document, &previous, &active, &desired, &target.key)?;
        let lease_path = parent.join(&lease_name);
        let lease = super::lease::acquire(&lease_path)?;
        let staged_config = stage_secret(config_path, document.to_string().as_bytes())?;
        active.insert(lease_name.clone(), desired.clone());
        let marker = serde_json::to_vec_pretty(&CatalogMarker::Leased {
            version: LEASE_VERSION,
            catalogs: active,
        })
        .context("failed to serialize Kimi marker")?;
        let staged_marker = stage_secret(&marker_path, &marker)?;
        if read_bytes(config_path)?.as_deref() != snapshot.as_deref() {
            continue;
        }
        file_io::persist(staged_config, config_path)?;
        file_io::persist(staged_marker, &marker_path)?;
        return Ok(lease);
    }
    bail!("{} changed repeatedly while seeding Kimi catalog", config_path.display())
}

fn catalog_lease_name(catalog: &OwnedCatalog) -> Result<String> {
    Ok(format!(".rx-kimi-{:x}", Sha256::digest(serde_json::to_vec(catalog)?)))
}

fn confirm_migration(allow_prompt: bool) -> Result<()> {
    let instruction = "close all Kimi sessions started by older rx, then rerun this command in a terminal and type 'migrate' to confirm";
    if !allow_prompt || !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        bail!("Kimi catalog migration required: {instruction}");
    }
    eprint!(
        "[rx] Kimi catalog migration: all Kimi sessions started by older rx must be closed. Type 'migrate' to confirm they have exited: "
    );
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    if answer.trim() != "migrate" {
        bail!("Kimi catalog migration declined: {instruction}");
    }
    Ok(())
}

fn validate_catalog(catalog: &OwnedCatalog) -> Result<()> {
    if catalog.version != MARKER_VERSION {
        bail!("unsupported Kimi catalog marker version {}", catalog.version);
    }
    if !catalog.provider.alias.starts_with("rx-")
        || catalog.models.iter().any(|(alias, model)| {
            model.provider != catalog.provider.alias
                || *alias != format!("{}/{}", catalog.provider.alias, model.model)
        })
    {
        bail!("invalid Kimi catalog ownership record");
    }
    Ok(())
}

fn desired_catalog(
    provider_alias: &str,
    target: &ProviderTarget,
    models: &[ListedModel],
) -> OwnedCatalog {
    let provider = OwnedProvider {
        alias: provider_alias.to_string(),
        base_url: catalog::openai_base(&target.provider.endpoint),
        api_key_sha256: key_hash(&target.key),
    };
    let models = models
        .iter()
        .map(|model| {
            let alias = format!("{provider_alias}/{}", model.id);
            let entry = OwnedModel {
                provider: provider_alias.to_string(),
                model: model.id.clone(),
                max_context_size: model
                    .context_length
                    .unwrap_or_else(|| catalog::fallback_context(&target.provider.id)),
                display_name: model.name.clone(),
            };
            (alias, entry)
        })
        .collect();
    OwnedCatalog { version: MARKER_VERSION, provider, models }
}

fn reconcile(
    document: &mut DocumentMut,
    previous: &[OwnedCatalog],
    active: &BTreeMap<String, OwnedCatalog>,
    desired: &OwnedCatalog,
    key: &str,
) -> Result<()> {
    for catalog in active.values() {
        if catalog.provider.alias == desired.provider.alias && catalog.provider != desired.provider
        {
            bail!("Kimi provider alias '{}' is in use by another launch", desired.provider.alias);
        }
        if let Some(alias) = desired.models.keys().find(|alias| {
            catalog
                .models
                .get(*alias)
                .is_some_and(|model| Some(model) != desired.models.get(*alias))
        }) {
            bail!("Kimi model alias '{alias}' is in use by another launch");
        }
    }
    for previous in previous {
        if let Some(models) = table_mut(document, "models")? {
            for (alias, owned) in &previous.models {
                if !active.values().any(|catalog| catalog.models.contains_key(alias))
                    && models.get(alias).is_some_and(|item| model_matches(item, owned))
                {
                    models.remove(alias);
                }
            }
        }
        let provider_referenced = table(document, "models")?.is_some_and(|models| {
            models.iter().any(|(_, item)| {
                item.as_table().and_then(|table| string_field(table, "provider"))
                    == Some(&previous.provider.alias)
            })
        });
        let reusable = previous.provider == desired.provider;
        if !provider_referenced
            && !reusable
            && !active.values().any(|catalog| catalog.provider.alias == previous.provider.alias)
            && let Some(providers) = table_mut(document, "providers")?
            && providers
                .get(&previous.provider.alias)
                .is_some_and(|item| provider_matches(item, &previous.provider))
        {
            providers.remove(&previous.provider.alias);
        }
    }

    let providers = table(document, "providers")?;
    let provider_exists =
        providers.is_some_and(|providers| providers.contains_key(&desired.provider.alias));
    let provider_reusable = previous.iter().any(|previous| previous.provider == desired.provider)
        && providers
            .and_then(|providers| providers.get(&desired.provider.alias))
            .is_some_and(|item| provider_matches(item, &desired.provider));
    if provider_exists && !provider_reusable {
        bail!(
            "Kimi provider alias '{}' already exists outside rx ownership",
            desired.provider.alias
        );
    }
    if let Some(models) = table(document, "models")?
        && let Some(alias) = desired.models.keys().find(|alias| {
            models.get(alias).is_some_and(|item| {
                !active.values().any(|catalog| {
                    catalog.models.get(*alias) == desired.models.get(*alias)
                        && catalog
                            .models
                            .get(*alias)
                            .is_some_and(|owned| model_matches(item, owned))
                })
            })
        })
    {
        bail!("Kimi model alias '{alias}' already exists outside rx ownership");
    }

    if !provider_reusable {
        let mut provider = Table::new();
        provider["type"] = value("openai");
        provider["base_url"] = value(&desired.provider.base_url);
        provider["api_key"] = value(key);
        let providers = ensure_table(document, "providers")?;
        providers.insert(&desired.provider.alias, Item::Table(provider));
    }
    let models = ensure_table(document, "models")?;
    for (alias, model) in &desired.models {
        if models.contains_key(alias) {
            continue;
        }
        let mut entry = Table::new();
        entry["provider"] = value(&model.provider);
        entry["model"] = value(&model.model);
        entry["max_context_size"] = value(model.max_context_size);
        if let Some(display_name) = &model.display_name {
            entry["display_name"] = value(display_name);
        }
        models.insert(alias, Item::Table(entry));
    }
    Ok(())
}

fn provider_matches(item: &Item, owned: &OwnedProvider) -> bool {
    let Some(table) = item.as_table() else {
        return false;
    };
    table.len() == 3
        && string_field(table, "type") == Some("openai")
        && string_field(table, "base_url") == Some(owned.base_url.as_str())
        && string_field(table, "api_key").is_some_and(|key| key_hash(key) == owned.api_key_sha256)
}

fn model_matches(item: &Item, owned: &OwnedModel) -> bool {
    let Some(table) = item.as_table() else {
        return false;
    };
    let expected_len = if owned.display_name.is_some() { 4 } else { 3 };
    table.len() == expected_len
        && string_field(table, "provider") == Some(owned.provider.as_str())
        && string_field(table, "model") == Some(owned.model.as_str())
        && table.get("max_context_size").and_then(Item::as_integer) == Some(owned.max_context_size)
        && match &owned.display_name {
            Some(display_name) => {
                string_field(table, "display_name") == Some(display_name.as_str())
            }
            None => !table.contains_key("display_name"),
        }
}

fn string_field<'a>(table: &'a Table, key: &str) -> Option<&'a str> {
    table.get(key)?.as_str()
}

fn table<'a>(document: &'a DocumentMut, key: &str) -> Result<Option<&'a Table>> {
    match document.get(key) {
        Some(item) => item
            .as_table()
            .map(Some)
            .ok_or_else(|| anyhow::anyhow!("Kimi config '{key}' is not a table")),
        None => Ok(None),
    }
}

fn table_mut<'a>(document: &'a mut DocumentMut, key: &str) -> Result<Option<&'a mut Table>> {
    match document.get_mut(key) {
        Some(item) => item
            .as_table_mut()
            .map(Some)
            .ok_or_else(|| anyhow::anyhow!("Kimi config '{key}' is not a table")),
        None => Ok(None),
    }
}

fn ensure_table<'a>(document: &'a mut DocumentMut, key: &str) -> Result<&'a mut Table> {
    if document.get(key).is_none() {
        document[key] = Item::Table(Table::new());
    }
    document[key]
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("Kimi config '{key}' is not a table"))
}

fn read_document(path: &Path, contents: Option<&[u8]>) -> Result<DocumentMut> {
    let Some(contents) = contents else {
        return Ok(DocumentMut::new());
    };
    let body = std::str::from_utf8(contents)
        .with_context(|| format!("{} is not UTF-8", path.display()))?;
    body.parse().with_context(|| format!("failed to parse {}", path.display()))
}

fn read_marker(path: &Path) -> Result<Option<CatalogMarker>> {
    read_bytes(path)?
        .map(|contents| {
            serde_json::from_slice(&contents)
                .with_context(|| format!("failed to parse {}", path.display()))
        })
        .transpose()
}

fn stage_secret(path: &Path, contents: &[u8]) -> Result<tempfile::NamedTempFile> {
    let temp = file_io::stage(path, contents)?;
    file_io::secret_mode(temp.as_file(), path)?;
    Ok(temp)
}

fn key_hash(key: &str) -> String {
    format!("{:x}", Sha256::digest(key.as_bytes()))
}

#[cfg(test)]
mod tests;
