use crate::types::FileEvidenceKind;
use std::io::Cursor;

use super::*;
use crate::adapters::test_support::{
    seed_empty_event_state, seed_empty_metadata_state, store as setup_store,
};
use crate::types::Session;

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

    store.insert_session(&make_existing_session(conversation_id, mtime, 1)).unwrap();

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
