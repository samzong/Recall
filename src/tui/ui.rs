use std::collections::BTreeMap;

use chrono::{Datelike, Duration, Local, NaiveDate};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::db::search::TimeRange;
use crate::tui::app::{
    App, AppMode, FilterFocus, PanelFocus, ResumeOrigin, SanitizedLine, SourcePickerRow,
};
use crate::types::{MatchSource, Role};
use crate::usage::{TokenTotals, UsageReport};

fn highlight_spans(text: &str, hay: &str, needle_lower: &str, base: Style) -> Vec<Span<'static>> {
    if needle_lower.is_empty() {
        return vec![Span::styled(text.to_string(), base)];
    }
    if hay.len() != text.len() {
        return vec![Span::styled(text.to_string(), base)];
    }
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cursor = 0usize;
    let match_style =
        Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD);
    while cursor < text.len() {
        match hay[cursor..].find(needle_lower) {
            Some(rel) => {
                let start = cursor + rel;
                let end = start + needle_lower.len();
                if !text.is_char_boundary(start) || !text.is_char_boundary(end) {
                    spans.push(Span::styled(text[cursor..].to_string(), base));
                    break;
                }
                if start > cursor {
                    spans.push(Span::styled(text[cursor..start].to_string(), base));
                }
                spans.push(Span::styled(text[start..end].to_string(), match_style));
                cursor = end;
            }
            None => {
                spans.push(Span::styled(text[cursor..].to_string(), base));
                break;
            }
        }
    }
    if spans.is_empty() {
        spans.push(Span::styled(text.to_string(), base));
    }
    spans
}

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
        AppMode::Usage => render_usage_dashboard(f, app),
        AppMode::Viewing => render_viewing(f, app),
        AppMode::Filters => {
            render_search(f, app);
            render_filter_picker(f, app);
        }
        AppMode::ExportInput => {
            render_viewing(f, app);
            render_export_input(f, app);
        }
        AppMode::Settings => {
            render_search(f, app);
            render_settings(f, app);
        }
        AppMode::ConfirmResume => {
            match app.pending_resume.as_ref().map(|p| p.origin) {
                Some(ResumeOrigin::Viewing) => render_viewing(f, app),
                _ => render_search(f, app),
            }
            render_confirm_resume(f, app);
        }
    }
}

fn render_usage_dashboard(f: &mut Frame, app: &App) {
    let area = f.area();
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Length(9),
            Constraint::Min(9),
            Constraint::Length(1),
        ])
        .split(area);

    render_usage_header(f, app, outer[0]);
    render_vibe_coding_map(f, app, outer[1]);

    let main = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(64), Constraint::Percentage(36)])
        .split(outer[2]);
    render_daily_token_chart(f, app, main[0]);
    render_usage_breakdown(f, app, main[1]);
    render_usage_status(f, outer[3]);
}

