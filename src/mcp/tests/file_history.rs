use super::*;

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
fn target_file_history_default_limit_converges_bytes_and_pages_losslessly() {
    use crate::types::{FileEvidence, FileEvidenceKind, FileOperation, FileTarget};

    let store = setup();
    let mut stored = session("s1", "codex", "target paging", 5_000);
    stored.repo_remote = Some("github.com/example/project".into());
    store.insert_session(&stored).unwrap();
    let file = FileEvidence {
        path: "src/lib.rs".into(),
        operation: FileOperation::Write,
        kind: FileEvidenceKind::Call,
        cwd: Some("/tmp/project".into()),
        target: Some(FileTarget {
            absolute_path: "/tmp/project/src/lib.rs".into(),
            repo_root: Some("/tmp/project".into()),
            repo_relative_path: Some("src/lib.rs".into()),
            repo_remote: Some("github.com/example/project".into()),
        }),
    };
    let events: Vec<_> = (0..18)
        .map(|seq| {
            let mut ev = event(
                seq,
                "file_write",
                Some("src/lib.rs"),
                (seq < 15).then_some(10_000 + i64::from(seq)),
                Some(&format!("\x1b[32mdraft\rfinal-{seq}\x1b[0m\r\n{}", "详".repeat(90))),
                Some("Edit"),
            );
            ev.files = vec![file.clone()];
            ev
        })
        .collect();
    persist_events(&store, "codex", "s1", &events);
    let index = ready(store);

    let full = file_history(
        &index,
        &FileHistoryArgs {
            path: "src/lib.rs".into(),
            target_project: Some("https://github.com/example/project.git".into()),
            limit: Some(50),
            ..Default::default()
        },
    )
    .unwrap();
    let expected_seqs: Vec<u32> = full.events.iter().map(|e| e.event_seq).collect();
    assert_eq!(expected_seqs.len(), 18);

    let mut cursor = None;
    let mut collected_seqs = Vec::new();
    let mut page_idx = 0;
    loop {
        let page = file_history(
            &index,
            &FileHistoryArgs {
                path: "src/lib.rs".into(),
                target_project: Some("https://github.com/example/project.git".into()),
                cursor: cursor.clone(),
                ..Default::default()
            },
        )
        .unwrap();
        let encoded = serde_json::to_vec(&page).unwrap();
        assert!(
            encoded.len() <= MCP_INLINE_MAX_BYTES,
            "target file_history page {} exceeded budget: {} bytes",
            page_idx,
            encoded.len()
        );
        if page_idx == 0 {
            assert!(page.coverage.is_some());
            assert!(page.events.len() < 18);
            assert_eq!(page.has_more, Some(true));
        } else {
            assert!(page.coverage.is_none());
        }
        assert!(!page.events.is_empty());
        for ev in &page.events {
            let summary = ev.summary.as_deref().unwrap();
            assert!(!summary.contains('\x1b'));
            assert!(summary.starts_with("final-"));
            collected_seqs.push(ev.event_seq);
        }
        page_idx += 1;
        assert!(page_idx <= 20);
        if page.has_more == Some(false) {
            assert!(page.next_cursor.is_none());
            break;
        }
        cursor = page.next_cursor;
        assert!(cursor.is_some());
    }
    assert!(page_idx >= 2);
    assert_eq!(collected_seqs, expected_seqs);
}
