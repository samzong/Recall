use super::*;

#[test]
fn project_picker_filters_by_path_tokens() {
    let mut app = app_with_sources();
    app.project_directories = vec![
        ProjectDirectory {
            directory: "/Users/x/git/samzong/Recall".to_string(),
            sessions: 10,
            last_seen: 2,
        },
        ProjectDirectory {
            directory: "/Users/x/git/openclaw".to_string(),
            sessions: 20,
            last_seen: 1,
        },
    ];
    app.filters.project_picker.query = "sam recall".to_string();

    let rows = app.project_picker_rows();

    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0], ProjectPickerRow::Project(0)));
}

#[test]
fn project_picker_space_toggles_pending_selection_without_committing() {
    let mut app = app_with_sources();
    app.project_directories = vec![ProjectDirectory {
        directory: "/Users/x/git/samzong/Recall".to_string(),
        sessions: 10,
        last_seen: 2,
    }];
    app.filters.project_picker.query = "recall".to_string();

    app.toggle_project_picker_row();

    assert_eq!(app.filters.project, Some("/Users/x/git/samzong/Recall".to_string()));
    assert!(app.filters.project_picker.dirty);
    assert_eq!(app.filters.active.scope, ProjectScope::Global);
}

#[test]
fn source_picker_space_toggles_while_filtering() {
    let mut app = app_with_sources();
    app.filters.source_picker.query = "cod".to_string();
    app.filters.source_picker.typing = true;

    app.toggle_source_picker_row();
    app.commit_source_picker_filter();

    assert_eq!(app.filters.draft.sources, vec!["codex".to_string()]);
    assert!(app.filters.active.sources.is_empty());
}

#[test]
fn app_preserves_disabled_sources_outside_visible_subset() {
    crate::db::schema::register_sqlite_vec();
    let store = Store::open_in_memory().unwrap();
    let mut config = AppConfig::default();
    config.disabled_sources = vec!["cline".to_string()];

    let app = App::new(&store, vec![source("codex", "Codex"), source("grok", "Grok")], config);

    assert_eq!(app.config.disabled_sources, vec!["cline".to_string()]);
    assert_eq!(app.enabled_sources().len(), 2);
}

#[test]
fn clear_filters_restores_relevance_sort() {
    let mut app = app_with_sources();
    app.filters.active.sort = SortOrder::Newest;
    app.open_filters();

    app.filters.clear_filters();

    assert_eq!(app.filters.active.sort, SortOrder::Newest);
    assert_eq!(app.filters.draft.sort, SortOrder::Relevance);
    assert!(app.filters.dirty);
}

#[test]
fn newest_sort_uses_latest_activity() {
    let app = app_with_sources();
    let mut results = vec![
        search_result_with_times("stale-newer-start", 900, Some(900)),
        search_result_with_times("active-older-start", 100, Some(1000)),
    ];

    app.apply_sort(&mut results);

    assert_eq!(results[0].session.source_id, "active-older-start");
    assert_eq!(results[1].session.source_id, "stale-newer-start");
}

#[test]
fn relevance_sort_keeps_engine_order_when_query_is_set() {
    let mut app = app_with_sources();
    app.query = "parser".to_string();
    let mut results = vec![
        search_result_with_times("stale-newer-start", 900, Some(900)),
        search_result_with_times("active-older-start", 100, Some(1000)),
    ];

    app.apply_sort(&mut results);

    assert_eq!(results[0].session.source_id, "stale-newer-start");
    assert_eq!(results[1].session.source_id, "active-older-start");
}

#[test]
fn stale_search_response_does_not_replace_current_results() {
    let (mut app, store) = app_and_store();
    app.query = "current".to_string();
    app.results = vec![search_result_with_times("current-result", 100, None)];
    app.active_search_id = 2;

    app.apply_search_response(
        &store,
        SearchResponse {
            id: 1,
            query: "old".to_string(),
            phase: SearchPhase::Text,
            result: Ok(vec![search_result_with_times("old-result", 200, None)]),
        },
    );

    assert_eq!(app.results[0].session.source_id, "current-result");
}

