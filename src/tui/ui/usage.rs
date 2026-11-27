use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::skill_audit::{SkillTier, SkillUsageEntry, format_last_used, format_signals};
use crate::tui::app::App;
use crate::tui::theme::THEME;
use crate::tui::usage_state::UsageTab;
use crate::usage::{TokenTotals, UsageReport};

use super::{format_compact, format_count, truncate_label};

pub(super) fn render_usage_dashboard(f: &mut Frame, app: &App) {
    match app.usage.tab {
        UsageTab::Tokens => render_tokens_dashboard(f, app),
        UsageTab::Projects => super::usage_projects::render_projects_dashboard(f, app),
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
    super::usage_chart::render_activity_map(f, app, outer[1]);

    let main = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(64), Constraint::Percentage(36)])
        .split(outer[2]);
    super::usage_chart::render_over_time_chart(f, app, main[0]);
    render_usage_breakdown(f, app, main[1]);
    render_usage_status(f, app, outer[3]);
}

pub(super) fn render_usage_header(f: &mut Frame, app: &App, area: Rect) {
    let title = match &app.usage.drilldown {
        Some((_, label)) => format!(" Recall Usage · {} ", truncate_label(label, 32)),
        None => " Recall Usage Dashboard ".to_string(),
    };
    let block = Block::default()
        .title(title)
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
    if let Some(scope_label) = app.usage.scope_chip_label() {
        control.push(Span::styled(" scope ", muted));
        control.push(Span::styled(format!("[{}]", truncate_label(&scope_label, 20)), chip));
    }

    let mut lines = vec![Line::from(control)];

    if app.usage.is_loading() {
        lines.push(Line::from(Span::styled(
            " Loading usage data...",
            Style::default().fg(THEME.accent).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
    } else if let Some(report) = app.usage.report.as_ref() {
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
                Style::default().fg(THEME.success).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" sessions ", muted),
            Span::styled(
                format_count(report.summary.sessions),
                Style::default().fg(THEME.text).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" events ", muted),
            Span::styled(format_count(report.summary.events), Style::default().fg(THEME.text)),
            Span::styled(" active-days ", muted),
            Span::styled(format_count(active_days), Style::default().fg(THEME.text)),
        ]));
        lines.push(Line::from(vec![
            Span::styled(" top-source ", muted),
            Span::styled(
                truncate_label(top_source, 18),
                Style::default().fg(THEME.highlight).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" top-model ", muted),
            Span::styled(truncate_label(top_model, 32), Style::default().fg(THEME.text)),
        ]));
    } else if let Some(error) = app.usage.error.as_ref() {
        lines.push(Line::from(Span::styled(error.clone(), Style::default().fg(THEME.error))));
        lines.push(Line::from(""));
    } else {
        lines.push(Line::from(Span::styled("No usage data loaded", muted)));
        lines.push(Line::from(""));
    }

    f.render_widget(Paragraph::new(lines).block(block), area);
}

pub(super) fn render_usage_breakdown(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(" Breakdown ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(THEME.accent));

    if app.usage.is_loading() {
        f.render_widget(
            Paragraph::new("Loading usage data...")
                .style(Style::default().fg(THEME.accent))
                .block(block),
            area,
        );
        return;
    }

    let Some(report) = app.usage.report.as_ref() else {
        f.render_widget(
            Paragraph::new("No usage data")
                .style(Style::default().fg(THEME.text_muted))
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
    let scroll = app.usage.breakdown_scroll.min(max_scroll as u16) as usize;
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
        Style::default().fg(THEME.text_muted).add_modifier(Modifier::BOLD),
    ))];

    for source in &report.by_source {
        lines.push(usage_bar_line(
            app.source_label_for(&source.source),
            source.tokens.total_tokens,
            source_max,
            inner_width,
            THEME.highlight,
        ));
    }
    if report.by_source.is_empty() {
        lines.push(Line::from(Span::styled("  -", Style::default().fg(THEME.text_muted))));
    }
    lines.push(Line::from(""));
    lines
}

fn build_usage_token_mix_lines(report: &UsageReport, inner_width: usize) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        " Token Mix",
        Style::default().fg(THEME.text_muted).add_modifier(Modifier::BOLD),
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
        Style::default().fg(THEME.text_muted).add_modifier(Modifier::BOLD),
    ))];

    for model in &report.by_model {
        let label = format!("{}:{}", app.source_label_for(&model.source), model.model);
        lines.push(usage_bar_line(
            &label,
            model.tokens.total_tokens,
            model_max,
            inner_width,
            THEME.success,
        ));
    }
    if report.by_model.is_empty() {
        lines.push(Line::from(Span::styled("  -", Style::default().fg(THEME.text_muted))));
    }
    lines
}

