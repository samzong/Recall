use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::handoff;
use crate::tui::app::App;
use crate::tui::search_state::{PanelFocus, ProjectPickerRow, SourcePickerRow};
use crate::tui::share_state::PendingCommandAction;

use super::truncate_start;

pub(super) fn render_handoff_target_picker(f: &mut Frame, app: &App) {
    let area = f.area();
    let width = area.width.clamp(36, 56);
    let height = (handoff::TARGETS.len() as u16 + 5).max(8);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let rect = Rect::new(x, y, width, height);

    let block = Block::default()
        .title(" Handoff target ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .style(Style::default().bg(Color::Black));

    let mut lines = Vec::new();
    lines.push(Line::from(""));
    for (index, target) in handoff::TARGETS.iter().enumerate() {
        let selected = index == app.handoff_target_selected;
        let marker = if selected { ">" } else { " " };
        let style = if selected {
            Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {marker} "), style),
            Span::styled(target.label, style),
            Span::styled(format!(" ({})", target.id), style),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(" [Enter] ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        Span::styled("select  ", Style::default().fg(Color::White)),
        Span::styled("[Esc] ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        Span::styled("cancel", Style::default().fg(Color::White)),
    ]));

    let widget = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
    f.render_widget(Clear, rect);
    f.render_widget(widget, rect);
}

pub(super) fn render_source_picker(f: &mut Frame, app: &App) {
    let area = f.area();
    let width = area.width.min(76);
    let rows = app.source_picker_rows();
    let desired_height = rows.len() as u16 + 7;
    let height = desired_height.clamp(8, area.height.saturating_sub(2).max(8));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup = Rect::new(x, y, width, height);

    let block = Block::default()
        .title(" Filters > Source ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let selected_style = Style::default().bg(Color::Cyan).fg(Color::Black);
    let normal_style = Style::default().fg(Color::White);
    let muted_style = Style::default().fg(Color::DarkGray);

    let visible_rows = height.saturating_sub(7) as usize;
    let start = if visible_rows == 0 || app.source_picker_selected < visible_rows {
        0
    } else {
        app.source_picker_selected + 1 - visible_rows
    };
    let end = (start + visible_rows).min(rows.len());

    let mut lines = Vec::new();
    let filter_value = if app.source_picker_query.is_empty() && !app.source_picker_typing {
        Span::styled("press / to filter", muted_style)
    } else {
        Span::styled(app.source_picker_query.clone(), normal_style)
    };
    lines.push(Line::from(vec![
        Span::styled(" Filter: ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        filter_value,
    ]));
    lines.push(Line::from(""));

    if rows.is_empty() {
        lines.push(Line::from(Span::styled(" No sources", muted_style)));
    } else {
        for (offset, row) in rows[start..end].iter().enumerate() {
            let row_index = start + offset;
            let style =
                if row_index == app.source_picker_selected { selected_style } else { normal_style };

            let text = match *row {
                SourcePickerRow::All => {
                    let marker = if app.source_picker_selection.is_empty() { "(*)" } else { "( )" };
                    format!(" {marker} All enabled sources")
                }
                SourcePickerRow::Source(index) => {
                    let Some((source_id, label)) = app.all_sources.get(index) else {
                        continue;
                    };
                    let marker =
                        if app.source_is_selected_in_picker(source_id) { "[x]" } else { "[ ]" };
                    format!(" {marker} {label} ({source_id})")
                }
            };
            lines.push(Line::from(Span::styled(text, style)));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(" Space", Style::default().fg(Color::Yellow)),
        Span::styled(" select/clear  ", muted_style),
        Span::styled("/", Style::default().fg(Color::Yellow)),
        Span::styled(" filter  ", muted_style),
        Span::styled("Enter", Style::default().fg(Color::Yellow)),
        Span::styled(" apply  ", muted_style),
        Span::styled("Esc", Style::default().fg(Color::Yellow)),
        Span::styled(" back", muted_style),
    ]));
    lines.push(Line::from(vec![
        Span::styled("Ctrl+A", Style::default().fg(Color::Yellow)),
        Span::styled(" all  ", muted_style),
        Span::styled("Ctrl+U", Style::default().fg(Color::Yellow)),
        Span::styled(" clear input", muted_style),
    ]));

    let widget = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
    f.render_widget(Clear, popup);
    f.render_widget(widget, popup);

    if app.source_picker_typing {
        let cursor_x = popup.x
            + 9
            + UnicodeWidthStr::width(&app.source_picker_query[..app.source_picker_cursor]) as u16;
        f.set_cursor_position((cursor_x.min(popup.right().saturating_sub(2)), popup.y + 1));
    }
}

pub(super) fn render_project_picker(f: &mut Frame, app: &App) {
    let area = f.area();
    let width = area.width.min(92);
    let rows = app.project_picker_rows();
    let desired_height = rows.len() as u16 + 7;
    let height = desired_height.clamp(8, area.height.saturating_sub(2).max(8));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup = Rect::new(x, y, width, height);

    let block = Block::default()
        .title(" Filters > Project ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let selected_style = Style::default().bg(Color::Cyan).fg(Color::Black);
    let normal_style = Style::default().fg(Color::White);
    let muted_style = Style::default().fg(Color::DarkGray);

    let visible_rows = height.saturating_sub(7) as usize;
    let start = if visible_rows == 0 || app.project_picker_selected < visible_rows {
        0
    } else {
        app.project_picker_selected + 1 - visible_rows
    };
    let end = (start + visible_rows).min(rows.len());

    let mut lines = Vec::new();
    let filter_value = if app.project_picker_query.is_empty() && !app.project_picker_typing {
        Span::styled("type to filter paths", muted_style)
    } else {
        Span::styled(app.project_picker_query.clone(), normal_style)
    };
    lines.push(Line::from(vec![
        Span::styled(" Filter: ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        filter_value,
    ]));
    lines.push(Line::from(""));

    if rows.is_empty() {
        lines.push(Line::from(Span::styled(" No matching projects", muted_style)));
    } else {
        let path_width = width.saturating_sub(30) as usize;
        for (offset, row) in rows[start..end].iter().enumerate() {
            let row_index = start + offset;
            let style = if row_index == app.project_picker_selected {
                selected_style
            } else {
                normal_style
            };

            let text = match *row {
                ProjectPickerRow::All => {
                    let marker = if app.project_picker_selection.is_none() { "(*)" } else { "( )" };
                    format!(" {marker} All projects")
                }
                ProjectPickerRow::Project(index) => {
                    let Some(project) = app.project_directories.get(index) else {
                        continue;
                    };
                    let marker = if app.project_picker_selection.as_deref()
                        == Some(project.directory.as_str())
                    {
                        "(*)"
                    } else {
                        "( )"
                    };
                    let path = truncate_start(&project.directory, path_width);
                    format!(" {marker} {path}  {}", project.sessions)
                }
            };
            lines.push(Line::from(Span::styled(text, style)));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(" Space", Style::default().fg(Color::Yellow)),
        Span::styled(" select/clear  ", muted_style),
        Span::styled("/", Style::default().fg(Color::Yellow)),
        Span::styled(" filter  ", muted_style),
        Span::styled("Enter", Style::default().fg(Color::Yellow)),
        Span::styled(" apply  ", muted_style),
        Span::styled("Esc", Style::default().fg(Color::Yellow)),
        Span::styled(" back", muted_style),
    ]));
    lines.push(Line::from(vec![
        Span::styled("Ctrl+A", Style::default().fg(Color::Yellow)),
        Span::styled(" all  ", muted_style),
        Span::styled("Ctrl+U", Style::default().fg(Color::Yellow)),
        Span::styled(" clear input", muted_style),
    ]));

    let widget = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
    f.render_widget(Clear, popup);
    f.render_widget(widget, popup);

    if app.project_picker_typing {
        let cursor_x = popup.x
            + 9
            + UnicodeWidthStr::width(&app.project_picker_query[..app.project_picker_cursor]) as u16;
        f.set_cursor_position((cursor_x.min(popup.right().saturating_sub(2)), popup.y + 1));
    }
}

pub(super) fn render_export_input(f: &mut Frame, app: &App) {
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

pub(super) fn render_settings(f: &mut Frame, app: &App) {
    let area = f.area();
    let width = area.width.min(70);
    let height = (app.all_sources.len() as u16 + 7).min(area.height.saturating_sub(2).max(7));
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
    let prefix = if app.config.default_current_repo_scope { "[x]" } else { "[ ]" };
    let style = if app.settings_selected == 1 { selected_style } else { normal_style };
    lines.push(Line::from(Span::styled(format!(" {prefix} Default Current Repo"), style)));
    lines.push(Line::from(""));

    for (index, (source_id, label)) in app.all_sources.iter().enumerate() {
        let enabled = app.config.is_source_enabled(source_id);
        let prefix = if enabled { "[x]" } else { "[ ]" };
        let style = if app.settings_selected == index + 2 { selected_style } else { normal_style };
        lines.push(Line::from(Span::styled(format!(" {prefix} {label} ({source_id})"), style)));
    }

    let widget = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
    f.render_widget(Clear, popup);
    f.render_widget(widget, popup);
}

pub(super) fn render_share_result(f: &mut Frame, app: &App) {
    let Some(popup) = app.share_popup.as_ref() else {
        return;
    };

    let area = f.area();
    let width = area.width.clamp(46, 88);
    let height: u16 = if popup.url.is_some() { 9 } else { 8 };
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let rect = Rect::new(x, y, width, height);

    let border = if popup.is_error { Color::Red } else { Color::Green };
    let title = if popup.is_error { " Share failed " } else { " Session shared " };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border))
        .style(Style::default().bg(Color::Black));

    let mut lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            popup.message.clone(),
            if popup.is_error {
                Style::default().fg(Color::Red)
            } else {
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
            },
        )),
    ];
    if let Some(url) = popup.url.as_ref() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(url.clone(), Style::default().fg(Color::Cyan))));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(" [O] ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::styled("open   ", Style::default().fg(Color::White)),
            Span::styled(" [C] ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::styled("copy URL   ", Style::default().fg(Color::White)),
            Span::styled(
                "[Enter/Esc] ",
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            Span::styled("close", Style::default().fg(Color::White)),
        ]));
    } else if popup.is_error {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(
                " [Enter/Esc] ",
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            Span::styled("close", Style::default().fg(Color::White)),
        ]));
    } else {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            " Publishing with Wrangler...",
            Style::default().fg(Color::DarkGray),
        )));
    }

    let widget = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
    f.render_widget(Clear, rect);
    f.render_widget(widget, rect);
}

