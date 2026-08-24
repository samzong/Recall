use std::time::Duration;

use anyhow::{Context, Result, bail};

pub(crate) fn fetch_get(url: &str, headers: &[(&str, String)]) -> Result<String> {
    let (status, body) = fetch(url, headers)?;
    if !(200..300).contains(&status) {
        bail!("HTTP {status}: {}", truncate(&body, 300));
    }
    Ok(body)
}

fn fetch(url: &str, headers: &[(&str, String)]) -> Result<(u16, String)> {
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

fn truncate(value: &str, max: usize) -> String {
    let trimmed = value.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    let cut: String = trimmed.chars().take(max).collect();
    format!("{cut}...")
}
