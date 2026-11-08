use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::tui::app::App;
use crate::tui::search_state::PanelFocus;
use crate::tui::share_state::PendingCommandAction;
use crate::tui::theme::THEME;

use unicode_width::UnicodeWidthStr;

pub(super) fn render_export_input(f: &mut Frame, app: &App) {
    let area = f.area();
    let popup_height = 3u16;
    let y = area.height.saturating_sub(popup_height + 1);
    let popup_area = Rect::new(area.x, y, area.width, popup_height);

    let block = Block::default()
        .title(" Export to (Enter confirm, Esc cancel) ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(THEME.accent))
        .style(Style::default().bg(THEME.popup_bg));

    let input = Paragraph::new(app.export_path.as_str())
        .style(Style::default().fg(THEME.text).bg(THEME.popup_bg))
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
    let height = (app.all_sources.len() as u16 + 8).min(area.height.saturating_sub(2).max(7));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup = Rect::new(x, y, width, height);

    let block = Block::default()
        .title(" Settings (Enter/Space toggle, Esc close) ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(THEME.accent))
        .style(Style::default().bg(THEME.popup_bg));

    let mut lines = Vec::new();
    let selected_style = Style::default().bg(THEME.selected_bg).fg(THEME.selected_fg);
    let normal_style = Style::default().fg(THEME.text);

    lines.push(Line::from(vec![
        Span::styled(" Time Scope: ", Style::default().fg(THEME.text_muted)),
        Span::styled(
            app.config.sync_window.label(),
            if app.settings_selected == 0 { selected_style } else { normal_style },
        ),
    ]));
    lines.push(Line::from(Span::styled(
        if app.config.semantic_enabled {
            " Semantic: On"
        } else {
            " Semantic: Off (enable downloads ~465 MiB)"
        },
        if app.settings_selected == 1 { selected_style } else { normal_style },
    )));
    lines.push(Line::from(""));

    for (index, (source_id, label)) in app.all_sources.iter().enumerate() {
        let enabled = app.config.is_source_enabled(source_id);
        let prefix = if enabled { "[x]" } else { "[ ]" };
        let style = if app.settings_selected == index + 2 { selected_style } else { normal_style };
        lines.push(Line::from(Span::styled(format!(" {prefix} {label} ({source_id})"), style)));
    }

    let visible_lines = height.saturating_sub(2) as usize;
    let selected_line = app.settings_selected + usize::from(app.settings_selected >= 2);
    let scroll = selected_line.saturating_sub(visible_lines.saturating_sub(1)) as u16;
    let widget = Paragraph::new(lines).block(block).wrap(Wrap { trim: false }).scroll((scroll, 0));
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

    let border = if popup.is_error { THEME.error } else { THEME.success };
    let title = if popup.is_error { " Share failed " } else { " Session shared " };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border))
        .style(Style::default().bg(THEME.popup_bg));

    let mut lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            popup.message.clone(),
            if popup.is_error {
                Style::default().fg(THEME.error)
            } else {
                Style::default().fg(THEME.success).add_modifier(Modifier::BOLD)
            },
        )),
    ];
    if let Some(url) = popup.url.as_ref() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(url.clone(), Style::default().fg(THEME.highlight))));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(" [O] ", Style::default().fg(THEME.accent).add_modifier(Modifier::BOLD)),
            Span::styled("open   ", Style::default().fg(THEME.text)),
            Span::styled(" [C] ", Style::default().fg(THEME.accent).add_modifier(Modifier::BOLD)),
            Span::styled("copy URL   ", Style::default().fg(THEME.text)),
            Span::styled(
                "[Enter/Esc] ",
                Style::default().fg(THEME.accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled("close", Style::default().fg(THEME.text)),
        ]));
    } else if popup.is_error {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(
                " [Enter/Esc] ",
                Style::default().fg(THEME.accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled("close", Style::default().fg(THEME.text)),
        ]));
    } else {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            " Publishing with Wrangler...",
            Style::default().fg(THEME.text_muted),
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
        .border_style(Style::default().fg(THEME.accent))
        .style(Style::default().bg(THEME.popup_bg));

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
                Style::default().fg(THEME.text_muted),
            ),
            Span::styled(
                pending.source_label.clone(),
                Style::default().fg(THEME.source).add_modifier(Modifier::BOLD),
            ),
            Span::raw("   "),
            Span::styled(title, Style::default().fg(THEME.text)),
        ]),
        Line::from(vec![
            Span::styled(" Cwd:     ", Style::default().fg(THEME.text_muted)),
            Span::styled(cwd_text, Style::default().fg(THEME.text)),
        ]),
        Line::from(vec![
            Span::styled(" Command: ", Style::default().fg(THEME.text_muted)),
            Span::styled(
                command_text,
                Style::default().fg(THEME.highlight).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  [Y] ", Style::default().fg(THEME.accent).add_modifier(Modifier::BOLD)),
            Span::styled(confirm_label, Style::default().fg(THEME.text)),
            Span::styled("[N] ", Style::default().fg(THEME.accent).add_modifier(Modifier::BOLD)),
            Span::styled("cancel", Style::default().fg(THEME.text)),
        ]),
    ];

    let widget = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
    f.render_widget(Clear, popup);
    f.render_widget(widget, popup);
}

