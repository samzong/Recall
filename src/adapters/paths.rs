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

pub(crate) fn file_uri_to_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let decoded = percent_decode(rest)?;
    if cfg!(windows)
        && let Some(drive) = decoded.strip_prefix('/')
        && drive.len() >= 2
        && drive.as_bytes()[1] == b':'
    {
        return Some(drive.to_string());
    }
    Some(decoded)
}

fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return None;
            }
            let hi = from_hex(bytes[index + 1])?;
            let lo = from_hex(bytes[index + 2])?;
            out.push((hi << 4) | lo);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn from_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

pub(crate) fn vscode_extension_task_dirs(extension_id: &str) -> Vec<PathBuf> {
    vscode_extension_task_dirs_from(dirs::config_dir(), extension_id)
}

pub(crate) fn vscode_extension_storage_dirs_from(
    config_dir: Option<PathBuf>,
    extension_id: &str,
) -> Vec<PathBuf> {
    let Some(config_dir) = config_dir else {
        return Vec::new();
    };
    VSCODE_HOSTS
        .iter()
        .map(|host| config_dir.join(host).join("User").join("globalStorage").join(extension_id))
        .filter(|path| path.is_dir())
        .collect()
}

pub(crate) fn vscode_extension_task_dirs_from(
    config_dir: Option<PathBuf>,
    extension_id: &str,
) -> Vec<PathBuf> {
    vscode_extension_storage_dirs_from(config_dir, extension_id)
        .into_iter()
        .map(|dir| dir.join("tasks"))
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
    fn file_uri_path_decodes_percent_escapes() {
        assert_eq!(
            file_uri_to_path("file:///Users/x/git/foo%20bar").as_deref(),
            Some("/Users/x/git/foo bar")
        );
        assert_eq!(file_uri_to_path("https://example.com"), None);
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

    #[test]
    fn storage_dirs_include_cursor_and_skip_missing_hosts() {
        let root = tempfile::tempdir().unwrap();
        let cursor =
            root.path().join("Cursor").join("User").join("globalStorage").join("sourcegraph.amp");
        let code =
            root.path().join("Code").join("User").join("globalStorage").join("sourcegraph.amp");
        fs::create_dir_all(&cursor).unwrap();
        fs::create_dir_all(&code).unwrap();
        fs::create_dir_all(
            root.path().join("VSCodium").join("User").join("globalStorage").join("other"),
        )
        .unwrap();
        let dirs =
            vscode_extension_storage_dirs_from(Some(root.path().to_path_buf()), "sourcegraph.amp");
        assert_eq!(dirs, vec![code, cursor]);
    }
}
