use super::*;

#[test]
fn export_jsonl_emits_session_messages_and_usage_events() {
    let store = setup();
    let mut session = make_session("s1", "codex", "raw1", "Export session");
    session.started_at = 1_800_000_000_000;
    session.updated_at = Some(1_800_000_001_000);
    session.message_count = 2;
    session.entrypoint = Some("codex resume raw1".to_string());
    session.custom_title = Some("Export custom title".to_string());
    session.summary = Some("Export summary".to_string());
    session.duration_minutes = Some(12);
    session.repo_remote = Some("github.com/samzong/Recall".to_string());
    session.repo_slug = Some("samzong/Recall".to_string());
    session.repo_name = Some("Recall".to_string());
    let messages = vec![
        make_message("s1", Role::User, "hello", 0),
        make_message("s1", Role::Assistant, "hi", 1),
    ];
    let usage = vec![make_usage_event("evt-1", 1_800_000_001_000, "gpt-5")];
    let events = vec![make_session_event("file_read", Some("read_file"), Some("src/main.rs"))];
    store
        .persist_session_with_usage_and_events(
            &session,
            &messages,
            &usage,
            Some(1),
            &events,
            Some(1),
        )
        .unwrap();

    let options = ExportOptions {
        session_ids: Vec::new(),
        sources: None,
        time_range: TimeRange::All,
        scope: ProjectScope::Global,
        thread_role: None,
        limit: Some(10),
        includes: ExportIncludes::full(),
    };
    let mut out = Vec::new();
    write_jsonl(&store, &options, &mut out).unwrap();

    let text = String::from_utf8(out).unwrap();
    let lines: Vec<_> = text.lines().collect();
    assert_eq!(lines.len(), 1);
    let value: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(value["schema_version"], 7);
    assert_eq!(value["record_type"], "session");
    assert_eq!(value["session"]["source"], "codex");
    assert_eq!(value["session"]["source_id"], "raw1");
    assert_eq!(value["session"]["topology"]["thread_role"], serde_json::Value::Null);
    assert_eq!(value["session"]["topology"]["parents"], serde_json::json!([]));
    assert_eq!(value["session"]["directory"], "/tmp/test");
    assert_eq!(value["session"]["repo_remote"], "github.com/samzong/Recall");
    assert_eq!(value["session"]["repo_slug"], "samzong/Recall");
    assert_eq!(value["session"]["repo_name"], "Recall");
    assert_eq!(value["session"]["custom_title"], "Export custom title");
    assert_eq!(value["session"]["summary"], "Export summary");
    assert_eq!(value["session"]["duration_minutes"], 12);
    assert_eq!(value["messages"][0]["seq"], 0);
    assert_eq!(value["messages"][0]["role"], "user");
    assert_eq!(value["messages"][1]["seq"], 1);
    assert_eq!(value["messages"][1]["role"], "assistant");
    assert_eq!(value["usage_events"][0]["event_key"], "evt-1");
    assert_eq!(value["usage_events"][0]["message_seq"], 1);
    assert_eq!(value["usage_events"][0]["model"], "gpt-5");
    assert_eq!(value["usage_events"][0]["token_source"], "observed");
    assert_eq!(value["usage_events"][0]["output_tokens"], 4);
    assert_eq!(value["usage_events"][0]["reasoning_tokens"], 1);
    assert_eq!(value["usage_events"][0]["parser_version"], 5);
    assert_eq!(value["usage_events"][0]["source_path"], "/tmp/source.jsonl");
    assert_eq!(value["usage_events"][0]["raw_usage_json"], r#"{"input_tokens":10}"#);
    assert_eq!(value["events"][0]["kind"], "file_read");
    assert_eq!(value["events"][0]["name"], "read_file");
    assert_eq!(value["events"][0]["target"], "src/main.rs");
    assert_eq!(value["events"][0]["message_seq"], 1);
    assert_eq!(value["events"][0]["source_event_id"], "42");
    assert_eq!(value["events"][0]["tool_call_id"], "call-42");
    assert_eq!(value["events"][0]["is_meta"], false);
    assert_eq!(value["events"][0]["visibility"], "visible");
    assert_eq!(value["events"][0]["parser_version"], 1);
    assert_eq!(value["events"][0]["attrs_json"], r#"{"path":"src/main.rs"}"#);
}

#[test]
fn export_jsonl_reads_every_record_from_one_snapshot() {
    struct VersionSwitchWriter {
        output: Vec<u8>,
        writer: rusqlite::Connection,
        switched: bool,
        checkpoint_busy: Option<i64>,
    }

    impl std::io::Write for VersionSwitchWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.output.extend_from_slice(buf);
            if !self.switched && buf.contains(&b'\n') {
                self.writer
                    .execute_batch(
                        "BEGIN IMMEDIATE;
                         UPDATE sessions SET title = 'version-b' WHERE id = 's2';
                         UPDATE messages SET content = 'version-b' WHERE session_id = 's2';
                         COMMIT;",
                    )
                    .map_err(std::io::Error::other)?;
                self.checkpoint_busy = Some(
                    self.writer
                        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))
                        .map_err(std::io::Error::other)?,
                );
                self.switched = true;
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    schema::register_sqlite_vec();
    let root = tempfile::tempdir().unwrap();
    let db_path = root.path().join("recall.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA busy_timeout=5000;
         PRAGMA foreign_keys=ON;",
    )
    .unwrap();
    schema::init(&conn).unwrap();
    let store =
        Store { trigram_message_flag: schema::has_trigram_message_flag(&conn).unwrap(), conn };

    let mut first = make_session("s1", "codex", "raw1", "version-a");
    first.started_at = 2;
    let mut second = make_session("s2", "codex", "raw2", "version-a");
    second.started_at = 1;
    for session in [&first, &second] {
        store.insert_session(session).unwrap();
        store.insert_messages(&[make_message(&session.id, Role::User, "version-a", 0)]).unwrap();
    }
    let mut third = make_session("s3", "codex", "raw3", "large");
    third.started_at = 0;
    store.insert_session(&third).unwrap();
    let large_message = "x".repeat(9 * 1024 * 1024);
    store.insert_messages(&[make_message(&third.id, Role::User, &large_message, 0)]).unwrap();
    drop(large_message);

    let writer_conn = rusqlite::Connection::open(&db_path).unwrap();
    writer_conn.execute_batch("PRAGMA busy_timeout=0; PRAGMA foreign_keys=ON;").unwrap();
    let mut writer = VersionSwitchWriter {
        output: Vec::new(),
        writer: writer_conn,
        switched: false,
        checkpoint_busy: None,
    };
    let options = ExportOptions {
        session_ids: Vec::new(),
        sources: None,
        time_range: TimeRange::All,
        scope: ProjectScope::Global,
        thread_role: None,
        limit: None,
        includes: ExportIncludes { messages: true, usage: false, events: false },
    };

    write_jsonl(&store, &options, &mut writer).unwrap();

    let records = String::from_utf8(std::mem::take(&mut writer.output))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(writer.checkpoint_busy, Some(0));
    assert_eq!(records[1]["session"]["title"], "version-a");
    assert_eq!(records[1]["messages"][0]["content"], "version-a");
    assert_eq!(records[2]["messages"][0]["content"].as_str().unwrap().len(), 9 * 1024 * 1024);
}

#[test]
fn export_jsonl_applies_include_projection() {
    let store = setup();
    let session = make_session("s1", "codex", "raw1", "Projected export");
    let messages = vec![make_message("s1", Role::User, "hello", 0)];
    let usage = vec![make_usage_event("evt-1", 1_800_000_001_000, "gpt-5")];
    let events = vec![make_session_event("file_read", Some("read_file"), Some("src/main.rs"))];
    store
        .persist_session_with_usage_and_events(
            &session,
            &messages,
            &usage,
            Some(1),
            &events,
            Some(1),
        )
        .unwrap();

    let options = ExportOptions {
        session_ids: Vec::new(),
        sources: None,
        time_range: TimeRange::All,
        scope: ProjectScope::Global,
        thread_role: None,
        limit: None,
        includes: ExportIncludes { messages: true, usage: false, events: false },
    };
    let mut out = Vec::new();
    write_jsonl(&store, &options, &mut out).unwrap();

    let text = String::from_utf8(out).unwrap();
    let value: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(value["messages"][0]["content"], "hello");
    assert_eq!(value["usage_events"].as_array().unwrap().len(), 0);
    assert_eq!(value["events"].as_array().unwrap().len(), 0);
}

#[test]
fn export_include_requires_messages() {
    let err = match crate::export::parse_export_includes(Some("metadata,usage")) {
        Ok(_) => panic!("messages should be required"),
        Err(err) => err,
    };
    assert_eq!(err.to_string(), "--include must include messages");
    assert!(crate::export::parse_export_includes(Some("metadata,messages")).unwrap().messages);
}

#[test]
fn export_jsonl_can_select_sessions_by_id() {
    let store = setup();
    for id in ["s1", "s2", "s3"] {
        let session = make_session(id, "codex", &format!("raw-{id}"), id);
        store.insert_session(&session).unwrap();
        store.insert_messages(&[make_message(id, Role::User, id, 0)]).unwrap();
    }

    let options = ExportOptions {
        session_ids: vec!["s3".to_string(), "s1".to_string(), "s3".to_string()],
        sources: None,
        time_range: TimeRange::All,
        scope: ProjectScope::Global,
        thread_role: None,
        limit: None,
        includes: ExportIncludes::full(),
    };
    let mut out = Vec::new();
    write_jsonl(&store, &options, &mut out).unwrap();

    let text = String::from_utf8(out).unwrap();
    let lines: Vec<_> = text.lines().collect();
    assert_eq!(lines.len(), 3);
    let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    let third: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
    assert_eq!(first["session"]["id"], "s3");
    assert_eq!(second["session"]["id"], "s1");
    assert_eq!(third["session"]["id"], "s3");
}

#[test]
fn export_jsonl_applies_source_time_project_and_limit_filters() {
    let store = setup();
    let now = chrono::Utc::now().timestamp_millis();

    let mut newest = make_session("s-newest", "codex", "raw-newest", "Newest Codex");
    newest.started_at = now;
    newest.directory = Some("/tmp/project".to_string());
    let mut recent = make_session("s-recent", "codex", "raw-recent", "Recent Codex");
    recent.started_at = now - 1_000;
    recent.directory = Some("/tmp/project/subdir".to_string());
    let mut old = make_session("s-old", "codex", "raw-old", "Old Codex");
    old.started_at = now - 40 * 24 * 60 * 60 * 1_000;
    old.directory = Some("/tmp/project".to_string());
    let mut sibling = make_session("s-sibling", "codex", "raw-sibling", "Sibling Project");
    sibling.started_at = now + 2_000;
    sibling.directory = Some("/tmp/project-sibling".to_string());
    let mut other_source = make_session("s-other", "claude-code", "raw-other", "Other Source");
    other_source.started_at = now + 1_000;
    other_source.directory = Some("/tmp/project".to_string());

    for session in [&newest, &recent, &old, &sibling, &other_source] {
        store.insert_session(session).unwrap();
        store.insert_messages(&[make_message(&session.id, Role::User, &session.title, 0)]).unwrap();
    }

    let options = ExportOptions {
        session_ids: Vec::new(),
        sources: Some(vec!["codex".to_string()]),
        time_range: TimeRange::Month,
        scope: ProjectScope::Directory("/tmp/project".to_string()),
        thread_role: None,
        limit: Some(1),
        includes: ExportIncludes::full(),
    };
    let mut out = Vec::new();
    write_jsonl(&store, &options, &mut out).unwrap();

    let text = String::from_utf8(out).unwrap();
    let lines: Vec<_> = text.lines().collect();
    assert_eq!(lines.len(), 1);
    let value: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(value["session"]["id"], "s-newest");
    assert_eq!(value["session"]["source"], "codex");
}

#[test]
fn export_jsonl_applies_repo_filter() {
    let store = setup();
    let mut main = make_session("s1", "codex", "raw1", "Main worktree");
    main.repo_slug = Some("samzong/Recall".to_string());
    main.repo_name = Some("Recall".to_string());
    let mut other = make_session("s2", "codex", "raw2", "Other repo");
    other.repo_slug = Some("other/Recall".to_string());
    other.repo_name = Some("Recall".to_string());

    for session in [&main, &other] {
        store.insert_session(session).unwrap();
        store.insert_messages(&[make_message(&session.id, Role::User, &session.title, 0)]).unwrap();
    }

    let options = ExportOptions {
        session_ids: Vec::new(),
        sources: None,
        time_range: TimeRange::All,
        scope: ProjectScope::Repository {
            filter: RepoFilter::Slug("samzong/Recall".to_string()),
            local_root: None,
        },
        thread_role: None,
        limit: None,
        includes: ExportIncludes::full(),
    };
    let mut out = Vec::new();
    write_jsonl(&store, &options, &mut out).unwrap();

    let text = String::from_utf8(out).unwrap();
    let lines: Vec<_> = text.lines().collect();
    assert_eq!(lines.len(), 1);
    let value: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(value["session"]["id"], "s1");
    assert_eq!(value["session"]["repo_slug"], "samzong/Recall");
}
