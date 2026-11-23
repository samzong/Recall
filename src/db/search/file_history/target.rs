use super::super::SearchEngine;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileHistoryTarget {
    pub(crate) requested_path: String,
    pub(crate) repo_remote: Option<String>,
    pub(crate) repo_root: Option<String>,
    pub(crate) path: Option<String>,
    pub(crate) absolute_path: Option<String>,
}

impl SearchEngine<'_> {
    pub(crate) fn resolve_file_history_target(
        &self,
        project: &str,
        path: &str,
    ) -> anyhow::Result<FileHistoryTarget> {
        use crate::project_scope::{SelectorForm, classify_selector};
        use crate::repo_identity::{
            RepoIdentityCache, git_toplevel, normalize_remote_url, origin_identity,
        };
        let project = project.trim();
        anyhow::ensure!(!project.is_empty(), "target_project is required");
        anyhow::ensure!(
            !path.is_empty()
                && !path.starts_with('~')
                && path.len() <= 4096
                && !path.contains('\0'),
            "invalid target path"
        );
        let (remote, root) = match classify_selector(project) {
            SelectorForm::Global => {
                anyhow::bail!("target_project must identify one repository or directory")
            }
            SelectorForm::Directory => {
                let directory = std::fs::canonicalize(project).map_err(|_| {
                    anyhow::anyhow!(
                        "target_project directory is unavailable; pass an indexed remote identity"
                    )
                })?;
                let directory = directory
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("invalid target directory"))?;
                let root = git_toplevel(directory)?.unwrap_or_else(|| directory.to_string());
                let remote = origin_identity(&root)?.map(|identity| identity.remote);
                (remote, Some(root))
            }
            SelectorForm::Repository(identity) => (Some(identity.remote), None),
            form => {
                let mut stmt = self.conn.prepare("SELECT DISTINCT json_extract(evidence_json, '$.target.repo_remote') FROM event_files WHERE json_extract(evidence_json, '$.target.repo_remote') IS NOT NULL")?;
                let mut matches = Vec::new();
                for remote in stmt.query_map([], |row| row.get::<_, String>(0))? {
                    let remote = remote?;
                    let Some(identity) = normalize_remote_url(&remote) else {
                        continue;
                    };
                    let matches_selector = match &form {
                        SelectorForm::Slug(slug) => identity.slug == *slug,
                        SelectorForm::IndexedName => identity.name == project,
                        _ => false,
                    };
                    if matches_selector {
                        matches.push(remote);
                    }
                }
                anyhow::ensure!(
                    matches.len() == 1,
                    "target_project has no unique indexed target identity; pass an absolute directory or remote URL"
                );
                (matches.pop(), None)
            }
        };
        let mut absolute_path = None;
        let relative = if std::path::Path::new(path).is_absolute() || root.is_some() {
            let resolved = RepoIdentityCache::default().resolve_file(path, root.as_deref())?;
            if let Some(file) = resolved.filter(|file| {
                remote.as_deref().is_some_and(|remote| file.repo_remote.as_deref() == Some(remote))
                    || root.as_deref().is_some_and(|root| {
                        file.repo_root.as_deref() == Some(root)
                            || (file.repo_root.is_none()
                                && std::path::Path::new(&file.absolute_path).starts_with(root))
                    })
            }) {
                absolute_path = Some(file.absolute_path.clone());
                file.repo_relative_path
                    .or_else(|| {
                        root.as_deref()
                            .and_then(|root| {
                                std::path::Path::new(&file.absolute_path).strip_prefix(root).ok()
                            })
                            .and_then(std::path::Path::to_str)
                            .map(str::to_string)
                    })
                    .ok_or_else(|| anyhow::anyhow!("target path is outside target_project"))?
            } else if std::path::Path::new(path).is_absolute() {
                let mut stmt = self.conn.prepare("SELECT DISTINCT json_extract(evidence_json, '$.target.repo_relative_path') FROM event_files WHERE json_extract(evidence_json, '$.target.repo_remote') = ?1 AND json_extract(evidence_json, '$.target.absolute_path') = ?2")?;
                let paths = stmt
                    .query_map(rusqlite::params![remote, path], |row| {
                        row.get::<_, Option<String>>(0)
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                if paths.len() != 1 || paths[0].is_none() {
                    return Ok(FileHistoryTarget {
                        requested_path: path.to_string(),
                        repo_remote: remote,
                        repo_root: root,
                        path: None,
                        absolute_path: Some(path.to_string()),
                    });
                }
                absolute_path = Some(path.to_string());
                paths
                    .into_iter()
                    .next()
                    .flatten()
                    .ok_or_else(|| anyhow::anyhow!("target path identity is unresolved"))?
            } else {
                anyhow::bail!("target path is outside target_project or cannot be resolved");
            }
        } else {
            path.to_string()
        };
        let mut normalized = std::path::PathBuf::new();
        for component in std::path::Path::new(&relative).components() {
            match component {
                std::path::Component::Normal(part) => normalized.push(part),
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir if normalized.pop() => {}
                _ => anyhow::bail!("target path must stay inside target_project"),
            }
        }
        anyhow::ensure!(!normalized.as_os_str().is_empty(), "target path must identify a file");
        Ok(FileHistoryTarget {
            requested_path: path.to_string(),
            repo_remote: remote,
            repo_root: root,
            path: Some(
                normalized
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("invalid target path"))?
                    .to_string(),
            ),
            absolute_path,
        })
    }
}
