use std::fs;

use super::*;
use crate::db::schema;
use crate::db::store::SessionTopologyWrite;
use crate::types::{Message, ParentLink, Role};

fn store() -> Store {
    schema::register_sqlite_vec();
    Store::open_in_memory().unwrap()
}

fn make_session(id: &str, source: &str, source_id: &str, title: &str) -> Session {
    Session {
        id: id.to_string(),
        source: source.to_string(),
        source_id: source_id.to_string(),
        title: title.to_string(),
        started_at: 100,
        message_count: 1,
        ..crate::types::test_support::session(id)
    }
}

fn persist_session(
    store: &Store,
    session: &Session,
    events: &[SessionEventRecord],
    topology: &SessionTopologyWrite<'_>,
) {
    let msgs = vec![Message {
        session_id: session.id.clone(),
        role: Role::User,
        content: "hi".to_string(),
        timestamp: Some(100),
        seq: 0,
    }];
    store
        .persist_session_with_usage_and_events_with_topology(
            session,
            &msgs,
            &[],
            None,
            events,
            None,
            topology,
        )
        .unwrap();
}

#[test]
fn resolves_recursive_spawn_and_subagent_fork_from_any_node() {
    let store = store();
    let root = make_session("root-id", "codex", "root-src", "Root");
    let mut child = make_session("child-id", "codex", "child-src", "Child");
    child.started_at = 200;
    let mut grandchild = make_session("grand-id", "codex", "grand-src", "Grand");
    grandchild.started_at = 300;
    let mut normal_fork = make_session("fork-id", "codex", "fork-src", "Normal Fork");
    normal_fork.started_at = 400;

    persist_session(
        &store,
        &root,
        &[],
        &SessionTopologyWrite {
            thread_role: Some(ThreadRole::Primary),
            parents: &[],
            parser_version: Some(1),
        },
    );
    persist_session(
        &store,
        &child,
        &[],
        &SessionTopologyWrite {
            thread_role: Some(ThreadRole::Subagent),
            parents: &[ParentLink {
                relation: ParentRelation::Spawn,
                source: "codex".into(),
                source_id: "root-src".into(),
            }],
            parser_version: Some(1),
        },
    );
    persist_session(
        &store,
        &grandchild,
        &[],
        &SessionTopologyWrite {
            thread_role: Some(ThreadRole::Subagent),
            parents: &[ParentLink {
                relation: ParentRelation::Fork,
                source: "codex".into(),
                source_id: "child-src".into(),
            }],
            parser_version: Some(1),
        },
    );
    persist_session(
        &store,
        &normal_fork,
        &[],
        &SessionTopologyWrite {
            thread_role: Some(ThreadRole::Primary),
            parents: &[ParentLink {
                relation: ParentRelation::Fork,
                source: "codex".into(),
                source_id: "root-src".into(),
            }],
            parser_version: Some(1),
        },
    );

    let graph =
        RelatedSessionGraph::for_session_with_confer_path(&store, &grandchild, None).unwrap();
    assert_eq!(graph.root.id, "root-id");
    assert_eq!(graph.related.len(), 2);
    assert_eq!(graph.related[0].session.id, "child-id");
    assert_eq!(graph.related[0].parent_id, "root-id");
    assert_eq!(graph.related[0].depth, 1);
    assert_eq!(graph.related[0].relation, RelatedRelation::Spawn);
    assert_eq!(graph.related[1].session.id, "grand-id");
    assert_eq!(graph.related[1].parent_id, "child-id");
    assert_eq!(graph.related[1].depth, 2);
    assert_eq!(graph.related[1].relation, RelatedRelation::Fork);
}

