use super::*;

#[test]
fn mouse_click_selects_visible_session_row() {
    let (mut app, store) = app_and_store();
    app.set_terminal_size(22, 12);
    app.results = numbered_results(6);

    app.handle_mouse_down(2, 6, &store);

    assert!(app.panel_focus == PanelFocus::SessionList);
    assert_eq!(app.selected_index, 1);
}

#[test]
fn mouse_click_selects_preview_message() {
    let (mut app, store) = app_and_store();
    app.set_terminal_size(80, 12);
    app.results = numbered_results(1);
    app.preview_messages = vec![message(Role::User, None, 0), message(Role::Assistant, None, 1)];

    app.handle_mouse_down(40, 8, &store);

    assert!(app.panel_focus == PanelFocus::Preview);
    assert_eq!(app.preview_selected_msg, 1);
}

#[test]
fn page_keys_move_session_list_by_visible_rows() {
    let (mut app, store) = app_and_store();
    app.set_terminal_size(80, 12);
    app.results = numbered_results(12);

    let down = KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE);
    let up = KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE);

    app.handle_key(down, &store);
    assert_eq!(app.selected_index, 5);
    assert_eq!(app.result_list_start(5), 1);

    app.handle_key(down, &store);
    assert_eq!(app.selected_index, 10);
    assert_eq!(app.result_list_start(5), 6);

    app.handle_key(up, &store);
    assert_eq!(app.selected_index, 5);
    assert_eq!(app.result_list_start(5), 5);
}

#[test]
fn page_keys_scroll_within_tall_preview_message() {
    let (mut app, store) = app_and_store();
    app.set_terminal_size(80, 12);
    app.results = numbered_results(1);
    app.preview_messages = vec![tall_message(Role::User, 0)];
    app.panel_focus = PanelFocus::Preview;

    let layout = search_layout(app.terminal_area);
    let inner = layout.preview_inner();
    let pane = app.preview_pane(inner.width as usize);
    let max_start = pane.total_rows() - inner.height as usize;

    app.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE), &store);
    assert_eq!(app.preview_selected_msg, 0);
    assert_eq!(app.preview_scroll_offset, max_start);
    assert_eq!(
        pane.scroll_start(
            app.preview_scroll_offset,
            app.preview_selected_msg,
            inner.height as usize
        ),
        max_start
    );

    app.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE), &store);
    assert_eq!(app.preview_selected_msg, 0);
    assert_eq!(app.preview_scroll_offset, 0);
}

#[test]
fn page_down_preview_reaches_last_message_at_bottom() {
    let (mut app, store) = app_and_store();
    app.set_terminal_size(80, 12);
    app.results = numbered_results(1);
    app.preview_messages = vec![
        message_with_lines(Role::User, 0, 1),
        message_with_lines(Role::Assistant, 1, 1),
        message_with_lines(Role::User, 2, 1),
        message_with_lines(Role::Assistant, 3, 2),
    ];
    app.panel_focus = PanelFocus::Preview;

    let layout = search_layout(app.terminal_area);
    let inner = layout.preview_inner();
    let pane = app.preview_pane(inner.width as usize);
    let max_start = pane.total_rows() - inner.height as usize;
    let down = KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE);

    app.handle_key(down, &store);
    app.handle_key(down, &store);

    assert_eq!(app.preview_selected_msg, 3);
    assert_eq!(app.preview_scroll_offset, max_start);
}

#[test]
fn search_scrollbar_bottom_selects_last_session() {
    let (mut app, store) = app_and_store();
    app.set_terminal_size(80, 12);
    app.results = numbered_results(20);

    let layout = search_layout(app.terminal_area);
    let column = layout.list.x + layout.list.width - 1;
    let row = layout.list.y + layout.list.height - 2;
    app.handle_mouse_down(column, row, &store);

    assert_eq!(app.result_scroll_offset, 15);
    assert_eq!(app.selected_index, 19);
}

#[test]
fn search_scrollbar_drag_keeps_capture_outside_scrollbar_column() {
    let (mut app, store) = app_and_store();
    app.set_terminal_size(80, 12);
    app.results = numbered_results(20);

    let layout = search_layout(app.terminal_area);
    let scrollbar_column = layout.list.x + layout.list.width - 1;
    let list_column = layout.list_inner().x;
    app.handle_mouse_down(scrollbar_column, layout.list.y + 2, &store);
    app.handle_mouse_drag(list_column, layout.list.y + layout.list.height - 2, &store);

    assert_eq!(app.result_scroll_offset, 15);
    assert_eq!(app.selected_index, 19);

    app.handle_mouse_up();
    app.handle_mouse_drag(list_column, layout.list_inner().y, &store);

    assert_eq!(app.selected_index, 15);
}

