#[allow(dead_code)]
#[path = "../src/error.rs"]
mod error;

use serde_json::{Value, json};
use std::io::Write;
use std::process::{Command, Output, Stdio};

const SECRET: &str = "private-fixture-value-do-not-emit";

fn command(config: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_recall-opendal"));
    command.env("RECALL_OPENDAL_CONFIG", config);
    for env in [
        "WEBDAV_USERNAME",
        "WEBDAV_PASSWORD",
        "WEBDAV_TOKEN",
        "B2_APPLICATION_KEY_ID",
        "B2_APPLICATION_KEY",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
        "AWS_ENDPOINT_URL",
        "AWS_ENDPOINT",
        "AWS_S3_ENDPOINT",
        "AWS_PROFILE",
        "AWS_DEFAULT_PROFILE",
        "AWS_SHARED_CREDENTIALS_FILE",
        "AWS_CONFIG_FILE",
    ] {
        command.env_remove(env);
    }
    command
}

fn transport(config: &std::path::Path) -> Output {
    let mut child = command(config)
        .arg("--recall-remote-transport")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"transport_version":1,"timeout_ms":3000,"operation":"probe"}"#)
        .unwrap();
    child.wait_with_output().unwrap()
}

fn read_http_request(stream: &mut std::net::TcpStream) -> String {
    use std::io::Read;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
    let mut request = Vec::new();
    let mut byte = [0; 1];
    while !request.ends_with(b"\r\n\r\n") {
        assert!(request.len() < 16384, "fixture request headers exceeded limit");
        stream.read_exact(&mut byte).unwrap();
        request.push(byte[0]);
    }
    let headers = String::from_utf8(request.clone()).unwrap();
    let length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    assert!(length < 65536, "fixture request body exceeded limit");
    let mut body = vec![0; length];
    stream.read_exact(&mut body).unwrap();
    request.extend(body);
    String::from_utf8(request).unwrap()
}

fn assert_secret_absent(output: &Output) {
    assert!(!String::from_utf8_lossy(&output.stdout).contains(SECRET));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(SECRET));
}

#[test]
fn every_configuration_entrypoint_rejects_credentials_without_echoing_them() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("saved.json");
    let candidate = temp.path().join("candidate.json");
    // fs used to silently ignore every unknown credential option and save it.
    for field in [
        "access_key_id",
        "aws_access_key_id",
        "secret_access_key",
        "aws_secret_access_key",
        "session_token",
        "aws_token",
        "token",
        "username",
        "password",
        "application_key_id",
        "application_key",
        "server_side_encryption_customer_key",
        "aws_sse_customer_key_base64",
        "credential_process",
        "unknown_credential_alias",
    ] {
        let mut value = json!({"scheme":"fs","root":temp.path()});
        value[field] = json!(SECRET);
        let raw = value.to_string();
        std::fs::write(&candidate, &raw).unwrap();
        for flag in ["--config-json", "--config-file"] {
            let arg =
                if flag == "--config-json" { raw.as_str() } else { candidate.to_str().unwrap() };
            let output =
                command(&config).args(["--recall-remote-configure", flag, arg]).output().unwrap();
            assert!(!output.status.success(), "credential accepted: {field} through {flag}");
            assert_secret_absent(&output);
            assert!(!config.exists());
        }
        std::fs::write(&config, &raw).unwrap();
        let output = command(&config).arg("--recall-remote-configure").output().unwrap();
        assert!(!output.status.success(), "saved credential accepted: {field}");
        assert_secret_absent(&output);
        let output = transport(&config);
        let response: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(response["error"]["code"], "not_configured", "{field}");
        assert_secret_absent(&output);
        std::fs::remove_file(&config).unwrap();
    }
}

#[test]
fn endpoint_userinfo_and_encoded_authorities_are_rejected_without_leakage() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("saved.json");
    for endpoint in [
        format!("https://{SECRET}:password@example.invalid/"),
        format!("https://{SECRET}%3Apassword@example.invalid/"),
        format!("https://{SECRET}%40example.invalid/"),
        format!("https://{SECRET}%2540example.invalid/"),
        format!("https://%75ser:{SECRET}@example.invalid/"),
        format!("https://@example.invalid/{SECRET}"),
    ] {
        std::fs::write(&config, json!({"scheme":"webdav","endpoint":endpoint}).to_string())
            .unwrap();
        let output = transport(&config);
        let response: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(response["error"]["code"], "not_configured");
        assert_secret_absent(&output);
    }
}

