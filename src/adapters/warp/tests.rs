use super::*;
use rusqlite::params;

const TASK: &[u8] = include_bytes!("../../../tests/fixtures/warp/task.bin");
const PROBE: &[u8] = include_bytes!("../../../tests/fixtures/warp/probe.bin");
const SCHEMA: &str = include_str!("../../../tests/fixtures/warp/schema.sql");

fn database() -> (tempfile::TempDir, PathBuf, Connection) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("warp.sqlite");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(SCHEMA).unwrap();
    conn.execute(
        "INSERT INTO agent_conversations (conversation_id, conversation_data, last_modified_at)
         VALUES ('conversation-1', ?1, '2026-09-30 16:00:09.250')",
        [r#"{"conversation_usage_metadata":{"token_usage":[{"warp_tokens":9999}],"tool_usage_metadata":{"run_command_stats":{"count":100}}}}"#],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO agent_tasks (conversation_id, task_id, task, last_modified_at)
         VALUES ('conversation-1', 'task-root', ?1, '2026-09-30 16:00:09.250')",
        [TASK],
    )
    .unwrap();
    (dir, path, conn)
}

fn write_task(conn: &Connection, task: &proto::Task) {
    conn.execute(
        "UPDATE agent_tasks SET task = ?1 WHERE task_id = 'task-root'",
        [task.encode_to_vec()],
    )
    .unwrap();
}

