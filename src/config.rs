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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DoctorLevel {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone)]
pub(crate) struct DoctorCheck {
    pub(crate) label: String,
    pub(crate) level: DoctorLevel,
    pub(crate) detail: String,
}

impl DoctorCheck {
    fn info(label: &str, detail: impl Into<String>) -> Self {
        Self { label: label.to_string(), level: DoctorLevel::Info, detail: detail.into() }
    }

    fn warn(label: &str, detail: impl Into<String>) -> Self {
        Self { label: label.to_string(), level: DoctorLevel::Warn, detail: detail.into() }
    }

    fn error(label: &str, detail: impl Into<String>) -> Self {
        Self { label: label.to_string(), level: DoctorLevel::Error, detail: detail.into() }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct DoctorReport {
    pub(crate) checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    pub(crate) fn has_errors(&self) -> bool {
        self.checks.iter().any(|c| c.level == DoctorLevel::Error)
    }
}

/// Evaluate configuration health from borrowed inputs — no filesystem access,
/// so it is fully unit-testable. `raw` is `None` when the config file is
/// absent (defaults apply — fine). `mode` is `st_mode & 0o777` when the file
/// exists on unix, else `None`. `known_sources` is `(id, label)` from
/// `adapters::source_labels()`.
pub(crate) fn evaluate_config(
    raw: Option<&str>,
    mode: Option<u32>,
    known_sources: &[(String, String)],
) -> DoctorReport {
    let mut checks = Vec::new();

    match raw {
        None => checks.push(DoctorCheck::info("config file", "not found; built-in defaults apply")),
        Some(_) => checks.push(DoctorCheck::info("config file", "present")),
    }

    if let Some(mode) = mode {
        let perms = mode & 0o777;
        if perms & 0o022 != 0 {
            checks.push(DoctorCheck::warn(
                "permissions",
                format!("group/world-writable ({perms:04o}); tighten with `chmod go-w`"),
            ));
        } else {
            checks.push(DoctorCheck::info("permissions", format!("{perms:04o}")));
        }
    }

    let parsed = match raw {
        None => Some(AppConfig::default()),
        Some(content) => match serde_json::from_str::<AppConfig>(content) {
            Ok(config) => {
                checks.push(DoctorCheck::info("json", "valid"));
                Some(config)
            }
            Err(err) => {
                checks.push(DoctorCheck::error("json", format!("invalid: {err}")));
                None
            }
        },
    };

    if let Some(config) = parsed {
        let enabled = known_sources.iter().filter(|(id, _)| config.is_source_enabled(id)).count();
        if enabled == 0 {
            checks.push(DoctorCheck::error(
                "sources",
                "no sources enabled; every known source is listed in disabled_sources",
            ));
        } else {
            checks.push(DoctorCheck::info(
                "sources",
                format!("{enabled} of {} enabled", known_sources.len()),
            ));
        }
    }

    DoctorReport { checks }
}

pub(crate) fn config_path() -> Result<PathBuf> {
    let dir =
        dirs::config_dir().ok_or_else(|| anyhow::anyhow!("cannot determine config directory"))?;
    Ok(dir.join("recall").join("config.json"))
}

#[cfg(test)]
mod tests {
    use super::{DoctorLevel, evaluate_config};

    fn known() -> Vec<(String, String)> {
        vec![
            ("codex".to_string(), "Codex".to_string()),
            ("claude-code".to_string(), "Claude Code".to_string()),
        ]
    }

    #[test]
    fn test_should_report_clean_when_config_file_missing() {
        let report = evaluate_config(None, None, &known());
        assert!(!report.has_errors());
        assert!(report.checks.iter().all(|c| c.level != DoctorLevel::Error));
    }

    #[test]
    fn test_should_error_when_json_is_invalid() {
        let report = evaluate_config(Some("{ not json"), Some(0o600), &known());
        assert!(report.has_errors());
        assert!(report.checks.iter().any(|c| c.level == DoctorLevel::Error && c.label == "json"));
    }

    #[test]
    fn test_should_error_when_no_sources_enabled() {
        let raw = r#"{"disabled_sources":["codex","claude-code"]}"#;
        let report = evaluate_config(Some(raw), Some(0o600), &known());
        assert!(report.has_errors());
        assert!(
            report.checks.iter().any(|c| c.level == DoctorLevel::Error && c.label == "sources")
        );
    }

    #[test]
    fn test_should_report_clean_when_default_config_valid() {
        let raw = serde_json::to_string_pretty(&super::AppConfig::default()).unwrap();
        let report = evaluate_config(Some(&raw), Some(0o600), &known());
        assert!(!report.has_errors());
    }

    #[test]
    fn test_should_warn_when_config_group_or_world_writable() {
        let report = evaluate_config(Some("{}"), Some(0o666), &known());
        assert!(!report.has_errors());
        assert!(
            report.checks.iter().any(|c| c.level == DoctorLevel::Warn && c.label == "permissions")
        );
    }

    #[test]
    fn test_should_not_warn_when_config_only_group_world_readable() {
        let report = evaluate_config(Some("{}"), Some(0o644), &known());
        assert!(
            report
                .checks
                .iter()
                .all(|c| !(c.level == DoctorLevel::Warn && c.label == "permissions"))
        );
    }
}
