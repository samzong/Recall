use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(crate) const MESSAGE_LIMIT: u64 = 2 * 1024 * 1024;

#[derive(Debug, Deserialize)]
pub(crate) struct Request {
    pub timeout_ms: u64,
    #[serde(flatten)]
    pub operation: Operation,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub(crate) enum Operation {
    Probe,
    List { prefix: String, cursor: Option<String>, page_size: u16 },
    Get { key: String, output_path: PathBuf, max_bytes: u64 },
    Put { key: String, input_path: PathBuf, size: u64, sha256: String },
}

#[derive(Debug, Serialize)]
pub(crate) struct Failure {
    pub code: &'static str,
    pub message: &'static str,
}

pub(crate) type Result<T> = std::result::Result<T, Failure>;

impl Failure {
    pub fn new(code: &'static str, message: &'static str) -> Self {
        Self { code, message }
    }

    pub fn invalid(message: &'static str) -> Self {
        Self::new("invalid_request", message)
    }

    pub fn integrity(message: &'static str) -> Self {
        Self::new("integrity", message)
    }

    pub fn io() -> Self {
        Self::new("unavailable", "local file operation failed")
    }
}

impl Request {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() as u64 > MESSAGE_LIMIT {
            return Err(Failure::invalid("request exceeds the control message limit"));
        }
        let envelope: Value = serde_json::from_slice(bytes)
            .map_err(|_| Failure::invalid("expected one complete JSON request"))?;
        let version = envelope
            .get("transport_version")
            .and_then(Value::as_u64)
            .ok_or_else(|| Failure::invalid("transport_version must be an integer"))?;
        if version != 1 {
            return Err(Failure::new("unsupported_protocol", "transport version is not supported"));
        }
        let request: Self = serde_json::from_value(envelope)
            .map_err(|_| Failure::invalid("invalid operation or request fields"))?;
        if request.timeout_ms == 0 {
            return Err(Failure::invalid("timeout_ms must be positive"));
        }
        match &request.operation {
            Operation::Probe => {}
            Operation::List { prefix, page_size, .. } => {
                validate_key(prefix, true)?;
                if !(1..=1000).contains(page_size) {
                    return Err(Failure::invalid("page_size must be between 1 and 1000"));
                }
            }
            Operation::Get { key, output_path, .. } => {
                validate_key(key, false)?;
                if !output_path.is_absolute() {
                    return Err(Failure::invalid("transfer paths must be absolute"));
                }
            }
            Operation::Put { key, input_path, size, sha256 } => {
                validate_key(key, false)?;
                if !input_path.is_absolute() || *size > i64::MAX as u64 {
                    return Err(Failure::invalid("invalid upload path or size"));
                }
                if sha256.len() != 64
                    || !sha256.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                {
                    return Err(Failure::invalid(
                        "sha256 must be 64 lowercase hexadecimal characters",
                    ));
                }
            }
        }
        Ok(request)
    }
}

pub(crate) fn response(result: Result<Value>) -> (Vec<u8>, bool) {
    let success = result.is_ok();
    let value = match result {
        Ok(result) => json!({"transport_version": 1, "result": result}),
        Err(error) => json!({"transport_version": 1, "error": error}),
    };
    let bytes = serde_json::to_vec(&value).expect("JSON values serialize");
    if bytes.len() as u64 > MESSAGE_LIMIT {
        return response(Err(Failure::integrity("response exceeds the control message limit")));
    }
    (bytes, success)
}

pub(crate) fn validate_key(key: &str, prefix: bool) -> Result<()> {
    if prefix && key.is_empty() {
        return Ok(());
    }
    let key = if prefix {
        key.strip_suffix('/').ok_or_else(|| Failure::invalid("list prefix must end with /"))?
    } else {
        key
    };
    if key.is_empty()
        || !key
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"/_-.".contains(&b))
        || key.split('/').any(|segment| matches!(segment, "" | "." | ".."))
    {
        return Err(Failure::invalid("invalid relative object key"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_rejects_unsupported_and_trailing_messages() {
        let unknown =
            Request::parse(br#"{"transport_version":2,"timeout_ms":1,"operation":"future"}"#)
                .unwrap_err();
        assert_eq!(unknown.code, "unsupported_protocol");
        let extra = br#"{"transport_version":1,"timeout_ms":1,"operation":"probe"} {}"#;
        assert!(Request::parse(extra).is_err());
        assert!(Request::parse(&vec![b' '; MESSAGE_LIMIT as usize + 1]).is_err());
        let request =
            Request::parse(br#"{"transport_version":1,"timeout_ms":1,"operation":"probe"}"#)
                .unwrap();
        assert!(matches!(request.operation, Operation::Probe));
    }

    #[test]
    fn oversized_response_fails_without_truncating_a_page() {
        let (bytes, success) =
            response(Ok(json!({"objects": ["x".repeat(MESSAGE_LIMIT as usize)]})));
        assert!(!success);
        let envelope: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(envelope["error"]["code"], "integrity");
        assert!(envelope.get("result").is_none());
    }

    #[test]
    fn keys_cannot_escape_or_alias_the_target_root() {
        for key in ["", "/a", "a/", "a//b", "a/../b", "a/./b", "a\\b", "a%2fb", "A", "中"] {
            assert!(validate_key(key, false).is_err(), "{key}");
        }
        assert!(validate_key("revisions/0123.json", false).is_ok());
        assert!(validate_key("", true).is_ok());
        assert!(validate_key("revisions/", true).is_ok());
        assert!(validate_key("revisions", true).is_err());
    }
}
