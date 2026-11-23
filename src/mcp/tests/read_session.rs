use super::*;

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
fn paged_get_session_caps_utf8_bytes_losslessly_and_bounds_included_events() {
    let store = setup();
    let mut stored = session("s1", "codex", "中文会话分页预算", 1_000);
    stored.summary = Some(format!("\x1b[32m加载 10%\r加载 100%\x1b[0m\r\n{}", "摘".repeat(220)));
    stored.message_count = 1;
    store.insert_session(&stored).unwrap();
    let prefix = "\x1b[31m彩色终端\x1b[0m\r保留帧\r\n\"引号\"\\\x00🦀";
    let escape_heavy = "\x01\"\n".repeat(700);
    let pad_chars = 6000 - prefix.chars().count() - escape_heavy.chars().count();
    let full_message = format!("{prefix}{escape_heavy}{}", "汉".repeat(pad_chars));
    assert_eq!(full_message.chars().count(), 6000);
    store.insert_messages(&[message("s1", Role::Assistant, &full_message, 83)]).unwrap();
    let events: Vec<_> = (0..45)
        .map(|seq| {
            let mut ev = event(
                seq,
                "tool_call",
                Some(&"路".repeat(180)),
                Some(1_700_000_000_000),
                Some(&format!("\x1b[33mstep 1\rstep 2-{seq}\x1b[0m\r\n{}", "事".repeat(160))),
                Some("Bash"),
            );
            ev.message_seq = Some(83);
            ev
        })
        .collect();
    persist_events(&store, "codex", "s1", &events);
    let index = ready(store);

    let mut args =
        GetSessionArgs { session_id: "s1".into(), around_seq: Some(83), ..Default::default() };
    let mut assembled = String::new();
    let mut pages = 0;
    loop {
        let page = get_session(&index, &args).unwrap();
        let encoded = serde_json::to_vec(&page).unwrap();
        assert!(
            encoded.len() <= MCP_INLINE_MAX_BYTES,
            "page {} exceeded inline budget: {} bytes",
            pages,
            encoded.len()
        );
        let summary = page.summary.as_deref().unwrap();
        assert!(!summary.contains('\x1b'));
        assert!(!summary.contains("加载 10%"));
        assert!(summary.starts_with("加载 100%\n"));
        assert_eq!(summary.chars().count(), EXCERPT_CHAR_CAP);
        assert_eq!(page.first_message_byte_offset, Some(assembled.len()));
        assembled.push_str(page.messages.strip_prefix("[83][assistant] ").unwrap());
        pages += 1;
        let Some(cursor) = page.next_cursor else {
            assert!(!page.truncated);
            break;
        };
        assert!(page.truncated);
        args =
            GetSessionArgs { session_id: "s1".into(), cursor: Some(cursor), ..Default::default() };
    }
    assert!(pages >= 3);
    assert_eq!(assembled, full_message);

    let with_events = get_session(
        &index,
        &GetSessionArgs {
            session_id: "s1".into(),
            around_seq: Some(83),
            include_events: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(serde_json::to_vec(&with_events).unwrap().len() <= MCP_INLINE_MAX_BYTES);
    assert_eq!(with_events.events_truncated, Some(true));
    let returned_events = with_events.events.unwrap();
    assert!(!returned_events.is_empty());
    assert!(returned_events.len() < 45);
    assert!(returned_events[0].summary.as_deref().unwrap().starts_with("step 2-0\n"));
    assert!(!returned_events[0].summary.as_deref().unwrap().contains('\x1b'));
}
