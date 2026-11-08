use super::*;

#[test]
fn ctrl_o_from_search_confirms_copilot_app_open() {
    let (mut app, store) = app_and_store();
    app.results = vec![copilot_search_result()];
    store.insert_session(&app.results[0].session).unwrap();

    app.handle_search_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL), &store);

    assert!(matches!(app.mode, AppMode::ConfirmResume));
    let pending = app.pending_resume.as_ref().unwrap();
    assert!(matches!(pending.action, PendingCommandAction::OpenApp));
    assert!(
        pending
            .command
            .args
            .iter()
            .any(|arg| arg == "ghapp://sessions/7d5c993a-0966-4e3e-9622-7a39ba9576ba")
    );
}

#[test]
fn ctrl_o_from_search_confirms_codex_app_open() {
    let (mut app, store) = app_and_store();
    app.results = vec![codex_search_result()];
    store.insert_session(&app.results[0].session).unwrap();

    app.handle_search_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL), &store);

    assert!(matches!(app.mode, AppMode::ConfirmResume));
    let pending = app.pending_resume.as_ref().unwrap();
    assert!(matches!(pending.action, PendingCommandAction::OpenApp));
    assert!(
        pending
            .command
            .args
            .iter()
            .any(|arg| arg == "codex://threads/019e6d8d-588b-7fd2-a326-c525469ed120")
    );
    let session = &app.results[0].session;
    store.delete_session_data(&session.source, &session.source_id).unwrap();
    app.handle_confirm_resume_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &store);
    assert!(app.exec_on_exit.is_none());
    assert!(!app.should_quit);
}

#[test]
fn ctrl_s_from_search_requests_sync_instead_of_settings() {
    let (mut app, store) = app_and_store();

    app.handle_search_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL), &store);

    assert!(matches!(app.mode, AppMode::Search));
    let request = app.take_sync_request().expect("sync request");
    assert_eq!(request.scope, ProjectScope::Global);
    assert!(request.sources.is_none());
    assert!(app.sync_in_flight);
    assert!(app.take_sync_request().is_none());
}

#[test]
fn in_flight_sync_coalesces_another_ctrl_s() {
    let (mut app, store) = app_and_store();

    app.handle_search_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL), &store);
    assert!(app.take_sync_request().is_some());
    app.handle_search_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL), &store);
    assert!(app.take_sync_request().is_none());

    app.apply_sync_response(&store, Ok(()));
    assert!(app.take_sync_request().is_some());
}

#[test]
fn sync_response_reloads_recent_results() {
    let (mut app, store) = app_and_store();
    store.insert_session(&codex_search_result().session).unwrap();
    app.sync_in_flight = true;

    app.apply_sync_response(&store, Ok(()));

    assert_eq!(app.results.len(), 1);
    assert_eq!(app.results[0].session.id, "session1");
    assert!(!app.sync_in_flight);
}

#[test]
fn sync_response_keeps_selected_viewing_message_visible() {
    let (mut app, store) = app_and_store();
    let mut result = codex_search_result();
    result.session.message_count = 5;
    let messages = (0..5).map(|n| message(Role::User, None, n)).collect::<Vec<_>>();
    store.insert_session(&result.session).unwrap();
    store.insert_messages(&messages).unwrap();
    app.set_terminal_size(80, 10);
    app.mode = AppMode::Viewing;
    app.viewing.session = Some(result.session);
    app.viewing.messages = messages;
    app.viewing.lines = build_viewing_caches(&app.viewing.messages);
    app.viewing.selected_msg = 4;
    app.viewing.scroll_offset = 9;
    app.sync_in_flight = true;

    app.apply_sync_response(&store, Ok(()));

    assert_eq!(app.viewing.selected_msg, 4);
    let layout = viewing_layout(app.terminal_area);
    let pane = app.viewing.pane(layout.messages.width as usize);
    let selected_start = pane.start_of(app.viewing.selected_msg);
    assert!(selected_start >= app.viewing.scroll_offset);
    assert!(selected_start < app.viewing.scroll_offset + layout.messages.height as usize);
}

