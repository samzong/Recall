use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

const VERSION: u32 = 1;
const CONTROL_LIMIT: usize = 2 * 1024 * 1024;
const DIAGNOSTIC_LIMIT: usize = 64 * 1024;

#[derive(Debug, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub(crate) enum Operation {
    Probe,
    List { prefix: String, cursor: Option<String>, page_size: u16 },
    Get { key: String, output_path: PathBuf, max_bytes: u64 },
    Put { key: String, input_path: PathBuf, size: u64, sha256: String },
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct Object {
    pub(crate) key: String,
    pub(crate) size: u64,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct Page {
    pub(crate) objects: Vec<Object>,
    pub(crate) next_cursor: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Reply {
    Readable,
    Listed(Page),
    Downloaded(u64),
    Published,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RemoteError {
    pub(crate) code: ErrorCode,
    pub(crate) message: String,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ErrorCode {
    NotConfigured,
    InvalidRequest,
    UnsupportedProtocol,
    TargetMissing,
    ObjectMissing,
    Authentication,
    Permission,
    Conflict,
    Integrity,
    Transient,
    Unavailable,
}

impl std::fmt::Display for RemoteError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "remote {:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for RemoteError {}

#[derive(Serialize)]
struct Request<'a> {
    transport_version: u32,
    timeout_ms: u64,
    #[serde(flatten)]
    operation: &'a Operation,
}

#[derive(Deserialize)]
struct Response {
    transport_version: u32,
    result: Option<serde_json::Value>,
    error: Option<RemoteError>,
}

pub(crate) fn invoke(provider: &str, operation: &Operation, budget: Duration) -> Result<Reply> {
    invoke_with_root(provider, operation, budget, &super::extension_root()?)
}

fn invoke_with_root(
    provider: &str,
    operation: &Operation,
    budget: Duration,
    root: &Path,
) -> Result<Reply> {
    super::validate_extension_name(provider)?;
    validate_operation(operation)?;
    let timeout_ms = u64::try_from(budget.as_millis()).context("transport budget is too large")?;
    ensure!(timeout_ms > 0, "transport budget must be positive");
    ensure!(std::time::Instant::now().checked_add(budget).is_some(), "invalid transport deadline");
    let request =
        serde_json::to_vec(&Request { transport_version: VERSION, timeout_ms, operation })?;
    ensure!(request.len() <= CONTROL_LIMIT, "transport request exceeds control limit");
    let binary = super::managed_binary_path(root, provider);
    ensure!(super::is_executable_file(&binary), "remote provider is not installed: {provider}");

    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let (stdout, stderr, success) =
        runtime.block_on(async { exchange(spawn_provider(&binary)?, &request, budget).await })?;
    let response: Response = serde_json::from_slice(&stdout).with_context(|| {
        if stderr.is_empty() {
            "invalid remote transport response".to_string()
        } else {
            format!("invalid remote transport response: {}", String::from_utf8_lossy(&stderr))
        }
    })?;
    ensure!(response.transport_version == VERSION, "incompatible remote transport version");
    match (success, response.result, response.error) {
        (true, Some(result), None) => parse_reply(operation, result),
        (false, None, Some(error)) => Err(error.into()),
        _ => bail!("remote transport response contradicts its exit status or envelope"),
    }
}

fn spawn_provider(binary: &Path) -> Result<tokio::process::Child> {
    tokio::process::Command::new(binary)
        .arg("--recall-remote-transport")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("failed to start remote provider")
}

async fn exchange(
    mut child: tokio::process::Child,
    request: &[u8],
    budget: Duration,
) -> Result<(Vec<u8>, Vec<u8>, bool)> {
    let mut stdin = child.stdin.take().context("remote provider stdin unavailable")?;
    let stdout = child.stdout.take().context("remote provider stdout unavailable")?;
    let stderr = child.stderr.take().context("remote provider stderr unavailable")?;
    let operation = async {
        let (stdout, stderr, (), status) = tokio::try_join!(
            read_control(stdout),
            read_diagnostics(stderr),
            async {
                stdin.write_all(request).await?;
                stdin.shutdown().await?;
                drop(stdin);
                Ok::<_, anyhow::Error>(())
            },
            async { child.wait().await.map_err(anyhow::Error::from) },
        )?;
        Ok::<_, anyhow::Error>((stdout, stderr, status.success()))
    };
    let result = match tokio::time::timeout(budget, operation).await {
        Ok(result) => result,
        Err(_) => Err(RemoteError {
            code: ErrorCode::Transient,
            message: "provider deadline expired; a remote write may already have completed"
                .to_string(),
        }
        .into()),
    };
    if result.is_err() {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    result
}

async fn read_control(reader: impl AsyncRead + Unpin) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(CONTROL_LIMIT as u64 + 1).read_to_end(&mut bytes).await?;
    ensure!(bytes.len() <= CONTROL_LIMIT, "remote response exceeds control limit");
    Ok(bytes)
}

async fn read_diagnostics(mut reader: impl AsyncRead + Unpin) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    (&mut reader).take(DIAGNOSTIC_LIMIT as u64).read_to_end(&mut bytes).await?;
    tokio::io::copy(&mut reader, &mut tokio::io::sink()).await?;
    Ok(bytes)
}

fn validate_operation(operation: &Operation) -> Result<()> {
    match operation {
        Operation::Probe => {}
        Operation::List { prefix, page_size, .. } => {
            ensure!((1..=1000).contains(page_size), "invalid remote page size");
            if !prefix.is_empty() {
                validate_key(prefix.strip_suffix('/').context("remote prefix must end in /")?)?;
            }
        }
        Operation::Get { key, output_path, .. } => {
            validate_key(key)?;
            ensure!(output_path.is_absolute(), "download path must be absolute");
            match output_path.symlink_metadata() {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
                Ok(_) => bail!("download path already exists"),
            }
        }
        Operation::Put { key, input_path, size, sha256 } => {
            validate_key(key)?;
            ensure!(input_path.is_absolute(), "upload path must be absolute");
            let metadata = input_path.metadata()?;
            ensure!(metadata.is_file() && metadata.len() == *size, "upload size mismatch");
            ensure!(
                sha256.len() == 64
                    && sha256
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
                "invalid upload SHA-256"
            );
        }
    }
    Ok(())
}

fn validate_key(key: &str) -> Result<()> {
    ensure!(
        !key.is_empty()
            && key.len() <= 1024
            && key.bytes().all(|byte| byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || b"/_-.".contains(&byte))
            && key.split('/').all(|part| !matches!(part, "" | "." | "..")),
        "invalid relative remote key"
    );
    Ok(())
}

fn parse_reply(operation: &Operation, value: serde_json::Value) -> Result<Reply> {
    match operation {
        Operation::Probe => {
            #[derive(Deserialize)]
            struct Probe {
                readable: bool,
            }
            let probe: Probe = serde_json::from_value(value)?;
            ensure!(probe.readable, "remote probe did not establish readable state");
            Ok(Reply::Readable)
        }
        Operation::List { prefix, page_size, .. } => {
            ensure!(
                value.get("next_cursor").is_some(),
                "remote page is missing its continuation state"
            );
            let page: Page = serde_json::from_value(value)?;
            ensure!(
                page.objects.len() <= usize::from(*page_size),
                "remote page exceeds requested size"
            );
            for object in &page.objects {
                validate_key(&object.key)?;
                ensure!(
                    object.key.starts_with(prefix),
                    "remote object is outside requested prefix"
                );
            }
            Ok(Reply::Listed(page))
        }
        Operation::Get { output_path, max_bytes, .. } => {
            #[derive(Deserialize)]
            struct Download {
                size: u64,
            }
            let download: Download = serde_json::from_value(value)?;
            let metadata = output_path.symlink_metadata()?;
            ensure!(
                metadata.is_file()
                    && metadata.len() == download.size
                    && download.size <= *max_bytes,
                "remote download length mismatch"
            );
            Ok(Reply::Downloaded(download.size))
        }
        Operation::Put { size, sha256, .. } => {
            #[derive(Deserialize)]
            struct Upload {
                size: u64,
                sha256: String,
            }
            let upload: Upload = serde_json::from_value(value)?;
            ensure!(
                upload.size == *size && upload.sha256 == *sha256,
                "remote upload receipt mismatch"
            );
            Ok(Reply::Published)
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn provider(root: &Path, body: &str) {
        let path = super::super::managed_binary_path(root, "r2");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn managed_transport_drains_diagnostics_and_delivers_request() {
        let root = tempfile::tempdir().unwrap();
        provider(
            root.path(),
            r#"
test "$1" = --recall-remote-transport || exit 4
request=$(cat)
case "$request" in *'"operation":"probe"'*) ;; *) exit 5 ;; esac
printf '%0200000d' 0 >&2
printf '%s' '{"transport_version":1,"result":{"readable":true}}'
"#,
        );
        assert_eq!(
            invoke_with_root("r2", &Operation::Probe, Duration::from_secs(5), root.path()).unwrap(),
            Reply::Readable
        );
    }

    #[test]
    fn transport_preserves_errors_and_rejects_false_success() {
        let root = tempfile::tempdir().unwrap();
        provider(
            root.path(),
            r#"cat >/dev/null
printf '%s' '{"transport_version":1,"error":{"code":"permission","message":"denied"}}'
exit 1"#,
        );
        let error = invoke_with_root("r2", &Operation::Probe, Duration::from_secs(5), root.path())
            .unwrap_err();
        assert_eq!(error.downcast_ref::<RemoteError>().unwrap().code, ErrorCode::Permission);
        for body in [
            r#"{"transport_version":2,"result":{"readable":true}}"#,
            r#"{"transport_version":1,"result":{"readable":true},"error":{"code":"permission","message":"denied"}}"#,
            r#"{"transport_version":1,"result":{"readable":false}}"#,
            r#"{"transport_version":1,"result":{"readable":true}} {}"#,
        ] {
            provider(root.path(), &format!("cat >/dev/null\nprintf '%s' '{body}'"));
            assert!(
                invoke_with_root("r2", &Operation::Probe, Duration::from_secs(5), root.path())
                    .is_err()
            );
        }
    }

    #[test]
    fn transport_deadline_covers_unread_stdin_and_reaps_provider() {
        let root = tempfile::tempdir().unwrap();
        provider(root.path(), "exec sleep 30");
        let operation = Operation::List {
            prefix: String::new(),
            cursor: Some("x".repeat(1024 * 1024)),
            page_size: 1,
        };
        let request = serde_json::to_vec(&Request {
            transport_version: VERSION,
            timeout_ms: 200,
            operation: &operation,
        })
        .unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let started = std::time::Instant::now();
        let (result, pid) = runtime.block_on(async {
            let child =
                spawn_provider(&super::super::managed_binary_path(root.path(), "r2")).unwrap();
            let pid = child.id().unwrap();
            (exchange(child, &request, Duration::from_millis(200)).await, pid)
        });
        let error = result.unwrap_err();
        assert_eq!(error.downcast_ref::<RemoteError>().unwrap().code, ErrorCode::Transient);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(
            !std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .output()
                .unwrap()
                .status
                .success()
        );
    }

    #[test]
    fn transport_rejects_oversized_but_valid_response() {
        let root = tempfile::tempdir().unwrap();
        provider(
            root.path(),
            r#"cat >/dev/null
printf '%s' '{"transport_version":1,"result":{"readable":true,"padding":"'
printf '%03000000d' 0
printf '%s' '"}}'"#,
        );
        assert!(
            invoke_with_root("r2", &Operation::Probe, Duration::from_secs(5), root.path()).is_err()
        );
    }

    #[test]
    fn transport_validates_page_scope_and_download_receipts() {
        let page_request =
            Operation::List { prefix: "v1/".to_string(), cursor: None, page_size: 1 };
        assert_eq!(
            parse_reply(&page_request, serde_json::json!({"objects":[],"next_cursor":"next"}))
                .unwrap(),
            Reply::Listed(Page { objects: vec![], next_cursor: Some("next".to_string()) })
        );
        assert!(parse_reply(&page_request, serde_json::json!({"objects":[]})).is_err());
        for key in ["v1/../outside", "v10/object", "/v1/object"] {
            assert!(
                parse_reply(
                    &page_request,
                    serde_json::json!({"objects":[{"key":key,"size":1}],"next_cursor":null})
                )
                .is_err()
            );
        }
        let root = tempfile::tempdir().unwrap();
        let output_path = root.path().join("download");
        std::fs::write(&output_path, b"data").unwrap();
        let get = Operation::Get { key: "v1/object".to_string(), output_path, max_bytes: 4 };
        assert_eq!(parse_reply(&get, serde_json::json!({"size":4})).unwrap(), Reply::Downloaded(4));
        assert!(parse_reply(&get, serde_json::json!({"size":3})).is_err());
        let put = Operation::Put {
            key: "v1/object".to_string(),
            input_path: root.path().join("upload"),
            size: 4,
            sha256: "a".repeat(64),
        };
        assert_eq!(
            parse_reply(&put, serde_json::json!({"size":4,"sha256":"a".repeat(64)})).unwrap(),
            Reply::Published
        );
        assert!(parse_reply(&put, serde_json::json!({"size":4,"sha256":"b".repeat(64)})).is_err());
    }
}
