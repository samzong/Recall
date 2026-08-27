use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, IsTerminal, Write};

use anyhow::Result;
use chrono::{DateTime, Datelike, Local, NaiveDate, TimeZone, Timelike};
use serde::Serialize;
use unicode_width::UnicodeWidthStr;

use crate::db::search::TimeRange;
use crate::db::store::Store;
use crate::types::UsageEventRecord;
use crate::usage::{TokenTotals, UsageDedup};

const INNER: usize = 52;
const YEAR_MS: i64 = 365 * 24 * 3600 * 1000;
const WEEKDAYS: [&str; 7] =
    ["monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WrappedPeriod {
    Week,
    Month,
    Year,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum WrappedFormat {
    Text,
    Json,
}

impl WrappedPeriod {
    fn card_label(self) -> &'static str {
        match self {
            Self::Week => "last 7 days",
            Self::Month => "last 30 days",
            Self::Year => "last 365 days",
            Self::All => "all time",
        }
    }

    fn after_millis_at(self, now: DateTime<Local>) -> Option<i64> {
        match self {
            Self::Week => TimeRange::Week.cutoff_millis_at(now),
            Self::Month => TimeRange::Month.cutoff_millis_at(now),
            Self::Year => Some(now.timestamp_millis() - YEAR_MS),
            Self::All => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub(crate) struct WrappedTopModel {
    pub(crate) model: String,
    pub(crate) tokens: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub(crate) struct WrappedTopSource {
    pub(crate) source: String,
    pub(crate) tokens: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub(crate) struct WrappedSourceRow {
    pub(crate) source: String,
    pub(crate) sessions: usize,
    pub(crate) tokens: TokenTotals,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub(crate) struct WrappedReport {
    pub(crate) protocol_version: u32,
    pub(crate) period: WrappedPeriod,
    pub(crate) empty: bool,
    pub(crate) tokens: TokenTotals,
    pub(crate) sessions: usize,
    pub(crate) active_days: usize,
    pub(crate) longest_streak: usize,
    pub(crate) top_model: Option<WrappedTopModel>,
    pub(crate) top_source: Option<WrappedTopSource>,
    pub(crate) busiest_weekday: Option<String>,
    pub(crate) busiest_hour: Option<u8>,
    pub(crate) by_source: Vec<WrappedSourceRow>,
}

#[derive(Default)]
struct SourceAcc {
    tokens: TokenTotals,
    sessions: BTreeSet<String>,
}

#[derive(Default)]
struct WrappedAcc {
    dedup: UsageDedup,
    tokens: TokenTotals,
    sessions: BTreeSet<String>,
    days: BTreeSet<NaiveDate>,
    by_source: BTreeMap<String, SourceAcc>,
    by_model: BTreeMap<String, i64>,
    slot_tokens: [[i64; 24]; 7],
    slot_events: [[usize; 24]; 7],
}

impl WrappedAcc {
    fn add(&mut self, event: &UsageEventRecord) {
        if !self.dedup.accept(event) {
            return;
        }

        let added = event_tokens(event);
        self.tokens.add_event(event);
        self.sessions.insert(event.session_id.clone());

        let dt = Local
            .timestamp_millis_opt(event.timestamp)
            .single()
            .unwrap_or_else(|| Local.timestamp_millis_opt(0).single().expect("epoch is valid"));
        self.days.insert(dt.date_naive());

        let source = self.by_source.entry(event.source.clone()).or_default();
        source.tokens.add_event(event);
        source.sessions.insert(event.session_id.clone());

        let model = if event.model.is_empty() { "unknown" } else { event.model.as_str() };
        *self.by_model.entry(model.to_string()).or_insert(0) += added;

        let weekday = dt.weekday().num_days_from_monday() as usize;
        let hour = dt.hour() as usize;
        self.slot_tokens[weekday][hour] += added;
        self.slot_events[weekday][hour] += 1;
    }

    fn finish(self, period: WrappedPeriod) -> WrappedReport {
        let empty = self.days.is_empty();
        let mut by_source = self
            .by_source
            .into_iter()
            .map(|(source, acc)| WrappedSourceRow {
                source,
                sessions: acc.sessions.len(),
                tokens: acc.tokens,
            })
            .collect::<Vec<_>>();
        by_source.sort_by(|a, b| {
            b.tokens.total_tokens.cmp(&a.tokens.total_tokens).then_with(|| a.source.cmp(&b.source))
        });

        let top_source = by_source.first().map(|row| WrappedTopSource {
            source: row.source.clone(),
            tokens: row.tokens.total_tokens,
        });
        let top_model = self
            .by_model
            .into_iter()
            .max_by(|(left_name, left_tokens), (right_name, right_tokens)| {
                left_tokens.cmp(right_tokens).then_with(|| right_name.cmp(left_name))
            })
            .map(|(model, tokens)| WrappedTopModel { model, tokens });

        let (busiest_weekday, busiest_hour) =
            match busiest_slot(&self.slot_tokens, &self.slot_events) {
                Some((weekday, hour)) => (Some(weekday), Some(hour)),
                None => (None, None),
            };

        WrappedReport {
            protocol_version: crate::PROTOCOL_VERSION,
            period,
            empty,
            tokens: self.tokens,
            sessions: self.sessions.len(),
            active_days: self.days.len(),
            longest_streak: longest_streak(&self.days),
            top_model: if empty { None } else { top_model },
            top_source: if empty { None } else { top_source },
            busiest_weekday,
            busiest_hour,
            by_source,
        }
    }
}

pub(crate) fn run_cli(period: WrappedPeriod, format: WrappedFormat) -> Result<()> {
    let mut progress = StderrProgress::new();
    let report = crate::sync::run_usage_sync_job_with_progress(&mut |source: &str| {
        progress.show_source(source);
    })
    .and_then(|()| Store::open())
    .and_then(|store| build_wrapped_report(&store, period));
    progress.clear();
    let report = report?;
    match format {
        WrappedFormat::Json => println!("{}", serde_json::to_string_pretty(&report)?),
        WrappedFormat::Text => print!("{}", render_card(&report, color_enabled())),
    }
    Ok(())
}

pub(crate) fn build_wrapped_report(store: &Store, period: WrappedPeriod) -> Result<WrappedReport> {
    build_wrapped_report_at(store, period, Local::now())
}

fn build_wrapped_report_at(
    store: &Store,
    period: WrappedPeriod,
    now: DateTime<Local>,
) -> Result<WrappedReport> {
    let after = period.after_millis_at(now);
    let acc = store.fold_usage_events_after(None, after, WrappedAcc::default(), |acc, event| {
        acc.add(&event);
    })?;
    Ok(acc.finish(period))
}

fn event_tokens(event: &UsageEventRecord) -> i64 {
    event.input_tokens.max(0)
        + event.output_tokens.max(0)
        + event.cache_read_tokens.max(0)
        + event.cache_write_tokens.max(0)
        + event.reasoning_tokens.max(0)
}

fn longest_streak(days: &BTreeSet<NaiveDate>) -> usize {
    let mut best = 0usize;
    let mut current = 0usize;
    let mut prev: Option<NaiveDate> = None;
    for day in days {
        current = if prev
            .and_then(|p| p.checked_add_signed(chrono::TimeDelta::days(1)))
            .is_some_and(|expected| expected == *day)
        {
            current + 1
        } else {
            1
        };
        best = best.max(current);
        prev = Some(*day);
    }
    best
}

fn busiest_slot(tokens: &[[i64; 24]; 7], events: &[[usize; 24]; 7]) -> Option<(String, u8)> {
    if events.iter().flatten().all(|count| *count == 0) {
        return None;
    }
    let index = (0..168).max_by(|&left, &right| {
        let (left_day, left_hour) = (left / 24, left % 24);
        let (right_day, right_hour) = (right / 24, right % 24);
        tokens[left_day][left_hour]
            .cmp(&tokens[right_day][right_hour])
            .then(events[left_day][left_hour].cmp(&events[right_day][right_hour]))
            .then(right.cmp(&left))
    })?;
    Some((WEEKDAYS[index / 24].to_string(), u8::try_from(index % 24).expect("hour fits u8")))
}

fn sanitize_label(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    for next in chars.by_ref() {
                        if next.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(next) = chars.next() {
                        if next == '\u{07}' {
                            break;
                        }
                        if next == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
        } else if !ch.is_control() {
            output.push(ch);
        }
    }
    output
}

fn color_enabled() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

struct StderrProgress {
    enabled: bool,
    color: bool,
    last_width: usize,
}

impl StderrProgress {
    fn new() -> Self {
        let enabled = io::stderr().is_terminal();
        Self { enabled, color: enabled && std::env::var_os("NO_COLOR").is_none(), last_width: 0 }
    }

    fn show_source(&mut self, source: &str) {
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

    fn clear(&mut self) {
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

fn format_tokens(value: i64) -> String {
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

fn title_source(id: &str) -> String {
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

fn weekday_label(name: &str) -> String {
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

fn visible_width(text: &str) -> usize {
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

fn render_card(report: &WrappedReport, color: bool) -> String {
    let ascii = !color;
    let borders = borders(ascii);
    let mut out = String::new();
    out.push_str(&rule(&borders, borders.tl, borders.tr, color));
    out.push_str(&row(
        &borders,
        color,
        &center_painted("RECALL WRAPPED", INNER, ascii, color, &["\x1b[1m", "\x1b[36m"]),
    ));
    out.push_str(&row(
        &borders,
        color,
        &center_painted(report.period.card_label(), INNER, ascii, color, &["\x1b[2m"]),
    ));
    out.push_str(&rule(&borders, borders.ml, borders.mr, color));
    out.push_str(&blank(&borders, color));

    if report.empty {
        out.push_str(&row(
            &borders,
            color,
            &center_plain("No usage data for this period.", INNER, ascii),
        ));
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

#[cfg(test)]
fn aggregate_wrapped_events(
    events: &[UsageEventRecord],
    period: WrappedPeriod,
    now: DateTime<Local>,
) -> WrappedReport {
    let after = period.after_millis_at(now);
    let mut acc = WrappedAcc::default();
    for event in events {
        if after.is_some_and(|cutoff| event.timestamp < cutoff) {
            continue;
        }
        acc.add(event);
    }
    acc.finish(period)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Message, RawUsageEvent, Role, Session, TokenSource};
    use crate::usage::aggregate_usage_events;

    fn event(
        source: &str,
        session_id: &str,
        event_key: &str,
        timestamp: i64,
        model: &str,
        input: i64,
        output: i64,
    ) -> UsageEventRecord {
        UsageEventRecord {
            session_id: session_id.to_string(),
            source: source.to_string(),
            source_id: session_id.to_string(),
            event_key: event_key.to_string(),
            timestamp,
            model: model.to_string(),
            provider: "test".to_string(),
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
            token_source: "observed".to_string(),
        }
    }

    fn local_ts(year: i32, month: u32, day: u32, hour: u32) -> i64 {
        Local
            .with_ymd_and_hms(year, month, day, hour, 0, 0)
            .single()
            .expect("valid local time")
            .timestamp_millis()
    }

    fn fixture_now() -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 8, 26, 12, 0, 0).single().expect("valid local time")
    }

    fn populated_report() -> WrappedReport {
        let now = fixture_now();
        let events = vec![
            event(
                "claude-code",
                "s1",
                "a:1",
                local_ts(2026, 8, 20, 14),
                "claude-sonnet",
                800_000,
                200_000,
            ),
            event(
                "claude-code",
                "s1",
                "a:2",
                local_ts(2026, 8, 21, 14),
                "claude-sonnet",
                100_000,
                50_000,
            ),
            event("codex", "s2", "b:1", local_ts(2026, 8, 22, 9), "gpt-5.5", 80_000, 20_000),
            event("codex", "s3", "b:2", local_ts(2026, 8, 24, 9), "gpt-5.5", 10_000, 5_000),
        ];
        aggregate_wrapped_events(&events, WrappedPeriod::Month, now)
    }

    #[test]
    fn format_tokens_uses_compact_units() {
        assert_eq!(format_tokens(42), "42");
        assert_eq!(format_tokens(1_200), "1.2K");
        assert_eq!(format_tokens(1_200_000), "1.2M");
        assert_eq!(format_tokens(1_200_000_000), "1.20B");
    }

    #[test]
    fn title_source_splits_tool_ids() {
        assert_eq!(title_source("claude-code"), "Claude Code");
        assert_eq!(title_source("codex"), "Codex");
    }

    #[test]
    fn longest_streak_counts_consecutive_days_only() {
        let days = BTreeSet::from([
            NaiveDate::from_ymd_opt(2026, 8, 20).unwrap(),
            NaiveDate::from_ymd_opt(2026, 8, 21).unwrap(),
            NaiveDate::from_ymd_opt(2026, 8, 22).unwrap(),
            NaiveDate::from_ymd_opt(2026, 8, 24).unwrap(),
        ]);
        assert_eq!(longest_streak(&days), 3);
        assert_eq!(longest_streak(&BTreeSet::new()), 0);
    }

    #[test]
    fn empty_period_is_friendly_and_not_an_error() {
        let report = aggregate_wrapped_events(&[], WrappedPeriod::Week, fixture_now());
        assert!(report.empty);
        assert_eq!(report.sessions, 0);
        assert_eq!(report.active_days, 0);
        assert_eq!(report.longest_streak, 0);
        assert!(report.top_model.is_none());
        assert!(report.busiest_weekday.is_none());

        let card = render_card(&report, false);
        assert!(card.contains("No usage data for this period."));
        assert!(card.contains("last 7 days"));
        assert!(!card.contains('\u{1b}'));
        assert!(card.starts_with('+'));
        assert_eq!(card.chars().filter(|ch| *ch == '\n').count(), 9);
        for line in card.lines() {
            assert_eq!(visible_width(line), INNER + 2, "{line}");
        }
    }

    #[test]
    fn populated_report_aggregates_highlights() {
        let report = populated_report();
        assert!(!report.empty);
        assert_eq!(report.sessions, 3);
        assert_eq!(report.active_days, 4);
        assert_eq!(report.longest_streak, 3);
        assert_eq!(report.tokens.input_tokens, 990_000);
        assert_eq!(report.tokens.output_tokens, 275_000);
        assert_eq!(report.tokens.total_tokens, 1_265_000);
        assert_eq!(report.top_model.as_ref().map(|m| m.model.as_str()), Some("claude-sonnet"));
        assert_eq!(report.top_source.as_ref().map(|s| s.source.as_str()), Some("claude-code"));
        assert_eq!(report.busiest_weekday.as_deref(), Some("thursday"));
        assert_eq!(report.busiest_hour, Some(14));
        assert_eq!(report.by_source.len(), 2);
        assert_eq!(report.by_source[0].source, "claude-code");
        assert_eq!(report.by_source[0].sessions, 1);
    }

    #[test]
    fn period_cutoff_matches_usage_windows() {
        let now = fixture_now();
        let events = vec![
            event("codex", "s1", "k1", local_ts(2026, 8, 24, 12), "gpt-5.5", 10, 1),
            event("codex", "s2", "k2", local_ts(2026, 8, 10, 12), "gpt-5.5", 10, 1),
            event("codex", "s3", "k3", local_ts(2026, 4, 1, 12), "gpt-5.5", 10, 1),
            event("codex", "s4", "k4", local_ts(2025, 6, 1, 12), "gpt-5.5", 10, 1),
        ];

        assert_eq!(aggregate_wrapped_events(&events, WrappedPeriod::Week, now).sessions, 1);
        assert_eq!(aggregate_wrapped_events(&events, WrappedPeriod::Month, now).sessions, 2);
        assert_eq!(aggregate_wrapped_events(&events, WrappedPeriod::Year, now).sessions, 3);
        assert_eq!(aggregate_wrapped_events(&events, WrappedPeriod::All, now).sessions, 4);
    }

    #[test]
    fn wrapped_totals_match_usage_report_dedup() {
        let events = vec![
            event("codex", "session-a", "token_count:1", 1_800_000_000_000, "gpt-5.5", 8, 3),
            event("codex", "session-b", "token_count:9", 1_800_000_000_000, "gpt-5.5", 8, 3),
        ];
        let usage = aggregate_usage_events(&events);
        let wrapped = aggregate_wrapped_events(&events, WrappedPeriod::All, fixture_now());
        assert_eq!(wrapped.tokens, usage.summary.tokens);
        assert_eq!(wrapped.sessions, usage.summary.sessions);
    }

    #[test]
    fn card_is_fixed_width_and_color_optional() {
        let report = populated_report();
        let plain = render_card(&report, false);
        let colored = render_card(&report, true);

        assert!(plain.contains("RECALL WRAPPED"));
        assert!(plain.contains("last 30 days"));
        assert!(plain.contains("1.3M"));
        assert!(plain.contains("Claude Code"));
        assert!(plain.contains("Thursday at 14:00"));
        assert!(plain.contains("longest streak"));
        assert!(!plain.contains('\u{1b}'));
        assert!(colored.contains("\x1b["));
        assert!(colored.contains("RECALL WRAPPED"));

        for line in plain.lines() {
            assert_eq!(visible_width(line), INNER + 2, "{line}");
        }
        for line in colored.lines() {
            assert_eq!(visible_width(line), INNER + 2, "{line}");
        }
    }

    #[test]
    fn json_uses_stable_snake_case_fields() {
        let report = populated_report();
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["protocol_version"], crate::PROTOCOL_VERSION);
        assert_eq!(value["period"], "month");
        assert_eq!(value["empty"], false);
        assert_eq!(value["busiest_weekday"], "thursday");
        assert_eq!(value["busiest_hour"], 14);
        assert_eq!(value["top_model"]["model"], "claude-sonnet");
        assert_eq!(value["top_source"]["source"], "claude-code");
        assert_eq!(value["tokens"]["input_tokens"], 990_000);
        assert_eq!(value["by_source"][0]["source"], "claude-code");
        assert!(value.get("costUsd").is_none());
    }

    fn make_session(id: &str, source: &str) -> Session {
        Session {
            id: id.to_string(),
            source: source.to_string(),
            source_id: id.to_string(),
            title: id.to_string(),
            directory: Some("/tmp/test".to_string()),
            repo_remote: None,
            repo_slug: None,
            repo_name: None,
            started_at: local_ts(2026, 8, 20, 14),
            updated_at: None,
            message_count: 1,
            entrypoint: None,
            custom_title: None,
            summary: None,
            duration_minutes: None,
            source_file_path: None,
            is_import: false,
        }
    }

    fn make_usage(
        key: &str,
        timestamp: i64,
        model: &str,
        input: i64,
        output: i64,
    ) -> RawUsageEvent {
        RawUsageEvent {
            event_key: key.to_string(),
            event_seq: 0,
            message_seq: Some(1),
            timestamp,
            model: model.to_string(),
            provider: "test".to_string(),
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
            token_source: TokenSource::Observed,
            parser_version: 1,
            source_path: None,
            raw_usage_json: None,
        }
    }

    #[test]
    fn period_card_labels_describe_rolling_windows() {
        assert_eq!(WrappedPeriod::Week.card_label(), "last 7 days");
        assert_eq!(WrappedPeriod::Month.card_label(), "last 30 days");
        assert_eq!(WrappedPeriod::Year.card_label(), "last 365 days");
        assert_eq!(WrappedPeriod::All.card_label(), "all time");
        let week = serde_json::to_value(WrappedPeriod::Week).unwrap();
        let month = serde_json::to_value(WrappedPeriod::Month).unwrap();
        let year = serde_json::to_value(WrappedPeriod::Year).unwrap();
        let all = serde_json::to_value(WrappedPeriod::All).unwrap();
        assert_eq!(week, "week");
        assert_eq!(month, "month");
        assert_eq!(year, "year");
        assert_eq!(all, "all");
    }

    #[test]
    fn busiest_slot_uses_actual_weekday_hour_pairs() {
        let now = fixture_now();
        let events = vec![
            event("codex", "s1", "k1", local_ts(2026, 8, 24, 14), "gpt-5.5", 100, 0),
            event("codex", "s2", "k2", local_ts(2026, 8, 25, 9), "gpt-5.5", 60, 0),
            event("codex", "s3", "k3", local_ts(2026, 8, 26, 9), "gpt-5.5", 60, 0),
        ];
        let report = aggregate_wrapped_events(&events, WrappedPeriod::Month, now);
        assert_eq!(report.busiest_weekday.as_deref(), Some("monday"));
        assert_eq!(report.busiest_hour, Some(14));
        let card = render_card(&report, false);
        assert!(card.contains("Monday at 14:00"));
        assert!(!card.contains("Monday at 09:00"));
    }

    #[test]
    fn card_sanitizes_model_and_source_labels_json_stays_verbatim() {
        let now = fixture_now();
        let model = "gpt-5\u{1b}]52;c;ZW1iZWQ=\u{07}\n\u{1b}[31m";
        let source = "claude-code\u{1b}[32m\ninjected";
        let events = vec![event(source, "s1", "k1", local_ts(2026, 8, 24, 14), model, 10, 0)];
        let report = aggregate_wrapped_events(&events, WrappedPeriod::All, now);
        assert_eq!(report.top_model.as_ref().map(|m| m.model.as_str()), Some(model));
        assert_eq!(report.top_source.as_ref().map(|s| s.source.as_str()), Some(source));
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\\u001b]52;c;ZW1iZWQ=\\u0007"));
        assert!(json.contains("claude-code\\u001b[32m\\ninjected"));

        let card = render_card(&report, false);
        assert!(card.contains("gpt-5"));
        assert!(card.contains("Claude Codeinjected"));
        assert!(!card.contains('\u{1b}'));
        assert!(!card.contains('\u{07}'));
        assert!(!card.contains("]52;"));
        assert!(!card.contains("[31m"));
        assert!(!card.contains("[32m"));
        for line in card.lines() {
            assert_eq!(visible_width(line), INNER + 2, "{line}");
        }
    }

    #[test]
    fn store_backed_report_reads_existing_usage_events() {
        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let session = make_session("s1", "claude-code");
        let messages = vec![Message {
            session_id: "s1".to_string(),
            role: Role::User,
            content: "hello".to_string(),
            timestamp: Some(local_ts(2026, 8, 20, 14)),
            seq: 0,
        }];
        store
            .persist_session_with_usage(
                &session,
                &messages,
                &[make_usage("evt-1", local_ts(2026, 8, 20, 14), "claude-sonnet", 100, 20)],
                Some(1),
            )
            .unwrap();

        let report = build_wrapped_report_at(&store, WrappedPeriod::All, fixture_now()).unwrap();
        assert!(!report.empty);
        assert_eq!(report.sessions, 1);
        assert_eq!(report.tokens.total_tokens, 120);
        assert_eq!(report.top_source.unwrap().source, "claude-code");

        let week_cutoff_now =
            Local.with_ymd_and_hms(2026, 9, 20, 12, 0, 0).single().expect("valid local time");
        let empty = build_wrapped_report_at(&store, WrappedPeriod::Week, week_cutoff_now).unwrap();
        assert!(empty.empty);
    }

    #[test]
    fn progress_overwrite_pads_when_the_next_label_is_shorter() {
        let (first, width) = progress_overwrite(0, "abcdef");
        assert_eq!(first, "\rabcdef");
        assert_eq!(width, 6);
        let (second, width) = progress_overwrite(width, "xy");
        assert_eq!(second, "\rxy    ");
        assert_eq!(width, 2);
        let colored = "\x1b[1m\x1b[36mxy\x1b[0m";
        let (third, width) = progress_overwrite(6, colored);
        assert_eq!(third, format!("\r{colored}    "));
        assert_eq!(width, 2);
        assert_eq!(progress_clear(5), "\r     \r");
        assert_eq!(progress_clear(0), "");
    }
}
