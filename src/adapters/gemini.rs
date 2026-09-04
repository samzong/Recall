use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use tracing::debug;
use walkdir::WalkDir;

use crate::adapters::json_util::rfc3339_ms;
use crate::adapters::paths;
use crate::adapters::usage::usage_count;
use crate::adapters::{RawMessage, RawSession, ResumeCommand, SourceAdapter};
use crate::types::{RawUsageEvent, Role};

pub(crate) struct GeminiAdapter;

const USAGE_PARSER_VERSION: u32 = 3;

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

    fn start_command(&self, prompt: String) -> Option<ResumeCommand> {
        Some(crate::adapters::prompt_start("gemini", prompt))
    }

    fn usage_parser_version(&self) -> Option<u32> {
        Some(USAGE_PARSER_VERSION)
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let Some(root) = resolve_gemini_root()? else {
            return Ok(vec![]);
        };

        let mut sessions = Vec::new();
        for path in collect_gemini_session_files(&root) {
            match parse_gemini_session_file(&path) {
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

fn resolve_gemini_root() -> anyhow::Result<Option<PathBuf>> {
    if let Some(dir) = paths::env_path_dir("GEMINI_HOME") {
        if dir.is_dir() {
            return Ok(Some(dir));
        }
        debug!("GEMINI_HOME not found, skipping Gemini CLI");
        return Ok(None);
    }
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home dir"))?;
    let gemini_tmp = home.join(".gemini/tmp");
    if !gemini_tmp.exists() {
        debug!("~/.gemini/tmp not found, skipping Gemini CLI");
        return Ok(None);
    }
    Ok(Some(gemini_tmp))
}

fn collect_gemini_session_files(root: &Path) -> Vec<PathBuf> {
    let mut by_stem: std::collections::HashMap<(PathBuf, String), PathBuf> =
        std::collections::HashMap::new();
    for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.parent().is_none_or(|parent| parent.file_name().is_none_or(|name| name != "chats"))
        {
            continue;
        }
        let ext = path.extension().and_then(|ext| ext.to_str()).map(str::to_ascii_lowercase);
        if ext.as_deref() != Some("json") && ext.as_deref() != Some("jsonl") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let key = (path.parent().unwrap_or(root).to_path_buf(), stem.to_string());
        match by_stem.get(&key) {
            Some(existing)
                if existing.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
                    && ext.as_deref() == Some("json") =>
            {
                continue;
            }
            _ => {}
        }
        by_stem.insert(key, path.to_path_buf());
    }
    by_stem.into_values().collect()
}

fn parse_gemini_session_file(path: &Path) -> anyhow::Result<Option<RawSession>> {
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let is_jsonl = path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"));
    let doc = if is_jsonl { replay_gemini_jsonl(reader) } else { serde_json::from_reader(reader)? };
    let fallback_id = path.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown");
    parse_gemini_session_value(doc, fallback_id, Some(path.display().to_string()))
}

fn replay_gemini_jsonl(reader: impl BufRead) -> Value {
    let mut session = Map::new();
    let mut messages = Vec::new();
    for line in reader.lines() {
        let Ok(line) = line else {
            break;
        };
        let trimmed = line.trim().trim_start_matches('\u{feff}');
        if trimmed.is_empty() {
            continue;
        }
        let Ok(Value::Object(mut object)) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let fields = match object.remove("$set") {
            Some(Value::Object(fields)) => fields,
            Some(_) => continue,
            None if object.get("kind").and_then(Value::as_str) == Some("main") => object,
            None => {
                if object.get("type").and_then(Value::as_str).is_some() {
                    messages.push(Value::Object(object));
                }
                continue;
            }
        };
        for (key, value) in fields {
            if key == "messages" {
                if let Value::Array(snapshot) = value {
                    messages = snapshot;
                }
            } else {
                session.insert(key, value);
            }
        }
    }
    session.insert("messages".to_string(), Value::Array(messages));
    Value::Object(session)
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

    let started_at = rfc3339_ms(doc.get("startTime")).unwrap_or(0);
    let updated_at = rfc3339_ms(doc.get("lastUpdated"));

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
            "gemini" | "model" => Role::Assistant,
            _ => continue,
        };

        let timestamp = rfc3339_ms(msg.get("timestamp"));

        let prose = gemini_text(msg.get("content"));

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

        if matches!(msg_type, "gemini" | "model")
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
    session.source_file_path = source_path;
    if !usage_events.is_empty() {
        session = session.with_usage(usage_events, USAGE_PARSER_VERSION);
    }
    Ok(Some(session))
}

fn gemini_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.trim().to_string(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| match item {
                Value::String(text) => Some(text.as_str()),
                Value::Object(object) => object.get("text").and_then(Value::as_str),
                _ => None,
            })
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn extract_gemini_usage_event(
    msg: &Value,
    event_seq: u32,
    message_seq: u32,
    timestamp: i64,
    source_path: Option<&str>,
) -> Option<RawUsageEvent> {
    let tokens = msg.get("tokens")?;
    let output_tokens = usage_count(tokens, &["output"]);
    let cache_read_tokens = usage_count(tokens, &["cached"]);
    let reasoning_tokens = usage_count(tokens, &["thoughts"]);
    let input_tokens = usage_count(tokens, &["input"]).saturating_sub(cache_read_tokens);
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
        assert_eq!(event.input_tokens, 70);
        assert_eq!(event.output_tokens, 20);
        assert_eq!(event.cache_read_tokens, 30);
        assert_eq!(event.reasoning_tokens, 5);
        assert_eq!(event.token_source, crate::types::TokenSource::Observed);
    }

    #[test]
    fn parse_gemini_session_file_sets_source_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        std::fs::write(
            &path,
            r#"{"sessionId":"abc-123","messages":[{"type":"user","content":"hello"}]}"#,
        )
        .unwrap();

        let session = parse_gemini_session_file(&path).unwrap().unwrap();

        assert_eq!(session.source_file_path.as_deref(), path.to_str());
    }

    #[test]
    fn parse_gemini_session_accepts_model_type_and_array_content() {
        let json = r#"{
            "sessionId": "model-1",
            "messages": [
                {"type":"user","content":[{"text":"hello"}]},
                {"type":"model","content":[{"text":"hi"}],"tokens":{"input":10,"output":2}}
            ]
        }"#;
        let session = parse_gemini_session(json, "fallback").unwrap().unwrap();
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].content, "hello");
        assert_eq!(session.messages[1].role, Role::Assistant);
        assert_eq!(session.messages[1].content, "hi");
        assert_eq!(session.usage_events.len(), 1);
        assert_eq!(session.usage_parser_version, Some(USAGE_PARSER_VERSION));
    }

    #[test]
    fn parse_gemini_jsonl_replays_set_patches_and_bare_messages() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-abc.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"kind":"main","sessionId":"abc"}"#,
                "\n",
                r#"{"$set":{"startTime":"2025-11-13T13:48:00.000Z"}}"#,
                "\n",
                r#"{"type":"user","content":"hello"}"#,
                "\n",
                r#"{"type":"gemini","content":"there"}"#,
                "\n",
            ),
        )
        .unwrap();
        let session = parse_gemini_session_file(&path).unwrap().unwrap();
        assert_eq!(session.source_id, "abc");
        assert_eq!(session.messages[0].content, "hello");
        assert_eq!(session.messages[1].content, "there");
        assert_eq!(
            session.started_at,
            rfc3339_ms(Some(&Value::String("2025-11-13T13:48:00.000Z".into()))).unwrap()
        );
    }

    #[test]
    fn collect_gemini_session_files_keeps_same_stem_in_different_projects() {
        let dir = tempfile::tempdir().unwrap();
        for project in ["proj-a", "proj-b"] {
            let chats = dir.path().join(project).join("chats");
            std::fs::create_dir_all(&chats).unwrap();
            std::fs::write(chats.join("wip.json"), "{}").unwrap();
        }
        let files = collect_gemini_session_files(dir.path());
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn collect_gemini_session_files_prefers_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let chats = dir.path().join("proj").join("chats");
        std::fs::create_dir_all(&chats).unwrap();
        std::fs::write(chats.join("session-1.json"), "{}").unwrap();
        std::fs::write(chats.join("session-1.jsonl"), "{}\n").unwrap();
        let files = collect_gemini_session_files(dir.path());
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].extension().and_then(|ext| ext.to_str()), Some("jsonl"));
    }
}
