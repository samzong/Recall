use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use tracing::debug;

use crate::adapters::{RawMessage, RawSession, ResumeCommand, SourceAdapter};
use crate::types::Role;

pub struct KiroAdapter;

impl SourceAdapter for KiroAdapter {
    fn id(&self) -> &str {
        "kiro-cli"
    }
    fn label(&self) -> &str {
        "KIRO"
    }

    fn resume_command(&self, _source_id: &str) -> Option<ResumeCommand> {
        None
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let db_path = kiro_db_path()?;

        if !db_path.exists() {
            debug!("Kiro CLI DB not found at {}, skipping", db_path.display());
            return Ok(vec![]);
        }

        let conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;

        let has_table = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='conversations_v2'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .is_ok();
        if !has_table {
            debug!("Kiro CLI DB missing conversations_v2 table, skipping");
            return Ok(vec![]);
        }

        let mut stmt = conn.prepare(
            "SELECT key, conversation_id, value, created_at, updated_at
             FROM conversations_v2
             ORDER BY updated_at DESC",
        )?;

        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;

        let mut sessions = Vec::new();

        for row in rows {
            let (cwd, conversation_id, value_json, created_at, updated_at) = match row {
                Ok(t) => t,
                Err(e) => {
                    debug!("failed to read kiro row: {e}");
                    continue;
                }
            };
            match parse_kiro_conversation(
                &conversation_id,
                &cwd,
                &value_json,
                created_at,
                updated_at,
            ) {
                Ok(Some(session)) => sessions.push(session),
                Ok(None) => {}
                Err(e) => {
                    debug!("failed to parse kiro conversation {conversation_id}: {e}");
                }
            }
        }

        Ok(sessions)
    }
}

fn kiro_db_path() -> anyhow::Result<std::path::PathBuf> {
    let data_dir = dirs::data_dir().ok_or_else(|| anyhow::anyhow!("no data dir"))?;
    Ok(data_dir.join("kiro-cli/data.sqlite3"))
}

pub fn parse_kiro_conversation(
    conversation_id: &str,
    cwd: &str,
    value_json: &str,
    created_at: i64,
    updated_at: i64,
) -> anyhow::Result<Option<RawSession>> {
    let doc: Value = serde_json::from_str(value_json)?;

    let history = match doc.get("history").and_then(|h| h.as_array()) {
        Some(arr) => arr,
        None => return Ok(None),
    };

    let mut messages = Vec::new();

    for turn in history {
        if let Some(user_obj) = turn.get("user") {
            let content = extract_user_content(user_obj);
            let timestamp = parse_kiro_timestamp(user_obj.get("timestamp"));
            if !content.is_empty() {
                messages.push(RawMessage { role: Role::User, content, timestamp });
            }
        }

        if let Some(assistant_obj) = turn.get("assistant") {
            let content = extract_assistant_content(assistant_obj);
            let timestamp = turn
                .get("request_metadata")
                .and_then(|m| m.get("request_start_timestamp_ms"))
                .and_then(|t| t.as_i64());
            if !content.is_empty() {
                messages.push(RawMessage { role: Role::Assistant, content, timestamp });
            }
        }
    }

    if messages.is_empty() {
        return Ok(None);
    }

    Ok(Some(RawSession {
        source_id: conversation_id.to_string(),
        directory: Some(cwd.to_string()),
        started_at: created_at,
        updated_at: Some(updated_at),
        entrypoint: None,
        messages,
    }))
}

fn extract_user_content(user_obj: &Value) -> String {
    let content = match user_obj.get("content") {
        Some(c) => c,
        None => return String::new(),
    };

    if let Some(prompt_obj) = content.get("Prompt")
        && let Some(text) = prompt_obj.get("prompt").and_then(|p| p.as_str())
    {
        return text.to_string();
    }

    if let Some(tool_results) = content.get("ToolUseResults")
        && let Some(arr) = tool_results.get("tool_use_results").and_then(|v| v.as_array())
    {
        let mut parts = Vec::new();
        for result in arr {
            let Some(inner) = result.get("content").and_then(|c| c.as_array()) else {
                continue;
            };
            for item in inner {
                if let Some(text) = item.get("Text").and_then(|t| t.as_str()) {
                    parts.push(text.to_string());
                } else if let Some(json_val) = item.get("Json")
                    && let Ok(s) = serde_json::to_string(json_val)
                {
                    parts.push(s);
                }
            }
        }
        if !parts.is_empty() {
            return parts.join("\n");
        }
    }

    String::new()
}

fn extract_assistant_content(assistant_obj: &Value) -> String {
    if let Some(response) = assistant_obj.get("Response")
        && let Some(text) = response.get("content").and_then(|c| c.as_str())
    {
        return text.to_string();
    }

    if let Some(tool_use) = assistant_obj.get("ToolUse") {
        let mut parts = Vec::new();
        if let Some(prose) = tool_use.get("content").and_then(|c| c.as_str())
            && !prose.is_empty()
        {
            parts.push(prose.to_string());
        }
        if let Some(tool_uses) = tool_use.get("tool_uses").and_then(|v| v.as_array()) {
            for tu in tool_uses {
                let name = tu.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                let args = tu
                    .get("args")
                    .map(|a| serde_json::to_string(a).unwrap_or_default())
                    .unwrap_or_default();
                parts.push(format!("[{name}] {args}"));
            }
        }
        if !parts.is_empty() {
            return parts.join("\n");
        }
    }

    String::new()
}

fn parse_kiro_timestamp(ts: Option<&Value>) -> Option<i64> {
    ts.and_then(|t| t.as_str())
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|dt| dt.timestamp_millis())
}