#[test]
fn text_search_response_keeps_semantic_refinement_pending() {
    let (mut app, store) = app_and_store();
    app.query = "parser".to_string();
    app.active_search_id = 1;
    app.config.semantic_enabled = true;
    app.semantic_progress.done_sessions = 1;

    app.apply_search_response(
        &store,
        SearchResponse {
            id: 1,
            query: "parser".to_string(),
            phase: SearchPhase::Text,
            result: Ok(vec![search_result_with_times("fts-result", 100, None)]),
        },
    );

    assert_eq!(app.results[0].session.source_id, "fts-result");
    assert!(app.search_in_flight);
    assert_eq!(app.search_feedback.as_deref(), Some("Refining semantic results..."));
}

#[test]
fn filter_time_range_left_right_defers_search_until_esc() {
    let (mut app, store) = app_and_store();
    app.mode = AppMode::Filters;
    app.query = "parser".to_string();
    app.filters.focus = FilterFocus::Time;

    app.handle_filters_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &store);

    assert_eq!(app.filters.active.time, TimeRange::All);
    assert!(app.filters.dirty);
    assert!(!app.search_pending);
    assert!(matches!(app.mode, AppMode::Filters));

    app.handle_filters_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &store);

    assert_eq!(app.filters.active.time, TimeRange::Today);
    assert!(matches!(app.mode, AppMode::Search));
    assert!(app.search_pending);
    assert_eq!(app.search_feedback.as_deref(), Some("Filters queued..."));
}

#[test]
fn filter_commit_invalidates_in_flight_search_response() {
    let (mut app, store) = app_and_store();
    app.query = "parser".to_string();
    app.mode = AppMode::Filters;
    app.filters.focus = FilterFocus::Time;
    app.results = vec![search_result_with_times("current-result", 100, None)];
    app.search_request_id = 1;
    app.active_search_id = 1;
    app.search_in_flight = true;

    app.handle_filters_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &store);
    app.handle_filters_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &store);
    app.apply_search_response(
        &store,
        SearchResponse {
            id: 1,
            query: "parser".to_string(),
            phase: SearchPhase::Text,
            result: Ok(vec![search_result_with_times("old-filter-result", 200, None)]),
        },
    );

    assert_eq!(app.results[0].session.source_id, "current-result");
    assert!(app.search_pending);
}

#[test]
fn stale_usage_response_does_not_replace_latest_filter_state() {
    use crate::tui::usage_worker::{DashboardReports, UsagePayload, UsageResponse};
    use crate::usage::UsageTarget;

    let mut app = app_with_sources();
    app.filters.active.sources = vec!["codex".to_string()];
    app.usage.time_filter = TimeRange::Month;
    app.usage.request_id = 1;
    app.usage.request_refresh();

    app.apply_usage_response(UsageResponse {
        id: 1,
        sources: Some(vec!["codex".to_string()]),
        time_range: TimeRange::Month,
        target: UsageTarget::Scope(ProjectScope::Global),
        payload: UsagePayload::Dashboard(Box::new(DashboardReports {
            current_report: Err("stale current report".to_string()),
            all_time_report: Err("stale all-time report".to_string()),
            projects_report: Err("stale projects report".to_string()),
        })),
    });
    app.apply_usage_response(UsageResponse {
        id: 1,
        sources: Some(vec!["codex".to_string()]),
        time_range: TimeRange::Month,
        target: UsageTarget::Scope(ProjectScope::Global),
        payload: UsagePayload::Skills(Err("stale skill audit".to_string())),
    });

    assert_eq!(app.usage.request_id, 2);
    assert!(app.usage.error.is_none());
    assert!(app.usage.projects_error.is_none());
    assert!(app.usage.skill_error.is_none());
    assert_eq!(app.usage.time_filter, TimeRange::Month);
}