pub(super) fn render_status_bar(f: &mut Frame, app: &App, area: Rect) {
    let semantic_span = if !app.config.semantic_enabled {
        Some(Span::styled(" [semantic off]", Style::default().fg(THEME.text_muted)))
    } else if app.semantic_progress.total_sessions > 0 {
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
        Some(Span::styled(text, Style::default().fg(THEME.info)))
    } else {
        None
    };

    let stats_span = Span::styled(
        format!(" [{} sessions, {} messages]", app.total_sessions, app.total_messages),
        Style::default().fg(THEME.text_muted),
    );

    let line = if app.sync_requested || app.sync_in_flight {
        let mut spans = vec![Span::styled(" Syncing...", Style::default().fg(THEME.info))];
        if let Some(span) = semantic_span.clone() {
            spans.push(span);
        }
        spans.push(stats_span);
        Line::from(spans)
    } else if let Some(ref msg) = app.status_message {
        let mut spans = vec![Span::styled(format!(" {msg}"), Style::default().fg(THEME.success))];
        if let Some(span) = semantic_span.clone() {
            spans.push(span);
        }
        spans.push(stats_span);
        Line::from(spans)
    } else {
        match app.panel_focus {
            PanelFocus::SessionList => {
                let mut spans = vec![
                    Span::styled(" ↑/↓", Style::default().fg(THEME.accent)),
                    Span::styled(" sessions  ", Style::default().fg(THEME.text_muted)),
                    Span::styled("→", Style::default().fg(THEME.accent)),
                    Span::styled(" preview  ", Style::default().fg(THEME.text_muted)),
                    Span::styled("Enter", Style::default().fg(THEME.accent)),
                    Span::styled(" detail  ", Style::default().fg(THEME.text_muted)),
                    Span::styled("Ctrl+F", Style::default().fg(THEME.accent)),
                    Span::styled(" filter  ", Style::default().fg(THEME.text_muted)),
                    Span::styled("Ctrl+S", Style::default().fg(THEME.accent)),
                    Span::styled(" sync  ", Style::default().fg(THEME.text_muted)),
                    Span::styled("Ctrl+R", Style::default().fg(THEME.accent)),
                    Span::styled(" resume  ", Style::default().fg(THEME.text_muted)),
                    Span::styled("Ctrl+O", Style::default().fg(THEME.accent)),
                    Span::styled(" app  ", Style::default().fg(THEME.text_muted)),
                    Span::styled("Ctrl+P", Style::default().fg(THEME.accent)),
                    Span::styled(" settings  ", Style::default().fg(THEME.text_muted)),
                    Span::styled("Esc", Style::default().fg(THEME.accent)),
                    Span::styled(" clear  ", Style::default().fg(THEME.text_muted)),
                ];
                if app.query.is_empty() {
                    spans.push(Span::styled("q", Style::default().fg(THEME.accent)));
                    spans.push(Span::styled(" quit", Style::default().fg(THEME.text_muted)));
                }
                if let Some(span) = semantic_span.clone() {
                    spans.push(span);
                }
                spans.push(stats_span);
                Line::from(spans)
            }
            PanelFocus::Preview => {
                let mut spans = vec![
                    Span::styled(" ↑/↓", Style::default().fg(THEME.accent)),
                    Span::styled(" messages  ", Style::default().fg(THEME.text_muted)),
                    Span::styled("←", Style::default().fg(THEME.accent)),
                    Span::styled(" sessions  ", Style::default().fg(THEME.text_muted)),
                    Span::styled("Enter", Style::default().fg(THEME.accent)),
                    Span::styled(" detail  ", Style::default().fg(THEME.text_muted)),
                    Span::styled("Esc", Style::default().fg(THEME.accent)),
                    Span::styled(" back", Style::default().fg(THEME.text_muted)),
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
