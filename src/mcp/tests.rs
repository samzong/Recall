use super::*;
use crate::db::schema;
use crate::db::store::SessionTopologyWrite;
use crate::types::{ParentLink, ParentRelation, RawSessionEvent, Role, Session, ThreadRole};

fn setup() -> Store {
    schema::register_sqlite_vec();
    Store::open_in_memory().unwrap()
}

fn session(id: &str, source: &str, title: &str, started_at: i64) -> Session {
    Session {
        source: source.into(),
        source_id: format!("src-{id}"),
        title: title.into(),
        directory: Some("/tmp/demo".into()),
        started_at,
        updated_at: Some(started_at),
        message_count: 1,
        summary: Some(format!("{title} summary")),
        ..crate::types::test_support::session(id)
    }
}

fn message(session_id: &str, role: Role, content: &str, seq: u32) -> Message {
    Message {
        timestamp: Some(1_700_000_000_000),
        ..crate::types::test_support::message(session_id, role, content, seq)
    }
}

fn ready(store: Store) -> IndexState {
    IndexState::Ready(store)
}

fn context(source: &str, source_session_id: &str) -> CurrentSessionContext {
    CurrentSessionContext {
        host_identity: Some(SourceSessionIdentity {
            source: source.to_string(),
            source_session_id: source_session_id.to_string(),
        }),
    }
}

fn probe_result(
    candidates: &[(&str, &str)],
    complete: bool,
) -> adapters::invocation_probe::InvocationProbeResult {
    adapters::invocation_probe::InvocationProbeResult {
        candidates: candidates
            .iter()
            .map(|(source, source_id)| adapters::invocation_probe::InvocationProbeCandidate {
                source: (*source).to_string(),
                source_id: (*source_id).to_string(),
            })
            .collect(),
        complete,
    }
}

fn without_fields(value: impl Serialize, fields: &[&str]) -> Value {
    let mut value = to_json(value);
    let object = value.as_object_mut().unwrap();
    for field in fields {
        object.remove(*field);
    }
    value
}

#[test]
fn missing_index_is_a_tool_error() {
    let index = IndexState::Unavailable { path: None, message: MISSING_INDEX.to_string() };
    let args = SearchSessionsArgs { query: "error".into(), ..Default::default() };
    let list = search_sessions(&index, &args).expect_err("missing index");
    assert!(list.hits.is_empty());
    assert_eq!(list.message.as_deref(), Some(MISSING_INDEX));
    assert_eq!(json_result(search_sessions(&index, &args)).is_error, Some(true));
}

#[test]
fn empty_index_search_and_list_explain_the_gap() {
    let store = setup();
    let index = ready(store);
    let search_args = SearchSessionsArgs { query: "anything".into(), ..Default::default() };
    let search = search_sessions(&index, &search_args).unwrap();
    assert!(search.hits.is_empty());
    assert_eq!(search.message.as_deref(), Some(EMPTY_INDEX));
    assert_eq!(json_result(search_sessions(&index, &search_args)).is_error, Some(false));

    let list_args = ListRecentSessionsArgs { ..Default::default() };
    let listed = list_recent_sessions(&index, &list_args).unwrap();
    assert!(listed.hits.is_empty());
    assert_eq!(listed.message.as_deref(), Some(EMPTY_INDEX));
    assert_eq!(json_result(list_recent_sessions(&index, &list_args)).is_error, Some(false));

    let history = file_history(&index, &file_history_args("src/db/schema.rs")).unwrap();
    assert!(history.events.is_empty());
    assert_eq!(history.message.as_deref(), Some(EMPTY_INDEX));
    assert_eq!(
        json_result(file_history(&index, &file_history_args("src/db/schema.rs"))).is_error,
        Some(false)
    );
}

#[test]
fn search_reuses_hybrid_search_and_clamps_limit() {
    let store = setup();
    store.insert_session(&session("s1", "codex", "iterator panic", 2_000)).unwrap();
    store
        .insert_messages(&[message("s1", Role::User, "how do I use iterators in Rust", 0)])
        .unwrap();
    let index = ready(store);

    let list = search_sessions(
        &index,
        &SearchSessionsArgs { query: "iterators".into(), limit: Some(80), ..Default::default() },
    )
    .unwrap();
    assert_eq!(list.hits.len(), 1);
    assert_eq!(list.hits[0].session_id, "s1");
    assert_eq!(list.hits[0].source_session_id, "src-s1");
    assert_eq!(list.hits[0].source, "codex");
    assert_eq!(list.hits[0].project.as_deref(), Some("/tmp/demo"));
    assert_eq!(list.hits[0].title, "iterator panic");
    assert!(list.hits[0].excerpt.as_deref().unwrap().contains("iterators"));
    assert_eq!(list.hits[0].timestamp, iso8601(2_000));
    assert!(list.message.is_none());
    assert_eq!(
        without_fields(&list.hits[0], &["source_session_id"]),
        serde_json::json!({
            "session_id": "s1",
            "source": "codex",
            "project": "/tmp/demo",
            "title": "iterator panic",
            "excerpt": "how do I use iterators in Rust",
            "timestamp": iso8601(2_000),
        })
    );
}

#[test]
fn list_recent_is_newest_first_and_matches_search_shape() {
    let store = setup();
    store.insert_session(&session("old", "codex", "older", 1_000)).unwrap();
    store.insert_session(&session("new", "claude-code", "newer", 9_000)).unwrap();
    let index = ready(store);

    let list = list_recent_sessions(
        &index,
        &ListRecentSessionsArgs { limit: Some(10), ..Default::default() },
    )
    .unwrap();
    assert_eq!(list.hits.len(), 2);
    assert_eq!(list.hits[0].session_id, "new");
    assert_eq!(list.hits[0].source_session_id, "src-new");
    assert_eq!(list.hits[0].excerpt.as_deref(), Some("newer summary"));
    assert_eq!(list.hits[1].session_id, "old");
    assert_eq!(list.hits[1].source_session_id, "src-old");
}

#[test]
fn host_identity_requires_verified_source_native_values() {
    let codex_id = "019c9c4f-a462-7cc1-99a5-4ab521648c91";
    let codex = CurrentSessionContext::from_values(None, Some(codex_id), Some(codex_id));
    assert_eq!(
        codex.host_identity,
        Some(SourceSessionIdentity {
            source: "codex".to_string(),
            source_session_id: codex_id.to_string(),
        })
    );
    assert!(
        CurrentSessionContext::from_values(None, Some("thread"), Some("session"))
            .host_identity
            .is_none()
    );
    assert!(CurrentSessionContext::from_values(None, Some(codex_id), None).host_identity.is_none());
    assert!(CurrentSessionContext::from_values(None, Some(" "), Some(" ")).host_identity.is_none());
    assert!(
        CurrentSessionContext::from_values(None, Some("same"), Some("same"))
            .host_identity
            .is_none()
    );
    let claude_id = "604c4e71-f49c-4cc0-9388-88905fe65473";
    let claude = CurrentSessionContext::from_values(Some(claude_id), None, None);
    assert_eq!(
        claude.host_identity,
        Some(SourceSessionIdentity {
            source: "claude-code".to_string(),
            source_session_id: claude_id.to_string(),
        })
    );
    assert!(
        CurrentSessionContext::from_values(Some(claude_id), Some(codex_id), Some(codex_id))
            .host_identity
            .is_none()
    );
}

