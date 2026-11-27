use std::io::{self, IsTerminal, Write};

use unicode_width::UnicodeWidthStr;

use super::{INNER, WrappedReport, WrappedSourceRow, sanitize_label};

pub(super) fn color_enabled() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

pub(super) struct StderrProgress {
    enabled: bool,
    color: bool,
    last_width: usize,
}

impl StderrProgress {
    pub(super) fn new() -> Self {
        let enabled = io::stderr().is_terminal();
        Self { enabled, color: enabled && std::env::var_os("NO_COLOR").is_none(), last_width: 0 }
    }

    pub(super) fn show_source(&mut self, source: &str) {
        if !self.enabled {
            return;
        }
        let name = title_source(source);
        let painted = paint(self.color, &["\x1b[1m", "\x1b[36m"], &name);
        let text = format!("Refreshing usage · {painted}");
        let (line, width) = progress_overwrite(self.last_width, &text);
        eprint!("{line}");
        let _ = io::stderr().flush();
        self.last_width = width;
    }

    pub(super) fn clear(&mut self) {
        if !self.enabled || self.last_width == 0 {
            return;
        }
        eprint!("{}", progress_clear(self.last_width));
        let _ = io::stderr().flush();
        self.last_width = 0;
    }
}

fn progress_overwrite(previous_width: usize, text: &str) -> (String, usize) {
    let width = visible_width(text);
    (format!("\r{text}{}", " ".repeat(previous_width.saturating_sub(width))), width)
}

fn progress_clear(previous_width: usize) -> String {
    if previous_width == 0 { String::new() } else { format!("\r{}\r", " ".repeat(previous_width)) }
}

