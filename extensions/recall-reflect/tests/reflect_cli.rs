use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const JSONL_FIXTURE: &str = r#"{"schema_version":4,"record_type":"session","session":{"id":"s1","source":"codex","source_id":"source-s1","title":"Codex session","directory":"/tmp/repo","repo_remote":"git@example.com:owner/repo.git","repo_slug":"owner/repo","repo_name":"repo","started_at":1000,"updated_at":1200,"message_count":2,"entrypoint":null,"custom_title":null,"summary":null,"duration_minutes":null,"source_file_path":null},"messages":[{"seq":0,"role":"user","timestamp":1000,"content":"please keep scope small"},{"seq":1,"role":"assistant","timestamp":1100,"content":"I will keep it focused"}],"usage_events":[],"events":[]}
"#;

const BROADER_SIGNALS_JSONL_FIXTURE: &str = r#"{"schema_version":4,"record_type":"session","session":{"id":"s1","source":"codex","source_id":"source-s1","title":"Planning session","directory":"/tmp/app-a","repo_remote":null,"repo_slug":null,"repo_name":null,"started_at":1000,"updated_at":1200,"message_count":1,"entrypoint":null,"custom_title":null,"summary":null,"duration_minutes":null,"source_file_path":null},"messages":[{"seq":0,"role":"assistant","timestamp":1100,"content":"Plan the approach, outline the options, and analyze tradeoffs"}],"usage_events":[],"events":[]}
{"schema_version":4,"record_type":"session","session":{"id":"s2","source":"opencode","source_id":"source-s2","title":"Implementation session","directory":"/tmp/app-b","repo_remote":null,"repo_slug":null,"repo_name":null,"started_at":2000,"updated_at":2200,"message_count":1,"entrypoint":null,"custom_title":null,"summary":null,"duration_minutes":null,"source_file_path":null},"messages":[{"seq":0,"role":"assistant","timestamp":2100,"content":"Implemented the migration, fixed the test, and verified the build"}],"usage_events":[],"events":[]}
"#;

#[test]
fn reflect_cli_reads_export_jsonl_from_recall_bin() {
    let fake = FakeRecall::new(JSONL_FIXTURE, 0, "");

    let output = Command::new(env!("CARGO_BIN_EXE_recall-reflect"))
        .env("RECALL_BIN", fake.script_path())
        .args(["--repo", "owner/repo", "--source", "codex", "--time", "week", "--format", "json"])
        .output()
        .unwrap();

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    assert!(output.stderr.is_empty(), "stderr should be empty on success");

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["summary"]["sessions"], 1);
    assert_eq!(json["summary"]["timeline_moments"], 2);
    assert_eq!(json["scope"]["kind"], "project");
    assert_eq!(json["scope"]["project"], serde_json::Value::Null);
    assert_eq!(json["scope"]["repo"], "owner/repo");
    assert_eq!(json["scope"]["time_range"], "week");
    assert_eq!(json["scope"]["sources"][0], "codex");

    let calls = fake.calls();
    assert_eq!(
        calls,
        [
            "export --limit 0 --include metadata,messages --project owner/repo --source codex --time week"
        ]
    );
}

#[test]
fn reflect_cli_json_exposes_broader_signals() {
    let fake = FakeRecall::new(BROADER_SIGNALS_JSONL_FIXTURE, 0, "");

    let output = Command::new(env!("CARGO_BIN_EXE_recall-reflect"))
        .env("RECALL_BIN", fake.script_path())
        .args(["--personal", "--time", "30d", "--format", "json"])
        .output()
        .unwrap();

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    assert!(output.stderr.is_empty(), "stderr should be empty on success");

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["source_roles"].as_array().unwrap().len(), 2);
    assert_eq!(json["source_roles"][0]["source"], "codex");
    assert_eq!(json["source_roles"][0]["observed_role"], "Planning and analysis");
    assert_eq!(json["source_roles"][1]["source"], "opencode");
    assert_eq!(json["source_roles"][1]["observed_role"], "Implementation and verification");
    assert_eq!(json["project_summaries"].as_array().unwrap().len(), 2);
    assert_eq!(json["project_summaries"][0]["project"], "/tmp/app-a");
    assert_eq!(json["project_summaries"][1]["project"], "/tmp/app-b");
    assert!(json["task_shapes"].as_array().unwrap().len() >= 2);

    let calls = fake.calls();
    assert_eq!(calls, ["export --limit 0 --include metadata,messages --project all --time 30d"]);
}

