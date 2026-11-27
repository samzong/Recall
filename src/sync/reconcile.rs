use anyhow::Result;

use super::{ExistingState, SyncJob, path_matches_scope};
use crate::adapters;
use crate::project_scope::ProjectScope;
use crate::repo_identity::RepoIdentity;

impl SyncJob {
    pub(super) fn apply_source_observations(
        &mut self,
        source_id: &str,
        observations: Vec<adapters::SourceObservation>,
        existing: &mut ExistingState,
    ) -> Result<()> {
        for observation in observations {
            let Some(stored) = existing.paths.get_mut(&observation.source_id) else {
                continue;
            };
            if !path_matches_scope(&self.options.scope, stored, &mut self.repo_cache)? {
                continue;
            }
            if let Some(source_file_path) = observation.source_file_path.as_deref()
                && stored.source_file_path.as_deref() != Some(source_file_path)
            {
                self.store.update_session_fields(
                    source_id,
                    &observation.source_id,
                    None,
                    None,
                    None,
                    Some(source_file_path),
                )?;
                stored.source_file_path = Some(source_file_path.to_string());
            }
            if existing.imported_ids.remove(&observation.source_id) {
                self.store.clear_import_marker(source_id, &observation.source_id)?;
            }
            if let Some(host) = &self.host {
                host.observe(&self.store.conn, source_id, &observation.source_id)?;
            }
        }
        Ok(())
    }

    pub(super) fn reconcile_source(
        &mut self,
        source_id: &str,
        label: &str,
        reconcile: Option<adapters::ReconcilePlan>,
        existing: &mut ExistingState,
    ) -> Result<()> {
        if !matches!(self.options.scope, ProjectScope::Global) {
            return Ok(());
        }
        let Some(reconcile) = reconcile else {
            return Ok(());
        };
        match reconcile {
            adapters::ReconcilePlan::PartialInventory(issues) => {
                self.report_incomplete_inventory(label, "partial", &issues);
            }
            adapters::ReconcilePlan::UnavailableInventory(issues) => {
                self.report_incomplete_inventory(label, "unavailable", &issues);
            }
            adapters::ReconcilePlan::CompleteLiveSet(live) => {
                let stale = existing
                    .meta
                    .keys()
                    .filter(|id| !live.contains(*id))
                    .cloned()
                    .collect::<Vec<_>>();
                for source_id_to_delete in stale {
                    self.store.delete_session_data(source_id, &source_id_to_delete)?;
                    existing.remove(&source_id_to_delete);
                }
            }
            adapters::ReconcilePlan::ExactTombstones(source_ids) => {
                for source_id_to_delete in source_ids {
                    if existing.remove(&source_id_to_delete) {
                        self.store.delete_session_data(source_id, &source_id_to_delete)?;
                    }
                }
            }
        }
        Ok(())
    }

    pub(super) fn report_incomplete_inventory(
        &self,
        label: &str,
        state: &str,
        issues: &[adapters::InventoryIssue],
    ) {
        if !self.options.emit {
            return;
        }
        if let Some(issue) = issues.first() {
            eprintln!(
                "Reconciliation skipped for {label}: inventory {state} at {} ({:?}; {} issue(s)).",
                issue.path.display(),
                issue.category,
                issues.len()
            );
        } else {
            eprintln!("Reconciliation skipped for {label}: inventory {state}.");
        }
    }

    pub(super) fn load_adapter_sync_context<'a>(
        &self,
        source_id: &str,
    ) -> Result<adapters::AdapterSyncContext<'a>> {
        let context = adapters::AdapterSyncContext::new(
            source_id.to_string(),
            self.store.session_meta_map(source_id)?,
            self.store
                .session_paths_for_source(source_id)?
                .into_iter()
                .map(|path| (path.source_id.clone(), path))
                .collect(),
            self.store.imported_source_ids(source_id)?,
            self.store.usage_state_meta_map(source_id)?,
            self.store.event_state_meta_map(source_id)?,
            self.store.metadata_state_meta_map(source_id)?,
        )
        .with_usage_only(self.options.usage_only);
        Ok(match self.options.target_session.as_deref() {
            Some(target) => context.restricted_to(target),
            None => context,
        })
    }

    pub(super) fn prepare_existing_state(
        &mut self,
        source_id: &str,
        context: adapters::AdapterSyncContext,
    ) -> Result<ExistingState> {
        let (existing, repo_backfill) = self.resolve_existing_state_parts(context.into_parts())?;
        self.write_repo_backfill(source_id, repo_backfill, &existing)?;
        Ok(existing)
    }

    pub(super) fn write_repo_backfill(
        &self,
        source_id: &str,
        repo_backfill: Vec<(String, RepoIdentity)>,
        existing: &ExistingState,
    ) -> Result<()> {
        for (session_id, repo) in repo_backfill {
            let unchanged = existing.paths.get(&session_id).is_some_and(|path| {
                path.repo_remote.as_deref() == Some(repo.remote.as_str())
                    && path.repo_slug.as_deref() == Some(repo.slug.as_str())
                    && path.repo_name.as_deref() == Some(repo.name.as_str())
            });
            if unchanged {
                self.store.update_session_repo_identity(source_id, &session_id, &repo)?;
            }
        }
        Ok(())
    }

    pub(super) fn resolve_existing_state_parts(
        &mut self,
        parts: adapters::AdapterSyncContextParts,
    ) -> Result<(ExistingState, Vec<(String, RepoIdentity)>)> {
        let adapters::AdapterSyncContextParts {
            session_meta: meta,
            session_paths: mut paths,
            imported_ids,
            usage_state: usage_meta,
            event_state: event_meta,
            metadata_state: metadata_meta,
        } = parts;
        let backfill_identity = !self.options.usage_only
            && self.event_backfill.is_none()
            && matches!(self.options.scope, ProjectScope::Global);
        let mut repo_backfill = Vec::new();
        for path in paths.values_mut() {
            if backfill_identity
                && path.directory.is_some()
                && (path.repo_remote.is_none()
                    || path.repo_slug.is_none()
                    || path.repo_name.is_none())
                && let Some(repo) = self.repo_cache.resolve(path.directory.as_deref())?
            {
                path.repo_remote = Some(repo.remote.clone());
                path.repo_slug = Some(repo.slug.clone());
                path.repo_name = Some(repo.name.clone());
                repo_backfill.push((path.source_id.clone(), repo));
            }
        }
        let existing =
            ExistingState { meta, paths, imported_ids, usage_meta, event_meta, metadata_meta };
        Ok((existing, repo_backfill))
    }
}
