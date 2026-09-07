#![cfg(unix)]

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};

fn command(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_recall-r2"));
    command
        .env_clear()
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("AWS_CONFIG_FILE", home.join("aws-config"))
        .env("AWS_SHARED_CREDENTIALS_FILE", home.join("aws-credentials"));
    command
}

fn request(home: &Path, value: Value) -> Output {
    let mut child = command(home)
        .arg("--recall-remote-transport")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&serde_json::to_vec(&value).unwrap()).unwrap();
    child.wait_with_output().unwrap()
}

fn configuration_path(home: &Path) -> std::path::PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/recall/r2.json")
    } else {
        home.join("config/recall/r2.json")
    }
}

#[test]
fn noninteractive_configuration_requires_explicit_destination_and_preserves_it_on_failure() {
    let home = tempfile::tempdir().unwrap();
    let path = configuration_path(home.path());
    let output = command(home.path())
        .arg("--recall-remote-configure")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!path.exists());
    let output = command(home.path())
        .args(["--recall-remote-configure", "--help"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!path.exists());
    let args = [
        "--recall-remote-configure",
        "--endpoint",
        "https://0123456789abcdef0123456789abcdef.r2.cloudflarestorage.com",
        "--bucket",
        "recall-test",
        "--prefix",
        "recall",
        "--credential-profile",
        "recall-test",
    ];
    let output = command(home.path()).args(args).stdin(Stdio::null()).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let saved = std::fs::read(&path).unwrap();
    let config: Value = serde_json::from_slice(&saved).unwrap();
    assert_eq!(config["prefix"], "recall/");
    assert_eq!(config["credential_profile"], "recall-test");
    let output = command(home.path())
        .args(["--recall-remote-configure", "--bucket", "another-bucket"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(std::fs::read(&path).unwrap(), saved);
    let output =
        request(home.path(), json!({"transport_version":1,"operation":"probe","timeout_ms":1000}));
    assert!(!output.status.success());
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["error"]["code"], "authentication");
    assert!(response.get("result").is_none());
}

#[test]
fn malformed_requests_fail_before_configuration_or_credentials_are_loaded() {
    let home = tempfile::tempdir().unwrap();
    for (operation, expected) in [
        (
            json!({"transport_version":8,"operation":"unknown","timeout_ms":1}),
            "unsupported_protocol",
        ),
        (
            json!({"transport_version":1,"operation":"list","timeout_ms":1,"prefix":"../","cursor":null,"page_size":1}),
            "invalid_request",
        ),
        (
            json!({"transport_version":1,"operation":"get","timeout_ms":1,"key":"a","output_path":"relative","max_bytes":1}),
            "invalid_request",
        ),
        (json!({"transport_version":1,"operation":"probe","timeout_ms":1000}), "not_configured"),
    ] {
        let output = request(home.path(), operation);
        assert!(!output.status.success());
        let response: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(response["transport_version"], 1);
        assert_eq!(response["error"]["code"], expected);
        assert!(response.get("result").is_none());
    }
    assert!(!configuration_path(home.path()).exists());
}
