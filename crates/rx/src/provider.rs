use std::collections::BTreeSet;
use std::sync::OnceLock;

use anyhow::{Result, bail};
use serde::Deserialize;

use crate::config::{ProviderConfig, RxConfig};

const SNAPSHOT: &str = include_str!("../data/providers.json");

#[derive(Debug, Deserialize)]
struct Snapshot {
    providers: Vec<SnapshotProvider>,
}

#[derive(Debug, Clone, Deserialize)]
struct SnapshotProvider {
    id: String,
    name: String,
    endpoint: String,
    env: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Setup {
    OpenRouter,
    Generated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Provider {
    pub id: String,
    pub name: String,
    pub endpoint: String,
    pub env: String,
    pub setup: Setup,
    pub default_model: Option<&'static str>,
    pub claude_default_model: Option<&'static str>,
}

pub(crate) fn catalog() -> &'static [Provider] {
    static CATALOG: OnceLock<Vec<Provider>> = OnceLock::new();
    CATALOG.get_or_init(|| {
        let snapshot: Snapshot =
            serde_json::from_str(SNAPSHOT).expect("bundled provider catalog must be valid JSON");
        snapshot.providers.into_iter().map(from_snapshot).collect()
    })
}

pub(crate) fn find(id: &str) -> Option<&'static Provider> {
    catalog().iter().find(|provider| provider.id == id)
}

pub(crate) fn resolve(id: &str, entry: Option<&ProviderConfig>) -> Result<Provider> {
    validate_id(id)?;
    if let Some(provider) = find(id) {
        let mut provider = provider.clone();
        if let Some(base_url) = entry.and_then(|entry| entry.base_url.as_ref()) {
            provider.endpoint.clone_from(base_url);
        }
        if let Some(env) = entry.and_then(|entry| entry.env.as_ref()) {
            provider.env.clone_from(env);
        }
        return Ok(provider);
    }
    let Some(entry) = entry else {
        bail!(
            "unknown provider: {id} (define it in rx.toml or choose one from rx providers login)"
        );
    };
    let Some(endpoint) = entry.base_url.as_ref() else {
        bail!("custom provider '{id}' must set base_url to an OpenAI-compatible /v1 endpoint");
    };
    Ok(Provider {
        id: id.to_string(),
        name: id.to_string(),
        endpoint: endpoint.clone(),
        env: entry.env.clone().unwrap_or_else(|| generated_env(id)),
        setup: Setup::Generated,
        default_model: None,
        claude_default_model: None,
    })
}

pub(crate) fn available(config: &RxConfig) -> Result<Vec<Provider>> {
    let mut providers = catalog().to_vec();
    let known: BTreeSet<String> = providers.iter().map(|provider| provider.id.clone()).collect();
    for (id, entry) in &config.provider {
        if !known.contains(id.as_str()) && entry.base_url.is_some() {
            providers.push(resolve(id, Some(entry))?);
        }
    }
    Ok(providers)
}

pub(crate) fn validate_id(id: &str) -> Result<()> {
    if !id.is_empty()
        && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Ok(());
    }
    bail!("invalid provider name '{id}'; use only letters, numbers, '-' and '_'")
}

fn from_snapshot(provider: SnapshotProvider) -> Provider {
    let (setup, default_model, claude_default_model) = match provider.id.as_str() {
        "openrouter" => {
            (Setup::OpenRouter, Some("~openai/gpt-latest"), Some("~anthropic/claude-sonnet-latest"))
        }
        "tokener" => (Setup::Generated, None, None),
        _ => (Setup::Generated, None, None),
    };
    Provider {
        id: provider.id,
        name: provider.name,
        endpoint: provider.endpoint,
        env: provider.env,
        setup,
        default_model,
        claude_default_model,
    }
}

fn generated_env(id: &str) -> String {
    let id =
        id.chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() { character.to_ascii_uppercase() } else { '_' }
            })
            .collect::<String>();
    format!("RX_PROVIDER_{id}_API_KEY")
}
