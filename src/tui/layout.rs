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
    pub(crate) fn page(
        &self,
        selected: &mut usize,
        scroll_offset: &mut usize,
        viewport: usize,
        up: bool,
    ) {
        if viewport == 0 || self.rows.is_empty() || self.total_rows() <= viewport {
            return;
        }

        let current = self.scroll_start(*scroll_offset, *selected, viewport);
        let max_start = self.total_rows().saturating_sub(viewport);
        let target =
            if up { current.saturating_sub(viewport) } else { (current + viewport).min(max_start) };
        let Some(index) = (!up && target == max_start)
            .then_some(self.rows.len() - 1)
            .or_else(|| self.index_at(target))
        else {
            return;
        };

        *selected = index;
        *scroll_offset = self.scroll_start(target, *selected, viewport);
    }
    pub(crate) fn message_at(
        &self,
        row: u16,
        area: Rect,
        offset: usize,
        selected: usize,
    ) -> Option<(usize, usize)> {
        if row < area.y || row >= area.bottom() {
            return None;
        }
        let start = self.scroll_start(offset, selected, area.height as usize);
        self.index_at(start + usize::from(row - area.y)).map(|index| (index, start))
    }

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

    vertical_scrollbar_row_position(row, area, content_len, viewport_len)
}

pub(crate) fn vertical_scrollbar_row_position(
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

    let max_position = content_len - viewport_len;
    let rel = usize::from(row.saturating_sub(track.y).min(track.height - 1));
    match usize::from(track.height - 1) {
        0 => Some(0),
        denom => Some((rel * max_position + denom / 2) / denom),
    }
}
