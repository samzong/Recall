use std::collections::BTreeMap;

use chrono::{Datelike, Duration, Local, NaiveDate, TimeZone, Weekday};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::db::search::TimeRange;
use crate::tui::app::App;
use crate::tui::theme::THEME;
use crate::tui::usage_state::{ChartGranularity, ChartMetric};
use crate::usage::UsageReport;

use super::{format_compact, format_count};

const BAR_BLOCKS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
const SPARK_BLOCKS: [char; 9] = ['·', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

pub(super) fn render_activity_map(f: &mut Frame, app: &App, area: Rect) {
    let title = match app.usage.chart_metric {
        ChartMetric::Tokens => " Token Activity Map ",
        ChartMetric::Sessions => " Session Activity Map ",
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(THEME.success));

    if app.usage.is_dashboard_loading() {
        f.render_widget(
            Paragraph::new("Loading usage data...")
                .style(Style::default().fg(THEME.accent))
                .block(block),
            area,
        );
        return;
    }

    let report = app.usage.year_report.as_ref().or(app.usage.report.as_ref());
    let Some(report) = report else {
        f.render_widget(
            Paragraph::new("No usage events")
                .style(Style::default().fg(THEME.text_muted))
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

    let values = daily_metric_map(report, app.usage.chart_metric);
    let max_value = values
        .iter()
        .filter(|(date, _)| **date >= start && **date <= today)
        .map(|(_, value)| *value)
        .max()
        .unwrap_or(0);

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
            Span::styled(format!(" {label} "), Style::default().fg(THEME.text_muted)),
            Span::styled(cells, Style::default().fg(THEME.success)),
        ]));
    }

    f.render_widget(Paragraph::new(lines).block(block), area);
}

pub(super) fn render_over_time_chart(f: &mut Frame, app: &App, area: Rect) {
    let metric_title = match app.usage.chart_metric {
        ChartMetric::Tokens => "Tokens Over Time",
        ChartMetric::Sessions => "Sessions Over Time",
    };
    let title = format!(" {metric_title} ({}) ", app.usage.granularity_label());
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(THEME.border_focus));

    if app.usage.is_dashboard_loading() {
        f.render_widget(
            Paragraph::new("Loading usage data...")
                .style(Style::default().fg(THEME.accent))
                .block(block),
            area,
        );
        return;
    }

    let Some(report) = app.usage.report.as_ref() else {
        let empty = match app.usage.chart_metric {
            ChartMetric::Tokens => "No token usage",
            ChartMetric::Sessions => "No session activity",
        };
        f.render_widget(
            Paragraph::new(empty).style(Style::default().fg(THEME.text_muted)).block(block),
            area,
        );
        return;
    };

    let inner_width = area.width.saturating_sub(2) as usize;
    let label_width = 8usize;
    let plot_width = inner_width.saturating_sub(label_width).max(1);
    let points = over_time_points(
        report,
        app.usage.time_filter,
        app.usage.chart_metric,
        app.usage.effective_granularity(),
        plot_width,
        Local::now(),
    );
    let max_value = points.iter().map(|(_, value)| *value).max().unwrap_or(0);
    if points.is_empty() || max_value == 0 {
        let empty = match app.usage.chart_metric {
            ChartMetric::Tokens => "No token usage in this range",
            ChartMetric::Sessions => "No sessions in this range",
        };
        f.render_widget(
            Paragraph::new(empty).style(Style::default().fg(THEME.text_muted)).block(block),
            area,
        );
        return;
    }

    let chart_height = area.height.saturating_sub(5).max(1) as usize;
    let units: Vec<usize> =
        points.iter().map(|(_, value)| bar_units(*value, max_value, chart_height)).collect();
    let max_label = match app.usage.chart_metric {
        ChartMetric::Tokens => format_compact(max_value),
        ChartMetric::Sessions => format_count(max_value as usize),
    };

    let mut lines = Vec::new();
    for row_from_bottom in (0..chart_height).rev() {
        let label = if row_from_bottom + 1 == chart_height {
            format!("{max_label:>7}")
        } else if row_from_bottom == 0 {
            "      0".to_string()
        } else {
            "       ".to_string()
        };
        let mut bars = String::with_capacity(plot_width);
        for (index, point_units) in units.iter().enumerate() {
            let bucket_width = distributed_width(index, units.len(), plot_width);
            let ch = bar_cell(*point_units, row_from_bottom);
            for _ in 0..bucket_width {
                bars.push(ch);
            }
        }
        lines.push(Line::from(vec![
            Span::styled(format!("{label} "), Style::default().fg(THEME.text_muted)),
            Span::styled(bars, Style::default().fg(THEME.highlight)),
        ]));
    }

    if let (Some((first, _)), Some((last, _))) = (points.first(), points.last()) {
        lines.push(Line::from(vec![
            Span::styled("        ", Style::default().fg(THEME.text_muted)),
            Span::styled(
                endpoint_labels(first, last, plot_width),
                Style::default().fg(THEME.text_muted),
            ),
        ]));
    }

    f.render_widget(Paragraph::new(lines).block(block), area);
}