#[test]
fn resolved_current_session_is_excluded_before_search_and_recent_limits() {
    let store = setup();
    let mut current = session("000-current", "codex", "current", 100_000);
    current.message_count = 1;
    store.insert_session(&current).unwrap();
    store.insert_messages(&[message("000-current", Role::User, "identityneedle", 0)]).unwrap();
    for index in 0..51 {
        let id = format!("history-{index:02}");
        let mut stored = session(&id, "codex", "history", 50_000 - index);
        stored.message_count = 1;
        store.insert_session(&stored).unwrap();
        store.insert_messages(&[message(&id, Role::User, "identityneedle", 0)]).unwrap();
    }
    store
        .persist_topology_for_existing_session(
            "codex",
            "src-history-00",
            &SessionTopologyWrite {
                thread_role: Some(ThreadRole::Primary),
                parents: &[],
                parser_version: Some(1),
            },
        )
        .unwrap();
    let spawn = ParentLink {
        relation: ParentRelation::Spawn,
        source: "codex".to_string(),
        source_id: "src-history-00".to_string(),
    };
    store
        .persist_topology_for_existing_session(
            "codex",
            "src-history-01",
            &SessionTopologyWrite {
                thread_role: Some(ThreadRole::Subagent),
                parents: std::slice::from_ref(&spawn),
                parser_version: Some(1),
            },
        )
        .unwrap();
    let index = ready(store);
    let current_context = context("codex", "src-000-current");

    let first = search_sessions_with_context(
        &index,
        &SearchSessionsArgs {
            query: "identityneedle".into(),
            limit: Some(1),
            ..Default::default()
        },
        &current_context,
    )
    .unwrap();
    assert_eq!(first.hits.len(), 1);
    assert_ne!(first.hits[0].session_id, "000-current");
    assert_eq!(first.current_session.resolution, CurrentSessionResolution::Resolved);
    assert_eq!(first.current_session.session_id.as_deref(), Some("000-current"));

    let fifty = search_sessions_with_context(
        &index,
        &SearchSessionsArgs {
            query: "identityneedle".into(),
            limit: Some(50),
            ..Default::default()
        },
        &current_context,
    )
    .unwrap();
    assert_eq!(fifty.hits.len(), 50);
    assert!(fifty.hits.iter().all(|hit| hit.session_id != "000-current"));

    let recent = list_recent_sessions_with_context(
        &index,
        &ListRecentSessionsArgs { limit: Some(1), ..Default::default() },
        &current_context,
    )
    .unwrap();
    assert_eq!(recent.hits.len(), 1);
    assert_ne!(recent.hits[0].session_id, "000-current");
    assert_eq!(recent.current_session.resolution, CurrentSessionResolution::Resolved);

    let recent_fifty = list_recent_sessions_with_context(
        &index,
        &ListRecentSessionsArgs { limit: Some(50), ..Default::default() },
        &current_context,
    )
    .unwrap();
    assert_eq!(recent_fifty.hits.len(), 50);
    assert!(recent_fifty.hits.iter().all(|hit| hit.session_id != "000-current"));
}

#[test]
fn unresolved_or_mismatched_host_identity_keeps_discovery_complete() {
    let store = setup();
    let mut stored = session("current", "claude-code", "current", 10_000);
    stored.source_id = "shared-source-id".to_string();
    store.insert_session(&stored).unwrap();
    store.insert_messages(&[message("current", Role::User, "mismatchneedle", 0)]).unwrap();
    let index = ready(store);

    for current_context in [
        context("codex", "shared-source-id"),
        context("codex", "not-indexed"),
        CurrentSessionContext::from_values(None, Some("a"), Some("b")),
    ] {
        let result = search_sessions_with_context(
            &index,
            &SearchSessionsArgs {
                query: "mismatchneedle".into(),
                limit: Some(1),
                ..Default::default()
            },
            &current_context,
        )
        .unwrap();
        assert_eq!(result.current_session, CurrentSession::unknown());
        assert_eq!(result.hits[0].session_id, "current");
    }
}

#[test]
fn discovery_scope_does_not_change_exact_current_session_resolution() {
    let store = setup();
    let current = session("current", "codex", "current", 10_000);
    store.insert_session(&current).unwrap();
    let mut other = session("other", "claude-code", "other", 9_000);
    other.directory = Some("/tmp/other".to_string());
    store.insert_session(&other).unwrap();
    store.insert_messages(&[message("other", Role::User, "scopeneedle", 0)]).unwrap();
    let result = search_sessions_with_context(
        &ready(store),
        &SearchSessionsArgs {
            query: "scopeneedle".into(),
            project: Some("/tmp/other".into()),
            source: Some("claude-code".into()),
            limit: Some(1),
            invocation_nonce: None,
        },
        &context("codex", "src-current"),
    )
    .unwrap();
    assert_eq!(result.current_session.resolution, CurrentSessionResolution::Resolved);
    assert_eq!(result.hits[0].session_id, "other");
}

#[test]
fn exact_reads_remain_complete_for_the_resolved_current_session() {
    let store = setup();
    let mut current = session("current", "codex", "current", 10_000);
    current.message_count = 1;
    store.insert_session(&current).unwrap();
    store.insert_messages(&[message("current", Role::User, "exact transcript", 0)]).unwrap();
    persist_events(
        &store,
        "codex",
        "current",
        &[event(0, "file_write", Some("src/current.rs"), Some(10_000), None, None)],
    );
    let index = ready(store);

    let detail =
        get_session(&index, &GetSessionArgs { session_id: "current".into(), ..Default::default() })
            .unwrap();
    assert_eq!(detail.messages, "[user] exact transcript");
    let history = file_history(&index, &file_history_args("src/current.rs")).unwrap();
    assert_eq!(history.events.len(), 1);
    assert_eq!(history.events[0].session_id, "current");
}