#[test]
fn stale_usage_response_rejects_mismatched_target() {
    use crate::tui::usage_worker::{DashboardReports, UsagePayload, UsageResponse};
    use crate::usage::{ProjectPartitionKey, UsageTarget};

    let mut app = app_with_sources();
    app.usage.request_id = 1;
    app.usage.in_flight = true;
    app.usage.drilldown = Some((
        ProjectPartitionKey::Repository("github.com/samzong/Recall".to_string()),
        "samzong/Recall".to_string(),
    ));

    app.apply_usage_response(UsageResponse {
        id: 1,
        sources: None,
        time_range: TimeRange::All,
        target: UsageTarget::Scope(ProjectScope::Global),
        payload: UsagePayload::Dashboard(Box::new(DashboardReports {
            current_report: Err("wrong target".to_string()),
            all_time_report: Err("wrong target".to_string()),
            projects_report: Err("wrong target".to_string()),
        })),
    });

    assert!(app.usage.in_flight);
    assert!(app.usage.error.is_none());
}

#[test]
fn usage_v_and_g_toggles_and_reset_keep_initial_scope_without_refresh() {
    use crate::tui::usage_state::{ChartGranularity, ChartMetric};

    let (mut app, store) = app_and_store();
    app.mode = AppMode::Usage;
    app.filters.active.scope = ProjectScope::Directory("/search/cwd".to_string());
    app.usage.initial_scope = ProjectScope::Directory("/usage/explicit".to_string());
    app.usage.time_filter = TimeRange::All;
    app.usage.request_id = 5;
    app.usage.refresh_requested_at = None;

    assert_eq!(app.usage.effective_granularity(), ChartGranularity::Weekly);

    app.handle_usage_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE), &store);
    assert_eq!(app.usage.chart_metric, ChartMetric::Sessions);
    assert_eq!(app.usage.request_id, 5);
    assert!(app.usage.refresh_requested_at.is_none());

    app.handle_usage_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE), &store);
    assert_eq!(app.usage.granularity_override, Some(ChartGranularity::Daily));
    assert_eq!(app.usage.effective_granularity(), ChartGranularity::Daily);
    assert_eq!(app.usage.request_id, 5);
    assert!(app.usage.refresh_requested_at.is_none());

    app.handle_usage_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE), &store);
    assert_eq!(app.usage.time_filter, TimeRange::All);
    assert!(app.usage.granularity_override.is_none());
    assert!(app.usage.drilldown.is_none());
    assert_eq!(app.usage.initial_scope, ProjectScope::Directory("/usage/explicit".to_string()));
    assert_eq!(app.usage.request_id, 6);
}

