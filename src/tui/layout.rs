use ratatui::layout::{Constraint, Direction, Layout, Margin, Rect};

pub(crate) struct SearchLayout {
    pub(crate) search_box: Rect,
    pub(crate) filters: Rect,
    pub(crate) list: Rect,
    pub(crate) preview: Rect,
    pub(crate) status: Rect,
}

impl SearchLayout {
    pub(crate) fn list_inner(&self) -> Rect {
        self.list.inner(Margin { horizontal: 1, vertical: 1 })
    }

    pub(crate) fn preview_inner(&self) -> Rect {
        self.preview.inner(Margin { horizontal: 1, vertical: 1 })
    }
}

pub(crate) fn search_layout(area: Rect) -> SearchLayout {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .split(area);

    let main = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(outer[2]);

    SearchLayout {
        search_box: outer[0],
        filters: outer[1],
        list: main[0],
        preview: main[1],
        status: outer[3],
    }
}

pub(crate) struct ViewingLayout {
    pub(crate) content: Rect,
    pub(crate) summary: Rect,
    pub(crate) messages: Rect,
    pub(crate) help: Rect,
}

impl ViewingLayout {
    pub(crate) fn scrollbar_area(&self) -> Rect {
        Rect::new(
            self.content.x,
            self.messages.y.saturating_sub(1),
            self.content.width,
            self.messages.height.saturating_add(2),
        )
    }
}

pub(crate) fn viewing_layout(area: Rect) -> ViewingLayout {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(area);

    let inner = outer[0].inner(Margin { horizontal: 1, vertical: 1 });
    let content = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner);

    ViewingLayout { content: outer[0], summary: content[0], messages: content[1], help: outer[1] }
}

pub(crate) struct MessagePane {
    rows: Vec<usize>,
    focus: Vec<usize>,
}

impl MessagePane {
    pub(crate) fn new(rows: Vec<usize>, focus: Vec<usize>) -> Self {
        debug_assert_eq!(rows.len(), focus.len());
        Self { rows, focus }
    }

    pub(crate) fn total_rows(&self) -> usize {
        self.rows.iter().sum()
    }

    pub(crate) fn start_of(&self, index: usize) -> usize {
        self.rows.iter().take(index).sum()
    }

    pub(crate) fn index_at(&self, row: usize) -> Option<usize> {
        let mut end = 0usize;
        for (index, count) in self.rows.iter().enumerate() {
            end += count;
            if row < end {
                return Some(index);
            }
        }
        None
    }

    pub(crate) fn scroll_start(
        &self,
        offset: usize,
        selected: usize,
        viewport_rows: usize,
    ) -> usize {
        if viewport_rows == 0 || self.rows.is_empty() {
            return 0;
        }

        let max_start = self.total_rows().saturating_sub(viewport_rows);
        let start = offset.min(max_start);
        let selected = selected.min(self.rows.len() - 1);
        let selected_start = self.start_of(selected);
        let selected_rows = self.rows[selected];
        let selected_end = selected_start + selected_rows;
        let focus_end = selected_start + self.focus[selected].min(viewport_rows);

        if selected_start < start {
            if selected_rows > viewport_rows && start < selected_end {
                start
            } else {
                selected_start.min(max_start)
            }
        } else if focus_end > start + viewport_rows {
            (focus_end - viewport_rows).min(max_start)
        } else {
            start
        }
    }
}

pub(crate) fn vertical_scrollbar_position(
    column: u16,
    row: u16,
    area: Rect,
    content_len: usize,
    viewport_len: usize,
) -> Option<usize> {
    if viewport_len == 0 || content_len <= viewport_len {
        return None;
    }

    let track = area.inner(Margin { horizontal: 0, vertical: 1 });
    if track.width == 0 || track.height == 0 {
        return None;
    }

    let bar_column = track.x + track.width - 1;
    if column != bar_column || row < track.y || row >= track.bottom() {
        return None;
    }

    let max_position = content_len - viewport_len;
    let rel = usize::from(row - track.y);
    match usize::from(track.height - 1) {
        0 => Some(0),
        denom => Some((rel * max_position + denom / 2) / denom),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane() -> MessagePane {
        MessagePane::new(vec![3, 3, 3, 3, 3], vec![2, 2, 2, 2, 2])
    }

    #[test]
    fn scroll_start_keeps_viewport_while_selection_visible() {
        let pane = pane();
        assert_eq!(pane.scroll_start(2, 1, 6), 2);
        assert_eq!(pane.scroll_start(2, 2, 6), 2);
    }

    #[test]
    fn scroll_start_follows_selection_above_viewport() {
        let pane = pane();
        assert_eq!(pane.scroll_start(6, 1, 6), 3);
        assert_eq!(pane.scroll_start(6, 0, 6), 0);
    }

    #[test]
    fn scroll_start_follows_selection_focus_below_viewport() {
        let pane = pane();
        assert_eq!(pane.scroll_start(0, 2, 6), 2);
        assert_eq!(pane.scroll_start(0, 4, 6), 8);
    }

    #[test]
    fn scroll_start_clamps_stale_offset() {
        let pane = pane();
        assert_eq!(pane.scroll_start(100, 4, 6), 9);
    }

    #[test]
    fn index_at_maps_rows_to_messages() {
        let pane = pane();
        assert_eq!(pane.index_at(0), Some(0));
        assert_eq!(pane.index_at(2), Some(0));
        assert_eq!(pane.index_at(3), Some(1));
        assert_eq!(pane.index_at(14), Some(4));
        assert_eq!(pane.index_at(15), None);
    }

    #[test]
    fn search_layout_partitions_main_area() {
        for width in 20u16..=200 {
            let layout = search_layout(Rect::new(0, 0, width, 24));
            assert_eq!(layout.list.width + layout.preview.width, width);
            assert_eq!(layout.preview.x, layout.list.x + layout.list.width);
        }
    }

    #[test]
    fn scrollbar_position_maps_track_ends() {
        let area = Rect::new(0, 4, 20, 10);
        let bar = area.x + area.width - 1;
        assert_eq!(vertical_scrollbar_position(bar, 5, area, 100, 8), Some(0));
        assert_eq!(vertical_scrollbar_position(bar, 12, area, 100, 8), Some(92));
        assert_eq!(vertical_scrollbar_position(bar - 1, 5, area, 100, 8), None);
        assert_eq!(vertical_scrollbar_position(bar, 5, area, 8, 8), None);
    }
}
