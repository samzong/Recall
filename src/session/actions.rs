use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) fn cmd_session_share(
    selector: &SessionSelector,
    dry_run: bool,
    open: bool,
    copy_url: bool,
    tldr_file: Option<&Path>,
    format: SessionActionFormat,
) -> Result<()> {
    let store = Store::open()?;
    let sources = adapters::source_labels();
    let session = resolve_session_ref(&store, &sources, selector)?;
    let messages = store.get_messages(&session.id)?;
    let events = store.list_session_events_for_session(&session.id)?;
    let usage_events = store.list_usage_events_for_session(&session.id)?;
    let config = AppConfig::load()?;
    let tldr_markdown = tldr_file.and_then(read_tldr_file);
    let render_options = crate::share::ShareRenderOptions { tldr_markdown };
    let preview = crate::share::preview_session_with_options(
        &config,
        &session,
        &messages,
        &events,
        &usage_events,
        &render_options,
    )?;
    let url = if dry_run {
        preview.url.clone()
    } else {
        eprintln!("Sharing session {}...", session.id);
        crate::share::publish_session_with_options(
            &config,
            &session,
            &messages,
            &events,
            &usage_events,
            &render_options,
        )?
    };

    if copy_url {
        crate::utils::copy_to_clipboard(&url)?;
    }
    if open {
        crate::utils::open_url_in_default_browser(&url)?;
    }

    match format {
        SessionActionFormat::Text => {
            if dry_run {
                println!("Dry run OK");
            }
            println!("{url}");
        }
        SessionActionFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "session": session_ref_json(&session),
                    "share": {
                        "provider": preview.provider,
                        "project_name": preview.project_name,
                        "project_domain": preview.project_domain,
                        "publish_dir": preview.publish_dir,
                        "file_path": preview.file_path,
                        "share_id": preview.share_id,
                        "url": url,
                        "html_bytes": preview.html_bytes
                    },
                    "dry_run": dry_run
                }))?
            );
        }
    }
    Ok(())
}

pub(super) fn cmd_session_command(
    selector: &SessionSelector,
    print_command: bool,
    format: SessionActionFormat,
    action: SessionAction,
) -> Result<()> {
    let store = Store::open()?;
    let sources = adapters::source_labels();
    let session = resolve_session_ref(&store, &sources, selector)?;
    if session.is_import || !store.has_native_binding(&session.id)? {
        anyhow::bail!("imported session is not resumable on this machine");
    }
    let command = session_action::command_for(action, &session.source, &session.source_id)
        .ok_or_else(|| {
            anyhow::anyhow!("{} is not supported for {}", action.label(), session.source)
        })?;

    if print_command {
        match format {
            SessionActionFormat::Text => println!("{}", command.display()),
            SessionActionFormat::Json => {
                let display = command.display();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "session": session_ref_json(&session),
                        "command": {
                            "program": command.program,
                            "args": command.args,
                            "display": display
                        }
                    }))?
                );
            }
        }
        return Ok(());
    }

    session_action::run(&command, session.directory.as_deref())
}

pub(super) fn cmd_session_handoff(
    selector: &SessionSelector,
    target: &str,
    print_prompt: bool,
) -> Result<()> {
    let store = Store::open()?;
    let sources = adapters::source_labels();
    let session = resolve_session_ref(&store, &sources, selector)?;
    let target = handoff::target_for(target)?;
    let messages = store.get_messages(&session.id)?;
    let prompt = handoff::build_prompt(&session, &messages);

    if print_prompt {
        print!("{prompt}");
        return Ok(());
    }

    let command = handoff::command_for_target(&target, prompt)?;
    handoff::require_installed(&command)?;
    session_action::run(&command, handoff_working_directory(session.directory.as_deref()))
}

pub(super) fn handoff_working_directory(directory: Option<&str>) -> Option<&str> {
    directory.filter(|dir| std::path::Path::new(*dir).is_dir())
}

pub(super) fn read_tldr_file(path: &Path) -> Option<String> {
    match fs::read_to_string(path) {
        Ok(content) => {
            let trimmed = content.trim();
            if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
        }
        Err(err) => {
            eprintln!("Warning: skipping TL;DR file {}: {err}", path.display());
            None
        }
    }
}
