use std::collections::BTreeSet;

use anyhow::{Result, bail};

use crate::catalog::{self, CatalogShape, ListedModel};
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
    let openai_url = format!("{}/models", launch::openai_base(&target.base_url));
    let anthropic_url =
        format!("{}/v1/models?limit=1000", launch::anthropic_base(&target.base_url));
    let openai = probe(&openai_url, &[("Authorization", format!("Bearer {}", target.key))]);
    let anthropic = probe(
        &anthropic_url,
        &[
            ("Authorization", format!("Bearer {}", target.key)),
            ("anthropic-version", "2023-06-01".to_string()),
        ],
    );
    print!("{}", render(target.spec.id, &target.base_url, &openai, &anthropic));
    Ok(())
}

fn probe(url: &str, headers: &[(&str, String)]) -> Probe {
    let shown_headers = headers
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
        .collect();
    match catalog::fetch(url, headers) {
        Ok((status, body)) => {
            if !(200..300).contains(&status) {
                return Probe {
                    url: url.to_string(),
                    headers: shown_headers,
                    status: Some(status),
                    error: Some(catalog::truncate(&body, 300)),
                    envelope: Vec::new(),
                    shape: CatalogShape::Unknown,
                    models: Vec::new(),
                };
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
                Err(error) => Probe {
                    url: url.to_string(),
                    headers: shown_headers,
                    status: Some(status),
                    error: Some(format!("{error}; body: {}", catalog::truncate(&body, 200))),
                    envelope: Vec::new(),
                    shape: CatalogShape::Unknown,
                    models: Vec::new(),
                },
            }
        }
        Err(error) => Probe {
            url: url.to_string(),
            headers: shown_headers,
            status: None,
            error: Some(error.to_string()),
            envelope: Vec::new(),
            shape: CatalogShape::Unknown,
            models: Vec::new(),
        },
    }
}

pub(crate) fn render(gateway: &str, base_url: &str, openai: &Probe, anthropic: &Probe) -> String {
    let mut out = String::new();
    out.push_str(&format!("gateway: {gateway}\n"));
    out.push_str(&format!("base_url: {base_url}\n"));
    out.push_str("\n## OpenAI (Codex)\n");
    write_probe(&mut out, openai);
    out.push_str("\n## Anthropic (Claude Code)\n");
    write_probe(&mut out, anthropic);
    let openai_ids = ids(openai);
    let anthropic_ids = ids(anthropic);
    let only_openai: Vec<_> = openai_ids.difference(&anthropic_ids).cloned().collect();
    let only_anthropic: Vec<_> = anthropic_ids.difference(&openai_ids).cloned().collect();
    let both = openai_ids.intersection(&anthropic_ids).count();
    out.push_str("\n## Diff (raw id)\n");
    out.push_str(&format!(
        "in both: {both}\nonly OpenAI: {}\nonly Anthropic: {}\n",
        only_openai.len(),
        only_anthropic.len()
    ));
    write_id_list(&mut out, "only OpenAI", &only_openai);
    write_id_list(&mut out, "only Anthropic", &only_anthropic);
    let kept = anthropic.models.iter().filter(|model| claude_keeps(&model.id)).count();
    out.push_str(&format!(
        "\nClaude filter (id contains claude|anthropic): {kept}/{}\n",
        anthropic.models.len()
    ));
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
        CatalogShape::Unknown => "unknown",
    }
}
