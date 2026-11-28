use std::collections::HashSet;

use anyhow::Result;

#[cfg(any(test, feature = "bench"))]
use super::SyncRunOptions;
use super::{BackfillPlan, ExistingSessionAction, ExistingState, SyncJob, paths_match_excluded};
use crate::adapters;
#[cfg(any(test, feature = "bench"))]
use crate::config::AppConfig;
#[cfg(any(test, feature = "bench"))]
use crate::db::store::Store;
use crate::db::store::{ParserStateMeta, SessionPath, SessionTopologyWrite};
#[cfg(any(test, feature = "bench"))]
use crate::project_scope::ProjectScope;
use crate::project_scope::SessionScopeFields;
use crate::repo_identity::RepoIdentity;
use crate::types::{Message, Role, Session};
use crate::utils;

impl SyncJob {
    pub(super) fn process_raw_session(
        &mut self,
        source_id: &str,
        mut raw: adapters::RawSession,
        existing: &mut ExistingState,
        purged_excluded_ids: &mut HashSet<String>,
    ) -> Result<()> {
        let raw_source_id = raw.source_id.clone();
        if self.options.target_session.as_deref().is_some_and(|target| target != raw_source_id) {
            return Ok(());
        }

        let repo_identity = self.repo_cache.resolve(raw.directory.as_deref())?;
        if !self
            .options
            .scope
            .matches(SessionScopeFields::new(raw.directory.as_deref(), repo_identity.as_ref()))
        {
            self.stats.out_of_scope += 1;
            return Ok(());
        }

        if let Some(matcher) = &self.path_excluder
            && paths_match_excluded(
                raw.directory.as_deref(),
                raw.source_file_path.as_deref(),
                matcher,
            )
        {
            if self.event_backfill.is_none() && existing.remove(&raw_source_id) {
                self.store.delete_session_data(source_id, &raw_source_id)?;
            }
            if purged_excluded_ids.insert(raw_source_id) {
                self.stats.excluded_out += 1;
            }
            return Ok(());
        }

        if self.event_backfill.is_none()
            && let Some(source_file_path) = raw.source_file_path.as_deref()
            && let Some(stored) = existing.paths.get_mut(&raw_source_id)
            && stored.source_file_path.as_deref() != Some(source_file_path)
        {
            self.store.update_session_fields(
                source_id,
                &raw_source_id,
                None,
                None,
                None,
                Some(source_file_path),
            )?;
            stored.source_file_path = Some(source_file_path.to_string());
        }

        if self.options.usage_only
            && self.event_backfill.is_none()
            && let Some(directory) = raw.directory.as_deref()
            && let Some(stored) = existing.paths.get_mut(&raw_source_id)
        {
            let directory_changed = stored.directory.as_deref() != Some(directory);
            let (repo_remote, repo_slug, repo_name) = match repo_identity.as_ref() {
                Some(repo) => {
                    (Some(repo.remote.as_str()), Some(repo.slug.as_str()), Some(repo.name.as_str()))
                }
                None if !directory_changed => (
                    stored.repo_remote.as_deref(),
                    stored.repo_slug.as_deref(),
                    stored.repo_name.as_deref(),
                ),
                None => (None, None, None),
            };
            let repo_changed = stored.repo_remote.as_deref() != repo_remote
                || stored.repo_slug.as_deref() != repo_slug
                || stored.repo_name.as_deref() != repo_name;
            if directory_changed || repo_changed {
                self.store.update_session_directory_and_repo(
                    source_id,
                    &raw_source_id,
                    directory,
                    repo_remote,
                    repo_slug,
                    repo_name,
                )?;
                stored.directory = Some(directory.to_string());
                stored.repo_remote = repo_remote.map(str::to_string);
                stored.repo_slug = repo_slug.map(str::to_string);
                stored.repo_name = repo_name.map(str::to_string);
                if let Some(meta) = existing.meta.get_mut(&raw_source_id) {
                    meta.directory = Some(directory.to_string());
                }
            }
        }

        if let Some(cutoff) = self.since_ts {
            let ts = raw.updated_at.unwrap_or(raw.started_at);
            if ts < cutoff {
                self.stats.filtered_out += 1;
                return Ok(());
            }
        }

        for event in &mut raw.events {
            for file in &mut event.files {
                file.target = self.repo_cache.resolve_file(&file.path, file.cwd.as_deref())?;
            }
        }

        if let Some(report) = &mut self.event_backfill {
            let Some(version) = raw.event_parser_version else {
                report.unsupported_sessions += 1;
                return Ok(());
            };
            if raw.events.is_empty() {
                report.no_events += 1;
                return Ok(());
            }
            if existing.meta.contains_key(&raw_source_id) {
                if !self.options.force
                    && crate::adapters::sync_state::parser_state_is_current(
                        version,
                        existing.event_meta.get(&raw_source_id).copied(),
                        raw.updated_at,
                    )
                {
                    self.stats.skipped += 1;
                    return Ok(());
                }
                if report.dry_run {
                    self.stats.reprocessed_sessions += 1;
                    return Ok(());
                }
                return self.apply_backfill(
                    source_id,
                    &raw_source_id,
                    &mut raw,
                    BackfillPlan { usage: false, events: true, metadata: false },
                    false,
                    existing,
                );
            }
            if report.dry_run {
                self.stats.new_sessions += 1;
                return Ok(());
            }
        }

        let existing_repo_fields = existing.paths.get(&raw_source_id).filter(|old| {
            repo_identity.is_none() && old.directory.as_deref() == raw.directory.as_deref()
        });
        let (repo_remote, repo_slug, repo_name) = match repo_identity.as_ref() {
            Some(repo) => {
                (Some(repo.remote.clone()), Some(repo.slug.clone()), Some(repo.name.clone()))
            }
            None => existing_repo_fields
                .map(|old| (old.repo_remote.clone(), old.repo_slug.clone(), old.repo_name.clone()))
                .unwrap_or((None, None, None)),
        };
        let msg_count = raw.messages.len() as u32;
        let needs_backfill = |version: Option<u32>, state: Option<&ParserStateMeta>| {
            version.is_some_and(|version| {
                !crate::adapters::sync_state::parser_state_is_current(
                    version,
                    state.copied(),
                    raw.updated_at,
                )
            })
        };
        let usage_backfill_needed =
            needs_backfill(raw.usage_parser_version, existing.usage_meta.get(&raw_source_id));
        let event_backfill_needed = (self.options.backfill_events || !self.options.usage_only)
            && needs_backfill(raw.event_parser_version, existing.event_meta.get(&raw_source_id));
        let metadata_parser_version = raw.metadata_parser_version;
        let metadata_backfill_needed = !self.options.usage_only
            && needs_backfill(metadata_parser_version, existing.metadata_meta.get(&raw_source_id));

        let session_uuid = match existing.meta.get(&raw_source_id).cloned() {
            Some(old) => {
                let was_imported = existing.imported_ids.remove(&raw_source_id);
                let metadata_changed = existing.paths.get(&raw_source_id).is_some_and(|old| {
                    raw_session_metadata_changed(&raw, repo_identity.as_ref(), old)
                });
                let content_changed = raw.refresh_session_metadata
                    || old.message_count != msg_count
                    || metadata_changed
                    || (raw.updated_at.is_some() && raw.updated_at != old.updated_at)
                    || (raw.refresh_session_on_metadata_backfill && metadata_backfill_needed);
                match decide_existing_session_action(
                    self.options.usage_only,
                    self.options.backfill_events,
                    self.options.force,
                    content_changed,
                    usage_backfill_needed,
                    event_backfill_needed,
                    metadata_backfill_needed,
                ) {
                    ExistingSessionAction::Skip => {
                        if was_imported {
                            self.store.clear_import_marker(source_id, &raw_source_id)?;
                        }
                        if let Some(host) = &self.host {
                            host.observe(&self.store.conn, source_id, &raw_source_id)?;
                        }
                        self.stats.skipped += 1;
                        return Ok(());
                    }
                    ExistingSessionAction::BackfillOnly(plan) => {
                        self.apply_backfill(
                            source_id,
                            &raw_source_id,
                            &mut raw,
                            plan,
                            was_imported,
                            existing,
                        )?;
                        if let Some(host) = &self.host {
                            host.observe(&self.store.conn, source_id, &raw_source_id)?;
                        }
                        return Ok(());
                    }
                    ExistingSessionAction::RefreshSession => {}
                }
                existing.usage_meta.remove(&raw_source_id);
                existing.event_meta.remove(&raw_source_id);
                existing.metadata_meta.remove(&raw_source_id);
                if content_changed {
                    self.stats.updated_sessions += 1;
                } else {
                    self.stats.reprocessed_sessions += 1;
                }
                old.id
            }
            None => {
                self.stats.new_sessions += 1;
                uuid::Uuid::new_v4().to_string()
            }
        };

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
            locations: Vec::new(),
            alternative_versions: 0,
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

        let persist_events = !self.options.usage_only || self.options.backfill_events;
        let (events, event_parser_version) = if persist_events {
            (raw.events, raw.event_parser_version)
        } else {
            (Vec::new(), None)
        };

        let topology = SessionTopologyWrite {
            thread_role: raw.thread_role,
            parents: &raw.parent_links,
            parser_version: metadata_parser_version,
        };
        self.store.replace_session_with_usage_events_and_codex_exclusions(
            source_id,
            &raw_source_id,
            &session,
            &messages,
            &raw.usage_events,
            &raw.excluded_codex_usage,
            raw.usage_parser_version,
            &events,
            event_parser_version,
            &topology,
        )?;
        if let Some(host) = &self.host {
            host.observe(&self.store.conn, source_id, &raw_source_id)?;
        }
        existing.record_replaced(
            &session,
            raw.usage_parser_version,
            event_parser_version,
            metadata_parser_version,
        );
        self.stats.total_messages += msg_count;
        Ok(())
    }
}

