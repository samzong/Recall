use super::*;

impl App {
    pub(crate) fn handle_mouse_down(&mut self, column: u16, row: u16, store: &Store) {
        self.mouse_drag_target = None;
        self.handle_mouse_down_at(column, row, store, true);
    }

    pub(crate) fn handle_mouse_drag(&mut self, column: u16, row: u16, store: &Store) {
        match self.mouse_drag_target {
            Some(MouseDragTarget::SearchList) if matches!(self.mode, AppMode::Search) => {
                let layout = search_layout(self.terminal_area);
                self.drag_search_list_scrollbar(row, &layout, store);
            }
            Some(MouseDragTarget::SearchPreview) if matches!(self.mode, AppMode::Search) => {
                let layout = search_layout(self.terminal_area);
                self.drag_search_preview_scrollbar(row, &layout);
            }
            Some(MouseDragTarget::Viewing) if matches!(self.mode, AppMode::Viewing) => {
                let layout = viewing_layout(self.terminal_area);
                self.drag_viewing_scrollbar(row, &layout);
            }
            Some(_) => {
                self.mouse_drag_target = None;
            }
            None => self.handle_mouse_down_at(column, row, store, false),
        }
    }

    pub(crate) fn handle_mouse_up(&mut self) {
        self.mouse_drag_target = None;
    }

    fn handle_mouse_down_at(
        &mut self,
        column: u16,
        row: u16,
        store: &Store,
        capture_scrollbar: bool,
    ) {
        if matches!(self.mode, AppMode::Viewing) {
            let layout = viewing_layout(self.terminal_area);
            if self.handle_viewing_scrollbar_down(column, row, &layout) {
                if capture_scrollbar {
                    self.mouse_drag_target = Some(MouseDragTarget::Viewing);
                }
                return;
            }
            if let Some((index, start)) = self.viewing_message_at_row(row, &layout) {
                self.viewing.selected_msg = index;
                self.viewing.scroll_offset = start;
            }
            return;
        }

        if !matches!(self.mode, AppMode::Search) {
            return;
        }

        let layout = search_layout(self.terminal_area);
        if let Some(target) = self.handle_search_scrollbar_down(column, row, &layout, store) {
            if capture_scrollbar {
                self.mouse_drag_target = Some(target);
            }
            return;
        }

        match self.search_mouse_target(column, row, &layout) {
            Some(SearchMouseTarget::SessionList(index)) => {
                self.panel_focus = PanelFocus::SessionList;
                if let Some(index) = index {
                    self.result_scroll_offset =
                        self.result_list_start(layout.list_inner().height as usize);
                    self.selected_index = index;
                    self.load_preview(store);
                }
            }
            Some(SearchMouseTarget::Preview) if !self.preview_messages.is_empty() => {
                self.panel_focus = PanelFocus::Preview;
                if let Some((index, start)) = self.preview_message_at_row(row, &layout) {
                    self.preview_selected_msg = index;
                    self.preview_scroll_offset = start;
                }
            }
            _ => {}
        }
    }

    fn handle_search_scrollbar_down(
        &mut self,
        column: u16,
        row: u16,
        layout: &SearchLayout,
        store: &Store,
    ) -> Option<MouseDragTarget> {
        let list_viewport = layout.list_inner().height as usize;
        if vertical_scrollbar_position(column, row, layout.list, self.results.len(), list_viewport)
            .is_some()
        {
            self.drag_search_list_scrollbar(row, layout, store);
            return Some(MouseDragTarget::SearchList);
        }

        if self.preview_messages.is_empty() {
            return None;
        }

        let inner = layout.preview_inner();
        let pane = self.preview_pane(inner.width as usize);
        if vertical_scrollbar_position(
            column,
            row,
            layout.preview,
            pane.total_rows(),
            inner.height as usize,
        )
        .is_some()
        {
            self.drag_search_preview_scrollbar(row, layout);
            return Some(MouseDragTarget::SearchPreview);
        }

        None
    }

    fn handle_viewing_scrollbar_down(
        &mut self,
        column: u16,
        row: u16,
        layout: &ViewingLayout,
    ) -> bool {
        let messages = layout.messages;
        let pane = self.viewing.pane(messages.width as usize);
        if vertical_scrollbar_position(
            column,
            row,
            layout.scrollbar_area(),
            pane.total_rows(),
            messages.height as usize,
        )
        .is_some()
        {
            self.drag_viewing_scrollbar(row, layout);
            return true;
        }

        false
    }

