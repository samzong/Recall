use anyhow::{Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::store::Store;
use crate::types::{Message, Role};

#[derive(Clone, Debug, Default)]
pub(crate) struct MessageWindow {
    pub(crate) from_seq: Option<u32>,
    pub(crate) to_seq: Option<u32>,
    pub(crate) around_seq: Option<u32>,
    pub(crate) before: Option<u32>,
    pub(crate) after: Option<u32>,
}

impl MessageWindow {
    pub(crate) fn is_selected(&self) -> bool {
        self.from_seq.is_some()
            || self.to_seq.is_some()
            || self.around_seq.is_some()
            || self.before.is_some()
            || self.after.is_some()
    }

    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            self.around_seq.is_none() || (self.from_seq.is_none() && self.to_seq.is_none()),
            "around-seq cannot be combined with from-seq or to-seq"
        );
        ensure!(
            self.around_seq.is_some() || (self.before.is_none() && self.after.is_none()),
            "before and after require around-seq"
        );
        ensure!(
            self.from_seq.unwrap_or(0) <= self.to_seq.unwrap_or(u32::MAX),
            "from-seq must not exceed to-seq"
        );
        Ok(())
    }

    fn bounds(&self, conn: &Connection, session_id: &str) -> Result<(u32, u32)> {
        self.validate()?;
        let Some(seq) = self.around_seq else {
            return Ok((self.from_seq.unwrap_or(0), self.to_seq.unwrap_or(u32::MAX)));
        };
        let count: u64 = conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE session_id = ?1 AND seq = ?2",
            params![session_id, seq],
            |row| row.get(0),
        )?;
        ensure!(count == 1, "message sequence {seq} is missing or ambiguous");
        let first = conn.query_row(
            "SELECT COALESCE(MIN(seq), ?2) FROM
             (SELECT seq FROM messages WHERE session_id = ?1 AND seq < ?2 ORDER BY seq DESC LIMIT ?3)",
            params![session_id, seq, self.before.unwrap_or(3)], |row| row.get(0))?;
        let last = conn.query_row(
            "SELECT COALESCE(MAX(seq), ?2) FROM
             (SELECT seq FROM messages WHERE session_id = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3)",
            params![session_id, seq, self.after.unwrap_or(3)],
            |row| row.get(0),
        )?;
        let (count, unique): (u64, u64) = conn.query_row(
            "SELECT COUNT(*), COUNT(DISTINCT seq) FROM messages WHERE session_id = ?1 AND seq BETWEEN ?2 AND ?3",
            params![session_id, first, last], |row| Ok((row.get(0)?, row.get(1)?)))?;
        ensure!(count == unique, "message window contains ambiguous sequences");
        Ok((first, last))
    }
}

pub(crate) struct MessagePage {
    pub(crate) messages: Vec<Message>,
    pub(crate) next_cursor: Option<String>,
    pub(crate) first_message_byte_offset: usize,
}

#[derive(Deserialize, Serialize)]
struct MessageCursor {
    version: u8,
    session_id: String,
    snapshot: (i64, u64),
    from_seq: u32,
    to_seq: u32,
    role: Option<String>,
    next_seq: u32,
    next_id: i64,
    byte_offset: usize,
}

pub(crate) struct MessageRead<'a> {
    pub(crate) window: &'a MessageWindow,
    pub(crate) role: Option<&'a str>,
    pub(crate) max_messages: usize,
    pub(crate) max_chars: usize,
    pub(crate) cursor: Option<&'a str>,
}

