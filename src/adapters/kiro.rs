use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use tracing::debug;

use crate::adapters::{RawMessage, RawSession, SourceAdapter};
use crate::types::Role;

pub struct KiroAdapter;

impl SourceAdapter for KiroAdapter {
    fn id(&self) -> &str {
        "kiro-cli"
    }
    fn label(&self) -> &str {
        "KIRO"
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let db_path = kiro_db_path()?;

        if !db_path.exists() {
            debug!("Kiro CLI DB not found at {}, skipping", db_path.display());
            return Ok(vec![]);
        }

        let conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;

        // conversations_v2 schema:
        //   key TEXT (cwd path), conversation_id TEXT, value TEXT (JSON),
        //   created_at INTEGER (ms), updated_at INTEGER (ms)
        let mut stmt = conn.prepare(
            "SELECT key, conversation_id, value, created_at, updated_at
             FROM conversations_v2
             ORDER BY updated_at DESC",
        )?;

        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })?
            .filter_map(|r| r.ok());

        let mut sessions = Vec::new();

        for (cwd, conversation_id, value_json, created_at, updated_at) in rows {
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
                    debug!("failed to parse kiro conversation {}: {e}", conversation_id);
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

fn parse_kiro_conversation(
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
        // Each turn has "user" and "assistant" objects
        if let Some(user_obj) = turn.get("user") {
            let content = extract_user_content(user_obj);
            let timestamp = parse_kiro_timestamp(user_obj.get("timestamp"));

            if !content.is_empty() {
                messages.push(RawMessage { role: Role::User, content, timestamp });
            }
        }

        if let Some(assistant_obj) = turn.get("assistant") {
            let content = extract_assistant_content(assistant_obj);
            // Assistant timestamp from request_metadata
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

/// Extract user prompt text from Kiro user object.
/// Content can be: {"Prompt": {"prompt": "..."}} or {"ToolResult": {...}}
fn extract_user_content(user_obj: &Value) -> String {
    let content = match user_obj.get("content") {
        Some(c) => c,
        None => return String::new(),
    };

    // Most common: {"Prompt": {"prompt": "..."}}
    if let Some(prompt_obj) = content.get("Prompt")
        && let Some(text) = prompt_obj.get("prompt").and_then(|p| p.as_str())
    {
        return text.to_string();
    }

    // Tool result: {"ToolResult": {"tool_use_id": "...", "content": [...]}}
    if let Some(tool_result) = content.get("ToolResult") {
        let tool_id = tool_result.get("tool_use_id").and_then(|t| t.as_str()).unwrap_or("tool");
        if let Some(arr) = tool_result.get("content").and_then(|c| c.as_array()) {
            let parts: Vec<String> = arr
                .iter()
                .filter_map(|item| {
                    if let Some(text_obj) = item.get("text") {
                        text_obj.get("text").and_then(|t| t.as_str()).map(String::from)
                    } else {
                        None
                    }
                })
                .collect();
            if !parts.is_empty() {
                return format!("[{tool_id}] {}", parts.join("\n"));
            }
        }
    }

    String::new()
}

/// Extract assistant response text from Kiro assistant object.
/// Can be: {"Response": {"content": "..."}} or {"ToolUse": {...}}
fn extract_assistant_content(assistant_obj: &Value) -> String {
    // Response: {"Response": {"content": "..."}}
    if let Some(response) = assistant_obj.get("Response")
        && let Some(text) = response.get("content").and_then(|c| c.as_str())
    {
        return text.to_string();
    }

    // ToolUse: {"ToolUse": {"name": "...", "input": {...}}}
    if let Some(tool_use) = assistant_obj.get("ToolUse") {
        let name = tool_use.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
        if let Some(input) = tool_use.get("input") {
            return format!("[{name}] {input}");
        }
    }

    String::new()
}

fn parse_kiro_timestamp(ts: Option<&Value>) -> Option<i64> {
    ts.and_then(|t| t.as_str())
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|dt| dt.timestamp_millis())
}
