use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fs2::FileExt;
use tempfile::NamedTempFile;

pub(crate) fn appended(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

pub(crate) fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

fn parent(path: &Path) -> Result<&Path> {
    let parent =
        path.parent().with_context(|| format!("path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    Ok(parent)
}

pub(crate) fn lock(path: &Path) -> Result<File> {
    parent(path)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    file.lock_exclusive().with_context(|| format!("failed to lock {}", path.display()))?;
    Ok(file)
}

pub(crate) fn stage(path: &Path, bytes: &[u8]) -> Result<NamedTempFile> {
    let mut temp = NamedTempFile::new_in(parent(path)?)
        .with_context(|| format!("failed to create temporary {}", path.display()))?;
    temp.write_all(bytes)
        .with_context(|| format!("failed to write temporary {}", path.display()))?;
    temp.as_file()
        .sync_all()
        .with_context(|| format!("failed to sync temporary {}", path.display()))?;
    Ok(temp)
}

pub(crate) fn persist(temp: NamedTempFile, path: &Path) -> Result<()> {
    temp.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

pub(crate) fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    persist(stage(path, bytes)?, path)
}

pub(crate) fn secret_mode(file: &File, path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to chmod {}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = (file, path);
    Ok(())
}
