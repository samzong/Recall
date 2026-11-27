use std::time::Instant;

use crate::db::search::TimeRange;
use crate::project_scope::ProjectScope;
use crate::skill_audit::{SkillAuditReport, SkillUsageEntry};
use crate::tui::usage_worker::{DashboardReports, UsagePayload, UsageResponse};
use crate::usage::{
    ProjectPartitionKey, ProjectUsageEntry, ProjectUsageReport, UsageReport, UsageTarget,
};

const USAGE_LOADING_MIN_MS: u128 = 75;
const USAGE_CACHE_LIMIT: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UsageTab {
    Tokens,
    Projects,
    Skills,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChartMetric {
    Tokens,
    Sessions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChartGranularity {
    Daily,
    Weekly,
}

#[derive(Clone, PartialEq, Eq)]
struct UsageCacheKey {
    sources: Option<Vec<String>>,
    time_range: TimeRange,
    target: UsageTarget,
}

#[derive(Clone)]
struct CachedUsageBundle {
    report: UsageReport,
    year_report: UsageReport,
    projects_report: ProjectUsageReport,
    skill_report: SkillAuditReport,
}

pub(crate) struct UsageState {
    pub(crate) report: Option<UsageReport>,
    pub(crate) year_report: Option<UsageReport>,
    pub(crate) projects_report: Option<ProjectUsageReport>,
    pub(crate) error: Option<String>,
    pub(crate) projects_error: Option<String>,
    pub(crate) time_filter: TimeRange,
    pub(crate) initial_scope: ProjectScope,
    pub(crate) drilldown: Option<(ProjectPartitionKey, String)>,
    pub(crate) refresh_requested_at: Option<Instant>,
    pub(crate) in_flight: bool,
    pub(crate) skill_in_flight: bool,
    pub(crate) silent_refresh: bool,
    pub(crate) request_id: u64,
    pub(crate) breakdown_scroll: u16,
    pub(crate) tab: UsageTab,
    pub(crate) chart_metric: ChartMetric,
    pub(crate) granularity_override: Option<ChartGranularity>,
    pub(crate) project_selected: usize,
    pub(crate) selected_project_key: Option<ProjectPartitionKey>,
    pub(crate) skill_report: Option<SkillAuditReport>,
    pub(crate) skill_error: Option<String>,
    pub(crate) skill_selected: usize,
    cache: Vec<(UsageCacheKey, CachedUsageBundle)>,
}

impl Default for UsageState {
    fn default() -> Self {
        Self {
            report: None,
            year_report: None,
            projects_report: None,
            error: None,
            projects_error: None,
            time_filter: TimeRange::All,
            initial_scope: ProjectScope::Global,
            drilldown: None,
            refresh_requested_at: None,
            in_flight: false,
            skill_in_flight: false,
            silent_refresh: false,
            request_id: 0,
            breakdown_scroll: 0,
            tab: UsageTab::Tokens,
            chart_metric: ChartMetric::Tokens,
            granularity_override: None,
            project_selected: 0,
            selected_project_key: None,
            skill_report: None,
            skill_error: None,
            skill_selected: 0,
            cache: Vec::new(),
        }
    }
}

impl UsageState {
    pub(crate) fn effective_target(&self) -> UsageTarget {
        match &self.drilldown {
            Some((key, label)) => UsageTarget::Partition { key: key.clone(), label: label.clone() },
            None => UsageTarget::Scope(self.initial_scope.clone()),
        }
    }

    pub(crate) fn request_refresh(&mut self) {
        self.error = None;
        self.projects_error = None;
        self.skill_error = None;
        self.silent_refresh = false;
        self.request_id = self.request_id.saturating_add(1);
        self.refresh_requested_at = Some(Instant::now());
        self.reset_selection();
    }

    pub(crate) fn request_silent_refresh(&mut self) {
        self.silent_refresh = true;
        self.request_id = self.request_id.saturating_add(1);
        self.refresh_requested_at = Some(Instant::now());
    }

    pub(crate) fn clear_cache(&mut self) {
        self.cache.clear();
    }

    pub(crate) fn is_dashboard_loading(&self) -> bool {
        !self.silent_refresh && (self.refresh_requested_at.is_some() || self.in_flight)
    }

    pub(crate) fn is_skill_loading(&self) -> bool {
        !self.silent_refresh
            && (self.refresh_requested_at.is_some() || self.skill_in_flight || self.in_flight)
    }

    pub(crate) fn is_loading(&self) -> bool {
        self.is_dashboard_loading() || (self.tab == UsageTab::Skills && self.is_skill_loading())
    }

    pub(crate) fn take_refresh(&mut self) -> Option<(u64, TimeRange, UsageTarget)> {
        let requested_at = self.refresh_requested_at?;
        let has_content =
            self.report.is_some() || self.projects_report.is_some() || self.skill_report.is_some();
        if has_content
            && !self.silent_refresh
            && requested_at.elapsed().as_millis() < USAGE_LOADING_MIN_MS
        {
            return None;
        }
        self.refresh_requested_at = None;
        self.in_flight = true;
        self.skill_in_flight = true;
        Some((self.request_id, self.time_filter, self.effective_target()))
    }

    pub(crate) fn fail_refresh(&mut self, error: impl std::fmt::Display) {
        self.refresh_requested_at = None;
        self.in_flight = false;
        self.skill_in_flight = false;
        self.silent_refresh = false;
        self.report = None;
        self.year_report = None;
        self.projects_report = None;
        self.skill_report = None;
        self.error = Some(format!("Usage unavailable: {error}"));
        self.projects_error = Some(format!("Project usage unavailable: {error}"));
        self.skill_error = Some(format!("Skill audit unavailable: {error}"));
    }

    pub(crate) fn apply_response(&mut self, response: UsageResponse, sources: Option<Vec<String>>) {
        if response.id != self.request_id
            || response.sources != sources
            || response.time_range != self.time_filter
            || response.target != self.effective_target()
        {
            return;
        }

        match response.payload {
            UsagePayload::Dashboard(reports) => {
                let DashboardReports { current_report, all_time_report, projects_report } =
                    *reports;
                self.in_flight = false;
                match current_report {
                    Ok(report) => {
                        self.error = None;
                        self.report = Some(report);
                    }
                    Err(error) => {
                        self.report = None;
                        self.error = Some(format!("Usage unavailable: {error}"));
                    }
                }
                match all_time_report {
                    Ok(report) => {
                        self.year_report = Some(report);
                    }
                    Err(error) => {
                        self.year_report = None;
                        self.error.get_or_insert_with(|| format!("Usage unavailable: {error}"));
                    }
                }
                match projects_report {
                    Ok(report) => {
                        self.projects_error = None;
                        self.projects_report = Some(report);
                    }
                    Err(error) => {
                        self.projects_report = None;
                        self.projects_error = Some(format!("Project usage unavailable: {error}"));
                    }
                }
                self.reanchor_project_selection();
            }
            UsagePayload::Skills(skill_audit_report) => {
                self.skill_in_flight = false;
                match skill_audit_report {
                    Ok(report) => {
                        self.skill_error = None;
                        self.skill_report = Some(report);
                    }
                    Err(error) => {
                        self.skill_report = None;
                        self.skill_error = Some(format!("Skill audit unavailable: {error}"));
                    }
                }
                let skill_count = self.skill_entry_count();
                if skill_count == 0 {
                    self.skill_selected = 0;
                } else {
                    self.skill_selected = self.skill_selected.min(skill_count - 1);
                }
            }
        }

        if !self.in_flight && !self.skill_in_flight {
            self.silent_refresh = false;
            if let (Some(report), Some(year_report), Some(projects_report), Some(skill_report)) = (
                self.report.as_ref(),
                self.year_report.as_ref(),
                self.projects_report.as_ref(),
                self.skill_report.as_ref(),
            ) {
                self.store_cache(
                    UsageCacheKey {
                        sources,
                        time_range: response.time_range,
                        target: response.target,
                    },
                    CachedUsageBundle {
                        report: report.clone(),
                        year_report: year_report.clone(),
                        projects_report: projects_report.clone(),
                        skill_report: skill_report.clone(),
                    },
                );
            }
        }
    }

    pub(crate) fn try_apply_cached(&mut self, sources: Option<Vec<String>>) -> bool {
        let target = self.effective_target();
        let Some(pos) = self.cache.iter().position(|(key, _)| {
            key.sources == sources && key.time_range == self.time_filter && key.target == target
        }) else {
            return false;
        };
        let (key, bundle) = self.cache.remove(pos);
        self.cache.push((key, bundle.clone()));
        self.request_id = self.request_id.saturating_add(1);
        self.refresh_requested_at = None;
        self.in_flight = false;
        self.skill_in_flight = false;
        self.silent_refresh = false;
        self.error = None;
        self.projects_error = None;
        self.skill_error = None;
        self.report = Some(bundle.report);
        self.year_report = Some(bundle.year_report);
        self.projects_report = Some(bundle.projects_report);
        self.skill_report = Some(bundle.skill_report);
        self.reanchor_project_selection();
        true
    }

    fn store_cache(&mut self, key: UsageCacheKey, bundle: CachedUsageBundle) {
        if let Some(pos) = self.cache.iter().position(|(existing, _)| existing == &key) {
            self.cache.remove(pos);
        } else if self.cache.len() >= USAGE_CACHE_LIMIT {
            self.cache.remove(0);
        }
        self.cache.push((key, bundle));
    }

    pub(crate) fn reanchor_project_selection(&mut self) {
        let Some(report) = self.projects_report.as_ref() else {
            self.project_selected = 0;
            return;
        };
        if report.projects.is_empty() {
            self.project_selected = 0;
            if self.drilldown.is_none() {
                self.selected_project_key = None;
            }
            return;
        }
        if let Some(key) = self.selected_project_key.as_ref()
            && let Some(index) = report.projects.iter().position(|entry| &entry.key == key)
        {
            self.project_selected = index;
            return;
        }
        self.project_selected = self.project_selected.min(report.projects.len() - 1);
        if self.drilldown.is_none() {
            self.selected_project_key =
                report.projects.get(self.project_selected).map(|entry| entry.key.clone());
        }
    }

    fn reset_selection(&mut self) {
        self.breakdown_scroll = 0;
        self.skill_selected = 0;
    }

    pub(crate) fn reset(&mut self) {
        self.time_filter = TimeRange::All;
        self.drilldown = None;
        self.granularity_override = None;
        self.project_selected = 0;
        self.selected_project_key = None;
        self.cache.clear();
        self.request_refresh();
    }

    pub(crate) fn cycle_tab(&mut self) {
        self.tab = match self.tab {
            UsageTab::Tokens => UsageTab::Projects,
            UsageTab::Projects => UsageTab::Skills,
            UsageTab::Skills => UsageTab::Tokens,
        };
        self.reset_selection();
    }

    pub(crate) fn cycle_time(&mut self) {
        self.time_filter = match self.time_filter {
            TimeRange::Today => TimeRange::Week,
            TimeRange::Week => TimeRange::Month,
            TimeRange::Month => TimeRange::All,
            TimeRange::All => TimeRange::Today,
        };
        self.request_refresh();
    }

    pub(crate) fn toggle_chart_metric(&mut self) {
        self.chart_metric = match self.chart_metric {
            ChartMetric::Tokens => ChartMetric::Sessions,
            ChartMetric::Sessions => ChartMetric::Tokens,
        };
    }

    pub(crate) fn effective_granularity(&self) -> ChartGranularity {
        self.granularity_override.unwrap_or(match self.time_filter {
            TimeRange::All => ChartGranularity::Weekly,
            TimeRange::Today | TimeRange::Week | TimeRange::Month => ChartGranularity::Daily,
        })
    }

    pub(crate) fn toggle_granularity(&mut self) {
        let next = match self.effective_granularity() {
            ChartGranularity::Daily => ChartGranularity::Weekly,
            ChartGranularity::Weekly => ChartGranularity::Daily,
        };
        self.granularity_override = Some(next);
    }

    pub(crate) fn select_project(&mut self, index: usize) {
        let count = self.project_entry_count();
        if count == 0 {
            self.project_selected = 0;
            self.selected_project_key = None;
            return;
        }
        self.project_selected = index.min(count - 1);
        self.selected_project_key = self.selected_project().map(|entry| entry.key.clone());
    }

    pub(crate) fn scroll(&mut self, up: bool) {
        match (self.tab, up) {
            (UsageTab::Tokens, true) => {
                self.breakdown_scroll = self.breakdown_scroll.saturating_sub(1);
            }
            (UsageTab::Tokens, false) => {
                self.breakdown_scroll = self.breakdown_scroll.saturating_add(1);
            }
            (UsageTab::Projects, true) => {
                self.select_project(self.project_selected.saturating_sub(1));
            }
            (UsageTab::Projects, false) => {
                self.select_project(self.project_selected.saturating_add(1));
            }
            (UsageTab::Skills, true) => self.skill_selected = self.skill_selected.saturating_sub(1),
            (UsageTab::Skills, false) if self.skill_selected + 1 < self.skill_entry_count() => {
                self.skill_selected += 1;
            }
            _ => {}
        }
    }

    pub(crate) fn page_list(&mut self, up: bool, page_size: usize) {
        let step = page_size.max(1);
        match (self.tab, up) {
            (UsageTab::Tokens, true) => {
                self.breakdown_scroll = self.breakdown_scroll.saturating_sub(step as u16);
            }
            (UsageTab::Tokens, false) => {
                self.breakdown_scroll = self.breakdown_scroll.saturating_add(step as u16);
            }
            (UsageTab::Projects, true) => {
                self.select_project(self.project_selected.saturating_sub(step));
            }
            (UsageTab::Projects, false) => {
                self.select_project(self.project_selected.saturating_add(step));
            }
            (UsageTab::Skills, true) => {
                self.skill_selected = self.skill_selected.saturating_sub(step);
            }
            (UsageTab::Skills, false) => {
                let count = self.skill_entry_count();
                if count > 0 {
                    self.skill_selected = (self.skill_selected + step).min(count - 1);
                }
            }
        }
    }

    pub(crate) fn jump_top(&mut self) {
        match self.tab {
            UsageTab::Tokens => self.breakdown_scroll = 0,
            UsageTab::Projects => self.select_project(0),
            UsageTab::Skills => self.skill_selected = 0,
        }
    }

    pub(crate) fn jump_bottom(&mut self) {
        match self.tab {
            UsageTab::Tokens => self.breakdown_scroll = u16::MAX,
            UsageTab::Projects => self.select_project(usize::MAX),
            UsageTab::Skills => {
                let count = self.skill_entry_count();
                if count > 0 {
                    self.skill_selected = count - 1;
                }
            }
        }
    }

    pub(crate) fn project_entry_count(&self) -> usize {
        self.projects_report.as_ref().map_or(0, |report| report.projects.len())
    }

    pub(crate) fn selected_project(&self) -> Option<&ProjectUsageEntry> {
        self.projects_report.as_ref()?.projects.get(self.project_selected)
    }

    fn skill_entry_count(&self) -> usize {
        self.skill_report
            .as_ref()
            .map(|report| report.core.len() + report.occasional.len() + report.dormant.len())
            .unwrap_or(0)
    }

    pub(crate) fn selected_skill(&self) -> Option<&SkillUsageEntry> {
        let report = self.skill_report.as_ref()?;
        report.core.iter().chain(&report.occasional).chain(&report.dormant).nth(self.skill_selected)
    }

    pub(crate) fn time_label(&self) -> &'static str {
        match self.time_filter {
            TimeRange::Today => "Today",
            TimeRange::Week => "7d",
            TimeRange::Month => "30d",
            TimeRange::All => "All day",
        }
    }

    pub(crate) fn tab_label(&self) -> &'static str {
        match self.tab {
            UsageTab::Tokens => "tokens",
            UsageTab::Projects => "projects",
            UsageTab::Skills => "skills",
        }
    }

    pub(crate) fn metric_label(&self) -> &'static str {
        match self.chart_metric {
            ChartMetric::Tokens => "tokens",
            ChartMetric::Sessions => "sessions",
        }
    }

    pub(crate) fn granularity_label(&self) -> &'static str {
        if self.time_filter == TimeRange::Today {
            return "today";
        }
        match self.effective_granularity() {
            ChartGranularity::Daily => "daily",
            ChartGranularity::Weekly => "weekly",
        }
    }

    pub(crate) fn scope_chip_label(&self) -> Option<String> {
        if let Some((_, label)) = &self.drilldown {
            return Some(label.clone());
        }
        match &self.initial_scope {
            ProjectScope::Global => None,
            ProjectScope::Directory(dir) => Some(dir.clone()),
            ProjectScope::Repository { filter, .. } => {
                Some(filter.column_and_value().1.to_string())
            }
        }
    }
}