#[test]
fn confer_resolver_prefers_event_bindings_and_falls_back_to_narrow_rooms_json() {
    let store = store();
    let room_id = "11111111-1111-1111-1111-111111111111";
    let host = make_session("host-id", "codex", "host-src", "Host");
    let seat_claude =
        make_session("claude-id", "claude-code", "claude-native-1", "Confer seat reviewer");
    let seat_gemini_like = make_session("rx-id", "rx-agent", "rx-native-2", "Untitled");
    let seat_conflict = make_session("conflict-id", "claude-code", "stale-rooms-id", "Stale");

    let mut ev1 = crate::types::test_support::session_event("tool_call");
    ev1.event_seq = 0;
    ev1.message_seq = Some(0);
    ev1.name = Some("mcp__confer__confer_ask".into());
    ev1.attrs_json = Some(format!(
        r#"{{"code":"await mcp__confer__confer_ask({{ room_id: \"{room_id}\" }})","resume_command":"claude --resume claude-native-1","seat":"reviewer"}}"#
    ));

    persist_session(
        &store,
        &host,
        &[ev1],
        &SessionTopologyWrite {
            thread_role: Some(ThreadRole::Primary),
            parents: &[],
            parser_version: Some(1),
        },
    );
    persist_session(&store, &seat_claude, &[], &SessionTopologyWrite::none());
    persist_session(&store, &seat_gemini_like, &[], &SessionTopologyWrite::none());
    persist_session(&store, &seat_conflict, &[], &SessionTopologyWrite::none());

    let dir = tempfile::tempdir().unwrap();
    let rooms_path = dir.path().join("rooms.json");
    fs::write(
        &rooms_path,
        format!(
            r#"{{
                "schema_version": 3,
                "rooms": [{{
                    "id": "{room_id}",
                    "name": "PR Review",
                    "workspace": "/secret/workspace/must-be-ignored",
                    "instructions": "secret instructions",
                    "seats": [
                        {{"name": "reviewer", "agent": "claude", "model": "opus", "native_session_id": "stale-rooms-id"}},
                        {{"name": "builder", "agent": "rx", "model": "rx-1", "native_session_id": "rx-native-2"}},
                        {{"name": "empty", "agent": "codex", "native_session_id": null}}
                    ]
                }}]
            }}"#
        ),
    )
    .unwrap();

    let graph = RelatedSessionGraph::for_session_with_confer_path(&store, &host, Some(&rooms_path))
        .unwrap();
    assert_eq!(graph.related.len(), 2);
    assert_eq!(graph.related[0].session.id, "claude-id");
    assert_eq!(graph.related[0].relation, RelatedRelation::Peer);
    assert_eq!(graph.related[0].via.as_deref(), Some("confer"));
    assert_eq!(graph.related[0].role_label.as_deref(), Some("reviewer"));
    assert_eq!(graph.related[0].display_title, "[reviewer] PR Review");
    assert_eq!(graph.related[0].spawn_anchor, Some(0));

    assert_eq!(graph.related[1].session.id, "rx-id");
    assert_eq!(graph.related[1].role_label.as_deref(), Some("builder"));
    assert_eq!(graph.related[1].display_title, "[builder] PR Review");
    assert_eq!(graph.related[1].spawn_anchor, Some(0));
}

