use std::collections::BTreeSet;

use anyhow::{Result, bail};

use crate::catalog::{self, CatalogShape, ListedModel};
use crate::claude_catalog;
use crate::config::Paths;
use crate::launch::{self, EnvLookup};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Probe {
    pub url: String,
    pub headers: Vec<String>,
    pub status: Option<u16>,
    pub error: Option<String>,
    pub envelope: Vec<String>,
    pub shape: CatalogShape,
    pub models: Vec<ListedModel>,
}

pub(crate) fn run(gateway: Option<String>, paths: &Paths, env: &EnvLookup) -> Result<()> {
    let Some(target) = launch::configured_gateway(gateway.as_deref(), paths, env)? else {
        bail!("no gateway configured; run: rx config set gateway <openrouter|tokener>");
    };
    let auth_header = ("Authorization", format!("Bearer {}", target.key));
    let openai_url = format!("{}/models", launch::openai_base(&target.base_url));
    let codex_url =
        format!("{}/v1/models?client_version=0.149.0", launch::anthropic_base(&target.base_url));
    let anthropic_url =
        format!("{}/v1/models?limit=1000", launch::anthropic_base(&target.base_url));
    let user_url =
        format!("{}/v1/models/user?limit=1000", launch::anthropic_base(&target.base_url));
    let openai = probe(&openai_url, std::slice::from_ref(&auth_header));
    let codex_headers = [auth_header.clone(), ("User-Agent", "codex/0.149.0".to_string())];
    let anthropic_headers = [auth_header.clone(), ("anthropic-version", "2023-06-01".to_string())];
    let codex = probe(&codex_url, &codex_headers);
    let anthropic = probe(&anthropic_url, &anthropic_headers);
    let user = probe_user(&user_url, std::slice::from_ref(&auth_header));
    print!("{}", render(target.spec.id, &target.base_url, &openai, &codex, &anthropic, &user));
    Ok(())
}

fn probe(url: &str, headers: &[(&str, String)]) -> Probe {
    let shown_headers = shown_headers(headers);
    match catalog::fetch(url, headers) {
        Ok((status, body)) => {
            if !(200..300).contains(&status) {
                return failed_probe(
                    url,
                    shown_headers,
                    Some(status),
                    catalog::truncate(&body, 300),
                );
            }
            match catalog::parse_catalog(&body) {
                Ok((shape, envelope, models)) => Probe {
                    url: url.to_string(),
                    headers: shown_headers,
                    status: Some(status),
                    error: None,
                    envelope,
                    shape,
                    models,
                },
                Err(error) => failed_probe(
                    url,
                    shown_headers,
                    Some(status),
                    format!("{error}; body: {}", catalog::truncate(&body, 200)),
                ),
            }
        }
        Err(error) => failed_probe(url, shown_headers, None, error.to_string()),
    }
}

fn probe_user(url: &str, headers: &[(&str, String)]) -> Probe {
    let shown_headers = shown_headers(headers);
    match catalog::fetch(url, headers) {
        Ok((status, body)) => {
            if !(200..300).contains(&status) {
                return failed_probe(
                    url,
                    shown_headers,
                    Some(status),
                    catalog::truncate(&body, 300),
                );
            }
            let value: serde_json::Value = match serde_json::from_str(&body) {
                Ok(value) => value,
                Err(error) => {
                    return failed_probe(
                        url,
                        shown_headers,
                        Some(status),
                        format!(
                            "catalog response is not JSON: {error}; body: {}",
                            catalog::truncate(&body, 200)
                        ),
                    );
                }
            };
            match claude_catalog::parse_user_catalog_value(&value) {
                Ok(models) => {
                    let envelope = value
                        .as_object()
                        .map(|object| object.keys().cloned().collect())
                        .unwrap_or_default();
                    Probe {
                        url: url.to_string(),
                        headers: shown_headers,
                        status: Some(status),
                        error: None,
                        envelope,
                        shape: CatalogShape::OpenAi,
                        models: models
                            .into_iter()
                            .map(|model| ListedModel { id: model.id, label: model.name })
                            .collect(),
                    }
                }
                Err(error) => failed_probe(
                    url,
                    shown_headers,
                    Some(status),
                    format!("{error}; body: {}", catalog::truncate(&body, 200)),
                ),
            }
        }
        Err(error) => failed_probe(url, shown_headers, None, error.to_string()),
    }
}

