use std::path::PathBuf;

use super::*;
use crate::session::related::RelatedSessionGraph;
use crate::share::{BundledSessionInput, ShareRenderOptions};
use crate::tui::share_state::{
    SharePickerItem, SharePickerState, SharePublishOutcome, SharePublishResponse,
};

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
        self.share_picker = None;
        self.share_popup = None;
        self.share_publish_rx = None;
        self.share_request_id = self.share_request_id.wrapping_add(1);
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

    pub(super) fn share_current_session(&mut self, store: &Store) {
        let Some(session) = self.viewing.session.clone() else {
            return;
        };
        if self.share_publish_rx.is_some() {
            return;
        }
        if let Ok(graph) = RelatedSessionGraph::for_session(store, &session)
            && graph.len() >= 2
        {
            self.share_picker = Some(SharePickerState::from_graph(graph));
            self.mode = AppMode::SharePicker;
            return;
        }
        self.dispatch_single_share(store, session);
    }

    pub(super) fn handle_share_picker_key(&mut self, key: KeyEvent, store: &Store) {
        let Some(picker) = self.share_picker.as_mut() else {
            self.mode = AppMode::Viewing;
            return;
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.share_picker = None;
                self.mode = AppMode::Viewing;
            }
            KeyCode::Up | KeyCode::Char('k') if picker.selected > 0 => {
                picker.selected -= 1;
            }
            KeyCode::Down | KeyCode::Char('j') if picker.selected + 1 < picker.items.len() => {
                picker.selected += 1;
            }
            KeyCode::Char(' ') => {
                if let Err(message) = picker.toggle_selected() {
                    self.status_message = Some(message.to_string());
                }
            }
            KeyCode::Char('a') => {
                picker.toggle_all_non_empty();
            }
            KeyCode::Char('v') => {
                self.preview_share_picker(store);
            }
            KeyCode::Enter => {
                self.confirm_share_picker(store);
            }
            _ => {}
        }
    }

    fn preview_share_picker(&mut self, store: &Store) {
        let Some(picker) = self.share_picker.as_mut() else {
            return;
        };
        let selected: Vec<SharePickerItem> =
            picker.items.iter().filter(|item| item.checked).cloned().collect();
        if selected.is_empty() {
            return;
        }
        let started = Instant::now();
        let result = load_bundled_inputs_from_store(store, &selected)
            .and_then(|inputs| crate::share::create_bundle_preview(&inputs))
            .and_then(|file| {
                crate::share::open_preview_file(file.path())?;
                Ok(file)
            });
        match result {
            Ok(file) => {
                picker.local_preview = Some(file);
                self.status_message =
                    Some(format!("Opened preview in {} ms", started.elapsed().as_millis()));
            }
            Err(error) => {
                self.status_message = Some(format!("Preview failed: {error}"));
            }
        }
    }

    fn confirm_share_picker(&mut self, store: &Store) {
        let Some(picker) = self.share_picker.take() else {
            return;
        };
        let selected: Vec<SharePickerItem> =
            picker.items.into_iter().filter(|item| item.checked).collect();
        if selected.is_empty() {
            self.mode = AppMode::Viewing;
            return;
        }
        self.dispatch_bundle_share(store, selected);
    }

    fn dispatch_single_share(&mut self, store: &Store, session: crate::types::Session) {
        let config = self.config.clone();
        let messages = self.viewing.messages.clone();
        let events = self.viewing.events.clone();
        let session_id = session.id.clone();
        let db_path = store.conn.path().filter(|p| !p.is_empty()).map(PathBuf::from);
        let preloaded_usage = if db_path.is_none() {
            store.list_usage_events_for_session(&session_id).unwrap_or_default()
        } else {
            Vec::new()
        };
        self.share_request_id = self.share_request_id.wrapping_add(1);
        let request_id = self.share_request_id;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let usage_events = match db_path {
                Some(path) => Store::open_read_only_at(&path)
                    .and_then(|s| s.list_usage_events_for_session(&session_id))
                    .unwrap_or_default(),
                None => preloaded_usage,
            };
            let title = crate::session::related::format_related_title(&session, None, None);
            let display_meta =
                crate::share::meta::collect_session_display_meta(&session, &usage_events);
            let input = BundledSessionInput {
                session,
                messages,
                events,
                display_meta,
                parent_id: None,
                depth: 0,
                relation: None,
                via: None,
                role_label: None,
                display_title: title,
                spawn_anchor: None,
            };
            let result = crate::share::publish_bundle_with_options(
                &config,
                &[input],
                &ShareRenderOptions::default(),
            )
            .map(|res| SharePublishOutcome {
                url: res.url,
                session_count: res.session_count,
                bundle_diff: res.bundle_diff,
            })
            .map_err(|e| e.to_string());
            let _ = tx.send(SharePublishResponse { request_id, result });
        });
        self.share_publish_rx = Some(rx);
        self.share_popup = Some(SharePopup {
            url: None,
            message: "Sharing session...".to_string(),
            is_error: false,
        });
        self.mode = AppMode::ShareResult;
    }

    fn dispatch_bundle_share(&mut self, store: &Store, selected: Vec<SharePickerItem>) {
        let count = selected.len();
        let config = self.config.clone();
        let db_path = store.conn.path().filter(|p| !p.is_empty()).map(PathBuf::from);
        let preloaded = if db_path.is_none() {
            Some(load_bundled_inputs_from_store(store, &selected).map_err(|e| e.to_string()))
        } else {
            None
        };
        self.share_request_id = self.share_request_id.wrapping_add(1);
        let request_id = self.share_request_id;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let loaded = match db_path {
                Some(path) => Store::open_read_only_at(&path)
                    .and_then(|s| load_bundled_inputs_from_store(&s, &selected))
                    .map_err(|e| e.to_string()),
                None => preloaded.unwrap_or_else(|| Err("unavailable store".to_string())),
            };
            let result = loaded.and_then(|inputs| {
                crate::share::publish_bundle_with_options(
                    &config,
                    &inputs,
                    &ShareRenderOptions::default(),
                )
                .map(|res| SharePublishOutcome {
                    url: res.url,
                    session_count: res.session_count,
                    bundle_diff: res.bundle_diff,
                })
                .map_err(|e| e.to_string())
            });
            let _ = tx.send(SharePublishResponse { request_id, result });
        });
        self.share_publish_rx = Some(rx);
        self.share_popup = Some(SharePopup {
            url: None,
            message: if count == 1 {
                "Sharing session...".to_string()
            } else {
                format!("Sharing {count} sessions...")
            },
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
            Ok(response) => {
                if response.request_id != self.share_request_id {
                    return;
                }
                self.share_popup = Some(match response.result {
                    Ok(outcome) => {
                        let mut message = if outcome.session_count <= 1 {
                            "Session shared".to_string()
                        } else {
                            format!("Shared {} sessions", outcome.session_count)
                        };
                        if let Some(diff) = outcome.bundle_diff {
                            message.push_str(&format!(
                                " (+{} added, -{} removed)",
                                diff.added.len(),
                                diff.removed.len()
                            ));
                        }
                        SharePopup { url: Some(outcome.url), message, is_error: false }
                    }
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

fn load_bundled_inputs_from_store(
    store: &Store,
    selected: &[SharePickerItem],
) -> anyhow::Result<Vec<BundledSessionInput>> {
    let mut inputs = Vec::with_capacity(selected.len());
    for item in selected {
        let messages = store.get_messages(&item.session.id)?;
        let events = store.list_session_events_for_session(&item.session.id)?;
        let usage_events = store.list_usage_events_for_session(&item.session.id)?;
        let display_meta =
            crate::share::meta::collect_session_display_meta(&item.session, &usage_events);
        inputs.push(BundledSessionInput {
            session: item.session.clone(),
            messages,
            events,
            display_meta,
            parent_id: item.parent_id.clone(),
            depth: item.depth,
            relation: item.relation,
            via: item.via.clone(),
            role_label: item.role_label.clone(),
            display_title: item.display_title.clone(),
            spawn_anchor: item.spawn_anchor,
        });
    }
    Ok(inputs)
}
