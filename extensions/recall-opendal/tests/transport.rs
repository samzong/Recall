use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

struct Provider {
    temp: tempfile::TempDir,
    config: PathBuf,
    root: PathBuf,
}

impl Provider {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("backend");
        std::fs::create_dir(&root).unwrap();
        let config = temp.path().join("provider.json");
        let this = Self { temp, config, root };
        let output = this
            .command()
            .args([
                "--recall-remote-configure",
                "--config-json",
                &json!({"scheme":"fs","root":this.root}).to_string(),
            ])
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert!(output.stdout.is_empty());
        this
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_recall-opendal"));
        command.env("RECALL_OPENDAL_CONFIG", &self.config);
        command
    }

    /// Run the transport from a specific working directory. The fs backend's
    /// atomic staging dir must stay under the backend root regardless of CWD.
    fn command_in(&self, dir: &Path) -> Command {
        let mut command = self.command();
        command.current_dir(dir);
        command
    }

    fn raw(&self, raw: &[u8]) -> (bool, Value) {
        let mut child = self
            .command()
            .arg("--recall-remote-transport")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(raw).unwrap();
        let output = child.wait_with_output().unwrap();
        let value: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "{error}: stdout={}, stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        assert_eq!(value["transport_version"], 1);
        assert!(value.get("result").is_some() ^ value.get("error").is_some());
        assert_eq!(output.status.success(), value.get("result").is_some());
        (output.status.success(), value)
    }

    fn request(&self, mut request: Value) -> Value {
        request["transport_version"] = json!(1);
        request["timeout_ms"] = json!(30_000);
        self.raw(&serde_json::to_vec(&request).unwrap()).1
    }

    fn upload(&self, key: &str, bytes: &[u8]) -> Value {
        let source = self.temp.path().join("source");
        std::fs::write(&source, bytes).unwrap();
        self.request(json!({"operation":"put","key":key,"input_path":source,
            "size":bytes.len(),"sha256":format!("{:x}",Sha256::digest(bytes))}))
    }

    fn download(&self, key: &str, out: &Path, max: u64) -> Value {
        self.request(json!({"operation":"get","key":key,"output_path":out,"max_bytes":max}))
    }
}

