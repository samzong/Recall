use std::fs;
use std::path::Path;

use serde_json::Value;
use tracing::warn;
use walkdir::WalkDir;

use crate::adapters::{RawMessage, RawSession, SourceAdapter};
use crate::types::Role;

pub struct CodexAdapter;

impl SourceAdapter for CodexAdapter {
    fn id(&self) -> &str {
        "codex"
    }
    fn label(&self) -> &str {
        "CDX"
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home dir"))?;
        let codex_dir = home.join(".codex");
        if !codex_dir.exists() {
            warn!("~/.codex not found, skipping Codex");
            return Ok(vec![]);
        }

        let mut sessions = Vec::new();

        let sessions_dir = codex_dir.join("sessions");
        if sessions_dir.exists() {
            sessions.extend(scan_dir(&sessions_dir));
        }

        let archived_dir = codex_dir.join("archived_sessions");
        if archived_dir.exists() {
            sessions.extend(scan_dir(&archived_dir));
        }

        Ok(sessions)
    }
}

fn scan_dir(dir: &Path) -> Vec<RawSession> {
    let mut sessions = Vec::new();

    for entry in WalkDir::new(dir).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        let ext = path.extension().and_then(|e| e.to_str());
        if ext != Some("jsonl") && ext != Some("json") {
            continue;
        }
        if !path.is_file() {
            continue;
        }

        match parse_codex_session(path) {
            Ok(Some(session)) => sessions.push(session),
            Ok(None) => {}
            Err(e) => {
                warn!("failed to parse codex session {}: {e}", path.display());
            }
        }
    }

    sessions
}

fn parse_codex_session(path: &Path) -> anyhow::Result<Option<RawSession>> {
    let content = fs::read_to_string(path)?;
    let mut meta_id: Option<String> = None;
    let mut meta_cwd: Option<String> = None;
    let mut meta_timestamp: Option<i64> = None;
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

        let msg_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");

        match msg_type {
            "session_meta" => {
                if let Some(payload) = v.get("payload") {
                    meta_id = payload.get("id").and_then(|s| s.as_str()).map(String::from);
                    meta_cwd = payload.get("cwd").and_then(|s| s.as_str()).map(String::from);
                    meta_timestamp = payload
                        .get("timestamp")
                        .and_then(|t| t.as_str())
                        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                        .map(|dt| dt.timestamp_millis());
                }
            }
            "event_msg" => {
                if let Some(payload) = v.get("payload") {
                    let payload_type = payload.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    match payload_type {
                        "user_message" => {
                            if let Some(text) = payload.get("message").and_then(|m| m.as_str())
                                && !text.is_empty()
                            {
                                let ts = parse_timestamp(&v);
                                messages.push(RawMessage {
                                    role: Role::User,
                                    content: text.to_string(),
                                    timestamp: ts,
                                });
                            }
                        }
                        "agent_message" => {
                            if let Some(text) = payload.get("message").and_then(|m| m.as_str())
                                && !text.is_empty()
                            {
                                let ts = parse_timestamp(&v);
                                messages.push(RawMessage {
                                    role: Role::Assistant,
                                    content: text.to_string(),
                                    timestamp: ts,
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
            "response_item" => {
                if let Some(payload) = v.get("payload")
                    && payload.get("type").and_then(|t| t.as_str()) == Some("message")
                    && payload.get("role").and_then(|r| r.as_str()) == Some("assistant")
                {
                    let text = extract_content_array(payload.get("content"));
                    if !text.is_empty() {
                        let ts = parse_timestamp(&v);
                        messages.push(RawMessage {
                            role: Role::Assistant,
                            content: text,
                            timestamp: ts,
                        });
                    }
                }
            }
            _ => {}
        }
    }

    if messages.is_empty() {
        return Ok(None);
    }

    let source_id = meta_id.unwrap_or_else(|| {
        path.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown").to_string()
    });

    let started_at =
        meta_timestamp.or_else(|| messages.first().and_then(|m| m.timestamp)).unwrap_or(0);

    Ok(Some(RawSession {
        source_id,
        directory: meta_cwd,
        started_at,
        updated_at: messages.last().and_then(|m| m.timestamp),
        entrypoint: None,
        messages,
    }))
}

fn extract_content_array(content: Option<&Value>) -> String {
    match content {
        Some(Value::Array(arr)) => {
            let mut parts = Vec::new();
            for item in arr {
                match item.get("type").and_then(|t| t.as_str()) {
                    Some("text" | "output_text") => {
                        if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                            parts.push(text.to_string());
                        }
                    }
                    Some("function_call") => {
                        let name = item.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                        if let Some(args) = item.get("arguments").and_then(|a| a.as_str()) {
                            parts.push(format!("[{name}] {args}"));
                        }
                    }
                    Some("function_call_output") => {
                        if let Some(output) = item.get("output").and_then(|o| o.as_str()) {
                            parts.push(output.to_string());
                        }
                    }
                    _ => {}
                }
            }
            parts.join("\n")
        }
        _ => String::new(),
    }
}

fn parse_timestamp(v: &Value) -> Option<i64> {
    v.get("timestamp")
        .and_then(|t| t.as_str())
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|dt| dt.timestamp_millis())
}
