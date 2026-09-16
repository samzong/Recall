use super::*;

#[test]
fn schema_migration_sets_current_version() {
    let store = setup();
    assert_eq!(schema::schema_version(&store.conn).unwrap(), schema::current_schema_version());
}

#[test]
fn store_insert_and_retrieve_session() {
    let store = setup();
    let session = make_session("s1", "test", "raw1", "Test session");
    store.insert_session(&session).unwrap();

    let sessions = store.list_recent_sessions(10).unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, "s1");
    assert_eq!(sessions[0].title, "Test session");
}

#[test]
fn store_insert_and_retrieve_messages() {
    let store = setup();
    let session = make_session("s1", "test", "raw1", "Test");
    store.insert_session(&session).unwrap();

    let messages = vec![
        make_message("s1", Role::User, "hello", 0),
        make_message("s1", Role::Assistant, "hi there", 1),
    ];
    store.insert_messages(&messages).unwrap();

    let loaded = store.get_messages("s1").unwrap();
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[0].role, Role::User);
    assert_eq!(loaded[0].content, "hello");
    assert_eq!(loaded[1].role, Role::Assistant);
}

#[test]
fn store_session_meta() {
    let store = setup();
    assert!(store.session_meta("test", "raw1").unwrap().is_none());

    let session = make_session("s1", "test", "raw1", "Test");
    store.insert_session(&session).unwrap();

    assert!(store.session_meta("test", "raw1").unwrap().is_some());
    assert!(store.session_meta("test", "raw999").unwrap().is_none());
}

#[test]
fn delete_session_cleans_embeddings() {
    let store = setup();
    let session = make_session("s1", "test", "raw1", "Test");
    store.insert_session(&session).unwrap();

    let messages = vec![make_message("s1", Role::User, "hello world test", 0)];
    store.insert_messages(&messages).unwrap();

    let msg_id: i64 = store
        .conn
        .query_row("SELECT id FROM messages WHERE session_id = 's1' LIMIT 1", [], |row| row.get(0))
        .unwrap();

    let embedding = vec![0.1f32; 384];
    store.upsert_embeddings(&[(msg_id, &embedding)]).unwrap();

    let count: i64 =
        store.conn.query_row("SELECT COUNT(*) FROM message_vec", [], |row| row.get(0)).unwrap();
    assert_eq!(count, 1);

    store.delete_session_data("test", "raw1").unwrap();

    let count: i64 =
        store.conn.query_row("SELECT COUNT(*) FROM message_vec", [], |row| row.get(0)).unwrap();
    assert_eq!(count, 0, "orphaned embedding must be cleaned on session delete");

    let sessions = store.list_recent_sessions(10).unwrap();
    assert!(sessions.is_empty());
}

#[test]
fn persist_session_writes_usage_events_and_report_aggregates() {
    let store = setup();
    let session = make_session("s1", "claude-code", "raw1", "Usage session");
    let messages = vec![
        make_message("s1", Role::User, "hello", 0),
        make_message("s1", Role::Assistant, "hi", 1),
    ];
    let usage = vec![make_usage_event("evt-1", 1_800_000_000_000, "claude-sonnet")];

    store.persist_session_with_usage(&session, &messages, &usage, Some(1)).unwrap();

    let count: i64 =
        store.conn.query_row("SELECT COUNT(*) FROM usage_events", [], |row| row.get(0)).unwrap();
    assert_eq!(count, 1);
    let state_count: i64 = store
        .conn
        .query_row("SELECT COUNT(*) FROM usage_session_state", [], |row| row.get(0))
        .unwrap();
    assert_eq!(state_count, 1);

    let report =
        build_usage_report(&store, &UsageFilters { sources: None, time_range: TimeRange::All })
            .unwrap();
    assert_eq!(report.summary.events, 1);
    assert_eq!(report.summary.sessions, 1);
    assert_eq!(report.summary.tokens.total_tokens, 21);
    assert_eq!(report.summary.token_source_events.get("observed"), Some(&1));
    assert_eq!(report.by_source[0].source, "claude-code");
    assert_eq!(report.by_model[0].model, "claude-sonnet");
}

#[test]
fn delete_session_cascades_usage_events() {
    let store = setup();
    let session = make_session("s1", "codex", "raw1", "Usage session");
    let messages = vec![make_message("s1", Role::User, "hello", 0)];
    let usage = vec![make_usage_event("evt-1", 1_800_000_000_000, "gpt-5")];
    store.persist_session_with_usage(&session, &messages, &usage, Some(1)).unwrap();

    store.delete_session_data("codex", "raw1").unwrap();

    let count: i64 =
        store.conn.query_row("SELECT COUNT(*) FROM usage_events", [], |row| row.get(0)).unwrap();
    assert_eq!(count, 0, "usage events must follow session lifecycle");
    let state_count: i64 = store
        .conn
        .query_row("SELECT COUNT(*) FROM usage_session_state", [], |row| row.get(0))
        .unwrap();
    assert_eq!(state_count, 0, "usage parser state must follow session lifecycle");
}

