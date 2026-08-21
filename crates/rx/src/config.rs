use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::args::ConfigCommand;

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

impl AuthMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::ApiKey => "api_key",
            Self::Env => "env",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct RxConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_gateway: Option<String>,
    #[serde(default)]
    pub gateway: BTreeMap<String, GatewayConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct GatewayConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
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

pub(crate) fn run(command: ConfigCommand, paths: &Paths) -> Result<()> {
    match command {
        ConfigCommand::SetGateway { name } => {
            let spec = crate::launch::provider(&name)?;
            let mut config = load_or_default(paths)?;
            config.default_gateway = Some(spec.id.to_string());
            ensure_gateway_entry(&mut config, spec.id, spec.default_base_url);
            save_config(paths, &config)?;
            println!("default gateway: {}", spec.id);
            Ok(())
        }
        ConfigCommand::SetKey { provider, key } => {
            let spec = crate::launch::provider(&provider)?;
            let mut config = load_or_default(paths)?;
            let entry = ensure_gateway_entry(&mut config, spec.id, spec.default_base_url);
            entry.auth = AuthMode::ApiKey;
            save_config(paths, &config)?;
            let mut keys = load_keys(paths)?;
            keys.values.insert(spec.id.to_string(), key);
            save_keys(paths, &keys)?;
            println!("stored API key for {}", spec.id);
            Ok(())
        }
        ConfigCommand::Get { name } => {
            print!("{}", format_get(paths, name.as_deref())?);
            Ok(())
        }
    }
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

fn load_or_default(paths: &Paths) -> Result<RxConfig> {
    Ok(load(paths)?.unwrap_or_default())
}

fn ensure_gateway_entry<'a>(
    config: &'a mut RxConfig,
    id: &str,
    default_base_url: &str,
) -> &'a mut GatewayConfig {
    config.gateway.entry(id.to_string()).or_insert_with(|| GatewayConfig {
        base_url: Some(default_base_url.to_string()),
        auth: AuthMode::ApiKey,
        ..Default::default()
    })
}

pub(crate) fn format_get(paths: &Paths, name: Option<&str>) -> Result<String> {
    let config = load_or_default(paths)?;
    let keys = load_keys(paths)?;
    match name {
        None => {
            let mut out = String::new();
            match config.default_gateway.as_deref() {
                Some(gateway) => out.push_str(&format!("default_gateway = {gateway}\n")),
                None => out.push_str("default_gateway = (unset)\n"),
            }
            for spec in crate::launch::PROVIDERS {
                out.push('\n');
                out.push_str(&format_provider(&config, &keys, spec.id)?);
            }
            Ok(out)
        }
        Some("gateway") => match config.default_gateway {
            Some(gateway) => Ok(format!("{gateway}\n")),
            None => Ok("(unset)\n".to_string()),
        },
        Some(name) => Ok(format_provider(&config, &keys, name)?),
    }
}

fn format_provider(config: &RxConfig, keys: &KeyFile, name: &str) -> Result<String> {
    let spec = crate::launch::provider(name)?;
    let entry = config.gateway.get(spec.id);
    let base_url =
        entry.and_then(|entry| entry.base_url.as_deref()).unwrap_or(spec.default_base_url);
    let auth = entry.map(|entry| entry.auth).unwrap_or_default();
    let key = if keys.values.contains_key(spec.id) { "set" } else { "unset" };
    let model =
        entry.and_then(|entry| entry.model.as_deref()).or(spec.default_model).unwrap_or("(unset)");
    Ok(format!(
        "[{}]\nbase_url = {base_url}\nmodel = {model}\nauth = {}\nkey = {key}\n",
        spec.id,
        auth.as_str()
    ))
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
