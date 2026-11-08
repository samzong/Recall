use super::*;

impl App {
    pub(crate) fn load_recent(&mut self, store: &Store) {
        let source_ids = self.source_filter_ids();
        let recent = store
            .list_recent_sessions_for_search_scope(
                source_ids.as_deref(),
                self.filters.active.time,
                &self.filters.active.scope,
                None,
                200,
            )
            .unwrap_or_default();
        self.results = recent
            .into_iter()
            .map(|session| SearchResult { session, match_source: MatchSource::Fts, snippet: None })
            .collect();
        self.selected_index = 0;
        self.result_scroll_offset = 0;
        self.panel_focus = PanelFocus::SessionList;
        self.search_pending = false;
        self.search_in_flight = false;
        self.load_preview(store);
    }

    pub(crate) fn try_search(&mut self, store: &Store, worker: &SearchWorker) {
        self.refresh_semantic_progress(store);
        if !self.search_pending {
            return;
        }
        if self.last_keystroke.elapsed() < Duration::from_millis(SEARCH_DEBOUNCE_MS) {
            return;
        }
        self.search_pending = false;
        let query = self.query.trim().to_string();

        let request_id = self.next_search_request_id();
        let request = SearchRequest {
            id: request_id,
            query,
            filters: self.search_filters(),
            semantic_ready: self.semantic_ready(),
        };

        if worker.search(request) {
            self.active_search_id = request_id;
            self.search_in_flight = true;
            self.search_feedback = Some("Searching...".to_string());
        } else {
            self.search_in_flight = false;
            self.search_feedback = None;
            self.status_message = Some("Search worker unavailable".to_string());
        }
    }

    pub(crate) fn apply_search_response(&mut self, store: &Store, response: SearchResponse) {
        if response.id != self.active_search_id || response.query != self.query.trim() {
            return;
        }

        match response.result {
            Ok(mut results) => {
                self.apply_sort(&mut results);
                self.results = results;
                self.selected_index = 0;
                self.result_scroll_offset = 0;
                self.panel_focus = PanelFocus::SessionList;
                self.load_preview(store);

                if response.phase == SearchPhase::Text
                    && !response.query.is_empty()
                    && self.semantic_ready()
                {
                    self.search_in_flight = true;
                    self.search_feedback = Some("Refining semantic results...".to_string());
                } else {
                    self.search_in_flight = false;
                    self.search_feedback = None;
                }
            }
            Err(error) => {
                if response.phase == SearchPhase::Text {
                    self.results.clear();
                    self.preview_messages.clear();
                } else {
                    self.embedding_unavailable = true;
                }
                self.search_in_flight = false;
                self.search_feedback = None;
                self.status_message = Some(error);
            }
        }
    }

    fn next_search_request_id(&mut self) -> u64 {
        self.search_request_id = self.search_request_id.saturating_add(1);
        self.search_request_id
    }

    pub(super) fn invalidate_active_search(&mut self) {
        self.active_search_id = self.next_search_request_id();
        self.search_in_flight = false;
    }

    fn search_filters(&self) -> SearchFilters {
        SearchFilters {
            scope: self.filters.active.scope.clone(),
            sources: self.source_filter_ids(),
            time_range: self.filters.active.time,
            thread_role: None,
            excluded_session_id: None,
        }
    }

    pub(super) fn queue_search(&mut self) {
        self.queue_search_with_feedback("Search queued...");
    }

    pub(super) fn queue_search_now(&mut self) {
        let now = Instant::now();
        self.last_keystroke =
            now.checked_sub(Duration::from_millis(SEARCH_DEBOUNCE_MS)).unwrap_or(now);
        self.search_pending = true;
        self.search_feedback = Some("Search queued...".to_string());
        self.panel_focus = PanelFocus::SessionList;
    }

    pub(super) fn queue_search_with_feedback(&mut self, feedback: &str) {
        self.last_keystroke = Instant::now();
        self.search_pending = true;
        self.search_feedback = Some(feedback.to_string());
    }

    pub(super) fn semantic_ready(&self) -> bool {
        self.config.semantic_enabled
            && !self.embedding_unavailable
            && (self.semantic_progress.done_sessions > 0
                || self.semantic_progress.processing_sessions > 0)
    }

    fn refresh_semantic_progress(&mut self, store: &Store) {
        if self.semantic_last_refresh.elapsed().as_millis() < 750 {
            return;
        }
        self.update_scope_metrics(store);
        self.semantic_last_refresh = Instant::now();
    }

    pub(super) fn update_scope_metrics(&mut self, store: &Store) {
        let scope = self.filters.active.scope.clone();
        if let Ok((sessions, messages)) = store.stats_for_search_scope(
            self.source_filter_ids().as_deref(),
            self.filters.active.time,
            &scope,
        ) {
            self.total_sessions = sessions;
            self.total_messages = messages;
        }
        if let Ok(progress) = store.semantic_progress_for_search_scope(
            self.source_filter_ids().as_deref(),
            self.filters.active.time,
            &scope,
        ) {
            self.semantic_progress = progress;
        }
    }

    pub(super) fn apply_sort(&self, results: &mut [SearchResult]) {
        if self.effective_sort_order() == SortOrder::Newest {
            results.sort_by_key(|b| {
                std::cmp::Reverse((
                    b.session.updated_at.unwrap_or(b.session.started_at),
                    b.session.started_at,
                ))
            });
        }
    }
}
