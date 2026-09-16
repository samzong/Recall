use super::*;
use crate::adapters::test_support::{
    seed_empty_event_state, seed_empty_metadata_state, seed_empty_usage_state, store as setup_store,
};
use crate::types::Session;
use std::io::Write;

#[test]
fn source_formats_preserve_title_and_file_evidence_policies() {
    let root = tempfile::tempdir().unwrap();
    for format in [Format::Pi, Format::Omp] {
        let path = write_session(
            format,
            root.path(),
            session_id(format),
            &[
                serde_json::json!({"type": "title", "title": "slot title"}),
                serde_json::json!({"type": "session", "id": session_id(format), "cwd": "/session", "title": "header title"}),
                serde_json::json!({"type": "message", "message": {"role": "assistant", "timestamp": 2000, "content": [
                    {"type": "toolCall", "name": "write", "arguments": {"path": "written.rs"}},
                    {"type": "toolCall", "name": "bash", "arguments": {"command": "git restore -- restored.rs", "cwd": "/explicit"}}
                ]}}),
                serde_json::json!({"type": "message", "message": {"role": "bashExecution", "timestamp": 3000, "command": "git restore -- output.rs"}}),
            ],
        );
        let parsed = parse_session(&path, 0, true, format).unwrap();
        assert_eq!(parsed.custom_title.as_deref(), (format == Format::Omp).then_some("slot title"));
        assert_eq!(parsed.events.len(), 3);
        let write = &parsed.events[0];
        match format {
            Format::Pi => {
                assert_eq!(write.kind, "file_write");
                assert_eq!(write.files[0].path, "written.rs");
            }
            Format::Omp => {
                assert_eq!(write.kind, "tool_call");
                assert!(write.files.is_empty());
            }
        }
        assert_eq!(
            parsed.events[1].files[0].cwd.as_deref(),
            Some(if format == Format::Pi { "/session" } else { "/explicit" })
        );
        assert_eq!(
            parsed.events[2].files[0].cwd.as_deref(),
            (format == Format::Pi).then_some("/session")
        );
    }
}

fn temp_root(label: &str) -> PathBuf {
    let root =
        std::env::temp_dir().join(format!("recall-pi-session-{label}-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    root
}

fn session_id(format: Format) -> &'static str {
    match format {
        Format::Pi => "019e5af2-5528-7d10-888a-b299c21d0e2e",
        Format::Omp => "01a05dd7-7cbc-7005-818b-73de30e4dc42",
    }
}

fn file_timestamp(format: Format) -> &'static str {
    match format {
        Format::Pi => "2026-05-24T17-04-51-496Z",
        Format::Omp => "2026-09-01T16-39-58-396Z",
    }
}

fn write_session(format: Format, dir: &Path, session_id: &str, lines: &[Value]) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join(format!("{}_{session_id}.jsonl", file_timestamp(format)));
    let mut file = fs::File::create(&path).unwrap();
    for line in lines {
        writeln!(file, "{line}").unwrap();
    }
    path
}

fn parse_file(
    format: Format,
    entry: FileScanEntry,
    mtime_ms: i64,
    include_events: bool,
) -> anyhow::Result<Option<RawSession>> {
    parse_session_file(entry, mtime_ms, include_events, format)
}

fn scan_for_sync_impl(
    format: Format,
    session_dirs: &[PathBuf],
    context: &AdapterSyncContext,
    since_ts: Option<i64>,
    include_events: bool,
) -> anyhow::Result<SyncScanResult> {
    scan_for_sync(session_dirs, context, since_ts, include_events, format)
}

fn make_existing_session(
    format: Format,
    source_id: &str,
    updated_at: i64,
    message_count: u32,
) -> Session {
    Session {
        source: format.source().to_string(),
        source_id: source_id.to_string(),
        title: "existing".to_string(),
        directory: Some(format!("/tmp/{}-project", format.source())),
        started_at: 1_000,
        updated_at: Some(updated_at),
        message_count,
        ..crate::types::test_support::session(&format!("internal-{source_id}"))
    }
}

pub(crate) fn usage_normalizes_reasoning_included_in_output(_format: Format) {
    let entry = serde_json::json!({"id": "assistant1"});
    let message = serde_json::json!({
        "provider": "openrouter",
        "model": "deepseek",
        "usage": {
            "input": 10,
            "output": 7,
            "cacheRead": 2,
            "cacheWrite": 1,
            "reasoningTokens": 4,
            "totalTokens": 20
        }
    });

    let event = extract_usage_event(
        &entry,
        &message,
        1,
        3_000,
        Some(1),
        (None, None),
        "/tmp/session.jsonl",
    )
    .unwrap();

    assert_eq!(event.output_tokens, 3);
    assert_eq!(event.reasoning_tokens, 4);
}

