use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use toml_edit::{DocumentMut, Item, Table, value};

use crate::args;
use crate::catalog::{self, ListedModel};
use crate::config::Paths;
use crate::launch::{EnvLookup, LaunchPlan, ProviderTarget};

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

pub(crate) fn prepare(
    target: &ProviderTarget,
    configured_model: Option<&str>,
    paths: &Paths,
    env: &EnvLookup,
    passthrough: &[OsString],
) -> Result<LaunchPlan> {
    let provider_id = target.provider.id.as_str();
    let provider_alias = format!("rx-{provider_id}");
    let model_prefix = format!("{provider_alias}/");
    let (requested_model, mut args) = take_model(passthrough)?;
    let preferred_model = requested_model
        .or_else(|| configured_model.map(str::to_string))
        .map(|model| model.strip_prefix(&model_prefix).unwrap_or(&model).to_string());
    let allow_fetch = env.is_real() || target.provider.setup == crate::provider::Setup::Generated;
    let mut notes = Vec::new();
    let mut models = match catalog::load_listed_models(
        paths,
        provider_id,
        &target.provider.endpoint,
        &target.key,
        allow_fetch,
    ) {
        Ok(models) => models,
        Err(error) if preferred_model.is_some() => {
            notes.push(format!(
                "[rx] kimi: provider catalog unavailable; seeding only the selected model: {error:#}"
            ));
            Vec::new()
        }
        Err(error) => return Err(error),
    };
    let selected_model = match preferred_model {
        Some(model) => model,
        None => {
            let model = models.first().map(|model| model.id.clone()).ok_or_else(|| {
                anyhow::anyhow!(
                    "kimi needs a model for provider '{provider_id}'; pass --model <id> or set [provider.{provider_id}] model in rx.toml"
                )
            })?;
            notes.push(format!(
                "[rx] kimi: no model selected; using first provider model '{model}' (set [provider.{provider_id}] model to choose)"
            ));
            model
        }
    };
    if !models.iter().any(|model| model.id == selected_model) {
        models.push(ListedModel {
            id: selected_model.clone(),
            name: None,
            context_length: Some(
                target.provider.default_context.unwrap_or(catalog::DEFAULT_CONTEXT_WINDOW),
            ),
        });
    }
    let config_path = kimi_config_path(paths, env)?;
    let lease = seed_catalog(&config_path, &provider_alias, target, &models, env.is_real())?;
    args.insert(0, OsString::from(format!("{provider_alias}/{selected_model}")));
    args.insert(0, OsString::from("--model"));
    Ok(LaunchPlan {
        launch_lease: Some(lease),
        program: PathBuf::from("kimi"),
        args,
        env_set: vec![("KIMI_MODEL_NAME".to_string(), String::new())],
        stderr_note: (!notes.is_empty()).then(|| notes.join("\n")),
    })
}

fn kimi_config_path(paths: &Paths, env: &EnvLookup) -> Result<PathBuf> {
    if let Some(home) = env.get("KIMI_CODE_HOME").filter(|value| !value.trim().is_empty()) {
        return Ok(PathBuf::from(home).join("config.toml"));
    }
    if !env.is_real() {
        return Ok(paths.dir.join("kimi-code").join("config.toml"));
    }
    let home = dirs::home_dir().context("cannot determine home directory")?;
    Ok(home.join(".kimi-code").join("config.toml"))
}

fn seed_catalog(
    config_path: &Path,
    provider_alias: &str,
    target: &ProviderTarget,
    models: &[ListedModel],
    allow_prompt: bool,
) -> Result<File> {
    let parent = config_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("path has no parent: {}", config_path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let lock_path = appended_path(config_path, ".rx.lock");
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("failed to open {}", lock_path.display()))?;
    lock.lock_exclusive().with_context(|| format!("failed to lock {}", lock_path.display()))?;
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
                    if lease_is_active(&parent.join(&name))? {
                        active.insert(name, catalog.clone());
                    }
                    previous.push(catalog);
                }
            }
            None => {}
        }
        reconcile(&mut document, &previous, &active, &desired, &target.key)?;
        let lease_path = parent.join(&lease_name);
        if !lease_path.try_exists()? {
            let mut options = OpenOptions::new();
            options.read(true).write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&lease_path) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error).context("failed to create Kimi catalog lease"),
            };
        }
        let lease = open_lease(&lease_path, false)?;
        #[cfg(not(windows))]
        FileExt::try_lock_shared(&lease).context("failed to share Kimi catalog lease")?;
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
        persist_secret(staged_config, config_path)?;
        persist_secret(staged_marker, &marker_path)?;
        return Ok(lease);
    }
    bail!("{} changed repeatedly while seeding Kimi catalog", config_path.display())
}

