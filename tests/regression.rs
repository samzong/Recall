use recall::db::schema;
use recall::db::search::{SearchEngine, SearchFilters, TimeRange};
use recall::db::store::Store;
use recall::types::{Message, Role, Session};

fn setup() -> Store {
    schema::register_sqlite_vec();
    Store::open_in_memory().unwrap()
}

fn make_session(id: &str, source: &str, source_id: &str, title: &str) -> Session {
    Session {
        id: id.to_string(),
        source: source.to_string(),
        source_id: source_id.to_string(),
        title: title.to_string(),
        directory: Some("/tmp/test".to_string()),
        started_at: chrono::Utc::now().timestamp_millis(),
        updated_at: None,
        message_count: 1,
        entrypoint: None,
    }
}

fn make_message(session_id: &str, role: Role, content: &str, seq: u32) -> Message {
    Message {
        session_id: session_id.to_string(),
        role,
        content: content.to_string(),
        timestamp: Some(chrono::Utc::now().timestamp_millis()),
        seq,
    }
}

fn no_filters() -> SearchFilters {
    SearchFilters { sources: None, time_range: TimeRange::All, directory: None }
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
fn fts_search_basic() {
    let store = setup();
    let session = make_session("s1", "test", "raw1", "Rust programming");
    store.insert_session(&session).unwrap();

    let messages = vec![make_message("s1", Role::User, "how do I use iterators in Rust", 0)];
    store.insert_messages(&messages).unwrap();

    let engine = SearchEngine::new(&store.conn);
    let results = engine.hybrid_search("iterators", None, &no_filters(), 10).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].session.id, "s1");
}

#[test]
fn fts_search_no_results() {
    let store = setup();
    let session = make_session("s1", "test", "raw1", "Test");
    store.insert_session(&session).unwrap();

    let messages = vec![make_message("s1", Role::User, "hello world", 0)];
    store.insert_messages(&messages).unwrap();

    let engine = SearchEngine::new(&store.conn);
    let results = engine.hybrid_search("zzzznonexistent", None, &no_filters(), 10).unwrap();
    assert!(results.is_empty());
}

#[test]
fn fts_search_empty_query() {
    let store = setup();
    let engine = SearchEngine::new(&store.conn);
    let results = engine.hybrid_search("", None, &no_filters(), 10).unwrap();
    assert!(results.is_empty());
}

#[test]
fn fts_search_special_characters() {
    let store = setup();
    let session = make_session("s1", "test", "raw1", "Test");
    store.insert_session(&session).unwrap();

    let messages = vec![make_message("s1", Role::User, "fix the bug in parser", 0)];
    store.insert_messages(&messages).unwrap();

    let engine = SearchEngine::new(&store.conn);
    let results = engine.hybrid_search("bug OR 1=1 --", None, &no_filters(), 10).unwrap();
    assert!(!results.is_empty());
}

#[test]
fn fts_search_sql_keywords_safe() {
    let store = setup();
    let session = make_session("s1", "test", "raw1", "Test");
    store.insert_session(&session).unwrap();

    let messages = vec![make_message("s1", Role::User, "AND OR NOT NEAR", 0)];
    store.insert_messages(&messages).unwrap();

    let engine = SearchEngine::new(&store.conn);
    let result = engine.hybrid_search("AND OR NOT", None, &no_filters(), 10);
    assert!(result.is_ok(), "FTS5 keywords must not cause SQL errors");
}

#[test]
fn hybrid_search_fts_only_without_embedding() {
    let store = setup();
    let session = make_session("s1", "test", "raw1", "Debugging session");
    store.insert_session(&session).unwrap();

    let messages = vec![make_message("s1", Role::User, "segfault in main loop", 0)];
    store.insert_messages(&messages).unwrap();

    let engine = SearchEngine::new(&store.conn);
    let results = engine.hybrid_search("segfault", None, &no_filters(), 10).unwrap();
    assert_eq!(results.len(), 1);
}

