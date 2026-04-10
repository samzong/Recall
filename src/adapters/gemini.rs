use std::fs;

use serde_json::Value;
use tracing::debug;
use walkdir::WalkDir;

use crate::adapters::{RawMessage, RawSession, SourceAdapter};
use crate::types::Role;

pub struct GeminiAdapter;

impl SourceAdapter for GeminiAdapter {
    fn id(&self) -> &str {
        "gemini-cli"
    }
    fn label(&self) -> &str {
        "GEM"
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home dir"))?;

        // Gemini CLI stores sessions under ~/.gemini/tmp/{project}/chats/
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
            // Only parse files inside a "chats" directory
            if !path.parent().is_some_and(|p| p.file_name().is_some_and(|n| n == "chats")) {
                continue;
            }

            match parse_gemini_session(path) {
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

fn parse_gemini_session(path: &std::path::Path) -> anyhow::Result<Option<RawSession>> {
    let content = fs::read_to_string(path)?;
    let doc: Value = serde_json::from_str(&content)?;

    let session_id = doc
        .get("sessionId")
        .and_then(|s| s.as_str())
        .unwrap_or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
        })
        .to_string();

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

    // Derive project directory from path: ~/.gemini/tmp/{project}/chats/session.json
    let directory = path
        .parent() // chats/
        .and_then(|p| p.parent()) // {project}/
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .map(String::from);

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

        // User messages have content as array of {text}, assistant has content as string
        let content = match role {
            Role::User => extract_user_content(msg.get("content")),
            Role::Assistant => msg
                .get("content")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string(),
        };

        if content.is_empty() {
            continue;
        }

        messages.push(RawMessage {
            role,
            content,
            timestamp,
        });
    }

    if messages.is_empty() {
        return Ok(None);
    }

    Ok(Some(RawSession {
        source_id: session_id,
        directory,
        started_at,
        updated_at,
        entrypoint: None,
        messages,
    }))
}

/// Extract text from user content array: [{"text": "..."}, ...]
fn extract_user_content(content: Option<&Value>) -> String {
    match content {
        Some(Value::Array(arr)) => {
            let parts: Vec<&str> = arr
                .iter()
                .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
                .collect();
            parts.join("\n")
        }
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}