#[test]
fn persist_session_writes_session_events_and_state() {
    let store = setup();
    let session = make_session("s1", "codex", "raw1", "Event session");
    let messages = vec![make_message("s1", Role::Assistant, "[read_file] src/main.rs", 0)];
    let events = vec![make_session_event("file_read", Some("read_file"), Some("src/main.rs"))];

    store
        .persist_session_with_usage_and_events(&session, &messages, &[], None, &events, Some(1))
        .unwrap();

    let count: i64 =
        store.conn.query_row("SELECT COUNT(*) FROM session_events", [], |row| row.get(0)).unwrap();
    assert_eq!(count, 1);
    let state_count: i64 = store
        .conn
        .query_row("SELECT COUNT(*) FROM event_session_state", [], |row| row.get(0))
        .unwrap();
    assert_eq!(state_count, 1);

    let loaded = store.list_session_events_for_session("s1").unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].kind, "file_read");
    assert_eq!(loaded[0].name.as_deref(), Some("read_file"));
    assert_eq!(loaded[0].target.as_deref(), Some("src/main.rs"));
    assert_eq!(loaded[0].source_event_id.as_deref(), Some("42"));
    assert_eq!(loaded[0].tool_call_id.as_deref(), Some("call-42"));
    assert_eq!(loaded[0].is_meta, Some(false));
    assert_eq!(loaded[0].visibility, Some(crate::types::EvidenceVisibility::Visible));
}

#[test]
fn sync_resolves_cross_repository_files_and_keeps_native_paths() {
    use crate::adapters::{RawMessage, RawSession};
    use crate::types::{FileEvidence, FileEvidenceKind, FileOperation};
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("target");
    let other = temp.path().join("other");
    for (root, name) in [(&target, "target"), (&other, "other")] {
        std::fs::create_dir_all(root).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init"])
                .current_dir(root)
                .output()
                .unwrap()
                .status
                .success()
        );
        assert!(
            std::process::Command::new("git")
                .args([
                    "remote",
                    "add",
                    "origin",
                    &format!("https://github.com/fixture/{name}.git")
                ])
                .current_dir(root)
                .output()
                .unwrap()
                .status
                .success()
        );
    }
    let raw_path = target.join("src/deleted.rs").to_str().unwrap().to_string();
    let mut event = make_session_event("file_write", Some("Edit"), Some(&raw_path));
    event.files =
        [(raw_path.clone(), None), ("src/deleted.rs".into(), target.to_str().map(str::to_string))]
            .into_iter()
            .map(|(path, cwd)| FileEvidence {
                path,
                operation: FileOperation::Write,
                kind: FileEvidenceKind::Call,
                cwd,
                target: None,
            })
            .collect();
    for (path, kind) in [
        ("src/unknown.rs".to_string(), FileEvidenceKind::Command),
        (raw_path.clone(), FileEvidenceKind::Command),
        ("src/fallback.rs".to_string(), FileEvidenceKind::Call),
    ] {
        event.files.push(FileEvidence {
            path,
            operation: FileOperation::Write,
            kind,
            cwd: None,
            target: None,
        });
    }
    let raw = RawSession::search_only(
        "cross",
        other.to_str().map(str::to_string),
        1000,
        None,
        None,
        vec![RawMessage {
            role: Role::User,
            content: "Update the target file".into(),
            timestamp: Some(1000),
        }],
    )
    .with_events(vec![event], 1);
    let store = crate::sync::persist_raw_session_for_conformance(setup(), "codex", raw).unwrap();
    let session = store.get_session_by_source_id("codex", "cross").unwrap().unwrap();
    assert_eq!(session.directory.as_deref(), other.to_str());
    assert_eq!(session.repo_remote.as_deref(), Some("github.com/fixture/other"));
    let events = store.list_session_events_for_session(&session.id).unwrap();
    assert!(events[0].files[2].target.is_none());
    assert_eq!(events[0].files[3].target, events[0].files[0].target);
    assert!(events[0].files[4].target.is_none());
    assert_eq!(events[0].files[0].path, raw_path);
    assert_eq!(events[0].files[0].target, events[0].files[1].target);
    let file = events[0].files[0].target.as_ref().unwrap();
    assert_eq!(file.repo_remote.as_deref(), Some("github.com/fixture/target"));
    assert_eq!(file.repo_relative_path.as_deref(), Some("src/deleted.rs"));
    let alternate = temp.path().join("TARGET");
    if alternate.is_dir() {
        let resolved = crate::repo_identity::RepoIdentityCache::default()
            .resolve_file("src/deleted.rs", alternate.to_str())
            .unwrap();
        assert_eq!(&resolved, file);
    }
}