#[test]
fn official_wire_fixture_yields_messages_tools_and_observed_usage() {
    let (_dir, path, conn) = database();
    let before = std::fs::read(&path).unwrap();
    let scan = scan_db(&path, None, true).unwrap();
    assert_eq!(scan.sessions.len(), 1);
    assert_eq!(scan.stats.candidates, 1);
    assert_eq!(scan.stats.parsed, 1);
    let raw = &scan.sessions[0];
    assert_eq!(raw.source_id, "conversation-1");
    assert_eq!(raw.custom_title.as_deref(), Some("Warp fixture task"));
    assert_eq!(raw.directory.as_deref(), Some("/tmp/recall-warp-project"));
    assert_eq!(raw.started_at, 1790784000125);
    assert_eq!(raw.updated_at, Some(1790784009250));
    assert_eq!(raw.source_file_path.as_deref(), path.to_str());
    assert_eq!(raw.messages.len(), 3);
    assert_eq!(raw.messages[0].role, Role::User);
    assert_eq!(raw.messages[1].role, Role::Assistant);
    assert_eq!(
        raw.messages[2].content,
        "Read the file, counted two lines, and updated output.txt."
    );
    assert_eq!(raw.messages[2].timestamp, Some(1790784008125));
    assert_eq!(raw.events.len(), 6);
    assert_eq!(raw.events[0].kind, "file_read");
    assert_eq!(raw.events[0].files[0].path, "input.txt");
    assert_eq!(raw.events[0].files[0].operation, FileOperation::Read);
    assert_eq!(raw.events[0].files[0].cwd.as_deref(), raw.directory.as_deref());
    assert_eq!(raw.events[0].tool_call_id.as_deref(), Some("read-1"));
    assert_eq!(raw.events[1].status.as_deref(), Some("success"));
    assert_eq!(raw.events[2].kind, "command");
    assert_eq!(raw.events[2].target.as_deref(), Some("wc -l input.txt"));
    assert_eq!(raw.events[3].status.as_deref(), Some("success"));
    assert_eq!(raw.events[3].attrs_json.as_deref(), Some(r#"{"exit_code":0}"#));
    assert_eq!(raw.events[4].kind, "file_write");
    assert_eq!(raw.events[4].files[0].path, "output.txt");
    assert_eq!(raw.events[4].files[0].operation, FileOperation::Write);
    assert_eq!(raw.events[5].status.as_deref(), Some("success"));
    assert_eq!(raw.usage_events.len(), 1);
    let usage = &raw.usage_events[0];
    assert_eq!(usage.model, "test-model");
    assert_eq!(usage.input_tokens, 11);
    assert_eq!(usage.output_tokens, 7);
    assert_eq!(usage.cache_read_tokens, 3);
    assert_eq!(usage.cache_write_tokens, 2);
    assert_eq!(usage.token_source, crate::types::TokenSource::Observed);
    assert_eq!(std::fs::read(path).unwrap(), before);
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM agent_tasks", [], |row| row.get::<_, i64>(0)).unwrap(),
        1
    );
}

#[test]
fn live_wal_snapshot_excludes_uncommitted_changes_and_sees_committed_changes() {
    let (_dir, path, conn) = database();
    conn.execute_batch("PRAGMA journal_mode=WAL; BEGIN IMMEDIATE;").unwrap();
    let mut task = proto::Task::decode(TASK).unwrap();
    task.messages[0].user_query.as_mut().unwrap().query = "Updated prompt".into();
    write_task(&conn, &task);
    let raw = scan_db(&path, None, true).unwrap().sessions.remove(0);
    assert!(raw.messages[0].content.starts_with("Read input.txt"));
    conn.execute_batch("COMMIT;").unwrap();
    let raw = scan_db(&path, None, true).unwrap().sessions.remove(0);
    assert_eq!(raw.messages[0].content, "Updated prompt");
}

#[test]
fn interleaved_subtasks_do_not_replace_the_root_tasks_tool_directory() {
    let (_dir, path, conn) = database();
    let child = proto::Task {
        id: "child-task".into(),
        dependencies: Some(proto::Dependencies { parent_task_id: "task-root".into() }),
        messages: vec![proto::Message {
            id: "child-prompt".into(),
            timestamp: Some(proto::Timestamp { seconds: 1790784001, nanos: 500000000 }),
            user_query: Some(proto::UserQuery {
                query: "Child task".into(),
                origin: None,
                context: Some(proto::InputContext {
                    directory: Some(proto::Directory { pwd: "/tmp/child-project".into() }),
                }),
            }),
            ..Default::default()
        }],
        ..Default::default()
    };
    conn.execute("INSERT INTO agent_tasks (conversation_id, task_id, task) VALUES ('conversation-1', 'child-task', ?1)", [child.encode_to_vec()]).unwrap();
    let raw = scan_db(&path, None, true).unwrap().sessions.remove(0);
    assert_eq!(raw.directory.as_deref(), Some("/tmp/recall-warp-project"));
    assert_eq!(raw.events[0].files[0].cwd.as_deref(), Some("/tmp/recall-warp-project"));
}

#[test]
fn corrupt_task_skips_its_conversation_without_losing_other_conversations() {
    let (_dir, path, conn) = database();
    conn.execute_batch("INSERT INTO agent_conversations (conversation_id, conversation_data) VALUES ('bad', '{}');
        INSERT INTO agent_tasks (conversation_id, task_id, task) VALUES ('bad', 'bad-task', x'ff');").unwrap();
    let scan = scan_db(&path, None, true).unwrap();
    assert_eq!(scan.stats.candidates, 2);
    assert_eq!(scan.sessions.len(), 1);
    assert_eq!(scan.sessions[0].source_id, "conversation-1");
}

#[test]
fn aggregate_stats_and_absent_token_counts_do_not_invent_usage_or_tool_calls() {
    let (_dir, path, conn) = database();
    let mut task = proto::Task::decode(TASK).unwrap();
    for message in &mut task.messages {
        message.request_metadata = None;
        message.tool_call = None;
        message.tool_call_result = None;
    }
    write_task(&conn, &task);
    let raw = scan_db(&path, None, true).unwrap().sessions.remove(0);
    assert_eq!(raw.messages.len(), 3);
    assert!(raw.events.is_empty());
    assert!(raw.usage_events.is_empty());
}

#[test]
fn missing_timestamps_preserve_message_order_and_duplicates_are_not_reindexed() {
    let (_dir, path, conn) = database();
    let mut task = proto::Task::decode(TASK).unwrap();
    task.messages[1].timestamp = None;
    let duplicate = task.messages[0].clone();
    task.messages.push(duplicate);
    write_task(&conn, &task);
    let raw = scan_db(&path, None, true).unwrap().sessions.remove(0);
    assert_eq!(raw.messages.len(), 3);
    assert_eq!(raw.messages[0].role, Role::User);
    assert_eq!(raw.messages[1].content, "I will inspect the file first.");
    assert_eq!(raw.messages[1].timestamp, None);
    assert_eq!(raw.messages[2].timestamp, Some(1790784008125));
}

#[test]
fn restricted_streaming_scan_and_usage_only_scan_obey_the_core_contract() {
    let (_dir, path, conn) = database();
    conn.execute_batch("INSERT INTO agent_conversations (conversation_id, conversation_data) VALUES ('other', '{}');").unwrap();
    let mut streamed = Vec::new();
    let mut sink = |raw: RawSession| {
        streamed.push(raw);
        Ok(())
    };
    let context = AdapterSyncContext::empty_for_test("warp")
        .restricted_to("conversation-1")
        .with_session_sink(&mut sink);
    let scan = scan_db(&path, Some(&context), false).unwrap();
    assert_eq!(scan.stats.candidates, 1);
    assert!(scan.sessions.is_empty());
    drop(context);
    assert_eq!(streamed.len(), 1);
    assert!(streamed[0].events.is_empty());
    assert_eq!(streamed[0].event_parser_version, None);
    assert_eq!(streamed[0].usage_events.len(), 1);
}

#[test]
fn taskless_conversations_do_not_emit_user_only_sessions() {
    let (_dir, path, conn) = database();
    conn.execute_batch("CREATE TABLE ai_queries (id INTEGER PRIMARY KEY, conversation_id TEXT, input TEXT, working_directory TEXT, start_ts TEXT);
        INSERT INTO agent_conversations (conversation_id, conversation_data) VALUES ('legacy', '{}');").unwrap();
    let input = r#"[{"Context":{"text":"hidden context"}},{"Query":{"text":"Legacy prompt"}}]"#;
    for id in ["legacy", "conversation-1"] {
        conn.execute("INSERT INTO ai_queries (conversation_id, input, working_directory, start_ts) VALUES (?1, ?2, '/tmp/legacy', '2026-09-30 15:00:00')", params![id, input]).unwrap();
    }
    let scan = scan_db(&path, None, true).unwrap();
    assert_eq!(scan.sessions.len(), 1);
    assert_eq!(scan.sessions[0].messages.len(), 3);
    assert_eq!(scan.sessions[0].source_id, "conversation-1");
    assert_eq!(scan.stats.filtered_sessions, 1);
}

#[test]
fn shell_results_keep_executed_commands_and_failure_status() {
    let (_dir, path, conn) = database();
    let mut task = proto::Task::decode(TASK).unwrap();
    let shell =
        task.messages[5].tool_call_result.as_mut().unwrap().run_shell_command.as_mut().unwrap();
    shell.command = "mv input.txt changed.txt".into();
    shell.command_finished.as_mut().unwrap().exit_code = 1;
    write_task(&conn, &task);
    let raw = scan_db(&path, None, true).unwrap().sessions.remove(0);
    let result = &raw.events[3];
    assert_eq!(result.status.as_deref(), Some("error"));
    assert_eq!(result.target.as_deref(), Some("mv input.txt changed.txt"));
    assert!(result.files.iter().any(|file| file.path == "changed.txt"));
    assert_eq!(result.attrs_json.as_deref(), Some(r#"{"exit_code":1}"#));
}

#[test]
fn file_delete_move_and_opaque_tool_variants_preserve_source_evidence() {
    let context = || EventContext {
        event_seq: 0,
        timestamp: None,
        source_path: None,
        source_event_id: None,
        message_seq: None,
        parser_version: EVENT_PARSER_VERSION,
    };
    let call = proto::ToolCall {
        apply_file_diffs: Some(proto::ApplyFileDiffs {
            deleted_files: vec![proto::FilePath { file_path: "deleted.txt".into() }],
            v4a_updates: vec![proto::FileUpdate {
                file_path: "old.txt".into(),
                move_to: "new.txt".into(),
            }],
            ..Default::default()
        }),
        ..Default::default()
    };
    let event = call_event(&call, context(), Some("/tmp/project"));
    assert_eq!(
        event.files.iter().map(|file| file.operation.clone()).collect::<Vec<_>>(),
        [FileOperation::Delete, FileOperation::MoveFrom, FileOperation::MoveTo]
    );
    let call = proto::ToolCall {
        other: Some(proto::OtherTool::CallMcpTool(Vec::new())),
        ..Default::default()
    };
    assert_eq!(call_event(&call, context(), None).name.as_deref(), Some("call_mcp_tool"));
}

#[test]
fn missing_sources_are_empty_and_not_created() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.sqlite");
    assert!(scan_db(&missing, None, true).unwrap().sessions.is_empty());
    assert!(!missing.exists());
    let path = dir.path().join("empty.sqlite");
    let _conn = Connection::open(&path).unwrap();
    assert!(scan_db(&path, None, true).unwrap().sessions.is_empty());
}

#[test]
fn sqlite_and_protobuf_timestamps_are_milliseconds_without_fabricated_values() {
    assert_eq!(parse_timestamp("2026-09-30 16:00:09.250"), Some(1790784009250));
    assert_eq!(parse_timestamp("2026-09-30T18:00:09.250+02:00"), Some(1790784009250));
    assert_eq!(parse_timestamp("not a date"), None);
    assert_eq!(proto::Timestamp { seconds: 1, nanos: 1_000_000_000 }.millis(), None);
    assert_eq!(proto::Timestamp { seconds: i64::MAX, nanos: 0 }.millis(), None);
}

#[test]
fn official_origin_fixture_excludes_agent_and_automated_prompts() {
    let (_dir, path, conn) = database();
    let task =
        proto::Task::decode(include_bytes!("../../../tests/fixtures/warp/origins.bin").as_slice())
            .unwrap();
    write_task(&conn, &task);
    let raw = scan_db(&path, None, true).unwrap().sessions.remove(0);
    assert_eq!(
        raw.messages.iter().map(|message| message.content.as_str()).collect::<Vec<_>>(),
        ["origin-legacy", "origin-client", "origin-external", "origin-api", "origin-unknown"]
    );
    assert!(raw.messages.iter().all(|message| message.role == Role::User));
}

#[test]
fn forked_tasks_preserve_message_based_usage_keys_and_parent_links() {
    let (_dir, path, conn) = database();
    let mut fork = proto::Task::decode(TASK).unwrap();
    fork.id = "fork-task".into();
    conn.execute(
        "INSERT INTO agent_conversations (conversation_id, conversation_data) VALUES ('fork', ?1)",
        [r#"{"parent_conversation_id":"conversation-1"}"#],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO agent_tasks (conversation_id, task_id, task) VALUES ('fork', 'fork-task', ?1)",
        [fork.encode_to_vec()],
    )
    .unwrap();
    let scan = scan_db(&path, None, true).unwrap();
    let root = &scan.sessions[0];
    let child = &scan.sessions[1];
    assert_eq!(root.usage_events[0].event_key, child.usage_events[0].event_key);
    assert_eq!(
        serde_json::from_str::<Value>(&child.usage_events[0].event_key).unwrap(),
        json!(["usage-1", "primary_agent", "warp", "test-model"])
    );
    assert_eq!(child.thread_role, Some(ThreadRole::Subagent));
    assert_eq!(
        child.parent_links,
        vec![ParentLink {
            relation: ParentRelation::Spawn,
            source: "warp".into(),
            source_id: "conversation-1".into()
        }]
    );
    assert_eq!(child.metadata_parser_version, Some(METADATA_PARSER_VERSION));
    assert!(root.thread_role.is_none());
    assert!(root.parent_links.is_empty());
}

#[test]
fn multiple_database_scan_streams_unique_conversations_across_channels() {
    let (_dir, path, _conn) = database();
    let (_other_dir, other_path, conn) = database();
    conn.execute("INSERT INTO agent_conversations (conversation_id, conversation_data) VALUES ('other', '{}')", []).unwrap();
    conn.execute("INSERT INTO agent_tasks (conversation_id, task_id, task) VALUES ('other', 'other-task', ?1)", [TASK]).unwrap();
    let mut streamed = Vec::new();
    let mut sink = |raw: RawSession| {
        streamed.push(raw);
        Ok(())
    };
    let context = AdapterSyncContext::empty_for_test("warp").with_session_sink(&mut sink);
    let scan = scan_paths(&[path, other_path], Some(&context), None, true).unwrap();
    assert_eq!(scan.stats.candidates, 3);
    assert_eq!(scan.stats.parsed, 2);
    assert_eq!(scan.stats.filtered_sessions, 1);
    assert!(scan.sessions.is_empty());
    drop(context);
    assert_eq!(
        streamed.iter().map(|raw| raw.source_id.as_str()).collect::<Vec<_>>(),
        ["conversation-1", "other"]
    );
}

#[test]
fn invalid_parent_metadata_does_not_discard_task_content() {
    let (_dir, path, conn) = database();
    for data in [r#"{"parent_conversation_id":"  "}"#, "malformed"] {
        conn.execute("UPDATE agent_conversations SET conversation_data = ?1", [data]).unwrap();
        let raw = scan_db(&path, None, true).unwrap().sessions.remove(0);
        assert_eq!(raw.messages.len(), 3);
        assert!(raw.parent_links.is_empty());
    }
}

#[test]
fn unchanged_shell_results_do_not_repeat_call_file_evidence() {
    let (_dir, path, conn) = database();
    let mut task = proto::Task::decode(TASK).unwrap();
    task.messages[4].tool_call.as_mut().unwrap().run_shell_command.as_mut().unwrap().command =
        "mv a.txt b.txt".into();
    task.messages[5]
        .tool_call_result
        .as_mut()
        .unwrap()
        .run_shell_command
        .as_mut()
        .unwrap()
        .command = "mv a.txt b.txt".into();
    write_task(&conn, &task);
    let raw = scan_db(&path, None, true).unwrap().sessions.remove(0);
    assert_eq!(
        raw.events[2]
            .files
            .iter()
            .map(|file| (&*file.path, file.operation.clone()))
            .collect::<Vec<_>>(),
        [("a.txt", FileOperation::MoveFrom), ("b.txt", FileOperation::MoveTo)]
    );
    assert!(raw.events[3].files.is_empty());
    assert_eq!(raw.events[3].target.as_deref(), Some("mv a.txt b.txt"));
    assert_eq!(raw.events[3].status.as_deref(), Some("success"));
    assert_eq!(raw.event_parser_version, Some(EVENT_PARSER_VERSION));
    task.messages[5].timestamp = Some(proto::Timestamp { seconds: 1790784001, nanos: 0 });
    write_task(&conn, &task);
    let raw = scan_db(&path, None, true).unwrap().sessions.remove(0);
    assert_eq!(
        raw.events
            .iter()
            .filter(|event| event.tool_call_id.as_deref() == Some("shell-1"))
            .map(|event| event.files.len())
            .sum::<usize>(),
        2
    );
    task.messages[4].tool_call = None;
    write_task(&conn, &task);
    let raw = scan_db(&path, None, true).unwrap().sessions.remove(0);
    let result = raw
        .events
        .iter()
        .find(|event| event.source_event_id.as_deref() == Some("shell-result"))
        .unwrap();
    assert_eq!(result.files.len(), 2);
}

#[test]
fn unreadable_database_does_not_block_another_database() {
    let (_dir, path, _conn) = database();
    let bad = path.with_file_name("bad.sqlite");
    std::fs::write(&bad, b"not a database").unwrap();
    let scan = scan_paths(&[bad, path], None, None, true).unwrap();
    assert_eq!(scan.sessions.len(), 1);
    assert_eq!(scan.stats.parsed, 1);
    assert_eq!(scan.sessions[0].source_id, "conversation-1");
}

#[test]
fn database_error_isolation_does_not_hide_session_write_failures() {
    let (_dir, path, _conn) = database();
    let mut writes = 0;
    let mut sink = |_raw: RawSession| {
        writes += 1;
        Err(anyhow::anyhow!("index write failed"))
    };
    let context = AdapterSyncContext::empty_for_test("warp").with_session_sink(&mut sink);
    let error = scan_paths(&[path.clone(), path], Some(&context), None, true).err().unwrap();
    assert!(error.is::<SessionWriteError>());
    drop(context);
    assert_eq!(writes, 1);
}

#[test]
fn invalid_utf8_metadata_skips_only_its_conversation() {
    let (_dir, path, conn) = database();
    conn.execute_batch("INSERT INTO agent_conversations (conversation_id, conversation_data) VALUES ('a-bad', x'ff'), ('z-good', '{}');").unwrap();
    for id in ["a-bad", "z-good"] {
        conn.execute(
            "INSERT INTO agent_tasks (conversation_id, task_id, task) VALUES (?1, ?1, ?2)",
            params![id, TASK],
        )
        .unwrap();
    }
    let scan = scan_db(&path, None, true).unwrap();
    assert_eq!(scan.stats.candidates, 3);
    assert_eq!(scan.stats.parsed, 2);
    assert_eq!(
        scan.sessions.iter().map(|raw| raw.source_id.as_str()).collect::<Vec<_>>(),
        ["conversation-1", "z-good"]
    );
    conn.execute_batch("UPDATE agent_conversations SET conversation_data = CAST(x'ff' AS TEXT) WHERE conversation_id = 'a-bad';").unwrap();
    assert_eq!(scan_db(&path, None, true).unwrap().sessions.len(), 2);
}

#[test]
fn cutoff_rejects_old_tasks_before_decoding_and_keeps_recent_task_updates() {
    let (_dir, path, conn) = database();
    let cutoff = 1790784009250;
    conn.execute_batch("UPDATE agent_tasks SET task = x'ff', last_modified_at = '2026-09-29 00:00:00';
        UPDATE agent_conversations SET last_modified_at = '2026-09-29 00:00:00';
        INSERT INTO agent_conversations (conversation_id, conversation_data, last_modified_at) VALUES ('recent-task', '{}', '2026-09-29 00:00:00'), ('recent-conversation', '{}', '2026-09-30T18:00:09.250+02:00'), ('unknown-time', '{}', 'unknown');").unwrap();
    for (id, modified) in [
        ("recent-task", "2026-09-30T18:00:09.250+02:00"),
        ("recent-conversation", "2026-09-29 00:00:00"),
        ("unknown-time", "unknown"),
    ] {
        conn.execute("INSERT INTO agent_tasks (conversation_id, task_id, task, last_modified_at) VALUES (?1, ?1, ?2, ?3)", params![id, TASK, modified]).unwrap();
    }
    let scan = scan_paths(&[path], None, Some(cutoff), true).unwrap();
    assert_eq!(scan.stats.candidates, 4);
    assert_eq!(scan.stats.rejected_before_parse, 1);
    assert_eq!(scan.stats.parsed, 3);
    assert_eq!(
        scan.sessions.iter().map(|raw| raw.source_id.as_str()).collect::<Vec<_>>(),
        ["recent-conversation", "recent-task", "unknown-time"]
    );
}

#[test]
fn interleaved_result_names_do_not_depend_on_call_timestamp_order() {
    let (_dir, path, conn) = database();
    let mut task = proto::Task::decode(TASK).unwrap();
    let results: Vec<_> = task
        .messages
        .iter()
        .filter(|message| message.tool_call_result.is_some())
        .cloned()
        .map(|mut message| {
            message.timestamp = Some(proto::Timestamp { seconds: 1790784001, nanos: 0 });
            message
        })
        .collect();
    task.messages.retain(|message| message.tool_call_result.is_none());
    write_task(&conn, &task);
    let child = proto::Task { id: "results-task".into(), messages: results, ..Default::default() };
    conn.execute("INSERT INTO agent_tasks (conversation_id, task_id, task) VALUES ('conversation-1', 'results-task', ?1)", [child.encode_to_vec()]).unwrap();
    let raw = scan_db(&path, None, true).unwrap().sessions.remove(0);
    for (id, name) in
        [("read-1", "read_files"), ("shell-1", "run_shell_command"), ("edit-1", "apply_file_diffs")]
    {
        let result = raw
            .events
            .iter()
            .find(|event| event.kind == "tool_result" && event.tool_call_id.as_deref() == Some(id))
            .unwrap();
        assert_eq!(result.name.as_deref(), Some(name));
    }
}

#[test]
fn real_probe_preserves_native_messages_events_and_missing_usage() {
    let (_dir, path, conn) = database();
    conn.execute_batch("UPDATE agent_conversations SET conversation_data = '{}', last_modified_at = '2026-10-07 15:42:09';
        UPDATE agent_tasks SET last_modified_at = '2026-10-07 15:42:09';").unwrap();
    conn.execute("UPDATE agent_tasks SET task = ?1", [PROBE]).unwrap();
    let raw = scan_db(&path, None, true).unwrap().sessions.remove(0);
    assert_eq!(raw.custom_title.as_deref(), Some("Count Lines And Append Text In File"));
    assert_eq!(raw.directory.as_deref(), Some("/tmp/recall-warp-probe"));
    assert_eq!(raw.started_at, 1791387715621);
    assert_eq!(raw.updated_at, Some(1791387729000));
    assert_eq!(raw.messages.len(), 2);
    assert_eq!(raw.messages[0].role, Role::User);
    assert_eq!(
        raw.messages[0].content,
        "Read sample.txt, run wc -l sample.txt, then append a line gamma to sample.txt."
    );
    assert_eq!(raw.messages[0].timestamp, Some(1791387715621));
    assert_eq!(raw.messages[1].role, Role::Assistant);
    assert!(raw.messages[1].content.contains("alpha\nbeta\ngamma"));
    assert_eq!(raw.messages[1].timestamp, Some(1791387728192));
    assert_eq!(raw.events.len(), 7);
    let read = raw.events.iter().find(|event| event.kind == "file_read").unwrap();
    assert_eq!(read.timestamp, Some(1791387719101));
    assert_eq!(read.files.len(), 1);
    assert_eq!(read.files[0].path, "/tmp/recall-warp-probe/sample.txt");
    assert_eq!(read.files[0].operation, FileOperation::Read);
    assert_eq!(read.files[0].cwd.as_deref(), raw.directory.as_deref());
    let read_result = raw
        .events
        .iter()
        .find(|event| event.kind == "tool_result" && event.name.as_deref() == Some("read_files"))
        .unwrap();
    assert_eq!(read_result.status.as_deref(), Some("success"));
    let commands: Vec<_> = raw.events.iter().filter(|event| event.kind == "command").collect();
    assert_eq!(commands.len(), 2);
    assert_eq!(commands[0].target.as_deref(), Some("wc -l sample.txt"));
    assert!(commands[1].target.as_ref().unwrap().contains("gamma >> sample.txt"));
    let shell_results: Vec<_> = raw
        .events
        .iter()
        .filter(|event| {
            event.kind == "tool_result" && event.name.as_deref() == Some("run_shell_command")
        })
        .collect();
    assert_eq!(shell_results.len(), 2);
    assert!(shell_results[0].summary.as_ref().unwrap().contains("2 sample.txt"));
    assert!(
        shell_results[1].summary.as_ref().unwrap().contains("3 sample.txt\nalpha\nbeta\ngamma")
    );
    for result in shell_results {
        assert_eq!(result.status.as_deref(), Some("success"));
        assert_eq!(result.attrs_json.as_deref(), Some(r#"{"exit_code":0}"#));
        assert!(result.files.is_empty());
    }
    assert!(raw.usage_events.is_empty());
    assert!(raw.thread_role.is_none());
    assert!(raw.parent_links.is_empty());
}