pub(crate) fn skips_hidden_bash_execution(format: Format) {
    let root = temp_root("hidden-bash");
    let session_dir = root.join(format!("--tmp-{}-project--", format.source()));
    let session_id = session_id(format);
    let path = write_session(
        format,
        &session_dir,
        session_id,
        &[
            serde_json::json!({
                "type": "session", "version": 3, "id": session_id,
                "timestamp": "1970-01-01T00:00:01.000Z", "cwd": format!("/tmp/{}-project", format.source())
            }),
            serde_json::json!({
                "type": "message", "id": "bash1", "parentId": "user1",
                "timestamp": "1970-01-01T00:00:03.000Z",
                "message": {
                    "role": "bashExecution",
                    "command": "cat secret.txt",
                    "output": "secret output",
                    "excludeFromContext": true,
                    "exitCode": 7,
                    "cancelled": true,
                    "timestamp": 3000
                }
            }),
        ],
    );
    let mtime = file_scan::stat_mtime_ms(&path).unwrap();
    let raw = parse_file(
        format,
        FileScanEntry { session_id: session_id.to_string(), stat_target: path, directory: None },
        mtime,
        true,
    )
    .unwrap()
    .unwrap();

    assert!(raw.messages.is_empty());
    assert_eq!(raw.events.len(), 1);
    let event = &raw.events[0];
    assert_eq!(event.kind, "tool_result");
    assert_eq!(event.visibility, Some(EvidenceVisibility::Hidden));
    assert_eq!(event.status.as_deref(), Some("cancelled"));
    assert_eq!(event.target.as_deref(), Some("cat secret.txt"));
    assert!(event.tool_call_id.is_none());
    assert!(event.files.is_empty());
    let payload: Value = serde_json::from_str(event.attrs_json.as_deref().unwrap()).unwrap();
    assert_eq!(payload.pointer("/message/exitCode"), Some(&Value::from(7)));

    let _ = fs::remove_dir_all(&root);
}

pub(crate) fn skips_fork_inherited_usage(format: Format) {
    let root = temp_root("fork-usage");
    let session_dir = root.join(format!("--tmp-{}-project--", format.source()));
    let session_id = session_id(format);
    let path = write_session(
        format,
        &session_dir,
        session_id,
        &[
            serde_json::json!({
                "type": "session", "version": 3, "id": session_id,
                "timestamp": "1970-01-01T00:00:03.000Z", "cwd": format!("/tmp/{}-project", format.source()),
                "parentSession": "/tmp/parent.jsonl"
            }),
            serde_json::json!({
                "type": "message", "id": "parent-assistant", "timestamp": "1970-01-01T00:00:02.000Z",
                "message": {"role": "assistant", "content": [{"type":"toolCall","id":"old-call","name":"read","arguments":{"path":"old.rs"}}], "usage": {"input": 10}, "timestamp": 2000}
            }),
            serde_json::json!({
                "type": "message", "id": "child-assistant", "timestamp": "1970-01-01T00:00:04.000Z",
                "message": {"role": "assistant", "content": [{"type":"toolCall","id":"new-call","name":"read","arguments":{"path":"new.rs"}}], "usage": {"input": 5}, "timestamp": 4000}
            }),
        ],
    );

    let parsed = parse_file(
        format,
        FileScanEntry {
            session_id: session_id.to_string(),
            stat_target: path.clone(),
            directory: None,
        },
        0,
        true,
    )
    .unwrap()
    .unwrap();

    assert!(parsed.messages.is_empty());
    assert_eq!(parsed.events.len(), 1);
    assert_eq!(parsed.events[0].tool_call_id.as_deref(), Some("new-call"));
    assert_eq!(parsed.events[0].files[0].path, "new.rs");
    assert_eq!(parsed.usage_events.len(), 1);
    assert_eq!(parsed.usage_events[0].event_key, "message:child-assistant");
    assert_eq!(parsed.usage_events[0].input_tokens, 5);

    let _ = fs::remove_dir_all(&root);
}

