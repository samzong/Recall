use std::path::PathBuf;

const VSCODE_HOSTS: &[&str] = &[
    "Code",
    "Code - Insiders",
    "Cursor",
    "VSCodium",
    "VSCodium - Insiders",
    "Windsurf",
    "Code - OSS",
];

pub(crate) fn resolve_home_dir(
    relative: &str,
    missing_message: &str,
) -> anyhow::Result<Option<PathBuf>> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home dir"))?;
    let dir = home.join(relative);
    if !dir.exists() {
        tracing::debug!("{missing_message}");
        return Ok(None);
    }
    Ok(Some(dir))
}

pub(crate) fn vscode_extension_task_dirs(extension_id: &str) -> Vec<PathBuf> {
    vscode_extension_task_dirs_from(dirs::config_dir(), extension_id)
}

pub(crate) fn vscode_extension_task_dirs_from(
    config_dir: Option<PathBuf>,
    extension_id: &str,
) -> Vec<PathBuf> {
    let Some(config_dir) = config_dir else {
        return Vec::new();
    };
    VSCODE_HOSTS
        .iter()
        .map(|host| {
            config_dir
                .join(host)
                .join("User")
                .join("globalStorage")
                .join(extension_id)
                .join("tasks")
        })
        .filter(|path| path.is_dir())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_tasks_dir(config: &std::path::Path, host: &str, extension_id: &str) -> PathBuf {
        let tasks =
            config.join(host).join("User").join("globalStorage").join(extension_id).join("tasks");
        fs::create_dir_all(&tasks).unwrap();
        tasks
    }

    #[test]
    fn missing_config_dir_returns_empty() {
        assert!(vscode_extension_task_dirs_from(None, "saoudrizwan.claude-dev").is_empty());
    }

    #[test]
    fn empty_config_dir_returns_empty() {
        let root = tempfile::tempdir().unwrap();
        assert!(
            vscode_extension_task_dirs_from(
                Some(root.path().to_path_buf()),
                "saoudrizwan.claude-dev"
            )
            .is_empty()
        );
    }

    #[test]
    fn collects_only_existing_host_task_dirs() {
        let root = tempfile::tempdir().unwrap();
        let cursor = write_tasks_dir(root.path(), "Cursor", "saoudrizwan.claude-dev");
        let code = write_tasks_dir(root.path(), "Code", "saoudrizwan.claude-dev");
        write_tasks_dir(root.path(), "Code", "rooveterinaryinc.roo-cline");
        fs::create_dir_all(
            root.path()
                .join("VSCodium")
                .join("User")
                .join("globalStorage")
                .join("saoudrizwan.claude-dev"),
        )
        .unwrap();

        let dirs = vscode_extension_task_dirs_from(
            Some(root.path().to_path_buf()),
            "saoudrizwan.claude-dev",
        );
        assert_eq!(dirs, vec![code, cursor]);
    }

    #[test]
    fn roo_extension_id_is_independent() {
        let root = tempfile::tempdir().unwrap();
        let roo = write_tasks_dir(root.path(), "Code", "rooveterinaryinc.roo-cline");
        write_tasks_dir(root.path(), "Code", "saoudrizwan.claude-dev");
        let dirs = vscode_extension_task_dirs_from(
            Some(root.path().to_path_buf()),
            "rooveterinaryinc.roo-cline",
        );
        assert_eq!(dirs, vec![roo]);
    }

    #[test]
    fn linux_style_code_oss_is_included() {
        let root = tempfile::tempdir().unwrap();
        let oss = write_tasks_dir(root.path(), "Code - OSS", "saoudrizwan.claude-dev");
        let dirs = vscode_extension_task_dirs_from(
            Some(root.path().to_path_buf()),
            "saoudrizwan.claude-dev",
        );
        assert_eq!(dirs, vec![oss]);
    }
}
