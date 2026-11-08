use super::*;

impl App {
    pub(crate) fn handle_key(&mut self, key: KeyEvent, store: &Store) {
        self.status_message = None;

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }

        match self.mode {
            AppMode::Search => self.handle_search_key(key, store),
            AppMode::Usage => self.handle_usage_key(key, store),
            AppMode::Viewing => {
                let before = self.viewing.selected_msg;
                self.handle_viewing_key(key, store);
                if matches!(self.mode, AppMode::Viewing) && self.viewing.selected_msg != before {
                    self.anchor_viewing_scroll();
                }
            }
            AppMode::ShareResult => self.handle_share_result_key(key),
            AppMode::ExportInput => self.handle_export_key(key),
            AppMode::Settings => self.handle_settings_key(key, store),
            AppMode::Filters => self.handle_filters_key(key, store),
            AppMode::HandoffTarget => self.handle_handoff_target_key(key),
            AppMode::Subagents => self.handle_subagents_key(key, store),
            AppMode::ConfirmResume => self.handle_confirm_resume_key(key, store),
        }
    }

    pub(crate) fn set_terminal_size(&mut self, width: u16, height: u16) {
        self.terminal_area = Rect::new(0, 0, width, height);
    }

    pub(super) fn handle_search_key(&mut self, key: KeyEvent, store: &Store) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('f') {
            self.open_filters();
            return;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            self.sync_requested = true;
            return;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('p') {
            self.mode = AppMode::Settings;
            self.settings_selected = 0;
            return;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('r') {
            self.start_resume_confirmation(ResumeOrigin::Search, store);
            return;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('o') {
            self.start_app_open_confirmation(ResumeOrigin::Search, store);
            return;
        }

        match key.code {
            KeyCode::Char('q')
                if self.query.is_empty() && self.panel_focus == PanelFocus::SessionList =>
            {
                self.should_quit = true;
            }
            KeyCode::Esc => {
                if self.panel_focus == PanelFocus::Preview {
                    self.panel_focus = PanelFocus::SessionList;
                } else if !self.query.is_empty() {
                    self.query.clear();
                    self.cursor_pos = 0;
                    self.queue_search();
                } else {
                    self.should_quit = true;
                }
            }
            KeyCode::Char(c) if self.panel_focus == PanelFocus::SessionList => {
                self.query.insert(self.cursor_pos, c);
                self.cursor_pos += c.len_utf8();
                self.queue_search();
            }
            KeyCode::Backspace
                if self.panel_focus == PanelFocus::SessionList && self.cursor_pos > 0 =>
            {
                let prev = cursor_prev(&self.query, self.cursor_pos);
                self.query.replace_range(prev..self.cursor_pos, "");
                self.cursor_pos = prev;
                self.queue_search();
            }
            KeyCode::Left => {
                if self.panel_focus == PanelFocus::Preview {
                    self.panel_focus = PanelFocus::SessionList;
                } else if self.cursor_pos > 0 {
                    self.cursor_pos = cursor_prev(&self.query, self.cursor_pos);
                }
            }
            KeyCode::Right if self.panel_focus == PanelFocus::SessionList => {
                if self.cursor_pos < self.query.len() {
                    self.cursor_pos = cursor_next(&self.query, self.cursor_pos);
                } else if !self.preview_messages.is_empty() {
                    self.panel_focus = PanelFocus::Preview;
                    self.preview_selected_msg = 0;
                    self.preview_scroll_offset = 0;
                }
            }
            KeyCode::Up => {
                self.handle_scroll_up(store);
            }
            KeyCode::Down => {
                self.handle_scroll_down(store);
            }
            KeyCode::PageUp => match self.panel_focus {
                PanelFocus::SessionList => self.page_result_list(true, store),
                PanelFocus::Preview => self.page_preview_selection(true),
            },
            KeyCode::PageDown => match self.panel_focus {
                PanelFocus::SessionList => self.page_result_list(false, store),
                PanelFocus::Preview => self.page_preview_selection(false),
            },
            KeyCode::Enter if !self.results.is_empty() => {
                self.enter_viewing(store);
            }
            KeyCode::Tab => {
                self.open_filters();
            }
            _ => {}
        }
    }

    pub(super) fn handle_usage_key(&mut self, key: KeyEvent, store: &Store) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                self.should_quit = true;
            }
            KeyCode::Char('m') | KeyCode::Char('M') => {
                self.usage.cycle_tab();
            }
            KeyCode::Char('t') | KeyCode::Char('T') => {
                self.usage.cycle_time();
            }
            KeyCode::Char('s') | KeyCode::Char('S')
                if !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.cycle_usage_source();
                self.usage.request_refresh();
            }
            KeyCode::Char('r') | KeyCode::Char('R') => {
                self.filters.active.sources.clear();
                self.usage.reset();
            }
            KeyCode::Enter if self.usage.tab == UsageTab::Skills && !self.usage.is_loading() => {
                self.open_skill_sessions(store);
            }
            KeyCode::Up | KeyCode::Char('k') => self.handle_scroll_up(store),
            KeyCode::Down | KeyCode::Char('j') => self.handle_scroll_down(store),
            _ => {}
        }
    }

    pub(super) fn handle_viewing_key(&mut self, key: KeyEvent, store: &Store) {
        if self.viewing.search_input.is_some() {
            self.handle_viewing_search_input(key);
            return;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('r') {
            self.start_resume_confirmation(ResumeOrigin::Viewing, store);
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('o') {
            self.start_app_open_confirmation(ResumeOrigin::Viewing, store);
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            self.sync_requested = true;
            return;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.back_out_of_viewing(store);
            }
            KeyCode::Char('a') => {
                self.open_subagents_picker();
            }
            KeyCode::Up | KeyCode::Char('k') if self.viewing.selected_msg > 0 => {
                self.viewing.selected_msg -= 1;
            }
            KeyCode::Down | KeyCode::Char('j')
                if self.viewing.selected_msg + 1 < self.viewing.messages.len() =>
            {
                self.viewing.selected_msg += 1;
            }
            KeyCode::PageUp => {
                self.page_viewing_selection(true);
            }
            KeyCode::PageDown => {
                self.page_viewing_selection(false);
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.viewing.selected_msg = 0;
            }
            KeyCode::End | KeyCode::Char('G') if !self.viewing.messages.is_empty() => {
                self.viewing.selected_msg = self.viewing.messages.len() - 1;
            }
            KeyCode::Char('c') | KeyCode::Char('C') => {
                self.copy_current_message();
            }
            KeyCode::Char('e') => {
                self.start_export();
            }
            KeyCode::Char('s') => {
                self.share_current_session();
            }
            KeyCode::Char('v') => {
                self.preview_current_session();
            }
            KeyCode::Char('h') => {
                self.open_handoff_target_picker();
            }
            KeyCode::Char('/') => {
                self.viewing.search_input = Some(String::new());
                self.viewing.search_cursor = 0;
                self.viewing.search_status = None;
            }
            KeyCode::Char('n') => {
                self.viewing.jump_match(true);
            }
            KeyCode::Char('N') => {
                self.viewing.jump_match(false);
            }
            _ => {}
        }
    }

    fn handle_share_result_key(&mut self, key: KeyEvent) {
        if self.share_publish_rx.is_some() {
            return;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
                self.share_popup = None;
                self.mode = AppMode::Viewing;
            }
            KeyCode::Char('o') | KeyCode::Char('O') => {
                if let Some(url) = self.share_popup.as_ref().and_then(|popup| popup.url.clone()) {
                    match crate::utils::open_url_in_default_browser(&url) {
                        Ok(()) => {
                            if let Some(popup) = self.share_popup.as_mut() {
                                popup.message = "Opened share URL".to_string();
                            }
                        }
                        Err(error) => {
                            self.status_message = Some(format!("Open failed: {error}"));
                        }
                    }
                }
            }
            KeyCode::Char('c') | KeyCode::Char('C') => {
                if let Some(url) = self.share_popup.as_ref().and_then(|popup| popup.url.clone()) {
                    self.copy_to_clipboard(&url);
                    if let Some(popup) = self.share_popup.as_mut() {
                        popup.message = "Copied share URL".to_string();
                    }
                }
            }
            _ => {}
        }
    }

    fn handle_viewing_search_input(&mut self, key: KeyEvent) {
        let Some(input) = self.viewing.search_input.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => {
                self.viewing.search_input = None;
                self.viewing.search_cursor = 0;
            }
            KeyCode::Enter => {
                let query = input.clone();
                self.viewing.search_input = None;
                self.viewing.search_cursor = 0;
                if query.is_empty() {
                    self.viewing.search_query.clear();
                    self.viewing.search_status = None;
                    self.viewing.matches.clear();
                    return;
                }
                self.viewing.search_query = query;
                self.viewing.recompute_matches();
                if self.viewing.matches.is_empty() {
                    self.viewing.search_status = Some("No match".to_string());
                    return;
                }
                self.viewing.search_status = None;
                self.viewing.jump_match(true);
            }
            _ => {
                edit_text(input, &mut self.viewing.search_cursor, key.code);
            }
        }
    }
}
