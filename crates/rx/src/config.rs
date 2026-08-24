use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct Paths {
    pub dir: PathBuf,
    pub config: PathBuf,
    pub keys: PathBuf,
}

impl Paths {
    pub(crate) fn user() -> Result<Self> {
        let home =
            dirs::home_dir().ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;
        Ok(Self::in_dir(home.join(".recall")))
    }

    pub(crate) fn in_dir(dir: PathBuf) -> Self {
        Self { config: dir.join("rx.toml"), keys: dir.join("rx.keys"), dir }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AuthMode {
    #[default]
    ApiKey,
    Env,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct RxConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_provider: Option<String>,
    #[serde(default)]
    pub provider: BTreeMap<String, ProviderConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProviderConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anthropic_base: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default)]
    pub auth: AuthMode,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct KeyFile {
    #[serde(flatten)]
    values: BTreeMap<String, String>,
}

pub(crate) fn load(paths: &Paths) -> Result<Option<RxConfig>> {
    match fs::read_to_string(&paths.config) {
        Ok(contents) => {
            let config: RxConfig = toml::from_str(&contents)
                .with_context(|| format!("failed to parse {}", paths.config.display()))?;
            Ok(Some(config))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("failed to read {}", paths.config.display()))
        }
    }
}

pub(crate) fn stored_key(paths: &Paths, provider: &str) -> Result<Option<String>> {
    Ok(load_keys(paths)?.values.get(provider).cloned())
}

pub(crate) fn load_or_default(paths: &Paths) -> Result<RxConfig> {
    Ok(load(paths)?.unwrap_or_default())
}

fn ensure_provider_entry<'a>(config: &'a mut RxConfig, id: &str) -> &'a mut ProviderConfig {
    config.provider.entry(id.to_string()).or_default()
}

pub(crate) fn stored_providers(paths: &Paths) -> Result<BTreeSet<String>> {
    Ok(load_keys(paths)?.values.keys().cloned().collect())
}

pub(crate) fn set_default(paths: &Paths, provider: &str) -> Result<()> {
    let mut config = load_or_default(paths)?;
    crate::provider::resolve(provider, config.provider.get(provider))?;
    config.default_provider = Some(provider.to_string());
    save_config(paths, &config)
}

pub(crate) fn login(paths: &Paths, provider: &str, key: String) -> Result<()> {
    let mut config = load_or_default(paths)?;
    crate::provider::resolve(provider, config.provider.get(provider))?;
    let mut keys = load_keys(paths)?;
    let entry = ensure_provider_entry(&mut config, provider);
    entry.auth = AuthMode::ApiKey;
    keys.values.insert(provider.to_string(), key);
    config.default_provider = Some(provider.to_string());
    save_keys(paths, &keys)?;
    save_config(paths, &config)
}

pub(crate) fn logout(paths: &Paths, provider: &str) -> Result<bool> {
    let mut config = load_or_default(paths)?;
    crate::provider::resolve(provider, config.provider.get(provider))?;
    let mut keys = load_keys(paths)?;
    let removed = keys.values.remove(provider).is_some();
    if removed {
        if config.default_provider.as_deref() == Some(provider) {
            config.default_provider = None;
        }
        save_keys(paths, &keys)?;
        save_config(paths, &config)?;
    }
    Ok(removed)
}

fn load_keys(paths: &Paths) -> Result<KeyFile> {
    match fs::read_to_string(&paths.keys) {
        Ok(contents) if contents.trim().is_empty() => Ok(KeyFile::default()),
        Ok(contents) => toml::from_str(&contents)
            .with_context(|| format!("failed to parse {}", paths.keys.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(KeyFile::default()),
        Err(error) => {
            Err(error).with_context(|| format!("failed to read {}", paths.keys.display()))
        }
    }
}

fn save_config(paths: &Paths, config: &RxConfig) -> Result<()> {
    let contents = toml::to_string_pretty(config).context("failed to serialize rx.toml")?;
    write_file(&paths.config, &contents, false)
}

fn save_keys(paths: &Paths, keys: &KeyFile) -> Result<()> {
    let contents = toml::to_string_pretty(keys).context("failed to serialize rx.keys")?;
    write_file(&paths.keys, &contents, true)
}

fn write_file(path: &Path, contents: &str, secret: bool) -> Result<()> {
    let parent =
        path.parent().ok_or_else(|| anyhow::anyhow!("path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to chmod {}", parent.display()))?;
    }
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("failed to create temporary file in {}", parent.display()))?;
    temp.write_all(contents.as_bytes())
        .with_context(|| format!("failed to write temporary file in {}", parent.display()))?;
    temp.as_file()
        .sync_all()
        .with_context(|| format!("failed to sync temporary file in {}", parent.display()))?;
    if secret {
        set_secret_mode(temp.as_file(), path)?;
    }
    let persist_path = path.to_path_buf();
    temp.persist(&persist_path)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to replace {}", persist_path.display()))?;
    if secret {
        set_secret_path_mode(path)?;
    }
    Ok(())
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

#[cfg(unix)]
fn set_secret_path_mode(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to chmod {}", path.display()))
}

#[cfg(not(unix))]
fn set_secret_path_mode(_path: &Path) -> Result<()> {
    Ok(())
}