fn render_usage_header(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(" Recall Usage Dashboard ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let chip = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let muted = Style::default().fg(Color::DarkGray);

    let control = vec![
        Span::styled(" range ", muted),
        Span::styled(format!("[{}]", app.usage_time_label()), chip),
        Span::styled(" source ", muted),
        Span::styled(format!("[{}]", app.source_filter_label()), chip),
        Span::styled(" metric ", muted),
        Span::styled("[tokens]", chip),
    ];

    let mut lines = vec![Line::from(control)];

    if app.usage_is_loading() {
        lines.push(Line::from(Span::styled(
            " Loading usage data...",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
    } else if let Some(report) = app.usage_report.as_ref() {
        let active_days = report
            .daily
            .iter()
            .filter(|day| day.sessions > 0 || day.events > 0 || day.tokens.total_tokens > 0)
            .count();
        let top_source =
            report.by_source.first().map(|source| source.source.as_str()).unwrap_or("-");
        let top_model = report.by_model.first().map(|model| model.model.as_str()).unwrap_or("-");

        lines.push(Line::from(vec![
            Span::styled(" tokens ", muted),
            Span::styled(
                format_compact(report.summary.tokens.total_tokens),
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" sessions ", muted),
            Span::styled(
                format_count(report.summary.sessions),
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" events ", muted),
            Span::styled(format_count(report.summary.events), Style::default().fg(Color::White)),
            Span::styled(" active-days ", muted),
            Span::styled(format_count(active_days), Style::default().fg(Color::White)),
        ]));
        lines.push(Line::from(vec![
            Span::styled(" top-source ", muted),
            Span::styled(
                truncate_label(top_source, 18),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" top-model ", muted),
            Span::styled(truncate_label(top_model, 32), Style::default().fg(Color::White)),
        ]));
    } else if let Some(error) = app.usage_error.as_ref() {
        lines.push(Line::from(Span::styled(error.clone(), Style::default().fg(Color::Red))));
        lines.push(Line::from(""));
    } else {
        lines.push(Line::from(Span::styled("No usage data loaded", muted)));
        lines.push(Line::from(""));
    }

    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_vibe_coding_map(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(" Vibe Coding Map (tokens) ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Green));

    if app.usage_is_loading() {
        f.render_widget(
            Paragraph::new("Loading usage data...")
                .style(Style::default().fg(Color::Yellow))
                .block(block),
            area,
        );
        return;
    }

    let report = app.usage_year_report.as_ref().or(app.usage_report.as_ref());
    let Some(report) = report else {
        f.render_widget(
            Paragraph::new("No usage events")
                .style(Style::default().fg(Color::DarkGray))
                .block(block),
            area,
        );
        return;
    };

    let today = Local::now().date_naive();
    let start = today.checked_sub_signed(Duration::days(364)).unwrap_or(today);
    let grid_start = start
        .checked_sub_signed(Duration::days(start.weekday().num_days_from_monday() as i64))
        .unwrap_or(start);
    let total_days = today.signed_duration_since(grid_start).num_days().max(0) as usize + 1;
    let weeks = total_days.div_ceil(7);

    let inner_width = area.width.saturating_sub(2) as usize;
    let map_width = inner_width.saturating_sub(5).max(1);
    let visible_weeks = weeks.min(map_width);
    let first_col = weeks.saturating_sub(visible_weeks);
    let max_value = report.daily.iter().map(|day| day.tokens.total_tokens).max().unwrap_or(0);
    let values = daily_token_map(report);

    let mut lines = Vec::new();
    for (row, label) in ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"].iter().enumerate() {
        let mut cells = String::new();
        for (visible_col, col) in (first_col..weeks).enumerate() {
            let cell_width = distributed_width(visible_col, visible_weeks, map_width);
            let offset = (col * 7 + row) as i64;
            let Some(date) = grid_start.checked_add_signed(Duration::days(offset)) else {
                continue;
            };
            if date < start || date > today {
                cells.push_str(&" ".repeat(cell_width));
                continue;
            }
            let value = values.get(&date).copied().unwrap_or(0);
            cells.push_str(&heatmap_cell(value, max_value).to_string().repeat(cell_width));
        }
        lines.push(Line::from(vec![
            Span::styled(format!(" {label} "), Style::default().fg(Color::DarkGray)),
            Span::styled(cells, Style::default().fg(Color::Green)),
        ]));
    }

    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_daily_token_chart(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(" Daily Token Usage ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    if app.usage_is_loading() {
        f.render_widget(
            Paragraph::new("Loading usage data...")
                .style(Style::default().fg(Color::Yellow))
                .block(block),
            area,
        );
        return;
    }

    let Some(report) = app.usage_report.as_ref() else {
        f.render_widget(
            Paragraph::new("No token usage")
                .style(Style::default().fg(Color::DarkGray))
                .block(block),
            area,
        );
        return;
    };

    let inner_width = area.width.saturating_sub(2) as usize;
    let label_width = 8usize;
    let plot_width = inner_width.saturating_sub(label_width).max(1);
    let points = daily_token_points(report, app.usage_time_filter, plot_width);
    let max_tokens = points.iter().map(|(_, value)| *value).max().unwrap_or(0);
    if points.is_empty() || max_tokens == 0 {
        f.render_widget(
            Paragraph::new("No token usage in this range")
                .style(Style::default().fg(Color::DarkGray))
                .block(block),
            area,
        );
        return;
    }

    let chart_height = area.height.saturating_sub(5).max(1) as usize;
    let mut lines = Vec::new();
    for row in (1..=chart_height).rev() {
        let label = if row == chart_height {
            format!("{:>7}", format_compact(max_tokens))
        } else if row == 1 {
            "      0".to_string()
        } else {
            "       ".to_string()
        };
        let mut bars = String::with_capacity(plot_width);
        for (index, (_, value)) in points.iter().enumerate() {
            let day_width = distributed_width(index, points.len(), plot_width);
            if *value * chart_height as i64 >= max_tokens * row as i64 {
                bars.push_str(&"█".repeat(day_width));
            } else {
                bars.push_str(&" ".repeat(day_width));
            }
        }
        lines.push(Line::from(vec![
            Span::styled(format!("{label} "), Style::default().fg(Color::DarkGray)),
            Span::styled(bars, Style::default().fg(Color::Cyan)),
        ]));
    }

    if let (Some((first, _)), Some((last, _))) = (points.first(), points.last()) {
        lines.push(Line::from(vec![
            Span::styled("        ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                endpoint_labels(first, last, plot_width),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }

    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_usage_breakdown(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(" Breakdown ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow));

    if app.usage_is_loading() {
        f.render_widget(
            Paragraph::new("Loading usage data...")
                .style(Style::default().fg(Color::Yellow))
                .block(block),
            area,
        );
        return;
    }

    let Some(report) = app.usage_report.as_ref() else {
        f.render_widget(
            Paragraph::new("No usage data")
                .style(Style::default().fg(Color::DarkGray))
                .block(block),
            area,
        );
        return;
    };

    let inner_width = area.width.saturating_sub(2) as usize;
    let source_max =
        report.by_source.iter().map(|source| source.tokens.total_tokens).max().unwrap_or(0);
    let model_max =
        report.by_model.iter().map(|model| model.tokens.total_tokens).max().unwrap_or(0);
    let mut lines = vec![Line::from(Span::styled(
        " Sources",
        Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD),
    ))];

    for source in report.by_source.iter().take(5) {
        lines.push(usage_bar_line(
            app.source_label_for(&source.source),
            source.tokens.total_tokens,
            source_max,
            inner_width,
            Color::Cyan,
        ));
    }
    if report.by_source.is_empty() {
        lines.push(Line::from(Span::styled("  -", Style::default().fg(Color::DarkGray))));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " Models",
        Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD),
    )));
    for model in report.by_model.iter().take(6) {
        let label = format!("{}:{}", app.source_label_for(&model.source), model.model);
        lines.push(usage_bar_line(
            &label,
            model.tokens.total_tokens,
            model_max,
            inner_width,
            Color::Green,
        ));
    }
    if report.by_model.is_empty() {
        lines.push(Line::from(Span::styled("  -", Style::default().fg(Color::DarkGray))));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " Token Mix",
        Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD),
    )));
    lines.extend(token_mix_lines(&report.summary.tokens, inner_width));

    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_usage_status(f: &mut Frame, area: Rect) {
    let line = Line::from(vec![
        Span::styled("t", Style::default().fg(Color::Yellow)),
        Span::styled(" time  ", Style::default().fg(Color::DarkGray)),
        Span::styled("s", Style::default().fg(Color::Yellow)),
        Span::styled(" source  ", Style::default().fg(Color::DarkGray)),
        Span::styled("r", Style::default().fg(Color::Yellow)),
        Span::styled(" reset  ", Style::default().fg(Color::DarkGray)),
        Span::styled("Esc/q", Style::default().fg(Color::Yellow)),
        Span::styled(" quit", Style::default().fg(Color::DarkGray)),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn daily_token_map(report: &UsageReport) -> BTreeMap<NaiveDate, i64> {
    let mut values = BTreeMap::new();
    for day in &report.daily {
        if let Ok(date) = NaiveDate::parse_from_str(&day.period, "%Y-%m-%d") {
            values.insert(date, day.tokens.total_tokens);
        }
    }
    values
}

fn daily_token_points(
    report: &UsageReport,
    time_range: TimeRange,
    max_points: usize,
) -> Vec<(String, i64)> {
    if max_points == 0 {
        return Vec::new();
    }

    let values: BTreeMap<NaiveDate, i64> = report
        .daily
        .iter()
        .filter_map(|day| {
            NaiveDate::parse_from_str(&day.period, "%Y-%m-%d")
                .ok()
                .map(|date| (date, day.tokens.total_tokens))
        })
        .collect();

    match time_range {
        TimeRange::Today | TimeRange::Week | TimeRange::Month => {
            let days = match time_range {
                TimeRange::Today => 1,
                TimeRange::Week => 7,
                TimeRange::Month => 30,
                TimeRange::All => unreachable!(),
            };
            let today = Local::now().date_naive();
            let start = today.checked_sub_signed(Duration::days(days - 1)).unwrap_or(today);
            (0..days)
                .filter_map(|offset| start.checked_add_signed(Duration::days(offset)))
                .rev()
                .take(max_points)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .map(|date| {
                    let label = date.format("%m-%d").to_string();
                    let value = values.get(&date).copied().unwrap_or(0);
                    (label, value)
                })
                .collect()
        }
        TimeRange::All => values
            .into_iter()
            .rev()
            .take(max_points)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|(date, value)| (date.format("%m-%d").to_string(), value))
            .collect(),
    }
}

fn heatmap_cell(value: i64, max_value: i64) -> char {
    if value <= 0 || max_value <= 0 {
        return '·';
    }
    let ratio = value as f64 / max_value as f64;
    if ratio < 0.25 {
        '░'
    } else if ratio < 0.5 {
        '▒'
    } else if ratio < 0.75 {
        '▓'
    } else {
        '█'
    }
}

fn distributed_width(index: usize, item_count: usize, total_width: usize) -> usize {
    if item_count == 0 || total_width == 0 {
        return 0;
    }
    let base = total_width / item_count;
    let remainder = total_width % item_count;
    base + usize::from(index < remainder)
}

fn endpoint_labels(first: &str, last: &str, width: usize) -> String {
    let mut chars = vec![' '; width];
    for (index, ch) in first.chars().take(width).enumerate() {
        chars[index] = ch;
    }
    let last_len = last.chars().count().min(width);
    let start = width.saturating_sub(last_len);
    for (offset, ch) in last.chars().take(last_len).enumerate() {
        chars[start + offset] = ch;
    }
    chars.into_iter().collect()
}

fn usage_bar_line(
    label: &str,
    value: i64,
    max_value: i64,
    width: usize,
    color: Color,
) -> Line<'static> {
    let label_width = width.clamp(18, 24);
    let value_text = format_compact(value);
    let bar_width = width.saturating_sub(label_width + value_text.len() + 4).max(4);
    let filled = if max_value > 0 {
        ((value as f64 / max_value as f64) * bar_width as f64).round() as usize
    } else {
        0
    }
    .min(bar_width);
    let bar = format!("{}{}", "█".repeat(filled), "░".repeat(bar_width - filled));

    Line::from(vec![
        Span::styled(
            format!(" {:<label_width$}", truncate_label(label, label_width)),
            Style::default().fg(Color::White),
        ),
        Span::styled(bar, Style::default().fg(color)),
        Span::styled(format!(" {value_text}"), Style::default().fg(Color::DarkGray)),
    ])
}

fn token_mix_lines(tokens: &TokenTotals, width: usize) -> Vec<Line<'static>> {
    let max_value = [
        tokens.input_tokens,
        tokens.output_tokens,
        tokens.cache_read_tokens,
        tokens.cache_write_tokens,
        tokens.reasoning_tokens,
    ]
    .into_iter()
    .max()
    .unwrap_or(0);

    [
        ("input", tokens.input_tokens, Color::Cyan),
        ("output", tokens.output_tokens, Color::Green),
        ("cache read", tokens.cache_read_tokens, Color::Blue),
        ("cache write", tokens.cache_write_tokens, Color::Magenta),
        ("reasoning", tokens.reasoning_tokens, Color::Yellow),
    ]
    .into_iter()
    .map(|(label, value, color)| usage_bar_line(label, value, max_value, width, color))
    .collect()
}

fn truncate_label(label: &str, max_chars: usize) -> String {
    if label.chars().count() <= max_chars {
        return label.to_string();
    }
    if max_chars <= 1 {
        return "…".to_string();
    }
    format!("{}…", label.chars().take(max_chars - 1).collect::<String>())
}

fn format_count(value: usize) -> String {
    format_compact(value as i64)
}

fn format_compact(value: i64) -> String {
    let abs = value.abs() as f64;
    if abs >= 1_000_000_000.0 {
        format!("{:.2}B", value as f64 / 1_000_000_000.0)
    } else if abs >= 1_000_000.0 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else if abs >= 1_000.0 {
        format!("{:.1}K", value as f64 / 1_000.0)
    } else {
        value.to_string()
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
    let time_label = app.time_filter_label();
    let sort_label = app.sort_label();

    let line = Line::from(vec![
        Span::styled("  Scope: Source ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("[{source_label}]"),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  Time ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("[{time_label}]"),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  Sort ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("[{sort_label}]"),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  (Ctrl+F)", Style::default().fg(Color::DarkGray)),
    ]);

    f.render_widget(Paragraph::new(line), area);
}

fn render_filter_picker(f: &mut Frame, app: &App) {
    if app.filters_editing_source {
        render_source_picker(f, app);
    } else {
        render_filter_overview(f, app);
    }
}

fn render_filter_overview(f: &mut Frame, app: &App) {
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
        &app.source_filter_label(),
        "Enter",
        app.filter_focus == FilterFocus::Source,
    ));
    lines.push(filter_overview_line(
        "Time Range",
        app.time_filter_label(),
        "d/w/m/l",
        app.filter_focus == FilterFocus::Time,
    ));
    lines.push(filter_overview_line(
        "Sort",
        app.sort_label(),
        "r/n",
        app.filter_focus == FilterFocus::Sort,
    ));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(" ↑/↓", Style::default().fg(Color::Yellow)),
        Span::styled(" navigate  ", Style::default().fg(Color::DarkGray)),
        Span::styled("Enter/Space", Style::default().fg(Color::Yellow)),
        Span::styled(" edit/toggle  ", Style::default().fg(Color::DarkGray)),
        Span::styled("c", Style::default().fg(Color::Yellow)),
        Span::styled(" clear  ", Style::default().fg(Color::DarkGray)),
        Span::styled("Esc", Style::default().fg(Color::Yellow)),
        Span::styled(" close", Style::default().fg(Color::DarkGray)),
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
    Line::from(Span::styled(format!(" {label:<12} {value:<22} {hint}"), style))
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

        let time_str = crate::utils::format_message_time(msg.timestamp);
        let mut header = vec![Span::styled(
            prefix,
            Style::default().fg(color).bg(bg).add_modifier(Modifier::BOLD),
        )];
        if !time_str.is_empty() {
            header.push(Span::styled(time_str, Style::default().fg(Color::DarkGray).bg(bg)));
        }
        lines.push(Line::from(header));

        let text: String = msg.content.chars().take(300).collect();
        for line in text.lines().take(6) {
            let line = crate::utils::sanitize_line(line);
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
    let needle_lower = app.viewing_search_query.to_lowercase();

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

        let time_str = crate::utils::format_message_time(msg.timestamp);
        let mut header = vec![Span::styled(
            format!("── {prefix} ──"),
            Style::default().fg(color).bg(bg).add_modifier(Modifier::BOLD),
        )];
        if !time_str.is_empty() {
            header.push(Span::styled(
                format!("  {time_str}"),
                Style::default().fg(Color::DarkGray).bg(bg),
            ));
        }
        lines.push(Line::from(header));

        let body_style = Style::default().fg(Color::White).bg(bg);
        let empty: Vec<SanitizedLine> = Vec::new();
        let cached_lines = app.viewing_sanitized_lines.get(i).unwrap_or(&empty);
        for sl in cached_lines {
            let spans = highlight_spans(&sl.text, &sl.lower, &needle_lower, body_style);
            lines.push(Line::from(spans));
        }
        lines.push(Line::from(""));
    }

    let scroll = scroll_offset(selected_line_start, block.inner(outer[0]).height as usize);

    let p = Paragraph::new(lines).block(block).wrap(Wrap { trim: false }).scroll((scroll, 0));
    f.render_widget(p, outer[0]);

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
        Span::styled("Ctrl+R", Style::default().fg(Color::Yellow)),
        Span::styled(" resume  ", Style::default().fg(Color::DarkGray)),
        Span::styled("Esc", Style::default().fg(Color::Yellow)),
        Span::styled(" back  ", Style::default().fg(Color::DarkGray)),
        Span::styled("q", Style::default().fg(Color::Yellow)),
        Span::styled(" quit", Style::default().fg(Color::DarkGray)),
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
        let cursor_x = outer[1].x + 2 + UnicodeWidthStr::width(&input[..cursor_byte]) as u16;
        f.set_cursor_position((cursor_x, outer[1].y));
    }

    f.render_widget(Paragraph::new(status_line), outer[1]);
}

fn render_source_picker(f: &mut Frame, app: &App) {
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
                    format!(" {marker}      All enabled sources")
                }
                SourcePickerRow::Source(index) => {
                    let Some((source_id, label)) = app.all_sources.get(index) else {
                        continue;
                    };
                    let marker =
                        if app.source_is_selected_in_picker(source_id) { "[x]" } else { "[ ]" };
                    match app.source_picker_row_key(row_index) {
                        Some(key) => format!(" {marker} [{key}] {label} ({source_id})"),
                        None => format!(" {marker}     {label} ({source_id})"),
                    }
                }
            };
            lines.push(Line::from(Span::styled(text, style)));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(" 1-9/0", Style::default().fg(Color::Yellow)),
        Span::styled(" toggle  ", muted_style),
        Span::styled("/", Style::default().fg(Color::Yellow)),
        Span::styled(" filter  ", muted_style),
        Span::styled("Space", Style::default().fg(Color::Yellow)),
        Span::styled(" current  ", muted_style),
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
                    Span::styled("Ctrl+F", Style::default().fg(Color::Yellow)),
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
