use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::file_io;

#[derive(Debug, Clone)]
pub(crate) struct Paths {
    pub(crate) dir: PathBuf,
    pub(crate) config: PathBuf,
    pub(crate) keys: PathBuf,
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

type KeyFile = BTreeMap<String, String>;

pub(crate) fn load(paths: &Paths) -> Result<Option<RxConfig>> {
    read_toml(&paths.config)
}

fn read_toml<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    file_io::read_optional(path)?
        .map(|contents| {
            let text = std::str::from_utf8(&contents)
                .with_context(|| format!("failed to read {}", path.display()))?;
            toml::from_str(text).with_context(|| format!("failed to parse {}", path.display()))
        })
        .transpose()
}

pub(crate) fn stored_key(paths: &Paths, provider: &str) -> Result<Option<String>> {
    Ok(load_keys(paths)?.get(provider).cloned())
}

pub(crate) fn load_or_default(paths: &Paths) -> Result<RxConfig> {
    Ok(load(paths)?.unwrap_or_default())
}

pub(crate) fn stored_providers(paths: &Paths) -> Result<BTreeSet<String>> {
    Ok(load_keys(paths)?.keys().cloned().collect())
}

pub(crate) fn set_default(paths: &Paths, provider: &str) -> Result<()> {
    let _lock = mutation_lock(paths)?;
    let mut config = load_or_default(paths)?;
    crate::provider::resolve(provider, config.provider.get(provider))?;
    config.default_provider = Some(provider.to_string());
    save_config(paths, &config)
}

pub(crate) fn set_none(paths: &Paths) -> Result<()> {
    let _lock = mutation_lock(paths)?;
    let mut config = load_or_default(paths)?;
    config.default_provider = Some(crate::provider::NONE.to_string());
    save_config(paths, &config)
}

pub(crate) fn login(paths: &Paths, provider: &str, key: String) -> Result<()> {
    let _lock = mutation_lock(paths)?;
    let mut config = load_or_default(paths)?;
    crate::provider::resolve(provider, config.provider.get(provider))?;
    let mut keys = load_keys(paths)?;
    let entry = config.provider.entry(provider.to_string()).or_default();
    entry.auth = AuthMode::ApiKey;
    keys.insert(provider.to_string(), key);
    config.default_provider = Some(provider.to_string());
    save_keys(paths, &keys)?;
    save_config(paths, &config)
}

pub(crate) fn logout(paths: &Paths, provider: &str) -> Result<bool> {
    let _lock = mutation_lock(paths)?;
    let mut config = load_or_default(paths)?;
    crate::provider::validate_id(provider)?;
    let mut keys = load_keys(paths)?;
    let removed = keys.remove(provider).is_some();
    if removed {
        if config.default_provider.as_deref() == Some(provider) {
            config.default_provider = None;
        }
        save_keys(paths, &keys)?;
        save_config(paths, &config)?;
    }
    Ok(removed)
}

fn mutation_lock(paths: &Paths) -> Result<fs::File> {
    file_io::lock(&paths.dir.join("rx.lock"))
}

fn load_keys(paths: &Paths) -> Result<KeyFile> {
    let Some(contents) = file_io::read_optional(&paths.keys)? else {
        return Ok(KeyFile::default());
    };
    let text = std::str::from_utf8(&contents)
        .with_context(|| format!("failed to read {}", paths.keys.display()))?;
    if text.trim().is_empty() {
        return Ok(KeyFile::default());
    }
    toml::from_str(text).with_context(|| format!("failed to parse {}", paths.keys.display()))
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
    let temp = file_io::stage(path, contents.as_bytes())?;
    if secret {
        file_io::secret_mode(temp.as_file(), path)?;
    }
    file_io::persist(temp, path)?;
    if secret {
        set_secret_path_mode(path)?;
    }
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