pub(super) fn render_confirm_resume(f: &mut Frame, app: &App) {
    let Some(pending) = app.pending_resume.as_ref() else {
        return;
    };

    let area = f.area();
    let width = area.width.clamp(40, 76);
    let height: u16 = 9;
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup = Rect::new(x, y, width, height);

    let block_title = match pending.action {
        PendingCommandAction::Resume => " Resume session ",
        PendingCommandAction::OpenApp => " Open in app ",
        PendingCommandAction::Handoff => " Handoff session ",
    };
    let confirm_label = match pending.action {
        PendingCommandAction::Resume => "confirm & exec     ",
        PendingCommandAction::OpenApp => "confirm & open     ",
        PendingCommandAction::Handoff => "confirm & handoff  ",
    };

    let block = Block::default()
        .title(block_title)
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
            Span::styled(
                match pending.action {
                    PendingCommandAction::Handoff => " Target:  ",
                    _ => " Source:  ",
                },
                Style::default().fg(Color::DarkGray),
            ),
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
            Span::styled(confirm_label, Style::default().fg(Color::White)),
            Span::styled("[N] ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::styled("cancel", Style::default().fg(Color::White)),
        ]),
    ];

    let widget = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
    f.render_widget(Clear, popup);
    f.render_widget(widget, popup);
}

pub(super) fn render_status_bar(f: &mut Frame, app: &App, area: Rect) {
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
                    Span::styled("Ctrl+F", Style::default().fg(Color::Yellow)),
                    Span::styled(" filter  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("Ctrl+R", Style::default().fg(Color::Yellow)),
                    Span::styled(" resume  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("Ctrl+O", Style::default().fg(Color::Yellow)),
                    Span::styled(" app  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("Ctrl+S", Style::default().fg(Color::Yellow)),
                    Span::styled(" settings  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("Esc", Style::default().fg(Color::Yellow)),
                    Span::styled(" clear  ", Style::default().fg(Color::DarkGray)),
                ];
                if app.query.is_empty() {
                    spans.push(Span::styled("q", Style::default().fg(Color::Yellow)));
                    spans.push(Span::styled(" quit", Style::default().fg(Color::DarkGray)));
                }
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
