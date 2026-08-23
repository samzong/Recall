use std::time::Duration;

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CatalogShape {
    OpenAi,
    Anthropic,
    Codex,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ListedModel {
    pub id: String,
    pub label: Option<String>,
}

pub(crate) fn fetch_get(url: &str, headers: &[(&str, String)]) -> Result<String> {
    let (status, body) = fetch(url, headers)?;
    if !(200..300).contains(&status) {
        bail!("HTTP {status}: {}", truncate(&body, 300));
    }
    Ok(body)
}

pub(crate) fn fetch(url: &str, headers: &[(&str, String)]) -> Result<(u16, String)> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .http_status_as_error(false)
        .build()
        .into();
    let mut request = agent.get(url).header("User-Agent", format!("rx/{}", crate::RELEASE_VERSION));
    for (name, value) in headers {
        request = request.header(*name, value);
    }
    let mut response = request.call().with_context(|| format!("GET {url}"))?;
    let status = response.status().as_u16();
    let body = response.body_mut().read_to_string().context("failed to read response body")?;
    Ok((status, body))
}

pub(crate) fn parse_catalog(body: &str) -> Result<(CatalogShape, Vec<String>, Vec<ListedModel>)> {
    let value: serde_json::Value = serde_json::from_str(body).context("response is not JSON")?;
    let envelope =
        value.as_object().map(|object| object.keys().cloned().collect()).unwrap_or_default();
    if let Some(models) = value.get("models").and_then(|value| value.as_array()) {
        let listed = models
            .iter()
            .filter_map(|entry| {
                let id = entry
                    .get("slug")
                    .or_else(|| entry.get("id"))
                    .and_then(|value| value.as_str())?
                    .to_string();
                let label = entry
                    .get("display_name")
                    .or_else(|| entry.get("name"))
                    .and_then(|value| value.as_str())
                    .map(one_line);
                Some(ListedModel { id, label })
            })
            .collect::<Vec<_>>();
        return Ok((CatalogShape::Codex, envelope, listed));
    }
    let Some(data) = value.get("data").and_then(|value| value.as_array()) else {
        bail!("JSON has no data or models array");
    };
    let models = data
        .iter()
        .filter_map(|entry| {
            let id = entry.get("id")?.as_str()?.to_string();
            let label = entry
                .get("display_name")
                .or_else(|| entry.get("name"))
                .and_then(|value| value.as_str())
                .map(one_line);
            Some(ListedModel { id, label })
        })
        .collect::<Vec<_>>();
    let shape = match data.first() {
        Some(entry) if entry.get("display_name").is_some() => CatalogShape::Anthropic,
        Some(entry) if entry.get("name").is_some() => CatalogShape::OpenAi,
        _ => CatalogShape::Unknown,
    };
    Ok((shape, envelope, models))
}

pub(crate) fn truncate(value: &str, max: usize) -> String {
    let trimmed = value.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    let cut: String = trimmed.chars().take(max).collect();
    format!("{cut}...")
}

fn one_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}
