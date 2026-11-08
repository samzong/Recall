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
    use crate::tui::usage_worker::UsageResponse;

    let mut app = app_with_sources();
    app.filters.active.sources = vec!["codex".to_string()];
    app.usage.time_filter = TimeRange::Month;
    app.usage.request_id = 1;
    app.usage.request_refresh();

    app.apply_usage_response(UsageResponse {
        id: 1,
        sources: Some(vec!["codex".to_string()]),
        time_range: TimeRange::Month,
        current_report: Err("stale current report".to_string()),
        all_time_report: Err("stale all-time report".to_string()),
        skill_audit_report: Err("stale skill audit".to_string()),
    });

    assert_eq!(app.usage.request_id, 2);
    assert!(app.usage.error.is_none());
    assert!(app.usage.skill_error.is_none());
    assert_eq!(app.usage.time_filter, TimeRange::Month);
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
