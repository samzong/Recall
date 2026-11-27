use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::tui::app::App;
use crate::tui::theme::THEME;
use crate::tui::usage_state::ChartMetric;
use crate::usage::{ProjectUsageEntry, ProjectUsageReport};

use super::usage::render_usage_status;
use super::usage_chart::sparkline_string;
use super::{format_compact, format_count, render_vertical_scrollbar, truncate_label};

pub(super) fn render_projects_dashboard(f: &mut Frame, app: &App) {
    let area = f.area();
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(6), Constraint::Min(8), Constraint::Length(1)])
        .split(area);

    render_projects_header(f, app, outer[0]);
    render_projects_table(f, app, outer[1]);
    render_usage_status(f, app, outer[2]);
}

pub(super) fn render_projects_header(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(" Recall Project Usage ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(THEME.border_focus));

    let chip = Style::default().fg(THEME.accent).add_modifier(Modifier::BOLD);
    let muted = Style::default().fg(THEME.text_muted);

    let mut control = vec![
        Span::styled(" range ", muted),
        Span::styled(format!("[{}]", app.usage.time_label()), chip),
        Span::styled(" source ", muted),
        Span::styled(format!("[{}]", app.source_filter_label()), chip),
        Span::styled(" metric ", muted),
        Span::styled(format!("[{}]", app.usage.metric_label()), chip),
        Span::styled(" tab ", muted),
        Span::styled(format!("[{}]", app.usage.tab_label()), chip),
    ];
    if app.sync_in_flight {
        control.push(Span::styled("  syncing...", muted));
    }

    let mut lines = vec![Line::from(control)];

    if app.usage.is_dashboard_loading() {
        lines.push(Line::from(Span::styled(
            " Loading project usage...",
            Style::default().fg(THEME.accent).add_modifier(Modifier::BOLD),
        )));
    } else if let Some(error) = app.usage.projects_error.as_ref() {
        lines.push(Line::from(Span::styled(error.clone(), Style::default().fg(THEME.error))));
    } else if let Some(report) = app.usage.projects_report.as_ref() {
        let top_project = report.projects.first().map(|p| p.project.as_str()).unwrap_or("-");
        lines.push(Line::from(vec![
            Span::styled(" projects ", muted),
            Span::styled(
                format_count(report.summary.projects),
                Style::default().fg(THEME.text).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" tokens ", muted),
            Span::styled(
                format_compact(report.summary.tokens.total_tokens),
                Style::default().fg(THEME.success).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" sessions ", muted),
            Span::styled(
                format!(
                    "{}/{}",
                    format_count(report.summary.sessions),
                    format_count(report.summary.indexed_sessions)
                ),
                Style::default().fg(THEME.text).add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::from(vec![
            Span::styled(" top-project ", muted),
            Span::styled(
                truncate_label(top_project, area.width.saturating_sub(18) as usize),
                Style::default().fg(THEME.highlight).add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::from(Span::styled(
            " sessions: token-bearing / indexed · Enter opens project usage",
            muted,
        )));
    } else {
        lines.push(Line::from(Span::styled("No project usage loaded", muted)));
    }

    f.render_widget(Paragraph::new(lines).block(block), area);
}

pub(super) fn render_projects_table(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(" Projects ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(THEME.accent));

    if app.usage.is_dashboard_loading() {
        f.render_widget(
            Paragraph::new("Loading project usage...")
                .style(Style::default().fg(THEME.accent))
                .block(block),
            area,
        );
        return;
    }

    let Some(report) = app.usage.projects_report.as_ref() else {
        let msg = app.usage.projects_error.as_deref().unwrap_or("No project usage data");
        let color = if app.usage.projects_error.is_some() { THEME.error } else { THEME.text_muted };
        f.render_widget(Paragraph::new(msg).style(Style::default().fg(color)).block(block), area);
        return;
    };

    if report.projects.is_empty() {
        f.render_widget(
            Paragraph::new("No project usage in this range")
                .style(Style::default().fg(THEME.text_muted))
                .block(block),
            area,
        );
        return;
    }

    let inner = block.inner(area);
    f.render_widget(block, area);

    let inner_width = inner.width as usize;
    let visible_rows = (inner.height as usize).saturating_sub(1);
    let cols = project_columns(report, inner_width);
    let total = report.projects.len();
    let selected = app.usage.project_selected.min(total - 1);
    let start =
        if visible_rows == 0 || selected < visible_rows { 0 } else { selected + 1 - visible_rows };
    let end = (start + visible_rows).min(total);

    let mut lines = Vec::with_capacity(1 + end.saturating_sub(start));
    lines.push(project_table_header(&cols));
    for (offset, entry) in report.projects[start..end].iter().enumerate() {
        let index = start + offset;
        lines.push(project_entry_line(
            entry,
            index == selected,
            app.usage.chart_metric,
            &cols,
            inner_width,
        ));
    }

    f.render_widget(Paragraph::new(lines), inner);
    render_vertical_scrollbar(f, area, total, visible_rows, start);
}

struct ProjectColumns {
    project: usize,
    trend: Option<usize>,
    trend_window: &'static str,
    sessions: usize,
    tokens: usize,
    model: usize,
}

fn project_columns(report: &ProjectUsageReport, width: usize) -> ProjectColumns {
    let sessions = 9usize;
    let tokens = 8usize;
    let show_trend = width >= 76;
    let trend_window = report.projects.first().map(|p| p.trend_label).unwrap_or("trend");
    let trend_header_len = format!("TREND ({trend_window})").chars().count();
    let sample_len = report.projects.first().map(|p| p.trend_tokens.len()).unwrap_or(0);
    let trend = show_trend.then(|| trend_header_len.max(sample_len).clamp(10, 16));
    let model = if show_trend { (width / 5).clamp(12, 20) } else { (width / 4).clamp(9, 16) };
    let fixed = 2 + sessions + tokens + model + trend.map_or(3, |w| w + 4);
    let project = width.saturating_sub(fixed).max(8);
    ProjectColumns { project, trend, trend_window, sessions, tokens, model }
}

fn project_table_header(cols: &ProjectColumns) -> Line<'static> {
    let style = Style::default().fg(THEME.text_muted).add_modifier(Modifier::BOLD);
    let mut spans =
        vec![Span::styled(format!("  {:<project_w$}", "PROJECT", project_w = cols.project), style)];
    if let Some(trend_w) = cols.trend {
        let header = format!("TREND ({})", cols.trend_window);
        spans.push(Span::styled(format!(" {:<trend_w$}", truncate_label(&header, trend_w)), style));
    }
    spans.push(Span::styled(
        format!(" {:>sessions_w$}", "SESSIONS", sessions_w = cols.sessions),
        style,
    ));
    spans.push(Span::styled(format!(" {:>tokens_w$}", "TOKENS", tokens_w = cols.tokens), style));
    spans.push(Span::styled(format!(" {:<model_w$}", "TOP MODEL", model_w = cols.model), style));
    Line::from(spans)
}

fn project_entry_line(
    entry: &ProjectUsageEntry,
    selected: bool,
    metric: ChartMetric,
    cols: &ProjectColumns,
    inner_width: usize,
) -> Line<'static> {
    let base = if selected {
        Style::default().fg(THEME.selected_fg).bg(THEME.selected_bg).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(THEME.text)
    };
    let muted = if selected {
        Style::default().fg(THEME.selected_fg).bg(THEME.selected_bg)
    } else {
        Style::default().fg(THEME.text_muted)
    };
    let spark_style = if selected { base } else { Style::default().fg(THEME.highlight) };

    let project = truncate_label(&entry.project, cols.project);
    let sessions =
        format!("{}/{}", format_count(entry.sessions), format_count(entry.indexed_sessions));
    let tokens = format_compact(entry.tokens.total_tokens);
    let top_model = truncate_label(entry.top_model.as_deref().unwrap_or("-"), cols.model);

    let mut spans =
        vec![Span::styled(format!("  {:<project_w$}", project, project_w = cols.project), base)];
    let mut used = 2 + cols.project;
    if let Some(trend_w) = cols.trend {
        let series: Vec<i64> = match metric {
            ChartMetric::Tokens => entry.trend_tokens.clone(),
            ChartMetric::Sessions => {
                entry.trend_sessions.iter().map(|count| *count as i64).collect()
            }
        };
        let spark = sparkline_string(&series, trend_w);
        spans.push(Span::styled(format!(" {:<trend_w$}", spark), spark_style));
        used += 1 + trend_w;
    }
    spans.push(Span::styled(
        format!(
            " {:>sessions_w$}",
            truncate_label(&sessions, cols.sessions),
            sessions_w = cols.sessions
        ),
        muted,
    ));
    spans.push(Span::styled(format!(" {:>tokens_w$}", tokens, tokens_w = cols.tokens), base));
    spans.push(Span::styled(format!(" {:<model_w$}", top_model, model_w = cols.model), muted));
    used += 3 + cols.sessions + cols.tokens + cols.model;
    if selected && used < inner_width {
        spans.push(Span::styled(" ".repeat(inner_width - used), base));
    }

    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::config::AppConfig;
    use crate::db::store::Store;
    use crate::tui::share_state::AppMode;
    use crate::tui::usage_state::{ChartMetric, UsageTab};
    use crate::usage::{ProjectPartitionKey, ProjectUsageSummary, TokenTotals};

    use super::*;

    fn test_app() -> App {
        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        App::new(&store, vec![("codex".to_string(), "Codex".to_string())], AppConfig::default())
    }

    fn project_entry(name: &str, tokens: i64, sessions: usize) -> ProjectUsageEntry {
        ProjectUsageEntry {
            key: ProjectPartitionKey::Repository(format!("github.com/{name}")),
            project: name.to_string(),
            kind: "repository",
            repository: None,
            directory: None,
            sessions,
            indexed_sessions: sessions + 2,
            events: sessions * 2,
            top_model: Some("gpt-5.4".to_string()),
            tokens: TokenTotals { total_tokens: tokens, ..TokenTotals::default() },
            by_model: Vec::new(),
            trend_tokens: vec![0, 100, 500, 1000, 0, 300, 800],
            trend_sessions: vec![0, 1, 2, 4, 0, 1, 3],
            trend_label: "7d",
        }
    }

    fn sample_projects_report(count: usize) -> ProjectUsageReport {
        let projects: Vec<ProjectUsageEntry> = (0..count)
            .map(|i| project_entry(&format!("owner/repo-{i:02}"), (100 - i as i64) * 1000, 5))
            .collect();
        ProjectUsageReport {
            scope: None,
            summary: ProjectUsageSummary {
                projects: count,
                events: count * 10,
                sessions: count * 5,
                indexed_sessions: count * 7,
                sources: 1,
                models: 1,
                tokens: TokenTotals { total_tokens: 900_000, ..TokenTotals::default() },
            },
            projects,
        }
    }

    fn render_projects(app: &App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| crate::tui::ui::render(f, app)).unwrap();
        let buf = terminal.backend().buffer();
        (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn projects_tab_renders_wide_with_trend_and_hides_trend_on_narrow() {
        let mut app = test_app();
        app.mode = AppMode::Usage;
        app.usage.tab = UsageTab::Projects;
        app.usage.projects_report = Some(sample_projects_report(15));

        let wide = render_projects(&app, 100, 20);
        assert!(wide.contains("Recall Project Usage"));
        assert!(wide.contains("TREND (7d)"));
        assert!(wide.contains("SESSIONS"));
        assert!(wide.contains("TOKENS"));
        assert!(wide.contains("TOP MODEL"));
        assert!(wide.contains("token-bearing / indexed"));
        assert!(wide.contains("·▁▄█·▃▇"));

        let narrow = render_projects(&app, 70, 16);
        assert!(narrow.contains("Recall Project Usage"));
        assert!(narrow.contains("PROJECT"));
        assert!(narrow.contains("SESSIONS"));
        assert!(!narrow.contains("TREND (7d)"));
    }

    #[test]
    fn projects_tab_scrolls_selected_row_into_view_and_switches_metric() {
        let mut app = test_app();
        app.mode = AppMode::Usage;
        app.usage.tab = UsageTab::Projects;
        app.usage.projects_report = Some(sample_projects_report(18));
        app.usage.select_project(17);
        app.usage.chart_metric = ChartMetric::Sessions;

        let rendered = render_projects(&app, 100, 14);
        assert!(rendered.contains("metric [sessions]"));
        assert!(rendered.contains("owner/repo-17"));
        assert!(!rendered.contains("owner/repo-01"));
        assert!(rendered.contains("▌"));
    }

    #[test]
    fn projects_tab_header_omits_scope_chip_when_scope_or_drilldown_is_set() {
        let mut app = test_app();
        app.mode = AppMode::Usage;
        app.usage.tab = UsageTab::Projects;
        app.usage.initial_scope =
            crate::project_scope::ProjectScope::Directory("/repo/scoped".into());
        app.usage.drilldown =
            Some((ProjectPartitionKey::Directory("/repo/drill".into()), "scoped-drill".into()));
        app.usage.projects_report = Some(sample_projects_report(2));

        let rendered = render_projects(&app, 120, 16);
        assert!(rendered.contains("Recall Project Usage"));
        assert!(rendered.contains("range [All day]"));
        assert!(rendered.contains("source [ALL]"));
        assert!(rendered.contains("metric [tokens]"));
        assert!(rendered.contains("tab [projects]"));
        assert!(!rendered.contains("scope ["));
    }
}
