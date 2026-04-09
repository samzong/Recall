use std::collections::HashMap;
use std::fs;
use std::path::Path;

use serde_json::Value;
use tracing::debug;
use walkdir::WalkDir;

use crate::adapters::{RawMessage, RawSession, SourceAdapter};
use crate::types::Role;

pub struct ClaudeCodeAdapter;

impl SourceAdapter for ClaudeCodeAdapter {
    fn id(&self) -> &str {
        "claude-code"
    }
    fn label(&self) -> &str {
        "CC"
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home dir"))?;
        let claude_dir = home.join(".claude");
        if !claude_dir.exists() {
            debug!("~/.claude not found, skipping Claude Code");
            return Ok(vec![]);
        }

        let session_index = load_session_index(&claude_dir);
        let mut sessions = Vec::new();

        sessions.extend(scan_projects(&claude_dir, &session_index));
        sessions.extend(scan_transcripts(&claude_dir));

        Ok(sessions)
    }
}

struct SessionMeta {
    cwd: Option<String>,
    started_at: i64,
    entrypoint: Option<String>,
}

fn load_session_index(claude_dir: &Path) -> HashMap<String, SessionMeta> {
    let sessions_dir = claude_dir.join("sessions");
    let mut index = HashMap::new();
    if !sessions_dir.exists() {
        return index;
    }

    let entries = match fs::read_dir(&sessions_dir) {
        Ok(e) => e,
        Err(e) => {
            debug!("cannot read ~/.claude/sessions: {e}");
            return index;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let v: Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(session_id) = v.get("sessionId").and_then(|s| s.as_str()) {
            let meta = SessionMeta {
                cwd: v.get("cwd").and_then(|s| s.as_str()).map(|s| s.to_string()),
                started_at: v.get("startedAt").and_then(|s| s.as_i64()).unwrap_or(0),
                entrypoint: v.get("entrypoint").and_then(|s| s.as_str()).map(|s| s.to_string()),
            };
            index.insert(session_id.to_string(), meta);
        }
    }
    index
}

fn scan_projects(
    claude_dir: &Path,
    session_index: &HashMap<String, SessionMeta>,
) -> Vec<RawSession> {
    let projects_dir = claude_dir.join("projects");
    if !projects_dir.exists() {
        return vec![];
    }

    let mut sessions = Vec::new();

    let project_dirs = match fs::read_dir(&projects_dir) {
        Ok(e) => e,
        Err(e) => {
            debug!("cannot read ~/.claude/projects: {e}");
            return vec![];
        }
    };

    for project_entry in project_dirs.flatten() {
        let project_path = project_entry.path();
        if !project_path.is_dir() {
            continue;
        }

        let dir_name = project_entry.file_name().to_string_lossy().to_string();
        let directory = project_key_to_path(&dir_name);

        let jsonl_files = match fs::read_dir(&project_path) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for file_entry in jsonl_files.flatten() {
            let file_path = file_entry.path();
            if file_path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }

            let session_id =
                file_path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();

            let meta = session_index.get(&session_id);

            let messages = match parse_conversation_jsonl(&file_path) {
                Ok(m) => m,
                Err(e) => {
                    debug!("failed to parse {}: {e}", file_path.display());
                    continue;
                }
            };

            if messages.is_empty() {
                continue;
            }

            let started_at = meta
                .map(|m| m.started_at)
                .or_else(|| messages.first().and_then(|m| m.timestamp))
                .unwrap_or(0);

            sessions.push(RawSession {
                source_id: session_id,
                directory: meta.and_then(|m| m.cwd.clone()).or_else(|| Some(directory.clone())),
                started_at,
                updated_at: messages.last().and_then(|m| m.timestamp),
                entrypoint: meta.and_then(|m| m.entrypoint.clone()),
                messages,
            });
        }
    }

    sessions
}

fn scan_transcripts(claude_dir: &Path) -> Vec<RawSession> {
    let transcripts_dir = claude_dir.join("transcripts");
    if !transcripts_dir.exists() {
        return vec![];
    }

    let mut sessions = Vec::new();

    for entry in WalkDir::new(&transcripts_dir).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }

        let session_id = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();

        let messages = match parse_conversation_jsonl(path) {
            Ok(m) => m,
            Err(e) => {
                debug!("failed to parse transcript {}: {e}", path.display());
                continue;
            }
        };

        if messages.is_empty() {
            continue;
        }

        let started_at = messages.first().and_then(|m| m.timestamp).unwrap_or(0);

        sessions.push(RawSession {
            source_id: session_id,
            directory: None,
            started_at,
            updated_at: messages.last().and_then(|m| m.timestamp),
            entrypoint: None,
            messages,
        });
    }

    sessions
}

fn parse_conversation_jsonl(path: &Path) -> anyhow::Result<Vec<RawMessage>> {
    let content = fs::read_to_string(path)?;
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
            "user" | "assistant" => {}
            _ => continue,
        }

        let role = if msg_type == "user" { Role::User } else { Role::Assistant };

        let message = match v.get("message") {
            Some(m) => m,
            None => continue,
        };

        let text = extract_content(message.get("content"));
        if text.is_empty() {
            continue;
        }

        let timestamp = v
            .get("timestamp")
            .and_then(|t| t.as_str())
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map(|dt| dt.timestamp_millis());

        messages.push(RawMessage { role, content: text, timestamp });
    }

    Ok(messages)
}

fn extract_content(content: Option<&Value>) -> String {
    match content {
        None => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => {
            let mut parts = Vec::new();
            for item in arr {
                match item.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                            parts.push(text.to_string());
                        }
                    }
                    Some("tool_use") => {
                        let name = item.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                        if let Some(input) = item.get("input") {
                            parts.push(format!("[{name}] {input}"));
                        }
                    }
                    Some("tool_result") => {
                        if let Some(content) = item.get("content") {
                            match content {
                                Value::String(s) => parts.push(s.clone()),
                                Value::Array(inner) => {
                                    for block in inner {
                                        if block.get("type").and_then(|t| t.as_str())
                                            == Some("text")
                                            && let Some(text) =
                                                block.get("text").and_then(|t| t.as_str())
                                        {
                                            parts.push(text.to_string());
                                        }
                                    }
                                }
                                _ => {}
                            }
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

fn project_key_to_path(key: &str) -> String {
    let key = key.strip_prefix('-').unwrap_or(key);
    let mut result = String::with_capacity(key.len() + 1);
    result.push('/');
    let mut chars = key.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '-' {
            if chars.peek() == Some(&'-') {
                chars.next();
                result.push_str("/.");
            } else {
                result.push('/');
            }
        } else {
            result.push(c);
        }
    }
    result
}
