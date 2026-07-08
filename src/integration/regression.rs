use crate::adapters::copilot::parse_copilot_events;
use crate::adapters::gemini::parse_gemini_session;
use crate::adapters::kiro::parse_kiro_conversation;
use crate::config::AppConfig;
use crate::db::schema;
use crate::db::search::{RepoFilter, SearchEngine, SearchFilters, TimeRange};
use crate::db::store::Store;
use crate::export::{ExportIncludes, ExportOptions, write_jsonl};
use crate::types::{Message, RawSessionEvent, RawUsageEvent, Role, Session, TokenSource};
use crate::usage::{UsageFilters, build_usage_report};

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
        repo_remote: None,
        repo_slug: None,
        repo_name: None,
        started_at: chrono::Utc::now().timestamp_millis(),
        updated_at: None,
        message_count: 1,
        entrypoint: None,
        custom_title: None,
        summary: None,
        duration_minutes: None,
        source_file_path: None,
        is_import: false,
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

fn make_usage_event(key: &str, timestamp: i64, model: &str) -> RawUsageEvent {
    RawUsageEvent {
        event_key: key.to_string(),
        event_seq: 0,
        message_seq: Some(1),
        timestamp,
        model: model.to_string(),
        provider: "test-provider".to_string(),
        input_tokens: 10,
        output_tokens: 5,
        cache_read_tokens: 3,
        cache_write_tokens: 2,
        reasoning_tokens: 1,
        token_source: TokenSource::Observed,
        parser_version: 1,
        source_path: Some("/tmp/source.jsonl".to_string()),
        raw_usage_json: Some(r#"{"input_tokens":10}"#.to_string()),
    }
}

fn make_session_event(kind: &str, name: Option<&str>, target: Option<&str>) -> RawSessionEvent {
    RawSessionEvent {
        event_seq: 0,
        timestamp: Some(1_800_000_001_000),
        kind: kind.to_string(),
        actor: "assistant".to_string(),
        name: name.map(String::from),
        status: None,
        target: target.map(String::from),
        message_seq: Some(1),
        summary: Some("event summary".to_string()),
        source_path: Some("/tmp/source.jsonl".to_string()),
        source_event_id: Some("42".to_string()),
        attrs_json: Some(r#"{"path":"src/main.rs"}"#.to_string()),
        parser_version: 1,
    }
}

fn count_rows(store: &Store, sql: &str) -> i64 {
    store.conn.query_row(sql, [], |row| row.get(0)).unwrap()
}

fn count_fts_matches(store: &Store, query: &str) -> i64 {
    store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH ?1",
            [query],
            |row| row.get(0),
        )
        .unwrap()
}

fn first_message_id(store: &Store, session_id: &str) -> i64 {
    store
        .conn
        .query_row(
            "SELECT id FROM messages WHERE session_id = ?1 ORDER BY id LIMIT 1",
            [session_id],
            |row| row.get(0),
        )
        .unwrap()
}

fn no_filters() -> SearchFilters {
    SearchFilters { sources: None, time_range: TimeRange::All, directory: None, repo: None }
}

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
}