pub(super) fn sparkline_string(values: &[i64], width: usize) -> String {
    if values.is_empty() || width == 0 {
        return String::new();
    }
    let sampled: Vec<i64> = if values.len() <= width {
        values.to_vec()
    } else {
        (0..width)
            .map(|bucket| {
                let start = bucket * values.len() / width;
                let end = ((bucket + 1) * values.len() / width).max(start + 1).min(values.len());
                values[start..end].iter().copied().max().unwrap_or(0)
            })
            .collect()
    };
    let max_value = sampled.iter().copied().max().unwrap_or(0);
    sampled
        .into_iter()
        .map(|value| {
            if value <= 0 || max_value <= 0 {
                SPARK_BLOCKS[0]
            } else {
                let idx = (((value as f64 / max_value as f64) * 8.0).ceil() as usize).clamp(1, 8);
                SPARK_BLOCKS[idx]
            }
        })
        .collect()
}

fn daily_metric_map(report: &UsageReport, metric: ChartMetric) -> BTreeMap<NaiveDate, i64> {
    let mut values = BTreeMap::new();
    match metric {
        ChartMetric::Tokens => {
            for day in &report.daily {
                if let Ok(date) = NaiveDate::parse_from_str(&day.period, "%Y-%m-%d") {
                    values.insert(date, day.tokens.total_tokens);
                }
            }
        }
        ChartMetric::Sessions => {
            if !report.daily_all_sessions.is_empty() {
                for (period, count) in &report.daily_all_sessions {
                    if let Ok(date) = NaiveDate::parse_from_str(period, "%Y-%m-%d") {
                        values.insert(date, *count as i64);
                    }
                }
            } else {
                for day in &report.daily {
                    if let Ok(date) = NaiveDate::parse_from_str(&day.period, "%Y-%m-%d") {
                        values.insert(date, day.sessions as i64);
                    }
                }
            }
        }
    }
    values
}

fn weekly_metric_map(report: &UsageReport, metric: ChartMetric) -> BTreeMap<NaiveDate, i64> {
    let mut values = BTreeMap::new();
    match metric {
        ChartMetric::Tokens => {
            for week in &report.weekly {
                if let Some(monday) = parse_iso_week_monday(&week.period) {
                    values.insert(monday, week.tokens.total_tokens);
                }
            }
        }
        ChartMetric::Sessions => {
            if !report.weekly_all_sessions.is_empty() {
                for (period, count) in &report.weekly_all_sessions {
                    if let Some(monday) = parse_iso_week_monday(period) {
                        values.insert(monday, *count as i64);
                    }
                }
            } else {
                for week in &report.weekly {
                    if let Some(monday) = parse_iso_week_monday(&week.period) {
                        values.insert(monday, week.sessions as i64);
                    }
                }
            }
        }
    }
    values
}

fn over_time_points(
    report: &UsageReport,
    time_range: TimeRange,
    metric: ChartMetric,
    granularity: ChartGranularity,
    max_points: usize,
    now: chrono::DateTime<Local>,
) -> Vec<(String, i64)> {
    if max_points == 0 {
        return Vec::new();
    }

    if time_range == TimeRange::Today {
        let start_hour = 24usize.saturating_sub(max_points);
        return (start_hour..24)
            .map(|hour| {
                let value = match metric {
                    ChartMetric::Tokens => report.hourly_today_tokens[hour],
                    ChartMetric::Sessions => report.hourly_today_sessions[hour] as i64,
                };
                (format!("{hour:02}"), value)
            })
            .collect();
    }

    match granularity {
        ChartGranularity::Daily => daily_series(report, time_range, metric, max_points, now),
        ChartGranularity::Weekly => weekly_series(report, time_range, metric, max_points, now),
    }
}

