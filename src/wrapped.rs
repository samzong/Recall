use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

mod render;

use anyhow::Result;
use chrono::{DateTime, Datelike, Days, Local, NaiveDate, TimeZone, Timelike};

use crate::db::search::{RepoFilter, first_valid_local_millis};
use crate::db::store::Store;
use crate::db::usage_store::UsageSlice;
use crate::project_scope::ProjectScope;
#[cfg(test)]
use crate::types::UsageEventRecord;
use crate::usage::TokenTotals;
use render::*;

pub(super) const INNER: usize = 52;
pub(super) const WEEKDAYS: [&str; 7] =
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
    #[cfg(test)]
    fn add(&mut self, event: &UsageEventRecord) {
        self.add_slice(&UsageSlice::from_event(event.clone()));
    }

    fn add_slice(&mut self, slice: &UsageSlice) {
        self.tokens.add_slice(slice);
        let added = slice.total_tokens();
        self.sessions.insert(slice.session_id.clone());

        let dt = Local
            .timestamp_millis_opt(slice.timestamp)
            .single()
            .unwrap_or_else(|| Local.timestamp_millis_opt(0).single().expect("epoch is valid"));
        self.days.insert(dt.date_naive());

        let source = self.by_source.entry(slice.source.clone()).or_default();
        source.tokens.add_slice(slice);
        source.sessions.insert(slice.session_id.clone());

        let model = if slice.model.is_empty() { "unknown" } else { slice.model.as_str() };
        let model_total = self.by_model.entry(model.to_string()).or_insert(0);
        *model_total = model_total.saturating_add(added);

        let weekday = dt.weekday().num_days_from_monday() as usize;
        let hour = dt.hour() as usize;
        self.slot_tokens[weekday][hour] = self.slot_tokens[weekday][hour].saturating_add(added);
        self.slot_events[weekday][hour] += slice.events;
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

pub(crate) fn build_wrapped_report_at(
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
        store.fold_usage_slices_after(None, after, WrappedAcc::default(), |acc, slice| {
            if allowed_ids.as_ref().is_none_or(|ids| ids.contains(&slice.session_id)) {
                acc.add_slice(&slice);
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

pub(super) fn sanitize_label(input: &str) -> String {
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

#[cfg(test)]
mod tests;

#[cfg(test)]
mod saturation_tests {
    use super::*;

    #[test]
    fn wrapped_models_and_slots_accumulate_after_global_saturation() {
        let mut acc = WrappedAcc::default();
        let mut slice = UsageSlice {
            session_id: "large".into(),
            source: "copilot-cli".into(),
            timestamp: 0,
            model: "first".into(),
            provider: "github".into(),
            input_tokens: i64::MAX,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
            token_source: "observed".into(),
            events: 1,
        };
        acc.add_slice(&slice);
        acc.add_slice(&slice);
        slice.model = "second".into();
        slice.input_tokens = 7;
        slice.timestamp = 86_400_000;
        acc.add_slice(&slice);
        assert_eq!(acc.tokens.total_tokens, i64::MAX);
        assert_eq!(acc.by_model["first"], i64::MAX);
        assert_eq!(acc.by_model["second"], 7);
        let dt = Local.timestamp_millis_opt(slice.timestamp).unwrap();
        assert_eq!(
            acc.slot_tokens[dt.weekday().num_days_from_monday() as usize][dt.hour() as usize],
            7
        );
    }
}
