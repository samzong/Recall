use crate::types::FileEvidenceKind;
use std::io::Cursor;

use super::*;
use crate::adapters::test_support::{
    seed_empty_event_state, seed_empty_metadata_state, seed_empty_usage_state, store as setup_store,
};
use crate::types::Session;
use rusqlite::params;

fn temp_antigravity_root(label: &str) -> PathBuf {
    let root =
        std::env::temp_dir().join(format!("recall-agy-test-{}-{}", label, uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    root
}

fn write_transcript(root: &Path, conversation_id: &str, text: &str) -> PathBuf {
    let transcript = TRANSCRIPT_RELATIVE_PATH
        .iter()
        .fold(root.join("brain").join(conversation_id), |acc, part| acc.join(part));
    fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    fs::write(&transcript, text).unwrap();
    transcript
}

fn make_existing_session(source_id: &str, updated_at: i64, message_count: u32) -> Session {
    Session {
        source: "antigravity-cli".to_string(),
        source_id: source_id.to_string(),
        title: "existing".to_string(),
        updated_at: Some(updated_at),
        message_count,
        ..crate::types::test_support::session(&format!("internal-{source_id}"))
    }
}

#[test]
fn parse_antigravity_transcript_extracts_user_and_assistant_text() {
    let jsonl = r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-05-20T06:03:19Z","content":"<USER_REQUEST>\nAnalyze this project\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nignored\n</ADDITIONAL_METADATA>"}
{"step_index":2,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-05-20T06:03:19Z","tool_calls":[{"name":"replace_file_content","args":{"TargetFile":"/work/project/src/auth.rs","TargetContent":"old","ReplacementContent":"new"}},{"name":"view_file","args":{"AbsolutePath":"/work/project/src/auth.rs"}}]}
{"step_index":3,"source":"MODEL","type":"GENERIC","status":"DONE","created_at":"2026-05-20T06:03:21Z","content":"tool output should not be indexed","truncated_fields":["content"]}
{"step_index":15,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-05-20T06:03:30Z","content":"This project is a local agent configuration hub."}
"#;
    let session = parse_antigravity_transcript_reader(
        Cursor::new(jsonl),
        "agy-session",
        Some("/tmp/transcript.jsonl"),
        Some("/work/project"),
        true,
        "antigravity-cli",
    )
    .unwrap()
    .unwrap();

    assert_eq!(session.source_id, "agy-session");
    assert_eq!(session.started_at, 1_779_256_999_000);
    assert_eq!(session.updated_at, Some(1_779_257_010_000));
    assert_eq!(session.events.len(), 3);
    assert_eq!(session.events[0].files[0].operation, FileOperation::Write);
    assert_eq!(session.events[0].files[0].cwd.as_deref(), Some("/work/project"));
    assert_eq!(session.events[1].files[0].operation, FileOperation::Read);
    assert_eq!(session.events[0].source_event_id.as_deref(), Some("line:1:part:0"));
    assert_eq!(session.events[0].message_seq, Some(0));
    assert!(
        session.events.iter().all(|event| event.status.is_none() && event.tool_call_id.is_none())
    );
    assert_eq!(session.events[2].kind, "native_record");
    assert!(session.events[2].files.is_empty());
    assert!(session.events[2].attrs_json.as_deref().unwrap().contains("truncated_fields"));
    let tool_only = jsonl.lines().nth(1).unwrap().replace("DONE", "RUNNING");
    let pending = parse_antigravity_transcript_reader(
        Cursor::new(&tool_only),
        "pending",
        None,
        None,
        true,
        "antigravity-cli",
    )
    .unwrap()
    .unwrap();
    assert!(pending.messages.is_empty());
    assert_eq!(pending.events.len(), 2);
    assert!(pending.events[0].message_seq.is_none());
    for cwd in [Some("/work/elsewhere"), None] {
        let command = serde_json::json!({"source":"MODEL","type":"PLANNER_RESPONSE","status":"RUNNING","tool_calls":[{"name":"run_command","args":{"CommandLine":"git restore -- src/auth.rs","Cwd":cwd}}]});
        let command = parse_antigravity_transcript_reader(
            Cursor::new(command.to_string()),
            "commands",
            None,
            Some("/not-command-cwd"),
            true,
            "antigravity-cli",
        )
        .unwrap()
        .unwrap();
        assert!(command.messages.is_empty());
        assert_eq!(command.events[0].kind, "command");
        assert_eq!(command.events[0].files[0].path, "src/auth.rs");
        assert_eq!(command.events[0].files[0].cwd.as_deref(), cwd);
        assert_eq!(command.events[0].files[0].kind, FileEvidenceKind::Command);
        assert_eq!(
            command.events[0].command_evidence_status,
            Some(if cwd.is_some() {
                crate::types::CommandEvidenceStatus::Complete
            } else {
                crate::types::CommandEvidenceStatus::Unsupported
            })
        );
    }
    assert!(
        parse_antigravity_transcript_reader(
            Cursor::new([0xff, b'\n']),
            "broken",
            None,
            None,
            true,
            "antigravity-cli",
        )
        .is_err()
    );
    assert!(
        parse_antigravity_transcript_reader(
            Cursor::new(tool_only),
            "pending",
            None,
            None,
            false,
            "antigravity-cli",
        )
        .unwrap()
        .is_none()
    );
    assert_eq!(session.messages.len(), 2);
    assert_eq!(session.messages[0].role, Role::User);
    assert_eq!(session.messages[0].content, "Analyze this project");
    assert_eq!(session.messages[1].role, Role::Assistant);
    assert_eq!(session.messages[1].content, "This project is a local agent configuration hub.");
}

#[test]
fn collect_antigravity_entries_joins_history_workspace() {
    let root = temp_antigravity_root("collect");
    let conversation_id = "52d82992-7695-4d38-8d02-9747eecba839";
    write_transcript(&root, conversation_id, "");
    fs::write(
        root.join("history.jsonl"),
        format!(
            r#"{{"display":"hi","workspace":"/tmp/project","conversationId":"{conversation_id}"}}"#
        ),
    )
    .unwrap();

    let entries = collect_antigravity_entries(&root, AntigravityAdapter::Cli).unwrap();

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].session_id, conversation_id);
    assert_eq!(entries[0].directory.as_deref(), Some("/tmp/project"));

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn scan_for_sync_skips_unchanged_session() {
    let root = temp_antigravity_root("skip");
    let conversation_id = "52d82992-7695-4d38-8d02-9747eecba839";
    let transcript = write_transcript(
        &root,
        conversation_id,
        r#"{"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-05-20T06:03:19Z","content":"hello"}"#,
    );
    let mtime = file_scan::stat_mtime_ms(&transcript).unwrap();
    let store = setup_store();

    let fresh = scan_for_sync_impl(
        &root,
        AntigravityAdapter::Cli,
        &AdapterSyncContext::from_store_for_test(&store, "antigravity-cli").unwrap(),
        None,
        false,
    )
    .unwrap();
    assert_eq!(fresh.sessions[0].source_file_path.as_deref(), transcript.to_str());
    assert_eq!(fresh.sessions[0].usage_parser_version, Some(usage::USAGE_PARSER_VERSION));

    store.insert_session(&make_existing_session(conversation_id, mtime, 1)).unwrap();
    seed_empty_usage_state(
        &store,
        "antigravity-cli",
        conversation_id,
        usage::USAGE_PARSER_VERSION,
        Some(mtime),
    );

    let result = scan_for_sync_impl(
        &root,
        AntigravityAdapter::Cli,
        &AdapterSyncContext::from_store_for_test(&store, "antigravity-cli").unwrap(),
        None,
        false,
    )
    .unwrap();

    assert_eq!(result.sessions.len(), 0);
    assert_eq!(result.stats.skipped_sessions, 1);
    for version in [None, Some(EVENT_PARSER_VERSION - 1), Some(EVENT_PARSER_VERSION)] {
        if let Some(version) = version {
            seed_empty_event_state(
                &store,
                "antigravity-cli",
                conversation_id,
                version,
                Some(mtime),
            );
            seed_empty_metadata_state(
                &store,
                "antigravity-cli",
                conversation_id,
                METADATA_PARSER_VERSION,
            );
        }
        let result = scan_for_sync_impl(
            &root,
            AntigravityAdapter::Cli,
            &AdapterSyncContext::from_store_for_test(&store, "antigravity-cli").unwrap(),
            None,
            true,
        )
        .unwrap();
        assert_eq!(result.sessions.len(), usize::from(version != Some(EVENT_PARSER_VERSION)));
        if let Some(session) = result.sessions.first() {
            assert_eq!(session.event_parser_version, Some(EVENT_PARSER_VERSION));
            assert_eq!(session.metadata_parser_version, Some(METADATA_PARSER_VERSION));
        }
    }

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn parse_antigravity_subagent_extracts_parent_turns_original_task_and_send_message() {
    let parent_id = "df54ba16-74fe-44ac-ba62-2de5e16cc222";
    let self_id = "f42cead9-0157-4e4e-bc0c-90c90a7ec22e";
    let child_id = "97d942ea-5b29-49bb-b235-8b294f8066c0";
    let huge_prior_attempt = "x".repeat(32_000);
    let initial_system = format!(
        "The following is a <SYSTEM_MESSAGE> not actually sent by the user.\n\n<SYSTEM_MESSAGE>\n[Message] timestamp=2026-11-27T09:30:41Z sender={parent_id} priority=MESSAGE_PRIORITY_HIGH content=<prior_attempt>\n{huge_prior_attempt}\n</prior_attempt>\n<original_task>\n<USER_REQUEST>\nVerify adapter topology\n</USER_REQUEST>\n</original_task>\n</SYSTEM_MESSAGE>"
    );
    let sys_noise = "<SYSTEM_MESSAGE>\n[Message] timestamp=2026-11-27T09:30:42Z sender=system priority=MESSAGE_PRIORITY_HIGH content=System ping\n</SYSTEM_MESSAGE>";
    let task_noise = format!(
        "<SYSTEM_MESSAGE>\n[Message] timestamp=2026-11-27T09:30:43Z sender={self_id}/task-12 priority=MESSAGE_PRIORITY_HIGH content=Command finished\n</SYSTEM_MESSAGE>"
    );
    let child_report = format!(
        "<SYSTEM_MESSAGE>\n[Message] timestamp=2026-11-27T09:30:44Z sender={child_id} priority=MESSAGE_PRIORITY_HIGH content=Child agent finished\n</SYSTEM_MESSAGE>"
    );
    let follow_up_parent = format!(
        "<SYSTEM_MESSAGE>\n[Message] timestamp=2026-11-27T09:31:00Z sender={parent_id} priority=MESSAGE_PRIORITY_HIGH content=Please also check codex\n</SYSTEM_MESSAGE>"
    );

    let jsonl = [
        serde_json::json!({
            "source": "SYSTEM",
            "type": "SYSTEM_MESSAGE",
            "status": "DONE",
            "created_at": "2026-11-27T09:30:41Z",
            "content": initial_system
        }),
        serde_json::json!({
            "source": "SYSTEM",
            "type": "SYSTEM_MESSAGE",
            "status": "DONE",
            "created_at": "2026-11-27T09:30:42Z",
            "content": sys_noise
        }),
        serde_json::json!({
            "source": "SYSTEM",
            "type": "SYSTEM_MESSAGE",
            "status": "DONE",
            "created_at": "2026-11-27T09:30:43Z",
            "content": task_noise
        }),
        serde_json::json!({
            "source": "SYSTEM",
            "type": "SYSTEM_MESSAGE",
            "status": "DONE",
            "created_at": "2026-11-27T09:30:44Z",
            "content": child_report
        }),
        serde_json::json!({
            "source": "MODEL",
            "type": "PLANNER_RESPONSE",
            "status": "DONE",
            "created_at": "2026-11-27T09:30:50Z",
            "content": "Checking adapter topology.",
            "tool_calls": [{
                "name": "send_message",
                "args": {
                    "Recipient": serde_json::to_string(parent_id).unwrap(),
                    "Message": serde_json::to_string("Initial report sent to parent.").unwrap()
                }
            }]
        }),
        serde_json::json!({
            "source": "SYSTEM",
            "type": "SYSTEM_MESSAGE",
            "status": "DONE",
            "created_at": "2026-11-27T09:31:00Z",
            "content": follow_up_parent
        }),
        serde_json::json!({
            "source": "MODEL",
            "type": "PLANNER_RESPONSE",
            "status": "DONE",
            "created_at": "2026-11-27T09:31:10Z",
            "content": "",
            "tool_calls": [{
                "name": "send_message",
                "args": {
                    "Recipient": serde_json::to_string(parent_id).unwrap(),
                    "Message": serde_json::to_string("Codex verified.").unwrap()
                }
            }, {
                "name": "send_message",
                "args": {
                    "Recipient": serde_json::to_string(child_id).unwrap(),
                    "Message": serde_json::to_string("Ignore message to child.").unwrap()
                }
            }]
        }),
        serde_json::json!({
            "source": "MODEL",
            "type": "PLANNER_RESPONSE",
            "status": "DONE",
            "created_at": "2026-11-27T09:31:15Z",
            "content": "Identical final text",
            "tool_calls": [{
                "name": "send_message",
                "args": {
                    "Recipient": serde_json::to_string(parent_id).unwrap(),
                    "Message": serde_json::to_string("Identical final text").unwrap()
                }
            }]
        }),
    ]
    .into_iter()
    .map(|v| v.to_string())
    .collect::<Vec<_>>()
    .join("\n");

    let session = parse_antigravity_transcript_reader(
        Cursor::new(&jsonl),
        self_id,
        None,
        None,
        true,
        "antigravity-cli",
    )
    .unwrap()
    .unwrap();

    assert_eq!(session.thread_role, Some(ThreadRole::Subagent));
    assert_eq!(
        session.parent_links,
        vec![ParentLink {
            relation: ParentRelation::Spawn,
            source: "antigravity-cli".to_string(),
            source_id: parent_id.to_string(),
        }]
    );
    assert_eq!(session.metadata_parser_version, Some(METADATA_PARSER_VERSION));
    assert!(session.refresh_session_on_metadata_backfill);
    assert_eq!(
        session.messages.iter().map(|m| (&m.role, m.content.as_str())).collect::<Vec<_>>(),
        vec![
            (&Role::User, "Verify adapter topology"),
            (&Role::Assistant, "Checking adapter topology."),
            (&Role::Assistant, "Initial report sent to parent."),
            (&Role::User, "Please also check codex"),
            (&Role::Assistant, "Codex verified."),
            (&Role::Assistant, "Identical final text"),
        ]
    );
    assert!(session.messages.iter().all(|m| !m.content.contains("prior_attempt")));
}

#[test]
fn parse_antigravity_primary_with_child_system_message_stays_primary() {
    let primary_id = "fc9f3fa4-e4be-4afb-a4c8-85b33d733278";
    let child_id = "a4fe0241-5fa0-4af7-923f-9e8ed87be661";
    let jsonl = [
        serde_json::json!({
            "source": "USER_EXPLICIT",
            "type": "USER_INPUT",
            "status": "DONE",
            "created_at": "2026-11-27T09:00:00Z",
            "content": "<USER_REQUEST>\nRun full review\n</USER_REQUEST>"
        }),
        serde_json::json!({
            "source": "SYSTEM",
            "type": "SYSTEM_MESSAGE",
            "status": "DONE",
            "created_at": "2026-11-27T09:05:00Z",
            "content": format!("<SYSTEM_MESSAGE>\n[Message] timestamp=2026-11-27T09:05:00Z sender={child_id} priority=MESSAGE_PRIORITY_HIGH content=Child report\n</SYSTEM_MESSAGE>")
        }),
        serde_json::json!({
            "source": "MODEL",
            "type": "PLANNER_RESPONSE",
            "status": "DONE",
            "created_at": "2026-11-27T09:06:00Z",
            "content": "Review complete."
        }),
    ]
    .into_iter()
    .map(|v| v.to_string())
    .collect::<Vec<_>>()
    .join("\n");

    let session = parse_antigravity_transcript_reader(
        Cursor::new(&jsonl),
        primary_id,
        None,
        None,
        true,
        "antigravity",
    )
    .unwrap()
    .unwrap();

    assert_eq!(session.thread_role, Some(ThreadRole::Primary));
    assert!(session.parent_links.is_empty());
    assert_eq!(
        session.messages.iter().map(|m| (&m.role, m.content.as_str())).collect::<Vec<_>>(),
        vec![(&Role::User, "Run full review"), (&Role::Assistant, "Review complete.")]
    );
}

#[test]
fn parse_antigravity_subagent_handles_nested_original_task_and_metadata_backfill() {
    let parent_id = "df54ba16-74fe-44ac-ba62-2de5e16cc222";
    let self_id = "a1b2c3d4-1111-4222-8333-444455556666";
    let nested_system = format!(
        "<SYSTEM_MESSAGE>\n[Message] timestamp=2026-11-27T10:00:00Z sender={parent_id} priority=MESSAGE_PRIORITY_HIGH content=<original_task>\nExtract <original_task>...</original_task> inner text\nVerify codex\n</original_task>\n<prior_attempt>\nShould be stripped\n</prior_attempt>\n</SYSTEM_MESSAGE>"
    );
    let jsonl = [
        serde_json::json!({
            "source": "SYSTEM",
            "type": "SYSTEM_MESSAGE",
            "status": "DONE",
            "created_at": "2026-11-27T10:00:00Z",
            "content": nested_system
        }),
        serde_json::json!({
            "source": "MODEL",
            "type": "PLANNER_RESPONSE",
            "status": "DONE",
            "created_at": "2026-11-27T10:01:00Z",
            "content": "",
            "tool_calls": [{
                "name": "send_message",
                "args": {
                    "Recipient": serde_json::to_string(parent_id).unwrap(),
                    "Message": serde_json::to_string("Done.").unwrap()
                }
            }]
        }),
    ]
    .into_iter()
    .map(|v| v.to_string())
    .collect::<Vec<_>>()
    .join("\n");

    let root = temp_antigravity_root("nested-backfill");
    let transcript = write_transcript(&root, self_id, &jsonl);
    let mtime = file_scan::stat_mtime_ms(&transcript).unwrap();
    let store = setup_store();
    store.insert_session(&make_existing_session(self_id, mtime, 0)).unwrap();
    seed_empty_event_state(
        &store,
        "antigravity-cli",
        self_id,
        EVENT_PARSER_VERSION - 1,
        Some(mtime),
    );

    let rescan = scan_for_sync_impl(
        &root,
        AntigravityAdapter::Cli,
        &AdapterSyncContext::from_store_for_test(&store, "antigravity-cli").unwrap(),
        None,
        true,
    )
    .unwrap();
    assert_eq!(rescan.sessions.len(), 1);
    let session = &rescan.sessions[0];
    assert_eq!(session.thread_role, Some(ThreadRole::Subagent));
    assert_eq!(
        session.parent_links,
        vec![ParentLink {
            relation: ParentRelation::Spawn,
            source: "antigravity-cli".to_string(),
            source_id: parent_id.to_string(),
        }]
    );
    assert!(session.refresh_session_on_metadata_backfill);
    assert_eq!(
        session.messages.iter().map(|m| (&m.role, m.content.as_str())).collect::<Vec<_>>(),
        vec![
            (&Role::User, "Extract <original_task>...</original_task> inner text\nVerify codex"),
            (&Role::Assistant, "Done."),
        ]
    );

    let _ = fs::remove_dir_all(&root);
}

fn enc_varint(field: u64, value: u64) -> Vec<u8> {
    let mut out = encode_varint(field << 3);
    out.extend(encode_varint(value));
    out
}

fn enc_len(field: u64, payload: &[u8]) -> Vec<u8> {
    let mut out = encode_varint((field << 3) | 2);
    out.extend(encode_varint(payload.len() as u64));
    out.extend_from_slice(payload);
    out
}

fn encode_varint(mut value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            break;
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn build_gen_row(
    model: Option<&str>,
    label: Option<&str>,
    provider_enum: u64,
    input: u64,
    total_output: u64,
    cache_write: u64,
    cache_read: u64,
    thinking: u64,
    response_output: u64,
    response_id: Option<&str>,
    kvs: &[(&str, &str)],
    inline_ts_sec: Option<u64>,
) -> Vec<u8> {
    let mut usage = Vec::new();
    usage.extend(enc_varint(1, 1427));
    usage.extend(enc_varint(2, input));
    usage.extend(enc_varint(3, total_output));
    if cache_write > 0 {
        usage.extend(enc_varint(4, cache_write));
    }
    usage.extend(enc_varint(5, cache_read));
    if provider_enum > 0 {
        usage.extend(enc_varint(6, provider_enum));
    }
    usage.extend(enc_varint(9, thinking));
    usage.extend(enc_varint(10, response_output));
    if let Some(id) = response_id {
        usage.extend(enc_len(11, id.as_bytes()));
    }

    let mut chat_model = Vec::new();
    chat_model.extend(enc_len(4, &usage));
    if let Some(secs) = inline_ts_sec {
        let mut ts = Vec::new();
        ts.extend(enc_varint(1, secs));
        ts.extend(enc_varint(2, 500_000_000));
        let gen9 = enc_len(4, &ts);
        chat_model.extend(enc_len(9, &gen9));
    }
    if let Some(model) = model {
        chat_model.extend(enc_len(19, model.as_bytes()));
    }
    for (k, v) in kvs {
        let mut kv = Vec::new();
        kv.extend(enc_len(1, k.as_bytes()));
        kv.extend(enc_len(2, v.as_bytes()));
        chat_model.extend(enc_len(20, &kv));
    }
    if let Some(label) = label {
        chat_model.extend(enc_len(21, label.as_bytes()));
    }
    enc_len(1, &chat_model)
}

fn build_step_meta(seconds: u64, response_id: Option<&str>) -> Vec<u8> {
    let mut ts = Vec::new();
    ts.extend(enc_varint(1, seconds));
    ts.extend(enc_varint(2, 0));
    let mut meta = Vec::new();
    meta.extend(enc_len(1, &ts));
    if let Some(id) = response_id {
        let chat_gen = enc_len(11, id.as_bytes());
        meta.extend(enc_len(9, &chat_gen));
    }
    meta
}

fn build_trajectory_meta_blob(uri: &str, created_sec: u64) -> Vec<u8> {
    let folder = enc_len(1, uri.as_bytes());
    let mut ts = Vec::new();
    ts.extend(enc_varint(1, created_sec));
    ts.extend(enc_varint(2, 0));
    let mut blob = Vec::new();
    blob.extend(enc_len(1, &folder));
    blob.extend(enc_len(2, &ts));
    blob
}

#[test]
fn parse_antigravity_db_extracts_disjoint_tokens_steps_and_acp_requests() {
    let dir = temp_antigravity_root("db-usage");
    let conv_dir = dir.join("conversations");
    fs::create_dir_all(&conv_dir).unwrap();
    let cid = "11111111-1111-4111-8111-111111111111";
    let db_path = conv_dir.join(format!("{cid}.db"));

    let conn = Connection::open(&db_path).unwrap();
    conn.execute_batch(
        "CREATE TABLE gen_metadata (idx integer, data blob, size integer);
         CREATE TABLE trajectory_metadata_blob (id text, data blob);
         CREATE TABLE steps (idx integer, step_type integer, metadata blob);",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO trajectory_metadata_blob (id, data) VALUES ('main', ?1)",
        params![build_trajectory_meta_blob("file:///work/lathe", 1_795_000_000)],
    )
    .unwrap();

    let row_claude = build_gen_row(
        Some("claude-opus-5-5-high"),
        Some("Claude Opus 5.5 (High)"),
        26,
        500,
        340,
        25,
        1_200,
        40,
        300,
        Some("resp-claude"),
        &[("cascade_id", cid), ("last_step_index", "0"), ("used_claude", "true")],
        None,
    );
    let row_acp = build_gen_row(
        Some("gemini-4-carbon"),
        Some("Gemini 4 Carbon (High)"),
        0,
        800,
        120,
        0,
        400,
        20,
        100,
        None,
        &[("request_id", "acp-req-1")],
        Some(1_795_000_200),
    );
    let row_zero = build_gen_row(Some(""), None, 24, 0, 0, 0, 0, 0, 0, None, &[], None);

    conn.execute(
        "INSERT INTO gen_metadata (idx, data, size) VALUES (0, ?1, 0)",
        params![row_claude],
    )
    .unwrap();
    conn.execute("INSERT INTO gen_metadata (idx, data, size) VALUES (1, ?1, 0)", params![row_acp])
        .unwrap();
    conn.execute("INSERT INTO gen_metadata (idx, data, size) VALUES (2, ?1, 0)", params![row_zero])
        .unwrap();
    conn.execute(
        "INSERT INTO steps (idx, step_type, metadata) VALUES (0, 14, ?1)",
        params![build_step_meta(1_794_999_999, None)],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO steps (idx, step_type, metadata) VALUES (1, 15, ?1)",
        params![build_step_meta(1_795_000_100, None)],
    )
    .unwrap();
    drop(conn);

    let live = HashSet::from([cid.to_string()]);
    let parsed = usage::parse_antigravity_db(&db_path, cid, &live, None).unwrap();

    assert_eq!(parsed.workspace_dir.as_deref(), Some("/work/lathe"));
    assert_eq!(parsed.created_at_ms, Some(1_795_000_000_000));
    assert_eq!(parsed.usage_events.len(), 2);

    let first = &parsed.usage_events[0];
    assert_eq!(first.event_key, "response:resp-claude");
    assert_eq!(first.provider, "anthropic");
    assert_eq!(first.model, "claude-opus-5-5-high");
    assert_eq!(first.input_tokens, 500);
    assert_eq!(first.output_tokens, 300);
    assert_eq!(first.reasoning_tokens, 40);
    assert_eq!(first.cache_read_tokens, 1_200);
    assert_eq!(first.cache_write_tokens, 25);
    assert_eq!(first.timestamp, 1_795_000_100_000);

    let second = &parsed.usage_events[1];
    assert_eq!(second.event_key, "request:acp-req-1");
    assert_eq!(second.provider, "google");
    assert_eq!(second.model, "gemini-4-carbon");
    assert_eq!(second.timestamp, 1_795_000_200_500);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn antigravity_branch_skips_live_parent_prefix_and_dedups_orphan_siblings() {
    let root = temp_antigravity_root("branch-dedup");
    let conv_dir = root.join("conversations");
    fs::create_dir_all(&conv_dir).unwrap();

    let parent_id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let child_id = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    let deleted_mid_id = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
    let sib_a_id = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";
    let sib_b_id = "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee";

    let write_session = |sid: &str, rows: Vec<Vec<u8>>| {
        write_transcript(
            &root,
            sid,
            r#"{"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-11-27T08:00:00Z","content":"hi"}"#,
        );
        let conn = Connection::open(conv_dir.join(format!("{sid}.db"))).unwrap();
        conn.execute_batch("CREATE TABLE gen_metadata (idx integer, data blob, size integer);")
            .unwrap();
        for (idx, row) in rows.into_iter().enumerate() {
            conn.execute(
                "INSERT INTO gen_metadata (idx, data, size) VALUES (?1, ?2, 0)",
                params![idx as i64, row],
            )
            .unwrap();
        }
    };

    let parent_row = build_gen_row(
        Some("gemini-4-carbon"),
        None,
        24,
        100,
        50,
        0,
        0,
        10,
        40,
        Some("resp-parent"),
        &[("cascade_id", parent_id)],
        Some(1_795_000_000),
    );
    let child_own_row = build_gen_row(
        Some("gemini-4-carbon"),
        None,
        24,
        200,
        60,
        0,
        0,
        10,
        50,
        Some("resp-child"),
        &[("cascade_id", child_id)],
        Some(1_795_000_010),
    );
    let orphan_row = build_gen_row(
        Some("gemini-4-carbon"),
        None,
        24,
        300,
        70,
        0,
        0,
        10,
        60,
        Some("resp-orphan"),
        &[("cascade_id", deleted_mid_id)],
        Some(1_795_000_020),
    );

    write_session(parent_id, vec![parent_row.clone()]);
    write_session(child_id, vec![parent_row, child_own_row]);
    write_session(sib_a_id, vec![orphan_row.clone()]);
    write_session(sib_b_id, vec![orphan_row]);

    let store = setup_store();
    let result = scan_for_sync_impl(
        &root,
        AntigravityAdapter::Desktop,
        &AdapterSyncContext::from_store_for_test(&store, "antigravity").unwrap(),
        None,
        false,
    )
    .unwrap();

    assert_eq!(result.sessions.len(), 4);
    let by_id: HashMap<_, _> = result.sessions.iter().map(|s| (s.source_id.as_str(), s)).collect();
    assert_eq!(by_id[parent_id].usage_events.len(), 1);
    assert_eq!(by_id[child_id].usage_events.len(), 1);
    assert_eq!(by_id[child_id].usage_events[0].event_key, "response:resp-child");
    assert_eq!(by_id[sib_a_id].usage_events.len(), 1);
    assert!(by_id[sib_a_id].usage_events[0].event_seq >= (1 << 30));
    assert_eq!(by_id[sib_b_id].usage_events.len(), 1);

    let persisted = store;
    for raw in result.sessions {
        let session = Session {
            id: format!("sess-{}", raw.source_id),
            source: "antigravity".to_string(),
            source_id: raw.source_id.clone(),
            title: "t".to_string(),
            started_at: raw.started_at,
            updated_at: raw.updated_at,
            message_count: raw.messages.len() as u32,
            ..crate::types::test_support::session(&format!("sess-{}", raw.source_id))
        };
        let msgs: Vec<_> = raw
            .messages
            .iter()
            .enumerate()
            .map(|(i, m)| crate::types::Message {
                session_id: session.id.clone(),
                role: m.role.clone(),
                content: m.content.clone(),
                timestamp: m.timestamp,
                seq: i as u32,
            })
            .collect();
        persisted
            .replace_session_with_usage_and_events_with_topology(
                "antigravity",
                &raw.source_id,
                &session,
                &msgs,
                &raw.usage_events,
                raw.usage_parser_version,
                &[],
                None,
                &crate::db::store::SessionTopologyWrite {
                    thread_role: None,
                    parents: &[],
                    parser_version: None,
                },
            )
            .unwrap();
    }

    let report = crate::usage::build_usage_report(
        &persisted,
        &crate::usage::UsageFilters {
            sources: Some(vec!["antigravity".to_string()]),
            time_range: crate::db::search::TimeRange::All,
        },
    )
    .unwrap();
    assert_eq!(report.summary.events, 3);
    assert_eq!(report.summary.tokens.input_tokens, 600);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn empty_wal_is_ignored_and_broken_db_returns_none_without_clobbering() {
    let root = temp_antigravity_root("wal-and-err");
    let conv_dir = root.join("conversations");
    fs::create_dir_all(&conv_dir).unwrap();
    let cid = "ffffffff-ffff-4fff-8fff-ffffffffffff";
    let transcript = write_transcript(
        &root,
        cid,
        r#"{"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-11-27T08:00:00Z","content":"hello"}"#,
    );
    let db_path = conv_dir.join(format!("{cid}.db"));
    {
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch("CREATE TABLE gen_metadata (idx integer, data blob, size integer);")
            .unwrap();
        conn.execute(
            "INSERT INTO gen_metadata (idx, data, size) VALUES (0, ?1, 0)",
            params![build_gen_row(
                Some("gemini-4-carbon"),
                None,
                24,
                42,
                15,
                0,
                0,
                5,
                10,
                Some("resp-wal"),
                &[("cascade_id", cid)],
                Some(1_795_000_000),
            )],
        )
        .unwrap();
    }
    let wal = wal_path(&db_path);
    fs::write(&wal, b"").unwrap();

    let scan_data = collect_antigravity_scan_data(&root).unwrap();
    let snap = antigravity_snapshot(&scan_data.entries[0], Some(&db_path)).unwrap();
    let mtime = snap.effective_mtime_ms();

    let store = setup_store();
    store.insert_session(&make_existing_session(cid, mtime, 1)).unwrap();
    seed_empty_usage_state(
        &store,
        "antigravity-cli",
        cid,
        usage::USAGE_PARSER_VERSION,
        Some(mtime),
    );

    let skipped = scan_for_sync_impl(
        &root,
        AntigravityAdapter::Cli,
        &AdapterSyncContext::from_store_for_test(&store, "antigravity-cli").unwrap(),
        None,
        false,
    )
    .unwrap();
    assert_eq!(skipped.stats.skipped_sessions, 1);

    fs::write(&db_path, b"not a valid sqlite database").unwrap();
    let broken = parse_antigravity_session_for_entry(
        scan_data.entries[0].clone(),
        Some(&db_path),
        &scan_data.live_cascade_ids,
        mtime,
        false,
        None,
        "antigravity-cli",
    )
    .unwrap();
    assert!(broken.is_none());
    let _ = transcript;
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn usage_only_shell_keeps_source_path_and_recovers_acp_meta_cwd() {
    let root = temp_antigravity_root("acp-shell");
    let conv_dir = root.join("conversations");
    fs::create_dir_all(&conv_dir).unwrap();
    let cid = "12345678-1234-4123-8123-1234567890ab";
    let transcript = write_transcript(
        &root,
        cid,
        r#"{"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-11-27T08:00:00Z","content":"hi"}"#,
    );
    fs::write(conv_dir.join(format!("{cid}.meta")), r#"{"cwd":"/work/lathe","mode_id":"yolo"}"#)
        .unwrap();
    let db_path = conv_dir.join(format!("{cid}.db"));
    {
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch("CREATE TABLE gen_metadata (idx integer, data blob, size integer);")
            .unwrap();
        conn.execute(
            "INSERT INTO gen_metadata (idx, data, size) VALUES (0, ?1, 0)",
            params![build_gen_row(
                Some("gemini-4-carbon"),
                None,
                0,
                150,
                40,
                0,
                0,
                10,
                30,
                None,
                &[("request_id", "acp-req-99")],
                Some(1_795_000_000),
            )],
        )
        .unwrap();
    }

    let snap_mtime = antigravity_snapshot(
        &collect_antigravity_scan_data(&root).unwrap().entries[0],
        Some(&db_path),
    )
    .unwrap()
    .effective_mtime_ms();

    let store = setup_store();
    let mut existing = make_existing_session(cid, snap_mtime, 1);
    existing.directory = None;
    existing.source_file_path = transcript.to_str().map(str::to_string);
    store.insert_session(&existing).unwrap();

    let context = AdapterSyncContext::from_store_for_test(&store, "antigravity-cli")
        .unwrap()
        .with_usage_only(true);
    let result = scan_for_sync_impl(&root, AntigravityAdapter::Cli, &context, None, false).unwrap();
    assert_eq!(result.sessions.len(), 1);
    let session = &result.sessions[0];
    assert!(session.messages.is_empty());
    assert_eq!(session.directory.as_deref(), Some("/work/lathe"));
    assert_eq!(session.source_file_path.as_deref(), transcript.to_str());
    assert_eq!(session.updated_at, Some(snap_mtime));
    assert_eq!(session.usage_events.len(), 1);
    assert_eq!(session.usage_events[0].event_key, "request:acp-req-99");
    assert_eq!(session.usage_events[0].output_tokens, 30);
    assert_eq!(session.usage_events[0].reasoning_tokens, 10);

    let _ = fs::remove_dir_all(&root);
}
