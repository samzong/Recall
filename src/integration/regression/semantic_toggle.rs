use super::*;

fn embedding_status(store: &Store, session_id: &str) -> Option<String> {
    store
        .conn
        .query_row(
            "SELECT status FROM session_embedding_state WHERE session_id = ?1",
            [session_id],
            |row| row.get(0),
        )
        .ok()
}

#[test]
fn requeue_processing_jobs_recovers_killed_worker() {
    let store = setup();
    let session = make_session("s1", "test", "raw1", "Test");
    let messages = vec![make_message("s1", Role::User, "hello world test", 0)];
    store.persist_session_with_usage(&session, &messages, &[], Some(1)).unwrap();
    assert_eq!(embedding_status(&store, "s1").as_deref(), Some("pending"));

    store
        .conn
        .execute(
            "UPDATE session_embedding_state SET status = 'processing' WHERE session_id = 's1'",
            [],
        )
        .unwrap();

    store.requeue_processing_jobs().unwrap();

    assert_eq!(
        embedding_status(&store, "s1").as_deref(),
        Some("pending"),
        "orphaned processing job must return to the claimable queue"
    );
}

#[test]
fn requeue_processing_jobs_leaves_other_states_alone() {
    let store = setup();
    for (id, status) in [("s-done", "done"), ("s-failed", "failed"), ("s-pending", "pending")] {
        let session = make_session(id, "test", id, "Test");
        store.insert_session(&session).unwrap();
        store
            .conn
            .execute(
                "INSERT INTO session_embedding_state (session_id, status, units_total, units_done)
                 VALUES (?1, ?2, 0, 0)",
                rusqlite::params![id, status],
            )
            .unwrap();
    }

    store.requeue_processing_jobs().unwrap();

    assert_eq!(embedding_status(&store, "s-done").as_deref(), Some("done"));
    assert_eq!(embedding_status(&store, "s-failed").as_deref(), Some("failed"));
    assert_eq!(embedding_status(&store, "s-pending").as_deref(), Some("pending"));
}

#[test]
fn disabled_query_embedding_returns_none_before_touching_the_model() {
    let store = setup();
    let session = make_session("s1", "test", "raw1", "Test");
    let messages = vec![make_message("s1", Role::User, "hello world test", 0)];
    store.persist_session_with_usage(&session, &messages, &[], Some(1)).unwrap();
    // A `done` row is the state in which an ungated call would construct the
    // provider and emit "Loading embedding model...".
    store
        .conn
        .execute("UPDATE session_embedding_state SET status = 'done' WHERE session_id = 's1'", [])
        .unwrap();

    let mut emitted = Vec::new();
    let result =
        crate::query::query_embedding_with(&store, false, "hello", |m| emitted.push(m.to_string()))
            .unwrap();

    assert!(result.is_none());
    assert_eq!(emitted, ["Semantic search is disabled; showing full-text matches only."]);
}

#[test]
fn enabled_query_embedding_still_short_circuits_without_indexed_sessions() {
    let store = setup();
    let session = make_session("s1", "test", "raw1", "Test");
    let messages = vec![make_message("s1", Role::User, "hello world test", 0)];
    store.persist_session_with_usage(&session, &messages, &[], Some(1)).unwrap();
    // Session is `pending`: no done/processing sessions, so the enabled path
    // must return before constructing a provider.
    let mut emitted = Vec::new();
    let result =
        crate::query::query_embedding_with(&store, true, "hello", |m| emitted.push(m.to_string()))
            .unwrap();

    assert!(result.is_none());
    assert!(emitted.is_empty(), "enabled path must stay silent: {emitted:?}");
}

#[test]
fn disabled_session_search_stays_full_text() {
    let store = setup();
    let session = make_session("s1", "test", "raw1", "Semantic boundary check");
    let messages = vec![
        make_message("s1", Role::User, "lexically present keyword", 0),
        make_message("s1", Role::Assistant, "an answer", 1),
    ];
    store.persist_session_with_usage(&session, &messages, &[], Some(1)).unwrap();

    let engine = SearchEngine::new(&store.conn);
    let results = engine.hybrid_search("lexically present", None, &no_filters(), 10, 3).unwrap();

    assert_eq!(results.len(), 1);
    assert!(matches!(results[0].match_source, crate::types::MatchSource::Fts));
}
