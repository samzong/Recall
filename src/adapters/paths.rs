use std::path::PathBuf;

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