#[test]
fn malformed_config_never_echoes_serde_values() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("saved.json");
    std::fs::write(&config, format!(r#"{{"scheme":"fs","root":{{"{SECRET}":true}}}}"#)).unwrap();
    let output = transport(&config);
    assert!(!output.status.success());
    assert_secret_absent(&output);
    // Numeric values appear verbatim in serde invalid-type diagnostics.
    std::fs::write(&config, br#"{"scheme":8675309000123456789}"#).unwrap();
    let output = transport(&config);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("8675309000123456789"));
    std::fs::write(&config, format!(r#"{{"scheme":{{"{SECRET}":true}}}}"#)).unwrap();
    assert_secret_absent(&transport(&config));
}

#[test]
fn fs_missing_root_is_not_recreated_and_relative_root_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("saved.json");
    let missing = temp.path().join("missing-backend");
    std::fs::write(&config, json!({"scheme":"fs","root":missing}).to_string()).unwrap();
    let output = transport(&config);
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["error"]["code"], "target_missing");
    assert!(!missing.exists(), "OpenDAL recreated the configured target");
    std::fs::write(&config, json!({"scheme":"fs","root":"."}).to_string()).unwrap();
    let output = transport(&config);
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["error"]["code"], "not_configured");
}

#[test]
fn diagnostics_drop_backend_context_sources_and_local_io_payloads() {
    let backend = opendal::Error::new(opendal::ErrorKind::PermissionDenied, SECRET)
        .with_context("endpoint", format!("https://user:{SECRET}@example.invalid"))
        .set_source(std::io::Error::other(SECRET));
    let failure = error::Failure::backend(backend);
    assert_eq!(failure.code, "permission");
    assert!(!failure.to_string().contains(SECRET));
    let failure =
        error::Failure::io(std::io::Error::new(std::io::ErrorKind::PermissionDenied, SECRET));
    assert_eq!(failure.code, "permission");
    assert!(!failure.to_string().contains(SECRET));
}

#[test]
fn authentication_requires_specific_evidence_and_opaque_denial_stays_permission() {
    for code in ["InvalidAccessKeyId", "SignatureDoesNotMatch", "ExpiredToken", "InvalidToken"] {
        let backend = opendal::Error::new(opendal::ErrorKind::PermissionDenied,
            format!("S3Error {{ code: {code:?}, message: {SECRET:?}, resource: \"\", request_id: \"\" }}"))
            .with_context("response", "Parts { status: 403, version: HTTP/1.1, headers: {} }")
            .with_context("service", "s3");
        let failure = error::Failure::backend(backend);
        assert_eq!(failure.code, "authentication", "{code}");
        assert!(!failure.to_string().contains(SECRET));
    }
    let backend = opendal::Error::new(opendal::ErrorKind::Unexpected, SECRET)
        .with_context("response", "Parts { status: 401, version: HTTP/1.1, headers: {} }");
    assert_eq!(error::Failure::backend(backend).code, "authentication");
    let backend =
        opendal::Error::new(opendal::ErrorKind::PermissionDenied, "expired credentials? unknown")
            .with_context("response", "Parts { status: 403, version: HTTP/1.1, headers: {} }");
    assert_eq!(error::Failure::backend(backend).code, "permission");
    let backend = opendal::Error::new(
        opendal::ErrorKind::PermissionDenied,
        "opaque body mentioning InvalidAccessKeyId and response: Parts { status: 401, unrelated }",
    );
    assert_eq!(error::Failure::backend(backend).code, "permission");
}

#[test]
fn real_http_authentication_denial_and_server_payload_redaction() {
    use std::net::TcpListener;
    for (scheme, status, body, expected) in [
        ("webdav", 401, SECRET.to_string(), "authentication"),
        (
            "webdav",
            403,
            "S3Error { code: \"InvalidAccessKeyId\", message: \"opaque body\" }".to_string(),
            "permission",
        ),
        ("webdav", 403, "loading credential to sign http request".to_string(), "permission"),
        (
            "webdav",
            403,
            format!("{SECRET} InvalidAccessKeyId response: Parts {{ status: 401,"),
            "permission",
        ),
        (
            "s3",
            403,
            format!("<Error><Code>InvalidAccessKeyId</Code><Message>{SECRET}</Message></Error>"),
            "authentication",
        ),
        (
            "s3",
            403,
            format!("<Error><Code>AccessDenied</Code><Message>{SECRET}</Message></Error>"),
            "permission",
        ),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("saved.json");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_http_request(&mut stream);
            write!(stream, "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\nX-Fixture: {SECRET}\r\n\r\n{body}", body.len()).unwrap();
        });
        std::fs::write(
            &config,
            json!({"scheme":scheme,"endpoint":endpoint,"root":"/"}).to_string(),
        )
        .unwrap();
        if scheme == "s3" {
            std::fs::write(&config, json!({"scheme":scheme,"endpoint":endpoint,"bucket":"fixture","region":"us-east-1"}).to_string()).unwrap();
        }
        let mut child = command(&config)
            .env("AWS_ACCESS_KEY_ID", "fixture-key")
            .env("AWS_SECRET_ACCESS_KEY", "fixture-secret")
            .env("AWS_EC2_METADATA_DISABLED", "true")
            .arg("--recall-remote-transport")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(br#"{"transport_version":1,"timeout_ms":3000,"operation":"probe"}"#)
            .unwrap();
        let output = child.wait_with_output().unwrap();
        server.join().unwrap();
        let response: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(response["error"]["code"], expected, "{scheme} {status}");
        assert_secret_absent(&output);
    }
}