fn daily_series(
    report: &UsageReport,
    time_range: TimeRange,
    metric: ChartMetric,
    max_points: usize,
    now: chrono::DateTime<Local>,
) -> Vec<(String, i64)> {
    let values = daily_metric_map(report, metric);
    let today = now.date_naive();
    let (start_date, end_date) = match time_range {
        TimeRange::Today | TimeRange::Week | TimeRange::Month => {
            let cutoff = time_range
                .cutoff_millis_at(now)
                .and_then(|ms| Local.timestamp_millis_opt(ms).single())
                .map(|dt| dt.date_naive())
                .unwrap_or(today);
            (cutoff.min(today), today)
        }
        TimeRange::All => {
            let (Some(earliest), Some(latest)) = (values.keys().next(), values.keys().next_back())
            else {
                return Vec::new();
            };
            (*earliest, (*latest).max(today))
        }
    };

    let total_days = end_date.signed_duration_since(start_date).num_days().max(0) as usize + 1;
    let visible = total_days.min(max_points);
    let visible_start =
        end_date.checked_sub_signed(Duration::days((visible - 1) as i64)).unwrap_or(start_date);

    (0..visible)
        .filter_map(|offset| visible_start.checked_add_signed(Duration::days(offset as i64)))
        .map(|date| {
            let label = date.format("%m-%d").to_string();
            let value = values.get(&date).copied().unwrap_or(0);
            (label, value)
        })
        .collect()
}

fn weekly_series(
    report: &UsageReport,
    time_range: TimeRange,
    metric: ChartMetric,
    max_points: usize,
    now: chrono::DateTime<Local>,
) -> Vec<(String, i64)> {
    let values = weekly_metric_map(report, metric);
    let today_monday = iso_week_monday(now.date_naive());
    let (start_monday, end_monday) = match time_range {
        TimeRange::Today | TimeRange::Week | TimeRange::Month => {
            let cutoff_date = time_range
                .cutoff_millis_at(now)
                .and_then(|ms| Local.timestamp_millis_opt(ms).single())
                .map(|dt| dt.date_naive())
                .unwrap_or_else(|| now.date_naive());
            let cutoff_monday = iso_week_monday(cutoff_date);
            let earliest =
                values.keys().next().copied().map_or(cutoff_monday, |d| d.min(cutoff_monday));
            let latest =
                values.keys().next_back().copied().map_or(today_monday, |d| d.max(today_monday));
            (earliest, latest)
        }
        TimeRange::All => {
            let (Some(earliest), Some(latest)) = (values.keys().next(), values.keys().next_back())
            else {
                return Vec::new();
            };
            (*earliest, *latest)
        }
    };

    let total_weeks =
        (end_monday.signed_duration_since(start_monday).num_days().max(0) as usize) / 7 + 1;
    let visible = total_weeks.min(max_points);
    let visible_start = end_monday
        .checked_sub_signed(Duration::days(((visible - 1) * 7) as i64))
        .unwrap_or(start_monday);

    (0..visible)
        .filter_map(|offset| visible_start.checked_add_signed(Duration::days((offset * 7) as i64)))
        .map(|monday| {
            let iso = monday.iso_week();
            let label = format!("{}-W{:02}", iso.year(), iso.week());
            let value = values.get(&monday).copied().unwrap_or(0);
            (label, value)
        })
        .collect()
}

fn iso_week_monday(date: NaiveDate) -> NaiveDate {
    date.checked_sub_signed(Duration::days(date.weekday().num_days_from_monday() as i64))
        .unwrap_or(date)
}

fn parse_iso_week_monday(period: &str) -> Option<NaiveDate> {
    let (year_str, week_str) = period.split_once("-W")?;
    let year: i32 = year_str.parse().ok()?;
    let week: u32 = week_str.parse().ok()?;
    NaiveDate::from_isoywd_opt(year, week, Weekday::Mon)
}

fn bar_units(value: i64, max_value: i64, chart_height: usize) -> usize {
    if value <= 0 || max_value <= 0 || chart_height == 0 {
        return 0;
    }
    let total_units = chart_height * 8;
    let scaled = ((value as f64 / max_value as f64) * total_units as f64).round() as usize;
    scaled.clamp(1, total_units)
}

fn bar_cell(units: usize, row_from_bottom: usize) -> char {
    let fill = units.saturating_sub(row_from_bottom * 8).min(8);
    BAR_BLOCKS[fill]
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
