mod discovery;
mod evidence;
mod file_history;
mod read_session;

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

fn file_history_args(path: &str) -> FileHistoryArgs {
    FileHistoryArgs { path: path.to_string(), ..Default::default() }
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
    assert!(report.server.capabilities.tools.is_some());
    let text = render_capabilities(&report);
    assert!(text.contains("get_session"));
    assert!(text.contains("Inputs: "));
    assert!(text.contains("tail"));
    assert!(text.contains("include_events"));
    assert!(text.contains("invocation_nonce"));
    assert!(text.contains("current_session.resolution"));
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
fn mcp_deserializers_accept_dual_forms_preserve_schemas_and_block_corrupted_retries() {
    let caps = serde_json::to_value(mcp_capabilities()).unwrap();
    let tool = |name: &str| {
        caps["tools"].as_array().unwrap().iter().find(|t| t["name"] == name).unwrap().clone()
    };
    let gs_schema = &tool("get_session")["inputSchema"];
    assert_eq!(gs_schema["required"], serde_json::json!(["session_id"]));
    assert_eq!(gs_schema["properties"]["max_bytes"]["minimum"], 1024);
    assert_eq!(gs_schema["properties"]["max_bytes"]["maximum"], 65536);
    assert_eq!(
        gs_schema["properties"]["max_bytes"]["type"],
        serde_json::json!(["integer", "null"])
    );
    assert_eq!(gs_schema["properties"]["max_chars"]["minimum"], 1);
    assert_eq!(gs_schema["properties"]["max_chars"]["maximum"], 6000);
    assert_eq!(gs_schema["properties"]["tail"]["type"], "boolean");
    assert_eq!(gs_schema["properties"]["include_events"]["type"], "boolean");
    assert_eq!(gs_schema["properties"]["cursor"]["type"], serde_json::json!(["string", "null"]));
    assert_eq!(gs_schema["properties"]["event_ref"]["type"], serde_json::json!(["string", "null"]));
    let fh_schema = &tool("file_history")["inputSchema"];
    assert_eq!(fh_schema["required"], serde_json::json!(["path"]));
    assert_eq!(fh_schema["properties"]["limit"]["minimum"], 1);
    assert_eq!(fh_schema["properties"]["limit"]["maximum"], 50);
    assert_eq!(
        fh_schema["properties"]["include_command_candidates"]["type"],
        serde_json::json!(["boolean", "null"])
    );
    assert_eq!(fh_schema["properties"]["source"]["type"], serde_json::json!(["string", "null"]));
    let sm_schema = &tool("search_messages")["inputSchema"];
    assert_eq!(sm_schema["required"], serde_json::json!(["query"]));
    assert_eq!(sm_schema["properties"]["limit"]["maximum"], 50);
    assert_eq!(sm_schema["properties"]["source"]["type"], serde_json::json!(["string", "null"]));

    let store = setup();
    store.insert_session(&session("s1", "codex", "Dual", 1_000)).unwrap();
    store.insert_messages(&[message("s1", Role::User, "hello world", 0)]).unwrap();
    let ev = event(0, "file_write", Some("src/a.rs"), Some(1_000), Some("ok"), Some("Edit"));
    persist_events(&store, "codex", "s1", &[ev]);
    let ref_str = {
        let tx = store.conn.unchecked_transaction().unwrap();
        let id: i64 = tx.query_row("SELECT id FROM session_events", [], |r| r.get(0)).unwrap();
        serde_json::to_string(&crate::db::event_store::event_reference(&tx, id).unwrap()).unwrap()
    };
    let ref_obj: Value = serde_json::from_str(&ref_str).unwrap();
    let index = ready(store);

    let paged_first = get_session(
        &index,
        &serde_json::from_value::<GetSessionArgs>(serde_json::json!({
            "session_id": "s1",
            "from_seq": "0",
            "to_seq": "0",
            "max_messages": "5",
            "max_chars": "4",
            "tail": "false",
            "include_events": "true"
        }))
        .unwrap(),
    )
    .unwrap();
    let cursor_obj: Value =
        serde_json::from_str(paged_first.next_cursor.as_deref().unwrap()).unwrap();
    let paged_second: GetSessionArgs = serde_json::from_value(serde_json::json!({
        "session_id": "s1",
        "cursor": cursor_obj,
        "max_chars": "100",
        "around_seq": "",
        "before": null,
        "after": "  "
    }))
    .unwrap();
    assert_eq!(get_session(&index, &paged_second).unwrap().messages, "[0][user] o world");

    let ev_args: GetSessionArgs = serde_json::from_value(serde_json::json!({
        "session_id": "s1",
        "event_ref": ref_obj,
        "evidence_part": "payload",
        "max_bytes": "4096"
    }))
    .unwrap();
    assert!(get_event_evidence(&index, &ev_args).is_ok());

    for raw_source in [
        serde_json::json!(["codex", "claude-code"]),
        serde_json::json!("[\"codex\",\"claude-code\"]"),
        serde_json::json!("codex,claude-code"),
    ] {
        let s_args: SearchSessionsArgs = serde_json::from_value(
            serde_json::json!({"query": "hello", "source": raw_source.clone()}),
        )
        .unwrap();
        assert_eq!(s_args.source.as_deref(), Some("codex,claude-code"));
        let m_args: SearchMessagesArgs =
            serde_json::from_value(serde_json::json!({"query": "hello", "source": raw_source}))
                .unwrap();
        assert_eq!(m_args.search.source.as_deref(), Some("codex,claude-code"));
    }

    let fh_args: FileHistoryArgs = serde_json::from_value(serde_json::json!({
        "path": "src/a.rs",
        "limit": "15",
        "include_command_candidates": "true",
        "source": ["codex"]
    }))
    .unwrap();
    assert_eq!(fh_args.limit, Some(15));
    assert_eq!(fh_args.include_command_candidates, Some(true));
    assert_eq!(fh_args.source.as_deref(), Some("codex"));

    for bad_source in [
        serde_json::json!([]),
        serde_json::json!("[]"),
        serde_json::json!(["codex", true]),
        serde_json::json!(["codex", null]),
        serde_json::json!(["codex", {}]),
        serde_json::json!(true),
        serde_json::json!({"a": 1}),
    ] {
        let err = serde_json::from_value::<SearchMessagesArgs>(serde_json::json!({
            "query": "hello",
            "source": bad_source
        }))
        .unwrap_err()
        .to_string();
        assert!(err.contains("call_mcp_tool"), "missing call_mcp_tool in: {err}");
        assert!(
            err.contains(
                "Do NOT retry the same payload unchanged; pass a comma-separated string such as \"codex,claude-code\""
            ),
            "unexpected source error: {err}"
        );
    }

    for bad_opaque in [
        serde_json::json!(true),
        serde_json::json!([]),
        serde_json::json!(123),
        serde_json::json!("true"),
        serde_json::json!("false"),
        serde_json::json!("[]"),
        serde_json::json!({}),
    ] {
        let c_err = serde_json::from_value::<GetSessionArgs>(serde_json::json!({
            "session_id": "s1",
            "cursor": bad_opaque.clone()
        }))
        .unwrap_err()
        .to_string();
        assert!(c_err.contains("call_mcp_tool") && c_err.contains("Do NOT retry"));
        let r_err = serde_json::from_value::<GetSessionArgs>(serde_json::json!({
            "session_id": "s1",
            "event_ref": bad_opaque
        }))
        .unwrap_err()
        .to_string();
        assert!(r_err.contains("call_mcp_tool") && r_err.contains("Do NOT retry"));
    }

    for payload in [
        serde_json::json!({"session_id": "s1", "around_seq": -1}),
        serde_json::json!({"session_id": "s1", "around_seq": 1.5}),
        serde_json::json!({"session_id": "s1", "around_seq": true}),
        serde_json::json!({"session_id": "s1", "around_seq": []}),
        serde_json::json!({"session_id": "s1", "around_seq": {}}),
        serde_json::json!({"session_id": "s1", "tail": 1}),
        serde_json::json!({"session_id": "s1", "include_events": "yes"}),
        serde_json::json!({"session_id": true}),
        serde_json::json!({"session_id": null}),
        serde_json::json!({"session_id": "s1", "evidence_part": true}),
        serde_json::json!({"session_id": "s1", "evidence_part": "invalid"}),
    ] {
        let err =
            serde_json::from_value::<GetSessionArgs>(payload.clone()).unwrap_err().to_string();
        assert!(err.contains("Do NOT retry"), "missing anti-retry for {payload}: {err}");
    }
    for payload in
        [serde_json::json!({"query": []}), serde_json::json!({"query": "q", "limit": "-2"})]
    {
        let err = serde_json::from_value::<SearchSessionsArgs>(payload).unwrap_err().to_string();
        assert!(err.contains("Do NOT retry"));
    }
    for payload in [
        serde_json::json!({"path": false}),
        serde_json::json!({"path": "a.rs", "include_command_candidates": []}),
    ] {
        let err = serde_json::from_value::<FileHistoryArgs>(payload).unwrap_err().to_string();
        assert!(err.contains("Do NOT retry"));
    }
}
