use super::*;

impl App {
    pub(crate) fn handle_scroll_up(&mut self, store: &Store) {
        match self.mode {
            AppMode::Search => match self.panel_focus {
                PanelFocus::SessionList => self.scroll_result_list_up(store),
                PanelFocus::Preview => self.move_preview_selection(true),
            },
            AppMode::Viewing if self.viewing.selected_msg > 0 => {
                self.viewing.selected_msg -= 1;
                self.anchor_viewing_scroll();
            }
            AppMode::Settings if self.settings_selected > 0 => {
                self.settings_selected -= 1;
            }
            AppMode::Filters
                if self.filters.editing == Some(FilterFocus::Source)
                    && self.filters.source_picker.selected > 0 =>
            {
                self.filters.source_picker.selected -= 1;
            }
            AppMode::Filters
                if self.filters.editing == Some(FilterFocus::Project)
                    && self.filters.project_picker.selected > 0 =>
            {
                self.filters.project_picker.selected -= 1;
            }
            AppMode::Filters
                if self.filters.editing != Some(FilterFocus::Source)
                    && self.filters.editing != Some(FilterFocus::Project) =>
            {
                self.filters.focus = self.filters.focus.previous();
            }
            AppMode::Usage => self.usage.scroll(true),
            _ => {}
        }
    }

    pub(crate) fn handle_scroll_down(&mut self, store: &Store) {
        match self.mode {
            AppMode::Search => match self.panel_focus {
                PanelFocus::SessionList => self.scroll_result_list_down(store),
                PanelFocus::Preview => self.move_preview_selection(false),
            },
            AppMode::Viewing if self.viewing.selected_msg + 1 < self.viewing.messages.len() => {
                self.viewing.selected_msg += 1;
                self.anchor_viewing_scroll();
            }
            AppMode::Settings if self.settings_selected + 1 < self.settings_row_count() => {
                self.settings_selected += 1;
            }
            AppMode::Filters
                if self.filters.editing == Some(FilterFocus::Source)
                    && self.filters.source_picker.selected + 1
                        < self.source_picker_rows().len() =>
            {
                self.filters.source_picker.selected += 1;
            }
            AppMode::Filters
                if self.filters.editing == Some(FilterFocus::Project)
                    && self.filters.project_picker.selected + 1
                        < self.project_picker_rows().len() =>
            {
                self.filters.project_picker.selected += 1;
            }
            AppMode::Filters
                if self.filters.editing != Some(FilterFocus::Source)
                    && self.filters.editing != Some(FilterFocus::Project) =>
            {
                self.filters.focus = self.filters.focus.next();
            }
            AppMode::Usage => self.usage.scroll(false),
            _ => {}
        }
    }

    pub(super) fn scroll_result_list_up(&mut self, store: &Store) {
        if self.results.is_empty() || self.selected_index == 0 {
            return;
        }

        let visible_rows = search_layout(self.terminal_area).list_inner().height as usize;
        let start = self.result_list_start(visible_rows);
        self.selected_index -= 1;
        if visible_rows > 0 && self.selected_index < start {
            self.result_scroll_offset = self.selected_index;
        } else {
            self.result_scroll_offset = start;
        }
        self.load_preview(store);
    }

    pub(super) fn scroll_result_list_down(&mut self, store: &Store) {
        if self.selected_index + 1 >= self.results.len() {
            return;
        }

        let visible_rows = search_layout(self.terminal_area).list_inner().height as usize;
        let start = self.result_list_start(visible_rows);
        self.selected_index += 1;
        if visible_rows > 0 && self.selected_index >= start + visible_rows {
            self.result_scroll_offset = self.selected_index + 1 - visible_rows;
        } else {
            self.result_scroll_offset = start;
        }
        self.load_preview(store);
    }

