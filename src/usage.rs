use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashSet};

use anyhow::Result;
use chrono::{Datelike, Local, TimeZone};
use serde::Serialize;

use crate::db::search::TimeRange;
use crate::db::store::Store;
use crate::types::UsageEventRecord;

#[derive(Debug, Clone)]
pub struct UsageFilters {
    pub sources: Option<Vec<String>>,
    pub time_range: TimeRange,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct TokenTotals {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub reasoning_tokens: i64,
    pub total_tokens: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageSummary {
    pub events: usize,
    pub sessions: usize,
    pub sources: usize,
    pub models: usize,
    pub token_source_events: BTreeMap<String, usize>,
    pub tokens: TokenTotals,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceUsage {
    pub source: String,
    pub sessions: usize,
    pub events: usize,
    pub tokens: TokenTotals,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelUsage {
    pub source: String,
    pub provider: String,
    pub model: String,
    pub sessions: usize,
    pub events: usize,
    pub tokens: TokenTotals,
}

#[derive(Debug, Clone, Serialize)]
pub struct PeriodUsage {
    pub period: String,
    pub sessions: usize,
    pub events: usize,
    pub tokens: TokenTotals,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageReport {
    pub summary: UsageSummary,
    pub by_source: Vec<SourceUsage>,
    pub by_model: Vec<ModelUsage>,
    pub daily: Vec<PeriodUsage>,
    pub weekly: Vec<PeriodUsage>,
    pub monthly: Vec<PeriodUsage>,
}

#[derive(Default)]
struct Accumulator {
    tokens: TokenTotals,
    sessions: BTreeSet<String>,
    events: usize,
}

impl Accumulator {
    fn add(&mut self, event: &UsageEventRecord) {
        self.tokens.input_tokens += event.input_tokens.max(0);
        self.tokens.output_tokens += event.output_tokens.max(0);
        self.tokens.cache_read_tokens += event.cache_read_tokens.max(0);
        self.tokens.cache_write_tokens += event.cache_write_tokens.max(0);
        self.tokens.reasoning_tokens += event.reasoning_tokens.max(0);
        self.tokens.total_tokens = self.tokens.input_tokens
            + self.tokens.output_tokens
            + self.tokens.cache_read_tokens
            + self.tokens.cache_write_tokens
            + self.tokens.reasoning_tokens;
        self.sessions.insert(event.session_id.clone());
        self.events += 1;
    }
}

pub fn build_usage_report(store: &Store, filters: &UsageFilters) -> Result<UsageReport> {
    let events = store.list_usage_events(filters.sources.as_deref(), filters.time_range)?;
    Ok(aggregate_usage_events(&events))
}

pub fn aggregate_usage_events(events: &[UsageEventRecord]) -> UsageReport {
    let events = dedupe_report_events(events);
    let mut total = Accumulator::default();
    let mut by_source: BTreeMap<String, Accumulator> = BTreeMap::new();
    let mut by_model: BTreeMap<(String, String, String), Accumulator> = BTreeMap::new();
    let mut daily: BTreeMap<String, Accumulator> = BTreeMap::new();
    let mut weekly: BTreeMap<String, Accumulator> = BTreeMap::new();
    let mut monthly: BTreeMap<String, Accumulator> = BTreeMap::new();
    let mut token_source_events = BTreeMap::new();

    for event in events {
        total.add(event);
        *token_source_events.entry(event.token_source.clone()).or_insert(0) += 1;

        by_source.entry(event.source.clone()).or_default().add(event);
        by_model
            .entry((event.source.clone(), event.provider.clone(), event.model.clone()))
            .or_default()
            .add(event);

        let (day, week, month) = period_keys(event.timestamp);
        daily.entry(day).or_default().add(event);
        weekly.entry(week).or_default().add(event);
        monthly.entry(month).or_default().add(event);
    }

    let source_count = by_source.len();
    let model_count = by_model.len();

    let mut by_source = by_source
        .into_iter()
        .map(|(source, acc)| SourceUsage {
            source,
            sessions: acc.sessions.len(),
            events: acc.events,
            tokens: acc.tokens,
        })
        .collect::<Vec<_>>();
    by_source.sort_by_key(|source| Reverse(source.tokens.total_tokens));

    let mut by_model = by_model
        .into_iter()
        .map(|((source, provider, model), acc)| ModelUsage {
            source,
            provider,
            model,
            sessions: acc.sessions.len(),
            events: acc.events,
            tokens: acc.tokens,
        })
        .collect::<Vec<_>>();
    by_model.sort_by_key(|model| Reverse(model.tokens.total_tokens));

    UsageReport {
        summary: UsageSummary {
            events: total.events,
            sessions: total.sessions.len(),
            sources: source_count,
            models: model_count,
            token_source_events,
            tokens: total.tokens,
        },
        by_source,
        by_model,
        daily: period_vec(daily),
        weekly: period_vec(weekly),
        monthly: period_vec(monthly),
    }
}

fn dedupe_report_events(events: &[UsageEventRecord]) -> Vec<&UsageEventRecord> {
    let mut codex_seen: HashSet<String> = HashSet::new();
    let mut claude_seen: HashSet<String> = HashSet::new();
    let mut deduped = Vec::with_capacity(events.len());

    for event in events {
        if event.source == "codex" {
            let key = format!(
                "codex:token_count:{}:{}:{}:{}:{}:{}:{}:{}",
                event.timestamp,
                event.provider,
                event.model,
                event.input_tokens,
                event.output_tokens,
                event.cache_read_tokens,
                event.cache_write_tokens,
                event.reasoning_tokens
            );
            if !codex_seen.insert(key) {
                continue;
            }
        } else if event.source == "claude-code"
            && event.event_key.starts_with("assistant:")
            && !event.event_key.contains(":line:")
            && !claude_seen.insert(event.event_key.clone())
        {
            continue;
        }

        deduped.push(event);
    }

    deduped
}

fn period_vec(map: BTreeMap<String, Accumulator>) -> Vec<PeriodUsage> {
    map.into_iter()
        .map(|(period, acc)| PeriodUsage {
            period,
            sessions: acc.sessions.len(),
            events: acc.events,
            tokens: acc.tokens,
        })
        .collect()
}

fn period_keys(timestamp_ms: i64) -> (String, String, String) {
    let dt = Local
        .timestamp_millis_opt(timestamp_ms)
        .single()
        .unwrap_or_else(|| Local.timestamp_millis_opt(0).single().expect("epoch is valid"));
    let date = dt.date_naive();
    let iso = date.iso_week();
    (
        date.format("%Y-%m-%d").to_string(),
        format!("{}-W{:02}", iso.year(), iso.week()),
        date.format("%Y-%m").to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(source: &str, session_id: &str, event_key: &str, timestamp: i64) -> UsageEventRecord {
        UsageEventRecord {
            session_id: session_id.to_string(),
            source: source.to_string(),
            source_id: session_id.to_string(),
            event_key: event_key.to_string(),
            timestamp,
            model: "gpt-5.5".to_string(),
            provider: "openai".to_string(),
            input_tokens: 8,
            output_tokens: 3,
            cache_read_tokens: 2,
            cache_write_tokens: 0,
            reasoning_tokens: 1,
            token_source: "derived".to_string(),
        }
    }

    #[test]
    fn aggregate_usage_dedupes_codex_token_count_across_sessions() {
        let events = vec![
            event("codex", "session-a", "token_count:1", 1_800_000_000_000),
            event("codex", "session-b", "token_count:9", 1_800_000_000_000),
        ];

        let report = aggregate_usage_events(&events);

        assert_eq!(report.summary.events, 1);
        assert_eq!(report.summary.sessions, 1);
        assert_eq!(report.summary.tokens.total_tokens, 14);
    }

    #[test]
    fn aggregate_usage_keeps_codex_identical_deltas_at_distinct_timestamps() {
        let events = vec![
            event("codex", "session-a", "token_count:1", 1_800_000_000_000),
            event("codex", "session-a", "token_count:2", 1_800_000_001_000),
        ];

        let report = aggregate_usage_events(&events);

        assert_eq!(report.summary.events, 2);
        assert_eq!(report.summary.tokens.total_tokens, 28);
    }

    #[test]
    fn aggregate_usage_dedupes_claude_events_with_stable_message_ids() {
        let mut first = event("claude-code", "session-a", "assistant:req-1:msg-1", 1);
        first.provider = "anthropic".to_string();
        first.model = "claude-sonnet".to_string();
        first.token_source = "observed".to_string();
        let mut duplicate = first.clone();
        duplicate.session_id = "session-b".to_string();

        let report = aggregate_usage_events(&[first, duplicate]);

        assert_eq!(report.summary.events, 1);
        assert_eq!(report.summary.tokens.total_tokens, 14);
    }
}
