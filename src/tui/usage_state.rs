use std::time::Instant;

use crate::db::search::TimeRange;
use crate::skill_audit::{SkillAuditReport, SkillUsageEntry};
use crate::tui::usage_worker::UsageResponse;
use crate::usage::UsageReport;

const USAGE_LOADING_MIN_MS: u128 = 75;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UsageTab {
    Tokens,
    Skills,
}

pub(crate) struct UsageState {
    pub(crate) report: Option<UsageReport>,
    pub(crate) year_report: Option<UsageReport>,
    pub(crate) error: Option<String>,
    pub(crate) time_filter: TimeRange,
    pub(crate) refresh_requested_at: Option<Instant>,
    pub(crate) in_flight: bool,
    pub(crate) request_id: u64,
    pub(crate) breakdown_scroll: u16,
    pub(crate) tab: UsageTab,
    pub(crate) skill_report: Option<SkillAuditReport>,
    pub(crate) skill_error: Option<String>,
    pub(crate) skill_selected: usize,
}

impl Default for UsageState {
    fn default() -> Self {
        Self {
            report: None,
            year_report: None,
            error: None,
            time_filter: TimeRange::All,
            refresh_requested_at: None,
            in_flight: false,
            request_id: 0,
            breakdown_scroll: 0,
            tab: UsageTab::Tokens,
            skill_report: None,
            skill_error: None,
            skill_selected: 0,
        }
    }
}

impl UsageState {
    pub(crate) fn request_refresh(&mut self) {
        self.error = None;
        self.skill_error = None;
        self.request_id = self.request_id.saturating_add(1);
        self.refresh_requested_at = Some(Instant::now());
        self.reset_selection();
    }

    pub(crate) fn is_loading(&self) -> bool {
        self.refresh_requested_at.is_some() || self.in_flight
    }

    pub(crate) fn take_refresh(&mut self) -> Option<(u64, TimeRange)> {
        if self.refresh_requested_at?.elapsed().as_millis() < USAGE_LOADING_MIN_MS {
            return None;
        }
        self.refresh_requested_at = None;
        self.in_flight = true;
        Some((self.request_id, self.time_filter))
    }

    pub(crate) fn fail_refresh(&mut self, error: impl std::fmt::Display) {
        self.refresh_requested_at = None;
        self.in_flight = false;
        self.report = None;
        self.year_report = None;
        self.skill_report = None;
        self.error = Some(format!("Usage unavailable: {error}"));
        self.skill_error = Some(format!("Skill audit unavailable: {error}"));
    }

    pub(crate) fn apply_response(&mut self, response: UsageResponse, sources: Option<Vec<String>>) {
        if response.id != self.request_id
            || response.sources != sources
            || response.time_range != self.time_filter
        {
            return;
        }

        self.in_flight = false;
        self.report = response
            .current_report
            .inspect_err(|error| {
                self.error = Some(format!("Usage unavailable: {error}"));
            })
            .ok();
        self.year_report = response
            .all_time_report
            .inspect_err(|error| {
                self.error.get_or_insert_with(|| format!("Usage unavailable: {error}"));
            })
            .ok();
        self.skill_report = response
            .skill_audit_report
            .inspect_err(|error| {
                self.skill_error = Some(format!("Skill audit unavailable: {error}"));
            })
            .ok();
    }

    fn reset_selection(&mut self) {
        self.breakdown_scroll = 0;
        self.skill_selected = 0;
    }

    pub(crate) fn reset(&mut self) {
        self.time_filter = TimeRange::All;
        self.request_refresh();
    }

    pub(crate) fn cycle_tab(&mut self) {
        self.tab = match self.tab {
            UsageTab::Tokens => UsageTab::Skills,
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

    pub(crate) fn scroll(&mut self, up: bool) {
        match (self.tab, up) {
            (UsageTab::Tokens, true) => {
                self.breakdown_scroll = self.breakdown_scroll.saturating_sub(1);
            }
            (UsageTab::Tokens, false) => {
                self.breakdown_scroll = self.breakdown_scroll.saturating_add(1);
            }
            (UsageTab::Skills, true) => self.skill_selected = self.skill_selected.saturating_sub(1),
            (UsageTab::Skills, false) if self.skill_selected + 1 < self.skill_entry_count() => {
                self.skill_selected += 1;
            }
            _ => {}
        }
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
            UsageTab::Skills => "skills",
        }
    }
}
