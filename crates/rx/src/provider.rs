use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use anyhow::{Result, bail};
use serde::Deserialize;

use crate::config::{ProviderConfig, RxConfig};

pub(crate) const NONE: &str = "none";

const SNAPSHOT_JSON: &str = include_str!("../data/providers.json");

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
    #[serde(default)]
    anthropic_base: Option<String>,
    #[serde(default)]
    default_context: Option<i64>,
    #[serde(default)]
    dsh_protocol: Option<ModelProtocol>,
    #[serde(default)]
    model_capabilities: BTreeMap<String, BTreeMap<ModelProtocol, ProtocolCapabilities>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
pub(crate) enum ModelProtocol {
    #[serde(rename = "openai-completions")]
    OpenAiCompletions,
    #[serde(rename = "openai-responses")]
    OpenAiResponses,
}

impl ModelProtocol {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiCompletions => "openai-completions",
            Self::OpenAiResponses => "openai-responses",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ReasoningLevel {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ReasoningLevel {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ReasoningControl {
    Fixed,
    Effort { levels: BTreeMap<ReasoningLevel, Option<String>> },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProtocolCapabilities {
    #[serde(default)]
    reasoning: Option<ReasoningControl>,
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
    pub anthropic_base: Option<String>,
    pub default_context: Option<i64>,
    pub env: String,
    pub setup: Setup,
    pub default_model: Option<&'static str>,
    pub claude_default_model: Option<&'static str>,
}

pub(crate) fn catalog() -> &'static [Provider] {
    static CATALOG: OnceLock<Vec<Provider>> = OnceLock::new();
    CATALOG.get_or_init(|| snapshot().providers.iter().cloned().map(from_snapshot).collect())
}

pub(crate) fn find(id: &str) -> Option<&'static Provider> {
    catalog().iter().find(|provider| provider.id == id)
}

pub(crate) fn reasoning_control(
    provider_id: &str,
    endpoint: &str,
    model_id: &str,
    protocol: ModelProtocol,
) -> Option<&'static ReasoningControl> {
    let provider = snapshot().providers.iter().find(|provider| provider.id == provider_id)?;
    if crate::catalog::openai_base(&provider.endpoint) != crate::catalog::openai_base(endpoint) {
        return None;
    }
    provider.model_capabilities.get(model_id)?.get(&protocol)?.reasoning.as_ref()
}

pub(crate) fn dsh_protocol(provider_id: &str, endpoint: &str) -> ModelProtocol {
    snapshot()
        .providers
        .iter()
        .find(|provider| {
            provider.id == provider_id
                && crate::catalog::openai_base(&provider.endpoint)
                    == crate::catalog::openai_base(endpoint)
        })
        .and_then(|provider| provider.dsh_protocol)
        .unwrap_or(ModelProtocol::OpenAiCompletions)
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
        if let Some(anthropic_base) = entry.and_then(|entry| entry.anthropic_base.as_ref()) {
            provider.anthropic_base = Some(anthropic_base.clone());
        } else if entry.and_then(|entry| entry.base_url.as_ref()).is_some() {
            // A base_url override points at a different origin, so the bundled
            // Anthropic endpoint no longer applies. Clear it so claude_base
            // falls back to the overridden endpoint instead of sending the key
            // to the original provider.
            provider.anthropic_base = None;
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
        anthropic_base: entry.anthropic_base.clone(),
        default_context: None,
        env: entry.env.clone().unwrap_or_else(|| generated_env(id)),
        setup: Setup::Generated,
        default_model: None,
        claude_default_model: None,
    })
}

pub(crate) fn claude_base(provider: &Provider) -> String {
    crate::catalog::anthropic_base(provider.anthropic_base.as_deref().unwrap_or(&provider.endpoint))
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

pub(crate) fn is_none(id: &str) -> bool {
    id == NONE
}

pub(crate) fn validate_id(id: &str) -> Result<()> {
    if is_none(id) {
        bail!("'{NONE}' is reserved; pass --provider none or run: rx providers use none");
    }
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
        anthropic_base: provider.anthropic_base,
        default_context: provider.default_context,
        env: provider.env,
        setup,
        default_model,
        claude_default_model,
    }
}

fn snapshot() -> &'static Snapshot {
    static SNAPSHOT: OnceLock<Snapshot> = OnceLock::new();
    SNAPSHOT.get_or_init(|| {
        let snapshot: Snapshot = serde_json::from_str(SNAPSHOT_JSON)
            .expect("bundled provider catalog must be valid JSON");
        validate_snapshot(&snapshot);
        snapshot
    })
}

fn validate_snapshot(snapshot: &Snapshot) {
    for provider in &snapshot.providers {
        for (model_id, protocols) in &provider.model_capabilities {
            assert!(!model_id.trim().is_empty(), "bundled model capability ID must not be empty");
            for capabilities in protocols.values() {
                let Some(ReasoningControl::Effort { levels }) = &capabilities.reasoning else {
                    continue;
                };
                assert!(
                    levels.keys().any(|level| *level != ReasoningLevel::Off),
                    "bundled effort control must offer a thinking level"
                );
                for (level, wire) in levels {
                    match wire {
                        Some(wire) => assert!(
                            !wire.trim().is_empty(),
                            "bundled reasoning wire value must not be empty"
                        ),
                        None => assert_eq!(
                            *level,
                            ReasoningLevel::Off,
                            "only the off reasoning level may omit a wire value"
                        ),
                    }
                }
            }
        }
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