#[test]
fn reflect_cli_syncs_selected_source_before_export() {
    let fake = FakeRecall::new(JSONL_FIXTURE, 0, "");
    let repo = TempDir::new("recall-reflect-repo");
    init_git_repo(repo.path());

    let output = Command::new(env!("CARGO_BIN_EXE_recall-reflect"))
        .env("RECALL_BIN", fake.script_path())
        .current_dir(repo.path())
        .args(["--sync", "--source", "opencode"])
        .output()
        .unwrap();

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Recall reflect"));
    assert!(stdout.contains("Sessions: 1"));

    let calls = fake.calls();
    assert_eq!(
        calls,
        [
            format!("sync --project {} --source opencode", repo.path().display()),
            format!(
                "export --limit 0 --include metadata,messages --project {} --source opencode",
                repo.path().display()
            )
        ]
    );
}

#[test]
fn reflect_cli_defaults_unscoped_reflection_to_current_git_root() {
    let fake = FakeRecall::new(JSONL_FIXTURE, 0, "");
    let repo = TempDir::new("recall-reflect-repo");
    init_git_repo(repo.path());
    let nested = repo.path().join("nested").join("child");
    fs::create_dir_all(&nested).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_recall-reflect"))
        .env("RECALL_BIN", fake.script_path())
        .current_dir(&nested)
        .arg("--format")
        .arg("json")
        .output()
        .unwrap();

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    assert!(output.stderr.is_empty(), "stderr should be empty on success");

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["scope"]["kind"], "project");
    assert_eq!(json["scope"]["project"], repo.path().display().to_string());

    let calls = fake.calls();
    assert_eq!(
        calls,
        [format!(
            "export --limit 0 --include metadata,messages --project {}",
            repo.path().display()
        )]
    );
}

#[test]
fn reflect_cli_defaults_to_personal_scope_outside_git_worktree() {
    let fake = FakeRecall::new(JSONL_FIXTURE, 0, "");
    let non_git = TempDir::new("recall-reflect-non-git");

    let output = Command::new(env!("CARGO_BIN_EXE_recall-reflect"))
        .env("RECALL_BIN", fake.script_path())
        .current_dir(non_git.path())
        .arg("--format")
        .arg("json")
        .output()
        .unwrap();

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    assert!(output.stderr.is_empty(), "stderr should be empty on success");

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["scope"]["kind"], "personal");
    assert_eq!(json["scope"]["project"], serde_json::Value::Null);
    assert_eq!(json["scope"]["repo"], serde_json::Value::Null);
    assert_eq!(json["scope"]["time_range"], "30d");

    let calls = fake.calls();
    assert_eq!(calls, ["export --limit 0 --include metadata,messages --project all --time 30d"]);
}

#[test]
fn reflect_cli_personal_overrides_git_root_scope() {
    let fake = FakeRecall::new(JSONL_FIXTURE, 0, "");
    let repo = TempDir::new("recall-reflect-repo");
    init_git_repo(repo.path());

    let output = Command::new(env!("CARGO_BIN_EXE_recall-reflect"))
        .env("RECALL_BIN", fake.script_path())
        .current_dir(repo.path())
        .args(["--personal", "--time", "7d", "--format", "json"])
        .output()
        .unwrap();

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    assert!(output.stderr.is_empty(), "stderr should be empty on success");

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["scope"]["kind"], "personal");
    assert_eq!(json["scope"]["project"], serde_json::Value::Null);
    assert_eq!(json["scope"]["time_range"], "7d");

    let calls = fake.calls();
    assert_eq!(calls, ["export --limit 0 --include metadata,messages --project all --time 7d"]);
}

