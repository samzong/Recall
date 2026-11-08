use super::*;

impl App {
    pub(super) fn handle_settings_key(&mut self, key: KeyEvent, store: &Store) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = AppMode::Search;
            }
            KeyCode::Up => self.handle_scroll_up(store),
            KeyCode::Down => self.handle_scroll_down(store),
            KeyCode::Left | KeyCode::Right | KeyCode::Enter | KeyCode::Char(' ') => {
                self.update_setting(store);
            }
            _ => {}
        }
    }

    pub(super) fn open_skill_sessions(&mut self, store: &Store) {
        let Some(entry) = self.usage.selected_skill() else {
            return;
        };
        let skill_id = entry.id.clone();
        let session_ids = entry.session_ids.clone();
        if session_ids.is_empty() {
            self.status_message =
                Some(format!("No indexed sessions for skill '{skill_id}' in this range"));
            return;
        }
        match store.list_sessions_by_ids(&session_ids) {
            Ok(sessions) if sessions.is_empty() => {
                self.status_message =
                    Some(format!("Sessions for '{skill_id}' are no longer in index"));
            }
            Ok(sessions) => {
                let count = sessions.len();
                self.results = sessions
                    .into_iter()
                    .map(|session| SearchResult {
                        session,
                        match_source: MatchSource::Fts,
                        snippet: None,
                    })
                    .collect();
                self.selected_index = 0;
                self.result_scroll_offset = 0;
                self.panel_focus = PanelFocus::SessionList;
                self.mode = AppMode::Search;
                self.query = format!("skill:{skill_id}");
                self.cursor_pos = self.query.len();
                self.load_preview(store);
                self.status_message = Some(format!("{count} sessions used skill '{skill_id}'"));
            }
            Err(err) => {
                self.status_message = Some(format!("Failed to load sessions: {err}"));
            }
        }
    }

    pub(super) fn cycle_usage_source(&mut self) {
        let enabled: Vec<String> =
            self.enabled_sources().into_iter().map(|(id, _)| id.clone()).collect();
        let current = self.normalized_source_selection(&self.filters.active.sources);
        let next_index = if current.len() == 1 {
            enabled.iter().position(|id| id == &current[0]).map_or(0, |index| index + 1)
        } else {
            0
        };
        self.filters.active.sources = enabled.get(next_index).cloned().into_iter().collect();
    }

    pub(super) fn settings_row_count(&self) -> usize {
        1 + self.all_sources.len()
    }

    fn update_setting(&mut self, store: &Store) {
        // Re-read the file so saving preserves keys edited outside this TUI
        // (e.g. `recall semantic disable` run while it was open).
        let mut config = AppConfig::load().unwrap_or_else(|_| self.config.clone());
        if self.settings_selected == 0 {
            config.sync_window = config.sync_window.next();
        } else if let Some((source_id, _)) = self.all_sources.get(self.settings_selected - 1) {
            if config.is_source_enabled(source_id) {
                let enabled_count =
                    self.all_sources.iter().filter(|(id, _)| config.is_source_enabled(id)).count();
                if enabled_count <= 1 {
                    self.status_message = Some("At least one source must stay enabled".to_string());
                    return;
                }
                config.disabled_sources.push(source_id.clone());
                config.disabled_sources.sort();
                config.disabled_sources.dedup();
            } else {
                config.disabled_sources.retain(|id| id != source_id);
            }
        }

        if let Err(e) = config.save() {
            self.status_message = Some(format!("Failed to save settings: {e}"));
            return;
        }
        self.config = config;

        self.reset_search_defaults();
        self.update_scope_metrics(store);
        self.status_message = Some("Settings saved".to_string());
        if self.query.is_empty() {
            self.load_recent(store);
        } else {
            self.queue_search_now();
        }
    }
}
