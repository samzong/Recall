use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::tui::app::App;
use crate::tui::layout::viewing_layout;
use crate::tui::text_layout::wrap_spans_to_lines;
use crate::tui::viewing_state::SanitizedLine;
use crate::types::Role;

use super::{
    format_compact, highlight_spans, line_with_background, render_vertical_scrollbar, row_visible,
    truncate_label,
};

pub(super) fn render_viewing(f: &mut Frame, app: &App) {
    let layout = viewing_layout(f.area());

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
    f.render_widget(block, layout.content);

    let inner_width = layout.messages.width as usize;
    let pane = app.viewing_pane(inner_width);
    let viewport_start = pane.scroll_start(
        app.viewing_scroll_offset,
        app.viewing_selected_msg,
        layout.messages.height as usize,
    );
    let viewport_end = viewport_start + layout.messages.height as usize;
    let mut visual_row = 0usize;
    let mut lines: Vec<Line> = Vec::new();
    let needles = app.viewing_search_terms();

    for (i, msg) in app.viewing_messages.iter().enumerate() {
        let selected = i == app.viewing_selected_msg;
        let (prefix, color) = match msg.role {
            Role::User => ("User", Color::Cyan),
            Role::Assistant => ("Assistant", Color::Green),
        };

        let time_str = crate::utils::format_message_time(msg.timestamp);
        let header_bg = if selected && row_visible(visual_row, viewport_start, viewport_end) {
            Color::DarkGray
        } else {
            Color::Reset
        };
        let mut header = vec![Span::styled(
            format!("── {prefix} ──"),
            Style::default().fg(color).bg(header_bg).add_modifier(Modifier::BOLD),
        )];
        if !time_str.is_empty() {
            header.push(Span::styled(
                format!("  {time_str}"),
                Style::default().fg(Color::DarkGray).bg(header_bg),
            ));
        }
        lines.push(Line::from(header));
        visual_row += 1;

        let empty: Vec<SanitizedLine> = Vec::new();
        let cached_lines = app.viewing_sanitized_lines.get(i).unwrap_or(&empty);
        for sl in cached_lines {
            let body_style = Style::default().fg(Color::White);
            let spans = highlight_spans(&sl.text, &sl.lower, &needles, body_style);
            for line in wrap_spans_to_lines(spans, inner_width) {
                let body_bg = if selected && row_visible(visual_row, viewport_start, viewport_end) {
                    Color::DarkGray
                } else {
                    Color::Reset
                };
                lines.push(line_with_background(line, body_bg));
                visual_row += 1;
            }
        }
        lines.push(Line::from(""));
        visual_row += 1;
    }

    render_viewing_summary(f, app, layout.summary);
    let p = Paragraph::new(lines).scroll((viewport_start as u16, 0));
    f.render_widget(p, layout.messages);
    render_vertical_scrollbar(
        f,
        layout.scrollbar_area(),
        pane.total_rows(),
        layout.messages.height as usize,
        viewport_start,
    );

    let help_spans = vec![
        Span::styled(" ↑/↓", Style::default().fg(Color::Yellow)),
        Span::styled(" messages  ", Style::default().fg(Color::DarkGray)),
        Span::styled("/", Style::default().fg(Color::Yellow)),
        Span::styled(" find  ", Style::default().fg(Color::DarkGray)),
        Span::styled("n/N", Style::default().fg(Color::Yellow)),
        Span::styled(" next/prev  ", Style::default().fg(Color::DarkGray)),
        Span::styled("c", Style::default().fg(Color::Yellow)),
        Span::styled(" copy  ", Style::default().fg(Color::DarkGray)),
        Span::styled("e", Style::default().fg(Color::Yellow)),
        Span::styled(" export  ", Style::default().fg(Color::DarkGray)),
        Span::styled("s", Style::default().fg(Color::Yellow)),
        Span::styled(" share  ", Style::default().fg(Color::DarkGray)),
        Span::styled("h", Style::default().fg(Color::Yellow)),
        Span::styled(" handoff  ", Style::default().fg(Color::DarkGray)),
        Span::styled("Ctrl+R", Style::default().fg(Color::Yellow)),
        Span::styled(" resume  ", Style::default().fg(Color::DarkGray)),
        Span::styled("Ctrl+O", Style::default().fg(Color::Yellow)),
        Span::styled(" app  ", Style::default().fg(Color::DarkGray)),
        Span::styled("Esc/q", Style::default().fg(Color::Yellow)),
        Span::styled(" back", Style::default().fg(Color::DarkGray)),
    ];

    let status_line = if let Some(ref input) = app.viewing_search_input {
        Line::from(vec![
            Span::styled(" /", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::styled(input.clone(), Style::default().fg(Color::White)),
        ])
    } else if let Some(ref msg) = app.status_message {
        Line::from(vec![Span::styled(format!(" {msg}"), Style::default().fg(Color::Green))])
    } else if let Some(ref note) = app.viewing_search_status {
        Line::from(vec![Span::styled(
            format!(" {note}: \"{}\"", app.viewing_search_query),
            Style::default().fg(Color::Red),
        )])
    } else if !app.viewing_search_query.is_empty() {
        let matches = app.viewing_match_indices();
        let total = matches.len();
        let current_pos =
            matches.iter().position(|&i| i == app.viewing_selected_msg).map(|n| n + 1).unwrap_or(0);
        let mut spans = help_spans.clone();
        spans.push(Span::styled(
            format!("  [{current_pos}/{total} \"{}\"]", app.viewing_search_query),
            Style::default().fg(Color::Yellow),
        ));
        Line::from(spans)
    } else {
        Line::from(help_spans)
    };

    if let Some(ref input) = app.viewing_search_input {
        let cursor_byte = app.viewing_search_input_cursor.min(input.len());
        let cursor_x = layout.help.x + 2 + UnicodeWidthStr::width(&input[..cursor_byte]) as u16;
        f.set_cursor_position((cursor_x, layout.help.y));
    }

    f.render_widget(Paragraph::new(status_line), layout.help);
}
pub(super) fn render_viewing_summary(f: &mut Frame, app: &App, area: Rect) {
    let text = viewing_summary_text(app, area.width as usize);
    let line = Line::from(Span::styled(text, Style::default().fg(Color::Green)));
    f.render_widget(Paragraph::new(line), area);
}

fn viewing_summary_text(app: &App, width: usize) -> String {
    let Some(summary) = app.viewing_session_summary.as_ref() else {
        return fit_summary_text(vec![" tokens - | time - | user msgs -".to_string()], width);
    };

    let duration = format_duration_minutes(summary.duration_minutes);
    let user_messages = format!("{}/{}", summary.user_messages, summary.total_messages);

    if summary.usage_events == 0 {
        return fit_summary_text(
            vec![
                format!(" tokens - | time {duration} | user msgs {user_messages}"),
                format!(" tok - | {duration} | user {user_messages}"),
            ],
            width,
        );
    }

    let tokens = &summary.tokens;
    let total = format_compact(tokens.total_tokens);
    let input = format_compact(tokens.input_tokens);
    let output = format_compact(tokens.output_tokens);
    let cache_read = format_compact(tokens.cache_read_tokens);
    let cache_write = format_compact(tokens.cache_write_tokens);
    let reasoning = format_compact(tokens.reasoning_tokens);

    fit_summary_text(
        vec![
            format!(
                " tokens {total} input {input} output {output} cache r/w {cache_read}/{cache_write} reasoning {reasoning} | time {duration} | user msgs {user_messages}"
            ),
            format!(
                " tok {total} in {input} out {output} cache {cache_read}/{cache_write} reason {reasoning} | {duration} | user {user_messages}"
            ),
            format!(" tok {total} | time {duration} | user {user_messages}"),
        ],
        width,
    )
}

fn fit_summary_text(variants: Vec<String>, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    for variant in &variants {
        if UnicodeWidthStr::width(variant.as_str()) <= width {
            return variant.clone();
        }
    }
    truncate_label(variants.last().map(String::as_str).unwrap_or(""), width)
}

fn format_duration_minutes(minutes: Option<u32>) -> String {
    let Some(minutes) = minutes else {
        return "-".to_string();
    };
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let hours = minutes / 60;
    let remaining = minutes % 60;
    if remaining == 0 { format!("{hours}h") } else { format!("{hours}h{remaining}m") }
}
