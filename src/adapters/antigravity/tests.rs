use crate::types::FileEvidenceKind;
use std::io::Cursor;

use super::*;
use crate::adapters::test_support::{seed_empty_event_state, store as setup_store};
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
    let pending =
        parse_antigravity_transcript_reader(Cursor::new(&tool_only), "pending", None, None, true)
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
        parse_antigravity_transcript_reader(Cursor::new([0xff, b'\n']), "broken", None, None, true)
            .is_err()
    );
    assert!(
        parse_antigravity_transcript_reader(Cursor::new(tool_only), "pending", None, None, false)
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
        }
    }

    let _ = fs::remove_dir_all(&root);
}