    fn drag_search_list_scrollbar(&mut self, row: u16, layout: &SearchLayout, store: &Store) {
        let list_viewport = layout.list_inner().height as usize;
        let Some(position) =
            vertical_scrollbar_row_position(row, layout.list, self.results.len(), list_viewport)
        else {
            return;
        };

        self.panel_focus = PanelFocus::SessionList;
        self.result_scroll_offset = position;
        let max_position = self.results.len().saturating_sub(list_viewport);
        self.selected_index = if position == max_position {
            self.results.len().saturating_sub(1)
        } else {
            position.min(self.results.len().saturating_sub(1))
        };
        self.load_preview(store);
    }

    fn drag_search_preview_scrollbar(&mut self, row: u16, layout: &SearchLayout) {
        if self.preview_messages.is_empty() {
            return;
        }

        let inner = layout.preview_inner();
        let pane = self.preview_pane(inner.width as usize);
        let Some(position) = vertical_scrollbar_row_position(
            row,
            layout.preview,
            pane.total_rows(),
            inner.height as usize,
        ) else {
            return;
        };

        self.panel_focus = PanelFocus::Preview;
        self.preview_scroll_offset = position;
        let max_position = pane.total_rows().saturating_sub(inner.height as usize);
        if position == max_position {
            self.preview_selected_msg = self.preview_messages.len() - 1;
        } else if let Some(index) = pane.index_at(position) {
            self.preview_selected_msg = index;
        }
    }

    fn drag_viewing_scrollbar(&mut self, row: u16, layout: &ViewingLayout) {
        let messages = layout.messages;
        let pane = self.viewing.pane(messages.width as usize);
        let Some(position) = vertical_scrollbar_row_position(
            row,
            layout.scrollbar_area(),
            pane.total_rows(),
            messages.height as usize,
        ) else {
            return;
        };

        self.viewing.scroll_offset = position;
        let max_position = pane.total_rows().saturating_sub(messages.height as usize);
        if position == max_position {
            self.viewing.selected_msg = self.viewing.messages.len() - 1;
        } else if let Some(index) = pane.index_at(position) {
            self.viewing.selected_msg = index;
        }
    }

    pub(crate) fn handle_mouse_scroll_up(&mut self, column: u16, row: u16, store: &Store) {
        self.handle_mouse_scroll(column, row, true, store);
    }

    pub(crate) fn handle_mouse_scroll_down(&mut self, column: u16, row: u16, store: &Store) {
        self.handle_mouse_scroll(column, row, false, store);
    }

    fn handle_mouse_scroll(&mut self, column: u16, row: u16, up: bool, store: &Store) {
        if matches!(self.mode, AppMode::Search) {
            let layout = search_layout(self.terminal_area);
            match self.search_mouse_target(column, row, &layout) {
                Some(SearchMouseTarget::SessionList(_)) => {
                    self.panel_focus = PanelFocus::SessionList;
                    if up {
                        self.scroll_result_list_up(store);
                    } else {
                        self.scroll_result_list_down(store);
                    }
                    return;
                }
                Some(SearchMouseTarget::Preview) if !self.preview_messages.is_empty() => {
                    self.panel_focus = PanelFocus::Preview;
                    self.move_preview_selection(up);
                    return;
                }
                _ => {}
            }
        }

        if up { self.handle_scroll_up(store) } else { self.handle_scroll_down(store) }
    }

    fn search_mouse_target(
        &self,
        column: u16,
        row: u16,
        layout: &SearchLayout,
    ) -> Option<SearchMouseTarget> {
        let pos = Position { x: column, y: row };
        if layout.list.contains(pos) {
            let inner = layout.list_inner();
            if !inner.contains(pos) {
                return Some(SearchMouseTarget::SessionList(None));
            }

            let start = self.result_list_start(inner.height as usize);
            let index = start + usize::from(row - inner.y);
            return Some(SearchMouseTarget::SessionList(
                (index < self.results.len()).then_some(index),
            ));
        }

        if layout.preview.contains(pos) {
            return Some(SearchMouseTarget::Preview);
        }

        None
    }
}