pub(super) fn format_tokens(value: i64) -> String {
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

pub(super) fn title_source(id: &str) -> String {
    id.split(['-', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub(super) fn weekday_label(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn streak_label(days: usize) -> String {
    if days == 1 { "1 day".to_string() } else { format!("{days} days") }
}

fn strip_ansi(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            output.push(ch);
        }
    }
    output
}

pub(super) fn visible_width(text: &str) -> usize {
    UnicodeWidthStr::width(strip_ansi(text).as_str())
}

fn truncate_width(text: &str, max: usize, ascii: bool) -> String {
    if UnicodeWidthStr::width(text) <= max {
        return text.to_string();
    }
    let ellipsis = if ascii { "..." } else { "…" };
    let ellipsis_w = UnicodeWidthStr::width(ellipsis);
    if max < ellipsis_w {
        return String::new();
    }
    let target = max - ellipsis_w;
    let mut acc = String::new();
    let mut width = 0usize;
    for ch in text.chars() {
        let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_width > target {
            break;
        }
        acc.push(ch);
        width += ch_width;
    }
    acc.push_str(ellipsis);
    acc
}

fn pad_end(content: &str, width: usize) -> String {
    let vis = visible_width(content);
    if vis >= width { content.to_string() } else { format!("{content}{}", " ".repeat(width - vis)) }
}

fn paint(enabled: bool, codes: &[&str], text: &str) -> String {
    if !enabled || text.is_empty() {
        return text.to_string();
    }
    let mut out = String::new();
    for code in codes {
        out.push_str(code);
    }
    out.push_str(text);
    out.push_str("\x1b[0m");
    out
}

fn center_plain(text: &str, width: usize, ascii: bool) -> String {
    let clipped = truncate_width(text, width, ascii);
    let vis = UnicodeWidthStr::width(clipped.as_str());
    let left = (width - vis) / 2;
    let right = width - vis - left;
    format!("{}{}{}", " ".repeat(left), clipped, " ".repeat(right))
}

fn center_painted(text: &str, width: usize, ascii: bool, color: bool, codes: &[&str]) -> String {
    let clipped = truncate_width(text, width, ascii);
    let vis = UnicodeWidthStr::width(clipped.as_str());
    let left = (width - vis) / 2;
    let right = width - vis - left;
    format!("{}{}{}", " ".repeat(left), paint(color, codes, &clipped), " ".repeat(right))
}

struct Borders {
    tl: char,
    tr: char,
    bl: char,
    br: char,
    h: char,
    v: char,
    ml: char,
    mr: char,
}

fn borders(ascii: bool) -> Borders {
    if ascii {
        Borders { tl: '+', tr: '+', bl: '+', br: '+', h: '-', v: '|', ml: '+', mr: '+' }
    } else {
        Borders {
            tl: '╭', tr: '╮', bl: '╰', br: '╯', h: '─', v: '│', ml: '├', mr: '┤'
        }
    }
}

fn rule(borders: &Borders, left: char, right: char, color: bool) -> String {
    let line = format!("{left}{}{right}", borders.h.to_string().repeat(INNER));
    format!("{}\n", paint(color, &["\x1b[34m"], &line))
}

fn row(borders: &Borders, color: bool, inner: &str) -> String {
    format!(
        "{}{}{}\n",
        paint(color, &["\x1b[34m"], &borders.v.to_string()),
        pad_end(inner, INNER),
        paint(color, &["\x1b[34m"], &borders.v.to_string())
    )
}

fn blank(borders: &Borders, color: bool) -> String {
    row(borders, color, "")
}

fn pair_cell(label: &str, value: &str, width: usize, color: bool) -> String {
    let label_col: usize = 14;
    let label_text = format!("  {label}");
    let label_pad = label_col.saturating_sub(UnicodeWidthStr::width(label_text.as_str()));
    let painted = paint(color, &["\x1b[1m", "\x1b[32m"], value);
    let used = label_col + UnicodeWidthStr::width(value);
    let tail = width.saturating_sub(used);
    format!("{label_text}{}{painted}{}", " ".repeat(label_pad), " ".repeat(tail))
}

fn pair_row(
    left_label: &str,
    left_value: &str,
    right_label: &str,
    right_value: &str,
    color: bool,
) -> String {
    let half = INNER / 2;
    format!(
        "{}{}",
        pair_cell(left_label, left_value, half, color),
        pair_cell(right_label, right_value, INNER - half, color)
    )
}

fn labeled_row(label: &str, value: &str, color: bool, ascii: bool) -> String {
    let label_col: usize = 18;
    let label_text = format!("  {label}");
    let label_vis = UnicodeWidthStr::width(label_text.as_str());
    let label_pad = label_col.saturating_sub(label_vis);
    let value_text = truncate_width(value, INNER.saturating_sub(label_col), ascii);
    format!(
        "{label_text}{}{}",
        " ".repeat(label_pad),
        paint(color, &["\x1b[1m", "\x1b[32m"], &value_text)
    )
}

pub(super) fn render_card(report: &WrappedReport, color: bool) -> String {
    let ascii = !color;
    let borders = borders(ascii);
    let mut out = String::new();
    out.push_str(&rule(&borders, borders.tl, borders.tr, color));
    out.push_str(&row(
        &borders,
        color,
        &center_painted("RECALL WRAPPED", INNER, ascii, color, &["\x1b[1m", "\x1b[36m"]),
    ));
    if let Some(project) = &report.project_label {
        out.push_str(&row(
            &borders,
            color,
            &center_painted(&sanitize_label(project), INNER, ascii, color, &["\x1b[1m"]),
        ));
    }
    out.push_str(&row(
        &borders,
        color,
        &center_painted(report.period.card_label(), INNER, ascii, color, &["\x1b[2m"]),
    ));
    out.push_str(&rule(&borders, borders.ml, borders.mr, color));
    out.push_str(&blank(&borders, color));

    if report.empty {
        let empty_msg = if report.project_label.is_some() {
            "No usage data for this project."
        } else {
            "No usage data for this period."
        };
        out.push_str(&row(&borders, color, &center_plain(empty_msg, INNER, ascii)));
        out.push_str(&row(
            &borders,
            color,
            &center_plain("Sync your tools, then try again.", INNER, ascii),
        ));
        out.push_str(&blank(&borders, color));
        out.push_str(&rule(&borders, borders.bl, borders.br, color));
        return out;
    }

    let total = format_tokens(report.tokens.total_tokens);
    out.push_str(&row(
        &borders,
        color,
        &center_painted(&total, INNER, ascii, color, &["\x1b[1m", "\x1b[32m"]),
    ));
    out.push_str(&row(
        &borders,
        color,
        &center_painted("total tokens", INNER, ascii, color, &["\x1b[2m"]),
    ));
    out.push_str(&blank(&borders, color));
    out.push_str(&row(
        &borders,
        color,
        &pair_row(
            "input",
            &format_tokens(report.tokens.input_tokens),
            "output",
            &format_tokens(report.tokens.output_tokens),
            color,
        ),
    ));
    out.push_str(&row(
        &borders,
        color,
        &pair_row(
            "cache read",
            &format_tokens(report.tokens.cache_read_tokens),
            "cache write",
            &format_tokens(report.tokens.cache_write_tokens),
            color,
        ),
    ));
    if report.tokens.reasoning_tokens > 0 {
        out.push_str(&row(
            &borders,
            color,
            &labeled_row("reasoning", &format_tokens(report.tokens.reasoning_tokens), color, ascii),
        ));
    }
    out.push_str(&blank(&borders, color));
    out.push_str(&row(
        &borders,
        color,
        &pair_row(
            "sessions",
            &format_tokens(report.sessions as i64),
            "active days",
            &format_tokens(report.active_days as i64),
            color,
        ),
    ));
    out.push_str(&row(
        &borders,
        color,
        &labeled_row("longest streak", &streak_label(report.longest_streak), color, ascii),
    ));
    out.push_str(&blank(&borders, color));
    if let Some(top_model) = &report.top_model {
        out.push_str(&row(
            &borders,
            color,
            &labeled_row("top model", &sanitize_label(&top_model.model), color, ascii),
        ));
    }
    if let Some(top_source) = &report.top_source {
        out.push_str(&row(
            &borders,
            color,
            &labeled_row(
                "top source",
                &title_source(&sanitize_label(&top_source.source)),
                color,
                ascii,
            ),
        ));
    }
    if let (Some(weekday), Some(hour)) = (&report.busiest_weekday, report.busiest_hour) {
        out.push_str(&row(
            &borders,
            color,
            &labeled_row(
                "busiest",
                &format!("{} at {hour:02}:00", weekday_label(weekday)),
                color,
                ascii,
            ),
        ));
    }
    if !report.by_source.is_empty() {
        out.push_str(&blank(&borders, color));
        out.push_str(&rule(&borders, borders.ml, borders.mr, color));
        out.push_str(&row(&borders, color, &source_header_row(color)));
        for row_data in &report.by_source {
            out.push_str(&row(&borders, color, &source_data_row(row_data, color, ascii)));
        }
    }
    out.push_str(&blank(&borders, color));
    out.push_str(&rule(&borders, borders.bl, borders.br, color));
    out
}

fn source_header_row(color: bool) -> String {
    let source = paint(color, &["\x1b[2m"], "source");
    let sessions = paint(color, &["\x1b[2m"], "sessions");
    let tokens = paint(color, &["\x1b[2m"], "tokens");
    format_source_columns(&source, 6, &sessions, 8, &tokens, 6)
}

fn source_data_row(row: &WrappedSourceRow, color: bool, ascii: bool) -> String {
    let name = truncate_width(&title_source(&sanitize_label(&row.source)), 26, ascii);
    let sessions = format_tokens(row.sessions as i64);
    let tokens = paint(color, &["\x1b[1m", "\x1b[32m"], &format_tokens(row.tokens.total_tokens));
    format_source_columns(
        &name,
        UnicodeWidthStr::width(name.as_str()),
        &sessions,
        UnicodeWidthStr::width(sessions.as_str()),
        &tokens,
        UnicodeWidthStr::width(format_tokens(row.tokens.total_tokens).as_str()),
    )
}

fn format_source_columns(
    source: &str,
    source_vis: usize,
    sessions: &str,
    sessions_vis: usize,
    tokens: &str,
    tokens_vis: usize,
) -> String {
    let left_pad = 2;
    let sessions_col = 8;
    let tokens_col = 10;
    let gap = 2;
    let source_col = INNER - left_pad - sessions_col - tokens_col - gap * 2;
    let source_pad = source_col.saturating_sub(source_vis);
    let sessions_pad = sessions_col.saturating_sub(sessions_vis);
    let tokens_pad = tokens_col.saturating_sub(tokens_vis);
    format!(
        "{}{source}{}{}{}{sessions}{}{}{tokens}",
        " ".repeat(left_pad),
        " ".repeat(source_pad),
        " ".repeat(gap),
        " ".repeat(sessions_pad),
        " ".repeat(gap),
        " ".repeat(tokens_pad),
    )
}