#[test]
fn export_jsonl_emits_session_messages_and_usage_events() {
    let store = setup();
    let mut session = make_session("s1", "codex", "raw1", "Export session");
    session.started_at = 1_800_000_000_000;
    session.updated_at = Some(1_800_000_001_000);
    session.message_count = 2;
    session.entrypoint = Some("codex resume raw1".to_string());
    session.custom_title = Some("Export custom title".to_string());
    session.summary = Some("Export summary".to_string());
    session.duration_minutes = Some(12);
    session.repo_remote = Some("github.com/samzong/Recall".to_string());
    session.repo_slug = Some("samzong/Recall".to_string());
    session.repo_name = Some("Recall".to_string());
    let messages = vec![
        make_message("s1", Role::User, "hello", 0),
        make_message("s1", Role::Assistant, "hi", 1),
    ];
    let usage = vec![make_usage_event("evt-1", 1_800_000_001_000, "gpt-5")];
    let events = vec![make_session_event("file_read", Some("read_file"), Some("src/main.rs"))];
    store
        .persist_session_with_usage_and_events(
            &session,
            &messages,
            &usage,
            Some(1),
            &events,
            Some(1),
        )
        .unwrap();

    let options = ExportOptions {
        session_ids: Vec::new(),
        sources: None,
        time_range: TimeRange::All,
        project: None,
        repo: None,
        limit: Some(10),
        includes: ExportIncludes::full(),
    };
    let mut out = Vec::new();
    write_jsonl(&store, &options, &mut out).unwrap();

    let text = String::from_utf8(out).unwrap();
    let lines: Vec<_> = text.lines().collect();
    assert_eq!(lines.len(), 1);
    let value: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(value["schema_version"], 4);
    assert_eq!(value["record_type"], "session");
    assert_eq!(value["session"]["source"], "codex");
    assert_eq!(value["session"]["source_id"], "raw1");
    assert_eq!(value["session"]["directory"], "/tmp/test");
    assert_eq!(value["session"]["repo_remote"], "github.com/samzong/Recall");
    assert_eq!(value["session"]["repo_slug"], "samzong/Recall");
    assert_eq!(value["session"]["repo_name"], "Recall");
    assert_eq!(value["session"]["custom_title"], "Export custom title");
    assert_eq!(value["session"]["summary"], "Export summary");
    assert_eq!(value["session"]["duration_minutes"], 12);
    assert_eq!(value["messages"][0]["seq"], 0);
    assert_eq!(value["messages"][0]["role"], "user");
    assert_eq!(value["messages"][1]["seq"], 1);
    assert_eq!(value["messages"][1]["role"], "assistant");
    assert_eq!(value["usage_events"][0]["event_key"], "evt-1");
    assert_eq!(value["usage_events"][0]["message_seq"], 1);
    assert_eq!(value["usage_events"][0]["model"], "gpt-5");
    assert_eq!(value["usage_events"][0]["token_source"], "observed");
    assert_eq!(value["usage_events"][0]["parser_version"], 1);
    assert_eq!(value["usage_events"][0]["source_path"], "/tmp/source.jsonl");
    assert_eq!(value["usage_events"][0]["raw_usage_json"], r#"{"input_tokens":10}"#);
    assert_eq!(value["events"][0]["kind"], "file_read");
    assert_eq!(value["events"][0]["name"], "read_file");
    assert_eq!(value["events"][0]["target"], "src/main.rs");
    assert_eq!(value["events"][0]["message_seq"], 1);
    assert_eq!(value["events"][0]["parser_version"], 1);
    assert_eq!(value["events"][0]["attrs_json"], r#"{"path":"src/main.rs"}"#);
}

#[test]
fn export_jsonl_applies_include_projection() {
    let store = setup();
    let session = make_session("s1", "codex", "raw1", "Projected export");
    let messages = vec![make_message("s1", Role::User, "hello", 0)];
    let usage = vec![make_usage_event("evt-1", 1_800_000_001_000, "gpt-5")];
    let events = vec![make_session_event("file_read", Some("read_file"), Some("src/main.rs"))];
    store
        .persist_session_with_usage_and_events(
            &session,
            &messages,
            &usage,
            Some(1),
            &events,
            Some(1),
        )
        .unwrap();

    let options = ExportOptions {
        session_ids: Vec::new(),
        sources: None,
        time_range: TimeRange::All,
        project: None,
        repo: None,
        limit: None,
        includes: ExportIncludes { messages: true, usage: false, events: false },
    };
    let mut out = Vec::new();
    write_jsonl(&store, &options, &mut out).unwrap();

    let text = String::from_utf8(out).unwrap();
    let value: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(value["messages"][0]["content"], "hello");
    assert_eq!(value["usage_events"].as_array().unwrap().len(), 0);
    assert_eq!(value["events"].as_array().unwrap().len(), 0);
}

#[test]
fn export_include_requires_messages() {
    let err = match crate::export::parse_export_includes(Some("metadata,usage")) {
        Ok(_) => panic!("messages should be required"),
        Err(err) => err,
    };
    assert_eq!(err.to_string(), "--include must include messages");
    assert!(crate::export::parse_export_includes(Some("metadata,messages")).unwrap().messages);
}

#[test]
fn export_jsonl_can_select_sessions_by_id() {
    let store = setup();
    for id in ["s1", "s2", "s3"] {
        let session = make_session(id, "codex", &format!("raw-{id}"), id);
        store.insert_session(&session).unwrap();
        store.insert_messages(&[make_message(id, Role::User, id, 0)]).unwrap();
    }

    let options = ExportOptions {
        session_ids: vec!["s3".to_string(), "s1".to_string()],
        sources: None,
        time_range: TimeRange::All,
        project: None,
        repo: None,
        limit: None,
        includes: ExportIncludes::full(),
    };
    let mut out = Vec::new();
    write_jsonl(&store, &options, &mut out).unwrap();

    let text = String::from_utf8(out).unwrap();
    let lines: Vec<_> = text.lines().collect();
    assert_eq!(lines.len(), 2);
    let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(first["session"]["id"], "s3");
    assert_eq!(second["session"]["id"], "s1");
}

#[test]
fn export_jsonl_applies_source_time_project_and_limit_filters() {
    let store = setup();
    let now = chrono::Utc::now().timestamp_millis();

    let mut newest = make_session("s-newest", "codex", "raw-newest", "Newest Codex");
    newest.started_at = now;
    newest.directory = Some("/tmp/project".to_string());
    let mut recent = make_session("s-recent", "codex", "raw-recent", "Recent Codex");
    recent.started_at = now - 1_000;
    recent.directory = Some("/tmp/project/subdir".to_string());
    let mut old = make_session("s-old", "codex", "raw-old", "Old Codex");
    old.started_at = now - 40 * 24 * 60 * 60 * 1_000;
    old.directory = Some("/tmp/project".to_string());
    let mut sibling = make_session("s-sibling", "codex", "raw-sibling", "Sibling Project");
    sibling.started_at = now + 2_000;
    sibling.directory = Some("/tmp/project-sibling".to_string());
    let mut other_source = make_session("s-other", "claude-code", "raw-other", "Other Source");
    other_source.started_at = now + 1_000;
    other_source.directory = Some("/tmp/project".to_string());

    for session in [&newest, &recent, &old, &sibling, &other_source] {
        store.insert_session(session).unwrap();
        store.insert_messages(&[make_message(&session.id, Role::User, &session.title, 0)]).unwrap();
    }

    let options = ExportOptions {
        session_ids: Vec::new(),
        sources: Some(vec!["codex".to_string()]),
        time_range: TimeRange::Month,
        project: Some("/tmp/project".to_string()),
        repo: None,
        limit: Some(1),
        includes: ExportIncludes::full(),
    };
    let mut out = Vec::new();
    write_jsonl(&store, &options, &mut out).unwrap();

    let text = String::from_utf8(out).unwrap();
    let lines: Vec<_> = text.lines().collect();
    assert_eq!(lines.len(), 1);
    let value: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(value["session"]["id"], "s-newest");
    assert_eq!(value["session"]["source"], "codex");
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
    let results = engine.hybrid_search("iterators", None, &no_filters(), 10, 3).unwrap();
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
    let results = engine.hybrid_search("zzzznonexistent", None, &no_filters(), 10, 3).unwrap();
    assert!(results.is_empty());
}

#[test]
fn fts_search_empty_query() {
    let store = setup();
    let engine = SearchEngine::new(&store.conn);
    let results = engine.hybrid_search("", None, &no_filters(), 10, 3).unwrap();
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
    let results = engine.hybrid_search("bug OR 1=1 --", None, &no_filters(), 10, 3).unwrap();
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
    let result = engine.hybrid_search("AND OR NOT", None, &no_filters(), 10, 3);
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
    let results = engine.hybrid_search("segfault", None, &no_filters(), 10, 3).unwrap();
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
        repo: None,
    };
    let results = engine.hybrid_search("parser", None, &filters, 10, 3).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].session.source, "claude-code");
}