#[test]
fn upsert_embedding_replaces_existing() {
    let store = setup();
    let session = make_session("s1", "test", "raw1", "Test");
    store.insert_session(&session).unwrap();

    let messages = vec![make_message("s1", Role::User, "test content here", 0)];
    store.insert_messages(&messages).unwrap();

    let msg_id: i64 = store
        .conn
        .query_row("SELECT id FROM messages WHERE session_id = 's1' LIMIT 1", [], |row| row.get(0))
        .unwrap();

    let v1 = vec![0.1f32; 384];
    store.upsert_embeddings(&[(msg_id, &v1)]).unwrap();
    store.upsert_embeddings(&[(msg_id, &v1)]).unwrap();

    let count: i64 =
        store.conn.query_row("SELECT COUNT(*) FROM message_vec", [], |row| row.get(0)).unwrap();
    assert_eq!(count, 1, "upsert should not create duplicates");
}

#[test]
fn replace_session_rolls_back_delete_when_reinsert_fails() {
    let store = setup();
    let mut old_session = Session {
        source: "test".to_string(),
        source_id: "raw1".to_string(),
        title: "Original".to_string(),
        started_at: 1000,
        updated_at: Some(2000),
        message_count: 1,
        ..crate::types::test_support::session("s1")
    };
    old_session.is_import = true;
    let old_usage = [make_usage_event("old-usage", 1_800_000_000_000, "old-model")];
    let old_events = [make_session_event("old_event", Some("old_tool"), Some("old-target"))];
    store
        .persist_session_with_usage_and_events(
            &old_session,
            &[make_message("s1", Role::User, "oldrollbacktoken", 0)],
            &old_usage,
            Some(1),
            &old_events,
            Some(1),
        )
        .unwrap();
    let old_message_id = first_message_id(&store, "s1");
    store.upsert_embeddings(&[(old_message_id, &vec![0.1f32; 384])]).unwrap();

    let replacement = Session {
        source: "test".to_string(),
        source_id: "raw1".to_string(),
        title: "Replacement".to_string(),
        started_at: 1000,
        updated_at: Some(3000),
        message_count: 1,
        ..crate::types::test_support::session("s2")
    };
    let invalid_messages = [make_message("missing-session", Role::User, "new message", 0)];

    let result = store.replace_session_with_usage_and_events(
        "test",
        "raw1",
        &replacement,
        &invalid_messages,
        &[],
        None,
        &[],
        None,
    );
    assert!(result.is_err(), "replacement must fail before commit");

    assert_eq!(store.session_meta("test", "raw1").unwrap(), Some((Some(2000), 1)));
    let messages = store.get_messages("s1").unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].content, "oldrollbacktoken");
    assert!(store.imported_source_ids("test").unwrap().contains("raw1"));
    assert_eq!(count_rows(&store, "SELECT COUNT(*) FROM message_vec"), 1);
    assert_eq!(count_fts_matches(&store, "oldrollbacktoken"), 1);
    assert_eq!(count_rows(&store, "SELECT COUNT(*) FROM usage_events"), 1);
    assert_eq!(count_rows(&store, "SELECT COUNT(*) FROM usage_session_state"), 1);
    assert_eq!(count_rows(&store, "SELECT COUNT(*) FROM session_events"), 1);
    assert_eq!(count_rows(&store, "SELECT COUNT(*) FROM event_session_state"), 1);
    assert_eq!(
        count_rows(&store, "SELECT COUNT(*) FROM session_embedding_state WHERE session_id = 's1'"),
        1
    );
}