fn catalog_lease_name(catalog: &OwnedCatalog) -> Result<String> {
    Ok(format!(".rx-kimi-{:x}", Sha256::digest(serde_json::to_vec(catalog)?)))
}

fn open_lease(path: &Path, write: bool) -> Result<File> {
    if !fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect Kimi lease {}", path.display()))?
        .file_type()
        .is_file()
    {
        bail!("Kimi lease {} is not a regular file", path.display());
    }
    let mut options = OpenOptions::new();
    options.read(true).write(write);
    #[cfg(windows)]
    if !write {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 1;
        options.share_mode(FILE_SHARE_READ);
    }
    options.open(path).with_context(|| format!("failed to open Kimi lease {}", path.display()))
}

fn lease_is_active(path: &Path) -> Result<bool> {
    #[cfg(windows)]
    {
        const ERROR_SHARING_VIOLATION: i32 = 32;
        match open_lease(path, true) {
            Ok(_) => Ok(false),
            Err(error)
                if error.downcast_ref::<io::Error>().and_then(io::Error::raw_os_error)
                    == Some(ERROR_SHARING_VIOLATION) =>
            {
                Ok(true)
            }
            Err(error) => Err(error),
        }
    }
    #[cfg(not(windows))]
    {
        let file = open_lease(path, true)?;
        match file.try_lock_exclusive() {
            Ok(()) => {
                FileExt::unlock(&file).context("failed to release Kimi lease probe")?;
                Ok(false)
            }
            Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
                Ok(true)
            }
            Err(error) => Err(error).context("failed to inspect Kimi lease lock"),
        }
    }
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
        && table
            .get("max_context_size")
            .and_then(Item::as_value)
            .and_then(|value| value.as_integer())
            == Some(owned.max_context_size)
        && match &owned.display_name {
            Some(display_name) => {
                string_field(table, "display_name") == Some(display_name.as_str())
            }
            None => !table.contains_key("display_name"),
        }
}