pub(super) fn render_usage_status(f: &mut Frame, app: &App, area: Rect) {
    let key = Style::default().fg(THEME.accent);
    let text = Style::default().fg(THEME.text_muted);
    let exit_spans = if app.usage.drilldown.is_some() {
        vec![
            Span::styled("Esc", key),
            Span::styled(" back  ", text),
            Span::styled("q", key),
            Span::styled(" quit", text),
        ]
    } else {
        vec![Span::styled("Esc/q", key), Span::styled(" quit", text)]
    };

    let mut spans = match app.usage.tab {
        UsageTab::Tokens => vec![
            Span::styled("m", key),
            Span::styled(" tab  ", text),
            Span::styled("t", key),
            Span::styled(" time  ", text),
            Span::styled("s", key),
            Span::styled(" source  ", text),
            Span::styled("v", key),
            Span::styled(" metric  ", text),
            Span::styled("g", key),
            Span::styled(" day/week  ", text),
            Span::styled("↑↓", key),
            Span::styled(" breakdown  ", text),
            Span::styled("r", key),
            Span::styled(" reset  ", text),
        ],
        UsageTab::Projects => vec![
            Span::styled("m", key),
            Span::styled(" tab  ", text),
            Span::styled("t", key),
            Span::styled(" time  ", text),
            Span::styled("s", key),
            Span::styled(" source  ", text),
            Span::styled("v", key),
            Span::styled(" metric  ", text),
            Span::styled("↑↓", key),
            Span::styled(" select  ", text),
            Span::styled("Enter", key),
            Span::styled(" drill  ", text),
            Span::styled("r", key),
            Span::styled(" reset  ", text),
        ],
        UsageTab::Skills => vec![
            Span::styled("m", key),
            Span::styled(" tab  ", text),
            Span::styled("t", key),
            Span::styled(" time  ", text),
            Span::styled("s", key),
            Span::styled(" source  ", text),
            Span::styled("↑↓", key),
            Span::styled(" select  ", text),
            Span::styled("Enter", key),
            Span::styled(" sessions  ", text),
            Span::styled("r", key),
            Span::styled(" reset  ", text),
        ],
    };
    spans.extend(exit_spans);
    f.render_widget(Paragraph::new(Line::from(spans)), area);
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
            Style::default().fg(THEME.text),
        ),
        Span::styled(bar, Style::default().fg(color)),
        Span::styled(format!(" {value_text}"), Style::default().fg(THEME.text_muted)),
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
        ("input", tokens.input_tokens, THEME.token_input),
        ("output", tokens.output_tokens, THEME.token_output),
        ("cache read", tokens.cache_read_tokens, THEME.token_cache_read),
        ("cache write", tokens.cache_write_tokens, THEME.token_cache_write),
        ("reasoning", tokens.reasoning_tokens, THEME.token_reasoning),
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
        .border_style(Style::default().fg(THEME.skill));

    let chip = Style::default().fg(THEME.accent).add_modifier(Modifier::BOLD);
    let muted = Style::default().fg(THEME.text_muted);

    let mut control = vec![
        Span::styled(" range ", muted),
        Span::styled(format!("[{}]", app.usage.time_label()), chip),
        Span::styled(" source ", muted),
        Span::styled(format!("[{}]", app.source_filter_label()), chip),
        Span::styled(" tab ", muted),
        Span::styled(format!("[{}]", app.usage.tab_label()), chip),
    ];
    if let Some(scope_label) = app.usage.scope_chip_label() {
        control.push(Span::styled(" scope ", muted));
        control.push(Span::styled(format!("[{}]", truncate_label(&scope_label, 20)), chip));
    }

    let mut lines = vec![Line::from(control)];

    if app.usage.is_loading() {
        lines.push(Line::from(Span::styled(
            " Loading skill audit...",
            Style::default().fg(THEME.accent).add_modifier(Modifier::BOLD),
        )));
    } else if let Some(error) = app.usage.skill_error.as_ref() {
        lines.push(Line::from(Span::styled(error.clone(), Style::default().fg(THEME.error))));
    } else if let Some(report) = app.usage.skill_report.as_ref() {
        lines.push(Line::from(vec![
            Span::styled(" installed ", muted),
            Span::styled(
                format_count(report.summary.installed),
                Style::default().fg(THEME.text).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" core ", muted),
            Span::styled(
                format_count(report.summary.core),
                Style::default().fg(THEME.success).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" occasional ", muted),
            Span::styled(
                format_count(report.summary.occasional),
                Style::default().fg(THEME.highlight),
            ),
            Span::styled(" dormant ", muted),
            Span::styled(
                format_count(report.summary.dormant),
                Style::default().fg(THEME.text_muted),
            ),
        ]));
        lines.push(Line::from(Span::styled(
            " core ≥10 calls · occasional 1-9 · dormant installed but unused in range",
            Style::default().fg(THEME.text_muted),
        )));
        if let Some(note) = report.coverage_note.as_ref() {
            lines.push(Line::from(Span::styled(note.clone(), Style::default().fg(THEME.accent))));
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
        .border_style(Style::default().fg(THEME.skill));

    if app.usage.is_loading() {
        f.render_widget(
            Paragraph::new("Loading skill audit...")
                .style(Style::default().fg(THEME.accent))
                .block(block),
            area,
        );
        return;
    }

    let Some(report) = app.usage.skill_report.as_ref() else {
        f.render_widget(
            Paragraph::new("No skill audit data")
                .style(Style::default().fg(THEME.text_muted))
                .block(block),
            area,
        );
        return;
    };

    let inner = block.inner(area);
    f.render_widget(block, area);
    let inner_width = inner.width as usize;
    let (lines, selected_line) =
        build_skill_audit_lines(report, app.usage.skill_selected, inner_width);
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
            Style::default().fg(THEME.text_muted).add_modifier(Modifier::BOLD),
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
            Style::default().fg(THEME.text_muted),
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
    let style = Style::default().fg(THEME.text_muted).add_modifier(Modifier::BOLD);
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
        Style::default().fg(THEME.text_muted),
    ))
}

fn skill_audit_entry_line(
    entry: &SkillUsageEntry,
    tier: SkillTier,
    selected: bool,
    cols: &SkillAuditColumns,
) -> Line<'static> {
    let base = if selected {
        Style::default().fg(THEME.selected_fg).bg(THEME.skill).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(THEME.text)
    };
    let muted = if selected {
        Style::default().fg(THEME.selected_fg).bg(THEME.skill)
    } else {
        Style::default().fg(THEME.text_muted)
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