#[test]
fn manifest_has_standalone_identity() {
    let output = Command::new(env!("CARGO_BIN_EXE_recall-opendal"))
        .arg("--recall-extension-manifest")
        .output()
        .unwrap();
    assert!(output.status.success());
    let manifest: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(manifest["name"], "opendal");
    assert_eq!(manifest["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(manifest["protocol"], 2);
    assert_eq!(manifest["min_recall"], "0.6.0");
}

#[test]
fn fs_round_trip_receipts_and_empty_file() {
    let p = Provider::new();
    assert_eq!(p.request(json!({"operation":"probe"}))["result"], json!({"readable":true}));
    let bytes = b"an actual payload\0with binary bytes";
    assert_eq!(
        p.upload("v1/objects/payload.bin", bytes)["result"],
        json!({
        "size":bytes.len(), "sha256":format!("{:x}",Sha256::digest(bytes))})
    );
    let out = p.temp.path().join("download");
    assert_eq!(
        p.download("v1/objects/payload.bin", &out, bytes.len() as u64)["result"],
        json!({"size":bytes.len()})
    );
    assert_eq!(std::fs::read(out).unwrap(), bytes);
    assert_eq!(p.upload("v1/empty", b"")["result"]["size"], 0);
    let out = p.temp.path().join("empty");
    assert_eq!(p.download("v1/empty", &out, 0)["result"], json!({"size":0}));
    assert_eq!(std::fs::read(out).unwrap(), b"");
}

#[test]
fn pagination_is_sorted_bounded_and_complete() {
    let p = Provider::new();
    for key in ["v1/z", "v1/b", "v1/d/sub", "v1/a", "outside/data", "v1/c"] {
        assert!(p.upload(key, key.as_bytes()).get("result").is_some());
    }
    let mut cursor = Value::Null;
    let mut all = vec![];
    for expected in [vec!["v1/a", "v1/b"], vec!["v1/c", "v1/d/sub"], vec!["v1/z"]] {
        let response =
            p.request(json!({"operation":"list","prefix":"v1/","cursor":cursor,"page_size":2}));
        let page = &response["result"];
        assert!(page.get("next_cursor").is_some());
        let keys: Vec<&str> = page["objects"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| {
                let key = o["key"].as_str().unwrap();
                assert_eq!(o["size"].as_u64().unwrap(), key.len() as u64);
                key
            })
            .collect();
        assert_eq!(keys, expected);
        all.extend(keys.iter().map(|s| s.to_string()));
        cursor = page["next_cursor"].clone();
        if !cursor.is_null() {
            assert_eq!(cursor, *keys.last().unwrap());
        }
    }
    assert!(cursor.is_null());
    assert_eq!(all.len(), 5);
    let empty =
        p.request(json!({"operation":"list","prefix":"missing/","cursor":null,"page_size":1}));
    assert_eq!(empty["result"], json!({"objects":[],"next_cursor":null}));
    // Exactly one full final page must end with null, rather than a phantom page.
    let exact = p.request(json!({"operation":"list","prefix":"v1/d/","cursor":null,"page_size":1}));
    assert!(exact["result"]["next_cursor"].is_null());
}

#[test]
fn invalid_upload_never_publishes() {
    let p = Provider::new();
    let source = p.temp.path().join("source");
    std::fs::write(&source, b"data").unwrap();
    for (size, hash, code) in [
        (3, "a".repeat(64), "integrity"),
        (4, "a".repeat(64), "integrity"),
        (4, "A".repeat(64), "invalid_request"),
    ] {
        let response = p.request(
            json!({"operation":"put","key":"v1/bad","input_path":source,"size":size,"sha256":hash}),
        );
        assert_eq!(response["error"]["code"], code);
        assert!(!p.root.join("v1/bad").exists());
    }
}

#[test]
fn get_enforces_limits_noclobber_and_missing_paths() {
    let p = Provider::new();
    p.upload("v1/data", b"hello");
    let out = p.temp.path().join("download");
    assert_eq!(p.download("v1/data", &out, 4)["error"]["code"], "integrity");
    assert!(!out.exists());
    assert_eq!(p.download("v1/missing", &out, 100)["error"]["code"], "object_missing");
    assert!(!out.exists());
    std::fs::write(&out, b"preserve me").unwrap();
    assert_eq!(p.download("v1/data", &out, 100)["error"]["code"], "conflict");
    assert_eq!(std::fs::read(out).unwrap(), b"preserve me");
    assert_eq!(
        p.download("v1/data", &p.temp.path().join("absent/download"), 100)["error"]["code"],
        "target_missing"
    );
    assert_eq!(
        p.download("v1/data", Path::new("relative"), 100)["error"]["code"],
        "invalid_request"
    );
}

#[test]
fn rejects_invalid_keys_prefixes_and_cursors() {
    let p = Provider::new();
    let out = p.temp.path().join("output");
    for key in ["", "/absolute", "a//b", "a/../b", "a/./b", "Upper", "space key", "a/", "a\\b"] {
        assert_eq!(p.download(key, &out, 100)["error"]["code"], "invalid_request", "{key}");
    }
    for (prefix, cursor, size) in [
        ("v1", None, 1),
        ("v1/", Some("v2/a"), 1),
        ("v1/", Some("v1/../b"), 1),
        ("", None, 0),
        ("", None, 1001),
    ] {
        assert_eq!(
            p.request(json!({"operation":"list","prefix":prefix,"cursor":cursor,"page_size":size}))
                ["error"]["code"],
            "invalid_request"
        );
    }
}

#[test]
fn malformed_and_unsupported_requests_have_error_envelopes() {
    let p = Provider::new();
    assert_eq!(p.raw(b"{").1["error"]["code"], "invalid_request");
    assert_eq!(
        p.raw(br#"{"transport_version":2,"timeout_ms":10,"operation":"probe"}"#).1["error"]["code"],
        "unsupported_protocol"
    );
    assert_eq!(
        p.raw(br#"{"transport_version":1,"timeout_ms":0,"operation":"probe"}"#).1["error"]["code"],
        "invalid_request"
    );
    assert_eq!(p.request(json!({"operation":"unknown"}))["error"]["code"], "invalid_request");
    assert_eq!(
        p.request(json!({"operation":"get","key":"v1/a"}))["error"]["code"],
        "invalid_request"
    );
    let large = vec![b' '; 2 * 1024 * 1024 + 1];
    assert_eq!(p.raw(&large).1["error"]["code"], "invalid_request");
}

#[test]
fn configuration_reverification_and_failure_preserve_previous_config() {
    let p = Provider::new();
    let previous = std::fs::read(&p.config).unwrap();
    assert!(p.command().arg("--recall-remote-configure").output().unwrap().status.success());
    let bad = p
        .command()
        .args(["--recall-remote-configure", "--config-json", "{\"scheme\":\"nonexistent-scheme\"}"])
        .output()
        .unwrap();
    assert!(!bad.status.success());
    let before: Value = serde_json::from_slice(&previous).unwrap();
    let after: Value = serde_json::from_slice(&std::fs::read(&p.config).unwrap()).unwrap();
    assert_eq!(before, after);
    std::fs::remove_file(&p.config).unwrap();
    assert_eq!(p.request(json!({"operation":"probe"}))["error"]["code"], "not_configured");
    assert!(!p.command().arg("--recall-remote-configure").output().unwrap().status.success());
}

#[cfg(unix)]
#[test]
fn configuration_is_private_and_get_rejects_dangling_symlink() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let p = Provider::new();
    assert_eq!(std::fs::metadata(&p.config).unwrap().permissions().mode() & 0o777, 0o600);
    p.upload("v1/data", b"secret");
    let out = p.temp.path().join("link");
    let target = p.temp.path().join("missing");
    symlink(&target, &out).unwrap();
    assert_eq!(p.download("v1/data", &out, 100)["error"]["code"], "conflict");
    assert!(!target.exists());
}

#[test]
fn files_larger_than_transfer_chunk_round_trip() {
    let p = Provider::new();
    let bytes: Vec<u8> = (0..4 * 1024 * 1024 + 913).map(|i| (i % 251) as u8).collect();
    assert_eq!(p.upload("v1/large", &bytes)["result"]["size"], bytes.len());
    let output = p.temp.path().join("large-download");
    assert_eq!(p.download("v1/large", &output, bytes.len() as u64)["result"]["size"], bytes.len());
    assert_eq!(Sha256::digest(std::fs::read(output).unwrap()), Sha256::digest(&bytes));
}

#[test]
fn configure_accepts_a_config_file() {
    let p = Provider::new();
    let candidate = p.temp.path().join("candidate.json");
    std::fs::write(&candidate, json!({"scheme":"fs","root":p.root}).to_string()).unwrap();
    let output = p
        .command()
        .args(["--recall-remote-configure", "--config-file"])
        .arg(candidate)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(p.request(json!({"operation":"probe"}))["result"]["readable"], true);
}

#[test]
fn fs_atomic_staging_stays_under_backend_root_across_cwd() {
    let p = Provider::new();
    // Put from a different, read-only working directory: staging must not be
    // created there, and the object must land under the backend root.
    let ro = p.temp.path().join("readonly-cwd");
    std::fs::create_dir(&ro).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
    }
    let bytes = b"cwd-independent";
    let source = p.temp.path().join("src-cwd");
    std::fs::write(&source, bytes).unwrap();
    let req = serde_json::to_vec(&json!({
        "transport_version":1,"timeout_ms":30000,"operation":"put",
        "key":"v1/cwd-test","input_path":source,
        "size":bytes.len(),"sha256":format!("{:x}",Sha256::digest(bytes))
    }))
    .unwrap();
    let mut child = p
        .command_in(&ro)
        .arg("--recall-remote-transport")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&req).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["result"]["size"], bytes.len());
    assert_eq!(std::fs::read(p.root.join("v1/cwd-test")).unwrap(), bytes);
    // The staging dir lives under the backend root and is hidden from v1/ listings.
    let staging = p.root.join(".recall-opendal-tmp");
    assert!(staging.is_dir());
    assert!(std::fs::read_dir(&ro).unwrap().next().is_none());
    let listed = p.request(json!({"operation":"list","prefix":"v1/","page_size":100}));
    let keys: Vec<&str> = listed["result"]["objects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, ["v1/cwd-test"]);
}

#[test]
fn deadline_covers_a_stalled_http_backend() {
    use std::io::Read;
    let p = Provider::new();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/", listener.local_addr().unwrap());
    std::fs::write(&p.config, json!({"scheme":"webdav","endpoint":endpoint}).to_string()).unwrap();
    // Accept connections on a dedicated thread so the test never blocks on
    // OpenDAL's connection pool or retries. The plugin's own deadline
    // (timeout_ms) must fire while the server is still stalling.
    std::thread::spawn(move || {
        while let Ok((mut socket, _)) = listener.accept() {
            std::thread::spawn(move || {
                let mut buffer = [0; 1024];
                let _ = socket.read(&mut buffer);
                std::thread::sleep(std::time::Duration::from_millis(300));
            });
        }
    });
    let started = std::time::Instant::now();
    let response = p.raw(br#"{"transport_version":1,"timeout_ms":100,"operation":"probe"}"#).1;
    assert_eq!(response["error"]["code"], "transient");
    assert!(started.elapsed() < std::time::Duration::from_secs(3));
    // The plugin's mpsc wait has returned; the worker thread may still be
    // blocked on the socket. Detach so the test process can exit.
}