#[test]
fn reflect_cli_rejects_personal_with_project_or_repo_scope() {
    let fake = FakeRecall::new(JSONL_FIXTURE, 0, "");

    let output = Command::new(env!("CARGO_BIN_EXE_recall-reflect"))
        .env("RECALL_BIN", fake.script_path())
        .args(["--personal", "--project", "/tmp/repo"])
        .output()
        .unwrap();

    assert!(!output.status.success(), "command should reject conflicting scope flags");
    assert!(output.stdout.is_empty(), "stdout must be empty on scope errors");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--personal"), "stderr: {stderr}");
    assert!(stderr.contains("--project") || stderr.contains("--repo"), "stderr: {stderr}");
    assert!(fake.calls().is_empty(), "export must not run for invalid scope flags");
}

#[test]
fn reflect_cli_reports_recall_command_failures_on_stderr() {
    let fake = FakeRecall::new("", 23, "boom");

    let output = Command::new(env!("CARGO_BIN_EXE_recall-reflect"))
        .env("RECALL_BIN", fake.script_path())
        .arg("--project")
        .arg("/tmp/repo")
        .arg("--format")
        .arg("json")
        .output()
        .unwrap();

    assert!(!output.status.success(), "command should fail");
    assert!(output.stdout.is_empty(), "stdout must be data-only and empty on command errors");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Recall command failed"), "stderr: {stderr}");
    assert!(stderr.contains("boom"), "stderr: {stderr}");

    let calls = fake.calls();
    assert_eq!(calls, ["export --limit 0 --include metadata,messages --project /tmp/repo"]);
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(prefix: &str) -> Self {
        let path = unique_temp_dir(prefix);
        fs::create_dir_all(&path).unwrap();
        // macOS resolves TMPDIR through /private, and git reports the resolved
        // path, so tests must compare against the canonical form.
        let path = fs::canonicalize(&path).unwrap_or(path);
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct FakeRecall {
    dir: PathBuf,
    script: PathBuf,
    calls: PathBuf,
}

impl FakeRecall {
    fn new(export_stdout: &str, export_exit_code: i32, export_stderr: &str) -> Self {
        let dir = unique_temp_dir("recall-reflect-test");
        fs::create_dir_all(&dir).unwrap();
        let script = dir.join("recall-fake.sh");
        let calls = dir.join("calls.txt");
        let stdout_path = dir.join("export.jsonl");
        let stderr_path = dir.join("export.stderr");
        fs::write(&stdout_path, export_stdout).unwrap();
        fs::write(&stderr_path, export_stderr).unwrap();
        fs::write(&calls, "").unwrap();

        let script_body = format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> "{calls}"
if [ "$1" = "sync" ]; then
  exit 0
fi
if [ "$1" = "export" ]; then
  cat "{stderr_path}" >&2
  cat "{stdout_path}"
  exit {export_exit_code}
fi
echo "unexpected command: $*" >&2
exit 99
"#,
            calls = calls.display(),
            stdout_path = stdout_path.display(),
            stderr_path = stderr_path.display(),
            export_exit_code = export_exit_code,
        );
        fs::write(&script, script_body).unwrap();
        make_executable(&script);

        Self { dir, script, calls }
    }

    fn script_path(&self) -> &Path {
        &self.script
    }

    fn calls(&self) -> Vec<String> {
        fs::read_to_string(&self.calls).unwrap().lines().map(str::to_string).collect()
    }
}

impl Drop for FakeRecall {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn unique_temp_dir(prefix: &str) -> PathBuf {
    // Clock resolution alone collides when tests start in the same tick, which
    // makes parallel runs share a capture file.
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("{prefix}-{}-{nanos}-{seq}", std::process::id()))
}

fn init_git_repo(path: &Path) {
    let output = Command::new("git")
        .env("GIT_MASTER", "1")
        .arg("init")
        .arg("--quiet")
        .current_dir(path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git init stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}
