use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

fn binary() -> PathBuf {
    let mut path = std::env::current_exe().expect("test binary path");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.join("recall-publish")
}

fn fake_recall(dir: &Path) -> PathBuf {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sessions.jsonl");
    let script = dir.join("recall");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"info\" ]; then\n  echo '{{\"protocol_version\":2}}'\n  exit 0\nfi\ncat {}\n",
            fixture.display()
        ),
    )
    .expect("write fake recall");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    script
}

struct Harness {
    home: tempfile::TempDir,
    recall: PathBuf,
    config: tempfile::TempDir,
}

impl Harness {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        let recall = fake_recall(home.path());
        Self { home, recall, config }
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        Command::new(binary())
            .args(args)
            .env("RECALL_PUBLISH_HOME", self.home.path())
            .env("RECALL_BIN", &self.recall)
            .env("XDG_CONFIG_HOME", self.config.path())
            .env("HOME", self.config.path())
            .output()
            .expect("run recall-publish")
    }
}

fn json(output: &std::process::Output) -> Value {
    serde_json::from_slice(&output.stdout).expect("command emitted JSON")
}

fn scanner_available() -> bool {
    std::env::var_os("RECALL_PUBLISH_E2E").is_some()
}

#[test]
fn the_manifest_declares_the_command_name_and_protocol() {
    let harness = Harness::new();
    let output = harness.run(&["--recall-extension-manifest"]);
    assert!(output.status.success());
    let manifest = json(&output);
    assert_eq!(manifest["name"], "publish");
    assert_eq!(manifest["protocol"], 2);
    assert!(manifest["min_recall"].is_string());
}

#[test]
fn approving_a_scope_that_was_never_prepared_is_refused() {
    let harness = Harness::new();
    let output = harness.run(&["approve", "missing-scope"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("has not been prepared"), "{stderr}");
}

#[test]
fn uploading_without_an_approval_is_refused_before_any_network_call() {
    let harness = Harness::new();
    let output =
        harness.run(&["upload", "missing-scope", "--repo", "someone/dataset", "--dry-run"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("has not been approved"), "{stderr}");
}

#[test]
fn a_scope_name_cannot_escape_the_workspace_directory() {
    let harness = Harness::new();
    let output = harness.run(&["approve", "../../etc"]);
    assert!(!output.status.success());
}

#[test]
fn prepare_redacts_secrets_and_identities_then_binds_an_approval() {
    if !scanner_available() {
        eprintln!(
            "SKIPPED: set RECALL_PUBLISH_E2E=1 with gitleaks and the redaction environment installed"
        );
        return;
    }
    let harness = Harness::new();
    let install = harness.run(&["doctor", "--install"]);
    assert!(install.status.success(), "{}", String::from_utf8_lossy(&install.stderr));

    let output = harness.run(&[
        "prepare",
        "--project",
        "all",
        "--since",
        "2026-08-01",
        "--until",
        "2026-09-01",
        "--license",
        "CC-BY-4.0",
        "--author",
        "samzong",
        "--scope",
        "test-scope",
    ]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let result = json(&output);
    assert_eq!(result["preview"]["sessions"], 2);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Exporting sessions"), "{stderr}");
    assert!(stderr.contains("Scanning"), "{stderr}");
    assert!(stderr.contains("Prepared"), "{stderr}");

    let quiet = harness.run(&[
        "--quiet",
        "prepare",
        "--project",
        "all",
        "--since",
        "2026-08-01",
        "--until",
        "2026-09-01",
        "--license",
        "CC-BY-4.0",
        "--author",
        "samzong",
        "--scope",
        "quiet-scope",
    ]);
    assert!(quiet.status.success(), "{}", String::from_utf8_lossy(&quiet.stderr));
    assert!(quiet.stderr.is_empty(), "{}", String::from_utf8_lossy(&quiet.stderr));

    let data = std::fs::read_to_string(
        harness.home.path().join("publish/scopes/test-scope/test-scope.recall.jsonl"),
    )
    .unwrap();
    assert!(!data.contains("ghp_"), "a secret survived redaction");
    assert!(!data.contains("13800138000"));
    assert!(!data.contains("110101199003072316"));
    assert!(!data.contains("/Users/x/.codex"));
    assert!(data.contains("[REDACTED:"));
    assert!(data.contains("Jordan Miller"), "named entities are allowed by default");
    assert!(!data.contains("jordan.miller@example.com"));

    let old_session = harness.run(&[
        "prepare",
        "--project",
        "all",
        "--session",
        "s-old",
        "--since",
        "2026-08-01",
        "--until",
        "2026-08-02",
        "--license",
        "CC-BY-4.0",
        "--author",
        "samzong",
        "--scope",
        "sha-scope",
    ]);
    assert!(old_session.status.success(), "{}", String::from_utf8_lossy(&old_session.stderr));
    let shas = std::fs::read_to_string(
        harness.home.path().join("publish/scopes/sha-scope/sha-scope.recall.jsonl"),
    )
    .unwrap();
    assert!(
        shas.contains("cff6c37c004e7ffd5cf6291722c7af11a84af955"),
        "a commit hash was mangled by entity detection"
    );
    assert!(shas.contains("93ea2c4d9285981185b2a0b6d39289676a1804f2"));
    assert!(data.contains("samzong/Recall"));
    assert_eq!(data.lines().count(), 2);

    let manifest: Value = serde_json::from_str(
        &std::fs::read_to_string(
            harness.home.path().join("publish/scopes/test-scope/manifest.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["license"], "CC-BY-4.0");
    assert_eq!(manifest["publisher"]["author"], "samzong");
    assert_eq!(manifest["selection"]["time"]["since"], "2026-08-01T00:00:00+00:00");

    let approve = harness.run(&["approve", "test-scope"]);
    assert!(approve.status.success(), "{}", String::from_utf8_lossy(&approve.stderr));

    let dry = harness.run(&["upload", "test-scope", "--repo", "samzong/x", "--dry-run"]);
    assert!(dry.status.success(), "{}", String::from_utf8_lossy(&dry.stderr));

    std::fs::write(harness.home.path().join("publish/scopes/test-scope/manifest.json"), b"{}\n")
        .unwrap();
    let tampered = harness.run(&["upload", "test-scope", "--repo", "samzong/x", "--dry-run"]);
    assert!(!tampered.status.success());
    assert!(String::from_utf8_lossy(&tampered.stderr).contains("changed after approval"));
}
