use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const DATA_FILE_SUFFIX: &str = ".recall.jsonl";
pub const MANIFEST_FILE: &str = "manifest.json";
pub const PREVIEW_FILE: &str = "preview.json";
pub const FINDINGS_FILE: &str = "findings.local.json";
pub const APPROVAL_FILE: &str = "approval.json";
pub const RECEIPT_FILE: &str = "receipt.json";
pub const CARD_FILE: &str = "README.md";

pub const PUBLIC_FILES: [&str; 2] = [MANIFEST_FILE, CARD_FILE];

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Approval {
    pub scope: String,
    pub approved_at: String,
    pub digests: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Receipt {
    pub scope: String,
    pub repo: String,
    pub url: String,
    pub uploaded_at: String,
    pub digests: BTreeMap<String, String>,
}

pub struct Workspace {
    pub root: PathBuf,
    pub scope: String,
}

impl Workspace {
    pub fn new(publish_root: &Path, scope: &str) -> Self {
        Self { root: publish_root.join("scopes").join(scope), scope: scope.to_string() }
    }

    pub fn data_file(&self) -> PathBuf {
        self.root.join(format!("{}{DATA_FILE_SUFFIX}", self.scope))
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    pub fn exists(&self) -> bool {
        self.root.is_dir()
    }

    pub fn reset(&self) -> Result<()> {
        if self.root.exists() {
            fs::remove_dir_all(&self.root)
                .with_context(|| format!("failed to clear {}", self.root.display()))?;
        }
        fs::create_dir_all(&self.root)
            .with_context(|| format!("failed to create {}", self.root.display()))?;
        Ok(())
    }

    pub fn public_paths(&self) -> Vec<(String, PathBuf)> {
        let mut paths = vec![(format!("{}{DATA_FILE_SUFFIX}", self.scope), self.data_file())];
        for name in PUBLIC_FILES {
            paths.push((name.to_string(), self.path(name)));
        }
        paths
    }

    pub fn local_only_paths(&self) -> Vec<(String, PathBuf)> {
        [PREVIEW_FILE, FINDINGS_FILE, APPROVAL_FILE, RECEIPT_FILE]
            .into_iter()
            .map(|name| (name.to_string(), self.path(name)))
            .collect()
    }

    pub fn digests(&self) -> Result<BTreeMap<String, String>> {
        let mut digests = BTreeMap::new();
        for (name, path) in self.public_paths() {
            let bytes = fs::read(&path).with_context(|| {
                format!("{} is missing; run `recall publish prepare`", path.display())
            })?;
            digests.insert(name, digest_of(&bytes));
        }
        Ok(digests)
    }

    pub fn write(&self, name: &str, contents: &[u8]) -> Result<()> {
        let path = self.path(name);
        fs::write(&path, contents).with_context(|| format!("failed to write {}", path.display()))
    }

    pub fn write_json(&self, name: &str, value: &serde_json::Value) -> Result<()> {
        let mut bytes = serde_json::to_vec_pretty(value)?;
        bytes.push(b'\n');
        self.write(name, &bytes)
    }

    pub fn read_approval(&self) -> Result<Approval> {
        let path = self.path(APPROVAL_FILE);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                bail!(
                    "scope `{}` has not been approved; run `recall publish approve {}`",
                    self.scope,
                    self.scope
                );
            }
            Err(error) => {
                return Err(error).with_context(|| format!("failed to read {}", path.display()));
            }
        };
        serde_json::from_slice(&bytes)
            .with_context(|| format!("{} is not a valid approval record", path.display()))
    }

    pub fn clear_approval(&self) -> Result<()> {
        for name in [APPROVAL_FILE, RECEIPT_FILE] {
            let path = self.path(name);
            if path.exists() {
                fs::remove_file(&path)
                    .with_context(|| format!("failed to remove {}", path.display()))?;
            }
        }
        Ok(())
    }

    pub fn verify_against_approval(&self) -> Result<Approval> {
        let approval = self.read_approval()?;
        let current = self.digests()?;
        if current != approval.digests {
            bail!(
                "scope `{}` changed after approval; run `recall publish prepare` and approve the new content",
                self.scope
            );
        }
        Ok(approval)
    }
}

pub fn digest_of(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

pub fn publish_root(data_dir: &Path) -> PathBuf {
    data_dir.join("publish")
}

pub fn default_data_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("RECALL_PUBLISH_HOME") {
        return Ok(PathBuf::from(dir));
    }
    dirs::data_dir()
        .map(|dir| dir.join("recall"))
        .context("no data directory is available on this platform")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(dir: &Path) -> Workspace {
        let workspace = Workspace::new(dir, "scope-a");
        workspace.reset().unwrap();
        workspace.write("scope-a.recall.jsonl", b"{}\n").unwrap();
        workspace.write(MANIFEST_FILE, b"{}\n").unwrap();
        workspace.write(CARD_FILE, b"# scope-a\n").unwrap();
        workspace
    }

    #[test]
    fn approval_binds_the_exact_prepared_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = workspace(dir.path());
        let approval = Approval {
            scope: "scope-a".to_string(),
            approved_at: "2026-09-18T00:00:00Z".to_string(),
            digests: workspace.digests().unwrap(),
        };
        workspace.write_json(APPROVAL_FILE, &serde_json::to_value(&approval).unwrap()).unwrap();
        assert!(workspace.verify_against_approval().is_ok());

        workspace.write("scope-a.recall.jsonl", b"{\"changed\":true}\n").unwrap();
        assert!(workspace.verify_against_approval().is_err());
    }

    #[test]
    fn manifest_changes_also_invalidate_approval() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = workspace(dir.path());
        let approval = Approval {
            scope: "scope-a".to_string(),
            approved_at: "2026-09-18T00:00:00Z".to_string(),
            digests: workspace.digests().unwrap(),
        };
        workspace.write_json(APPROVAL_FILE, &serde_json::to_value(&approval).unwrap()).unwrap();
        workspace.write(MANIFEST_FILE, b"{\"license\":\"MIT\"}\n").unwrap();
        assert!(workspace.verify_against_approval().is_err());
    }

    #[test]
    fn preparing_again_drops_a_previous_approval_and_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = workspace(dir.path());
        workspace.write(APPROVAL_FILE, b"{}").unwrap();
        workspace.write(RECEIPT_FILE, b"{}").unwrap();
        workspace.clear_approval().unwrap();
        assert!(!workspace.path(APPROVAL_FILE).exists());
        assert!(!workspace.path(RECEIPT_FILE).exists());
    }

    #[test]
    fn upload_without_approval_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = workspace(dir.path());
        assert!(workspace.verify_against_approval().is_err());
    }

    #[test]
    fn local_only_artifacts_are_not_part_of_the_public_digest_set() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = workspace(dir.path());
        workspace.write(FINDINGS_FILE, b"{\"secret\":\"real\"}").unwrap();
        workspace.write(PREVIEW_FILE, b"{}").unwrap();
        let names: Vec<String> =
            workspace.public_paths().into_iter().map(|(name, _)| name).collect();
        assert_eq!(names, vec!["scope-a.recall.jsonl", MANIFEST_FILE, CARD_FILE]);
    }
}
