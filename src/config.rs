use std::path::PathBuf;

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
    #[serde(default)]
    pub(crate) default_current_repo_scope: bool,
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
        if !path.exists() {
            return Ok(Self::default());
        }

        let content = std::fs::read_to_string(path)?;
        let config: Self = serde_json::from_str(&content)?;
        Ok(config)
    }

    pub(crate) fn load_or_default() -> Self {
        Self::load().unwrap_or_default()
    }

    pub(crate) fn save(&self) -> Result<()> {
        let path = config_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub(crate) fn normalize_sources(&mut self, known_sources: &[(String, String)]) {
        self.legacy_enabled_sources.clear();

        self.disabled_sources.retain(|id| known_sources.iter().any(|(known, _)| known == id));
        self.disabled_sources.sort();
        self.disabled_sources.dedup();

        let enabled_count = known_sources.len().saturating_sub(self.disabled_sources.len());
        if enabled_count == 0 {
            self.disabled_sources.clear();
        }
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
