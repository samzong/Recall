use std::collections::BTreeMap;

use chrono::{Datelike, Duration, Local, NaiveDate};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::db::search::TimeRange;
use crate::skill_audit::{SkillTier, SkillUsageEntry, format_last_used, format_signals};
use crate::tui::app::App;
use crate::tui::usage_state::UsageTab;
use crate::usage::{TokenTotals, UsageReport};

use super::{format_compact, format_count, truncate_label};

pub(super) fn render_usage_dashboard(f: &mut Frame, app: &App) {
    match app.usage_tab {
        UsageTab::Tokens => render_tokens_dashboard(f, app),
        UsageTab::Skills => render_skill_audit_dashboard(f, app),
    }
}

pub(super) fn render_tokens_dashboard(f: &mut Frame, app: &App) {
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
    render_activity_map(f, app, outer[1]);

    let main = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(64), Constraint::Percentage(36)])
        .split(outer[2]);
    render_daily_token_chart(f, app, main[0]);
    render_usage_breakdown(f, app, main[1]);
    render_usage_status(f, app, outer[3]);
}

pub(super) fn render_usage_header(f: &mut Frame, app: &App, area: Rect) {
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
        Span::styled(format!("[{}]", app.usage_tab_label()), chip),
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

pub(super) fn render_activity_map(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(" Token Activity Map ")
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

pub(super) fn render_daily_token_chart(f: &mut Frame, app: &App, area: Rect) {
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

pub(super) fn render_usage_breakdown(f: &mut Frame, app: &App, area: Rect) {
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

    let inner = block.inner(area);
    f.render_widget(block, area);

    let inner_width = inner.width as usize;
    let source_rows = report.by_source.len().max(1) as u16 + 2;
    let token_mix_rows = 7;
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(source_rows),
            Constraint::Length(token_mix_rows),
            Constraint::Min(1),
        ])
        .split(inner);

    f.render_widget(
        Paragraph::new(build_usage_source_lines(app, report, inner_width)),
        sections[0],
    );
    f.render_widget(Paragraph::new(build_usage_token_mix_lines(report, inner_width)), sections[1]);

    let model_lines = build_usage_model_lines(app, report, inner_width);
    let visible_height = sections[2].height as usize;
    let max_scroll = model_lines.len().saturating_sub(visible_height);
    let scroll = app.usage_breakdown_scroll.min(max_scroll as u16) as usize;
    f.render_widget(Paragraph::new(model_lines).scroll((scroll as u16, 0)), sections[2]);
}

fn build_usage_source_lines(
    app: &App,
    report: &UsageReport,
    inner_width: usize,
) -> Vec<Line<'static>> {
    let source_max =
        report.by_source.iter().map(|source| source.tokens.total_tokens).max().unwrap_or(0);
    let mut lines = vec![Line::from(Span::styled(
        format!(" Sources ({})", report.by_source.len()),
        Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD),
    ))];

    for source in &report.by_source {
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
    lines
}

fn build_usage_token_mix_lines(report: &UsageReport, inner_width: usize) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        " Token Mix",
        Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD),
    ))];
    lines.extend(token_mix_lines(&report.summary.tokens, inner_width));
    lines.push(Line::from(""));
    lines
}

fn build_usage_model_lines(
    app: &App,
    report: &UsageReport,
    inner_width: usize,
) -> Vec<Line<'static>> {
    let model_max =
        report.by_model.iter().map(|model| model.tokens.total_tokens).max().unwrap_or(0);
    let mut lines = vec![Line::from(Span::styled(
        format!(" Models ({})", report.by_model.len()),
        Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD),
    ))];

    for model in &report.by_model {
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
    lines
}