pub(crate) fn scan_for_sync_skips_unchanged_session_when_usage_state_is_current(format: Format) {
    let root = temp_root("skip");
    let session_dir = root.join(format!("--tmp-{}-project--", format.source()));
    let session_id = session_id(format);
    let path = write_session(
        format,
        &session_dir,
        session_id,
        &[
            serde_json::json!({
                "type": "session",
                "version": 3,
                "id": session_id,
                "timestamp": "1970-01-01T00:00:01.000Z",
                "cwd": format!("/tmp/{}-project", format.source())
            }),
            serde_json::json!({
                "type": "message",
                "id": "user1",
                "parentId": null,
                "timestamp": "1970-01-01T00:00:02.000Z",
                "message": {
                    "role": "user",
                    "content": format!("hello {}", format.source()),
                    "timestamp": 2000
                }
            }),
        ],
    );
    let mtime = file_scan::stat_mtime_ms(&path).unwrap();
    let store = setup_store();
    store.insert_session(&make_existing_session(format, session_id, mtime, 1)).unwrap();
    seed_empty_usage_state(&store, format.source(), session_id, USAGE_PARSER_VERSION, Some(mtime));
    seed_empty_metadata_state(&store, format.source(), session_id, METADATA_PARSER_VERSION);

    let usage_only = scan_for_sync_impl(
        format,
        std::slice::from_ref(&session_dir),
        &AdapterSyncContext::from_store_for_test(&store, format.source()).unwrap(),
        None,
        false,
    )
    .unwrap();
    assert_eq!(usage_only.stats.skipped_sessions, 1);
    for previous_version in [None, Some(EVENT_PARSER_VERSION - 1)] {
        if let Some(version) = previous_version {
            seed_empty_event_state(&store, format.source(), session_id, version, Some(mtime));
        }
        let backfill = scan_for_sync_impl(
            format,
            std::slice::from_ref(&session_dir),
            &AdapterSyncContext::from_store_for_test(&store, format.source()).unwrap(),
            None,
            true,
        )
        .unwrap();
        assert_eq!(backfill.sessions.len(), 1);
        assert_eq!(backfill.sessions[0].event_parser_version, Some(EVENT_PARSER_VERSION));
    }
    seed_empty_event_state(&store, format.source(), session_id, EVENT_PARSER_VERSION, Some(mtime));
    let result = scan_for_sync_impl(
        format,
        &[root.join(format!("--tmp-{}-project--", format.source()))],
        &AdapterSyncContext::from_store_for_test(&store, format.source()).unwrap(),
        None,
        true,
    )
    .unwrap();
    assert_eq!(result.sessions.len(), 0);
    assert_eq!(result.stats.skipped_sessions, 1);

    let _ = fs::remove_dir_all(&root);
}

pub(crate) fn maps_parent_session_to_primary_fork(format: Format) {
    let root = temp_root("parent-session");
    let session_dir = root.join(format!("--tmp-{}-project--", format.source()));
    let session_id = session_id(format);
    let path = write_session(
        format,
        &session_dir,
        session_id,
        &[
            serde_json::json!({
                "type": "session",
                "version": 3,
                "id": session_id,
                "parentSession": format!("/home/x/.{}/agent/sessions/--proj--/{}_019e0000-0000-0000-0000-000000000001.jsonl", format.source(), file_timestamp(format)),
                "timestamp": "1970-01-01T00:00:01.000Z",
                "cwd": format!("/tmp/{}-project", format.source())
            }),
            serde_json::json!({
                "type": "message",
                "id": "user1",
                "timestamp": "1970-01-01T00:00:02.000Z",
                "message": {
                    "role": "user",
                    "content": [{"type": "text", "text": format!("hello {}", format.source())}],
                    "timestamp": 2000
                }
            }),
        ],
    );
    let mtime = file_scan::stat_mtime_ms(&path).unwrap();
    let entry =
        FileScanEntry { session_id: session_id.to_string(), stat_target: path, directory: None };

    let raw = parse_file(format, entry, mtime, true).unwrap().unwrap();

    assert_eq!(raw.thread_role, Some(ThreadRole::Primary));
    assert_eq!(
        raw.parent_links,
        vec![ParentLink {
            relation: ParentRelation::Fork,
            source: format.source().to_string(),
            source_id: "019e0000-0000-0000-0000-000000000001".to_string(),
        }]
    );
    assert_eq!(raw.metadata_parser_version, Some(METADATA_PARSER_VERSION));

    let _ = fs::remove_dir_all(&root);
}

pub(crate) fn drops_unresolvable_parent_session(format: Format) {
    let root = temp_root("parent-unresolvable");
    let session_dir = root.join(format!("--tmp-{}-project--", format.source()));
    let session_id = "019e5af2-5528-7d10-888a-b299c21d0e2f";
    let path = write_session(
        format,
        &session_dir,
        session_id,
        &[
            serde_json::json!({
                "type": "session",
                "version": 3,
                "id": session_id,
                "parentSession": "not-a-session-path",
                "timestamp": "1970-01-01T00:00:01.000Z",
                "cwd": format!("/tmp/{}-project", format.source())
            }),
            serde_json::json!({
                "type": "message",
                "id": "user1",
                "timestamp": "1970-01-01T00:00:02.000Z",
                "message": {
                    "role": "user",
                    "content": [{"type": "text", "text": format!("hello {}", format.source())}],
                    "timestamp": 2000
                }
            }),
        ],
    );
    let mtime = file_scan::stat_mtime_ms(&path).unwrap();
    let entry =
        FileScanEntry { session_id: session_id.to_string(), stat_target: path, directory: None };

    let raw = parse_file(format, entry, mtime, true).unwrap().unwrap();

    assert_eq!(raw.thread_role, Some(ThreadRole::Primary));
    assert!(raw.parent_links.is_empty(), "an unparseable parent must not leak a path");

    let _ = fs::remove_dir_all(&root);
}