#[test]
fn search_with_directory_filter_respects_project_boundary() {
    let store = setup();
    let mut exact = make_session("s1", "codex", "raw1", "Exact project");
    exact.directory = Some("/tmp/project".to_string());
    let mut child = make_session("s2", "opencode", "raw2", "Child project path");
    child.directory = Some("/tmp/project/subdir".to_string());
    let mut sibling = make_session("s3", "claude-code", "raw3", "Sibling prefix");
    sibling.directory = Some("/tmp/project-sibling".to_string());
    let mut missing = make_session("s4", "gemini-cli", "raw4", "Missing directory");
    missing.directory = None;

    for session in [&exact, &child, &sibling, &missing] {
        store.insert_session(session).unwrap();
    }
    let messages = vec![
        make_message("s1", Role::User, "fix the parser", 0),
        make_message("s2", Role::User, "fix the parser", 0),
        make_message("s3", Role::User, "fix the parser", 0),
        make_message("s4", Role::User, "fix the parser", 0),
    ];
    store.insert_messages(&messages).unwrap();

    let engine = SearchEngine::new(&store.conn);
    let filters = SearchFilters {
        sources: None,
        time_range: TimeRange::All,
        directory: Some("/tmp/project".to_string()),
        repo: None,
    };
    let results = engine.hybrid_search("parser", None, &filters, 10, 3).unwrap();
    let mut ids: Vec<String> = results.into_iter().map(|result| result.session.id).collect();
    ids.sort();

    assert_eq!(ids, vec!["s1".to_string(), "s2".to_string()]);
}

#[test]
fn recent_sessions_with_directory_filter_respects_project_boundary() {
    let store = setup();
    let mut exact = make_session("s1", "codex", "raw1", "Exact project");
    exact.directory = Some("/tmp/project".to_string());
    let mut sibling = make_session("s2", "opencode", "raw2", "Sibling prefix");
    sibling.directory = Some("/tmp/project-sibling".to_string());

    store.insert_session(&exact).unwrap();
    store.insert_session(&sibling).unwrap();

    let sessions = store
        .list_recent_sessions_for_search_scope(None, TimeRange::All, Some("/tmp/project"), None, 10)
        .unwrap();

    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, "s1");
}