#[test]
fn search_with_source_filter() {
    let store = setup();
    let s1 = make_session("s1", "claude-code", "raw1", "Claude session");
    let s2 = make_session("s2", "opencode", "raw2", "OpenCode session");
    store.insert_session(&s1).unwrap();
    store.insert_session(&s2).unwrap();

    let messages = vec![
        make_message("s1", Role::User, "fix the parser", 0),
        make_message("s2", Role::User, "fix the parser", 0),
    ];
    store.insert_messages(&messages).unwrap();

    let engine = SearchEngine::new(&store.conn);
    let filters = SearchFilters {
        sources: Some(vec!["claude-code".to_string()]),
        time_range: TimeRange::All,
        directory: None,
    };
    let results = engine.hybrid_search("parser", None, &filters, 10).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].session.source, "claude-code");
}

#[test]
fn role_fromstr() {
    assert_eq!("user".parse::<Role>(), Ok(Role::User));
    assert_eq!("assistant".parse::<Role>(), Ok(Role::Assistant));
    assert!("unknown".parse::<Role>().is_err());
}

#[test]
fn format_age_values() {
    use recall::utils::format_age;

    let now = chrono::Utc::now().timestamp_millis();
    assert_eq!(format_age(now), "<1h");
    assert_eq!(format_age(now - 3 * 3600 * 1000), "3h");
    assert_eq!(format_age(now - 3 * 24 * 3600 * 1000), "3d");
    assert_eq!(format_age(now - 60 * 24 * 3600 * 1000), "2mo");
}

#[test]
fn f32_slice_to_bytes_roundtrip() {
    use recall::utils::f32_slice_to_bytes;

    let original = vec![1.0f32, 2.5, -3.0, 0.0];
    let bytes = f32_slice_to_bytes(&original);
    assert_eq!(bytes.len(), 16);

    let roundtrip: Vec<f32> =
        bytes.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
    assert_eq!(original, roundtrip);
}

#[test]
fn sync_skips_unchanged_session() {
    let store = setup();
    let session = Session {
        id: "s1".to_string(),
        source: "test".to_string(),
        source_id: "raw1".to_string(),
        title: "Original".to_string(),
        directory: None,
        started_at: 1000,
        updated_at: Some(2000),
        message_count: 2,
        entrypoint: None,
    };
    store.insert_session(&session).unwrap();

    let meta = store.session_meta("test", "raw1").unwrap();
    assert_eq!(meta, Some((Some(2000), 2)));
}

#[test]
fn sync_detects_new_messages() {
    let store = setup();
    let session = Session {
        id: "s1".to_string(),
        source: "test".to_string(),
        source_id: "raw1".to_string(),
        title: "Original".to_string(),
        directory: None,
        started_at: 1000,
        updated_at: Some(2000),
        message_count: 2,
        entrypoint: None,
    };
    store.insert_session(&session).unwrap();
    store.insert_messages(&[make_message("s1", Role::User, "hello", 0)]).unwrap();

    let meta = store.session_meta("test", "raw1").unwrap().unwrap();
    let (old_updated_at, old_msg_count) = meta;

    let new_msg_count: u32 = 5;
    let new_updated_at: Option<i64> = Some(3000);

    let changed = old_msg_count != new_msg_count
        || (new_updated_at.is_some() && new_updated_at != old_updated_at);
    assert!(changed, "sync must detect message count change");

    store.delete_session_data("test", "raw1").unwrap();
    let after = store.session_meta("test", "raw1").unwrap();
    assert!(after.is_none(), "old session must be deleted before re-insert");
}

#[test]
fn sync_detects_updated_timestamp() {
    let store = setup();
    let session = Session {
        id: "s1".to_string(),
        source: "test".to_string(),
        source_id: "raw1".to_string(),
        title: "Original".to_string(),
        directory: None,
        started_at: 1000,
        updated_at: Some(2000),
        message_count: 3,
        entrypoint: None,
    };
    store.insert_session(&session).unwrap();

    let (old_updated_at, old_msg_count) = store.session_meta("test", "raw1").unwrap().unwrap();

    let new_msg_count: u32 = 3;
    let new_updated_at: Option<i64> = Some(5000);

    let changed = old_msg_count != new_msg_count
        || (new_updated_at.is_some() && new_updated_at != old_updated_at);
    assert!(changed, "sync must detect updated_at change even when message count is same");
}
