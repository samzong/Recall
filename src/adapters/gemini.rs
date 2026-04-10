use std::fs;
use std::path::Path;

use serde_json::Value;
use tracing::debug;
use walkdir::WalkDir;

use crate::adapters::{RawMessage, RawSession, ResumeCommand, SourceAdapter};
use crate::types::Role;

pub struct GeminiAdapter;

impl SourceAdapter for GeminiAdapter {
    fn id(&self) -> &str {
        "gemini-cli"
    }
    fn label(&self) -> &str {
        "GEM"
    }

    fn resume_command(&self, _source_id: &str) -> Option<ResumeCommand> {
        None
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home dir"))?;

        let gemini_tmp = home.join(".gemini/tmp");
        if !gemini_tmp.exists() {
            debug!("~/.gemini/tmp not found, skipping Gemini CLI");
            return Ok(vec![]);
        }

        let mut sessions = Vec::new();

        for entry in WalkDir::new(&gemini_tmp).into_iter().filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if !path.is_file() {
                continue;
            }
            if path.parent().is_none_or(|p| p.file_name().is_none_or(|n| n != "chats")) {
                continue;
            }

            match parse_gemini_session_file(path) {
                Ok(Some(session)) => sessions.push(session),
                Ok(None) => {}
                Err(e) => {
                    debug!("failed to parse gemini session {}: {e}", path.display());
                }
            }
        }

        Ok(sessions)
    }
}

fn parse_gemini_session_file(path: &Path) -> anyhow::Result<Option<RawSession>> {
    let content = fs::read_to_string(path)?;
    let fallback_id = path.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown");
    parse_gemini_session(&content, fallback_id)
}

pub fn parse_gemini_session(json: &str, fallback_id: &str) -> anyhow::Result<Option<RawSession>> {
    let doc: Value = serde_json::from_str(json)?;

    let session_id =
        doc.get("sessionId").and_then(|s| s.as_str()).unwrap_or(fallback_id).to_string();

    let started_at = doc
        .get("startTime")
        .and_then(|t| t.as_str())
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|dt| dt.timestamp_millis())
        .unwrap_or(0);

    let updated_at = doc
        .get("lastUpdated")
        .and_then(|t| t.as_str())
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|dt| dt.timestamp_millis());

    let messages_arr = match doc.get("messages").and_then(|m| m.as_array()) {
        Some(arr) => arr,
        None => return Ok(None),
    };

    let mut messages = Vec::new();

    for msg in messages_arr {
        let msg_type = msg.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let role = match msg_type {
            "user" => Role::User,
            "gemini" => Role::Assistant,
            _ => continue,
        };

        let timestamp = msg
            .get("timestamp")
            .and_then(|t| t.as_str())
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map(|dt| dt.timestamp_millis());

        let prose = msg.get("content").and_then(|c| c.as_str()).unwrap_or("").trim().to_string();

        let tool_text = if matches!(role, Role::Assistant) {
            extract_tool_calls(msg.get("toolCalls"))
        } else {
            String::new()
        };

        let content = match (prose.is_empty(), tool_text.is_empty()) {
            (true, true) => continue,
            (false, true) => prose,
            (true, false) => tool_text,
            (false, false) => format!("{prose}\n{tool_text}"),
        };

        messages.push(RawMessage { role, content, timestamp });
    }

    if messages.is_empty() {
        return Ok(None);
    }

    Ok(Some(RawSession {
        source_id: session_id,
        directory: None,
        started_at,
        updated_at,
        entrypoint: None,
        messages,
    }))
}

fn extract_tool_calls(tool_calls: Option<&Value>) -> String {
    let Some(arr) = tool_calls.and_then(|v| v.as_array()) else {
        return String::new();
    };

    let mut parts = Vec::new();
    for call in arr {
        let name = call.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
        let args = call
            .get("args")
            .map(|a| serde_json::to_string(a).unwrap_or_default())
            .unwrap_or_default();
        let result_text = extract_tool_result(call.get("result"));

        let mut part = format!("[{name}] {args}");
        if !result_text.is_empty() {
            part.push_str(" -> ");
            part.push_str(&result_text);
        }
        parts.push(part);
    }
    parts.join("\n")
}

fn extract_tool_result(result: Option<&Value>) -> String {
    let Some(arr) = result.and_then(|v| v.as_array()) else {
        return String::new();
    };

    let mut parts = Vec::new();
    for item in arr {
        if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
            parts.push(text.to_string());
        }
    }
    parts.join("\n")
}