#[test]
fn search_with_repo_filter_matches_sibling_worktrees() {
    let store = setup();
    let mut main = make_session("s1", "codex", "raw1", "Main worktree");
    main.directory = Some("/tmp/Recall".to_string());
    main.repo_remote = Some("github.com/samzong/Recall".to_string());
    main.repo_slug = Some("samzong/Recall".to_string());
    main.repo_name = Some("Recall".to_string());
    let mut sibling = make_session("s2", "opencode", "raw2", "Sibling worktree");
    sibling.directory = Some("/tmp/Recall--feature".to_string());
    sibling.repo_remote = Some("github.com/samzong/Recall".to_string());
    sibling.repo_slug = Some("samzong/Recall".to_string());
    sibling.repo_name = Some("Recall".to_string());
    let mut other = make_session("s3", "claude-code", "raw3", "Other repo");
    other.directory = Some("/tmp/other".to_string());
    other.repo_remote = Some("github.com/other/Recall".to_string());
    other.repo_slug = Some("other/Recall".to_string());
    other.repo_name = Some("Recall".to_string());

    for session in [&main, &sibling, &other] {
        store.insert_session(session).unwrap();
        store.insert_messages(&[make_message(&session.id, Role::User, "fix parser", 0)]).unwrap();
    }

    let engine = SearchEngine::new(&store.conn);
    let filters = SearchFilters {
        sources: None,
        time_range: TimeRange::All,
        directory: None,
        repo: Some(RepoFilter::Slug("samzong/Recall".to_string())),
    };
    let results = engine.hybrid_search("parser", None, &filters, 10, 3).unwrap();
    let mut ids: Vec<String> = results.into_iter().map(|result| result.session.id).collect();
    ids.sort();

    assert_eq!(ids, vec!["s1".to_string(), "s2".to_string()]);
}

#[test]
fn export_jsonl_applies_repo_filter() {
    let store = setup();
    let mut main = make_session("s1", "codex", "raw1", "Main worktree");
    main.repo_slug = Some("samzong/Recall".to_string());
    main.repo_name = Some("Recall".to_string());
    let mut other = make_session("s2", "codex", "raw2", "Other repo");
    other.repo_slug = Some("other/Recall".to_string());
    other.repo_name = Some("Recall".to_string());

    for session in [&main, &other] {
        store.insert_session(session).unwrap();
        store.insert_messages(&[make_message(&session.id, Role::User, &session.title, 0)]).unwrap();
    }

    let options = ExportOptions {
        session_ids: Vec::new(),
        sources: None,
        time_range: TimeRange::All,
        project: None,
        repo: Some(RepoFilter::Slug("samzong/Recall".to_string())),
        limit: None,
        includes: ExportIncludes::full(),
    };
    let mut out = Vec::new();
    write_jsonl(&store, &options, &mut out).unwrap();

    let text = String::from_utf8(out).unwrap();
    let lines: Vec<_> = text.lines().collect();
    assert_eq!(lines.len(), 1);
    let value: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(value["session"]["id"], "s1");
    assert_eq!(value["session"]["repo_slug"], "samzong/Recall");
}

#[test]
fn repo_name_filter_fails_when_ambiguous() {
    let store = setup();
    let mut first = make_session("s1", "codex", "raw1", "First");
    first.repo_slug = Some("samzong/Recall".to_string());
    first.repo_name = Some("Recall".to_string());
    let mut second = make_session("s2", "opencode", "raw2", "Second");
    second.repo_slug = Some("other/Recall".to_string());
    second.repo_name = Some("Recall".to_string());
    store.insert_session(&first).unwrap();
    store.insert_session(&second).unwrap();

    let err = store.resolve_repo_filter("Recall").unwrap_err().to_string();
    assert!(err.contains("ambiguous"));
    assert!(err.contains("samzong/Recall"));
    assert!(err.contains("other/Recall"));
}

#[test]
fn project_filter_prefers_indexed_relative_directory() {
    let store = setup();
    let mut session = make_session("s1", "codex", "raw1", "Relative directory");
    session.directory = Some("samzong/Recall".to_string());
    store.insert_session(&session).unwrap();

    let (directory, repo) =
        store.resolve_project_repo_filters(Some("samzong/Recall"), None).unwrap();

    assert_eq!(directory.as_deref(), Some("samzong/Recall"));
    assert_eq!(repo, None);
}

#[test]
fn role_fromstr() {
    assert_eq!("user".parse::<Role>(), Ok(Role::User));
    assert_eq!("assistant".parse::<Role>(), Ok(Role::Assistant));
    assert!("unknown".parse::<Role>().is_err());
}

