use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::tui::app::App;
use crate::tui::layout::search_layout;
use crate::tui::search_state::{FilterFocus, PanelFocus};
use crate::tui::text_layout::wrap_visual_rows;
use crate::types::{MatchSource, Role};

use super::popups::render_status_bar;
use super::{render_vertical_scrollbar, row_visible, truncate_label};

pub(super) fn render_search(f: &mut Frame, app: &App) {
    let layout = search_layout(f.area());

    render_search_box(f, app, layout.search_box);
    render_filters(f, app, layout.filters);
    render_result_list(f, app, layout.list);
    render_preview(f, app, layout.preview);
    render_status_bar(f, app, layout.status);
}

pub(super) fn render_search_box(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(" Recall ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let line = if app.query.is_empty() {
        if let Some(feedback) = app.search_feedback.as_deref() {
            Line::from(Span::styled(feedback.to_string(), Style::default().fg(Color::Yellow)))
        } else {
            Line::from(Span::styled("Type to search...", Style::default().fg(Color::DarkGray)))
        }
    } else if let Some(feedback) = app.search_feedback.as_deref() {
        Line::from(vec![
            Span::styled(app.query.clone(), Style::default().fg(Color::White)),
            Span::styled(format!("  {feedback}"), Style::default().fg(Color::Yellow)),
        ])
    } else {
        Line::from(Span::styled(app.query.clone(), Style::default().fg(Color::White)))
    };

    let input = Paragraph::new(line).block(block);
    f.render_widget(input, area);

    if app.panel_focus == PanelFocus::SessionList {
        let cursor_x = area.x + 1 + UnicodeWidthStr::width(&app.query[..app.cursor_pos]) as u16;
        f.set_cursor_position((cursor_x, area.y + 1));
    }
}

pub(super) fn render_filters(f: &mut Frame, app: &App, area: Rect) {
    let source_label = app.source_filter_label();
    let project_label = truncate_label(&app.project_filter_label(), 20);
    let time_label = app.time_filter_label();
    let sort_label = app.sort_label();

    let line = Line::from(vec![
        Span::styled(" S:", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("[{source_label}]"),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  P:", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("[{project_label}]"),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  T:", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("[{time_label}]"),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  Sort:", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("[{sort_label}]"),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  Ctrl+F", Style::default().fg(Color::DarkGray)),
    ]);

    f.render_widget(Paragraph::new(line), area);
}

pub(super) fn render_filter_picker(f: &mut Frame, app: &App) {
    if app.filters_editing_source {
        super::popups::render_source_picker(f, app);
    } else if app.filters_editing_project {
        super::popups::render_project_picker(f, app);
    } else {
        render_filter_overview(f, app);
    }
}

pub(super) fn render_filter_overview(f: &mut Frame, app: &App) {
    let area = f.area();
    let width = area.width.min(68);
    let available_height = area.height.saturating_sub(2);
    let height = if available_height == 0 { 1 } else { available_height.min(9) };
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup = Rect::new(x, y, width, height);

    let block = Block::default()
        .title(" Filters ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let mut lines = vec![Line::from("")];
    lines.push(filter_overview_line(
        "Source",
        &app.draft_source_filter_label(),
        "Enter",
        app.filter_focus == FilterFocus::Source,
    ));
    lines.push(filter_overview_line(
        "Project",
        &app.draft_project_filter_label(),
        "Enter",
        app.filter_focus == FilterFocus::Project,
    ));
    lines.push(filter_overview_line(
        "Time Range",
        app.draft_time_filter_label(),
        "←/→",
        app.filter_focus == FilterFocus::Time,
    ));
    lines.push(filter_overview_line(
        "Sort",
        app.draft_sort_label(),
        "←/→",
        app.filter_focus == FilterFocus::Sort,
    ));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(" ↑/↓", Style::default().fg(Color::Yellow)),
        Span::styled(" nav  ", Style::default().fg(Color::DarkGray)),
        Span::styled("←/→", Style::default().fg(Color::Yellow)),
        Span::styled(" adjust  ", Style::default().fg(Color::DarkGray)),
        Span::styled("Enter", Style::default().fg(Color::Yellow)),
        Span::styled(" edit  ", Style::default().fg(Color::DarkGray)),
        Span::styled("c", Style::default().fg(Color::Yellow)),
        Span::styled(" clear  ", Style::default().fg(Color::DarkGray)),
        Span::styled("Esc", Style::default().fg(Color::Yellow)),
        Span::styled(" apply", Style::default().fg(Color::DarkGray)),
    ]));

    let widget = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
    f.render_widget(Clear, popup);
    f.render_widget(widget, popup);
}