#[test]
fn replace_session_clears_import_marker_on_success() {
    let store = setup();
    let mut old_session = Session {
        source: "test".to_string(),
        source_id: "raw1".to_string(),
        title: "Original".to_string(),
        started_at: 1000,
        updated_at: Some(2000),
        message_count: 1,
        ..crate::types::test_support::session("s1")
    };
    old_session.is_import = true;
    let old_usage = [make_usage_event("old-usage", 1_800_000_000_000, "old-model")];
    let old_events = [make_session_event("old_event", Some("old_tool"), Some("old-target"))];
    store
        .persist_session_with_usage_and_events(
            &old_session,
            &[make_message("s1", Role::User, "oldsuccesstoken", 0)],
            &old_usage,
            Some(1),
            &old_events,
            Some(1),
        )
        .unwrap();
    let old_message_id = first_message_id(&store, "s1");
    store.upsert_embeddings(&[(old_message_id, &vec![0.1f32; 384])]).unwrap();

    let replacement = Session {
        source: "test".to_string(),
        source_id: "raw1".to_string(),
        title: "Replacement".to_string(),
        started_at: 1000,
        updated_at: Some(3000),
        message_count: 1,
        ..crate::types::test_support::session("s2")
    };
    let messages = [make_message("s2", Role::User, "newsuccesstoken", 0)];

    store
        .replace_session_with_usage_and_events(
            "test",
            "raw1",
            &replacement,
            &messages,
            &[],
            None,
            &[],
            None,
        )
        .unwrap();

    assert_eq!(store.session_meta("test", "raw1").unwrap(), Some((Some(3000), 1)));
    assert!(store.get_messages("s1").unwrap().is_empty());
    assert_eq!(store.get_messages("s2").unwrap()[0].content, "newsuccesstoken");
    assert!(store.imported_source_ids("test").unwrap().is_empty());
    assert_eq!(count_rows(&store, "SELECT COUNT(*) FROM message_vec"), 0);
    assert_eq!(count_fts_matches(&store, "oldsuccesstoken"), 0);
    assert_eq!(count_fts_matches(&store, "newsuccesstoken"), 1);
    assert_eq!(count_rows(&store, "SELECT COUNT(*) FROM usage_events"), 0);
    assert_eq!(count_rows(&store, "SELECT COUNT(*) FROM usage_session_state"), 0);
    assert_eq!(count_rows(&store, "SELECT COUNT(*) FROM session_events"), 0);
    assert_eq!(count_rows(&store, "SELECT COUNT(*) FROM event_session_state"), 0);
    assert_eq!(
        count_rows(&store, "SELECT COUNT(*) FROM session_embedding_state WHERE session_id = 's1'"),
        0
    );
    assert_eq!(
        count_rows(&store, "SELECT COUNT(*) FROM session_embedding_state WHERE session_id = 's2'"),
        1
    );
}

#[test]
fn stale_embedding_job_cannot_update_replacement_state() {
    let store = setup();
    let mut original = make_session("stable-id", "test", "raw-1", "Original");
    original.updated_at = Some(2_000);
    store
        .persist_session_with_usage_and_events(
            &original,
            &[make_message("stable-id", Role::User, "old content", 0)],
            &[],
            None,
            &[],
            None,
        )
        .unwrap();
    let job = store.claim_next_session_embedding_job().unwrap().unwrap();
    let status = || {
        store
            .conn
            .query_row(
                "SELECT status FROM session_embedding_state WHERE session_id = 'stable-id'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
    };
    assert_eq!(status(), "processing");

    let mut replacement = make_session("stable-id", "test", "raw-1", "Replacement");
    replacement.updated_at = Some(3_000);
    store
        .replace_session_with_usage_and_events(
            "test",
            "raw-1",
            &replacement,
            &[make_message("stable-id", Role::User, "new content", 0)],
            &[],
            None,
            &[],
            None,
        )
        .unwrap();
    assert_eq!(status(), "pending");

    store.update_session_embedding_progress(&job.session_id, 1).unwrap();
    assert_eq!(status(), "pending");
    store.complete_session_embedding(&job.session_id).unwrap();
    assert_eq!(status(), "pending");
    store.fail_session_embedding(&job.session_id, "stale").unwrap();
    assert_eq!(status(), "pending");
}

#[test]
fn usage_duplicates_update_on_refresh_but_backfill_rolls_back() {
    let store = setup();
    let session = make_session("s1", "test", "raw1", "usage");
    let first = make_usage_event("same", 1000, "first");
    let second = make_usage_event("same", 2000, "second");
    let events = [first, second];
    store.persist_session_with_usage(&session, &[], &events, Some(1)).unwrap();
    let persisted = store.list_usage_events_for_session("s1").unwrap();
    assert_eq!(persisted.len(), 1);
    assert_eq!(persisted[0].model, "second");
    assert!(
        store
            .persist_usage_events_for_existing_session("test", "raw1", &events, 2, Some(3000))
            .is_err()
    );
    let restored = store.list_usage_events_for_session("s1").unwrap();
    assert_eq!(serde_json::to_value(restored).unwrap(), serde_json::to_value(persisted).unwrap());
    assert_eq!(store.usage_state_meta_map("test").unwrap()["raw1"].parser_version, 1);
}