#[test]
fn webdav_credentials_are_loaded_only_from_environment_and_never_saved() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("saved.json");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let request = read_http_request(&mut stream);
        let body = r#"<?xml version="1.0"?><D:multistatus xmlns:D="DAV:"></D:multistatus>"#;
        write!(stream, "HTTP/1.1 207 Multi-Status\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        request
    });
    let output = command(&config)
        .env("WEBDAV_TOKEN", SECRET)
        .args([
            "--recall-remote-configure",
            "--config-json",
            &json!({"scheme":"webdav","endpoint":endpoint,"root":"/","disable_create_dir":"true"})
                .to_string(),
        ])
        .output()
        .unwrap();
    let request = server.join().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(request.contains(&format!("Bearer {SECRET}")));
    let saved = std::fs::read_to_string(config).unwrap();
    assert!(!saved.contains(SECRET));
    assert!(!saved.contains("token"));
    assert_secret_absent(&output);
}

#[test]
fn s3_credential_profile_selects_sdk_credentials_without_persisting_them() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("saved.json");
    let credentials = temp.path().join("sdk-credentials");
    let sdk_config = temp.path().join("sdk-config");
    std::fs::write(
        &credentials,
        format!(
            "[fixture]\naws_access_key_id = fixture-profile-key\naws_secret_access_key = {SECRET}\n"
        ),
    )
    .unwrap();
    std::fs::write(&sdk_config, "").unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let request = read_http_request(&mut stream);
        let body = r#"<?xml version="1.0"?><ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><IsTruncated>false</IsTruncated></ListBucketResult>"#;
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        request
    });
    let output = command(&config)
        .env("HOME", temp.path())
        .env("AWS_SHARED_CREDENTIALS_FILE", credentials)
        .env("AWS_CONFIG_FILE", sdk_config)
        .env("AWS_PROFILE", "unrelated-profile")
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .args([
            "--recall-remote-configure",
            "--config-json",
            &json!({"scheme":"s3","bucket":"fixture","endpoint":endpoint,"region":"us-east-1",
                "credential_profile":"fixture","disable_ec2_metadata":"true"})
            .to_string(),
        ])
        .output()
        .unwrap();
    let request = server.join().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(request.contains("Credential=fixture-profile-key/"));
    let saved = std::fs::read_to_string(config).unwrap();
    assert!(!saved.contains(SECRET));
    assert!(!saved.contains("fixture-profile-key"));
    assert!(saved.contains("credential_profile"));
    assert_secret_absent(&output);
}

#[test]
fn typed_sdk_credential_failure_is_authentication_but_unrelated_signing_failure_is_not() {
    let backend = opendal::Error::new(opendal::ErrorKind::Unexpected, "signing http request")
        .set_source(reqsign_core::Error::credential_invalid(SECRET));
    let failure = error::Failure::backend(backend);
    assert_eq!(failure.code, "authentication");
    assert!(!failure.to_string().contains(SECRET));
    let backend = opendal::Error::new(opendal::ErrorKind::Unexpected, "signing http request")
        .set_source(reqsign_core::Error::request_invalid(SECRET));
    assert_eq!(error::Failure::backend(backend).code, "unavailable");
}

#[test]
fn sdk_endpoint_environment_cannot_bypass_userinfo_rejection() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("saved.json");
    std::fs::write(&config, json!({"scheme":"s3","bucket":"fixture","region":"us-east-1","disable_ec2_metadata":"true"}).to_string()).unwrap();
    for env in ["AWS_ENDPOINT_URL", "AWS_ENDPOINT", "AWS_S3_ENDPOINT"] {
        let mut child = command(&config)
            .env(env, format!("https://user:{SECRET}@example.invalid"))
            .env("HOME", temp.path())
            .env("AWS_EC2_METADATA_DISABLED", "true")
            .arg("--recall-remote-transport")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(br#"{"transport_version":1,"timeout_ms":3000,"operation":"probe"}"#)
            .unwrap();
        let output = child.wait_with_output().unwrap();
        let response: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(response["error"]["code"], "not_configured", "{env}");
        assert_secret_absent(&output);
    }
}

#[test]
fn structured_s3_missing_bucket_is_target_missing_not_unconfigured() {
    let backend = opendal::Error::new(opendal::ErrorKind::ConfigInvalid,
        format!("S3Error {{ code: \"NoSuchBucket\", message: {SECRET:?}, resource: \"\", request_id: \"\" }}"))
        .with_context("response", "Parts { status: 404, version: HTTP/1.1, headers: {} }")
        .with_context("service", "s3");
    let failure = error::Failure::backend(backend);
    assert_eq!(failure.code, "target_missing");
    assert!(!failure.to_string().contains(SECRET));
}

#[test]
fn opaque_http_404_does_not_establish_missing_object_or_target() {
    for service in ["s3", "webdav", "b2"] {
        let backend = opendal::Error::new(opendal::ErrorKind::NotFound, SECRET)
            .with_context("response", "Parts { status: 404, version: HTTP/1.1, headers: {} }")
            .with_context("service", service);
        let failure = error::Failure::backend(backend);
        assert_eq!(failure.code, "unavailable", "{service}");
        assert!(!failure.to_string().contains(SECRET));
    }
}
