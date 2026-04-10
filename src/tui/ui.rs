use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::db::search::TimeRange;
use crate::tui::app::{App, AppMode, PanelFocus, SortOrder};
use crate::types::{MatchSource, Role};

fn scroll_offset(selected_line_start: usize, inner_height: usize) -> u16 {
    if inner_height == 0 {
        0
    } else if selected_line_start + 4 > inner_height {
        (selected_line_start + 4).saturating_sub(inner_height) as u16
    } else {
        0
    }
}

pub fn render(f: &mut Frame, app: &App) {
    match app.mode {
        AppMode::Search => render_search(f, app),
        AppMode::Viewing => render_viewing(f, app),
        AppMode::ExportInput => {
            render_viewing(f, app);
            render_export_input(f, app);
        }
        AppMode::Settings => {
            render_search(f, app);
            render_settings(f, app);
        }
        AppMode::ConfirmResume => {
            render_search(f, app);
            render_confirm_resume(f, app);
        }
    }
}

fn render_search(f: &mut Frame, app: &App) {
    let area = f.area();

    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .split(area);

    render_search_box(f, app, outer[0]);
    render_filters(f, app, outer[1]);

    let main_area = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(outer[2]);

    render_result_list(f, app, main_area[0]);
    render_preview(f, app, main_area[1]);
    render_status_bar(f, app, outer[3]);
}

