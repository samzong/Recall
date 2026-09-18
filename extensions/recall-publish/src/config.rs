use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const CONFIG_SCHEMA: u32 = 1;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Config {
    #[serde(default = "default_schema")]
    pub schema: u32,
    #[serde(default)]
    pub publisher: Publisher,
    #[serde(default = "default_languages")]
    pub languages: Vec<String>,
    #[serde(default)]
    pub allow: Allow,
    #[serde(default)]
    pub path_substitutions: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Publisher {
    #[serde(default)]
    pub author: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Allow {
    #[serde(default)]
    pub identities: Vec<String>,
    #[serde(default)]
    pub gitleaks_rules: Vec<String>,
    #[serde(default = "default_presidio_entities")]
    pub presidio_entities: Vec<String>,
}

impl Default for Allow {
    fn default() -> Self {
        Self {
            identities: Vec::new(),
            gitleaks_rules: Vec::new(),
            presidio_entities: default_presidio_entities(),
        }
    }
}

fn default_schema() -> u32 {
    CONFIG_SCHEMA
}

fn default_presidio_entities() -> Vec<String> {
    vec!["PERSON".to_string(), "LOCATION".to_string()]
}

fn default_languages() -> Vec<String> {
    vec!["en".to_string(), "zh".to_string()]
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema: CONFIG_SCHEMA,
            publisher: Publisher::default(),
            languages: default_languages(),
            allow: Allow::default(),
            path_substitutions: BTreeMap::new(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => {
                return Err(error).with_context(|| format!("failed to read {}", path.display()));
            }
        };
        let config: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("{} is not valid publish configuration", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema != CONFIG_SCHEMA {
            bail!("unsupported publish configuration schema {}", self.schema);
        }
        if self.languages.is_empty() {
            bail!("publish configuration must enable at least one language");
        }
        for language in &self.languages {
            if !matches!(language.as_str(), "en" | "zh") {
                bail!("unsupported publish language `{language}`; expected en or zh");
            }
        }
        for (from, to) in &self.path_substitutions {
            if from.is_empty() {
                bail!("path substitution keys must not be empty");
            }
            if to.contains('"') || to.contains('\\') {
                bail!("path substitution `{from}` must not introduce quotes or backslashes");
            }
        }
        Ok(())
    }

    pub fn apply_substitutions(&self, text: &str) -> String {
        let mut text = text.to_string();
        for (from, to) in &self.path_substitutions {
            if text.contains(from.as_str()) {
                text = text.replace(from.as_str(), to);
            }
        }
        text
    }
}

pub fn config_path() -> Result<PathBuf> {
    dirs::config_dir()
        .map(|dir| dir.join("recall").join("publish.json"))
        .context("no configuration directory is available on this platform")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_entities_are_allowed_by_default_and_can_be_switched_back_on() {
        assert_eq!(Config::default().allow.presidio_entities, ["PERSON", "LOCATION"]);
        let strict: Config =
            serde_json::from_str(r#"{"schema":1,"allow":{"presidio_entities":[]}}"#).unwrap();
        assert!(strict.allow.presidio_entities.is_empty());
    }

    #[test]
    fn missing_configuration_falls_back_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load(&dir.path().join("publish.json")).unwrap();
        assert_eq!(config.languages, vec!["en".to_string(), "zh".to_string()]);
        assert!(config.publisher.author.is_none());
    }

    #[test]
    fn substitutions_rewrite_local_paths_without_marking_them_redacted() {
        let mut config = Config::default();
        config.path_substitutions.insert("/Users/x/git".to_string(), "~/git".to_string());
        assert_eq!(
            config.apply_substitutions("open /Users/x/git/Recall/src/lib.rs"),
            "open ~/git/Recall/src/lib.rs"
        );
    }

    #[test]
    fn substitution_targets_cannot_break_json_strings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("publish.json");
        fs::write(&path, br#"{"schema":1,"path_substitutions":{"/Users/x":"\"evil"}}"#).unwrap();
        assert!(Config::load(&path).is_err());
    }

    #[test]
    fn unsupported_language_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("publish.json");
        fs::write(&path, br#"{"schema":1,"languages":["fr"]}"#).unwrap();
        assert!(Config::load(&path).is_err());
    }
}
