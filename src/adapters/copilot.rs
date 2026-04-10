use std::collections::HashMap;
use std::fs;

use serde_json::Value;
use tracing::debug;

use crate::adapters::{RawMessage, RawSession, ResumeCommand, SourceAdapter};
use crate::types::Role;

pub struct CopilotAdapter;

impl SourceAdapter for CopilotAdapter {
    fn id(&self) -> &str {
        "copilot-cli"
    }
    fn label(&self) -> &str {
        "CPL"
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        Some(ResumeCommand {
            program: "copilot".to_string(),
            args: vec![format!("--resume={source_id}")],
        })
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home dir"))?;
        let sessions_dir = home.join(".copilot/session-state");
        if !sessions_dir.exists() {
            debug!("~/.copilot/session-state not found, skipping Copilot CLI");
            return Ok(vec![]);
        }

        let entries = match fs::read_dir(&sessions_dir) {
            Ok(e) => e,
            Err(e) => {
                debug!("cannot read {}: {e}", sessions_dir.display());
                return Ok(vec![]);
            }
        };

        let mut sessions = Vec::new();

        for entry in entries.flatten() {
            let session_dir = entry.path();
            if !session_dir.is_dir() {
                continue;
            }
            let events_path = session_dir.join("events.jsonl");
            if !events_path.is_file() {
                continue;
            }
            let fallback_id =
                session_dir.file_name().and_then(|n| n.to_str()).unwrap_or("unknown").to_string();

            let content = match fs::read_to_string(&events_path) {
                Ok(c) => c,
                Err(e) => {
                    debug!("failed to read {}: {e}", events_path.display());
                    continue;
                }
            };

            match parse_copilot_events(&content, &fallback_id) {
                Ok(Some(session)) => sessions.push(session),
                Ok(None) => {}
                Err(e) => {
                    debug!("failed to parse copilot session {}: {e}", events_path.display());
                }
            }
        }

        Ok(sessions)
    }
}

pub fn parse_copilot_events(
    content: &str,
    fallback_id: &str,
) -> anyhow::Result<Option<RawSession>> {
    let mut session_id: Option<String> = None;
    let mut directory: Option<String> = None;
    let mut meta_started_at: Option<i64> = None;
    let mut tool_names: HashMap<String, String> = HashMap::new();
    let mut messages = Vec::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let event_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let timestamp = parse_timestamp(&v);

        match event_type {
            "session.start" => {
                if let Some(data) = v.get("data") {
                    session_id = data.get("sessionId").and_then(|s| s.as_str()).map(String::from);
                    meta_started_at = data
                        .get("startTime")
                        .and_then(|t| t.as_str())
                        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                        .map(|dt| dt.timestamp_millis());
                    directory = data
                        .get("context")
                        .and_then(|c| c.get("cwd"))
                        .and_then(|c| c.as_str())
                        .map(String::from);
                }
            }
            "user.message" => {
                let Some(data) = v.get("data") else { continue };
                let content =
                    data.get("content").and_then(|c| c.as_str()).unwrap_or("").trim().to_string();
                if content.is_empty() {
                    continue;
                }
                messages.push(RawMessage { role: Role::User, content, timestamp });
            }
            "assistant.message" => {
                let Some(data) = v.get("data") else { continue };
                let prose =
                    data.get("content").and_then(|c| c.as_str()).unwrap_or("").trim().to_string();
                let tool_text = extract_tool_requests(data.get("toolRequests"));
                let content = match (prose.is_empty(), tool_text.is_empty()) {
                    (true, true) => continue,
                    (false, true) => prose,
                    (true, false) => tool_text,
                    (false, false) => format!("{prose}\n{tool_text}"),
                };
                messages.push(RawMessage { role: Role::Assistant, content, timestamp });
            }
            "tool.execution_start" => {
                if let Some(data) = v.get("data")
                    && let (Some(id), Some(name)) = (
                        data.get("toolCallId").and_then(|s| s.as_str()),
                        data.get("toolName").and_then(|s| s.as_str()),
                    )
                {
                    tool_names.insert(id.to_string(), name.to_string());
                }
            }
            "tool.execution_complete" => {
                let Some(data) = v.get("data") else { continue };
                let Some(result) = data.get("result") else { continue };
                let text = result
                    .get("detailedContent")
                    .and_then(|c| c.as_str())
                    .or_else(|| result.get("content").and_then(|c| c.as_str()))
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if text.is_empty() {
                    continue;
                }
                let tool_name = data
                    .get("toolCallId")
                    .and_then(|s| s.as_str())
                    .and_then(|id| tool_names.get(id).cloned())
                    .unwrap_or_else(|| "tool".to_string());
                messages.push(RawMessage {
                    role: Role::Assistant,
                    content: format!("[{tool_name}] {text}"),
                    timestamp,
                });
            }
            _ => {}
        }
    }

    if messages.is_empty() {
        return Ok(None);
    }

    let source_id = session_id.unwrap_or_else(|| fallback_id.to_string());
    let started_at =
        meta_started_at.or_else(|| messages.first().and_then(|m| m.timestamp)).unwrap_or(0);
    let updated_at = messages.last().and_then(|m| m.timestamp);

    Ok(Some(RawSession {
        source_id,
        directory,
        started_at,
        updated_at,
        entrypoint: None,
        messages,
    }))
}

fn extract_tool_requests(tool_requests: Option<&Value>) -> String {
    let Some(arr) = tool_requests.and_then(|v| v.as_array()) else {
        return String::new();
    };

    let mut parts = Vec::new();
    for req in arr {
        let name = req.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
        let args = req
            .get("arguments")
            .map(|a| serde_json::to_string(a).unwrap_or_default())
            .unwrap_or_default();
        parts.push(format!("[{name}] {args}"));
    }
    parts.join("\n")
}

fn parse_timestamp(v: &Value) -> Option<i64> {
    v.get("timestamp")
        .and_then(|t| t.as_str())
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|dt| dt.timestamp_millis())
}