impl Store {
    pub(crate) fn get_messages_in_window(
        &self,
        session_id: &str,
        window: &MessageWindow,
        role: Option<&str>,
        limit: usize,
        tail: bool,
    ) -> Result<Vec<Message>> {
        let tx = self.conn.unchecked_transaction()?;
        let (first, last) = window.bounds(&tx, session_id)?;
        let order = if tail { "DESC" } else { "ASC" };
        let mut stmt = tx.prepare(&format!(
            "SELECT role, content, timestamp, seq FROM messages
             WHERE session_id = ?1 AND seq BETWEEN ?2 AND ?3 AND (?4 IS NULL OR role = ?4)
             ORDER BY seq {order}, id {order} LIMIT ?5"
        ))?;
        let mut messages = stmt
            .query_map(
                params![session_id, first, last, role, i64::try_from(limit).unwrap_or(i64::MAX)],
                |row| {
                    Ok(Message {
                        session_id: session_id.to_string(),
                        role: row.get::<_, String>(0)?.parse().unwrap_or(Role::User),
                        content: row.get(1)?,
                        timestamp: row.get(2)?,
                        seq: row.get(3)?,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if tail {
            messages.reverse();
        }
        Ok(messages)
    }

    pub(crate) fn read_message_page(
        &self,
        session_id: &str,
        read: &MessageRead<'_>,
    ) -> Result<MessagePage> {
        ensure!(
            read.max_messages > 0 && read.max_messages <= 1000,
            "max-messages must be between 1 and 1000"
        );
        ensure!(
            read.max_chars > 0 && read.max_chars <= 32_000,
            "max-chars must be between 1 and 32000"
        );
        read.window.validate()?;
        ensure!(
            read.cursor.is_none() || (!read.window.is_selected() && read.role.is_none()),
            "cursor cannot be combined with message selection or role"
        );
        let tx = self.conn.unchecked_transaction()?;
        let snapshot = tx.query_row(
            "SELECT COALESCE(MAX(id), 0), COUNT(*) FROM messages WHERE session_id = ?1",
            [session_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, u64>(1)?)),
        )?;
        let mut cursor = if let Some(value) = read.cursor {
            ensure!(value.len() <= 4096, "invalid message cursor");
            let value: MessageCursor = serde_json::from_str(value)
                .map_err(|_| anyhow::anyhow!("invalid message cursor"))?;
            ensure!(
                value.version == 1
                    && value.session_id == session_id
                    && value.from_seq <= value.next_seq
                    && value.next_seq <= value.to_seq
                    && value.next_id >= 0
                    && value.byte_offset <= i64::MAX as usize,
                "invalid message cursor"
            );
            ensure!(
                value.snapshot == snapshot,
                "message cursor is stale; search or select the window again"
            );
            value
        } else {
            let (from_seq, to_seq) = read.window.bounds(&tx, session_id)?;
            MessageCursor {
                version: 1,
                session_id: session_id.to_string(),
                snapshot,
                from_seq,
                to_seq,
                role: read.role.map(str::to_string),
                next_seq: from_seq,
                next_id: 0,
                byte_offset: 0,
            }
        };
        let mut messages = Vec::new();
        let first_message_byte_offset = cursor.byte_offset;
        let mut remaining = read.max_chars;
        let mut stmt = tx.prepare(
            "SELECT id, seq, role, timestamp, length(CAST(content AS BLOB)),
                    COALESCE(substr(CAST(content AS BLOB), ?7 + 1, ?8), X'')
             FROM messages WHERE session_id = ?1 AND seq BETWEEN ?2 AND ?3
               AND (?4 IS NULL OR role = ?4)
               AND (seq > ?5 OR (seq = ?5 AND id >= ?6))
             ORDER BY seq, id LIMIT 1",
        )?;
        let has_more = loop {
            let row = stmt
                .query_row(
                    params![
                        session_id,
                        cursor.from_seq,
                        cursor.to_seq,
                        cursor.role,
                        cursor.next_seq,
                        cursor.next_id,
                        cursor.byte_offset as i64,
                        (remaining.max(1) * 4) as i64
                    ],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, u32>(1)?,
                            row.get::<_, String>(2)?.parse().unwrap_or(Role::User),
                            row.get(3)?,
                            row.get::<_, usize>(4)?,
                            row.get::<_, Vec<u8>>(5)?,
                        ))
                    },
                )
                .optional()?;
            let Some((id, seq, role, timestamp, total_bytes, bytes)) = row else {
                break false;
            };
            cursor.next_id = id;
            cursor.next_seq = seq;
            if remaining == 0 || messages.len() >= read.max_messages {
                break true;
            }
            ensure!(cursor.byte_offset <= total_bytes, "invalid message cursor offset");
            let text = match std::str::from_utf8(&bytes) {
                Ok(text) => text,
                Err(error) if error.error_len().is_none() => {
                    std::str::from_utf8(&bytes[..error.valid_up_to()])?
                }
                Err(_) => bail!("invalid message cursor offset or message encoding"),
            };
            let content: String = text.chars().take(remaining).collect();
            remaining -= content.chars().count();
            cursor.byte_offset += content.len();
            messages.push(Message {
                session_id: session_id.to_string(),
                seq,
                role,
                timestamp,
                content,
            });
            if cursor.byte_offset < total_bytes {
                break true;
            }
            cursor.next_id =
                id.checked_add(1).ok_or_else(|| anyhow::anyhow!("message id overflow"))?;
            cursor.byte_offset = 0;
        };
        Ok(MessagePage {
            messages,
            first_message_byte_offset,
            next_cursor: has_more.then(|| serde_json::to_string(&cursor)).transpose()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with_messages(messages: &[(u32, &str)]) -> Store {
        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        store.conn.execute("INSERT INTO sessions (id, source, source_id, title, started_at) VALUES ('s', 'test', 's', 'Test', 0)", []).unwrap();
        for (seq, content) in messages {
            store.conn.execute("INSERT INTO messages (session_id, seq, role, content) VALUES ('s', ?1, 'assistant', ?2)", params![seq, content]).unwrap();
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
        let conflict =
            MessageWindow { around_seq: Some(40), from_seq: Some(0), ..Default::default() };
        assert!(store.get_messages_in_window("s", &conflict, None, 100, false).is_err());
        store.conn.execute("INSERT INTO messages (session_id, seq, role, content) VALUES ('s', 40, 'user', 'duplicate')", []).unwrap();
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
        let input = [
            (2, "\u{4f60}\u{597d}🦀e\u{301}\0end"),
            (2, "second"),
            (9, ""),
            (100, "\u{6700}\u{540e}"),
        ];
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
                            cursor: cursor.as_deref(),
                        },
                    )
                    .unwrap();
                assert!(page.messages.len() <= 2);
                assert!(
                    page.messages.iter().map(|m| m.content.chars().count()).sum::<usize>()
                        <= budget
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
            cursor: page.next_cursor.as_deref(),
        };
        let rest = store.read_message_page("s", &read).unwrap();
        assert_eq!(rest.first_message_byte_offset, 3);
        assert_eq!(rest.messages.len(), 1);
        assert_eq!(rest.messages[0].content, "defghij");
        assert!(rest.next_cursor.is_none());
        assert!(store.read_message_page("other", &read).is_err());
        store.conn.execute("DELETE FROM messages WHERE session_id = 's' AND seq = 5", []).unwrap();
        store.conn.execute("INSERT INTO messages (session_id, seq, role, content) VALUES ('s', 5, 'assistant', 'abcdefghij')", []).unwrap();
        assert!(store.read_message_page("s", &read).is_err());
    }
}
