use super::*;

impl App {
    pub(crate) fn take_usage_request(&mut self, sync: bool) -> Option<UsageRequest> {
        let (id, time_range) = self.usage.take_refresh()?;
        Some(UsageRequest { id, sources: self.source_filter_ids(), time_range, sync })
    }

    pub(crate) fn take_sync_request(&mut self) -> Option<SyncRequest> {
        if !self.sync_requested || self.sync_in_flight {
            return None;
        }

        self.sync_requested = false;
        self.sync_in_flight = true;
        Some(SyncRequest {
            sources: self.source_filter_ids(),
            scope: self.filters.active.scope.clone(),
        })
    }

    pub(crate) fn fail_sync(&mut self, error: impl std::fmt::Display) {
        self.sync_requested = false;
        self.sync_in_flight = false;
        self.status_message = Some(format!("Sync failed: {error}"));
    }

    pub(crate) fn apply_sync_response(&mut self, store: &Store, result: Result<(), String>) {
        if !self.sync_in_flight {
            return;
        }

        self.sync_in_flight = false;
        match result {
            Ok(()) => {
                self.reload_after_sync(store);
                if !self.sync_requested {
                    self.status_message = Some("Synced".to_string());
                }
            }
            Err(error) => {
                if !self.sync_requested {
                    self.status_message = Some(format!("Sync failed: {error}"));
                }
            }
        }
    }

    fn reload_after_sync(&mut self, store: &Store) {
        self.project_directories = store.list_project_directories().unwrap_or_default();
        self.update_scope_metrics(store);
        if matches!(self.mode, AppMode::Viewing) && self.viewing.search_input.is_none() {
            let selected = self.viewing.selected_msg;
            if let Some(session) = self.viewing.session.clone() {
                let session =
                    store.get_session_by_id(&session.id).ok().flatten().unwrap_or(session);
                if self.viewing.load(session, store) && !self.viewing.messages.is_empty() {
                    self.viewing.selected_msg = selected.min(self.viewing.messages.len() - 1);
                    self.anchor_viewing_scroll();
                }
            }
        }
        if self.query.trim().is_empty() {
            self.load_recent(store);
        } else {
            self.queue_search_now();
        }
    }

    pub(crate) fn apply_usage_response(&mut self, response: UsageResponse) {
        self.usage.apply_response(response, self.source_filter_ids());
    }
}
