use std::collections::{HashMap, HashSet};

use anyhow::Result;
use tracing::info;

use crate::adapters;
use crate::config::AppConfig;
use crate::db::store::{EventSessionStateMeta, SessionPath, Store, UsageSessionStateMeta};
use crate::query::resolve_source_filter;
use crate::repo_identity::{RepoIdentity, RepoIdentityCache};
use crate::semantic;
use crate::types::{Message, Role, Session};
use crate::utils;

#[derive(Debug, Clone)]
pub(crate) struct SyncRunOptions {
    pub(crate) force: bool,
    pub(crate) verbose: bool,
    pub(crate) emit: bool,
    pub(crate) usage_only: bool,
    pub(crate) backfill_events: bool,
    pub(crate) sources: Option<Vec<String>>,
}

pub(crate) fn run_cli(force: bool, verbose: bool, source_filter: Option<&str>) -> Result<()> {
    let labels = adapters::source_labels();
    let sources = resolve_source_filter(source_filter, &labels)?;
    run_sync_job_inner(SyncRunOptions {
        force,
        verbose,
        emit: true,
        usage_only: false,
        backfill_events: false,
        sources,
    })?;
    semantic::ensure_background_worker(false)?;
    Ok(())
}

pub(crate) fn run_sync_job(force: bool, verbose: bool) -> Result<()> {
    run_cli(force, verbose, None)
}

pub(crate) fn run_usage_sync_job() -> Result<()> {
    run_sync_job_inner(SyncRunOptions {
        force: false,
        verbose: false,
        emit: false,
        usage_only: true,
        backfill_events: false,
        sources: None,
    })
}

pub(crate) fn run_dashboard_sync_job() -> Result<()> {
    run_sync_job_inner(SyncRunOptions {
        force: false,
        verbose: false,
        emit: false,
        usage_only: true,
        backfill_events: true,
        sources: None,
    })
}

