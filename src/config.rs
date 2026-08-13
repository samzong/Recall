use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};

use crate::db::search::TimeRange;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub(crate) enum SyncWindow {
    Today,
    Week,
    Month,
    #[default]
    All,
}

impl SyncWindow {
    pub(crate) fn next(self) -> Self {
        match self {
            Self::Today => Self::Week,
            Self::Week => Self::Month,
            Self::Month => Self::All,
            Self::All => Self::Today,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Today => "today",
            Self::Week => "7d",
            Self::Month => "30d",
            Self::All => "all",
        }
    }

    pub(crate) fn to_since_cutoff(self) -> Option<i64> {
        match self {
            Self::Today => crate::utils::parse_since("1d"),
            Self::Week => crate::utils::parse_since("7d"),
            Self::Month => crate::utils::parse_since("30d"),
            Self::All => None,
        }
    }

    pub(crate) fn to_time_range(self) -> TimeRange {
        match self {
            Self::Today => TimeRange::Today,
            Self::Week => TimeRange::Week,
            Self::Month => TimeRange::Month,
            Self::All => TimeRange::All,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct AppConfig {
    #[serde(default)]
    pub(crate) disabled_sources: Vec<String>,
    #[serde(default, rename = "enabled_sources", skip_serializing_if = "Vec::is_empty")]
    legacy_enabled_sources: Vec<String>,
    #[serde(default)]
    pub(crate) sync_window: SyncWindow,
    /// Glob patterns matched against each session's `directory` (cwd) field.
    /// Sessions whose cwd matches ANY glob are dropped at sync time — they
    /// never enter the FTS or vector index. Edit via the config file.
    ///
    /// The pattern matches the cwd itself, so to exclude a directory use a
    /// trailing-`**`-free pattern (a `dir/**` glob matches only its
    /// children, not `dir`). Examples: `**/observer-sessions`,
    /// `**/.claude-mem/**`, `**/scratch-*`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) excluded_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) share: Option<ShareConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ShareConfig {
    pub(crate) provider: String,
    pub(crate) project_name: String,
    #[serde(default)]
    pub(crate) project_domain: String,
    pub(crate) publish_dir: String,
}

impl AppConfig {
    pub(crate) fn load() -> Result<Self> {
        let path = config_path()?;
        Self::load_from(&path)
    }

    fn load_from(path: &Path) -> Result<Self> {
        let content = match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => {
                return Err(error).with_context(|| format!("failed to read {}", path.display()));
            }
        };
        serde_json::from_str(&content)
            .with_context(|| format!("failed to parse {}", path.display()))
    }

    pub(crate) fn save(&self) -> Result<()> {
        let path = config_path()?;
        self.save_to(&path)
    }

    fn save_to(&self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("config path has no parent: {}", path.display()))?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;

        let content = serde_json::to_vec_pretty(self)?;
        let mut temp = tempfile::NamedTempFile::new_in(parent)
            .with_context(|| format!("failed to create temporary file in {}", parent.display()))?;
        temp.write_all(&content)
            .with_context(|| format!("failed to write temporary config in {}", parent.display()))?;
        temp.as_file()
            .sync_all()
            .with_context(|| format!("failed to sync temporary config in {}", parent.display()))?;
        temp.persist(path)
            .map_err(|error| error.error)
            .with_context(|| format!("failed to replace {}", path.display()))?;
        Ok(())
    }

    pub(crate) fn normalize_sources(&mut self, known_sources: &[(String, String)]) {
        self.legacy_enabled_sources.clear();

        self.disabled_sources.retain(|id| known_sources.iter().any(|(known, _)| known == id));
        self.disabled_sources.sort();
        self.disabled_sources.dedup();
    }

    pub(crate) fn is_source_enabled(&self, source_id: &str) -> bool {
        !self.disabled_sources.iter().any(|id| id == source_id)
    }

    /// Compile the `excluded_paths` globs into a single `GlobSet` matcher.
    /// Returns `None` when no rules are configured. Errors propagate so an
    /// invalid pattern fails loud — the user gets a startup error, not a
    /// silent half-applied filter.
    pub(crate) fn build_path_excluder(&self) -> Result<Option<GlobSet>> {
        if self.excluded_paths.is_empty() {
            return Ok(None);
        }
        let mut builder = GlobSetBuilder::new();
        for pat in &self.excluded_paths {
            let glob =
                Glob::new(pat).with_context(|| format!("invalid excluded_paths glob: {pat}"))?;
            builder.add(glob);
        }
        Ok(Some(builder.build()?))
    }
}

pub(crate) fn config_path() -> Result<PathBuf> {
    let dir =
        dirs::config_dir().ok_or_else(|| anyhow::anyhow!("cannot determine config directory"))?;
    Ok(dir.join("recall").join("config.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_invalid_config_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, "{ invalid json").unwrap();

        let error = AppConfig::load_from(&path).unwrap_err();

        assert!(error.to_string().contains("failed to parse"), "{error:#}");
    }

    #[test]
    fn atomic_save_replaces_existing_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, "{}").unwrap();
        let mut config = AppConfig::default();
        config.disabled_sources.push("codex".to_string());

        config.save_to(&path).unwrap();

        let reloaded = AppConfig::load_from(&path).unwrap();
        assert_eq!(reloaded.disabled_sources, vec!["codex"]);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn failed_atomic_save_preserves_existing_target() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::create_dir(&path).unwrap();
        let marker = path.join("existing");
        std::fs::write(&marker, "keep").unwrap();

        let result = AppConfig::default().save_to(&path);

        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "keep");
    }
}