#[test]
fn format_age_values() {
    use crate::utils::format_age;

    let now = chrono::Utc::now().timestamp_millis();
    assert_eq!(format_age(now), "<1h");
    assert_eq!(format_age(now - 3 * 3600 * 1000), "3h");
    assert_eq!(format_age(now - 3 * 24 * 3600 * 1000), "3d");
    assert_eq!(format_age(now - 60 * 24 * 3600 * 1000), "2mo");
}

#[test]
fn f32_slice_to_bytes_roundtrip() {
    use crate::utils::f32_slice_to_bytes;

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
        repo_remote: None,
        repo_slug: None,
        repo_name: None,
        started_at: 1000,
        updated_at: Some(2000),
        message_count: 2,
        entrypoint: None,
        custom_title: None,
        summary: None,
        duration_minutes: None,
        source_file_path: None,
        is_import: false,
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
        repo_remote: None,
        repo_slug: None,
        repo_name: None,
        started_at: 1000,
        updated_at: Some(2000),
        message_count: 2,
        entrypoint: None,
        custom_title: None,
        summary: None,
        duration_minutes: None,
        source_file_path: None,
        is_import: false,
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
}

#[test]
fn replace_session_rolls_back_delete_when_reinsert_fails() {
    let store = setup();
    let mut old_session = Session {
        id: "s1".to_string(),
        source: "test".to_string(),
        source_id: "raw1".to_string(),
        title: "Original".to_string(),
        directory: None,
        repo_remote: None,
        repo_slug: None,
        repo_name: None,
        started_at: 1000,
        updated_at: Some(2000),
        message_count: 1,
        entrypoint: None,
        custom_title: None,
        summary: None,
        duration_minutes: None,
        source_file_path: None,
        is_import: false,
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
        id: "s2".to_string(),
        source: "test".to_string(),
        source_id: "raw1".to_string(),
        title: "Replacement".to_string(),
        directory: None,
        repo_remote: None,
        repo_slug: None,
        repo_name: None,
        started_at: 1000,
        updated_at: Some(3000),
        message_count: 1,
        entrypoint: None,
        custom_title: None,
        summary: None,
        duration_minutes: None,
        source_file_path: None,
        is_import: false,
    };
    // Foreign keys are enabled in setup(); this fails after the replacement deletes old rows.
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
        id: "s1".to_string(),
        source: "test".to_string(),
        source_id: "raw1".to_string(),
        title: "Original".to_string(),
        directory: None,
        repo_remote: None,
        repo_slug: None,
        repo_name: None,
        started_at: 1000,
        updated_at: Some(2000),
        message_count: 1,
        entrypoint: None,
        custom_title: None,
        summary: None,
        duration_minutes: None,
        source_file_path: None,
        is_import: false,
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
        id: "s2".to_string(),
        source: "test".to_string(),
        source_id: "raw1".to_string(),
        title: "Replacement".to_string(),
        directory: None,
        repo_remote: None,
        repo_slug: None,
        repo_name: None,
        started_at: 1000,
        updated_at: Some(3000),
        message_count: 1,
        entrypoint: None,
        custom_title: None,
        summary: None,
        duration_minutes: None,
        source_file_path: None,
        is_import: false,
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
fn sync_detects_updated_timestamp() {
    let store = setup();
    let session = Session {
        id: "s1".to_string(),
        source: "test".to_string(),
        source_id: "raw1".to_string(),
        title: "Original".to_string(),
        directory: None,
        repo_remote: None,
        repo_slug: None,
        repo_name: None,
        started_at: 1000,
        updated_at: Some(2000),
        message_count: 3,
        entrypoint: None,
        custom_title: None,
        summary: None,
        duration_minutes: None,
        source_file_path: None,
        is_import: false,
    };
    store.insert_session(&session).unwrap();

    let (old_updated_at, old_msg_count) = store.session_meta("test", "raw1").unwrap().unwrap();

    let new_msg_count: u32 = 3;
    let new_updated_at: Option<i64> = Some(5000);

    let changed = old_msg_count != new_msg_count
        || (new_updated_at.is_some() && new_updated_at != old_updated_at);
    assert!(changed, "sync must detect updated_at change even when message count is same");
}

#[test]
fn gemini_parser_plain_conversation() {
    let json = r#"{
        "sessionId": "abc-123",
        "projectHash": "deadbeef",
        "startTime": "2025-11-13T13:48:00.000Z",
        "lastUpdated": "2025-11-13T14:00:00.000Z",
        "messages": [
            {"id": 0, "type": "user", "content": "hello", "timestamp": "2025-11-13T13:48:05.000Z"},
            {"id": 1, "type": "gemini", "content": "hi there", "timestamp": "2025-11-13T13:48:10.000Z"}
        ]
    }"#;

    let session = parse_gemini_session(json, "fallback").unwrap().unwrap();
    assert_eq!(session.source_id, "abc-123");
    assert_eq!(session.directory, None, "gemini has no resolvable cwd");
    assert_eq!(session.messages.len(), 2);
    assert!(matches!(session.messages[0].role, Role::User));
    assert_eq!(session.messages[0].content, "hello");
    assert!(matches!(session.messages[1].role, Role::Assistant));
    assert_eq!(session.messages[1].content, "hi there");
}

