use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::skill_audit::{SkillTier, SkillUsageEntry, format_last_used, format_signals};
use crate::tui::app::App;
use crate::tui::theme::THEME;

use super::usage::render_usage_status;
use super::{format_count, truncate_label};

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
    if app.sync_in_flight {
        control.push(Span::styled("  syncing...", muted));
    }

    let mut lines = vec![Line::from(control)];

    if app.usage.is_skill_loading() {
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

    if app.usage.is_skill_loading() {
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
