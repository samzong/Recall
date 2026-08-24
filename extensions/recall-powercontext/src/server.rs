use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use ureq::Agent;
use url::{Host, Url};

use crate::session::CaptureRequest;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureResult {
    Accepted { position: u64 },
    Conflict,
    Failed,
}

pub struct PowerContextClient {
    agent: Agent,
    base: String,
    token: Option<String>,
}

impl PowerContextClient {
    pub fn new(base: String, token: Option<String>) -> Self {
        let config = Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_global(Some(REQUEST_TIMEOUT))
            .build();
        Self { agent: config.into(), base, token }
    }

    pub fn journal_watermark(&self, scope_id: &str) -> Result<u64> {
        let url = format!("{}/v1/stats", self.base);
        let mut request = self.agent.get(&url).query("scope_id", scope_id);
        if let Some(token) = &self.token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        let mut response = match request.call() {
            Ok(response) => response,
            Err(error) => return map_transport(error),
        };
        let status = response.status().as_u16();
        let body = response.body_mut().read_to_string().context("failed to read /v1/stats")?;
        if status != 200 {
            bail!("PowerContext /v1/stats failed with HTTP {status}");
        }
        parse_watermark(&body).context("invalid PowerContext /v1/stats response")
    }

    pub fn capture(&self, scope_id: &str, request: &CaptureRequest) -> Result<CaptureResult> {
        let url = format!("{}/v1/sources/content", self.base);
        let mut metadata = json!({
            "kind": "recall-session",
            "adapter": request.adapter,
            "session_id": request.session_id,
            "seq": request.seq,
        });
        if let Some(started_at) = &request.started_at {
            metadata["started_at"] = Value::String(started_at.clone());
        }
        let payload = json!({
            "scope_id": scope_id,
            "source_id": request.source_id,
            "content": request.content,
            "metadata": metadata,
        });
        let mut http_request = self.agent.post(&url).header("Content-Type", "application/json");
        if let Some(token) = &self.token {
            http_request = http_request.header("Authorization", format!("Bearer {token}"));
        }
        let mut response = match http_request.send(payload.to_string()) {
            Ok(response) => response,
            Err(error) => return map_transport(error),
        };
        let status = response.status().as_u16();
        let body =
            response.body_mut().read_to_string().context("failed to read /v1/sources/content")?;
        match status {
            202 => Ok(CaptureResult::Accepted {
                position: parse_capture_response(&body, &request.source_id)
                    .context("invalid PowerContext /v1/sources/content response")?,
            }),
            409 if is_source_conflict(&body) => Ok(CaptureResult::Conflict),
            _ => {
                eprintln!("powercontext capture failed for {}: HTTP {status}", request.source_id);
                Ok(CaptureResult::Failed)
            }
        }
    }
}

fn map_transport<T>(error: ureq::Error) -> Result<T> {
    if is_unreachable(&error) {
        bail!("PowerContext Server is unreachable: {error}");
    }
    Err(anyhow!(error))
}

fn is_unreachable(error: &ureq::Error) -> bool {
    matches!(
        error,
        ureq::Error::Io(_)
            | ureq::Error::ConnectionFailed
            | ureq::Error::HostNotFound
            | ureq::Error::Timeout(_)
    )
}

pub fn validate_server_url(raw: &str) -> Result<String> {
    let value = raw.trim();
    let url = Url::parse(value).context("invalid server URL")?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("server URL must be http:// or https://");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("server URL must not include userinfo");
    }
    if url.query().is_some() || url.fragment().is_some() {
        bail!("server URL must not include a query or fragment");
    }
    let loopback = match url.host() {
        Some(Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(host)) => host.is_loopback(),
        Some(Host::Ipv6(host)) => host.is_loopback(),
        None => false,
    };
    if !loopback {
        bail!("refusing non-loopback server URL: {value}");
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}

fn parse_watermark(body: &str) -> Result<u64> {
    #[derive(Deserialize)]
    struct Stats {
        inventory: Inventory,
    }
    #[derive(Deserialize)]
    struct Inventory {
        sources: Sources,
    }
    #[derive(Deserialize)]
    struct Sources {
        total: u64,
    }
    Ok(serde_json::from_str::<Stats>(body)?.inventory.sources.total)
}

fn parse_capture_response(body: &str, expected_source_id: &str) -> Result<u64> {
    #[derive(Deserialize)]
    struct Capture {
        status: String,
        source: Source,
        position: u64,
    }
    #[derive(Deserialize)]
    struct Source {
        name: String,
        source_id: String,
    }
    let capture: Capture = serde_json::from_str(body)?;
    if capture.status != "accepted"
        || capture.source.name != "content"
        || capture.source.source_id != expected_source_id
        || capture.position == 0
    {
        bail!("capture response does not match the request");
    }
    Ok(capture.position)
}

fn is_source_conflict(body: &str) -> bool {
    #[derive(Deserialize)]
    struct Envelope {
        error: ErrorBody,
    }
    #[derive(Deserialize)]
    struct ErrorBody {
        code: String,
    }
    serde_json::from_str::<Envelope>(body)
        .map(|envelope| envelope.error.code == "source_conflict")
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::{is_source_conflict, parse_watermark, validate_server_url};

    #[test]
    fn accepts_loopback_http_and_rejects_lan() {
        assert!(validate_server_url("http://127.0.0.1:8000/").is_ok());
        assert!(validate_server_url("http://[::1]:8000").is_ok());
        assert!(validate_server_url("https://localhost").is_ok());
        let error = validate_server_url("http://192.168.1.9:8000").unwrap_err().to_string();
        assert!(error.contains("non-loopback"), "{error}");
    }

    #[test]
    fn detects_payload_conflict_and_stats_watermark() {
        assert!(is_source_conflict(
            r#"{"error":{"code":"source_conflict","message":"x","details":null}}"#
        ));
        assert!(!is_source_conflict(r#"{"error":{"code":"revision_conflict"}}"#));
        assert_eq!(parse_watermark(r#"{"inventory":{"sources":{"total":7}}}"#).unwrap(), 7);
    }
}
