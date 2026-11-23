use super::*;

fn store_with_messages(messages: &[(u32, &str)]) -> Store {
    crate::db::schema::register_sqlite_vec();
    let store = Store::open_in_memory().unwrap();
    store
        .conn
        .execute(
            "INSERT INTO sessions (id, source, source_id, title, started_at) VALUES ('s', 'test', 's', 'Test', 0)",
            [],
        )
        .unwrap();
    for (seq, content) in messages {
        store
            .conn
            .execute(
                "INSERT INTO messages (session_id, seq, role, content) VALUES ('s', ?1, 'assistant', ?2)",
                params![seq, content],
            )
            .unwrap();
    }
    store
}

#[test]
fn around_counts_messages_across_gaps_and_stops_at_edges() {
    let store = store_with_messages(&[(0, "a"), (10, "b"), (40, "c"), (90, "d"), (100, "e")]);
    let window = MessageWindow {
        around_seq: Some(40),
        before: Some(1),
        after: Some(1),
        ..Default::default()
    };
    let messages = store.get_messages_in_window("s", &window, None, 100, false).unwrap();
    assert_eq!(messages.iter().map(|m| m.seq).collect::<Vec<_>>(), vec![10, 40, 90]);
    let window = MessageWindow {
        around_seq: Some(0),
        before: Some(3),
        after: Some(0),
        ..Default::default()
    };
    assert_eq!(store.get_messages_in_window("s", &window, None, 100, false).unwrap().len(), 1);
    let missing = MessageWindow { around_seq: Some(11), ..Default::default() };
    assert!(store.get_messages_in_window("s", &missing, None, 100, false).is_err());
    let conflict = MessageWindow { around_seq: Some(40), from_seq: Some(0), ..Default::default() };
    assert!(store.get_messages_in_window("s", &conflict, None, 100, false).is_err());
    store
        .conn
        .execute(
            "INSERT INTO messages (session_id, seq, role, content) VALUES ('s', 40, 'user', 'duplicate')",
            [],
        )
        .unwrap();
    assert!(
        store
            .get_messages_in_window(
                "s",
                &MessageWindow { around_seq: Some(40), ..Default::default() },
                None,
                100,
                false
            )
            .is_err()
    );
}

#[test]
fn pages_preserve_unicode_nul_empty_and_duplicate_sequence_messages() {
    let input =
        [(2, "\u{4f60}\u{597d}🦀e\u{301}\0end"), (2, "second"), (9, ""), (100, "\u{6700}\u{540e}")];
    let store = store_with_messages(&input);
    for budget in 1..=12 {
        let mut cursor = None;
        let mut text = String::new();
        let mut pages = 0;
        loop {
            let page = store
                .read_message_page(
                    "s",
                    &MessageRead {
                        window: &MessageWindow::default(),
                        role: None,
                        max_messages: 2,
                        max_chars: budget,
                        max_rendered_bytes: None,
                        cursor: cursor.as_deref(),
                    },
                )
                .unwrap();
            assert!(page.messages.len() <= 2);
            assert!(
                page.messages.iter().map(|m| m.content.chars().count()).sum::<usize>() <= budget
            );
            for message in page.messages {
                text.push_str(&message.content);
            }
            pages += 1;
            assert!(pages < 100);
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(text, input.iter().map(|(_, s)| *s).collect::<String>());
    }
}

#[test]
fn cursor_keeps_selection_and_rejects_reindexed_messages() {
    let store = store_with_messages(&[(0, "outside"), (5, "abcdefghij"), (10, "outside")]);
    let window = MessageWindow { from_seq: Some(5), to_seq: Some(5), ..Default::default() };
    let page = store
        .read_message_page(
            "s",
            &MessageRead {
                window: &window,
                role: None,
                max_messages: 2,
                max_chars: 3,
                max_rendered_bytes: None,
                cursor: None,
            },
        )
        .unwrap();
    assert_eq!(page.messages[0].content, "abc");
    let read = MessageRead {
        window: &MessageWindow::default(),
        role: None,
        max_messages: 2,
        max_chars: 32,
        max_rendered_bytes: None,
        cursor: page.next_cursor.as_deref(),
    };
    let rest = store.read_message_page("s", &read).unwrap();
    assert_eq!(rest.first_message_byte_offset, 3);
    assert_eq!(rest.messages.len(), 1);
    assert_eq!(rest.messages[0].content, "defghij");
    assert!(rest.next_cursor.is_none());
    assert!(store.read_message_page("other", &read).is_err());
    store.conn.execute("DELETE FROM messages WHERE session_id = 's' AND seq = 5", []).unwrap();
    store
        .conn
        .execute(
            "INSERT INTO messages (session_id, seq, role, content) VALUES ('s', 5, 'assistant', 'abcdefghij')",
            [],
        )
        .unwrap();
    assert!(store.read_message_page("s", &read).is_err());
}

#[test]
fn rendered_byte_budget_preserves_lossless_cursor_and_advances_single_codepoint() {
    for ch in ('\x00'..='\x7f').chain(['汉', '🦀', '…', '\u{2028}']) {
        let expected = serde_json::to_vec(&ch.to_string()).unwrap().len() - 2;
        assert_eq!(json_escaped_char_bytes(ch), expected);
    }
    let input = [
        (3, "\x1b[31m红\x1b[0m\r覆写\r\n\"quote\"\\\x00🦀"),
        (4, ""),
        (83, "终端进度10%\r终端进度100%\n完成"),
    ];
    let store = store_with_messages(&input);
    for byte_budget in [1usize, 19, 64] {
        let mut cursor = None;
        let mut reconstructed = vec![String::new(); input.len()];
        let mut pages = 0;
        loop {
            let page = store
                .read_message_page(
                    "s",
                    &MessageRead {
                        window: &MessageWindow::default(),
                        role: None,
                        max_messages: 50,
                        max_chars: 6000,
                        max_rendered_bytes: Some(byte_budget),
                        cursor: cursor.as_deref(),
                    },
                )
                .unwrap();
            assert!(!page.messages.is_empty());
            if page.messages.len() > 1 || page.messages[0].content.chars().count() > 1 {
                let rendered = page
                    .messages
                    .iter()
                    .map(|m| format!("[{}][{}] {}", m.seq, m.role.as_str(), m.content))
                    .collect::<Vec<_>>()
                    .join("\n\n");
                assert!(json_escaped_str_bytes(&rendered) <= byte_budget);
            }
            for (idx, msg) in page.messages.iter().enumerate() {
                let slot = input.iter().position(|(seq, _)| *seq == msg.seq).unwrap();
                if idx == 0 {
                    assert_eq!(reconstructed[slot].len(), page.first_message_byte_offset);
                }
                reconstructed[slot].push_str(&msg.content);
            }
            pages += 1;
            assert!(pages < 200);
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        for ((_, expected), actual) in input.iter().zip(reconstructed.iter()) {
            assert_eq!(actual, expected);
        }
    }
}
