mod config;
mod export;
mod parsers;
mod persistence;
mod search;

use crate::adapters::copilot::parse_copilot_events;
use crate::adapters::gemini::parse_gemini_session;
use crate::adapters::kimi_code::parse_kimi_session;
use crate::adapters::kiro::{
    parse_kiro_conversation, parse_kiro_v2_session, parse_kiro_v3_session,
};
use crate::config::AppConfig;
use crate::db::schema;
use crate::db::search::{RepoFilter, SearchEngine, SearchFilters, TimeRange};
use crate::db::store::Store;
use crate::export::{ExportIncludes, ExportOptions, write_jsonl};
use crate::project_scope::{ProjectScope, SessionScopeFields};
use crate::types::{Message, RawSessionEvent, RawUsageEvent, Role, Session};
use crate::usage::{UsageFilters, build_usage_report};

fn setup() -> Store {
    schema::register_sqlite_vec();
    Store::open_in_memory().unwrap()
}

fn make_session(id: &str, source: &str, source_id: &str, title: &str) -> Session {
    Session {
        source: source.to_string(),
        source_id: source_id.to_string(),
        title: title.to_string(),
        directory: Some("/tmp/test".to_string()),
        started_at: chrono::Utc::now().timestamp_millis(),
        message_count: 1,
        ..crate::types::test_support::session(id)
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
        message_seq: Some(1),
        timestamp,
        model: model.to_string(),
        provider: "test-provider".to_string(),
        input_tokens: 10,
        output_tokens: 5,
        cache_read_tokens: 3,
        cache_write_tokens: 2,
        reasoning_tokens: 1,
        source_path: Some("/tmp/source.jsonl".to_string()),
        raw_usage_json: Some(r#"{"input_tokens":10}"#.to_string()),
        ..crate::types::test_support::usage_event(key)
    }
}

fn make_session_event(kind: &str, name: Option<&str>, target: Option<&str>) -> RawSessionEvent {
    RawSessionEvent {
        timestamp: Some(1_800_000_001_000),
        name: name.map(String::from),
        target: target.map(String::from),
        message_seq: Some(1),
        summary: Some("event summary".to_string()),
        source_path: Some("/tmp/source.jsonl".to_string()),
        source_event_id: Some("42".to_string()),
        tool_call_id: Some("call-42".to_string()),
        is_meta: Some(false),
        visibility: Some(crate::types::EvidenceVisibility::Visible),
        attrs_json: Some(r#"{"path":"src/main.rs"}"#.to_string()),
        ..crate::types::test_support::session_event(kind)
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
    SearchFilters {
        sources: None,
        time_range: TimeRange::All,
        scope: ProjectScope::Global,
        thread_role: None,
        excluded_session_id: None,
    }
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
        bytes.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect();
    assert_eq!(original, roundtrip);
}