fn shown_headers(headers: &[(&str, String)]) -> Vec<String> {
    headers
        .iter()
        .map(|(name, value)| {
            if name.eq_ignore_ascii_case("authorization") {
                format!("{name}: Bearer")
            } else if name.eq_ignore_ascii_case("x-api-key") {
                format!("{name}: (set)")
            } else {
                format!("{name}: {value}")
            }
        })
        .collect()
}

fn failed_probe(url: &str, headers: Vec<String>, status: Option<u16>, error: String) -> Probe {
    Probe {
        url: url.to_string(),
        headers,
        status,
        error: Some(error),
        envelope: Vec::new(),
        shape: CatalogShape::Unknown,
        models: Vec::new(),
    }
}

pub(crate) fn render(
    gateway: &str,
    base_url: &str,
    openai: &Probe,
    codex: &Probe,
    anthropic: &Probe,
    user: &Probe,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("gateway: {gateway}\n"));
    out.push_str(&format!("base_url: {base_url}\n"));
    out.push_str("\n## OpenAI (GET /v1/models)\n");
    write_probe(&mut out, openai);
    out.push_str("\n## Codex (GET /v1/models?client_version=...)\n");
    write_probe(&mut out, codex);
    out.push_str("\n## Anthropic discovery (GET /v1/models?limit=1000)\n");
    write_probe(&mut out, anthropic);
    out.push_str("\n## Claude seed (GET /v1/models/user?limit=1000)\n");
    write_probe(&mut out, user);
    let openai_ids = ids(openai);
    let anthropic_ids = ids(anthropic);
    let only_openai: Vec<_> = openai_ids.difference(&anthropic_ids).cloned().collect();
    let only_anthropic: Vec<_> = anthropic_ids.difference(&openai_ids).cloned().collect();
    let both = openai_ids.intersection(&anthropic_ids).count();
    out.push_str("\n## Diff OpenAI list vs Anthropic discovery (raw id)\n");
    out.push_str(&format!(
        "in both: {both}\nonly OpenAI: {}\nonly Anthropic: {}\n",
        only_openai.len(),
        only_anthropic.len()
    ));
    write_id_list(&mut out, "only OpenAI", &only_openai);
    write_id_list(&mut out, "only Anthropic", &only_anthropic);
    let kept = anthropic.models.iter().filter(|model| claude_keeps(&model.id)).count();
    out.push_str(&format!(
        "\nClaude discovery filter (id contains claude|anthropic): {kept}/{}\n",
        anthropic.models.len()
    ));
    out.push_str(&format!("Claude user catalog models: {}\n", user.models.len()));
    out
}

fn write_probe(out: &mut String, probe: &Probe) {
    out.push_str(&format!("GET {}\n", probe.url));
    for header in &probe.headers {
        out.push_str(&format!("{header}\n"));
    }
    match probe.status {
        Some(status) => out.push_str(&format!("HTTP {status}\n")),
        None => out.push_str("HTTP (request failed)\n"),
    }
    if let Some(error) = &probe.error {
        out.push_str(&format!("error: {error}\n"));
        return;
    }
    out.push_str(&format!(
        "{} models, {} envelope ({})\n",
        probe.models.len(),
        shape_name(probe.shape),
        probe.envelope.join(", ")
    ));
    for model in &probe.models {
        match &model.label {
            Some(label) if label != &model.id => {
                out.push_str(&format!("  {}  {label}\n", model.id));
            }
            _ => out.push_str(&format!("  {}\n", model.id)),
        }
    }
}

fn write_id_list(out: &mut String, title: &str, ids: &[String]) {
    if ids.is_empty() {
        return;
    }
    out.push_str(&format!("{title}:\n"));
    for id in ids {
        out.push_str(&format!("  {id}\n"));
    }
}

fn ids(probe: &Probe) -> BTreeSet<String> {
    probe.models.iter().map(|model| model.id.clone()).collect()
}

fn claude_keeps(id: &str) -> bool {
    let lower = id.to_ascii_lowercase();
    lower.contains("claude") || lower.contains("anthropic")
}

fn shape_name(shape: CatalogShape) -> &'static str {
    match shape {
        CatalogShape::OpenAi => "openai",
        CatalogShape::Anthropic => "anthropic",
        CatalogShape::Codex => "codex",
        CatalogShape::Unknown => "unknown",
    }
}