#[test]
fn projects_drilldown_and_esc_reanchors_by_selected_project_key() {
    use std::collections::BTreeMap;

    use crate::tui::usage_worker::{DashboardReports, UsagePayload, UsageResponse};
    use crate::usage::{
        ProjectPartitionKey, ProjectUsageEntry, ProjectUsageReport, ProjectUsageSummary,
        TokenTotals, UsageReport, UsageSummary, UsageTarget,
    };

    fn entry(id: &str, tokens: i64) -> ProjectUsageEntry {
        ProjectUsageEntry {
            key: ProjectPartitionKey::Repository(format!("github.com/{id}")),
            project: id.to_string(),
            kind: "repository",
            repository: None,
            directory: None,
            sessions: 1,
            indexed_sessions: 1,
            events: 1,
            top_model: None,
            tokens: TokenTotals { total_tokens: tokens, ..TokenTotals::default() },
            by_model: Vec::new(),
            trend_tokens: vec![tokens],
            trend_sessions: vec![1],
            trend_label: "12w",
        }
    }

    fn empty_usage_report() -> UsageReport {
        UsageReport {
            scope: None,
            summary: UsageSummary {
                events: 0,
                sessions: 0,
                sources: 0,
                models: 0,
                token_source_events: BTreeMap::new(),
                tokens: TokenTotals::default(),
            },
            by_source: Vec::new(),
            by_model: Vec::new(),
            daily: Vec::new(),
            weekly: Vec::new(),
            monthly: Vec::new(),
            hourly_today_tokens: [0; 24],
            hourly_today_sessions: [0; 24],
            daily_all_sessions: BTreeMap::new(),
            weekly_all_sessions: BTreeMap::new(),
        }
    }

    fn projects_report(items: Vec<ProjectUsageEntry>) -> ProjectUsageReport {
        ProjectUsageReport {
            scope: None,
            summary: ProjectUsageSummary {
                projects: items.len(),
                events: items.len(),
                sessions: items.len(),
                indexed_sessions: items.len(),
                sources: 1,
                models: 1,
                tokens: TokenTotals::default(),
            },
            projects: items,
        }
    }

    let (mut app, store) = app_and_store();
    app.mode = AppMode::Usage;
    app.usage.tab = UsageTab::Projects;
    app.usage.projects_report =
        Some(projects_report(vec![entry("a/one", 200), entry("b/two", 100)]));

    app.handle_usage_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &store);
    assert_eq!(app.usage.project_selected, 1);
    let key_b = ProjectPartitionKey::Repository("github.com/b/two".to_string());
    assert_eq!(app.usage.selected_project_key.as_ref(), Some(&key_b));

    app.handle_usage_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &store);
    assert_eq!(app.usage.tab, UsageTab::Tokens);
    assert_eq!(app.usage.drilldown, Some((key_b.clone(), "b/two".to_string())));
    assert_eq!(
        app.usage.effective_target(),
        UsageTarget::Partition { key: key_b.clone(), label: "b/two".to_string() }
    );

    let req_id = app.usage.request_id;
    app.apply_usage_response(UsageResponse {
        id: req_id,
        sources: None,
        time_range: TimeRange::All,
        target: UsageTarget::Partition { key: key_b.clone(), label: "b/two".to_string() },
        payload: UsagePayload::Dashboard(Box::new(DashboardReports {
            current_report: Ok(empty_usage_report()),
            all_time_report: Ok(empty_usage_report()),
            projects_report: Ok(projects_report(vec![entry("b/two", 500), entry("a/one", 200)])),
        })),
    });
    app.apply_usage_response(UsageResponse {
        id: req_id,
        sources: None,
        time_range: TimeRange::All,
        target: UsageTarget::Partition { key: key_b.clone(), label: "b/two".to_string() },
        payload: UsagePayload::Skills(Ok(SkillAuditReport {
            summary: crate::skill_audit::SkillAuditSummary {
                installed: 0,
                core: 0,
                occasional: 0,
                dormant: 0,
            },
            core: Vec::new(),
            occasional: Vec::new(),
            dormant: Vec::new(),
            coverage_note: None,
        })),
    });

    app.handle_usage_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &store);
    assert!(app.usage.drilldown.is_none());
    assert_eq!(app.usage.tab, UsageTab::Projects);
    assert_eq!(app.usage.project_selected, 0);
    assert_eq!(app.usage.selected_project_key.as_ref(), Some(&key_b));
}

#[test]
fn skill_drill_down_is_disabled_while_usage_is_loading() {
    use crate::skill_audit::{SkillAuditSummary, SkillTier, SkillUsageEntry};

    let (mut app, store) = app_and_store();
    app.mode = AppMode::Usage;
    app.usage.tab = UsageTab::Skills;
    app.usage.in_flight = true;
    app.usage.skill_report = Some(SkillAuditReport {
        summary: SkillAuditSummary { installed: 1, core: 0, occasional: 1, dormant: 0 },
        core: Vec::new(),
        occasional: vec![SkillUsageEntry {
            id: "stale-skill".to_string(),
            tier: SkillTier::Occasional,
            invocations: 1,
            last_used: None,
            signals: Vec::new(),
            install_path: None,
            session_ids: vec!["stale-session".to_string()],
        }],
        dormant: Vec::new(),
        coverage_note: None,
    });

    app.handle_usage_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &store);

    assert!(matches!(app.mode, AppMode::Usage));
    assert!(app.status_message.is_none());
}
