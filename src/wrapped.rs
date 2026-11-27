use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, IsTerminal, Write};
use std::path::Path;

use anyhow::Result;
use chrono::{
    DateTime, Datelike, Days, Local, NaiveDate, NaiveDateTime, TimeDelta, TimeZone, Timelike,
};
use unicode_width::UnicodeWidthStr;

use crate::db::search::RepoFilter;
use crate::db::store::Store;
use crate::project_scope::ProjectScope;
use crate::types::UsageEventRecord;
use crate::usage::TokenTotals;

const INNER: usize = 52;
const WEEKDAYS: [&str; 7] =
    ["monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum WrappedPeriod {
    Week,
    Month,
    Year,
    All,
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
        let lookback_days = match self {
            Self::Week => 6,
            Self::Month => 29,
            Self::Year => 364,
            Self::All => return None,
        };
        let start = now
            .date_naive()
            .checked_sub_days(Days::new(lookback_days))?
            .and_hms_opt(0, 0, 0)
            .expect("midnight is a valid wall-clock value");
        Some(first_valid_local_millis(start, |candidate| {
            Local.from_local_datetime(&candidate).earliest().map(|value| value.timestamp_millis())
        }))
    }
}

fn first_valid_local_millis(
    start: NaiveDateTime,
    mut resolve: impl FnMut(NaiveDateTime) -> Option<i64>,
) -> i64 {
    (0_i64..=48 * 60)
        .find_map(|minutes| {
            start.checked_add_signed(TimeDelta::minutes(minutes)).and_then(&mut resolve)
        })
        .expect("local period boundary resolves within 48 hours")
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WrappedTopModel {
    pub(crate) model: String,
    pub(crate) tokens: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WrappedTopSource {
    pub(crate) source: String,
    pub(crate) tokens: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WrappedSourceRow {
    pub(crate) source: String,
    pub(crate) sessions: usize,
    pub(crate) tokens: TokenTotals,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WrappedReport {
    pub(crate) period: WrappedPeriod,
    pub(crate) project_label: Option<String>,
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
        let previous = self.tokens.total_tokens;
        self.tokens.add_event(event);
        let added = self.tokens.total_tokens - previous;
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

    fn finish(self, period: WrappedPeriod, project_label: Option<String>) -> WrappedReport {
        let has_usage = !self.days.is_empty();
        let empty = !has_usage && self.sessions.is_empty();
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
            period,
            project_label,
            empty,
            tokens: self.tokens,
            sessions: self.sessions.len(),
            active_days: self.days.len(),
            longest_streak: longest_streak(&self.days),
            top_model: if has_usage { top_model } else { None },
            top_source: if has_usage { top_source } else { None },
            busiest_weekday,
            busiest_hour,
            by_source,
        }
    }
}

pub(crate) fn run_card(period: WrappedPeriod, project_filter: Option<&str>) -> Result<()> {
    if let Some(project) = project_filter
        && project.trim().is_empty()
    {
        anyhow::bail!("project filter cannot be empty");
    }
    let mut progress = StderrProgress::new();
    let report = crate::sync::run_usage_sync_job_with_progress(&mut |source: &str| {
        progress.show_source(source);
    })
    .and_then(|()| Store::open())
    .and_then(|store| build_wrapped_report(&store, period, project_filter));
    progress.clear();
    print!("{}", render_card(&report?, color_enabled()));
    Ok(())
}

pub(crate) fn build_wrapped_report(
    store: &Store,
    period: WrappedPeriod,
    project_filter: Option<&str>,
) -> Result<WrappedReport> {
    build_wrapped_report_at(store, period, project_filter, Local::now())
}

fn build_wrapped_report_at(
    store: &Store,
    period: WrappedPeriod,
    project_filter: Option<&str>,
    now: DateTime<Local>,
) -> Result<WrappedReport> {
    let scope = match project_filter {
        Some(project) => Some(store.resolve_cli_project_selector(project)?),
        None => None,
    };
    let project_label = scope.as_ref().and_then(wrapped_project_label);
    let allowed_ids = match scope.as_ref() {
        Some(ProjectScope::Global) | None => None,
        Some(scope) => Some(store.session_ids_in_scope(scope)?),
    };
    let after = period.after_millis_at(now);
    let mut acc =
        store.fold_usage_events_after(None, after, WrappedAcc::default(), |acc, event| {
            if allowed_ids.as_ref().is_none_or(|ids| ids.contains(&event.session_id)) {
                acc.add(&event);
            }
        })?;
    for (source, session_id) in store.indexed_active_sessions_after(after)? {
        if allowed_ids.as_ref().is_none_or(|ids| ids.contains(&session_id)) {
            acc.sessions.insert(session_id.clone());
            acc.by_source.entry(source).or_default().sessions.insert(session_id);
        }
    }
    Ok(acc.finish(period, project_label))
}

fn wrapped_project_label(scope: &ProjectScope) -> Option<String> {
    match scope {
        ProjectScope::Global => None,
        ProjectScope::Repository { filter, .. } => {
            let label = match filter {
                RepoFilter::Remote(remote) => {
                    remote.split_once('/').map(|(_, slug)| slug).unwrap_or(remote)
                }
                RepoFilter::Slug(slug) => slug.as_str(),
                RepoFilter::Name(name) => name.as_str(),
            };
            Some(label.to_string())
        }
        ProjectScope::Directory(dir) => Some(shorten_wrapped_directory(dir)),
    }
}

fn shorten_wrapped_directory(dir: &str) -> String {
    let trimmed = dir.trim();
    let normalized = if trimmed == "/" || trimmed == "\\" {
        trimmed
    } else {
        trimmed.trim_end_matches(['/', '\\'])
    };

    if let Some(home) = dirs::home_dir()
        && let Ok(rel) = Path::new(normalized).strip_prefix(&home)
    {
        let rel_str = rel.to_string_lossy();
        let segs: Vec<&str> = rel_str.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
        return format_home_segments(&segs);
    }

    let starts_slash = normalized.starts_with('/');
    let segs: Vec<&str> = normalized.split(['/', '\\']).filter(|s| !s.is_empty()).collect();

    if starts_slash && segs.len() >= 2 && (segs[0] == "Users" || segs[0] == "home") {
        return format_home_segments(&segs[2..]);
    }

    if !starts_slash
        && segs.len() >= 3
        && segs[0].len() == 2
        && segs[0].as_bytes()[0].is_ascii_alphabetic()
        && segs[0].as_bytes()[1] == b':'
        && segs[1].eq_ignore_ascii_case("Users")
    {
        return format_home_segments(&segs[3..]);
    }

    match segs.len() {
        0 => {
            if starts_slash {
                "/".to_string()
            } else {
                normalized.to_string()
            }
        }
        1 => {
            if starts_slash {
                format!("/{}", segs[0])
            } else {
                segs[0].to_string()
            }
        }
        2 if starts_slash => format!("/{}/{}", segs[0], segs[1]),
        n => format!("{}/{}", segs[n - 2], segs[n - 1]),
    }
}

fn format_home_segments(segs: &[&str]) -> String {
    match segs.len() {
        0 => "~".to_string(),
        1 => format!("~/{}", segs[0]),
        n => format!("~/{}/{}", segs[n - 2], segs[n - 1]),
    }
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

#[cfg(test)]
mod tests;
