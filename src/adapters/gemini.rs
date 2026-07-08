use std::fs;
use std::io::BufReader;
use std::path::Path;

use serde_json::Value;
use tracing::debug;
use walkdir::WalkDir;

use crate::adapters::usage::usage_count;
use crate::adapters::{RawMessage, RawSession, ResumeCommand, SourceAdapter};
use crate::types::{RawUsageEvent, Role};

pub(crate) struct GeminiAdapter;

const USAGE_PARSER_VERSION: u32 = 1;

impl SourceAdapter for GeminiAdapter {
    fn id(&self) -> &str {
        "gemini-cli"
    }
    fn label(&self) -> &str {
        "GEM"
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        Some(ResumeCommand {
            program: "gemini".to_string(),
            args: vec!["--resume".to_string(), source_id.to_string()],
        })
    }

    fn usage_parser_version(&self) -> Option<u32> {
        Some(USAGE_PARSER_VERSION)
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
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let doc: Value = serde_json::from_reader(reader)?;
    let fallback_id = path.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown");
    parse_gemini_session_value(doc, fallback_id, Some(path.display().to_string()))
}

#[cfg(test)]
pub(crate) fn parse_gemini_session(
    json: &str,
    fallback_id: &str,
) -> anyhow::Result<Option<RawSession>> {
    let doc: Value = serde_json::from_str(json)?;
    parse_gemini_session_value(doc, fallback_id, None)
}

fn parse_gemini_session_value(
    doc: Value,
    fallback_id: &str,
    source_path: Option<String>,
) -> anyhow::Result<Option<RawSession>> {
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
    let mut usage_events = Vec::new();

    for (index, msg) in messages_arr.iter().enumerate() {
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

        let message_seq = messages.len() as u32;
        messages.push(RawMessage { role, content, timestamp });

        if msg_type == "gemini"
            && let Some(event) = extract_gemini_usage_event(
                msg,
                index as u32,
                message_seq,
                timestamp.unwrap_or(started_at),
                source_path.as_deref(),
            )
        {
            usage_events.push(event);
        }
    }

    if messages.is_empty() && usage_events.is_empty() {
        return Ok(None);
    }

    let mut session =
        RawSession::search_only(session_id, None, started_at, updated_at, None, messages);
    if !usage_events.is_empty() {
        session = session.with_usage(usage_events, USAGE_PARSER_VERSION);
    }
    Ok(Some(session))
}

fn extract_gemini_usage_event(
    msg: &Value,
    event_seq: u32,
    message_seq: u32,
    timestamp: i64,
    source_path: Option<&str>,
) -> Option<RawUsageEvent> {
    let tokens = msg.get("tokens")?;
    let input_tokens = usage_count(tokens, &["input"]);
    let output_tokens = usage_count(tokens, &["output"]);
    let cache_read_tokens = usage_count(tokens, &["cached"]);
    let reasoning_tokens = usage_count(tokens, &["thoughts"]);
    if input_tokens == 0 && output_tokens == 0 && cache_read_tokens == 0 && reasoning_tokens == 0 {
        return None;
    }

    let model = msg
        .get("model")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("unknown")
        .to_string();
    let event_key = msg
        .get("id")
        .map(|value| format!("message:{value}"))
        .unwrap_or_else(|| format!("line:{event_seq}"));

    Some(RawUsageEvent {
        message_seq: Some(message_seq),
        model: model.clone(),
        provider: "google".to_string(),
        input_tokens,
        output_tokens,
        cache_read_tokens,
        reasoning_tokens,
        source_path: source_path.map(str::to_string),
        raw_usage_json: Some(tokens.to_string()),
        ..RawUsageEvent::observed(event_key, event_seq, timestamp, USAGE_PARSER_VERSION)
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_gemini_session_extracts_usage_events() {
        let json = r#"{
            "sessionId": "abc-123",
            "startTime": "2025-11-13T13:48:00.000Z",
            "messages": [
                {"id": 0, "type": "user", "content": "hello", "timestamp": "2025-11-13T13:48:05.000Z"},
                {
                    "id": 1,
                    "type": "gemini",
                    "content": "hi there",
                    "timestamp": "2025-11-13T13:48:10.000Z",
                    "model": "gemini-2.5-pro",
                    "tokens": { "input": 100, "output": 20, "cached": 30, "thoughts": 5 }
                }
            ]
        }"#;

        let session = parse_gemini_session(json, "fallback").unwrap().unwrap();
        assert_eq!(session.usage_events.len(), 1);
        let event = &session.usage_events[0];
        assert_eq!(event.model, "gemini-2.5-pro");
        assert_eq!(event.provider, "google");
        assert_eq!(event.input_tokens, 100);
        assert_eq!(event.output_tokens, 20);
        assert_eq!(event.cache_read_tokens, 30);
        assert_eq!(event.reasoning_tokens, 5);
        assert_eq!(event.token_source, crate::types::TokenSource::Observed);
    }
}
