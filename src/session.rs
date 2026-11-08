use std::fs;
use std::path::{Path, PathBuf};

use crate::adapters;
use crate::config::AppConfig;
use crate::db::message_store::{MessageRead, MessageWindow};
use crate::db::search::{SearchEngine, SearchFilters, TimeRange};
use crate::db::store::{SessionListSort, Store};
use crate::export::{ExportIncludes, ExportOptions};
use crate::handoff;
use crate::project_scope::ProjectScope;
use crate::query::{parse_time_range, query_embedding, resolve_source_filter};
use crate::semantic;
use crate::session_action::{self, SessionAction};
use crate::types::{MatchSource, Session};
use crate::{sync::SyncRunOptions, sync::run_sync_job_inner, transcript};
use anyhow::Result;

pub(crate) use crate::cli::session_args::*;

mod actions;
mod detail;
mod export;
mod list;

use actions::*;
use detail::*;
use export::*;
pub(crate) use list::run_session_list;

pub(crate) fn cmd_session(command: SessionCommands) -> Result<()> {
    match command {
        SessionCommands::List(args) => run_session_list(&args),
        SessionCommands::Show {
            selector,
            messages,
            include,
            from_seq,
            to_seq,
            around_seq,
            before,
            after,
            max_chars,
            cursor,
            role,
            format,
        } => cmd_session_show(
            &selector,
            messages,
            include.as_deref(),
            MessageWindow { from_seq, to_seq, around_seq, before, after },
            max_chars,
            cursor.as_deref(),
            role,
            format,
        ),
        SessionCommands::Export { ids, source, source_id, ids_file, format, include, output } => {
            cmd_session_export(
                ids,
                source.as_deref(),
                source_id.as_deref(),
                ids_file,
                format,
                include.as_deref(),
                output,
            )
        }
        SessionCommands::Share { selector, dry_run, open, copy_url, tldr_file, format } => {
            cmd_session_share(&selector, dry_run, open, copy_url, tldr_file.as_deref(), format)
        }
        SessionCommands::Resume { selector, print_command, format } => {
            cmd_session_command(&selector, print_command, format, SessionAction::Resume)
        }
        SessionCommands::Open { selector, print_command, format } => {
            cmd_session_command(&selector, print_command, format, SessionAction::OpenApp)
        }
        SessionCommands::Handoff { selector, to, print_prompt } => {
            cmd_session_handoff(&selector, &to, print_prompt)
        }
    }
}

fn resolve_session_ref(
    store: &Store,
    sources: &[(String, String)],
    selector: &SessionSelector,
) -> Result<Session> {
    match (selector.id.as_deref(), selector.source.as_deref(), selector.source_id.as_deref()) {
        (Some(id), None, None) => {
            store.get_session_by_id(id)?.ok_or_else(|| anyhow::anyhow!("session not found: {id}"))
        }
        (None, Some(source), Some(source_id)) => {
            let source = crate::query::resolve_source_id(source, sources)?;
            store.get_session_by_source_id(&source, source_id)?.ok_or_else(|| {
                anyhow::anyhow!("session not found: source={source} source_id={source_id}")
            })
        }
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
            anyhow::bail!("use either --id or --source with --source-id, not both")
        }
        (None, Some(_), None) | (None, None, Some(_)) => {
            anyhow::bail!("--source and --source-id must be provided together")
        }
        (None, None, None) => {
            anyhow::bail!("missing session selector: pass --id or --source with --source-id")
        }
    }
}

fn resolve_session_refs(
    store: &Store,
    sources: &[(String, String)],
    mut ids: Vec<String>,
    source_filter: Option<&str>,
    source_id: Option<&str>,
    ids_file: Option<PathBuf>,
) -> Result<Vec<Session>> {
    if let Some(path) = ids_file {
        let content = fs::read_to_string(&path)?;
        ids.extend(
            content
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .map(String::from),
        );
    }

    let mut sessions = Vec::new();
    for id in ids {
        sessions.push(resolve_session_ref(
            store,
            sources,
            &SessionSelector { id: Some(id), source: None, source_id: None },
        )?);
    }
    if source_filter.is_some() || source_id.is_some() {
        sessions.push(resolve_session_ref(
            store,
            sources,
            &SessionSelector {
                id: None,
                source: source_filter.map(str::to_string),
                source_id: source_id.map(str::to_string),
            },
        )?);
    }
    Ok(sessions)
}

fn session_ref_json(session: &Session) -> serde_json::Value {
    serde_json::json!({
        "id": session.id,
        "source": session.source,
        "source_id": session.source_id,
        "title": session.title
    })
}

fn source_label_for<'a>(source: &'a str, sources: &'a [(String, String)]) -> &'a str {
    sources.iter().find(|(id, _)| id == source).map(|(_, label)| label.as_str()).unwrap_or(source)
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handoff_working_directory_keeps_existing_directory() {
        let directory = std::env::temp_dir().to_string_lossy().into_owned();

        assert_eq!(handoff_working_directory(Some(directory.as_str())), Some(directory.as_str()));
    }

    #[test]
    fn read_tldr_file_trims_blank_and_content() {
        let path = std::env::temp_dir().join(format!("recall-tldr-{}.md", uuid::Uuid::new_v4()));
        fs::write(&path, "  **Summary**  \n").unwrap();

        assert_eq!(read_tldr_file(&path).as_deref(), Some("**Summary**"));

        fs::write(&path, " \n\t").unwrap();
        assert_eq!(read_tldr_file(&path), None);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn jsonl_file_export_replaces_target_only_after_success() {
        let dir = tempfile::tempdir().unwrap();
        let target_path = dir.path().join("export.jsonl");
        fs::write(&target_path, "keep").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&target_path, fs::Permissions::from_mode(0o640)).unwrap();
        }
        #[cfg(unix)]
        let path = {
            let path = dir.path().join("export-link.jsonl");
            std::os::unix::fs::symlink("export.jsonl", &path).unwrap();
            path
        };
        #[cfg(not(unix))]
        let path = target_path.clone();
        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let mut options = ExportOptions {
            session_ids: vec!["missing".to_string()],
            sources: None,
            time_range: TimeRange::All,
            scope: ProjectScope::Global,
            thread_role: None,
            limit: None,
            includes: ExportIncludes::full(),
        };

        assert!(write_jsonl_file(&store, &options, &path).is_err());
        assert_eq!(fs::read_to_string(&target_path).unwrap(), "keep");

        options.session_ids.clear();
        write_jsonl_file(&store, &options, &path).unwrap();
        assert_eq!(fs::read_to_string(&target_path).unwrap(), "");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert!(fs::symlink_metadata(&path).unwrap().file_type().is_symlink());
            assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
            assert_eq!(fs::metadata(target_path).unwrap().permissions().mode() & 0o777, 0o640);
        }
        #[cfg(not(unix))]
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