fn filter_overview_line(
    label: &'static str,
    value: &str,
    hint: &'static str,
    selected: bool,
) -> Line<'static> {
    let style = if selected {
        Style::default().bg(Color::Cyan).fg(Color::Black).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };
    Line::from(Span::styled(
        format!(" {label:<12} {:<22} {hint}", truncate_label(value, 22)),
        style,
    ))
}
pub(super) fn render_result_list(f: &mut Frame, app: &App, area: Rect) {
    let focused = app.panel_focus == PanelFocus::SessionList;
    let border_color = if focused { Color::Cyan } else { Color::DarkGray };
    let title = if app.results.is_empty() {
        " Sessions (0) ".to_string()
    } else {
        format!(" Sessions [{}/{}] ", app.selected_index + 1, app.results.len())
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    if app.results.is_empty() {
        let msg = "No results";
        let p = Paragraph::new(msg).style(Style::default().fg(Color::DarkGray)).block(block);
        f.render_widget(p, area);
        return;
    }

    let visible_rows = block.inner(area).height as usize;
    let start = app.result_list_start(visible_rows);
    let end = (start + visible_rows).min(app.results.len());

    let items: Vec<ListItem> = app.results[start..end]
        .iter()
        .enumerate()
        .map(|(offset, result)| {
            let i = start + offset;
            let s = &result.session;
            let age = crate::utils::format_age(s.started_at);
            let source_label = app.source_label_for(&s.source);
            let match_label = match result.match_source {
                MatchSource::Fts => "F",
                MatchSource::Vector => "V",
                MatchSource::Hybrid => "H",
            };
            let title: String = s.title.chars().take(40).collect();
            let selected = i == app.selected_index;
            let active_selected = focused && selected;
            let passive_selected = selected && !focused;
            let selected_text_style = if active_selected {
                Style::default().fg(Color::Black)
            } else if passive_selected {
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };

            let line = Line::from(vec![
                Span::styled(
                    source_label.to_string(),
                    if selected { selected_text_style } else { Style::default().fg(Color::Green) },
                ),
                Span::raw(" "),
                Span::styled(
                    match_label.to_string(),
                    if selected {
                        selected_text_style
                    } else {
                        Style::default().fg(Color::DarkGray)
                    },
                ),
                Span::raw(" "),
                Span::styled(
                    title,
                    if selected { selected_text_style } else { Style::default().fg(Color::White) },
                ),
                Span::styled(
                    format!("  {age}"),
                    if selected {
                        selected_text_style
                    } else {
                        Style::default().fg(Color::DarkGray)
                    },
                ),
            ]);

            ListItem::new(line).style(if active_selected {
                Style::default().bg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            })
        })
        .collect();

    let list_inner = block.inner(area);
    let list = List::new(items).block(block);
    f.render_widget(list, area);
    if focused && app.selected_index >= start && app.selected_index < end {
        let row_y = list_inner.y + (app.selected_index - start) as u16;
        f.buffer_mut().set_style(
            Rect::new(list_inner.x, row_y, list_inner.width, 1),
            Style::default().bg(Color::Cyan).add_modifier(Modifier::BOLD),
        );
    }
    render_vertical_scrollbar(f, area, app.results.len(), visible_rows, start);
}

pub(super) fn render_preview(f: &mut Frame, app: &App, area: Rect) {
    let focused = app.panel_focus == PanelFocus::Preview;
    let border_color = if focused { Color::Cyan } else { Color::DarkGray };

    let title = if let Some(result) = app.results.get(app.selected_index) {
        let dir = result.session.directory.as_deref().unwrap_or("-");
        let short_dir: String =
            dir.chars().rev().take(30).collect::<String>().chars().rev().collect();
        if focused {
            let pos = app.preview_selected_msg + 1;
            let total = app.preview_messages.len();
            format!(" Preview [{pos}/{total}] — {short_dir} ")
        } else {
            format!(" Preview — {short_dir} ")
        }
    } else {
        " Preview ".to_string()
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    if app.preview_messages.is_empty() {
        let p =
            Paragraph::new("No messages").style(Style::default().fg(Color::DarkGray)).block(block);
        f.render_widget(p, area);
        return;
    }

    let inner = block.inner(area);
    let inner_width = inner.width as usize;
    let pane = app.preview_pane(inner_width);
    let viewport_start = pane.scroll_start(
        app.preview_scroll_offset,
        app.preview_selected_msg,
        inner.height as usize,
    );
    let viewport_end = viewport_start + inner.height as usize;
    let mut visual_row = 0usize;
    let mut lines: Vec<Line> = Vec::new();
    for (i, msg) in app.preview_messages.iter().enumerate() {
        let selected = focused && i == app.preview_selected_msg;
        let (prefix, color) = match msg.role {
            Role::User => ("User: ", Color::Cyan),
            Role::Assistant => ("Asst: ", Color::Green),
        };

        let time_str = crate::utils::format_message_time(msg.timestamp);
        let header_bg = if selected && row_visible(visual_row, viewport_start, viewport_end) {
            Color::DarkGray
        } else {
            Color::Reset
        };
        let mut header = vec![Span::styled(
            prefix,
            Style::default().fg(color).bg(header_bg).add_modifier(Modifier::BOLD),
        )];
        if !time_str.is_empty() {
            header.push(Span::styled(time_str, Style::default().fg(Color::DarkGray).bg(header_bg)));
        }
        lines.push(Line::from(header));
        visual_row += 1;

        let text: String = msg.content.chars().take(300).collect();
        for line in text.lines().take(6) {
            let line = crate::utils::sanitize_line(line);
            let line = format!("  {line}");
            for row in wrap_visual_rows(&line, inner_width) {
                let body_bg = if selected && row_visible(visual_row, viewport_start, viewport_end) {
                    Color::DarkGray
                } else {
                    Color::Reset
                };
                lines.push(Line::from(Span::styled(
                    row,
                    Style::default().fg(Color::White).bg(body_bg),
                )));
                visual_row += 1;
            }
        }
        lines.push(Line::from(""));
        visual_row += 1;
    }

    let p = Paragraph::new(lines).block(block).scroll((viewport_start as u16, 0));
    f.render_widget(p, area);
    render_vertical_scrollbar(f, area, pane.total_rows(), inner.height as usize, viewport_start);
}