fn render_search_box(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(" Recall ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let display_query =
        if app.query.is_empty() { "Type to search...".to_string() } else { app.query.clone() };

    let style = if app.query.is_empty() {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(Color::White)
    };

    let input = Paragraph::new(display_query).style(style).block(block);
    f.render_widget(input, area);

    if app.panel_focus == PanelFocus::SessionList {
        let cursor_x = area.x + 1 + UnicodeWidthStr::width(&app.query[..app.cursor_pos]) as u16;
        f.set_cursor_position((cursor_x, area.y + 1));
    }
}

fn render_filters(f: &mut Frame, app: &App, area: Rect) {
    let source_label = app.source_filter_label();
    let time_label = match app.time_filter {
        TimeRange::Today => "today",
        TimeRange::Week => "7d",
        TimeRange::Month => "30d",
        TimeRange::All => "all",
    };

    let sort_label = match app.sort_order {
        SortOrder::Relevance => "relevance",
        SortOrder::Newest => "newest",
    };

    let line = Line::from(vec![
        Span::styled("  Source: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("[{source_label}]"),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  Time: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("[{time_label}]"),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  Sort: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("[{sort_label}]"),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  (Tab to cycle)", Style::default().fg(Color::DarkGray)),
    ]);

    f.render_widget(Paragraph::new(line), area);
}

fn render_result_list(f: &mut Frame, app: &App, area: Rect) {
    let focused = app.panel_focus == PanelFocus::SessionList;
    let border_color = if focused { Color::Cyan } else { Color::DarkGray };
    let block = Block::default()
        .title(format!(" Sessions ({}) ", app.results.len()))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    if app.results.is_empty() {
        let msg = "No results";
        let p = Paragraph::new(msg).style(Style::default().fg(Color::DarkGray)).block(block);
        f.render_widget(p, area);
        return;
    }

    let items: Vec<ListItem> = app
        .results
        .iter()
        .enumerate()
        .map(|(i, result)| {
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

            let line = Line::from(vec![
                Span::styled(
                    source_label.to_string(),
                    Style::default().fg(if selected { Color::Black } else { Color::Green }),
                ),
                Span::raw(" "),
                Span::styled(
                    match_label.to_string(),
                    Style::default().fg(if selected { Color::Black } else { Color::DarkGray }),
                ),
                Span::raw(" "),
                Span::styled(
                    title,
                    Style::default().fg(if selected { Color::Black } else { Color::White }),
                ),
                Span::styled(
                    format!("  {age}"),
                    Style::default().fg(if selected { Color::Black } else { Color::DarkGray }),
                ),
            ]);

            ListItem::new(line).style(if selected {
                Style::default().bg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            })
        })
        .collect();

    let list = List::new(items).block(block);
    f.render_widget(list, area);
}

fn render_preview(f: &mut Frame, app: &App, area: Rect) {
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

    let mut lines: Vec<Line> = Vec::new();
    let mut selected_line_start: usize = 0;

    for (i, msg) in app.preview_messages.iter().enumerate() {
        let selected = focused && i == app.preview_selected_msg;
        let (prefix, color) = match msg.role {
            Role::User => ("User: ", Color::Cyan),
            Role::Assistant => ("Asst: ", Color::Green),
        };

        if selected {
            selected_line_start = lines.len();
        }

        let bg = if selected { Color::DarkGray } else { Color::Reset };

        lines.push(Line::from(Span::styled(
            prefix,
            Style::default().fg(color).bg(bg).add_modifier(Modifier::BOLD),
        )));

        let text: String = msg.content.chars().take(300).collect();
        for line in text.lines().take(6) {
            lines.push(Line::from(Span::styled(
                format!("  {line}"),
                Style::default().fg(Color::White).bg(bg),
            )));
        }
        lines.push(Line::from(""));
    }

    let scroll = scroll_offset(selected_line_start, block.inner(area).height as usize);

    let p = Paragraph::new(lines).block(block).wrap(Wrap { trim: false }).scroll((scroll, 0));
    f.render_widget(p, area);
}

fn render_viewing(f: &mut Frame, app: &App) {
    let area = f.area();

    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(area);

    let session_info = app
        .results
        .get(app.selected_index)
        .map(|r| {
            let s = &r.session;
            let dir = s.directory.as_deref().unwrap_or("");
            let count = app.viewing_messages.len();
            let pos = app.viewing_selected_msg + 1;
            format!(" {} — {dir} [{pos}/{count}] ", s.title)
        })
        .unwrap_or_else(|| " Conversation ".to_string());

    let block = Block::default()
        .title(session_info)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let mut lines: Vec<Line> = Vec::new();
    let mut selected_line_start: usize = 0;

    for (i, msg) in app.viewing_messages.iter().enumerate() {
        let selected = i == app.viewing_selected_msg;
        let (prefix, color) = match msg.role {
            Role::User => ("User", Color::Cyan),
            Role::Assistant => ("Assistant", Color::Green),
        };

        if selected {
            selected_line_start = lines.len();
        }

        let bg = if selected { Color::DarkGray } else { Color::Reset };

        lines.push(Line::from(Span::styled(
            format!("── {prefix} ──"),
            Style::default().fg(color).bg(bg).add_modifier(Modifier::BOLD),
        )));

        for line in msg.content.lines() {
            lines.push(Line::from(Span::styled(
                line.to_string(),
                Style::default().fg(Color::White).bg(bg),
            )));
        }
        lines.push(Line::from(""));
    }

    let scroll = scroll_offset(selected_line_start, block.inner(outer[0]).height as usize);

    let p = Paragraph::new(lines).block(block).wrap(Wrap { trim: false }).scroll((scroll, 0));
    f.render_widget(p, outer[0]);

    let help_spans = vec![
        Span::styled(" ↑/↓", Style::default().fg(Color::Yellow)),
        Span::styled(" messages  ", Style::default().fg(Color::DarkGray)),
        Span::styled("c", Style::default().fg(Color::Yellow)),
        Span::styled(" copy  ", Style::default().fg(Color::DarkGray)),
        Span::styled("e", Style::default().fg(Color::Yellow)),
        Span::styled(" export  ", Style::default().fg(Color::DarkGray)),
        Span::styled("Esc", Style::default().fg(Color::Yellow)),
        Span::styled(" back  ", Style::default().fg(Color::DarkGray)),
        Span::styled("q", Style::default().fg(Color::Yellow)),
        Span::styled(" quit", Style::default().fg(Color::DarkGray)),
    ];

    let status_line = if let Some(ref msg) = app.status_message {
        Line::from(vec![Span::styled(format!(" {msg}"), Style::default().fg(Color::Green))])
    } else {
        Line::from(help_spans)
    };

    f.render_widget(Paragraph::new(status_line), outer[1]);
}

fn render_export_input(f: &mut Frame, app: &App) {
    let area = f.area();
    let popup_height = 3u16;
    let y = area.height.saturating_sub(popup_height + 1);
    let popup_area = Rect::new(area.x, y, area.width, popup_height);

    let block = Block::default()
        .title(" Export to (Enter confirm, Esc cancel) ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .style(Style::default().bg(Color::Black));

    let input = Paragraph::new(app.export_path.as_str())
        .style(Style::default().fg(Color::White).bg(Color::Black))
        .block(block);

    f.render_widget(Clear, popup_area);
    f.render_widget(input, popup_area);

    let cursor_x =
        popup_area.x + 1 + UnicodeWidthStr::width(&app.export_path[..app.export_cursor]) as u16;
    f.set_cursor_position((cursor_x.min(popup_area.right() - 2), y + 1));
}

fn render_settings(f: &mut Frame, app: &App) {
    let area = f.area();
    let width = area.width.min(70);
    let height = (app.all_sources.len() as u16 + 6).min(area.height.saturating_sub(2).max(6));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup = Rect::new(x, y, width, height);

    let block = Block::default()
        .title(" Settings (Enter/Space toggle, Esc close) ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .style(Style::default().bg(Color::Black));

    let mut lines = Vec::new();
    let selected_style = Style::default().bg(Color::Yellow).fg(Color::Black);
    let normal_style = Style::default().fg(Color::White);

    lines.push(Line::from(vec![
        Span::styled(" Time Scope: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            app.config.sync_window.label(),
            if app.settings_selected == 0 { selected_style } else { normal_style },
        ),
    ]));
    lines.push(Line::from(""));

    for (index, (source_id, label)) in app.all_sources.iter().enumerate() {
        let enabled = app.config.is_source_enabled(source_id);
        let prefix = if enabled { "[x]" } else { "[ ]" };
        let style = if app.settings_selected == index + 1 { selected_style } else { normal_style };
        lines.push(Line::from(Span::styled(format!(" {prefix} {label} ({source_id})"), style)));
    }

    let widget = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
    f.render_widget(Clear, popup);
    f.render_widget(widget, popup);
}

fn render_confirm_resume(f: &mut Frame, app: &App) {
    let Some(pending) = app.pending_resume.as_ref() else {
        return;
    };

    let area = f.area();
    let width = area.width.clamp(40, 76);
    let height: u16 = 9;
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup = Rect::new(x, y, width, height);

    let block = Block::default()
        .title(" Resume session ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .style(Style::default().bg(Color::Black));

    let title: String = pending.session_title.chars().take(width as usize - 10).collect();
    let command_text: String =
        pending.command.display().chars().take(width as usize - 14).collect();
    let cwd_text: String = pending
        .cwd
        .as_deref()
        .unwrap_or("-")
        .chars()
        .rev()
        .take(width as usize - 10)
        .collect::<String>()
        .chars()
        .rev()
        .collect();

    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(" Source:  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                pending.source_label.clone(),
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
            ),
            Span::raw("   "),
            Span::styled(title, Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled(" Cwd:     ", Style::default().fg(Color::DarkGray)),
            Span::styled(cwd_text, Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled(" Command: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                command_text,
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  [Y] ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::styled("confirm & exec     ", Style::default().fg(Color::White)),
            Span::styled("[N] ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::styled("cancel", Style::default().fg(Color::White)),
        ]),
    ];

    let widget = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
    f.render_widget(Clear, popup);
    f.render_widget(widget, popup);
}

fn render_status_bar(f: &mut Frame, app: &App, area: Rect) {
    let semantic_span = if app.semantic_progress.total_sessions > 0 {
        let mut text = format!(
            " [semantic {}/{}]",
            app.semantic_progress.done_sessions, app.semantic_progress.total_sessions
        );
        if app.semantic_progress.failed_sessions > 0 {
            text = format!(
                " [semantic {}/{}, {} failed]",
                app.semantic_progress.done_sessions,
                app.semantic_progress.total_sessions,
                app.semantic_progress.failed_sessions
            );
        }
        Some(Span::styled(text, Style::default().fg(Color::Blue)))
    } else {
        None
    };

    let stats_span = Span::styled(
        format!(" [{} sessions, {} messages]", app.total_sessions, app.total_messages),
        Style::default().fg(Color::DarkGray),
    );

    let line = if let Some(ref msg) = app.status_message {
        let mut spans = vec![Span::styled(format!(" {msg}"), Style::default().fg(Color::Green))];
        if let Some(span) = semantic_span.clone() {
            spans.push(span);
        }
        spans.push(stats_span);
        Line::from(spans)
    } else {
        match app.panel_focus {
            PanelFocus::SessionList => {
                let mut spans = vec![
                    Span::styled(" ↑/↓", Style::default().fg(Color::Yellow)),
                    Span::styled(" sessions  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("→", Style::default().fg(Color::Yellow)),
                    Span::styled(" preview  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("Enter", Style::default().fg(Color::Yellow)),
                    Span::styled(" detail  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("Tab", Style::default().fg(Color::Yellow)),
                    Span::styled(" filter  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("Ctrl+R", Style::default().fg(Color::Yellow)),
                    Span::styled(" resume  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("Ctrl+S", Style::default().fg(Color::Yellow)),
                    Span::styled(" settings  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("Esc", Style::default().fg(Color::Yellow)),
                    Span::styled(" clear  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("q", Style::default().fg(Color::Yellow)),
                    Span::styled(" quit", Style::default().fg(Color::DarkGray)),
                ];
                if let Some(span) = semantic_span.clone() {
                    spans.push(span);
                }
                spans.push(stats_span);
                Line::from(spans)
            }
            PanelFocus::Preview => {
                let mut spans = vec![
                    Span::styled(" ↑/↓", Style::default().fg(Color::Yellow)),
                    Span::styled(" messages  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("←", Style::default().fg(Color::Yellow)),
                    Span::styled(" sessions  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("Enter", Style::default().fg(Color::Yellow)),
                    Span::styled(" detail  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("Esc", Style::default().fg(Color::Yellow)),
                    Span::styled(" back", Style::default().fg(Color::DarkGray)),
                ];
                if let Some(span) = semantic_span {
                    spans.push(span);
                }
                spans.push(stats_span);
                Line::from(spans)
            }
        }
    };
    f.render_widget(Paragraph::new(line), area);
}