pub(crate) fn run_background_worker(sync_first: bool) -> Result<()> {
    semantic::run_background_worker(sync_first, || run_sync_job(false, false))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BackfillPlan {
    usage: bool,
    events: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExistingSessionAction {
    Skip,
    BackfillOnly(BackfillPlan),
    RefreshSession,
}

pub(crate) fn run_sync_job_inner(options: SyncRunOptions) -> Result<()> {
    let store = Store::open()?;
    let all = adapters::all_adapters();
    let labels = adapters::source_labels();
    let mut config = AppConfig::load_or_default();
    config.normalize_sources(&labels);
    let since_ts = if options.usage_only { None } else { config.sync_window.to_since_cutoff() };
    let path_excluder = config.build_path_excluder()?;
    let mut repo_cache = RepoIdentityCache::default();

    let mut new_sessions = 0u32;
    let mut updated_sessions = 0u32;
    let mut reprocessed_sessions = 0u32;
    let mut total_messages = 0u32;
    let mut skipped = 0u32;
    let mut filtered_out = 0u32;
    let mut excluded_out = 0u32;

    for adapter in &all {
        let source_id = adapter.id();
        let label = adapter.label();

        if options.usage_only
            && !adapters::adapter_supports_usage_dashboard(
                adapter.as_ref(),
                options.backfill_events,
            )
        {
            continue;
        }

        if let Some(sources) = &options.sources
            && !sources.iter().any(|id| id == source_id)
        {
            continue;
        }

        if !config.is_source_enabled(source_id) {
            if options.verbose {
                println!("Skipping {label} (filtered)");
            }
            continue;
        }

        let mut purged_excluded_ids = HashSet::new();
        if let Some(matcher) = &path_excluder {
            excluded_out += delete_excluded_sessions_for_source(
                &store,
                source_id,
                matcher,
                &mut purged_excluded_ids,
            )?;
        }

        if options.verbose {
            println!("Scanning {label}...");
        }
        if let Err(e) = adapter.prune(&store)
            && options.emit
        {
            eprintln!("Error pruning {label}: {e}");
        }
        let include_events = !options.usage_only || options.backfill_events;
        let optimized = if options.force {
            None
        } else {
            match adapter.scan_for_sync(&store, since_ts, include_events) {
                Ok(scan) => scan,
                Err(e) => {
                    if options.emit {
                        eprintln!("Error scanning {label}: {e}");
                    }
                    continue;
                }
            }
        };
        let (raw_sessions, pre_skipped, pre_filtered) = match optimized {
            Some(scan) => {
                (scan.sessions, scan.stats.skipped_sessions, scan.stats.filtered_sessions)
            }
            None => {
                let raw_sessions = match adapter.scan() {
                    Ok(s) => s,
                    Err(e) => {
                        if options.emit {
                            eprintln!("Error scanning {label}: {e}");
                        }
                        continue;
                    }
                };
                (raw_sessions, 0, 0)
            }
        };
        skipped += pre_skipped;
        filtered_out += pre_filtered;
        if let Some(matcher) = &path_excluder {
            excluded_out += delete_excluded_sessions_for_source(
                &store,
                source_id,
                matcher,
                &mut purged_excluded_ids,
            )?;
        }
        if options.verbose {
            println!("  Found {} sessions", raw_sessions.len());
        }

        let mut existing_meta = store.session_meta_map(source_id)?;
        let mut existing_paths = HashMap::new();
        for mut path in store.session_paths_for_source(source_id)? {
            if path.directory.is_some()
                && (path.repo_remote.is_none()
                    || path.repo_slug.is_none()
                    || path.repo_name.is_none())
            {
                let repo_identity = repo_cache.resolve(path.directory.as_deref());
                if let Some(repo) = repo_identity.as_ref() {
                    store.update_session_repo_identity(source_id, &path.source_id, repo)?;
                    path.repo_remote = Some(repo.remote.clone());
                    path.repo_slug = Some(repo.slug.clone());
                    path.repo_name = Some(repo.name.clone());
                }
            }
            existing_paths.insert(path.source_id.clone(), path);
        }
        let mut imported_ids = store.imported_source_ids(source_id)?;
        let mut existing_usage_meta = store.usage_state_meta_map(source_id)?;
        let mut existing_event_meta = if options.usage_only && !options.backfill_events {
            Default::default()
        } else {
            store.event_state_meta_map(source_id)?
        };

        for raw in raw_sessions {
            if let Some(cutoff) = since_ts {
                let ts = raw.updated_at.unwrap_or(raw.started_at);
                if ts < cutoff {
                    filtered_out += 1;
                    continue;
                }
            }

            let raw_source_id = raw.source_id.clone();

            if let Some(matcher) = &path_excluder
                && paths_match_excluded(
                    raw.directory.as_deref(),
                    raw.source_file_path.as_deref(),
                    matcher,
                )
            {
                if existing_meta.remove(&raw_source_id).is_some() {
                    store.delete_session_data(source_id, &raw_source_id)?;
                    existing_paths.remove(&raw_source_id);
                    existing_usage_meta.remove(&raw_source_id);
                    existing_event_meta.remove(&raw_source_id);
                }
                if purged_excluded_ids.insert(raw_source_id) {
                    excluded_out += 1;
                }
                continue;
            }

            let repo_identity = repo_cache.resolve(raw.directory.as_deref());
            let existing_repo_fields = existing_paths.get(&raw_source_id).filter(|old| {
                repo_identity.is_none() && old.directory.as_deref() == raw.directory.as_deref()
            });
            let (repo_remote, repo_slug, repo_name) = match repo_identity.as_ref() {
                Some(repo) => {
                    (Some(repo.remote.clone()), Some(repo.slug.clone()), Some(repo.name.clone()))
                }
                None => existing_repo_fields
                    .map(|old| {
                        (old.repo_remote.clone(), old.repo_slug.clone(), old.repo_name.clone())
                    })
                    .unwrap_or((None, None, None)),
            };
            let msg_count = raw.messages.len() as u32;
            let usage_backfill_needed = raw.usage_parser_version.is_some_and(|version| {
                !crate::adapters::sync_state::usage_state_is_current(
                    version,
                    existing_usage_meta.get(&raw_source_id).copied(),
                    raw.updated_at,
                )
            });
            let event_backfill_needed = (options.backfill_events || !options.usage_only)
                && raw.event_parser_version.is_some_and(|version| {
                    !crate::adapters::sync_state::event_state_is_current(
                        version,
                        existing_event_meta.get(&raw_source_id).copied(),
                        raw.updated_at,
                    )
                });

            match existing_meta.get(&raw_source_id) {
                Some(&(old_updated_at, old_msg_count)) => {
                    let was_imported = imported_ids.remove(&raw_source_id);
                    let metadata_changed = existing_paths.get(&raw_source_id).is_some_and(|old| {
                        raw_session_metadata_changed(&raw, repo_identity.as_ref(), old)
                    });
                    let content_changed = old_msg_count != msg_count
                        || metadata_changed
                        || (raw.updated_at.is_some() && raw.updated_at != old_updated_at);
                    match decide_existing_session_action(
                        options.usage_only,
                        options.backfill_events,
                        options.force,
                        content_changed,
                        usage_backfill_needed,
                        event_backfill_needed,
                    ) {
                        ExistingSessionAction::Skip => {
                            if was_imported {
                                store.clear_import_marker(source_id, &raw_source_id)?;
                            }
                            skipped += 1;
                            continue;
                        }
                        ExistingSessionAction::BackfillOnly(plan) => {
                            let mut reprocessed = false;
                            if plan.usage
                                && let Some(parser_version) = raw.usage_parser_version
                                && store.persist_usage_events_for_existing_session(
                                    source_id,
                                    &raw_source_id,
                                    &raw.usage_events,
                                    parser_version,
                                    raw.updated_at,
                                )?
                            {
                                existing_usage_meta.insert(
                                    raw_source_id.clone(),
                                    UsageSessionStateMeta {
                                        parser_version,
                                        source_updated_at: raw.updated_at,
                                    },
                                );
                                reprocessed = true;
                            }
                            if plan.events
                                && let Some(parser_version) = raw.event_parser_version
                                && store.persist_session_events_for_existing_session(
                                    source_id,
                                    &raw_source_id,
                                    &raw.events,
                                    parser_version,
                                    raw.updated_at,
                                )?
                            {
                                existing_event_meta.insert(
                                    raw_source_id.clone(),
                                    EventSessionStateMeta {
                                        parser_version,
                                        source_updated_at: raw.updated_at,
                                    },
                                );
                                reprocessed = true;
                            }
                            if raw.custom_title.is_some()
                                || raw.summary.is_some()
                                || raw.duration_minutes.is_some()
                            {
                                store.update_session_fields(
                                    source_id,
                                    &raw_source_id,
                                    raw.custom_title.as_deref(),
                                    raw.summary.as_deref(),
                                    raw.duration_minutes,
                                    None,
                                )?;
                            }
                            if was_imported {
                                store.clear_import_marker(source_id, &raw_source_id)?;
                            }
                            if reprocessed {
                                reprocessed_sessions += 1;
                            }
                            continue;
                        }
                        ExistingSessionAction::RefreshSession => {}
                    }
                    existing_usage_meta.remove(&raw_source_id);
                    existing_event_meta.remove(&raw_source_id);
                    if content_changed {
                        updated_sessions += 1;
                    } else {
                        reprocessed_sessions += 1;
                    }
                }
                None => {
                    new_sessions += 1;
                }
            }

            let session_uuid = uuid::Uuid::new_v4().to_string();
            let title = raw
                .custom_title
                .clone()
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| generate_title(&raw.messages));

            let session = Session {
                id: session_uuid.clone(),
                source: source_id.to_string(),
                source_id: raw.source_id,
                title,
                directory: raw.directory,
                repo_remote,
                repo_slug,
                repo_name,
                started_at: raw.started_at,
                updated_at: raw.updated_at,
                message_count: msg_count,
                entrypoint: raw.entrypoint,
                custom_title: raw.custom_title,
                summary: raw.summary,
                duration_minutes: raw.duration_minutes,
                source_file_path: raw.source_file_path,
                is_import: false,
            };

            let messages: Vec<Message> = raw
                .messages
                .into_iter()
                .enumerate()
                .map(|(i, m)| Message {
                    session_id: session_uuid.clone(),
                    role: m.role,
                    content: m.content,
                    timestamp: m.timestamp,
                    seq: i as u32,
                })
                .collect();

            let persist_events = !options.usage_only || options.backfill_events;
            let (events, event_parser_version) = if persist_events {
                (raw.events, raw.event_parser_version)
            } else {
                (Vec::new(), None)
            };

            store.replace_session_with_usage_and_events(
                source_id,
                &raw_source_id,
                &session,
                &messages,
                &raw.usage_events,
                raw.usage_parser_version,
                &events,
                event_parser_version,
            )?;
            existing_meta
                .insert(session.source_id.clone(), (session.updated_at, session.message_count));
            existing_paths.insert(
                session.source_id.clone(),
                SessionPath {
                    source_id: session.source_id.clone(),
                    directory: session.directory.clone(),
                    source_file_path: session.source_file_path.clone(),
                    repo_remote: session.repo_remote.clone(),
                    repo_slug: session.repo_slug.clone(),
                    repo_name: session.repo_name.clone(),
                },
            );
            if let Some(parser_version) = raw.usage_parser_version {
                existing_usage_meta.insert(
                    session.source_id.clone(),
                    UsageSessionStateMeta { parser_version, source_updated_at: session.updated_at },
                );
            }
            if let Some(parser_version) = event_parser_version {
                existing_event_meta.insert(
                    session.source_id.clone(),
                    EventSessionStateMeta { parser_version, source_updated_at: session.updated_at },
                );
            }
            total_messages += msg_count;
        }

        info!("{label} done");
    }

    let touched = new_sessions + updated_sessions + reprocessed_sessions;

    if options.verbose {
        println!();
        if options.force {
            print!(
                "Force sync: {new_sessions} new, {updated_sessions} updated, {reprocessed_sessions} reprocessed, {total_messages} messages"
            );
        } else {
            print!(
                "Sync: {new_sessions} new, {updated_sessions} updated, {skipped} unchanged, {total_messages} messages"
            );
        }
        if filtered_out > 0 {
            print!(", {filtered_out} outside configured time scope");
        }
        if excluded_out > 0 {
            print!(", {excluded_out} excluded by excluded_paths");
        }
        println!();
        println!(
            "Settings: sources [{}], time scope [{}]",
            labels
                .iter()
                .filter(|(id, _)| config.is_source_enabled(id))
                .map(|(_, label)| label.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            config.sync_window.label()
        );
        let progress = store.semantic_progress()?;
        if progress.total_sessions > 0 {
            println!(
                "Semantic queue: {}/{} done, {} pending, {} failed",
                progress.done_sessions,
                progress.total_sessions,
                progress.pending_sessions + progress.processing_sessions,
                progress.failed_sessions
            );
        }
    } else if !options.emit {
    } else if options.force {
        println!("Reprocessed {touched} sessions, {total_messages} messages");
    } else if touched == 0 {
        println!("Up to date.");
    } else {
        println!("{new_sessions} new, {updated_sessions} updated, {total_messages} messages");
    }

    Ok(())
}

fn decide_existing_session_action(
    usage_only: bool,
    backfill_events: bool,
    force: bool,
    content_changed: bool,
    usage_backfill_needed: bool,
    event_backfill_needed: bool,
) -> ExistingSessionAction {
    if usage_only {
        let needs_usage = usage_backfill_needed;
        let needs_events = backfill_events && event_backfill_needed;
        return if needs_usage || needs_events {
            ExistingSessionAction::BackfillOnly(BackfillPlan {
                usage: needs_usage,
                events: needs_events,
            })
        } else {
            ExistingSessionAction::Skip
        };
    }

    if !content_changed && !force {
        return if usage_backfill_needed || event_backfill_needed {
            ExistingSessionAction::BackfillOnly(BackfillPlan {
                usage: usage_backfill_needed,
                events: event_backfill_needed,
            })
        } else {
            ExistingSessionAction::Skip
        };
    }

    ExistingSessionAction::RefreshSession
}

fn raw_session_metadata_changed(
    raw: &adapters::RawSession,
    repo_identity: Option<&RepoIdentity>,
    old: &SessionPath,
) -> bool {
    let repo_changed = repo_identity.is_some_and(|repo| {
        old.repo_remote.as_deref() != Some(repo.remote.as_str())
            || old.repo_slug.as_deref() != Some(repo.slug.as_str())
            || old.repo_name.as_deref() != Some(repo.name.as_str())
    });
    raw.directory.as_deref().is_some_and(|directory| old.directory.as_deref() != Some(directory))
        || raw
            .source_file_path
            .as_deref()
            .is_some_and(|path| old.source_file_path.as_deref() != Some(path))
        || repo_changed
}

fn generate_title(messages: &[adapters::RawMessage]) -> String {
    let user_contents: Vec<&str> =
        messages.iter().filter(|m| m.role == Role::User).map(|m| m.content.as_str()).collect();
    utils::title_from_user_messages(&user_contents)
}

fn delete_excluded_sessions_for_source(
    store: &Store,
    source_id: &str,
    matcher: &globset::GlobSet,
    deleted: &mut HashSet<String>,
) -> Result<u32> {
    let mut count = 0;
    for path in store.session_paths_for_source(source_id)? {
        if paths_match_excluded(
            path.directory.as_deref(),
            path.source_file_path.as_deref(),
            matcher,
        ) {
            let source_id_to_delete = path.source_id;
            store.delete_session_data(source_id, &source_id_to_delete)?;
            if deleted.insert(source_id_to_delete) {
                count += 1;
            }
        }
    }
    Ok(count)
}

fn paths_match_excluded(
    directory: Option<&str>,
    source_file_path: Option<&str>,
    matcher: &globset::GlobSet,
) -> bool {
    directory.is_some_and(|path| matcher.is_match(path))
        || source_file_path.is_some_and(|path| path_or_ancestor_matches(path, matcher))
}

fn path_or_ancestor_matches(path: &str, matcher: &globset::GlobSet) -> bool {
    let path = std::path::Path::new(path);
    path.ancestors().any(|candidate| matcher.is_match(candidate))
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use crate::adapters::RawSession;
    use crate::db::{
        schema,
        store::{SessionPath, Store},
    };
    use crate::types::Session;

    use super::{
        BackfillPlan, ExistingSessionAction, decide_existing_session_action,
        delete_excluded_sessions_for_source, raw_session_metadata_changed,
    };

    fn matcher(pattern: &str) -> globset::GlobSet {
        let mut builder = globset::GlobSetBuilder::new();
        builder.add(globset::Glob::new(pattern).unwrap());
        builder.build().unwrap()
    }

    fn session(id: &str, source: &str, source_id: &str) -> Session {
        Session {
            id: id.to_string(),
            source: source.to_string(),
            source_id: source_id.to_string(),
            title: "t".to_string(),
            directory: None,
            repo_remote: None,
            repo_slug: None,
            repo_name: None,
            started_at: 0,
            updated_at: Some(1),
            message_count: 0,
            entrypoint: None,
            custom_title: None,
            summary: None,
            duration_minutes: None,
            source_file_path: None,
            is_import: false,
        }
    }

    #[test]
    fn usage_only_never_refreshes_existing_session() {
        assert_eq!(
            decide_existing_session_action(true, false, false, true, true, true),
            ExistingSessionAction::BackfillOnly(BackfillPlan { usage: true, events: false })
        );
        assert_eq!(
            decide_existing_session_action(true, false, false, true, false, true),
            ExistingSessionAction::Skip
        );
    }

    #[test]
    fn usage_only_can_backfill_events_without_refresh() {
        assert_eq!(
            decide_existing_session_action(true, true, false, true, false, true),
            ExistingSessionAction::BackfillOnly(BackfillPlan { usage: false, events: true })
        );
        assert_eq!(
            decide_existing_session_action(true, true, false, true, true, true),
            ExistingSessionAction::BackfillOnly(BackfillPlan { usage: true, events: true })
        );
    }

    #[test]
    fn full_sync_refreshes_changed_existing_session() {
        assert_eq!(
            decide_existing_session_action(false, false, false, true, true, true),
            ExistingSessionAction::RefreshSession
        );
    }

    #[test]
    fn full_sync_backfills_unchanged_existing_session_in_place() {
        assert_eq!(
            decide_existing_session_action(false, false, false, false, true, true),
            ExistingSessionAction::BackfillOnly(BackfillPlan { usage: true, events: true })
        );
        assert_eq!(
            decide_existing_session_action(false, false, false, false, false, false),
            ExistingSessionAction::Skip
        );
    }

    #[test]
    fn full_sync_treats_new_session_metadata_as_changed() {
        let raw = RawSession::search_only(
            "raw1",
            Some("/Users/x/git/samzong/Recall".to_string()),
            0,
            Some(1),
            None,
            vec![],
        );
        let missing = SessionPath {
            source_id: "raw1".to_string(),
            directory: None,
            source_file_path: None,
            repo_remote: None,
            repo_slug: None,
            repo_name: None,
        };
        let same = SessionPath {
            source_id: "raw1".to_string(),
            directory: Some("/Users/x/git/samzong/Recall".to_string()),
            source_file_path: None,
            repo_remote: Some("github.com/samzong/Recall".to_string()),
            repo_slug: None,
            repo_name: None,
        };
        assert!(raw_session_metadata_changed(&raw, None, &missing));
        assert!(!raw_session_metadata_changed(&raw, None, &same));

        let mut raw_with_path = RawSession::search_only("raw1", None, 0, Some(1), None, vec![]);
        raw_with_path.source_file_path = Some("/tmp/session.jsonl".to_string());
        assert!(raw_session_metadata_changed(&raw_with_path, None, &missing));
    }

    #[test]
    fn delete_excluded_sessions_for_source_uses_persisted_source_file_path() {
        schema::register_sqlite_vec();
        let matcher = matcher("**/observer-sessions");
        let store = Store::open_in_memory().unwrap();
        store.insert_session(&session("id-1", "claude-code", "s1")).unwrap();
        store
            .update_session_fields(
                "claude-code",
                "s1",
                None,
                None,
                None,
                Some("/tmp/observer-sessions/session.jsonl"),
            )
            .unwrap();

        let mut deleted = HashSet::new();
        let count =
            delete_excluded_sessions_for_source(&store, "claude-code", &matcher, &mut deleted)
                .unwrap();

        assert_eq!(count, 1);
        assert!(deleted.contains("s1"));
        assert!(store.session_paths_for_source("claude-code").unwrap().is_empty());
    }
}