#[test]
fn gemini_parser_indexes_tool_calls() {
    let json = r##"{
        "sessionId": "xyz",
        "startTime": "2025-11-13T13:48:00.000Z",
        "messages": [
            {"id": 0, "type": "user", "content": "read README", "timestamp": "2025-11-13T13:48:00.000Z"},
            {
                "id": 1,
                "type": "gemini",
                "content": "Let me read the file.",
                "timestamp": "2025-11-13T13:48:05.000Z",
                "toolCalls": [{
                    "id": "t1",
                    "name": "read_file",
                    "args": {"path": "/tmp/README.md"},
                    "result": [{"text": "# My Project\nHello world."}]
                }]
            }
        ]
    }"##;

    let session = parse_gemini_session(json, "fallback").unwrap().unwrap();
    let assistant = &session.messages[1];
    assert!(
        assistant.content.contains("Let me read the file"),
        "prose preserved: {}",
        assistant.content
    );
    assert!(assistant.content.contains("[read_file]"), "tool name indexed: {}", assistant.content);
    assert!(
        assistant.content.contains("/tmp/README.md"),
        "tool args indexed: {}",
        assistant.content
    );
    assert!(
        assistant.content.contains("Hello world"),
        "tool result indexed: {}",
        assistant.content
    );
}

#[test]
fn gemini_parser_skips_info_messages() {
    let json = r#"{
        "sessionId": "s",
        "startTime": "2025-11-13T13:48:00.000Z",
        "messages": [
            {"id": 0, "type": "info", "content": "CLI update available"},
            {"id": 1, "type": "user", "content": "hi", "timestamp": "2025-11-13T13:48:05.000Z"}
        ]
    }"#;

    let session = parse_gemini_session(json, "fallback").unwrap().unwrap();
    assert_eq!(session.messages.len(), 1, "info messages should be skipped");
    assert_eq!(session.messages[0].content, "hi");
}

#[test]
fn gemini_parser_empty_returns_none() {
    let json = r#"{"sessionId": "s", "messages": []}"#;
    assert!(parse_gemini_session(json, "fallback").unwrap().is_none());
}

#[test]
fn kiro_parser_prompt_and_response() {
    let json = r#"{
        "history": [{
            "user": {
                "content": {"Prompt": {"prompt": "how use skill"}},
                "timestamp": "2026-04-11T00:34:50.549369+08:00"
            },
            "assistant": {
                "Response": {"message_id": "m1", "content": "Skills are markdown files."}
            },
            "request_metadata": {"request_start_timestamp_ms": 1775838890550}
        }]
    }"#;

    let session =
        parse_kiro_conversation("conv1", "/Users/x/proj", json, 1000, 2000).unwrap().unwrap();
    assert_eq!(session.source_id, "conv1");
    assert_eq!(session.directory.as_deref(), Some("/Users/x/proj"));
    assert_eq!(session.started_at, 1000);
    assert_eq!(session.updated_at, Some(2000));
    assert_eq!(session.messages.len(), 2);
    assert_eq!(session.messages[0].content, "how use skill");
    assert_eq!(session.messages[1].content, "Skills are markdown files.");
    assert_eq!(session.messages[1].timestamp, Some(1775838890550));
}

#[test]
fn kiro_parser_assistant_tool_use() {
    let json = r#"{
        "history": [{
            "user": {
                "content": {"Prompt": {"prompt": "analyze project"}}
            },
            "assistant": {
                "ToolUse": {
                    "message_id": "m1",
                    "content": "Let me look around.",
                    "tool_uses": [
                        {"id": "t1", "name": "fs_read", "args": {"path": "/src"}},
                        {"id": "t2", "name": "execute_bash", "args": {"command": "ls"}}
                    ]
                }
            },
            "request_metadata": {"request_start_timestamp_ms": 1775838890550}
        }]
    }"#;

    let session = parse_kiro_conversation("c", "/proj", json, 0, 0).unwrap().unwrap();
    let assistant = &session.messages[1];
    assert!(
        assistant.content.contains("Let me look around"),
        "prose preserved: {}",
        assistant.content
    );
    assert!(assistant.content.contains("[fs_read]"), "first tool indexed: {}", assistant.content);
    assert!(
        assistant.content.contains("[execute_bash]"),
        "second tool indexed: {}",
        assistant.content
    );
    assert!(assistant.content.contains("/src"), "fs_read args indexed: {}", assistant.content);
}

