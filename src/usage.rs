pub(crate) mod project;

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashSet};

use anyhow::{Result, bail};
use chrono::{DateTime, Datelike, Local, NaiveDate, TimeZone, Timelike};
use serde::Serialize;

use crate::adapters;
use crate::cli::usage_args::UsageArgs;
use crate::db::search::TimeRange;
use crate::db::store::Store;
use crate::project_scope::ProjectScope;
use crate::query::{parse_time_range, resolve_source_filter};
use crate::sync::run_usage_sync_job;
use crate::types::UsageEventRecord;

pub(crate) use project::{
    ProjectPartitionKey, ProjectUsageEntry, ProjectUsageReport, UsageTarget,
    build_dashboard_bundle, build_project_usage_report, matches_target, session_partition_key,
};
#[cfg(test)]
pub(crate) use project::{ProjectUsageSummary, build_dashboard_bundle_at};

#[derive(Debug, Clone)]
pub(crate) struct UsageStart {
    pub(crate) sources: Option<Vec<String>>,
    pub(crate) time_range: Option<TimeRange>,
    pub(crate) initial_scope: ProjectScope,
    pub(crate) open_projects_tab: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct UsageFilters {
    pub(crate) sources: Option<Vec<String>>,
    pub(crate) time_range: TimeRange,
}

pub(crate) fn run_cli(args: UsageArgs) -> Result<()> {
    if args.card {
        return crate::wrapped::run_card(args.period, args.project.as_deref());
    }

    if let Some(project) = args.project.as_deref()
        && project.trim().is_empty()
    {
        bail!("project filter cannot be empty");
    }

    let time_range = parse_time_range(args.time.as_deref())?;
    let sources = usage_source_labels();
    let resolved_sources = resolve_source_filter(args.source.as_deref(), &sources)?;

    if !args.json {
        let initial_scope = match args.project.as_deref() {
            Some(project) => Store::open()?.resolve_cli_project_selector(project)?,
            None => ProjectScope::Global,
        };
        return crate::tui::runner::run(Some(UsageStart {
            sources: resolved_sources,
            time_range: args.time.map(|_| time_range),
            initial_scope,
            open_projects_tab: args.projects,
        }));
    }

    run_usage_sync_job()?;
    let store = Store::open()?;

    if args.projects {
        let report = build_project_usage_report(&store, resolved_sources.as_deref(), time_range)?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    let report = match args.project.as_deref() {
        Some(project) => {
            let scope = store.resolve_cli_project_selector(project)?;
            let envelope = scope.envelope();
            let mut report = build_usage_report_for_target(
                &store,
                resolved_sources.as_deref(),
                time_range,
                &UsageTarget::Scope(scope),
            )?;
            report.scope = Some(envelope);
            report
        }
        None => {
            build_usage_report(&store, &UsageFilters { sources: resolved_sources, time_range })?
        }
    };

    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn usage_source_labels() -> Vec<(String, String)> {
    adapters::all_adapters()
        .into_iter()
        .filter(|adapter| adapter.usage_parser_version().is_some())
        .map(|adapter| (adapter.id().to_string(), adapter.label().to_string()))
        .collect()
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub(crate) struct TokenTotals {
    pub(crate) input_tokens: i64,
    pub(crate) output_tokens: i64,
    pub(crate) cache_read_tokens: i64,
    pub(crate) cache_write_tokens: i64,
    pub(crate) reasoning_tokens: i64,
    pub(crate) total_tokens: i64,
}

impl TokenTotals {
    pub(crate) fn add_event(&mut self, event: &UsageEventRecord) {
        self.input_tokens += event.input_tokens;
        self.output_tokens += event.output_tokens;
        self.cache_read_tokens += event.cache_read_tokens;
        self.cache_write_tokens += event.cache_write_tokens;
        self.reasoning_tokens += event.reasoning_tokens;
        self.total_tokens = self.input_tokens
            + self.output_tokens
            + self.cache_read_tokens
            + self.cache_write_tokens
            + self.reasoning_tokens;
    }
}

pub(crate) fn event_total_tokens(event: &UsageEventRecord) -> i64 {
    event.input_tokens
        + event.output_tokens
        + event.cache_read_tokens
        + event.cache_write_tokens
        + event.reasoning_tokens
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct UsageSummary {
    pub(crate) events: usize,
    pub(crate) sessions: usize,
    pub(crate) sources: usize,
    pub(crate) models: usize,
    pub(crate) token_source_events: BTreeMap<String, usize>,
    pub(crate) tokens: TokenTotals,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SourceUsage {
    pub(crate) source: String,
    pub(crate) sessions: usize,
    pub(crate) events: usize,
    pub(crate) tokens: TokenTotals,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ModelUsage {
    pub(crate) source: String,
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) sessions: usize,
    pub(crate) events: usize,
    pub(crate) tokens: TokenTotals,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct PeriodUsage {
    pub(crate) period: String,
    pub(crate) sessions: usize,
    pub(crate) events: usize,
    pub(crate) tokens: TokenTotals,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct UsageReport {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) scope: Option<serde_json::Value>,
    pub(crate) summary: UsageSummary,
    pub(crate) by_source: Vec<SourceUsage>,
    pub(crate) by_model: Vec<ModelUsage>,
    pub(crate) daily: Vec<PeriodUsage>,
    pub(crate) weekly: Vec<PeriodUsage>,
    pub(crate) monthly: Vec<PeriodUsage>,
    #[serde(skip)]
    pub(crate) hourly_today_tokens: [i64; 24],
    #[serde(skip)]
    pub(crate) hourly_today_sessions: [usize; 24],
    #[serde(skip)]
    pub(crate) daily_all_sessions: BTreeMap<String, usize>,
    #[serde(skip)]
    pub(crate) weekly_all_sessions: BTreeMap<String, usize>,
}

#[derive(Default)]
struct Accumulator {
    tokens: TokenTotals,
    sessions: BTreeSet<String>,
    events: usize,
}

impl Accumulator {
    fn add(&mut self, event: &UsageEventRecord) {
        self.tokens.add_event(event);
        self.sessions.insert(event.session_id.clone());
        self.events += 1;
    }
}

pub(super) struct UsageReportAccumulator {
    today: NaiveDate,
    total: Accumulator,
    by_source: BTreeMap<String, Accumulator>,
    by_model: BTreeMap<(String, String, String), Accumulator>,
    daily: BTreeMap<String, Accumulator>,
    weekly: BTreeMap<String, Accumulator>,
    monthly: BTreeMap<String, Accumulator>,
    token_source_events: BTreeMap<String, usize>,
    hourly_today_tokens: [i64; 24],
    hourly_today_sessions: [HashSet<String>; 24],
    daily_all_sessions: BTreeMap<String, HashSet<String>>,
    weekly_all_sessions: BTreeMap<String, HashSet<String>>,
}

impl UsageReportAccumulator {
    pub(super) fn new(today: NaiveDate) -> Self {
        Self {
            today,
            total: Accumulator::default(),
            by_source: BTreeMap::new(),
            by_model: BTreeMap::new(),
            daily: BTreeMap::new(),
            weekly: BTreeMap::new(),
            monthly: BTreeMap::new(),
            token_source_events: BTreeMap::new(),
            hourly_today_tokens: [0; 24],
            hourly_today_sessions: std::array::from_fn(|_| HashSet::new()),
            daily_all_sessions: BTreeMap::new(),
            weekly_all_sessions: BTreeMap::new(),
        }
    }

    pub(super) fn add(&mut self, event: &UsageEventRecord) {
        self.total.add(event);
        *self.token_source_events.entry(event.token_source.clone()).or_insert(0) += 1;
        self.by_source.entry(event.source.clone()).or_default().add(event);
        self.by_model
            .entry((event.source.clone(), event.provider.clone(), event.model.clone()))
            .or_default()
            .add(event);

        let dt = local_datetime(event.timestamp);
        let (day, week, month) = period_keys_from_date(dt.date_naive());
        self.daily.entry(day.clone()).or_default().add(event);
        self.weekly.entry(week.clone()).or_default().add(event);
        self.monthly.entry(month).or_default().add(event);

        self.daily_all_sessions.entry(day).or_default().insert(event.session_id.clone());
        self.weekly_all_sessions.entry(week).or_default().insert(event.session_id.clone());
        if dt.date_naive() == self.today {
            let hour = dt.hour() as usize;
            self.hourly_today_tokens[hour] += event_total_tokens(event);
            self.hourly_today_sessions[hour].insert(event.session_id.clone());
        }
    }

    pub(super) fn add_zero_event_session(&mut self, session_id: &str, active_ts: i64) {
        if self.total.sessions.contains(session_id) {
            return;
        }
        let dt = local_datetime(active_ts);
        let (day, week, _) = period_keys_from_date(dt.date_naive());
        self.daily_all_sessions.entry(day).or_default().insert(session_id.to_string());
        self.weekly_all_sessions.entry(week).or_default().insert(session_id.to_string());
        if dt.date_naive() == self.today {
            let hour = dt.hour() as usize;
            self.hourly_today_sessions[hour].insert(session_id.to_string());
        }
    }

    pub(super) fn finish(self) -> UsageReport {
        let source_count = self.by_source.len();
        let model_count = self.by_model.len();

        let mut by_source = self
            .by_source
            .into_iter()
            .map(|(source, acc)| SourceUsage {
                source,
                sessions: acc.sessions.len(),
                events: acc.events,
                tokens: acc.tokens,
            })
            .collect::<Vec<_>>();
        by_source.sort_by_key(|source| Reverse(source.tokens.total_tokens));

        let mut by_model = self
            .by_model
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
            scope: None,
            summary: UsageSummary {
                events: self.total.events,
                sessions: self.total.sessions.len(),
                sources: source_count,
                models: model_count,
                token_source_events: self.token_source_events,
                tokens: self.total.tokens,
            },
            by_source,
            by_model,
            daily: period_vec(self.daily),
            weekly: period_vec(self.weekly),
            monthly: period_vec(self.monthly),
            hourly_today_tokens: self.hourly_today_tokens,
            hourly_today_sessions: self.hourly_today_sessions.map(|sessions| sessions.len()),
            daily_all_sessions: self
                .daily_all_sessions
                .into_iter()
                .map(|(day, sessions)| (day, sessions.len()))
                .collect(),
            weekly_all_sessions: self
                .weekly_all_sessions
                .into_iter()
                .map(|(week, sessions)| (week, sessions.len()))
                .collect(),
        }
    }
}

pub(crate) fn build_usage_report(store: &Store, filters: &UsageFilters) -> Result<UsageReport> {
    build_usage_report_for_target(
        store,
        filters.sources.as_deref(),
        filters.time_range,
        &UsageTarget::Scope(ProjectScope::Global),
    )
}

pub(crate) fn build_usage_report_for_target(
    store: &Store,
    sources: Option<&[String]>,
    time_range: TimeRange,
    target: &UsageTarget,
) -> Result<UsageReport> {
    let now = Local::now();
    let scope_ids = match target {
        UsageTarget::Scope(ProjectScope::Global) => None,
        UsageTarget::Scope(scope) => Some(store.session_ids_in_scope(scope)?),
        UsageTarget::Partition { .. } => None,
    };
    let session_meta = store.session_usage_meta_map(sources)?;
    let cutoff = time_range.cutoff_millis_at(now);
    let mut acc = UsageReportAccumulator::new(now.date_naive());

    store.fold_usage_events(sources, time_range, (), |(), event| {
        if matches_target(target, scope_ids.as_ref(), &session_meta, &event.session_id) {
            acc.add(&event);
        }
    })?;

    for (session_id, meta) in &session_meta {
        let active_ts = meta.updated_at.unwrap_or(meta.started_at);
        if cutoff.is_none_or(|c| active_ts >= c)
            && matches_target(target, scope_ids.as_ref(), &session_meta, session_id)
        {
            acc.add_zero_event_session(session_id, active_ts);
        }
    }

    Ok(acc.finish())
}

#[cfg(any(test, feature = "bench"))]
pub(crate) fn aggregate_usage_events(events: &[UsageEventRecord]) -> UsageReport {
    let mut report = UsageReportAccumulator::new(Local::now().date_naive());
    Store::for_each_deduped_usage_event(events, |event| report.add(event));
    report.finish()
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

pub(crate) fn local_datetime(timestamp_ms: i64) -> DateTime<Local> {
    Local
        .timestamp_millis_opt(timestamp_ms)
        .single()
        .unwrap_or_else(|| Local.timestamp_millis_opt(0).single().expect("epoch is valid"))
}

fn period_keys_from_date(date: NaiveDate) -> (String, String, String) {
    let iso = date.iso_week();
    (
        date.format("%Y-%m-%d").to_string(),
        format!("{:04}-W{:02}", iso.year(), iso.week()),
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

    #[test]
    fn aggregate_usage_dedupes_warp_events_across_forked_sessions() {
        let first =
            event("warp", "session-a", r#"["msg-1","primary_agent","warp","test-model"]"#, 1);
        let mut duplicate = first.clone();
        duplicate.session_id = "session-b".into();
        let mut next = first.clone();
        next.event_key = r#"["msg-2","primary_agent","warp","test-model"]"#.into();
        let mut category = first.clone();
        category.event_key = r#"["msg-1","subagent","warp","test-model"]"#.into();
        let report = aggregate_usage_events(&[first, duplicate, next, category]);
        assert_eq!(report.summary.events, 3);
        assert_eq!(report.summary.tokens.total_tokens, 42);
    }

    #[test]
    fn aggregate_usage_dedupes_qoder_forked_messages() {
        let mut events = Vec::new();
        for (index, (input, output)) in [(120, 20), (170, 15), (190, 12)].into_iter().enumerate() {
            let mut original =
                event("qoder", "parent", &format!("assistant:msg-{index}"), index as i64);
            original.input_tokens = input;
            original.output_tokens = output;
            original.cache_read_tokens = 0;
            original.reasoning_tokens = 0;
            original.token_source = "observed".to_string();
            let mut forked = original.clone();
            forked.session_id = "fork".to_string();
            forked.source_id = "fork".to_string();
            events.extend([original, forked]);
        }
        let report = aggregate_usage_events(&events);
        assert_eq!(report.summary.events, 3);
        assert_eq!(report.summary.tokens.input_tokens, 480);
        assert_eq!(report.summary.tokens.output_tokens, 47);
        assert_eq!(report.summary.tokens.total_tokens, 527);
    }

    #[test]
    fn aggregate_usage_keeps_qoder_line_keys_and_other_sources_separate() {
        let events = [
            event("qoder", "parent", "assistant:line:2", 1),
            event("qoder", "fork", "assistant:line:2", 1),
            event("claude-code", "claude", "assistant:msg-1", 1),
            event("qoder", "parent", "assistant:msg-1", 1),
        ];
        let report = aggregate_usage_events(&events);
        assert_eq!(report.summary.events, 4);
        assert_eq!(report.summary.tokens.total_tokens, 56);
    }
}