#[test]
fn confer_resolver_skips_unknown_schema_and_malformed_json() {
    let store = store();
    let room_id = "22222222-2222-2222-2222-222222222222";
    let host = make_session("host-id", "codex", "host-src", "Host");
    let seat = make_session("seat-id", "codex", "seat-src", "Seat");

    let mut ev = crate::types::test_support::session_event("tool_call");
    ev.message_seq = Some(0);
    ev.attrs_json = Some(format!(r#"{{"room_id":"{room_id}"}}"#));
    persist_session(&store, &host, &[ev], &SessionTopologyWrite::none());
    persist_session(&store, &seat, &[], &SessionTopologyWrite::none());

    let dir = tempfile::tempdir().unwrap();
    let bad_schema = dir.path().join("bad-schema.json");
    fs::write(
        &bad_schema,
        format!(
            r#"{{"schema_version":99,"rooms":[{{"id":"{room_id}","seats":[{{"name":"s","agent":"codex","native_session_id":"seat-src"}}]}}]}}"#
        ),
    )
    .unwrap();
    let g1 = RelatedSessionGraph::for_session_with_confer_path(&store, &host, Some(&bad_schema))
        .unwrap();
    assert!(g1.related.is_empty());

    let malformed = dir.path().join("malformed.json");
    fs::write(&malformed, "{not-json").unwrap();
    let g2 =
        RelatedSessionGraph::for_session_with_confer_path(&store, &host, Some(&malformed)).unwrap();
    assert!(g2.related.is_empty());
}

#[test]
fn spawn_anchor_matches_subagent_meta_spawn_agent_and_invoke_subagent_without_guessing() {
    let store = store();
    let parent = make_session("p", "claude-code", "p-src", "Parent");
    let c1 = make_session("c1", "claude-code", "agent-a111", "Claude Sub");
    let mut c2 = make_session("c2", "codex", "codex-sub-2", "Codex Sub");
    c2.source_file_path = Some("/tmp/rollout-sub-2.jsonl".into());
    let c3 = make_session("c3", "antigravity-cli", "agy-sub-3", "Agy Sub");
    let c4 = make_session("c4", "codex", "unanchored-4", "Unanchored");

    let mut pe1 = crate::types::test_support::session_event("tool_call");
    pe1.event_seq = 0;
    pe1.message_seq = Some(1);
    pe1.tool_call_id = Some("call-claude-1".into());

    let mut pe2 = crate::types::test_support::session_event("tool_result");
    pe2.event_seq = 1;
    pe2.message_seq = Some(3);
    pe2.name = Some("spawn_agent".into());
    pe2.attrs_json = Some(r#"{"agent_path":"/tmp/rollout-sub-2.jsonl"}"#.into());

    let mut pe3 = crate::types::test_support::session_event("tool_call");
    pe3.event_seq = 2;
    pe3.message_seq = Some(5);
    pe3.name = Some("invoke_subagent".into());

    let mut pe4 = crate::types::test_support::session_event("native_record");
    pe4.event_seq = 3;
    pe4.message_seq = None;
    pe4.attrs_json = Some(r#"{"child_id":"agy-sub-3"}"#.into());

    persist_session(&store, &parent, &[pe1, pe2, pe3, pe4], &SessionTopologyWrite::none());

    let mut c1_ev = crate::types::test_support::session_event("subagent_meta");
    c1_ev.tool_call_id = Some("call-claude-1".into());

    let parent_events = store.list_session_events_for_session("p").unwrap();
    assert_eq!(resolve_spawn_anchors(&parent_events, &c1, &[c1_ev], None), Some(1));
    assert_eq!(resolve_spawn_anchors(&parent_events, &c2, &[], None), Some(3));
    assert_eq!(resolve_spawn_anchors(&parent_events, &c3, &[], None), Some(5));
    assert_eq!(resolve_spawn_anchors(&parent_events, &c4, &[], None), None);
}

#[test]
fn confer_multi_room_nested_seats_and_unpositioned_tool_result_anchors_resolve() {
    let store = store();
    let room1 = "33333333-3333-3333-3333-333333333333";
    let room2 = "44444444-4444-4444-4444-444444444444";
    let host = make_session("host", "codex", "host-src", "Host");
    let peer1 = make_session("peer1", "claude-code", "c-native-1", "Untitled");
    let peer2 = make_session("peer2", "codex", "codex-native-2", "Untitled");
    let sub_stripped = make_session("sub-s", "claude-code", "agent-xyz9", "Stripped Sub");

    let mut ev1 = crate::types::test_support::session_event("tool_call");
    ev1.event_seq = 0;
    ev1.message_seq = Some(2);
    ev1.tool_call_id = Some("call-r1".into());
    ev1.name = Some("mcp__confer__confer_ask".into());
    ev1.attrs_json = Some(format!(
        r#"{{"name":"mcp__confer__confer_ask","room_id":"{room1}","room_name":"Design Review","seats":[{{"name":"reviewer","resume_command":"/opt/homebrew/bin/claude -p --resume=c-native-1"}}]}}"#
    ));

    let mut ev2 = crate::types::test_support::session_event("tool_call");
    ev2.event_seq = 1;
    ev2.message_seq = Some(4);
    ev2.tool_call_id = Some("call-r2".into());
    ev2.attrs_json = Some(format!(r#"{{"room_id":"{room2}","room_name":"Impl Review"}}"#));

    let mut ev3 = crate::types::test_support::session_event("tool_result");
    ev3.event_seq = 2;
    ev3.message_seq = None;
    ev3.tool_call_id = Some("call-r2".into());
    ev3.attrs_json = Some(
        r#"{"seat":"builder","resume_command":"codex --model gpt-5 resume codex-native-2"}"#.into(),
    );

    let mut ev4 = crate::types::test_support::session_event("tool_call");
    ev4.event_seq = 3;
    ev4.message_seq = Some(6);
    ev4.tool_call_id = Some("call-sub".into());

    let mut ev5 = crate::types::test_support::session_event("tool_result");
    ev5.event_seq = 4;
    ev5.message_seq = None;
    ev5.tool_call_id = Some("call-sub".into());
    ev5.attrs_json = Some(r#"{"agentId":"xyz9"}"#.into());

    persist_session(&store, &host, &[ev1, ev2, ev3, ev4, ev5], &SessionTopologyWrite::none());
    persist_session(&store, &peer1, &[], &SessionTopologyWrite::none());
    persist_session(&store, &peer2, &[], &SessionTopologyWrite::none());
    persist_session(
        &store,
        &sub_stripped,
        &[],
        &SessionTopologyWrite {
            thread_role: Some(ThreadRole::Subagent),
            parents: &[ParentLink {
                relation: ParentRelation::Spawn,
                source: "codex".into(),
                source_id: "host-src".into(),
            }],
            parser_version: Some(1),
        },
    );

    let graph = RelatedSessionGraph::for_session_with_confer_path(&store, &host, None).unwrap();
    assert_eq!(graph.related.len(), 3);
    let p1 = graph.related.iter().find(|r| r.session.id == "peer1").unwrap();
    assert_eq!(p1.role_label.as_deref(), Some("reviewer"));
    assert_eq!(p1.display_title, "[reviewer] Design Review");
    assert_eq!(p1.spawn_anchor, Some(2));
    let p2 = graph.related.iter().find(|r| r.session.id == "peer2").unwrap();
    assert_eq!(p2.role_label.as_deref(), Some("builder"));
    assert_eq!(p2.spawn_anchor, Some(4));
    let sub = graph.related.iter().find(|r| r.session.id == "sub-s").unwrap();
    assert_eq!(sub.spawn_anchor, Some(6));
}