#[test]
fn kiro_parser_tool_use_results_text_and_json() {
    let json = r#"{
        "history": [{
            "user": {
                "content": {
                    "ToolUseResults": {
                        "tool_use_results": [
                            {
                                "tool_use_id": "t1",
                                "content": [{"Text": "file contents here"}]
                            },
                            {
                                "tool_use_id": "t2",
                                "content": [{"Json": {"status": "ok", "rows": 42}}]
                            }
                        ]
                    }
                }
            },
            "assistant": {"Response": {"message_id": "m", "content": "done"}}
        }]
    }"#;

    let session = parse_kiro_conversation("c", "/proj", json, 0, 0).unwrap().unwrap();
    let user_msg = &session.messages[0];
    assert!(
        user_msg.content.contains("file contents here"),
        "Text variant indexed: {}",
        user_msg.content
    );
    assert!(user_msg.content.contains("\"status\""), "Json variant indexed: {}", user_msg.content);
    assert!(user_msg.content.contains("42"), "Json values indexed: {}", user_msg.content);
}

#[test]
fn kiro_parser_empty_history_returns_none() {
    let json = r#"{"history": []}"#;
    assert!(parse_kiro_conversation("c", "/proj", json, 0, 0).unwrap().is_none());
}

#[test]
fn copilot_parser_plain_conversation() {
    let jsonl = r#"{"type":"session.start","data":{"sessionId":"sess-1","startTime":"2026-02-26T06:29:59.692Z","context":{"cwd":"/Users/x/proj","repository":"x/proj","branch":"main"}},"id":"e1","timestamp":"2026-02-26T06:29:59.802Z","parentId":null}
{"type":"user.message","data":{"content":"how do I run tests","transformedContent":"wrapped","attachments":[]},"id":"e2","timestamp":"2026-02-26T06:30:00.000Z","parentId":"e1"}
{"type":"assistant.message","data":{"messageId":"m1","content":"Run make check","toolRequests":[]},"id":"e3","timestamp":"2026-02-26T06:30:01.000Z","parentId":"e2"}"#;

    let session = parse_copilot_events(jsonl, "fallback").unwrap().unwrap();
    assert_eq!(session.source_id, "sess-1");
    assert_eq!(session.directory.as_deref(), Some("/Users/x/proj"));
    assert_eq!(session.messages.len(), 2);
    assert!(matches!(session.messages[0].role, Role::User));
    assert_eq!(session.messages[0].content, "how do I run tests");
    assert!(matches!(session.messages[1].role, Role::Assistant));
    assert_eq!(session.messages[1].content, "Run make check");
}

#[test]
fn copilot_parser_indexes_tool_requests_and_results() {
    let jsonl = r##"{"type":"session.start","data":{"sessionId":"sess-2","startTime":"2026-02-26T06:29:59.692Z","context":{"cwd":"/proj"}},"id":"e1","timestamp":"2026-02-26T06:29:59.802Z","parentId":null}
{"type":"assistant.message","data":{"messageId":"m1","content":"Let me read the file.","toolRequests":[{"toolCallId":"tc1","name":"read_file","arguments":{"path":"/tmp/README.md"},"type":"function"}]},"id":"e2","timestamp":"2026-02-26T06:30:00.000Z","parentId":"e1"}
{"type":"tool.execution_start","data":{"toolCallId":"tc1","toolName":"read_file","arguments":{"path":"/tmp/README.md"}},"id":"e3","timestamp":"2026-02-26T06:30:00.100Z","parentId":"e2"}
{"type":"tool.execution_complete","data":{"toolCallId":"tc1","success":true,"result":{"content":"short summary","detailedContent":"# My Project\nHello world."}},"id":"e4","timestamp":"2026-02-26T06:30:00.500Z","parentId":"e3"}"##;

    let session = parse_copilot_events(jsonl, "fallback").unwrap().unwrap();
    assert_eq!(session.messages.len(), 2);
    let assistant = &session.messages[0];
    assert!(
        assistant.content.contains("Let me read the file"),
        "prose preserved: {}",
        assistant.content
    );
    assert!(assistant.content.contains("[read_file]"), "tool name indexed: {}", assistant.content);
    assert!(
        assistant.content.contains("/tmp/README.md"),
        "tool args indexed: {}",
        assistant.content
    );
    let tool_result = &session.messages[1];
    assert!(
        tool_result.content.contains("[read_file]"),
        "tool result tagged with name: {}",
        tool_result.content
    );
    assert!(
        tool_result.content.contains("Hello world"),
        "detailedContent preferred over content: {}",
        tool_result.content
    );
}