#[test]
fn invocation_nonce_resolves_only_one_complete_indexed_candidate() {
    let store = setup();
    let mut current = session("current", "codex", "current", 10_000);
    current.source_id = "nonce-source".to_string();
    store.insert_session(&current).unwrap();
    let context = CurrentSessionContext::default();

    let resolved = context.resolve_with_probe(&store, Some("nonce"), |_| {
        probe_result(&[("codex", "nonce-source")], true)
    });
    assert_eq!(resolved.resolution, CurrentSessionResolution::Resolved);
    assert_eq!(resolved.session_id.as_deref(), Some("current"));

    for result in [
        probe_result(&[], true),
        probe_result(&[("codex", "nonce-source"), ("claude-code", "other")], true),
        probe_result(&[("codex", "nonce-source")], false),
        probe_result(&[("codex", "unindexed")], true),
        probe_result(&[("claude-code", "nonce-source")], true),
    ] {
        assert_eq!(
            context.resolve_with_probe(&store, Some("nonce"), |_| result),
            CurrentSession::unknown()
        );
    }
    assert_eq!(
        context.resolve_with_probe(&store, Some(" "), |_| { panic!("blank nonce must not probe") }),
        CurrentSession::unknown()
    );
}

#[test]
fn resolved_host_identity_skips_invocation_probe() {
    let store = setup();
    let mut current = session("current", "codex", "current", 10_000);
    current.source_id = "host-source".to_string();
    store.insert_session(&current).unwrap();

    let resolved =
        context("codex", "host-source").resolve_with_probe(&store, Some("nonce"), |_| {
            panic!("resolved host identity must skip probing")
        });
    assert_eq!(resolved.resolution, CurrentSessionResolution::Resolved);
    assert_eq!(resolved.session_id.as_deref(), Some("current"));
}

#[test]
fn unknown_source_is_empty_not_a_crash() {
    let store = setup();
    store.insert_session(&session("s1", "codex", "title", 1_000)).unwrap();
    store.insert_messages(&[message("s1", Role::User, "hello", 0)]).unwrap();
    let index = ready(store);
    let args = SearchSessionsArgs {
        query: "hello".into(),
        source: Some("not-a-source".into()),
        ..Default::default()
    };
    let list = search_sessions(&index, &args).expect_err("unknown source");
    assert!(list.hits.is_empty());
    assert!(list.message.unwrap().contains("unknown source"));
    assert_eq!(json_result(search_sessions(&index, &args)).is_error, Some(true));
}

#[test]
fn get_session_returns_plain_text_and_caps_long_messages() {
    let store = setup();
    let mut long = session("s1", "codex", "long", 1_000);
    long.message_count = 2;
    store.insert_session(&long).unwrap();
    let huge = "x".repeat(GET_MESSAGE_CHAR_CAP + 20);
    store
        .insert_messages(&[
            message("s1", Role::User, &huge, 0),
            message("s1", Role::Assistant, "done", 1),
        ])
        .unwrap();
    let index = ready(store);

    let detail = get_session(
        &index,
        &GetSessionArgs { session_id: "s1".into(), max_messages: Some(1), ..Default::default() },
    )
    .unwrap();
    assert_eq!(detail.session_id.as_deref(), Some("s1"));
    assert_eq!(detail.source_session_id.as_deref(), Some("src-s1"));
    assert_eq!(detail.returned_messages, 1);
    assert_eq!(detail.first_message_seq, Some(0));
    assert_eq!(detail.last_message_seq, Some(0));
    assert!(detail.truncated);
    assert!(detail.messages.starts_with("[user] "));
    assert!(detail.messages.ends_with('…'));
    assert!(!detail.messages.contains("[assistant]"));
    assert!(detail.messages.chars().count() <= "[user] ".chars().count() + GET_MESSAGE_CHAR_CAP);
}

#[test]
fn get_session_adds_provenance_without_changing_legacy_fields() {
    let store = setup();
    let mut stored = session("s1", "codex", "ordinary", 1_000);
    stored.message_count = 2;
    store.insert_session(&stored).unwrap();
    store
        .insert_messages(&[
            message("s1", Role::User, "question", 4),
            message("s1", Role::Assistant, "answer", 7),
        ])
        .unwrap();
    let detail = get_session(
        &ready(store),
        &GetSessionArgs { session_id: "s1".into(), ..Default::default() },
    )
    .unwrap();

    assert_eq!(detail.source_session_id.as_deref(), Some("src-s1"));
    assert_eq!(detail.first_message_seq, Some(4));
    assert_eq!(detail.last_message_seq, Some(7));
    assert_eq!(
        without_fields(&detail, &["source_session_id", "first_message_seq", "last_message_seq"]),
        serde_json::json!({
            "message": null,
            "session_id": "s1",
            "source": "codex",
            "project": "/tmp/demo",
            "title": "ordinary",
            "summary": "ordinary summary",
            "timestamp": iso8601(1_000),
            "message_count": 2,
            "returned_messages": 2,
            "truncated": false,
            "messages": "[user] question\n\n[assistant] answer",
        })
    );
}

#[test]
fn get_session_empty_messages_have_null_sequence_range() {
    let store = setup();
    let mut stored = session("s1", "codex", "empty", 1_000);
    stored.message_count = 0;
    store.insert_session(&stored).unwrap();
    let detail = get_session(
        &ready(store),
        &GetSessionArgs { session_id: "s1".into(), ..Default::default() },
    )
    .unwrap();

    assert_eq!(detail.source_session_id.as_deref(), Some("src-s1"));
    assert_eq!(detail.returned_messages, 0);
    assert_eq!(detail.first_message_seq, None);
    assert_eq!(detail.last_message_seq, None);
    assert!(!detail.truncated);
    assert!(detail.messages.is_empty());
}