#[cfg(any(test, feature = "bench"))]
pub(crate) fn persist_raw_session_for_conformance(
    store: Store,
    source: &str,
    raw: adapters::RawSession,
) -> Result<Store> {
    let mut job = SyncJob::new(
        SyncRunOptions {
            force: false,
            verbose: false,
            emit: false,
            usage_only: false,
            backfill_events: false,
            sources: None,
            scope: ProjectScope::Global,
            target_session: None,
        },
        store,
        AppConfig::default(),
        &[],
    )?;
    let context = job.load_adapter_sync_context(source)?;
    let mut existing = job.prepare_existing_state(source, context)?;
    job.process_raw_session(source, raw, &mut existing, &mut HashSet::new())?;
    Ok(job.store)
}

pub(super) fn decide_existing_session_action(
    usage_only: bool,
    backfill_events: bool,
    force: bool,
    content_changed: bool,
    usage_backfill_needed: bool,
    event_backfill_needed: bool,
    metadata_backfill_needed: bool,
) -> ExistingSessionAction {
    if !usage_only && (content_changed || force) {
        return ExistingSessionAction::RefreshSession;
    }
    let plan = BackfillPlan {
        usage: usage_backfill_needed,
        events: event_backfill_needed && (!usage_only || backfill_events),
        metadata: metadata_backfill_needed && !usage_only,
    };
    if plan.usage || plan.events || plan.metadata {
        ExistingSessionAction::BackfillOnly(plan)
    } else {
        ExistingSessionAction::Skip
    }
}

pub(super) fn raw_session_metadata_changed(
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

pub(super) fn generate_title(messages: &[adapters::RawMessage]) -> String {
    let user_contents: Vec<&str> =
        messages.iter().filter(|m| m.role == Role::User).map(|m| m.content.as_str()).collect();
    utils::title_from_user_messages(&user_contents)
}