#[test]
fn copilot_parser_skips_empty_and_unknown() {
    let jsonl = r#"{"type":"session.start","data":{"sessionId":"s","startTime":"2026-02-26T06:29:59.692Z","context":{"cwd":"/p"}},"id":"e1","timestamp":"2026-02-26T06:29:59.802Z"}
{"type":"session.info","data":{"msg":"anything"},"id":"e2","timestamp":"2026-02-26T06:30:00.000Z"}
{"type":"user.message","data":{"content":"   "},"id":"e3","timestamp":"2026-02-26T06:30:01.000Z"}
{"type":"assistant.message","data":{"messageId":"m","content":"","toolRequests":[]},"id":"e4","timestamp":"2026-02-26T06:30:02.000Z"}
{"type":"user.message","data":{"content":"real question"},"id":"e5","timestamp":"2026-02-26T06:30:03.000Z"}"#;

    let session = parse_copilot_events(jsonl, "fallback").unwrap().unwrap();
    assert_eq!(session.messages.len(), 1, "empty and unknown events should be skipped");
    assert_eq!(session.messages[0].content, "real question");
}

#[test]
fn copilot_parser_empty_returns_none() {
    let jsonl = r#"{"type":"session.start","data":{"sessionId":"s","startTime":"2026-02-26T06:29:59.692Z"},"id":"e1","timestamp":"2026-02-26T06:29:59.802Z"}"#;
    assert!(parse_copilot_events(jsonl, "fallback").unwrap().is_none());
}

#[test]
fn copilot_parser_falls_back_to_dir_id_when_session_missing() {
    let jsonl = r#"{"type":"user.message","data":{"content":"hi"},"id":"e1","timestamp":"2026-02-26T06:30:00.000Z"}"#;
    let session = parse_copilot_events(jsonl, "dir-uuid").unwrap().unwrap();
    assert_eq!(session.source_id, "dir-uuid");
}

#[test]
fn config_migrates_legacy_enabled_sources() {
    let legacy_json = r#"{
        "enabled_sources": ["claude-code", "codex", "opencode"],
        "sync_window": "week"
    }"#;
    let mut config: AppConfig = serde_json::from_str(legacy_json).unwrap();

    let known = vec![
        ("claude-code".to_string(), "CC".to_string()),
        ("opencode".to_string(), "OC".to_string()),
        ("codex".to_string(), "CDX".to_string()),
        ("gemini-cli".to_string(), "GEM".to_string()),
        ("kiro-cli".to_string(), "KIRO".to_string()),
    ];
    config.normalize_sources(&known);

    assert!(config.is_source_enabled("claude-code"));
    assert!(config.is_source_enabled("opencode"));
    assert!(config.is_source_enabled("codex"));
    assert!(
        config.is_source_enabled("gemini-cli"),
        "newly-added adapter should be enabled after migration"
    );
    assert!(
        config.is_source_enabled("kiro-cli"),
        "newly-added adapter should be enabled after migration"
    );

    let round_tripped = serde_json::to_string(&config).unwrap();
    assert!(
        !round_tripped.contains("enabled_sources"),
        "legacy field must not be re-serialized: {round_tripped}"
    );
}

#[test]
fn config_disables_persist_across_reloads() {
    let mut known = vec![
        ("claude-code".to_string(), "CC".to_string()),
        ("gemini-cli".to_string(), "GEM".to_string()),
    ];

    let mut config = AppConfig::default();
    config.normalize_sources(&known);
    config.disabled_sources.push("gemini-cli".to_string());

    let json = serde_json::to_string(&config).unwrap();
    let mut reloaded: AppConfig = serde_json::from_str(&json).unwrap();
    reloaded.normalize_sources(&known);

    assert!(reloaded.is_source_enabled("claude-code"));
    assert!(
        !reloaded.is_source_enabled("gemini-cli"),
        "explicit disable must survive a save/load cycle"
    );

    known.push(("kiro-cli".to_string(), "KIRO".to_string()));
    reloaded.normalize_sources(&known);
    assert!(
        reloaded.is_source_enabled("kiro-cli"),
        "a brand new adapter should default to enabled"
    );
    assert!(
        !reloaded.is_source_enabled("gemini-cli"),
        "previously disabled adapter must stay disabled"
    );
}

#[test]
fn config_drops_obsolete_disabled_entries() {
    let mut config = AppConfig::default();
    config.disabled_sources = vec!["ghost-adapter".to_string(), "claude-code".to_string()];
    let known = vec![("claude-code".to_string(), "CC".to_string())];
    config.normalize_sources(&known);

    assert!(!config.disabled_sources.iter().any(|id| id == "ghost-adapter"));
    assert!(config.is_source_enabled("claude-code"), "cleared to avoid zero-source state");
}