#[test]
fn preview_scrollbar_bottom_selects_last_message() {
    let (mut app, store) = app_and_store();
    app.set_terminal_size(80, 12);
    app.results = numbered_results(1);
    app.preview_messages = (0..5).map(|n| message(Role::User, None, n)).collect();

    let layout = search_layout(app.terminal_area);
    let column = layout.preview.x + layout.preview.width - 1;
    let row = layout.preview.y + layout.preview.height - 2;
    app.handle_mouse_down(column, row, &store);

    assert_eq!(app.preview_scroll_offset, 10);
    assert_eq!(app.preview_selected_msg, 4);
}

#[test]
fn preview_scrollbar_bottom_preserves_tall_final_message_bottom() {
    let (mut app, store) = app_and_store();
    app.set_terminal_size(80, 12);
    app.results = numbered_results(1);
    app.preview_messages = vec![message(Role::User, None, 0), tall_message(Role::Assistant, 1)];

    let layout = search_layout(app.terminal_area);
    let inner = layout.preview_inner();
    let pane = app.preview_pane(inner.width as usize);
    let max_start = pane.total_rows() - inner.height as usize;
    let column = layout.preview.x + layout.preview.width - 1;
    let row = layout.preview.y + layout.preview.height - 2;
    app.handle_mouse_down(column, row, &store);

    assert_eq!(app.preview_selected_msg, 1);
    assert_eq!(app.preview_scroll_offset, max_start);
    assert_eq!(
        pane.scroll_start(
            app.preview_scroll_offset,
            app.preview_selected_msg,
            inner.height as usize
        ),
        max_start
    );
}

#[test]
fn page_keys_move_viewing_by_visible_message_viewport() {
    let (mut app, store) = app_and_store();
    app.set_terminal_size(80, 10);
    app.mode = AppMode::Viewing;
    app.viewing.messages = (0..5).map(|n| message(Role::User, None, n)).collect();
    app.viewing.lines = build_viewing_caches(&app.viewing.messages);

    let down = KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE);
    let up = KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE);

    app.handle_key(down, &store);
    assert_eq!((app.viewing.selected_msg, app.viewing.scroll_offset), (2, 6));

    app.handle_key(up, &store);
    assert_eq!((app.viewing.selected_msg, app.viewing.scroll_offset), (0, 0));
}

#[test]
fn page_keys_scroll_within_tall_viewing_message() {
    let (mut app, store) = app_and_store();
    app.set_terminal_size(80, 10);
    app.mode = AppMode::Viewing;
    app.viewing.messages = vec![tall_message(Role::User, 0)];
    app.viewing.lines = build_viewing_caches(&app.viewing.messages);

    let layout = viewing_layout(app.terminal_area);
    let pane = app.viewing.pane(layout.messages.width as usize);
    let max_start = pane.total_rows() - layout.messages.height as usize;

    app.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE), &store);
    assert_eq!(app.viewing.selected_msg, 0);
    assert_eq!(app.viewing.scroll_offset, max_start);
    assert_eq!(
        pane.scroll_start(
            app.viewing.scroll_offset,
            app.viewing.selected_msg,
            layout.messages.height as usize
        ),
        max_start
    );

    app.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE), &store);
    assert_eq!(app.viewing.selected_msg, 0);
    assert_eq!(app.viewing.scroll_offset, 0);
}

#[test]
fn viewing_scrollbar_bottom_selects_last_message() {
    let (mut app, store) = app_and_store();
    app.set_terminal_size(80, 10);
    app.mode = AppMode::Viewing;
    app.viewing.messages = (0..5).map(|n| message(Role::User, None, n)).collect();
    app.viewing.lines = build_viewing_caches(&app.viewing.messages);

    let layout = viewing_layout(app.terminal_area);
    let area = layout.scrollbar_area();
    let column = area.x + area.width - 1;
    let row = area.y + area.height - 2;
    app.handle_mouse_down(column, row, &store);

    assert_eq!(app.viewing.scroll_offset, 9);
    assert_eq!(app.viewing.selected_msg, 4);
}

