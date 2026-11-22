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
    pub(crate) max_rendered_bytes: Option<usize>,
    pub(crate) cursor: Option<&'a str>,
}

pub(crate) fn json_escaped_char_bytes(ch: char) -> usize {
    match ch {
        '"' | '\\' | '\x08' | '\x0c' | '\n' | '\r' | '\t' => 2,
        c if (c as u32) < 0x20 => 6,
        c => c.len_utf8(),
    }
}

pub(crate) fn json_escaped_str_bytes(text: &str) -> usize {
    text.chars().map(json_escaped_char_bytes).sum()
}

fn decimal_digits(mut value: usize) -> usize {
    let mut digits = 1;
    while value >= 10 {
        value /= 10;
        digits += 1;
    }
    digits
}

fn rendered_prefix_bytes(seq: u32, role: &Role) -> usize {
    decimal_digits(seq as usize) + role.as_str().len() + 5
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
        let mut remaining_bytes = read.max_rendered_bytes;
        let mut stmt = tx.prepare(
            "SELECT id, seq, role, timestamp, length(CAST(content AS BLOB)),
                    COALESCE(substr(CAST(content AS BLOB), ?7 + 1, ?8), X'')
             FROM messages WHERE session_id = ?1 AND seq BETWEEN ?2 AND ?3
               AND (?4 IS NULL OR role = ?4)
               AND (seq > ?5 OR (seq = ?5 AND id >= ?6))
             ORDER BY seq, id LIMIT 1",
        )?;
        let has_more = loop {
            let fetch_bytes = (remaining.max(1) * 4)
                .min(remaining_bytes.map_or(usize::MAX, |budget| budget.saturating_add(4)));
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
                        fetch_bytes as i64
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
            let content = if let Some(rem_bytes) = remaining_bytes {
                let sep_bytes = if messages.is_empty() { 0 } else { 4 };
                let header_bytes = sep_bytes + rendered_prefix_bytes(seq, &role);
                if !messages.is_empty() {
                    let min_needed =
                        header_bytes + text.chars().next().map_or(0, json_escaped_char_bytes);
                    if min_needed > rem_bytes {
                        break true;
                    }
                }
                let mut msg_budget = rem_bytes.saturating_sub(header_bytes);
                let mut kept_chars = 0;
                let mut kept_utf8 = 0;
                for ch in text.chars().take(remaining) {
                    let cost = json_escaped_char_bytes(ch);
                    if kept_chars == 0 && messages.is_empty() {
                        msg_budget = msg_budget.saturating_sub(cost);
                    } else if cost <= msg_budget {
                        msg_budget -= cost;
                    } else {
                        break;
                    }
                    kept_chars += 1;
                    kept_utf8 += ch.len_utf8();
                }
                remaining -= kept_chars;
                remaining_bytes = Some(msg_budget);
                text[..kept_utf8].to_string()
            } else {
                let content: String = text.chars().take(remaining).collect();
                remaining -= content.chars().count();
                content
            };
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
mod tests;