#[test]
fn sync_with_query_queues_search() {
    let (mut app, store) = app_and_store();
    app.query = "handoff".to_string();
    app.sync_in_flight = true;

    app.apply_sync_response(&store, Ok(()));

    assert!(app.search_pending);
    assert!(app.results.is_empty());
}

#[test]
fn imported_session_suppresses_resume_and_app_open() {
    let (mut app, store) = app_and_store();
    let mut result = codex_search_result();
    result.session.is_import = true;
    app.results = vec![result];

    for key_char in ['r', 'o'] {
        app.status_message = None;
        app.handle_search_key(
            KeyEvent::new(KeyCode::Char(key_char), KeyModifiers::CONTROL),
            &store,
        );

        assert!(
            !matches!(app.mode, AppMode::ConfirmResume),
            "ctrl+{key_char} must not open confirmation for imported session"
        );
        assert!(app.pending_resume.is_none());
        assert!(
            app.status_message.as_deref().unwrap_or_default().contains("Imported"),
            "status message must explain why nothing happened"
        );
    }
}

#[test]
fn imported_session_can_handoff_from_detail_view() {
    let mut app = app_with_sources();
    let mut result = codex_search_result();
    result.session.is_import = true;
    app.viewing.session = Some(result.session.clone());
    app.results = vec![result];
    app.viewing.messages = vec![message(Role::User, None, 0)];
    app.mode = AppMode::Viewing;

    app.present_handoff_targets(vec![handoff::HandoffTarget {
        id: "codex".to_string(),
        label: "Codex".to_string(),
    }]);
    assert!(matches!(app.mode, AppMode::HandoffTarget));

    app.handle_handoff_target_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert!(matches!(app.mode, AppMode::ConfirmResume));
    let pending = app.pending_resume.as_ref().unwrap();
    assert_eq!(pending.action, PendingCommandAction::Handoff);
    assert_eq!(pending.command.program, "codex");
    assert!(pending.command.args[0].contains("This is a handoff, not a native resume."));
    assert!(pending.command.args[0].contains("Use Recall session"));
}

#[test]
fn subagent_drilldown_switches_viewing_session_and_pops_back() {
    let (mut app, store) = app_and_store();

    let mut primary = codex_search_result();
    primary.session.id = "primary1".to_string();
    primary.session.source_id = "P".to_string();
    primary.session.title = "Primary task".to_string();
    let mut child = codex_search_result();
    child.session.id = "child1".to_string();
    child.session.source_id = "C".to_string();
    child.session.title = "review-diff".to_string();
    store.insert_session(&primary.session).unwrap();
    store.insert_session(&child.session).unwrap();
    store
        .conn
        .execute(
            "INSERT INTO session_parent_links
                     (session_id, relation, parent_source, parent_source_id)
                 VALUES (?1, 'spawn', ?2, ?3)",
            rusqlite::params!["child1", "codex", "P"],
        )
        .unwrap();

    app.results = vec![primary];
    app.selected_index = 0;
    app.enter_viewing(&store);

    assert_eq!(app.viewing.children.len(), 1, "primary should list its spawn child");
    assert_eq!(app.viewing.children[0].source_id, "C");
    assert_eq!(app.viewing.session.as_ref().unwrap().source_id, "P");

    app.handle_viewing_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE), &store);
    assert!(matches!(app.mode, AppMode::Subagents));

    app.handle_subagents_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &store);
    assert!(matches!(app.mode, AppMode::Viewing));
    assert_eq!(app.viewing.session.as_ref().unwrap().source_id, "C", "drilled into the child");
    assert_eq!(app.viewing.stack.len(), 1);

    app.handle_viewing_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), &store);
    assert!(matches!(app.mode, AppMode::Viewing), "q pops back to the parent, not search");
    assert_eq!(app.viewing.session.as_ref().unwrap().source_id, "P");
    assert!(app.viewing.stack.is_empty());

    app.handle_viewing_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), &store);
    assert!(matches!(app.mode, AppMode::Search), "q at the root leaves the viewer");
}
