use super::*;

pub(super) fn cmd_session_export(
    ids: Vec<String>,
    source_filter: Option<&str>,
    source_id: Option<&str>,
    ids_file: Option<PathBuf>,
    format: SessionExportFormat,
    include: Option<&str>,
    output: Option<PathBuf>,
) -> Result<()> {
    let store = Store::open()?;
    let sources = adapters::source_labels();
    let sessions = resolve_session_refs(&store, &sources, ids, source_filter, source_id, ids_file)?;
    if sessions.is_empty() {
        anyhow::bail!("no sessions selected; pass --id, --ids-file, or --source with --source-id");
    }

    match format {
        SessionExportFormat::Jsonl => {
            let options = ExportOptions {
                session_ids: sessions.iter().map(|session| session.id.clone()).collect(),
                sources: None,
                time_range: TimeRange::All,
                scope: ProjectScope::Global,
                thread_role: None,
                limit: None,
                includes: crate::export::parse_export_includes(include)?,
            };
            if let Some(path) = output {
                write_jsonl_file(&store, &options, &path)?;
            } else {
                let stdout = std::io::stdout();
                let handle = stdout.lock();
                crate::export::write_jsonl(&store, &options, handle)?;
            }
        }
        SessionExportFormat::Text => {
            let mut content = String::new();
            for (idx, session) in sessions.iter().enumerate() {
                if idx > 0 {
                    content.push_str("\n\n");
                }
                let messages = store.get_messages(&session.id)?;
                content.push_str(&transcript::render_plain(session, &messages));
            }
            if let Some(path) = output {
                ensure_parent_dir(&path)?;
                fs::write(path, content)?;
            } else {
                print!("{content}");
            }
        }
    }
    Ok(())
}

fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

pub(super) fn write_jsonl_file(store: &Store, options: &ExportOptions, path: &Path) -> Result<()> {
    ensure_parent_dir(path)?;
    let target = resolve_output_target(path)?;
    let path = target.as_path();
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    #[cfg(unix)]
    let mut builder = tempfile::Builder::new();
    #[cfg(not(unix))]
    let builder = tempfile::Builder::new();
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    #[cfg(unix)]
    let existing_permissions = match fs::metadata(path) {
        Ok(metadata) => Some(fs::Permissions::from_mode(metadata.permissions().mode() & 0o777)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    #[cfg(unix)]
    if existing_permissions.is_none() {
        builder.permissions(fs::Permissions::from_mode(0o666));
    }
    let mut temp = builder.tempfile_in(parent)?;
    crate::export::write_jsonl(store, options, temp.as_file_mut())?;
    #[cfg(unix)]
    if let Some(permissions) = existing_permissions {
        temp.as_file().set_permissions(permissions)?;
    }
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn resolve_output_target(path: &Path) -> Result<PathBuf> {
    let mut target = path.to_path_buf();
    for _ in 0..40 {
        match fs::symlink_metadata(&target) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let link = fs::read_link(&target)?;
                target = if link.is_absolute() {
                    link
                } else {
                    target
                        .parent()
                        .filter(|parent| !parent.as_os_str().is_empty())
                        .unwrap_or_else(|| Path::new("."))
                        .join(link)
                };
            }
            Ok(_) => return Ok(target),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(target),
            Err(error) => return Err(error.into()),
        }
    }
    anyhow::bail!("too many symbolic links in output path: {}", path.display())
}