pub(super) fn render_usage_status(f: &mut Frame, app: &App, area: Rect) {
    let line = match app.usage_tab {
        UsageTab::Tokens => Line::from(vec![
            Span::styled("m", Style::default().fg(Color::Yellow)),
            Span::styled(" tab  ", Style::default().fg(Color::DarkGray)),
            Span::styled("t", Style::default().fg(Color::Yellow)),
            Span::styled(" time  ", Style::default().fg(Color::DarkGray)),
            Span::styled("s", Style::default().fg(Color::Yellow)),
            Span::styled(" source  ", Style::default().fg(Color::DarkGray)),
            Span::styled("↑↓", Style::default().fg(Color::Yellow)),
            Span::styled(" breakdown  ", Style::default().fg(Color::DarkGray)),
            Span::styled("r", Style::default().fg(Color::Yellow)),
            Span::styled(" reset  ", Style::default().fg(Color::DarkGray)),
            Span::styled("Esc/q", Style::default().fg(Color::Yellow)),
            Span::styled(" quit", Style::default().fg(Color::DarkGray)),
        ]),
        UsageTab::Skills => Line::from(vec![
            Span::styled("m", Style::default().fg(Color::Yellow)),
            Span::styled(" tab  ", Style::default().fg(Color::DarkGray)),
            Span::styled("t", Style::default().fg(Color::Yellow)),
            Span::styled(" time  ", Style::default().fg(Color::DarkGray)),
            Span::styled("s", Style::default().fg(Color::Yellow)),
            Span::styled(" source  ", Style::default().fg(Color::DarkGray)),
            Span::styled("↑↓", Style::default().fg(Color::Yellow)),
            Span::styled(" select  ", Style::default().fg(Color::DarkGray)),
            Span::styled("Enter", Style::default().fg(Color::Yellow)),
            Span::styled(" sessions  ", Style::default().fg(Color::DarkGray)),
            Span::styled("r", Style::default().fg(Color::Yellow)),
            Span::styled(" reset  ", Style::default().fg(Color::DarkGray)),
            Span::styled("Esc/q", Style::default().fg(Color::Yellow)),
            Span::styled(" quit", Style::default().fg(Color::DarkGray)),
        ]),
    };
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

pub(super) fn render_skill_audit_dashboard(f: &mut Frame, app: &App) {
    let area = f.area();
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(6), Constraint::Min(8), Constraint::Length(1)])
        .split(area);

    render_skill_audit_header(f, app, outer[0]);
    render_skill_audit_list(f, app, outer[1]);
    render_usage_status(f, app, outer[2]);
}

