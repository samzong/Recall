use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use anyhow::Result;
use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, Timelike};
use serde::Serialize;

use crate::db::project_store::SessionUsageMeta;
use crate::db::search::TimeRange;
use crate::db::store::Store;
use crate::db::usage_store::UsageSlice;
use crate::project_scope::ProjectScope;
use crate::usage::{
    ModelUsage, TokenTotals, UsageReport, UsageReportAccumulator, local_datetime,
    slice_total_tokens,
};

#[derive(Debug, Clone)]
pub(crate) struct DashboardUsageBundle {
    pub(crate) current_report: UsageReport,
    pub(crate) all_time_report: UsageReport,
    pub(crate) projects_report: ProjectUsageReport,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum ProjectPartitionKey {
    Repository(String),
    Directory(String),
    Unattributed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum UsageTarget {
    Scope(ProjectScope),
    Partition { key: ProjectPartitionKey, label: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ProjectRepoMeta {
    pub(crate) remote: Option<String>,
    pub(crate) slug: Option<String>,
    pub(crate) name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ProjectUsageEntry {
    #[serde(skip)]
    pub(crate) key: ProjectPartitionKey,
    pub(crate) project: String,
    pub(crate) kind: &'static str,
    pub(crate) repository: Option<ProjectRepoMeta>,
    pub(crate) directory: Option<String>,
    pub(crate) sessions: usize,
    pub(crate) indexed_sessions: usize,
    pub(crate) events: usize,
    pub(crate) top_model: Option<String>,
    pub(crate) tokens: TokenTotals,
    pub(crate) by_model: Vec<ModelUsage>,
    #[serde(skip)]
    pub(crate) trend_tokens: Vec<i64>,
    #[serde(skip)]
    pub(crate) trend_sessions: Vec<usize>,
    #[serde(skip)]
    pub(crate) trend_label: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ProjectUsageSummary {
    pub(crate) projects: usize,
    pub(crate) events: usize,
    pub(crate) sessions: usize,
    pub(crate) indexed_sessions: usize,
    pub(crate) sources: usize,
    pub(crate) models: usize,
    pub(crate) tokens: TokenTotals,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ProjectUsageReport {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) scope: Option<serde_json::Value>,
    pub(crate) summary: ProjectUsageSummary,
    pub(crate) projects: Vec<ProjectUsageEntry>,
}

pub(crate) fn session_partition_key(meta: Option<&SessionUsageMeta>) -> ProjectPartitionKey {
    let Some(meta) = meta else {
        return ProjectPartitionKey::Unattributed;
    };
    if let Some(remote) = non_empty(meta.repo_remote.as_deref()) {
        return ProjectPartitionKey::Repository(remote.to_string());
    }
    if let Some(slug) = non_empty(meta.repo_slug.as_deref()) {
        return ProjectPartitionKey::Repository(format!("slug:{slug}"));
    }
    if let Some(name) = non_empty(meta.repo_name.as_deref()) {
        return ProjectPartitionKey::Repository(format!("name:{name}"));
    }
    if let Some(dir) = non_empty(meta.directory.as_deref()) {
        let trimmed = trim_directory_root(dir);
        if !trimmed.is_empty() {
            return ProjectPartitionKey::Directory(trimmed.to_string());
        }
    }
    ProjectPartitionKey::Unattributed
}

pub(crate) fn build_project_usage_report(
    store: &Store,
    sources: Option<&[String]>,
    time_range: TimeRange,
) -> Result<ProjectUsageReport> {
    let bundle = build_dashboard_bundle(
        store,
        sources,
        time_range,
        &UsageTarget::Scope(ProjectScope::Global),
    )?;
    Ok(bundle.projects_report)
}

pub(crate) struct TrendWindow {
    label: &'static str,
    len: usize,
    mode: TrendMode,
}

enum TrendMode {
    Today(NaiveDate),
    Daily { start: NaiveDate, days: i64 },
    Weekly { start_monday: NaiveDate, weeks: i64 },
}

impl TrendWindow {
    pub(crate) fn new(time_range: TimeRange, now: DateTime<Local>) -> Self {
        let today = now.date_naive();
        match time_range {
            TimeRange::Today => Self { label: "today", len: 24, mode: TrendMode::Today(today) },
            TimeRange::Week => Self {
                label: "7d",
                len: 7,
                mode: TrendMode::Daily { start: today - Duration::days(6), days: 7 },
            },
            TimeRange::Month => Self {
                label: "30d",
                len: 30,
                mode: TrendMode::Daily { start: today - Duration::days(29), days: 30 },
            },
            TimeRange::All => {
                let end_monday = week_monday(today);
                let start_monday = end_monday - Duration::weeks(11);
                Self { label: "12w", len: 12, mode: TrendMode::Weekly { start_monday, weeks: 12 } }
            }
        }
    }

    pub(crate) fn bucket(&self, timestamp_ms: i64) -> Option<usize> {
        let dt = local_datetime(timestamp_ms);
        let date = dt.date_naive();
        match self.mode {
            TrendMode::Today(today) => (date == today).then_some(dt.hour() as usize),
            TrendMode::Daily { start, days } => {
                let offset = (date - start).num_days();
                (0..days).contains(&offset).then_some(offset as usize)
            }
            TrendMode::Weekly { start_monday, weeks } => {
                let offset = (date - start_monday).num_days().div_euclid(7);
                (0..weeks).contains(&offset).then_some(offset as usize)
            }
        }
    }
}

fn week_monday(date: NaiveDate) -> NaiveDate {
    date - Duration::days(i64::from(date.weekday().num_days_from_monday()))
}

#[derive(Default)]
struct ModelAcc {
    tokens: TokenTotals,
    sessions: BTreeSet<String>,
    events: usize,
}

impl ModelAcc {
    fn add_slice(&mut self, slice: &UsageSlice) {
        self.tokens.add_slice(slice);
        self.sessions.insert(slice.session_id.clone());
        self.events += slice.events;
    }
}

struct ProjectEntryAcc {
    remote: Option<String>,
    slug: Option<String>,
    name: Option<String>,
    directory: Option<String>,
    tokens: TokenTotals,
    sessions: HashSet<String>,
    indexed_sessions: HashSet<String>,
    events: usize,
    by_model: BTreeMap<(String, String, String), ModelAcc>,
    model_totals: BTreeMap<String, i64>,
    trend_tokens: Vec<i64>,
    trend_sessions: Vec<HashSet<String>>,
}

impl ProjectEntryAcc {
    fn new(trend_len: usize) -> Self {
        Self {
            remote: None,
            slug: None,
            name: None,
            directory: None,
            tokens: TokenTotals::default(),
            sessions: HashSet::new(),
            indexed_sessions: HashSet::new(),
            events: 0,
            by_model: BTreeMap::new(),
            model_totals: BTreeMap::new(),
            trend_tokens: vec![0; trend_len],
            trend_sessions: vec![HashSet::new(); trend_len],
        }
    }

    fn observe_meta(&mut self, meta: &SessionUsageMeta) {
        if self.remote.is_none() {
            self.remote = non_empty(meta.repo_remote.as_deref()).map(str::to_string);
        }
        if self.slug.is_none() {
            self.slug = non_empty(meta.repo_slug.as_deref()).map(str::to_string);
        }
        if self.name.is_none() {
            self.name = non_empty(meta.repo_name.as_deref()).map(str::to_string);
        }
        if self.directory.is_none() {
            self.directory = non_empty(meta.directory.as_deref())
                .map(trim_directory_root)
                .filter(|dir| !dir.is_empty())
                .map(str::to_string);
        }
    }

    fn add_slice(
        &mut self,
        meta: Option<&SessionUsageMeta>,
        slice: &UsageSlice,
        trend: &TrendWindow,
    ) {
        if let Some(meta) = meta {
            self.observe_meta(meta);
        }
        self.tokens.add_slice(slice);
        self.sessions.insert(slice.session_id.clone());
        self.events += slice.events;
        self.by_model
            .entry((slice.source.clone(), slice.provider.clone(), slice.model.clone()))
            .or_default()
            .add_slice(slice);
        let model_name = if slice.model.is_empty() { "unknown" } else { slice.model.as_str() };
        let model_total = self.model_totals.entry(model_name.to_string()).or_insert(0);
        *model_total = model_total.saturating_add(slice_total_tokens(slice));
        if let Some(bucket) = trend.bucket(slice.timestamp) {
            self.trend_tokens[bucket] =
                self.trend_tokens[bucket].saturating_add(slice_total_tokens(slice));
            self.trend_sessions[bucket].insert(slice.session_id.clone());
        }
    }

    fn finish(self, key: ProjectPartitionKey, trend_label: &'static str) -> ProjectUsageEntry {
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
        by_model.sort_by(|left, right| {
            right
                .tokens
                .total_tokens
                .cmp(&left.tokens.total_tokens)
                .then_with(|| left.source.cmp(&right.source))
                .then_with(|| left.provider.cmp(&right.provider))
                .then_with(|| left.model.cmp(&right.model))
        });

        let top_model = self
            .model_totals
            .into_iter()
            .max_by(|(left_name, left_tokens), (right_name, right_tokens)| {
                left_tokens.cmp(right_tokens).then_with(|| right_name.cmp(left_name))
            })
            .map(|(model, _)| model);

        let (kind, project, repository, directory) = match &key {
            ProjectPartitionKey::Repository(repo_key) => {
                let slug = self.slug.or_else(|| {
                    repo_key.strip_prefix("slug:").map(str::to_string).or_else(|| {
                        self.remote
                            .as_deref()
                            .and_then(|r| r.split_once('/'))
                            .map(|(_, s)| s.to_string())
                    })
                });
                let name = self.name.or_else(|| {
                    repo_key.strip_prefix("name:").map(str::to_string).or_else(|| {
                        slug.as_deref().and_then(|s| s.rsplit('/').next()).map(str::to_string)
                    })
                });
                let project = slug
                    .clone()
                    .or_else(|| name.clone())
                    .or_else(|| self.remote.clone())
                    .unwrap_or_else(|| repo_key.clone());
                (
                    "repository",
                    project,
                    Some(ProjectRepoMeta { remote: self.remote, slug, name }),
                    None,
                )
            }
            ProjectPartitionKey::Directory(dir) => {
                ("directory", dir.clone(), None, Some(dir.clone()))
            }
            ProjectPartitionKey::Unattributed => {
                ("unattributed", "unattributed".to_string(), None, None)
            }
        };

        ProjectUsageEntry {
            key,
            project,
            kind,
            repository,
            directory,
            sessions: self.sessions.len(),
            indexed_sessions: self.indexed_sessions.len(),
            events: self.events,
            top_model,
            tokens: self.tokens,
            by_model,
            trend_tokens: self.trend_tokens,
            trend_sessions: self.trend_sessions.into_iter().map(|s| s.len()).collect(),
            trend_label,
        }
    }
}

pub(crate) struct ProjectsAccumulator {
    trend: TrendWindow,
    entries: BTreeMap<ProjectPartitionKey, ProjectEntryAcc>,
    events: usize,
    sessions: HashSet<String>,
    indexed_sessions: HashSet<String>,
    sources: HashSet<String>,
    models: HashSet<(String, String, String)>,
    tokens: TokenTotals,
}

impl ProjectsAccumulator {
    pub(crate) fn new(time_range: TimeRange, now: DateTime<Local>) -> Self {
        Self {
            trend: TrendWindow::new(time_range, now),
            entries: BTreeMap::new(),
            events: 0,
            sessions: HashSet::new(),
            indexed_sessions: HashSet::new(),
            sources: HashSet::new(),
            models: HashSet::new(),
            tokens: TokenTotals::default(),
        }
    }

    pub(crate) fn add_slice(&mut self, meta: Option<&SessionUsageMeta>, slice: &UsageSlice) {
        let key = session_partition_key(meta);
        let len = self.trend.len;
        self.entries.entry(key).or_insert_with(|| ProjectEntryAcc::new(len)).add_slice(
            meta,
            slice,
            &self.trend,
        );
        self.events += slice.events;
        self.sessions.insert(slice.session_id.clone());
        self.sources.insert(slice.source.clone());
        self.models.insert((slice.source.clone(), slice.provider.clone(), slice.model.clone()));
        self.tokens.add_slice(slice);
    }

    pub(crate) fn finish_with_sessions(
        &mut self,
        session_meta: &HashMap<String, SessionUsageMeta>,
        cutoff: Option<i64>,
    ) {
        let len = self.trend.len;
        for (session_id, meta) in session_meta {
            let active_ts = meta.updated_at.unwrap_or(meta.started_at);
            let in_window = cutoff.is_none_or(|c| active_ts >= c);
            let key = session_partition_key(Some(meta));
            let had_event =
                self.entries.get(&key).is_some_and(|entry| entry.sessions.contains(session_id));
            if !(in_window || had_event) {
                continue;
            }
            let entry = self.entries.entry(key).or_insert_with(|| ProjectEntryAcc::new(len));
            entry.observe_meta(meta);
            entry.indexed_sessions.insert(session_id.clone());
            self.indexed_sessions.insert(session_id.clone());
            if in_window
                && !had_event
                && let Some(bucket) = self.trend.bucket(active_ts)
            {
                entry.trend_sessions[bucket].insert(session_id.clone());
            }
        }
        for (key, entry) in &mut self.entries {
            if matches!(key, ProjectPartitionKey::Unattributed) {
                for session_id in &entry.sessions {
                    if !session_meta.contains_key(session_id) {
                        entry.indexed_sessions.insert(session_id.clone());
                        self.indexed_sessions.insert(session_id.clone());
                    }
                }
            }
        }
    }

    pub(crate) fn into_report(self) -> ProjectUsageReport {
        let trend_label = self.trend.label;
        let mut projects = self
            .entries
            .into_iter()
            .map(|(key, entry)| entry.finish(key, trend_label))
            .collect::<Vec<_>>();
        projects.sort_by(|left, right| {
            right
                .tokens
                .total_tokens
                .cmp(&left.tokens.total_tokens)
                .then_with(|| right.sessions.cmp(&left.sessions))
                .then_with(|| right.indexed_sessions.cmp(&left.indexed_sessions))
                .then_with(|| left.project.cmp(&right.project))
                .then_with(|| left.key.cmp(&right.key))
        });

        ProjectUsageReport {
            scope: None,
            summary: ProjectUsageSummary {
                projects: projects.len(),
                events: self.events,
                sessions: self.sessions.len(),
                indexed_sessions: self.indexed_sessions.len(),
                sources: self.sources.len(),
                models: self.models.len(),
                tokens: self.tokens,
            },
            projects,
        }
    }
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn trim_directory_root(dir: &str) -> &str {
    if dir == "/" || dir == "\\" { dir } else { dir.trim_end_matches(['/', '\\']) }
}

pub(crate) fn build_dashboard_bundle(
    store: &Store,
    sources: Option<&[String]>,
    time_range: TimeRange,
    target: &UsageTarget,
) -> Result<DashboardUsageBundle> {
    build_dashboard_bundle_at(store, sources, time_range, target, Local::now())
}

pub(crate) fn build_dashboard_bundle_at(
    store: &Store,
    sources: Option<&[String]>,
    time_range: TimeRange,
    target: &UsageTarget,
    now: DateTime<Local>,
) -> Result<DashboardUsageBundle> {
    let scope_ids = match target {
        UsageTarget::Scope(ProjectScope::Global) => None,
        UsageTarget::Scope(scope) => Some(store.session_ids_in_scope(scope)?),
        UsageTarget::Partition { .. } => None,
    };
    let session_meta = store.session_usage_meta_map(sources)?;
    let cutoff = time_range.cutoff_millis_at(now);
    let today = now.date_naive();

    let mut current_acc = UsageReportAccumulator::new(today);
    let mut all_time_acc =
        (time_range != TimeRange::All).then(|| UsageReportAccumulator::new(today));
    let mut projects_acc = ProjectsAccumulator::new(time_range, now);

    store.fold_dashboard_usage_slices(sources, cutoff, (), |(), slice, in_window| {
        if matches_target(target, scope_ids.as_ref(), &session_meta, &slice.session_id) {
            if let Some(all_time) = all_time_acc.as_mut() {
                all_time.add_slice(&slice);
            }
            if in_window {
                current_acc.add_slice(&slice);
            }
        }
        if in_window {
            projects_acc.add_slice(session_meta.get(&slice.session_id), &slice);
        }
    })?;

    for (session_id, meta) in &session_meta {
        let active_ts = meta.updated_at.unwrap_or(meta.started_at);
        let in_window = cutoff.is_none_or(|c| active_ts >= c);
        if matches_target(target, scope_ids.as_ref(), &session_meta, session_id) {
            if let Some(all_time) = all_time_acc.as_mut() {
                all_time.add_zero_event_session(session_id, active_ts);
            }
            if in_window {
                current_acc.add_zero_event_session(session_id, active_ts);
            }
        }
    }
    projects_acc.finish_with_sessions(&session_meta, cutoff);

    let current_report = current_acc.finish();
    let all_time_report = match all_time_acc {
        Some(acc) => acc.finish(),
        None => current_report.clone(),
    };

    Ok(DashboardUsageBundle {
        current_report,
        all_time_report,
        projects_report: projects_acc.into_report(),
    })
}

pub(crate) fn matches_target(
    target: &UsageTarget,
    scope_ids: Option<&HashSet<String>>,
    session_meta: &HashMap<String, SessionUsageMeta>,
    session_id: &str,
) -> bool {
    match target {
        UsageTarget::Scope(ProjectScope::Global) => true,
        UsageTarget::Scope(_) => scope_ids.is_some_and(|ids| ids.contains(session_id)),
        UsageTarget::Partition { key, .. } => {
            session_partition_key(session_meta.get(session_id)) == *key
        }
    }
}

#[cfg(test)]
mod saturation_tests {
    use super::*;

    #[test]
    fn project_models_and_trends_saturate_independently() {
        let now = Local::now();
        let trend = TrendWindow::new(TimeRange::Today, now);
        let mut acc = ProjectEntryAcc::new(24);
        let mut slice = UsageSlice {
            session_id: "large".into(),
            source: "copilot-cli".into(),
            timestamp: now.timestamp_millis(),
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
        acc.add_slice(None, &slice, &trend);
        acc.add_slice(None, &slice, &trend);
        slice.model = "second".into();
        slice.input_tokens = 7;
        acc.add_slice(None, &slice, &trend);
        assert_eq!(acc.tokens.total_tokens, i64::MAX);
        assert_eq!(acc.model_totals["first"], i64::MAX);
        assert_eq!(acc.model_totals["second"], 7);
        assert_eq!(acc.trend_tokens[trend.bucket(slice.timestamp).unwrap()], i64::MAX);
    }
}
