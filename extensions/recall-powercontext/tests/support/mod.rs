use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use super::temp_dir;

pub(super) struct FakeRecall {
    _dir: TempDir,
    script: PathBuf,
    calls: PathBuf,
}

impl FakeRecall {
    pub(super) fn new(export_stdout: &str, export_exit_code: i32, export_stderr: &str) -> Self {
        let dir = temp_dir("recall-pc-fake");
        let script = dir.path().join("recall-fake.sh");
        let calls = dir.path().join("calls.txt");
        let stdout_path = dir.path().join("export.jsonl");
        let stderr_path = dir.path().join("export.stderr");
        let scope = if export_stdout.is_empty() {
            serde_json::json!({"kind":"directory", "repository":null})
        } else {
            serde_json::json!({"kind":"repository", "value":"github.com/samzong/Recall", "repository":{"remote":"github.com/samzong/Recall"}})
        };
        let scope_line =
            serde_json::json!({"record_type":"scope", "protocol_version":2, "scope":scope});
        let info_path = dir.path().join("info.json");
        fs::write(&info_path, serde_json::json!({"protocol_version":2,"scope":scope}).to_string())
            .unwrap();
        fs::write(&stdout_path, format!("{scope_line}\n{export_stdout}")).unwrap();
        fs::write(&stderr_path, export_stderr).unwrap();
        fs::write(&calls, "").unwrap();
        let script_body = format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> "{calls}"
if [ "$1" = "info" ]; then
  cat "{info_path}"
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
            info_path = info_path.display(),
            stdout_path = stdout_path.display(),
            stderr_path = stderr_path.display(),
            export_exit_code = export_exit_code,
        );
        fs::write(&script, script_body).unwrap();
        make_executable(&script);
        Self { _dir: dir, script, calls }
    }

    pub(super) fn set_scope(&self, scope: serde_json::Value) {
        fs::write(
            self._dir.path().join("info.json"),
            serde_json::json!({"protocol_version":2,"scope":scope}).to_string(),
        )
        .unwrap();
    }

    pub(super) fn script_path(&self) -> &Path {
        &self.script
    }

    pub(super) fn calls(&self) -> Vec<String> {
        fs::read_to_string(&self.calls).unwrap().lines().map(str::to_string).collect()
    }
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