    pub(super) fn page_result_list(&mut self, up: bool, store: &Store) {
        if self.results.is_empty() {
            return;
        }

        let visible_rows = search_layout(self.terminal_area).list_inner().height as usize;
        if visible_rows == 0 {
            return;
        }

        let previous = self.selected_index;
        if up {
            self.selected_index = self.selected_index.saturating_sub(visible_rows);
        } else {
            self.selected_index = (self.selected_index + visible_rows).min(self.results.len() - 1);
        }
        self.result_scroll_offset = self.result_list_start(visible_rows);
        if self.selected_index != previous {
            self.load_preview(store);
        }
    }

    pub(crate) fn result_list_start(&self, visible_rows: usize) -> usize {
        if visible_rows == 0 || self.results.is_empty() {
            return 0;
        }

        let selected_index = self.selected_index.min(self.results.len().saturating_sub(1));
        let max_start = self.results.len().saturating_sub(visible_rows);
        let start = self.result_scroll_offset.min(max_start);
        if selected_index < start {
            selected_index
        } else if selected_index >= start + visible_rows {
            selected_index + 1 - visible_rows
        } else {
            start
        }
    }

    pub(super) fn move_preview_selection(&mut self, up: bool) {
        let can_move = if up {
            self.preview_selected_msg > 0
        } else {
            self.preview_selected_msg + 1 < self.preview_messages.len()
        };
        if !can_move {
            return;
        }

        let inner = search_layout(self.terminal_area).preview_inner();
        let pane = self.preview_pane(inner.width as usize);
        let viewport = inner.height as usize;
        self.preview_scroll_offset =
            pane.scroll_start(self.preview_scroll_offset, self.preview_selected_msg, viewport);
        if up {
            self.preview_selected_msg -= 1;
        } else {
            self.preview_selected_msg += 1;
        }
        self.preview_scroll_offset =
            pane.scroll_start(self.preview_scroll_offset, self.preview_selected_msg, viewport);
    }

    pub(super) fn page_preview_selection(&mut self, up: bool) {
        let area = search_layout(self.terminal_area).preview_inner();
        self.preview_pane(area.width as usize).page(
            &mut self.preview_selected_msg,
            &mut self.preview_scroll_offset,
            area.height as usize,
            up,
        );
    }

    pub(super) fn page_viewing_selection(&mut self, up: bool) {
        let area = viewing_layout(self.terminal_area).messages;
        self.viewing.pane(area.width as usize).page(
            &mut self.viewing.selected_msg,
            &mut self.viewing.scroll_offset,
            area.height as usize,
            up,
        );
    }

    pub(super) fn anchor_viewing_scroll(&mut self) {
        self.viewing.anchor(viewing_layout(self.terminal_area).messages);
    }

    pub(crate) fn preview_pane(&self, inner_width: usize) -> MessagePane {
        let mut rows = Vec::with_capacity(self.preview_messages.len());
        let mut focus = Vec::with_capacity(self.preview_messages.len());
        for msg in &self.preview_messages {
            let text: String = msg.content.chars().take(300).collect();
            let body: usize = text
                .lines()
                .take(6)
                .map(|line| {
                    let line = crate::utils::sanitize_line(line);
                    wrap_visual_rows(&format!("  {line}"), inner_width).len()
                })
                .sum();
            rows.push(body + 2);
            focus.push(1 + usize::from(text.lines().next().is_some()));
        }
        MessagePane::new(rows, focus)
    }

    pub(super) fn preview_message_at_row(
        &self,
        row: u16,
        layout: &SearchLayout,
    ) -> Option<(usize, usize)> {
        let area = layout.preview_inner();
        self.preview_pane(area.width as usize).message_at(
            row,
            area,
            self.preview_scroll_offset,
            self.preview_selected_msg,
        )
    }

    pub(super) fn viewing_message_at_row(
        &self,
        row: u16,
        layout: &ViewingLayout,
    ) -> Option<(usize, usize)> {
        let area = layout.messages;
        self.viewing.pane(area.width as usize).message_at(
            row,
            area,
            self.viewing.scroll_offset,
            self.viewing.selected_msg,
        )
    }
}
