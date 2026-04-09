use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::db::search::TimeRange;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum SyncWindow {
    Today,
    Week,
    #[default]
    Month,
    All,
}

impl SyncWindow {
    pub fn next(self) -> Self {
        match self {
            Self::Today => Self::Week,
            Self::Week => Self::Month,
            Self::Month => Self::All,
            Self::All => Self::Today,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Today => "today",
            Self::Week => "7d",
            Self::Month => "30d",
            Self::All => "all",
        }
    }

    pub fn to_since_cutoff(self) -> Option<i64> {
        match self {
            Self::Today => crate::utils::parse_since("1d"),
            Self::Week => crate::utils::parse_since("7d"),
            Self::Month => crate::utils::parse_since("30d"),
            Self::All => None,
        }
    }

    pub fn to_time_range(self) -> TimeRange {
        match self {
            Self::Today => TimeRange::Today,
            Self::Week => TimeRange::Week,
            Self::Month => TimeRange::Month,
            Self::All => TimeRange::All,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub enabled_sources: Vec<String>,
    pub sync_window: SyncWindow,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self { enabled_sources: Vec::new(), sync_window: SyncWindow::Month }
    }
}

impl AppConfig {
    pub fn load() -> Result<Self> {
        let path = config_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }

        let content = std::fs::read_to_string(path)?;
        let mut config: Self = serde_json::from_str(&content)?;
        config.enabled_sources.sort();
        config.enabled_sources.dedup();
        Ok(config)
    }

    pub fn load_or_default() -> Self {
        Self::load().unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn normalize_sources(&mut self, known_sources: &[(String, String)]) {
        if self.enabled_sources.is_empty() {
            self.enabled_sources = known_sources.iter().map(|(id, _)| id.clone()).collect();
        } else {
            self.enabled_sources.retain(|id| known_sources.iter().any(|(known, _)| known == id));
            if self.enabled_sources.is_empty() {
                self.enabled_sources = known_sources.iter().map(|(id, _)| id.clone()).collect();
            }
        }
        self.enabled_sources.sort();
        self.enabled_sources.dedup();
    }

    pub fn is_source_enabled(&self, source_id: &str) -> bool {
        self.enabled_sources.iter().any(|id| id == source_id)
    }
}

pub fn config_path() -> Result<PathBuf> {
    let dir =
        dirs::config_dir().ok_or_else(|| anyhow::anyhow!("cannot determine config directory"))?;
    Ok(dir.join("recall").join("config.json"))
}
