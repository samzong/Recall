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
use crate::adapters::minimax_code::parse_minimax_transcript;
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