#[test]
fn viewing_scrollbar_bottom_preserves_tall_final_message_bottom() {
    let (mut app, store) = app_and_store();
    app.set_terminal_size(80, 10);
    app.mode = AppMode::Viewing;
    app.viewing.messages = vec![message(Role::User, None, 0), tall_message(Role::Assistant, 1)];
    app.viewing.lines = build_viewing_caches(&app.viewing.messages);

    let layout = viewing_layout(app.terminal_area);
    let pane = app.viewing.pane(layout.messages.width as usize);
    let max_start = pane.total_rows() - layout.messages.height as usize;
    let area = layout.scrollbar_area();
    let column = area.x + area.width - 1;
    let row = area.y + area.height - 2;
    app.handle_mouse_down(column, row, &store);

    assert_eq!(app.viewing.selected_msg, 1);
    assert_eq!(app.viewing.scroll_offset, max_start);
    assert_eq!(
        pane.scroll_start(
            app.viewing.scroll_offset,
            app.viewing.selected_msg,
            layout.messages.height as usize
        ),
        max_start
    );
}

#[test]
fn enter_viewing_seeds_search_query_and_jumps_to_first_match() {
    let (mut app, store) = app_and_store();
    let result = codex_search_result();
    store.insert_session(&result.session).unwrap();
    let mut first = message(Role::User, None, 0);
    first.content = "intro".to_string();
    let mut second = message(Role::Assistant, None, 1);
    second.content = "Deploy finished".to_string();
    store.insert_messages(&[first, second]).unwrap();
    app.results = vec![result];
    app.query = "deploy missing".to_string();

    app.enter_viewing(&store);

    assert!(matches!(app.mode, AppMode::Viewing));
    assert_eq!(app.viewing.search_query, "deploy missing");
    assert_eq!(&app.viewing.matches, &[1]);
    assert_eq!(app.viewing.selected_msg, 1);
}

#[test]
fn enter_viewing_strips_punctuation_from_seeded_query_like_fts() {
    let (mut app, store) = app_and_store();
    let result = codex_search_result();
    store.insert_session(&result.session).unwrap();
    let mut first = message(Role::User, None, 0);
    first.content = "intro".to_string();
    let mut second = message(Role::Assistant, None, 1);
    second.content = "run deploy(now) with config.yaml".to_string();
    store.insert_messages(&[first, second]).unwrap();
    app.results = vec![result];
    app.query = "deploy() config.yaml".to_string();

    app.enter_viewing(&store);

    assert_eq!(app.viewing.search_query, "deploy config yaml");
    assert_eq!(&app.viewing.matches, &[1]);
    assert_eq!(app.viewing.selected_msg, 1);
}

#[test]
fn enter_viewing_with_blank_query_starts_at_first_message() {
    let (mut app, store) = app_and_store();
    let result = codex_search_result();
    store.insert_session(&result.session).unwrap();
    store.insert_messages(&[message(Role::User, None, 0)]).unwrap();
    app.results = vec![result];
    app.query = "   ".to_string();

    app.enter_viewing(&store);

    assert!(matches!(app.mode, AppMode::Viewing));
    assert!(app.viewing.search_query.is_empty());
    assert!(&app.viewing.matches.is_empty());
    assert_eq!(app.viewing.selected_msg, 0);
}

#[test]
fn viewing_session_summary_counts_messages_duration_and_tokens() {
    let messages = vec![
        message(Role::User, Some(0), 0),
        message(Role::Assistant, Some(120_000), 1),
        message(Role::User, None, 2),
    ];
    let usage_events = vec![usage_event(10, 5), usage_event(-1, 4)];

    let summary = ViewingSessionSummary::from_session(&messages, None, &usage_events);

    assert_eq!(summary.user_messages, 2);
    assert_eq!(summary.total_messages, 3);
    assert_eq!(summary.duration_minutes, Some(2));
    assert_eq!(summary.usage_events, 2);
    assert_eq!(summary.tokens.input_tokens, 10);
    assert_eq!(summary.tokens.output_tokens, 9);
    assert_eq!(summary.tokens.cache_read_tokens, 6);
    assert_eq!(summary.tokens.cache_write_tokens, 4);
    assert_eq!(summary.tokens.reasoning_tokens, 2);
    assert_eq!(summary.tokens.total_tokens, 31);
}
