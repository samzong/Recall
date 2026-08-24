use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;

use tempfile::TempDir;

fn adapter_jsonl() -> String {
    ["codex", "future-harness"]
        .iter()
        .enumerate()
        .map(|(idx, adapter)| {
            format!(
                r#"{{"schema_version":5,"record_type":"session","session":{{"id":"s{idx}","source":"{adapter}","source_id":"raw-{idx}","title":"{adapter}","started_at":1000,"updated_at":1100,"message_count":1,"topology":{{"thread_role":"primary","parents":[]}}}},"messages":[{{"seq":0,"role":"user","content":"prompt from {adapter}"}}]}}"#
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn manifest_is_protocol_two() {
    let output = Command::new(env!("CARGO_BIN_EXE_recall-powercontext"))
        .arg("--recall-extension-manifest")
        .output()
        .unwrap();
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["name"], "powercontext");
    assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(json["protocol"], 2);
    assert_eq!(json["min_recall"], "0.4.0");
}

#[test]
fn dry_run_reports_bound_scope_and_unknown_adapters() {
    let fake = FakeRecall::new(&adapter_jsonl(), 0, "export warning");
    let repo = temp_dir("recall-pc-repo");
    init_git_repo(repo.path(), "https://USER:token@GitHub.com/samzong/Recall.git");
    bind_workstream_scope(repo.path(), "workstream:bound");

    let output = Command::new(env!("CARGO_BIN_EXE_recall-powercontext"))
        .env("RECALL_BIN", fake.script_path())
        .current_dir(repo.path())
        .args(["backfill", "--dry-run"])
        .output()
        .unwrap();

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    assert!(output.stdout.is_empty());
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(text.contains("Dry run: 2 sources selected"), "{text}");
    assert!(text.contains("Scope: workstream:bound"), "{text}");
    assert!(text.contains("codex"), "{text}");
    assert!(text.contains("future-harness"), "{text}");
    assert!(text.contains("Total"), "{text}");
    assert!(!text.contains("Skipped"), "{text}");
    assert!(!text.contains("Conflicts"), "{text}");
    assert!(!text.contains("Failed"), "{text}");
    assert!(text.contains("export warning"), "{text}");
    let calls = fake.calls();
    assert_eq!(
        calls,
        ["export --project github.com/samzong/Recall --limit 0 --include metadata,messages"]
    );
}

#[test]
fn dry_run_forwards_time_window_to_export() {
    let fake = FakeRecall::new(&adapter_jsonl(), 0, "");
    let repo = temp_dir("recall-pc-time");
    init_git_repo(repo.path(), "git@github.com:samzong/Recall.git");

    let output = Command::new(env!("CARGO_BIN_EXE_recall-powercontext"))
        .env("RECALL_BIN", fake.script_path())
        .current_dir(repo.path())
        .args(["backfill", "--dry-run", "--time", "30d", "--format", "json"])
        .output()
        .unwrap();

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["time"], "30d");
    assert_eq!(
        fake.calls(),
        [
            "export --project github.com/samzong/Recall --limit 0 --include metadata,messages --time 30d"
        ]
    );
}

#[test]
fn recall_export_failures_are_reported() {
    let repo = temp_dir("recall-pc-export-failure");
    init_git_repo(repo.path(), "https://github.com/example/repo.git");
    let server = MockServer::start();
    let jsonl = r#"{"schema_version":5,"record_type":"session","session":{"id":"new","source":"cursor","started_at":1},"messages":[{"seq":0,"role":"user","content":"fresh"}]}"#;

    for (stdout, exit_code, stderr, expected) in
        [(jsonl, 23, "boom", "boom"), ("not-json", 0, "", "line 1")]
    {
        let fake = FakeRecall::new(stdout, exit_code, stderr);
        let output = Command::new(env!("CARGO_BIN_EXE_recall-powercontext"))
            .env("RECALL_BIN", fake.script_path())
            .current_dir(repo.path())
            .args(["backfill", "--server-url", &server.url()])
            .output()
            .unwrap();

        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(expected), "{stderr}");
        assert!(server.posted_ids().is_empty());
    }
}

#[test]
fn rejects_unknown_time_and_time_with_stdin() {
    let repo = temp_dir("recall-pc-time-bad");
    init_git_repo(repo.path(), "git@github.com:samzong/Recall.git");

    let unknown = Command::new(env!("CARGO_BIN_EXE_recall-powercontext"))
        .current_dir(repo.path())
        .args(["backfill", "--dry-run", "--time", "yesterday"])
        .output()
        .unwrap();
    assert!(!unknown.status.success());
    let stderr = String::from_utf8_lossy(&unknown.stderr);
    assert!(stderr.contains("yesterday"), "{stderr}");

    let stdin = Command::new(env!("CARGO_BIN_EXE_recall-powercontext"))
        .current_dir(repo.path())
        .args(["backfill", "--stdin", "--time", "30d"])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(!stdin.status.success());
    let stderr = String::from_utf8_lossy(&stdin.stderr);
    assert!(stderr.contains("--stdin"), "{stderr}");
}

#[test]
fn stdin_backfill_posts_and_classifies_skip_conflict_and_import() {
    let repo = temp_dir("recall-pc-http");
    init_git_repo(repo.path(), "https://github.com/samzong/Recall.git");
    let server = MockServer::start();
    let jsonl = r#"{"schema_version":5,"record_type":"session","session":{"id":"new","source":"cursor","started_at":1,"topology":{"thread_role":"primary","parents":[]}},"messages":[{"seq":0,"role":"user","content":"fresh"}]}
{"schema_version":5,"record_type":"session","session":{"id":"old","source":"codex","started_at":1,"topology":{"thread_role":"primary","parents":[]}},"messages":[{"seq":0,"role":"user","content":"same"}]}
{"schema_version":5,"record_type":"session","session":{"id":"changed","source":"opencode","started_at":1,"topology":{"thread_role":"primary","parents":[]}},"messages":[{"seq":0,"role":"user","content":"changed"}]}
{"schema_version":5,"record_type":"session","session":{"id":"child","source":"gemini-cli","started_at":1,"topology":{"thread_role":"subagent","parents":[]}},"messages":[{"seq":0,"role":"user","content":"nested"}]}"#;

    let output = run_stdin_backfill(repo.path(), &server.url(), jsonl);

    assert!(
        output.status.success(),
        "stderr: {}\nstdout: {}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["totals"]["imported"], 1, "{json}");
    assert_eq!(json["totals"]["skipped"], 1);
    assert_eq!(json["totals"]["conflicts"], 1);
    assert_eq!(json["totals"]["failed"], 0);
    assert!(server.posted_ids().contains(&"recall:cursor:new:0".to_string()));
    assert!(!server.posted_ids().iter().any(|id| id.contains("gemini-cli")));
}

#[test]
fn missing_origin_exits_two_without_export() {
    let fake = FakeRecall::new("", 0, "");
    let repo = temp_dir("recall-pc-noorigin");
    init_git_repo(repo.path(), "");

    let output = Command::new(env!("CARGO_BIN_EXE_recall-powercontext"))
        .env("RECALL_BIN", fake.script_path())
        .current_dir(repo.path())
        .args(["backfill", "--dry-run"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("origin"), "{stderr}");
    assert!(fake.calls().is_empty());
}

#[test]
fn unreachable_server_stops_the_run() {
    let repo = temp_dir("recall-pc-down");
    init_git_repo(repo.path(), "git@github.com:samzong/Recall.git");
    let jsonl = r#"{"schema_version":5,"record_type":"session","session":{"id":"s1","source":"cursor","started_at":1},"messages":[{"seq":0,"role":"user","content":"hi"}]}"#;

    let output = run_stdin_backfill(repo.path(), "http://127.0.0.1:1", jsonl);

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unreachable") || stderr.contains("Connection"), "{stderr}");
}

#[test]
fn rejects_non_loopback_http_and_redirects() {
    for server_url in ["http://192.168.1.9:8000", "http://localhost:8000@evil.example"] {
        let output = Command::new(env!("CARGO_BIN_EXE_recall-powercontext"))
            .args(["backfill", "--dry-run", "--stdin", "--server-url", server_url])
            .output()
            .unwrap();
        assert!(!output.status.success(), "accepted {server_url}");
    }

    let repo = temp_dir("recall-pc-redirect");
    init_git_repo(repo.path(), "https://github.com/example/repo.git");
    let target = MockServer::start();
    let redirect = MockServer::start_redirect(&format!("{}/v1/stats", target.url()));
    let output = run_stdin_backfill(repo.path(), &redirect.url(), "");
    assert!(
        !output.status.success(),
        "followed redirect: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn malformed_and_failed_captures_return_nonzero() {
    let repo = temp_dir("recall-pc-server-errors");
    init_git_repo(repo.path(), "git@github.com:samzong/Recall.git");
    let server = MockServer::start();

    for session_id in ["badresponse", "servererror"] {
        let jsonl = format!(
            r#"{{"schema_version":5,"record_type":"session","session":{{"id":"{session_id}","source":"codex","started_at":1}},"messages":[{{"seq":0,"role":"user","content":"hello"}}]}}"#
        );
        let output = run_stdin_backfill(repo.path(), &server.url(), &jsonl);
        assert!(
            !output.status.success(),
            "{session_id}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}

#[test]
fn invalid_stats_response_stops_before_capture() {
    let repo = temp_dir("recall-pc-bad-stats");
    init_git_repo(repo.path(), "git@github.com:samzong/Recall.git");
    let jsonl = r#"{"schema_version":5,"record_type":"session","session":{"id":"new","source":"cursor","started_at":1},"messages":[{"seq":0,"role":"user","content":"hello"}]}"#;

    for (status, body) in [(200, "{}"), (500, r#"{"error":{"code":"boom"}}"#)] {
        let server = MockServer::start_with_stats(status, body);
        let output = run_stdin_backfill(repo.path(), &server.url(), jsonl);
        assert!(
            !output.status.success(),
            "HTTP {status}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(server.posted_ids().is_empty());
    }
}

fn run_stdin_backfill(repo: &Path, server_url: &str, jsonl: &str) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_recall-powercontext"))
        .current_dir(repo)
        .args(["backfill", "--stdin", "--server-url", server_url, "--format", "json"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(jsonl.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

struct FakeRecall {
    _dir: TempDir,
    script: PathBuf,
    calls: PathBuf,
}

impl FakeRecall {
    fn new(export_stdout: &str, export_exit_code: i32, export_stderr: &str) -> Self {
        let dir = temp_dir("recall-pc-fake");
        let script = dir.path().join("recall-fake.sh");
        let calls = dir.path().join("calls.txt");
        let stdout_path = dir.path().join("export.jsonl");
        let stderr_path = dir.path().join("export.stderr");
        fs::write(&stdout_path, export_stdout).unwrap();
        fs::write(&stderr_path, export_stderr).unwrap();
        fs::write(&calls, "").unwrap();
        let script_body = format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> "{calls}"
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
        Self { _dir: dir, script, calls }
    }

    fn script_path(&self) -> &Path {
        &self.script
    }

    fn calls(&self) -> Vec<String> {
        fs::read_to_string(&self.calls).unwrap().lines().map(str::to_string).collect()
    }
}

struct MockServer {
    url: String,
    posted: PathBuf,
    _dir: TempDir,
}

impl MockServer {
    fn start() -> Self {
        Self::start_with_stats(
            200,
            r#"{"scope_id":"git:github.com/samzong/Recall","inventory":{"sources":{"total":5}}}"#,
        )
    }

    fn start_with_stats(stats_status: u16, stats_body: &str) -> Self {
        Self::start_with_stats_response(stats_status, stats_body, None)
    }

    fn start_redirect(location: &str) -> Self {
        Self::start_with_stats_response(302, "", Some(location))
    }

    fn start_with_stats_response(
        stats_status: u16,
        stats_body: &str,
        stats_location: Option<&str>,
    ) -> Self {
        let dir = temp_dir("recall-pc-http");
        let posted = dir.path().join("posted.txt");
        fs::write(&posted, "").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let posted_for_thread = posted.clone();
        let stats_body = stats_body.to_string();
        let stats_location = stats_location.map(str::to_string);
        thread::spawn(move || {
            for incoming in listener.incoming() {
                let Ok(mut stream) = incoming else {
                    continue;
                };
                let Ok(request) = read_http_request(&mut stream) else {
                    continue;
                };
                let is_stats = request.starts_with("GET /v1/stats");
                let (status, body) = if is_stats {
                    (stats_status, stats_body.clone())
                } else if request.starts_with("POST /v1/sources/content") {
                    let source_id = request
                        .split("\"source_id\":\"")
                        .nth(1)
                        .and_then(|rest| rest.split('"').next())
                        .unwrap_or("")
                        .to_string();
                    let mut file =
                        fs::OpenOptions::new().append(true).open(&posted_for_thread).unwrap();
                    writeln!(file, "{source_id}").unwrap();
                    match source_id.as_str() {
                        "recall:cursor:new:0" => (
                            202,
                            r#"{"status":"accepted","source":{"name":"content","source_id":"recall:cursor:new:0"},"position":6}"#.to_string(),
                        ),
                        "recall:codex:old:0" => (
                            202,
                            r#"{"status":"accepted","source":{"name":"content","source_id":"recall:codex:old:0"},"position":3}"#.to_string(),
                        ),
                        "recall:opencode:changed:0" => (
                            409,
                            r#"{"error":{"code":"source_conflict","message":"The Source identity has different content.","details":null}}"#.to_string(),
                        ),
                        "recall:codex:badresponse:0" => (202, "{}".to_string()),
                        "recall:codex:servererror:0" => (
                            500,
                            r#"{"error":{"code":"boom","message":"server error"}}"#.to_string(),
                        ),
                        other => (
                            500,
                            format!(r#"{{"error":{{"code":"unexpected","message":"{other}"}}}}"#),
                        ),
                    }
                } else {
                    (404, r#"{"error":{"code":"not_found"}}"#.to_string())
                };
                let headers = if is_stats {
                    stats_location
                        .as_ref()
                        .map(|location| format!("Location: {location}\r\n"))
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Self { url: format!("http://127.0.0.1:{}", addr.port()), posted, _dir: dir }
    }

    fn url(&self) -> String {
        self.url.clone()
    }

    fn posted_ids(&self) -> Vec<String> {
        fs::read_to_string(&self.posted).unwrap().lines().map(str::to_string).collect()
    }
}

fn bind_workstream_scope(path: &Path, scope_id: &str) {
    let state_dir = path.join(".git/powercontext");
    fs::create_dir_all(&state_dir).unwrap();
    fs::write(
        state_dir.join("codex-workspace.json"),
        format!(r#"{{"schema":"powercontext.codex-workspace.v1","scope_id":"{scope_id}"}}"#),
    )
    .unwrap();
}

fn read_http_request(stream: &mut std::net::TcpStream) -> std::io::Result<String> {
    stream.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
    let mut buf = Vec::new();
    let mut chunk = [0_u8; 2048];
    loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(header_end) = find_double_crlf(&buf) {
            let content_len = content_length(&buf[..header_end]).unwrap_or(0);
            if buf.len() >= header_end + content_len {
                break;
            }
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|window| window == b"\r\n\r\n").map(|idx| idx + 4)
}

fn content_length(headers: &[u8]) -> Option<usize> {
    let headers = std::str::from_utf8(headers).ok()?;
    headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("content-length") { value.trim().parse().ok() } else { None }
    })
}

fn temp_dir(prefix: &str) -> TempDir {
    tempfile::Builder::new().prefix(prefix).tempdir().unwrap()
}

fn init_git_repo(path: &Path, origin: &str) {
    let output = Command::new("git")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["init", "--quiet"])
        .current_dir(path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git init stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    if origin.is_empty() {
        return;
    }
    let output = Command::new("git")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["remote", "add", "origin", origin])
        .current_dir(path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git remote stderr: {}",
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
