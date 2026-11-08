use super::*;

impl App {
    pub(super) fn load_preview(&mut self, store: &Store) {
        self.preview_selected_msg = 0;
        self.preview_scroll_offset = 0;
        if let Some(result) = self.results.get(self.selected_index) {
            match store.get_messages(&result.session.id) {
                Ok(msgs) => {
                    self.preview_messages = msgs.into_iter().take(30).collect();
                }
                Err(_) => {
                    self.preview_messages.clear();
                }
            }
        } else {
            self.preview_messages.clear();
        }
    }

    pub(super) fn enter_viewing(&mut self, store: &Store) {
        let Some(session) = self.results.get(self.selected_index).map(|r| r.session.clone()) else {
            return;
        };
        self.viewing.search_query = self
            .query
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if self.viewing.load(session, store) {
            self.viewing.stack.clear();
            if let Some(&first) = self.viewing.matches.first() {
                self.viewing.selected_msg = first;
            }
            self.mode = AppMode::Viewing;
        }
    }

    pub(super) fn open_subagents_picker(&mut self) {
        if self.viewing.children.is_empty() {
            self.status_message = Some("No subagents".to_string());
            return;
        }
        self.viewing.child_selected = 0;
        self.mode = AppMode::Subagents;
    }

    pub(super) fn handle_subagents_key(&mut self, key: KeyEvent, store: &Store) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = AppMode::Viewing;
            }
            KeyCode::Up | KeyCode::Char('k') if self.viewing.child_selected > 0 => {
                self.viewing.child_selected -= 1;
            }
            KeyCode::Down | KeyCode::Char('j')
                if self.viewing.child_selected + 1 < self.viewing.children.len() =>
            {
                self.viewing.child_selected += 1;
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                self.drill_into_subagent(store);
            }
            _ => {}
        }
    }

    fn drill_into_subagent(&mut self, store: &Store) {
        if self.viewing.open_child(store, viewing_layout(self.terminal_area).messages) {
            self.mode = AppMode::Viewing;
        }
    }

    pub(super) fn back_out_of_viewing(&mut self, store: &Store) {
        if !self.viewing.back(store, viewing_layout(self.terminal_area).messages) {
            self.exit_viewing_to_search();
        }
    }

    fn exit_viewing_to_search(&mut self) {
        self.mode = AppMode::Search;
        self.viewing.clear();
        self.share_popup = None;
    }

    pub(super) fn copy_current_message(&mut self) {
        let text = self.viewing.messages.get(self.viewing.selected_msg).map(|m| m.content.clone());
        if let Some(text) = text {
            self.copy_to_clipboard(&text);
        }
    }

    pub(super) fn preview_current_session(&mut self) {
        let started = Instant::now();
        self.viewing.search_status = None;
        self.status_message = Some(match self.viewing.local_preview.as_ref() {
            Some(Ok(file)) => match crate::share::open_preview_file(file.path()) {
                Ok(()) => format!("Opened preview in {} ms", started.elapsed().as_millis()),
                Err(error) => format!("Preview failed: {error}"),
            },
            Some(Err(error)) => format!("Preview failed: {error}"),
            None => "Preview unavailable".to_string(),
        });
    }

    pub(super) fn share_current_session(&mut self) {
        let Some(session) = self.viewing.session.clone() else {
            return;
        };
        if self.share_publish_rx.is_some() {
            return;
        }
        let config = self.config.clone();
        let messages = self.viewing.messages.clone();
        let events = self.viewing.events.clone();
        let session_id = session.id.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let usage_events = crate::db::store::Store::open()
                .and_then(|store| store.list_usage_events_for_session(&session_id))
                .unwrap_or_default();
            let result =
                crate::share::publish_session(&config, &session, &messages, &events, &usage_events)
                    .map_err(|e| e.to_string());
            let _ = tx.send(result);
        });
        self.share_publish_rx = Some(rx);
        self.share_popup = Some(SharePopup {
            url: None,
            message: "Sharing session...".to_string(),
            is_error: false,
        });
        self.mode = AppMode::ShareResult;
    }

    pub(super) fn copy_to_clipboard(&mut self, text: &str) {
        self.status_message = Some(match crate::utils::copy_to_clipboard(text) {
            Ok(()) => "Copied to clipboard".to_string(),
            Err(error) => error.to_string(),
        });
    }

    pub(crate) fn poll_share_publish(&mut self) {
        let Some(rx) = self.share_publish_rx.take() else {
            return;
        };
        match rx.try_recv() {
            Ok(result) => {
                self.share_popup = Some(match result {
                    Ok(url) => SharePopup {
                        url: Some(url),
                        message: "Session shared".to_string(),
                        is_error: false,
                    },
                    Err(message) => SharePopup { url: None, message, is_error: true },
                });
            }
            Err(mpsc::TryRecvError::Empty) => {
                self.share_publish_rx = Some(rx);
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.share_popup = Some(SharePopup {
                    url: None,
                    message: "Share worker stopped before publishing finished".to_string(),
                    is_error: true,
                });
            }
        }
    }
}