fn string_field<'a>(table: &'a Table, key: &str) -> Option<&'a str> {
    table.get(key)?.as_value()?.as_str()
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
    match fs::read(path) {
        Ok(contents) => serde_json::from_slice(&contents)
            .with_context(|| format!("failed to parse {}", path.display()))
            .map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

fn read_bytes(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

fn stage_secret(path: &Path, contents: &[u8]) -> Result<tempfile::NamedTempFile> {
    let parent =
        path.parent().ok_or_else(|| anyhow::anyhow!("path has no parent: {}", path.display()))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("failed to create temporary file in {}", parent.display()))?;
    temp.write_all(contents)
        .with_context(|| format!("failed to write temporary file for {}", path.display()))?;
    temp.as_file()
        .sync_all()
        .with_context(|| format!("failed to sync temporary file for {}", path.display()))?;
    set_secret_mode(temp.as_file(), path)?;
    Ok(temp)
}

fn persist_secret(temp: tempfile::NamedTempFile, path: &Path) -> Result<()> {
    temp.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

fn appended_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn key_hash(key: &str) -> String {
    format!("{:x}", Sha256::digest(key.as_bytes()))
}

#[cfg(unix)]
fn set_secret_mode(file: &fs::File, path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to chmod {}", path.display()))
}

#[cfg(not(unix))]
fn set_secret_mode(_file: &fs::File, _path: &Path) -> Result<()> {
    Ok(())
}

fn take_model(passthrough: &[OsString]) -> Result<(Option<String>, Vec<OsString>)> {
    let limit = args::before_double_dash(passthrough).len();
    let mut model = None;
    let mut kept = Vec::with_capacity(passthrough.len());
    let mut i = 0;
    while i < passthrough.len() {
        let arg = &passthrough[i];
        if i < limit && (arg == "-m" || arg == "--model") {
            let value = passthrough
                .get(i + 1)
                .filter(|_| i + 1 < limit)
                .ok_or_else(|| anyhow::anyhow!("{} requires a model id", arg.to_string_lossy()))?;
            let value = value.to_str().filter(|value| !value.is_empty()).ok_or_else(|| {
                anyhow::anyhow!("{} requires a UTF-8 model id", arg.to_string_lossy())
            })?;
            model = Some(value.to_string());
            i += 2;
            continue;
        }
        let text = if i < limit { arg.to_str() } else { None };
        if let Some(value) =
            text.and_then(|text| text.strip_prefix("--model=").or_else(|| text.strip_prefix("-m=")))
        {
            if value.is_empty() {
                bail!("{} requires a model id", arg.to_string_lossy());
            }
            model = Some(value.to_string());
            i += 1;
            continue;
        }
        kept.push(arg.clone());
        i += 1;
    }
    Ok((model, kept))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    #[ignore = "requires native Kimi Code and Python"]
    fn native_kimi_lease_survives_launcher_exit() -> Result<()> {
        if let Some(home) = std::env::var_os("RX_TEST_KIMI_HOME") {
            let paths = Paths::in_dir(PathBuf::from(home).join(".recall"));
            let request = args::LaunchRequest {
                harness: args::Harness::Kimi,
                provider: Some(std::env::var("RX_TEST_KIMI_PROVIDER")?),
                passthrough: os(&[
                    "--model",
                    "model-a",
                    if std::env::var_os("RX_TEST_KIMI_ACP").is_some() {
                        "acp"
                    } else {
                        "--version"
                    },
                ]),
            };
            let plan = crate::launch::plan(&request, &paths, &EnvLookup::real())?;
            crate::launch::exec(&plan)
        } else {
            let status = std::process::Command::new("python")
                .arg(
                    Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("scripts/probe-kimi-windows-leases.py"),
                )
                .arg("--launcher")
                .arg(std::env::current_exe()?)
                .status()?;
            assert!(status.success(), "native Kimi lease probe failed: {status}");
            Ok(())
        }
    }

    fn target(key: &str) -> ProviderTarget {
        let mut provider = crate::provider::find("tokener").unwrap().clone();
        provider.endpoint = "https://provider.test".to_string();
        ProviderTarget { provider, key: key.to_string(), model: None }
    }

    fn listed(id: &str, name: &str, context: i64) -> ListedModel {
        ListedModel {
            id: id.to_string(),
            name: Some(name.to_string()),
            context_length: Some(context),
        }
    }

    fn os(argv: &[&str]) -> Vec<OsString> {
        argv.iter().map(OsString::from).collect()
    }

    fn run_isolated(name: &str) -> bool {
        if std::env::var_os("RX_TEST_KIMI_CATALOG_CHILD").is_some() {
            return false;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env("RX_TEST_KIMI_CATALOG_CHILD", "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        true
    }

    #[test]
    fn model_flag_selects_catalog_model_and_preserves_literal_arguments() {
        assert_eq!(
            take_model(&os(&["--plan", "-m", "kimi-k3", "--", "--model", "literal"])).unwrap(),
            (Some("kimi-k3".to_string()), os(&["--plan", "--", "--model", "literal"]))
        );
        assert_eq!(
            take_model(&os(&["--model=glm-5", "-m=deepseek-v4"])).unwrap(),
            (Some("deepseek-v4".to_string()), Vec::new())
        );
    }

    #[test]
    fn catalog_refresh_preserves_user_config_and_replaces_owned_models() {
        if run_isolated(
            "kimi::tests::catalog_refresh_preserves_user_config_and_replaces_owned_models",
        ) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "default_model = \"native/model\"\n\n[providers.native]\n# keep this\ntype = \"openai\"\nbase_url = \"https://native.test/v1\"\napi_key = \"native-key\"\n\n[models.\"native/model\"]\nprovider = \"native\"\nmodel = \"native-model\"\nmax_context_size = 100000\n",
        )
        .unwrap();
        seed_catalog(
            &path,
            "rx-tokener",
            &target("rx-secret"),
            &[listed("model-a", "Model A", 200_000), listed("model-b", "Model B", 300_000)],
            false,
        )
        .unwrap();
        seed_catalog(
            &path,
            "rx-tokener",
            &target("rx-secret"),
            &[listed("model-b", "Model B", 300_000), listed("model-c", "Model C", 400_000)],
            false,
        )
        .unwrap();
        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("# keep this"));
        let document: toml::Value = toml::from_str(&body).unwrap();
        assert_eq!(document["default_model"].as_str(), Some("native/model"));
        assert_eq!(document["providers"]["native"]["api_key"].as_str(), Some("native-key"));
        assert!(document["models"].get("native/model").is_some());
        assert!(document["models"].get("rx-tokener/model-a").is_none());
        assert!(document["models"].get("rx-tokener/model-b").is_some());
        assert!(document["models"].get("rx-tokener/model-c").is_some());
        let marker = fs::read_to_string(appended_path(&path, ".rx-catalog.json")).unwrap();
        assert!(!marker.contains("rx-secret"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(
                fs::metadata(appended_path(&path, ".rx-catalog.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn modified_owned_model_is_preserved_and_blocks_reclaim() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let models = [listed("model-a", "Model A", 200_000)];
        seed_catalog(&path, "rx-tokener", &target("rx-secret"), &models, false).unwrap();
        let mut document = fs::read_to_string(&path).unwrap().parse::<DocumentMut>().unwrap();
        document["models"]["rx-tokener/model-a"]["model"] = value("user-model");
        fs::write(&path, document.to_string()).unwrap();
        let error =
            seed_catalog(&path, "rx-tokener", &target("rx-secret"), &models, false).unwrap_err();
        assert!(error.to_string().contains("outside rx ownership"), "{error:#}");
        let document: toml::Value = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(document["models"]["rx-tokener/model-a"]["model"].as_str(), Some("user-model"));
    }

    #[test]
    fn overlapping_catalogs_keep_live_routes_and_reclaim_only_exited_launches() {
        if run_isolated(
            "kimi::tests::overlapping_catalogs_keep_live_routes_and_reclaim_only_exited_launches",
        ) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let first_models = [listed("model-a", "Model A", 200_000)];
        let second_models = [listed("model-b", "Model B", 300_000)];
        let first =
            seed_catalog(&path, "rx-first", &target("first-key"), &first_models, false).unwrap();
        let same =
            seed_catalog(&path, "rx-first", &target("first-key"), &first_models, false).unwrap();
        let second =
            seed_catalog(&path, "rx-first", &target("first-key"), &second_models, false).unwrap();
        let third =
            seed_catalog(&path, "rx-second", &target("second-key"), &first_models, false).unwrap();
        let document: toml::Value = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(document["models"]["rx-first/model-a"]["provider"].as_str(), Some("rx-first"));
        assert_eq!(document["models"]["rx-first/model-b"]["provider"].as_str(), Some("rx-first"));
        assert_eq!(document["models"]["rx-second/model-a"]["provider"].as_str(), Some("rx-second"));
        assert_eq!(document["providers"]["rx-first"]["api_key"].as_str(), Some("first-key"));
        assert_eq!(document["providers"]["rx-second"]["api_key"].as_str(), Some("second-key"));

        drop(first);
        seed_catalog(&path, "rx-second", &target("second-key"), &first_models, false).unwrap();
        let document: toml::Value = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(document["models"].get("rx-first/model-a").is_some());
        drop(same);
        let fourth =
            seed_catalog(&path, "rx-second", &target("second-key"), &first_models, false).unwrap();
        let document: toml::Value = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(document["models"].get("rx-first/model-a").is_none());
        assert!(document["models"].get("rx-first/model-b").is_some());
        assert!(document["providers"].get("rx-first").is_some());

        drop(second);
        drop(third);
        let _fifth =
            seed_catalog(&path, "rx-second", &target("second-key"), &first_models, false).unwrap();
        let document: toml::Value = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(document["models"].get("rx-first/model-b").is_none());
        assert!(document["providers"].get("rx-first").is_none());
        let marker = read_marker(&appended_path(&path, ".rx-catalog.json")).unwrap().unwrap();
        let CatalogMarker::Leased { catalogs, .. } = marker else {
            panic!("expected leased catalog")
        };
        assert_eq!(catalogs.len(), 1);
        assert_eq!(
            fs::read_dir(dir.path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().starts_with(".rx-kimi-"))
                .count(),
            3
        );
        drop(fourth);
    }

    #[test]
    fn historical_lease_reuse_preserves_files_without_claiming_config() {
        if run_isolated(
            "kimi::tests::historical_lease_reuse_preserves_files_without_claiming_config",
        ) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let models = [listed("model-a", "Model A", 200_000)];
        let desired = desired_catalog("rx-first", &target("first-key"), &models);
        seed_catalog(&path, "rx-first", &target("first-key"), &models, false).unwrap();
        let lease_path = dir.path().join(catalog_lease_name(&desired).unwrap());
        assert_eq!(fs::metadata(&lease_path).unwrap().len(), 0);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&lease_path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        fs::write(&lease_path, b"user bytes").unwrap();
        for _ in 0..3 {
            seed_catalog(&path, "rx-second", &target("second-key"), &models, false).unwrap();
            seed_catalog(&path, "rx-first", &target("first-key"), &models, false).unwrap();
        }
        assert_eq!(fs::read(&lease_path).unwrap(), b"user bytes");
        assert_eq!(
            fs::read_dir(dir.path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().starts_with(".rx-kimi-"))
                .count(),
            2
        );
        let owned = fs::read_to_string(&path).unwrap().parse::<DocumentMut>().unwrap();
        seed_catalog(&path, "rx-second", &target("second-key"), &models, false).unwrap();
        let mut document = fs::read_to_string(&path).unwrap().parse::<DocumentMut>().unwrap();
        document["providers"]["rx-first"] = owned["providers"]["rx-first"].clone();
        fs::write(&path, document.to_string()).unwrap();
        let before = fs::read(&path).unwrap();
        let marker_path = appended_path(&path, ".rx-catalog.json");
        let marker = fs::read(&marker_path).unwrap();
        assert!(seed_catalog(&path, "rx-first", &target("first-key"), &models, false).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(fs::read(&marker_path).unwrap(), marker);
    }

    #[test]
    fn live_route_conflicts_and_user_edits_leave_catalog_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let models = [listed("model-a", "Model A", 200_000)];
        let _first = seed_catalog(&path, "rx-first", &target("first-key"), &models, false).unwrap();
        let marker_path = appended_path(&path, ".rx-catalog.json");
        let marker = fs::read(&marker_path).unwrap();
        let before = fs::read(&path).unwrap();
        assert!(seed_catalog(&path, "rx-first", &target("rotated-key"), &models, false).is_err());
        assert!(
            seed_catalog(
                &path,
                "rx-first",
                &target("first-key"),
                &[listed("model-a", "Model A", 400_000)],
                false
            )
            .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(fs::read(&marker_path).unwrap(), marker);
        let mut document = String::from_utf8(before).unwrap().parse::<DocumentMut>().unwrap();
        document["models"]["rx-first/model-a"]["model"] = value("user-model");
        fs::write(&path, document.to_string()).unwrap();
        let edited = fs::read(&path).unwrap();
        assert!(seed_catalog(&path, "rx-first", &target("first-key"), &models, false).is_err());
        assert_eq!(fs::read(&path).unwrap(), edited);
        assert_eq!(fs::read(&marker_path).unwrap(), marker);
    }

    #[test]
    fn legacy_and_unverifiable_leases_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let models = [listed("model-a", "Model A", 200_000)];
        let _lease = seed_catalog(&path, "rx-first", &target("first-key"), &models, false).unwrap();
        let marker_path = appended_path(&path, ".rx-catalog.json");
        let before = fs::read(&path).unwrap();
        let current: serde_json::Value =
            serde_json::from_slice(&fs::read(&marker_path).unwrap()).unwrap();
        let legacy = desired_catalog("rx-first", &target("first-key"), &models);
        let mut missing = current.clone();
        let name = missing["catalogs"].as_object().unwrap().keys().next().unwrap().clone();
        let catalog = missing["catalogs"].as_object_mut().unwrap().remove(&name).unwrap();
        missing["catalogs"][".rx-kimi-missing"] = catalog;
        let mut invalid = current.clone();
        invalid["version"] = 999.into();
        for marker in [serde_json::to_value(legacy).unwrap(), missing, invalid] {
            fs::write(&marker_path, serde_json::to_vec(&marker).unwrap()).unwrap();
            let marker_bytes = fs::read(&marker_path).unwrap();
            assert!(seed_catalog(&path, "rx-first", &target("first-key"), &models, false).is_err());
            assert_eq!(fs::read(&path).unwrap(), before);
            assert_eq!(fs::read(&marker_path).unwrap(), marker_bytes);
        }
        let marker_bytes = serde_json::to_vec(&current).unwrap();
        fs::write(&marker_path, &marker_bytes).unwrap();
        drop(_lease);
        fs::remove_file(dir.path().join(name)).unwrap();
        assert!(seed_catalog(&path, "rx-first", &target("first-key"), &models, false).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(fs::read(&marker_path).unwrap(), marker_bytes);
    }
}