#[test]
fn get_session_tail_returns_latest_messages_in_sequence_order() {
    let store = setup();
    let mut stored = session("s1", "codex", "unfinished", 1_000);
    stored.message_count = 4;
    store.insert_session(&stored).unwrap();
    store
        .insert_messages(&[
            message("s1", Role::User, "first", 0),
            message("s1", Role::Assistant, "second", 1),
            message("s1", Role::User, "third", 2),
            message("s1", Role::Assistant, "fourth", 3),
        ])
        .unwrap();
    let index = ready(store);

    let detail = get_session(
        &index,
        &GetSessionArgs {
            session_id: "s1".into(),
            max_messages: Some(2),
            tail: true,
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(detail.returned_messages, 2);
    assert_eq!(detail.first_message_seq, Some(2));
    assert_eq!(detail.last_message_seq, Some(3));
    assert!(detail.truncated);
    assert_eq!(detail.messages, "[user] third\n\n[assistant] fourth");
}

#[test]
fn get_session_tail_keeps_newest_messages_under_response_cap() {
    let store = setup();
    let mut stored = session("s1", "codex", "long tail", 1_000);
    stored.message_count = 20;
    store.insert_session(&stored).unwrap();
    let messages = (0..20)
        .map(|seq| {
            message("s1", Role::Assistant, &format!("message-{seq:02}-{}", "x".repeat(1_990)), seq)
        })
        .collect::<Vec<_>>();
    store.insert_messages(&messages).unwrap();
    let index = ready(store);

    let detail = get_session(
        &index,
        &GetSessionArgs {
            session_id: "s1".into(),
            max_messages: Some(20),
            tail: true,
            ..Default::default()
        },
    )
    .unwrap();

    assert!(detail.truncated);
    assert_eq!(detail.returned_messages, 15);
    assert_eq!(detail.first_message_seq, Some(5));
    assert_eq!(detail.last_message_seq, Some(19));
    assert!(!detail.messages.contains("message-04-"));
    assert!(detail.messages.contains("message-05-"));
    assert!(detail.messages.rsplit("\n\n").next().unwrap().starts_with("[assistant] message-19-"));
}

#[test]
fn get_session_without_events_preserves_json_bytes_and_skips_event_read() {
    let store = setup();
    store.insert_session(&session("s1", "codex", "title", 1_000)).unwrap();
    store.insert_messages(&[message("s1", Role::User, "hello", 0)]).unwrap();
    store.conn.execute_batch("DROP TABLE session_events").unwrap();
    let args = GetSessionArgs { session_id: "s1".into(), ..Default::default() };

    let index = ready(store);
    let detail = get_session(&index, &args).unwrap();
    let bytes = serde_json::to_vec(&detail).unwrap();

    assert_eq!(
        bytes,
        br#"{"message":null,"session_id":"s1","source_session_id":"src-s1","source":"codex","project":"/tmp/demo","title":"title","summary":"title summary","timestamp":"1970-01-01T00:00:01.000Z","message_count":1,"returned_messages":1,"first_message_seq":0,"last_message_seq":0,"truncated":false,"messages":"[user] hello"}"#
    );
    assert!(detail.events.is_none());
    assert!(detail.returned_events.is_none());
    assert!(detail.events_truncated.is_none());
    let event_error = get_session(&index, &GetSessionArgs { include_events: true, ..args })
        .expect_err("include_events must read the event table");
    assert!(event_error.contains("session_events"));
}

#[test]
fn get_session_events_follow_returned_message_range_and_preserve_null_anchors() {
    let store = setup();
    let mut stored = session("s1", "codex", "events", 1_000);
    stored.message_count = 4;
    store.insert_session(&stored).unwrap();
    store
        .insert_messages(&[
            message("s1", Role::User, "zero", 0),
            message("s1", Role::Assistant, "one", 1),
            message("s1", Role::User, "two", 2),
            message("s1", Role::Assistant, "three", 3),
        ])
        .unwrap();
    let mut events = (0..5)
        .map(|seq| event(seq, "tool_call", None, Some(1_700_000_000_000), None, None))
        .collect::<Vec<_>>();
    events[0].message_seq = Some(0);
    events[1].message_seq = None;
    events[2].message_seq = Some(2);
    events[2].source_event_id = Some("line:2".to_string());
    events[2].tool_call_id = Some("call-2".to_string());
    events[2].is_meta = Some(false);
    events[2].visibility = Some(EvidenceVisibility::Visible);
    events[3].message_seq = Some(3);
    events[4].message_seq = None;
    persist_events(&store, "codex", "s1", &events);
    let index = ready(store);

    let head = get_session(
        &index,
        &GetSessionArgs {
            session_id: "s1".into(),
            max_messages: Some(2),
            include_events: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        head.events.as_ref().unwrap().iter().map(|event| event.event_seq).collect::<Vec<_>>(),
        vec![0, 1, 4]
    );
    assert_eq!(head.returned_events, Some(3));
    assert_eq!(head.events_truncated, Some(true));

    let tail = get_session(
        &index,
        &GetSessionArgs {
            session_id: "s1".into(),
            max_messages: Some(2),
            tail: true,
            include_events: true,
            ..Default::default()
        },
    )
    .unwrap();
    let returned = tail.events.as_ref().unwrap();
    assert_eq!(returned.iter().map(|event| event.event_seq).collect::<Vec<_>>(), vec![2, 3, 1, 4]);
    assert_eq!(returned[0].timestamp.as_deref(), Some("2023-11-14T22:13:20.000Z"));
    assert_eq!(returned[0].source_event_id.as_deref(), Some("line:2"));
    assert_eq!(returned[0].tool_call_id.as_deref(), Some("call-2"));
    assert_eq!(returned[0].is_meta, Some(false));
    assert_eq!(returned[0].visibility, Some(EvidenceVisibility::Visible));
    assert_eq!(tail.returned_events, Some(4));
    assert_eq!(tail.events_truncated, Some(true));
    let value = serde_json::to_value(&tail).unwrap();
    let serialized = value.to_string();
    assert!(!serialized.contains("attrs_json"));
    assert!(!serialized.contains("source_path"));
    assert!(!serialized.contains("parser_version"));
    assert!(!serialized.contains("secret-value"));
}

#[test]
fn get_session_events_apply_head_and_tail_event_caps() {
    let store = setup();
    let mut stored = session("s1", "codex", "event cap", 1_000);
    stored.message_count = 0;
    store.insert_session(&stored).unwrap();
    let events = (0..55)
        .map(|seq| {
            let mut event = event(seq, "tool_call", None, None, None, None);
            event.message_seq = None;
            event
        })
        .collect::<Vec<_>>();
    persist_events(&store, "codex", "s1", &events);
    let index = ready(store);

    for (tail, first, last) in [(false, 0, 49), (true, 5, 54)] {
        let detail = get_session(
            &index,
            &GetSessionArgs {
                session_id: "s1".into(),
                tail,
                include_events: true,
                ..Default::default()
            },
        )
        .unwrap();
        let events = detail.events.unwrap();
        assert_eq!(events.len(), GET_EVENT_LIMIT);
        assert_eq!(events.first().unwrap().event_seq, first);
        assert_eq!(events.last().unwrap().event_seq, last);
        assert_eq!(detail.returned_events, Some(GET_EVENT_LIMIT));
        assert_eq!(detail.events_truncated, Some(true));
    }
}

#[test]
fn get_session_events_bound_unicode_fields_and_total_text() {
    let store = setup();
    let mut stored = session("s1", "codex", "event text cap", 1_000);
    stored.message_count = 0;
    store.insert_session(&stored).unwrap();
    let long = "汉".repeat(GET_EVENT_FIELD_CHAR_CAP + 50);
    let events = (0..GET_EVENT_LIMIT as u32)
        .map(|seq| {
            let mut event = event(seq, "tool_call", None, None, Some(&long), None);
            event.message_seq = None;
            event.target = Some(long.clone());
            event
        })
        .collect::<Vec<_>>();
    persist_events(&store, "codex", "s1", &events);

    let detail = get_session(
        &ready(store),
        &GetSessionArgs { session_id: "s1".into(), include_events: true, ..Default::default() },
    )
    .unwrap();
    let events = detail.events.unwrap();
    assert!(events.len() < GET_EVENT_LIMIT);
    assert_eq!(detail.returned_events, Some(events.len()));
    assert_eq!(detail.events_truncated, Some(true));
    assert!(events.iter().all(|event| {
        let summary = event.summary.as_deref().unwrap();
        summary.chars().count() == GET_EVENT_FIELD_CHAR_CAP && summary.ends_with('…')
    }));
    assert!(events.iter().map(session_event_text_chars).sum::<usize>() <= GET_EVENT_TEXT_CHAR_CAP);
}

#[test]
fn get_session_include_events_returns_explicit_empty_payload() {
    let store = setup();
    let mut stored = session("s1", "codex", "no events", 1_000);
    stored.message_count = 0;
    store.insert_session(&stored).unwrap();

    let detail = get_session(
        &ready(store),
        &GetSessionArgs { session_id: "s1".into(), include_events: true, ..Default::default() },
    )
    .unwrap();

    assert_eq!(detail.events, Some(Vec::new()));
    assert_eq!(detail.returned_events, Some(0));
    assert_eq!(detail.events_truncated, Some(false));
}

#[test]
fn get_session_missing_id_is_a_tool_error() {
    let store = setup();
    store.insert_session(&session("s1", "codex", "title", 1_000)).unwrap();
    let index = ready(store);
    let args = GetSessionArgs { session_id: "missing".into(), ..Default::default() };
    let error = get_session(&index, &args).expect_err("missing session");
    assert!(error.contains(SESSION_NOT_FOUND));
    assert_eq!(
        json_result(get_session(&index, &args).map_err(|message| empty_detail(Some(message))))
            .is_error,
        Some(true)
    );
}

#[test]
fn open_read_only_rejects_writes() {
    schema::register_sqlite_vec();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("recall.db");
    {
        let store = Store::open_at(&path).unwrap();
        store.insert_session(&session("s1", "codex", "title", 1_000)).unwrap();
    }
    let store = Store::open_read_only_at(&path).unwrap();
    let loaded = store.get_session_by_id("s1").unwrap().unwrap();
    assert_eq!(loaded.title, "title");
    assert!(store.insert_session(&session("s2", "codex", "nope", 2_000)).is_err());
}

#[test]
fn missing_db_path_does_not_create_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing").join("recall.db");
    let index = IndexState::open(Some(&path));
    match index {
        IndexState::Unavailable { message, path: kept } => {
            assert!(message.contains("not found"));
            assert_eq!(kept.as_deref(), Some(path.as_path()));
            assert!(!path.exists());
        }
        IndexState::Ready(_) => panic!("missing database should stay unavailable"),
    }
}

#[test]
fn missing_index_opens_after_the_file_appears() {
    schema::register_sqlite_vec();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("recall.db");
    let mut index = IndexState::open(Some(&path));
    match &index {
        IndexState::Unavailable { message, path: kept } => {
            assert!(message.contains("not found"));
            assert_eq!(kept.as_deref(), Some(path.as_path()));
        }
        IndexState::Ready(_) => panic!("missing database should stay unavailable"),
    }
    {
        let store = Store::open_at(&path).unwrap();
        store.insert_session(&session("s1", "codex", "later", 1_000)).unwrap();
        store.insert_messages(&[message("s1", Role::User, "hello later", 0)]).unwrap();
    }
    index.ensure_open();
    let listed =
        list_recent_sessions(&index, &ListRecentSessionsArgs { ..Default::default() }).unwrap();
    assert_eq!(listed.hits.len(), 1);
    assert_eq!(listed.hits[0].session_id, "s1");
    assert!(listed.message.is_none());
}

#[test]
fn list_recent_bounds_summary_excerpt() {
    let store = setup();
    let mut long = session("s1", "codex", "long", 1_000);
    long.summary = Some("y".repeat(EXCERPT_CHAR_CAP + 40));
    store.insert_session(&long).unwrap();
    let index = ready(store);
    let list =
        list_recent_sessions(&index, &ListRecentSessionsArgs { ..Default::default() }).unwrap();
    let excerpt = list.hits[0].excerpt.as_deref().unwrap();
    assert_eq!(excerpt.chars().count(), EXCERPT_CHAR_CAP);
    assert!(excerpt.ends_with('…'));
}

#[test]
fn get_session_flags_truncated_when_one_char_over_cap() {
    let store = setup();
    let mut long = session("s1", "codex", "edge", 1_000);
    long.message_count = 1;
    store.insert_session(&long).unwrap();
    let content = "x".repeat(GET_MESSAGE_CHAR_CAP + 1);
    store.insert_messages(&[message("s1", Role::User, &content, 0)]).unwrap();
    let index = ready(store);
    let detail =
        get_session(&index, &GetSessionArgs { session_id: "s1".into(), ..Default::default() })
            .unwrap();
    assert!(detail.truncated);
    assert_eq!(detail.first_message_seq, Some(0));
    assert_eq!(detail.last_message_seq, Some(0));
    assert!(detail.messages.ends_with('…'));
    assert_eq!(detail.messages.chars().count(), "[user] ".chars().count() + GET_MESSAGE_CHAR_CAP);
}

#[test]
fn limit_deserializer_accepts_string_numbers() {
    let args: SearchSessionsArgs =
        serde_json::from_value(serde_json::json!({"query": "q", "limit": "12"})).unwrap();
    assert_eq!(args.limit, Some(12));
    assert!(args.invocation_nonce.is_none());
    assert_eq!(clamp_limit(Some(80), 10, 50), 50);
    assert_eq!(clamp_limit(None, 10, 50), 10);
    assert_eq!(clamp_limit(Some(80), EVENT_LIMIT_DEFAULT, EVENT_LIMIT_MAX), 50);
    assert_eq!(clamp_limit(None, EVENT_LIMIT_DEFAULT, EVENT_LIMIT_MAX), 20);
    let get_args: GetSessionArgs =
        serde_json::from_value(serde_json::json!({"session_id": "s1"})).unwrap();
    assert!(!get_args.include_events);
}

#[test]
fn tools_advertise_read_only_closed_world() {
    for notes in [
        RecallMcp::search_sessions_tool_attr().annotations.unwrap(),
        RecallMcp::search_messages_tool_attr().annotations.unwrap(),
        RecallMcp::get_session_tool_attr().annotations.unwrap(),
        RecallMcp::list_recent_sessions_tool_attr().annotations.unwrap(),
        RecallMcp::file_history_tool_attr().annotations.unwrap(),
    ] {
        assert_eq!(notes.read_only_hint, Some(true));
        assert_eq!(notes.idempotent_hint, Some(true));
        assert_eq!(notes.open_world_hint, Some(false));
    }
}

#[test]
fn capabilities_report_uses_registered_tool_schemas() {
    let report = mcp_capabilities();
    let value = serde_json::to_value(&report).unwrap();
    let tools = value["tools"].as_array().unwrap();
    let get_session =
        tools.iter().find(|tool| tool["name"] == "get_session").expect("get_session capability");
    let search_sessions = tools
        .iter()
        .find(|tool| tool["name"] == "search_sessions")
        .expect("search_sessions capability");
    let list_recent_sessions = tools
        .iter()
        .find(|tool| tool["name"] == "list_recent_sessions")
        .expect("list_recent_sessions capability");

    assert!(get_session["inputSchema"]["properties"]["tail"].is_object());
    assert!(get_session["inputSchema"]["properties"]["include_events"].is_object());
    assert!(search_sessions["inputSchema"]["properties"]["invocation_nonce"].is_object());
    assert!(list_recent_sessions["inputSchema"]["properties"]["invocation_nonce"].is_object());
    assert!(get_session["description"].as_str().unwrap().contains("source_session_id"));
    assert!(get_session["description"].as_str().unwrap().contains("first_message_seq"));
    let description = get_session["description"].as_str().unwrap();
    assert!(description.contains("50 events"));
    assert!(description.contains("200 characters"));
    assert!(description.contains("10000 characters"));
    assert!(description.contains("never returns raw arguments"));
    assert!(report.server.capabilities.tools.is_some());
    let text = render_capabilities(&report);
    assert!(text.contains("get_session"));
    assert!(text.contains("Inputs: "));
    assert!(text.contains("tail"));
    assert!(text.contains("include_events"));
    assert!(text.contains("invocation_nonce"));
    assert!(text.contains("current_session.resolution"));
}

fn event(
    seq: u32,
    kind: &str,
    target: Option<&str>,
    timestamp: Option<i64>,
    summary: Option<&str>,
    name: Option<&str>,
) -> RawSessionEvent {
    RawSessionEvent {
        event_seq: seq,
        timestamp,
        name: name.map(str::to_string),
        target: target.map(str::to_string),
        message_seq: Some(1),
        summary: summary.map(str::to_string),
        attrs_json: Some(r#"{"token":"secret-value"}"#.to_string()),
        ..crate::types::test_support::session_event(kind)
    }
}

fn persist_events(store: &Store, source: &str, session_id: &str, events: &[RawSessionEvent]) {
    assert!(
        store
            .persist_session_events_for_existing_session(
                source,
                &format!("src-{session_id}"),
                events,
                1,
                None,
            )
            .unwrap()
    );
}

fn seed_agent_events() -> Store {
    let store = setup();
    store.insert_session(&session("codex-demo", "codex", "codex demo", 2_000)).unwrap();
    store.insert_session(&session("claude-demo", "claude-code", "claude demo", 3_000)).unwrap();
    let mut other = session("codex-other", "codex", "other project", 1_500);
    other.directory = Some("/tmp/other".to_string());
    store.insert_session(&other).unwrap();
    persist_events(
        &store,
        "codex",
        "codex-demo",
        &[
            event(
                0,
                "file_write",
                Some("/tmp/demo/src/db/schema.rs"),
                Some(5_000),
                Some("wrote schema"),
                Some("Edit"),
            ),
            event(
                1,
                "command",
                Some("/tmp/demo/src/db/schema.rs"),
                Some(6_000),
                Some("ran cargo test"),
                Some("Bash"),
            ),
            event(
                2,
                "file_write",
                Some("old_schema.rs"),
                Some(4_000),
                Some("renamed"),
                Some("Edit"),
            ),
            event(
                3,
                "file_read",
                Some(r"src\db\schema.rs"),
                Some(7_000),
                Some("win path"),
                Some("Read"),
            ),
        ],
    );
    persist_events(
        &store,
        "claude-code",
        "claude-demo",
        &[event(
            0,
            "file_read",
            Some("src/db/schema.rs"),
            Some(8_000),
            Some("read schema"),
            Some("Read"),
        )],
    );
    persist_events(
        &store,
        "codex",
        "codex-other",
        &[event(
            0,
            "file_write",
            Some("/tmp/other/src/db/schema.rs"),
            Some(9_000),
            Some("other schema"),
            Some("Edit"),
        )],
    );
    store
}

fn file_history_args(path: &str) -> FileHistoryArgs {
    FileHistoryArgs { path: path.to_string(), ..Default::default() }
}

#[test]
fn missing_index_is_a_tool_error_for_file_history() {
    let index = IndexState::Unavailable { path: None, message: MISSING_INDEX.to_string() };
    let history =
        file_history(&index, &file_history_args("src/db/schema.rs")).expect_err("missing index");
    assert!(history.events.is_empty());
    assert_eq!(history.message.as_deref(), Some(MISSING_INDEX));
    assert_eq!(
        json_result(file_history(&index, &file_history_args("src/db/schema.rs"))).is_error,
        Some(true)
    );
}

#[test]
fn file_history_defaults_to_file_kinds_and_suffix_match() {
    let store = seed_agent_events();
    store.conn.execute("UPDATE session_events SET visibility = 'hidden', is_meta = 1 WHERE session_id = 'codex-demo' AND kind = 'file_write'", []).unwrap();
    let index = ready(store);
    let list = file_history(&index, &file_history_args("src/db/schema.rs")).unwrap();
    assert!(list.message.is_none());
    assert!(
        list.events.iter().any(|event| event.visibility == Some(EvidenceVisibility::Hidden)
            && event.is_meta == Some(true))
    );
    let targets: Vec<_> = list.events.iter().map(|event| event.target.clone()).collect();
    assert!(targets.contains(&Some("/tmp/demo/src/db/schema.rs".into())));
    assert!(targets.contains(&Some("src/db/schema.rs".into())));
    assert!(targets.contains(&Some("/tmp/other/src/db/schema.rs".into())));
    assert!(!targets.contains(&Some("old_schema.rs".into())));
    assert!(list.events.iter().all(|event| event.kind != "command"));
    assert_eq!(list.events[0].timestamp.as_deref(), Some(iso8601(9_000).as_str()));

    let payload = to_json(&list);
    assert!(
        payload.to_string().contains("wrote schema") || payload.to_string().contains("read schema")
    );
    assert!(!payload.to_string().contains("secret-value"));
    assert!(
        payload
            .get("events")
            .and_then(|events| events.get(0))
            .and_then(|event| event.get("attrs_json"))
            .is_none()
    );

    let bare = file_history(&index, &file_history_args("schema.rs")).unwrap();
    assert!(bare.events.iter().any(|event| event.target.as_deref() == Some(r"src\db\schema.rs")));
    assert!(bare.events.iter().all(|event| {
        event.target.as_deref().is_some_and(|target| {
            target == "schema.rs"
                || target.ends_with("/schema.rs")
                || target.ends_with(r"\schema.rs")
        })
    }));
    assert!(bare.events.iter().all(|event| event.target.as_deref() != Some("old_schema.rs")));

    let absolute =
        file_history(&index, &file_history_args("/abs/elsewhere/src/db/schema.rs")).unwrap();
    assert!(
        absolute.events.iter().any(|event| event.target.as_deref() == Some("src/db/schema.rs"))
    );

    let commands = file_history(
        &index,
        &FileHistoryArgs {
            path: "src/db/schema.rs".into(),
            kind: Some("command".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(commands.events.len(), 1);
    assert_eq!(commands.events[0].kind, "command");
    assert_eq!(commands.events[0].session_id, "codex-demo");
}

#[test]
fn file_history_honors_project_source_and_unknown_source() {
    let index = ready(seed_agent_events());
    let scoped = file_history(
        &index,
        &FileHistoryArgs {
            path: "src/db/schema.rs".into(),
            project: Some("/tmp/demo".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(scoped.events.len(), 2);
    assert!(scoped.events.iter().all(|event| event.project.as_deref() == Some("/tmp/demo")));

    let by_source = file_history(
        &index,
        &FileHistoryArgs {
            path: "src/db/schema.rs".into(),
            project: Some("/tmp/demo".into()),
            source: Some("claude-code".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(by_source.events.len(), 1);
    assert_eq!(by_source.events[0].source, "claude-code");

    let unknown = file_history(
        &index,
        &FileHistoryArgs {
            path: "src/db/schema.rs".into(),
            source: Some("not-a-source".into()),
            ..Default::default()
        },
    )
    .expect_err("unknown source");
    assert!(unknown.events.is_empty());
    assert!(unknown.message.unwrap().contains("unknown source"));
    assert_eq!(
        json_result(file_history(&index, &file_history_args("src/db/schema.rs"))).is_error,
        Some(false)
    );
}

#[test]
fn file_history_clamps_limit_and_rejects_blank_path() {
    let store = setup();
    store.insert_session(&session("s1", "codex", "many", 1_000)).unwrap();
    let events: Vec<_> = (0..51)
        .map(|seq| {
            event(
                seq,
                "file_read",
                Some("src/db/schema.rs"),
                Some(10_000 + i64::from(seq)),
                Some("read"),
                Some("Read"),
            )
        })
        .collect();
    persist_events(&store, "codex", "s1", &events);
    let index = ready(store);
    let list = file_history(
        &index,
        &FileHistoryArgs { path: "src/db/schema.rs".into(), limit: Some(80), ..Default::default() },
    )
    .unwrap();
    assert_eq!(list.events.len(), 50);

    let blank = file_history(&index, &file_history_args("   ")).expect_err("blank path");
    assert_eq!(blank.message.as_deref(), Some(PATH_REQUIRED));
    assert_eq!(json_result(file_history(&index, &file_history_args("   "))).is_error, Some(true));
}

#[test]
fn file_history_truncates_long_summaries() {
    let store = setup();
    store.insert_session(&session("s1", "codex", "long", 1_000)).unwrap();
    persist_events(
        &store,
        "codex",
        "s1",
        &[event(
            0,
            "tool_call",
            Some("src/db/schema.rs"),
            Some(1_000),
            Some(&"z".repeat(EXCERPT_CHAR_CAP + 20)),
            Some("Tool"),
        )],
    );
    let list = file_history(
        &ready(store),
        &FileHistoryArgs {
            path: "src/db/schema.rs".into(),
            kind: Some("tool_call".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let summary = list.events[0].summary.as_deref().unwrap();
    assert_eq!(summary.chars().count(), EXCERPT_CHAR_CAP);
    assert!(summary.ends_with('…'));
}

#[test]
fn message_search_opens_sparse_context_and_preserves_explicit_current_session_reads() {
    let store = setup();
    store.insert_session(&session("s1", "codex", "Evidence", 2000)).unwrap();
    store
        .insert_messages(&[
            message("s1", Role::User, "What fixed it?", 0),
            message("s1", Role::Assistant, "evidencekeyword fixed it", 83),
            message("s1", Role::User, "Confirmed", 100),
        ])
        .unwrap();
    let index = ready(store);
    let args: SearchMessagesArgs =
        serde_json::from_value(serde_json::json!({"query": "evidencekeyword"})).unwrap();
    let ctx = context("codex", "src-s1");
    let hidden = search_message_matches(&index, &args, &ctx).unwrap();
    assert!(hidden["matches"].as_array().unwrap().is_empty());
    let args = SearchMessagesArgs { session_id: Some("s1".into()), ..args };
    let hits = search_message_matches(&index, &args, &ctx).unwrap();
    assert_eq!(hits["matches"][0]["seq"], 83);
    assert_eq!(hits["current_session_excluded"], false);
    let detail = get_session(
        &index,
        &GetSessionArgs { session_id: "s1".into(), around_seq: Some(83), ..Default::default() },
    )
    .unwrap();
    assert_eq!((detail.first_message_seq, detail.last_message_seq), (Some(0), Some(100)));
    assert_eq!(detail.returned_messages, 3);
    assert!(detail.messages.contains("[83][assistant] evidencekeyword"));
    assert!(!detail.truncated);
    assert!(
        get_session(
            &index,
            &GetSessionArgs { session_id: "s1".into(), around_seq: Some(82), ..Default::default() }
        )
        .is_err()
    );
    assert!(
        get_session(
            &index,
            &GetSessionArgs {
                session_id: "s1".into(),
                around_seq: Some(83),
                tail: true,
                ..Default::default()
            }
        )
        .is_err()
    );
}

#[test]
fn get_session_cursor_reads_the_rest_of_a_long_message() {
    let store = setup();
    store.insert_session(&session("s1", "codex", "Long", 2000)).unwrap();
    let original = "\u{4f60}\u{597d}🦀end".repeat(500);
    store.insert_messages(&[message("s1", Role::Assistant, &original, 83)]).unwrap();
    let index = ready(store);
    let mut args = GetSessionArgs {
        session_id: "s1".into(),
        around_seq: Some(83),
        max_chars: Some(731),
        ..Default::default()
    };
    let mut text = String::new();
    loop {
        let page = get_session(&index, &args).unwrap();
        assert_eq!(page.first_message_byte_offset, Some(text.len()));
        text.push_str(page.messages.strip_prefix("[83][assistant] ").unwrap());
        let Some(cursor) = page.next_cursor else {
            assert!(!page.truncated);
            break;
        };
        assert!(page.truncated);
        args = GetSessionArgs {
            session_id: "s1".into(),
            cursor: Some(cursor),
            max_chars: Some(731),
            ..Default::default()
        };
    }
    assert_eq!(text, original);
}

#[test]
fn file_history_requires_explicit_target_mode_for_new_parameters() {
    let index = ready(seed_agent_events());
    let legacy = file_history(&index, &file_history_args("src/db/schema.rs")).unwrap();
    let json = serde_json::to_value(&legacy).unwrap();
    assert!(json.get("next_cursor").is_none());
    assert!(json.get("target_file").is_none());
    let mut args = file_history_args("src/db/schema.rs");
    args.include_command_candidates = Some(false);
    assert!(file_history(&index, &args).is_err());
    args.target_project = Some("https://github.com/example/project.git".to_string());
    let page = file_history(&index, &args).unwrap();
    assert_eq!(page.has_more, Some(false));
    assert_eq!(
        page.target_file.unwrap().repo_remote.as_deref(),
        Some("github.com/example/project")
    );
    assert!(page.coverage.is_some());
    args.project = Some("/tmp/demo".to_string());
    assert!(file_history(&index, &args).is_err());
}

#[test]
fn event_evidence_pages_native_payload_and_binds_related_revisions() {
    let store = setup();
    for id in ["s1", "s2"] {
        store.insert_session(&session(id, "cursor", "Evidence", 1000)).unwrap();
    }
    store
        .insert_messages(&[message("s1", Role::User, "Explain the requested change", 10)])
        .unwrap();
    let mut call = event(3, "tool_call", Some("file"), None, Some("summary"), Some("apply_patch"));
    call.message_seq = Some(10);
    call.tool_call_id = Some("shared-call-id".into());
    call.source_event_id = Some("call-record".into());
    call.source_path = Some("composer:src-s1".into());
    call.attrs_json = Some(serde_json::json!({"input":"汉\n\\\"".repeat(900)}).to_string());
    let mut result = event(4, "tool_result", None, None, Some("result"), None);
    result.tool_call_id = call.tool_call_id.clone();
    result.source_event_id = Some("r".repeat(3500));
    persist_events(&store, "cursor", "s1", &[call.clone(), result.clone()]);
    persist_events(&store, "cursor", "s2", &[result.clone()]);
    let reference = {
        let tx = store.conn.unchecked_transaction().unwrap();
        let id = tx
            .query_row(
                "SELECT id FROM session_events WHERE session_id='s1' AND event_seq=3",
                [],
                |row| row.get(0),
            )
            .unwrap();
        serde_json::to_string(&crate::db::event_store::event_reference(&tx, id).unwrap()).unwrap()
    };
    let index = ready(store);
    let mut args = GetSessionArgs {
        session_id: "s1".into(),
        event_ref: Some(reference.clone()),
        max_bytes: Some(2048),
        ..Default::default()
    };
    args.evidence_part = Some(crate::event_evidence::EvidencePart::Before);
    let missing = get_event_evidence(&index, &args).unwrap_err();
    assert!(missing.starts_with("content_reference_not_recorded"));
    assert!(missing.contains("payload.related_event_refs"));
    args.evidence_part = None;
    let mut body = String::new();
    let mut first_cursor = None;
    for _ in 0..200 {
        let page = get_event_evidence(&index, &args).unwrap();
        assert!(serde_json::to_vec(&page).unwrap().len() <= 2048);
        let page = serde_json::to_value(page).unwrap();
        assert_eq!(page["byte_offset"].as_u64().unwrap() as usize, body.len());
        body.push_str(page["data"].as_str().unwrap());
        args.cursor = page["next_cursor"].as_str().map(str::to_string);
        if first_cursor.is_none() {
            first_cursor = args.cursor.clone();
        }
        if args.cursor.is_none() {
            break;
        }
    }
    assert!(args.cursor.is_none());
    let document: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(document["event"]["attrs_json"], call.attrs_json.as_deref().unwrap());
    assert_eq!(document["related_event_refs"].as_array().unwrap().len(), 1);
    let related = GetSessionArgs {
        session_id: "s1".into(),
        event_ref: document["related_event_refs"][0].as_str().map(str::to_string),
        ..Default::default()
    };
    assert!(get_event_evidence(&index, &related).is_ok());
    let discussion: GetSessionArgs =
        serde_json::from_value(document["discussion"].clone()).unwrap();
    assert!(get_session(&index, &discussion).unwrap().messages.contains("requested change"));
    let IndexState::Ready(store) = &index else { panic!() };
    persist_events(store, "cursor", "s1", &[call.clone(), result.clone()]);
    assert!(get_event_evidence(&index, &args).unwrap_err().contains("stale"));
    result.attrs_json = Some("changed result".into());
    persist_events(store, "cursor", "s1", &[call.clone(), result.clone()]);
    args.cursor = first_cursor;
    assert!(get_event_evidence(&index, &args).unwrap_err().contains("stale"));
    args.cursor = None;
    let tx = store.conn.unchecked_transaction().unwrap();
    let id = tx
        .query_row(
            "SELECT id FROM session_events WHERE session_id = 's1' AND event_seq = 3",
            [],
            |row| row.get(0),
        )
        .unwrap();
    args.event_ref = Some(
        serde_json::to_string(&crate::db::event_store::event_reference(&tx, id).unwrap()).unwrap(),
    );
    drop(tx);
    assert!(get_event_evidence(&index, &args).is_ok());
    call.attrs_json = Some("changed call".into());
    persist_events(store, "cursor", "s1", &[call, result]);
    assert!(get_event_evidence(&index, &args).unwrap_err().contains("stale"));
}

#[test]
fn event_evidence_never_reads_imported_locators_and_rejects_oversized_reads() {
    let store = setup();
    let mut imported = session("s1", "cursor", "Imported evidence", 1000);
    imported.is_import = true;
    store.insert_session(&imported).unwrap();
    let mut record = event(0, "tool_result", None, None, None, None);
    record.source_path = Some("/private/untrusted/source.sqlite".into());
    record.attrs_json = Some("{\"afterContentId\":\"composer.content.opaque\"}".into());
    persist_events(&store, "cursor", "s1", &[record]);
    let reference = {
        let tx = store.conn.unchecked_transaction().unwrap();
        let id = tx.query_row("SELECT id FROM session_events", [], |row| row.get(0)).unwrap();
        serde_json::to_string(&crate::db::event_store::event_reference(&tx, id).unwrap()).unwrap()
    };
    let index = ready(store);
    let mut args = GetSessionArgs {
        session_id: "s1".into(),
        event_ref: Some(reference),
        evidence_part: Some(crate::event_evidence::EvidencePart::After),
        ..Default::default()
    };
    assert_eq!(get_event_evidence(&index, &args).unwrap_err(), "source_unverified");
    args.evidence_part = None;
    assert!(get_event_evidence(&index, &args).is_ok());
    let IndexState::Ready(store) = &index else { panic!() };
    store
        .conn
        .execute("UPDATE session_events SET attrs_json = CAST(zeroblob(67108865) AS TEXT)", [])
        .unwrap();
    assert_eq!(get_event_evidence(&index, &args).unwrap_err(), "evidence_budget_exceeded");
}
