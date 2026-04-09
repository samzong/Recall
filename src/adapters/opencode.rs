use std::collections::HashMap;

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use tracing::warn;

use crate::adapters::{RawMessage, RawSession, SourceAdapter};
use crate::types::Role;

pub struct OpenCodeAdapter;

impl SourceAdapter for OpenCodeAdapter {
    fn id(&self) -> &str {
        "opencode"
    }
    fn label(&self) -> &str {
        "OC"
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let db_path = dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("no home dir"))?
            .join(".local/share/opencode/opencode.db");

        if !db_path.exists() {
            warn!("OpenCode DB not found at {}, skipping", db_path.display());
            return Ok(vec![]);
        }

        let conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;

        let mut session_stmt =
            conn.prepare("SELECT id, title, directory, time_created, time_updated FROM session")?;

        let session_rows: Vec<(String, String, String, i64, Option<i64>)> = session_stmt
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
            })?
            .filter_map(|r| r.ok())
            .collect();

        let mut msg_stmt = conn.prepare(
            "SELECT m.session_id, json_extract(m.data, '$.role') AS role,
                    p.data, m.time_created
             FROM message m
             JOIN part p ON p.message_id = m.id
             ORDER BY m.time_created, p.id",
        )?;

        let mut session_messages: HashMap<String, Vec<RawMessage>> = HashMap::new();

        let msg_rows = msg_stmt.query_map([], |row| {
            let session_id: String = row.get(0)?;
            let role: Option<String> = row.get(1)?;
            let part_data: String = row.get(2)?;
            let timestamp: Option<i64> = row.get(3)?;
            Ok((session_id, role, part_data, timestamp))
        })?;

        for row in msg_rows.flatten() {
            let (session_id, role_str, part_data, timestamp) = row;

            let role = match role_str.as_deref() {
                Some("user") => Role::User,
                Some("assistant") => Role::Assistant,
                _ => continue,
            };

            let part: Value = match serde_json::from_str(&part_data) {
                Ok(v) => v,
                Err(_) => continue,
            };

            let part_type = part.get("type").and_then(|t| t.as_str()).unwrap_or("");
            let text = match part_type {
                "text" => match part.get("text").and_then(|t| t.as_str()) {
                    Some(t) if !t.is_empty() => t.to_string(),
                    _ => continue,
                },
                "tool-invocation" | "tool-result" => {
                    let name = part.get("toolName").and_then(|n| n.as_str()).unwrap_or("tool");
                    if let Some(input) = part.get("input") {
                        format!("[{name}] {input}")
                    } else if let Some(result) = part.get("result") {
                        format!("[{name}] {result}")
                    } else {
                        continue;
                    }
                }
                _ => continue,
            };

            session_messages.entry(session_id).or_default().push(RawMessage {
                role,
                content: text,
                timestamp,
            });
        }

        let mut sessions = Vec::new();
        for (id, _title, directory, time_created, time_updated) in session_rows {
            let messages = session_messages.remove(&id).unwrap_or_default();
            if messages.is_empty() {
                continue;
            }
            sessions.push(RawSession {
                source_id: id,
                directory: Some(directory),
                started_at: time_created,
                updated_at: time_updated,
                entrypoint: None,
                messages,
            });
        }

        Ok(sessions)
    }
}
