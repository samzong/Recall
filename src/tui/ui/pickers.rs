use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::tui::app::App;
use crate::tui::search_state::{FilterFocus, ProjectPickerRow, SourcePickerRow};
use crate::tui::theme::THEME;

use super::truncate_start;

/// Truncate `s` to at most `max` terminal columns (CJK-aware), appending `…`.
fn truncate_to_width(s: &str, max: usize) -> String {
    if UnicodeWidthStr::width(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for ch in s.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + cw > max.saturating_sub(1) {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out.push('…');
    out
}

pub(super) fn render_subagents_picker(f: &mut Frame, app: &App) {
    let area = f.area();
    let width = area.width.saturating_sub(4).clamp(48, 96).min(area.width);
    let count = app.viewing.children.len();
    let desired_height = count as u16 + 5;
    let height = desired_height.clamp(8, area.height.saturating_sub(2).max(8));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let rect = Rect::new(x, y, width, height);

    let block = Block::default()
        .title(format!(" Subagents ({count}) "))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(THEME.accent))
        .style(Style::default().bg(THEME.popup_bg));

    // Inner width available for one row, minus the leading " > " marker.
    let inner = (width as usize).saturating_sub(2);
    let marker_w = 3;

    // Scroll the row window so the selected subagent stays visible. Rows share
    // the popup with the border (2), a top blank (1), and a blank + help (2).
    let visible_rows = (height as usize).saturating_sub(5).max(1);
    let start = if app.viewing.child_selected < visible_rows {
        0
    } else {
        app.viewing.child_selected + 1 - visible_rows
    };
    let end = (start + visible_rows).min(count);

    let mut lines = vec![Line::from("")];
    for (offset, child) in app.viewing.children[start..end].iter().enumerate() {
        let index = start + offset;
        let selected = index == app.viewing.child_selected;
        let marker = if selected { ">" } else { " " };
        let style = if selected {
            Style::default()
                .fg(THEME.selected_fg)
                .bg(THEME.selected_bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(THEME.text)
        };
        let meta = format!(
            "{} · {} · {} msgs",
            app.source_label_for(&child.source),
            crate::utils::format_age(child.started_at),
            child.message_count
        );
        let meta_w = UnicodeWidthStr::width(meta.as_str());
        let title_budget = inner.saturating_sub(marker_w + meta_w + 2);
        let title = truncate_to_width(&child.title, title_budget);
        let gap =
            inner.saturating_sub(marker_w + UnicodeWidthStr::width(title.as_str()) + meta_w).max(1);
        lines.push(Line::from(vec![
            Span::styled(format!(" {marker} "), style),
            Span::styled(title, style),
            Span::styled(" ".repeat(gap), style),
            Span::styled(
                meta,
                if selected { style } else { Style::default().fg(THEME.text_muted) },
            ),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(" [Enter] ", Style::default().fg(THEME.accent).add_modifier(Modifier::BOLD)),
        Span::styled("open  ", Style::default().fg(THEME.text)),
        Span::styled("[Esc] ", Style::default().fg(THEME.accent).add_modifier(Modifier::BOLD)),
        Span::styled("close", Style::default().fg(THEME.text)),
    ]));

    let widget = Paragraph::new(lines).block(block);
    f.render_widget(Clear, rect);
    f.render_widget(widget, rect);
}

pub(super) fn render_handoff_target_picker(f: &mut Frame, app: &App) {
    let area = f.area();
    let width = area.width.clamp(36, 56).min(area.width);
    let count = app.handoff_targets.len();
    let desired_height = count as u16 + 5;
    let max_height = area.height.max(1);
    let height = if max_height < 8 { max_height } else { desired_height.clamp(8, max_height) };
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let rect = Rect::new(x, y, width, height);

    let block = Block::default()
        .title(" Handoff target ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(THEME.accent))
        .style(Style::default().bg(THEME.popup_bg));

    let visible_rows = (height as usize).saturating_sub(5).max(1);
    let selected = app.handoff_target_selected.min(count.saturating_sub(1));
    let start = if count == 0 || selected < visible_rows { 0 } else { selected + 1 - visible_rows };
    let end = (start + visible_rows).min(count);

    let mut lines = vec![Line::from("")];
    for (offset, target) in app.handoff_targets[start..end].iter().enumerate() {
        let index = start + offset;
        let selected = index == app.handoff_target_selected;
        let marker = if selected { ">" } else { " " };
        let style = if selected {
            Style::default()
                .fg(THEME.selected_fg)
                .bg(THEME.selected_bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(THEME.text)
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {marker} "), style),
            Span::styled(target.label.as_str(), style),
            Span::styled(format!(" ({})", target.id), style),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(" [Enter] ", Style::default().fg(THEME.accent).add_modifier(Modifier::BOLD)),
        Span::styled("select  ", Style::default().fg(THEME.text)),
        Span::styled("[Esc] ", Style::default().fg(THEME.accent).add_modifier(Modifier::BOLD)),
        Span::styled("cancel", Style::default().fg(THEME.text)),
    ]));

    let widget = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
    f.render_widget(Clear, rect);
    f.render_widget(widget, rect);
}

pub(super) fn render_filter_picker(f: &mut Frame, app: &App) {
    let project = app.filters.editing == Some(FilterFocus::Project);
    let picker = app.filters.picker();
    let area = f.area();
    let (width, title, hint, empty) = if project {
        (area.width.min(92), " Filters > Project ", "type to filter paths", " No matching projects")
    } else {
        (area.width.min(76), " Filters > Source ", "press / to filter", " No sources")
    };
    let rows: Vec<Option<String>> = if project {
        app.project_picker_rows()
            .iter()
            .map(|row| match *row {
                ProjectPickerRow::All => {
                    let marker = if app.filters.project.is_none() { "(*)" } else { "( )" };
                    Some(format!(" {marker} All projects"))
                }
                ProjectPickerRow::Project(index) => {
                    app.project_directories.get(index).map(|project| {
                        let marker =
                            if app.filters.project.as_deref() == Some(project.directory.as_str()) {
                                "(*)"
                            } else {
                                "( )"
                            };
                        let path =
                            truncate_start(&project.directory, width.saturating_sub(30) as usize);
                        format!(" {marker} {path}  {}", project.sessions)
                    })
                }
            })
            .collect()
    } else {
        app.source_picker_rows()
            .iter()
            .map(|row| match *row {
                SourcePickerRow::All => {
                    let marker = if app.filters.sources.is_empty() { "(*)" } else { "( )" };
                    Some(format!(" {marker} All enabled sources"))
                }
                SourcePickerRow::Source(index) => {
                    app.all_sources.get(index).map(|(source_id, label)| {
                        let marker =
                            if app.source_is_selected_in_picker(source_id) { "[x]" } else { "[ ]" };
                        format!(" {marker} {label} ({source_id})")
                    })
                }
            })
            .collect()
    };
    let desired_height = rows.len() as u16 + 7;
    let height = desired_height.clamp(8, area.height.saturating_sub(2).max(8));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup = Rect::new(x, y, width, height);
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(THEME.border_focus));
    let selected_style = Style::default().bg(THEME.selected_bg).fg(THEME.selected_fg);
    let normal_style = Style::default().fg(THEME.text);
    let muted_style = Style::default().fg(THEME.text_muted);
    let visible_rows = height.saturating_sub(7) as usize;
    let start = if visible_rows == 0 || picker.selected < visible_rows {
        0
    } else {
        picker.selected + 1 - visible_rows
    };
    let end = (start + visible_rows).min(rows.len());
    let filter_value = if picker.query.is_empty() && !picker.typing {
        Span::styled(hint, muted_style)
    } else {
        Span::styled(picker.query.clone(), normal_style)
    };
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                " Filter: ",
                Style::default().fg(THEME.highlight).add_modifier(Modifier::BOLD),
            ),
            filter_value,
        ]),
        Line::from(""),
    ];
    if rows.is_empty() {
        lines.push(Line::from(Span::styled(empty, muted_style)));
    } else {
        for (offset, row) in rows[start..end].iter().enumerate() {
            if let Some(text) = row {
                let style =
                    if start + offset == picker.selected { selected_style } else { normal_style };
                lines.push(Line::from(Span::styled(text.clone(), style)));
            }
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(" Space", Style::default().fg(THEME.accent)),
        Span::styled(" select/clear  ", muted_style),
        Span::styled("/", Style::default().fg(THEME.accent)),
        Span::styled(" filter  ", muted_style),
        Span::styled("Enter", Style::default().fg(THEME.accent)),
        Span::styled(" apply  ", muted_style),
        Span::styled("Esc", Style::default().fg(THEME.accent)),
        Span::styled(" back", muted_style),
    ]));
    lines.push(Line::from(vec![
        Span::styled("Ctrl+A", Style::default().fg(THEME.accent)),
        Span::styled(" all  ", muted_style),
        Span::styled("Ctrl+U", Style::default().fg(THEME.accent)),
        Span::styled(" clear input", muted_style),
    ]));
    f.render_widget(Clear, popup);
    f.render_widget(Paragraph::new(lines).block(block).wrap(Wrap { trim: false }), popup);
    if picker.typing {
        let cursor_x = popup.x + 9 + UnicodeWidthStr::width(&picker.query[..picker.cursor]) as u16;
        f.set_cursor_position((cursor_x.min(popup.right().saturating_sub(2)), popup.y + 1));
    }
}