pub(super) fn render_skill_audit_header(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(" Recall Skill Audit ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta));

    let chip = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let muted = Style::default().fg(Color::DarkGray);

    let control = vec![
        Span::styled(" range ", muted),
        Span::styled(format!("[{}]", app.usage_time_label()), chip),
        Span::styled(" source ", muted),
        Span::styled(format!("[{}]", app.source_filter_label()), chip),
        Span::styled(" tab ", muted),
        Span::styled(format!("[{}]", app.usage_tab_label()), chip),
    ];

    let mut lines = vec![Line::from(control)];

    if app.usage_is_loading() {
        lines.push(Line::from(Span::styled(
            " Loading skill audit...",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )));
    } else if let Some(error) = app.skill_audit_error.as_ref() {
        lines.push(Line::from(Span::styled(error.clone(), Style::default().fg(Color::Red))));
    } else if let Some(report) = app.skill_audit_report.as_ref() {
        lines.push(Line::from(vec![
            Span::styled(" installed ", muted),
            Span::styled(
                format_count(report.summary.installed),
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" core ", muted),
            Span::styled(
                format_count(report.summary.core),
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" occasional ", muted),
            Span::styled(format_count(report.summary.occasional), Style::default().fg(Color::Cyan)),
            Span::styled(" dormant ", muted),
            Span::styled(
                format_count(report.summary.dormant),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
        lines.push(Line::from(Span::styled(
            " core ≥10 calls · occasional 1-9 · dormant installed but unused in range",
            Style::default().fg(Color::DarkGray),
        )));
        if let Some(note) = report.coverage_note.as_ref() {
            lines.push(Line::from(Span::styled(note.clone(), Style::default().fg(Color::Yellow))));
        }
    } else {
        lines.push(Line::from(Span::styled("No skill audit loaded", muted)));
    }

    f.render_widget(Paragraph::new(lines).block(block), area);
}

pub(super) fn render_skill_audit_list(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(" Skills ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta));

    if app.usage_is_loading() {
        f.render_widget(
            Paragraph::new("Loading skill audit...")
                .style(Style::default().fg(Color::Yellow))
                .block(block),
            area,
        );
        return;
    }

    let Some(report) = app.skill_audit_report.as_ref() else {
        f.render_widget(
            Paragraph::new("No skill audit data")
                .style(Style::default().fg(Color::DarkGray))
                .block(block),
            area,
        );
        return;
    };

    let inner = block.inner(area);
    f.render_widget(block, area);
    let inner_width = inner.width as usize;
    let (lines, selected_line) =
        build_skill_audit_lines(report, app.skill_audit_selected, inner_width);
    let visible_height = inner.height as usize;
    let max_scroll = lines.len().saturating_sub(visible_height);
    let scroll = selected_line.saturating_sub(visible_height.saturating_sub(1)).min(max_scroll);
    f.render_widget(Paragraph::new(lines).scroll((scroll as u16, 0)), inner);
}

fn build_skill_audit_lines(
    report: &crate::skill_audit::SkillAuditReport,
    selected: usize,
    inner_width: usize,
) -> (Vec<Line<'static>>, usize) {
    let cols = skill_audit_columns(inner_width);
    let mut lines = Vec::new();
    let mut selected_line = 0usize;
    let mut entry_index = 0usize;
    let mut header_added = false;

    let sections = [
        ("CORE", SkillTier::Core, &report.core),
        ("OCCASIONAL", SkillTier::Occasional, &report.occasional),
        ("DORMANT", SkillTier::Dormant, &report.dormant),
    ];

    for (title, tier, entries) in sections {
        if entries.is_empty() {
            continue;
        }
        if !header_added {
            lines.push(skill_audit_table_header(&cols));
            lines.push(skill_audit_metrics_legend());
            header_added = true;
        }
        lines.push(Line::from(Span::styled(
            format!(" {title} ({})", entries.len()),
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD),
        )));
        for entry in entries {
            if entry_index == selected {
                selected_line = lines.len();
            }
            lines.push(skill_audit_entry_line(entry, tier, entry_index == selected, &cols));
            entry_index += 1;
        }
        lines.push(Line::from(""));
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            " No personal skills found in ~/.claude/skills, ~/.codex/skills, or ~/.agents/skills",
            Style::default().fg(Color::DarkGray),
        )));
    }

    (lines, selected_line)
}

struct SkillAuditColumns {
    skill: usize,
    calls: usize,
    last: usize,
    via: usize,
}

fn skill_audit_columns(width: usize) -> SkillAuditColumns {
    let calls = 5;
    let last = 8;
    let via = 6;
    let gaps = 3;
    let skill = width.saturating_sub(calls + last + via + gaps + 2).max(16);
    SkillAuditColumns { skill, calls, last, via }
}

fn skill_audit_table_header(cols: &SkillAuditColumns) -> Line<'static> {
    let style = Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD);
    Line::from(vec![
        Span::styled(format!("  {:<skill_w$}", "SKILL", skill_w = cols.skill), style),
        Span::styled(format!(" {:>calls_w$}", "CALLS", calls_w = cols.calls), style),
        Span::styled(format!(" {:>last_w$}", "LAST", last_w = cols.last), style),
        Span::styled(format!(" {:>via_w$}", "VIA", via_w = cols.via), style),
    ])
}

fn skill_audit_metrics_legend() -> Line<'static> {
    Line::from(Span::styled(
        "  CALLS session uses · LAST last use · VIA invoke/read/both",
        Style::default().fg(Color::DarkGray),
    ))
}

fn skill_audit_entry_line(
    entry: &SkillUsageEntry,
    tier: SkillTier,
    selected: bool,
    cols: &SkillAuditColumns,
) -> Line<'static> {
    let base = if selected {
        Style::default().fg(Color::Black).bg(Color::Magenta).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };
    let muted = if selected {
        Style::default().fg(Color::Black).bg(Color::Magenta)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let skill = truncate_label(&entry.id, cols.skill);
    let (calls, last, via) = if tier == SkillTier::Dormant {
        ("-".to_string(), "never".to_string(), "-".to_string())
    } else {
        (
            format!("{}", entry.invocations),
            format_last_used(entry.last_used),
            format_signals(&entry.signals).to_string(),
        )
    };

    Line::from(vec![
        Span::styled(format!("  {:<skill_w$}", skill, skill_w = cols.skill), base),
        Span::styled(format!(" {:>calls_w$}", calls, calls_w = cols.calls), base),
        Span::styled(format!(" {:>last_w$}", last, last_w = cols.last), muted),
        Span::styled(format!(" {:>via_w$}", via, via_w = cols.via), muted),
    ])
}
