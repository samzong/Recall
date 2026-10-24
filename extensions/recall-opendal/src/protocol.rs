use crate::error::{Failure, Result};
use serde::Deserialize;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub const CONTROL_LIMIT: usize = 2 * 1024 * 1024;

#[derive(Deserialize)]
pub struct Request {
    transport_version: u64,
    pub timeout_ms: u64,
    #[serde(flatten)]
    pub operation: Operation,
}

#[derive(Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum Operation {
    Probe,
    List { prefix: String, cursor: Option<String>, page_size: u16 },
    Get { key: String, output_path: PathBuf, max_bytes: u64 },
    Put { key: String, input_path: PathBuf, size: u64, sha256: String },
}

pub fn parse(bytes: &[u8]) -> Result<Request> {
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|error| Failure::new("invalid_request", error.to_string()))?;
    if value.get("transport_version").and_then(serde_json::Value::as_u64) != Some(1) {
        return Err(Failure::new("unsupported_protocol", "transport_version must be 1"));
    }
    let request: Request = serde_json::from_value(value)
        .map_err(|error| Failure::new("invalid_request", error.to_string()))?;
    debug_assert_eq!(request.transport_version, 1);
    if request.timeout_ms == 0
        || Instant::now().checked_add(Duration::from_millis(request.timeout_ms)).is_none()
    {
        return Err(Failure::new("invalid_request", "invalid transport deadline"));
    }
    match &request.operation {
        Operation::Probe => {}
        Operation::List { prefix, cursor, page_size } => {
            if !(1..=1000).contains(page_size) {
                return Err(Failure::new(
                    "invalid_request",
                    "page_size must be between 1 and 1000",
                ));
            }
            if !prefix.is_empty() {
                validate_key(
                    prefix
                        .strip_suffix('/')
                        .ok_or_else(|| Failure::new("invalid_request", "prefix must end in /"))?,
                )?;
            }
            if let Some(cursor) = cursor {
                validate_key(cursor)?;
                if !cursor.starts_with(prefix) {
                    return Err(Failure::new("invalid_request", "cursor is outside prefix"));
                }
            }
        }
        Operation::Get { key, output_path, .. } => {
            validate_key(key)?;
            if !output_path.is_absolute() {
                return Err(Failure::new("invalid_request", "output_path must be absolute"));
            }
            match output_path.symlink_metadata() {
                Ok(_) => return Err(Failure::new("conflict", "output_path already exists")),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(Failure::io(error)),
            }
            if !output_path.parent().is_some_and(|parent| parent.is_dir()) {
                return Err(Failure::new("target_missing", "output directory does not exist"));
            }
        }
        Operation::Put { key, input_path, size, sha256 } => {
            validate_key(key)?;
            if !input_path.is_absolute() {
                return Err(Failure::new("invalid_request", "input_path must be absolute"));
            }
            let metadata = input_path.metadata().map_err(Failure::io)?;
            if !metadata.is_file() || metadata.len() != *size {
                return Err(Failure::new("integrity", "upload size mismatch"));
            }
            if sha256.len() != 64
                || !sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err(Failure::new(
                    "invalid_request",
                    "sha256 must be 64 lowercase hexadecimal characters",
                ));
            }
        }
    }
    Ok(request)
}

pub fn validate_key(key: &str) -> Result<()> {
    if key.is_empty()
        || key.len() > 1024
        || !key.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"/_-.".contains(&byte)
        })
        || !key.split('/').all(|part| !matches!(part, "" | "." | ".."))
    {
        return Err(Failure::new("invalid_request", "invalid relative remote key"));
    }
    Ok(())
}
