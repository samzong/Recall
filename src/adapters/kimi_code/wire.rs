use std::path::Path;

use serde_json::Value;

use super::{EVENT_PARSER_VERSION, StateMeta, USAGE_PARSER_VERSION};
use crate::adapters::events::{EventContext, tool_call_event, tool_result_event};
use crate::adapters::json_util::{json_i64, jsonl_indexed};
use crate::adapters::usage::usage_count;
use crate::adapters::{RawMessage, RawSession, first_timestamp};
use crate::types::{FileEvidence, FileOperation, RawSessionEvent, RawUsageEvent, Role};

pub(super) fn parse_kimi_wire(
    meta: StateMeta,
    session_id: String,
    lines: impl Iterator<Item = std::io::Result<String>>,
    mtime_ms: i64,
    directory: Option<String>,
    source_path: Option<String>,
    include_events: bool,
) -> anyhow::Result<Option<RawSession>> {
    let directory = meta.cwd.clone().or(directory);
    let mut messages = Vec::new();
    let mut events: Vec<RawSessionEvent> = Vec::new();
    let mut usage_events: Vec<RawUsageEvent> = Vec::new();
    let mut last_provider: Option<String> = None;

    for item in jsonl_indexed(lines) {
        let (line_index, v) = item?;
        let line_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let time = json_i64(v.get("time"));

        match line_type {
            "llm.request" => {
                if let Some(provider) =
                    v.get("provider").and_then(|p| p.as_str()).filter(|p| !p.is_empty())
                {
                    last_provider = Some(provider.to_string());
                }
            }
            "context.append_message" => {
                let origin = v.pointer("/message/origin/kind").and_then(|k| k.as_str());
                let role = match v.pointer("/message/role").and_then(|r| r.as_str()) {
                    Some("user") if origin == Some("user") => Role::User,
                    Some("assistant") => Role::Assistant,
                    _ => continue,
                };
                let text = extract_text_parts(v.pointer("/message/content"));
                if text.is_empty() {
                    continue;
                }
                messages.push(RawMessage { role, content: text, timestamp: time });
            }
            "context.append_loop_event" => {
                let event = v.get("event");
                let event_type = event.and_then(|e| e.get("type")).and_then(|t| t.as_str());
                if include_events
                    && let Some(event) = event
                    && let Some(raw) = extract_tool_event(
                        &v,
                        event,
                        EventContext {
                            event_seq: events.len() as u32,
                            timestamp: time,
                            source_path: source_path.clone(),
                            source_event_id: Some(
                                event
                                    .get("uuid")
                                    .and_then(Value::as_str)
                                    .filter(|id| !id.is_empty())
                                    .map(str::to_string)
                                    .unwrap_or_else(|| format!("wire:{line_index}")),
                            ),
                            message_seq: messages.len().checked_sub(1).map(|seq| seq as u32),
                            parser_version: EVENT_PARSER_VERSION,
                        },
                        directory.as_deref(),
                    )
                {
                    events.push(raw);
                }
                let part = event.and_then(|e| e.get("part"));
                let part_type = part.and_then(|p| p.get("type")).and_then(|t| t.as_str());
                if event_type != Some("content.part") || part_type != Some("text") {
                    continue;
                }
                let text = part
                    .and_then(|p| p.get("text"))
                    .and_then(|t| t.as_str())
                    .map(str::trim)
                    .unwrap_or("");
                if text.is_empty() {
                    continue;
                }
                messages.push(RawMessage {
                    role: Role::Assistant,
                    content: text.to_string(),
                    timestamp: time,
                });
            }
            "usage.record" => {
                if let Some(event) = extract_usage_event(
                    &v,
                    line_index,
                    time.unwrap_or(mtime_ms),
                    last_provider.as_deref(),
                    source_path.as_deref(),
                ) {
                    usage_events.push(event);
                }
            }
            _ => {}
        }
    }

    if messages.is_empty() && usage_events.is_empty() && events.is_empty() {
        return Ok(None);
    }

    let started_at =
        first_timestamp(meta.created_at, &messages, &usage_events, &events).unwrap_or(0);
    let duration_minutes = match (
        first_timestamp(None, &messages, &usage_events, &events),
        crate::adapters::last_timestamp(None, &messages, &usage_events, &events),
    ) {
        (Some(first), Some(last)) if last >= first => Some(((last - first) / 60_000) as u32),
        _ => None,
    };
    let (custom_title, summary) =
        if meta.is_custom_title { (meta.title, None) } else { (None, meta.title) };

    let mut session =
        RawSession::search_only(session_id, directory, started_at, Some(mtime_ms), None, messages);
    session.source_file_path = source_path;
    session.custom_title = custom_title;
    session.summary = summary;
    session.duration_minutes = duration_minutes;
    session = session.with_usage(usage_events, USAGE_PARSER_VERSION);
    if include_events {
        session = session.with_events(events, EVENT_PARSER_VERSION);
    }
    Ok(Some(session))
}

fn extract_tool_event(
    record: &Value,
    event: &Value,
    context: EventContext,
    cwd: Option<&str>,
) -> Option<RawSessionEvent> {
    let mut raw = match event.get("type")?.as_str()? {
        "tool.call" => {
            let name = event.get("name")?.as_str()?;
            let args = event.get("args");
            let path = args
                .and_then(|args| args.get("path"))
                .and_then(Value::as_str)
                .filter(|path| !path.trim().is_empty());
            let operation = match name {
                "Read" => Some(FileOperation::Read),
                "Write" | "Edit" => Some(FileOperation::Write),
                _ => None,
            };
            let mut call = tool_call_event(context, name.to_string(), args);
            call.kind = "tool_call".to_string();
            call.target = None;
            if let Some(operation) = operation
                && let Some(path) = path
            {
                call.kind = match operation {
                    FileOperation::Read => "file_read",
                    _ => "file_write",
                }
                .to_string();
                call.target = Some(path.to_string());
                call.files.push(FileEvidence::call(
                    path.to_string(),
                    operation,
                    cwd.map(str::to_string),
                ));
            } else if name == "Bash" {
                call.kind = "command".to_string();
                call.target = args
                    .and_then(|args| args.get("command"))
                    .and_then(Value::as_str)
                    .filter(|command| !command.trim().is_empty())
                    .map(str::to_string);
                if let Some(command) = call.target.as_deref() {
                    let shell_cwd = match args.and_then(|args| args.get("cwd")) {
                        Some(value) => value.as_str(),
                        None => cwd,
                    }
                    .filter(|cwd| Path::new(cwd).is_absolute());
                    let (files, status) =
                        crate::adapters::events::shell_file_evidence(command, shell_cwd);
                    call.files = files;
                    call.command_evidence_status = Some(status);
                }
            }
            call
        }
        "tool.result" => {
            let result = event.get("result");
            let mut result_event = tool_result_event(
                context,
                None,
                result.and_then(|result| result.get("output")).map(|output| match output {
                    Value::String(text) => text.to_string(),
                    value => value.to_string(),
                }),
            );
            result_event.status = result
                .and_then(|result| result.get("isError"))
                .and_then(Value::as_bool)
                .map(|is_error| if is_error { "error" } else { "success" }.to_string());
            result_event
        }
        _ => return None,
    };
    raw.tool_call_id = event
        .get("toolCallId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string);
    raw.attrs_json = Some(record.to_string());
    Some(raw)
}

fn extract_text_parts(content: Option<&Value>) -> String {
    let Some(parts) = content.and_then(|c| c.as_array()) else {
        return String::new();
    };
    let text = parts
        .iter()
        .filter(|part| part.get("type").and_then(|t| t.as_str()) == Some("text"))
        .filter_map(|part| part.get("text").and_then(|t| t.as_str()))
        .collect::<Vec<_>>()
        .join("\n");
    text.trim().to_string()
}

fn extract_usage_event(
    v: &Value,
    line_index: usize,
    timestamp: i64,
    provider: Option<&str>,
    source_path: Option<&str>,
) -> Option<RawUsageEvent> {
    let usage = v.get("usage")?;
    let input_tokens = usage_count(usage, &["inputOther"]);
    let output_tokens = usage_count(usage, &["output"]);
    let cache_read_tokens = usage_count(usage, &["inputCacheRead"]);
    let cache_write_tokens = usage_count(usage, &["inputCacheCreation"]);
    if input_tokens == 0 && output_tokens == 0 && cache_read_tokens == 0 && cache_write_tokens == 0
    {
        return None;
    }

    let model = v
        .get("model")
        .and_then(|m| m.as_str())
        .filter(|m| !m.trim().is_empty())
        .unwrap_or("unknown")
        .to_string();
    Some(RawUsageEvent {
        model,
        provider: provider.unwrap_or("unknown").to_string(),
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_write_tokens,
        source_path: source_path.map(str::to_string),
        raw_usage_json: Some(usage.to_string()),
        ..RawUsageEvent::observed(
            format!("wire:{line_index}"),
            line_index as u32,
            timestamp,
            USAGE_PARSER_VERSION,
        )
    })
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::super::parse_kimi_session;
    use super::super::tests::{fixture_state, fixture_wire};
    use super::{EVENT_PARSER_VERSION, USAGE_PARSER_VERSION};
    use crate::types::{FileEvidenceKind, FileOperation, Role};

    #[test]
    fn parse_kimi_session_extracts_messages_and_usage() {
        let session = parse_kimi_session(fixture_state(), fixture_wire(), "session_abc").unwrap();

        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, Role::User);
        assert_eq!(session.messages[0].content, "fix the bug");
        assert_eq!(session.messages[1].role, Role::Assistant);
        assert_eq!(session.messages[1].content, "Here is the fix.");

        assert_eq!(session.directory.as_deref(), Some("/repo"));
        assert_eq!(session.started_at, 1700000000000);
        assert_eq!(session.summary.as_deref(), Some("fix the bug"));
        assert_eq!(session.custom_title, None);
        assert_eq!(session.duration_minutes, Some(0));

        assert_eq!(session.usage_events.len(), 1);
        let event = &session.usage_events[0];
        assert_eq!(event.model, "kimi-k3");
        assert_eq!(event.provider, "moonshot");
        assert_eq!(event.input_tokens, 1200);
        assert_eq!(event.output_tokens, 80);
        assert_eq!(event.cache_read_tokens, 300);
        assert_eq!(event.cache_write_tokens, 40);
        assert_eq!(event.token_source, crate::types::TokenSource::Observed);
        assert_eq!(session.usage_parser_version, Some(USAGE_PARSER_VERSION));
        assert_eq!(session.event_parser_version, Some(EVENT_PARSER_VERSION));
        assert_eq!(session.events.len(), 2);
        let call = &session.events[0];
        assert_eq!(call.source_event_id.as_deref(), Some("call-uuid"));
        assert_eq!(call.tool_call_id.as_deref(), Some("Edit:1"));
        assert_eq!(call.timestamp, Some(1700000005500));
        assert_eq!(call.message_seq, Some(1));
        assert_eq!(call.files.len(), 1);
        assert_eq!(call.files[0].path, "DEVELOPMENT.md");
        assert_eq!(call.files[0].cwd.as_deref(), Some("/repo"));
        assert_eq!(call.files[0].operation, FileOperation::Write);
        assert_eq!(call.files[0].kind, FileEvidenceKind::Call);
        let attrs: Value = serde_json::from_str(call.attrs_json.as_deref().unwrap()).unwrap();
        assert_eq!(attrs.pointer("/event/args/old_string"), Some(&Value::from("old")));
        assert_eq!(attrs.pointer("/event/args/new_string"), Some(&Value::from("new")));
        let result = &session.events[1];
        assert_eq!(result.tool_call_id, call.tool_call_id);
        assert_eq!(result.message_seq, Some(1));
        assert_eq!(result.status, None);
        assert!(result.files.is_empty());
        let attrs: Value = serde_json::from_str(result.attrs_json.as_deref().unwrap()).unwrap();
        assert_eq!(attrs.pointer("/event/parentUuid"), Some(&Value::from("call-uuid")));
        assert_eq!(
            attrs.pointer("/event/result/output"),
            Some(&Value::from("Replaced 1 occurrence in DEVELOPMENT.md"))
        );
    }

    #[test]
    fn parse_kimi_session_empty_wire_returns_none() {
        let wire = r#"{"type":"metadata","protocol_version":"1.5","created_at":1700000000100}"#;
        assert!(parse_kimi_session(fixture_state(), wire, "session_abc").is_none());
        for (name, operation) in [
            ("Read", Some(FileOperation::Read)),
            ("Write", Some(FileOperation::Write)),
            ("DeleteFile", None),
            ("CustomEdit", None),
        ] {
            let record = serde_json::json!({
                "type": "context.append_loop_event", "time": 1700000001000_i64,
                "event": {"type": "tool.call", "uuid": "uuid", "toolCallId": "call",
                    "name": name, "args": {"path": " spaced file.rs ", "content": "code"}}
            })
            .to_string();
            let session = parse_kimi_session(fixture_state(), &record, "session_abc").unwrap();
            assert!(session.messages.is_empty());
            assert!(session.usage_events.is_empty());
            let event = &session.events[0];
            if let Some(operation) = operation {
                assert_eq!(event.files[0].path, " spaced file.rs ");
                assert_eq!(event.files[0].operation, operation);
            } else {
                assert!(event.files.is_empty());
                assert_eq!(event.kind, "tool_call");
                assert_eq!(event.target, None);
            }
        }
        let record = serde_json::json!({
            "type": "context.append_loop_event", "event": {
                "type": "tool.call", "name": "Bash", "args": {
                    "command": "git restore -- src/lib.rs", "cwd": "/other/repo"
                }
            }
        });
        let session =
            parse_kimi_session(fixture_state(), &record.to_string(), "session_abc").unwrap();
        assert_eq!(session.events[0].files[0].cwd.as_deref(), Some("/other/repo"));
        for (flag, status) in [
            (Value::Bool(true), Some("error")),
            (Value::Bool(false), Some("success")),
            (Value::Null, None),
        ] {
            let output = "result".repeat(1000);
            let record = serde_json::json!({
                "type": "context.append_loop_event", "event": {
                    "type": "tool.result", "parentUuid": "missing-call", "toolCallId": "orphan",
                    "result": {"output": output, "isError": flag}
                }
            })
            .to_string();
            let session = parse_kimi_session(fixture_state(), &record, "session_abc").unwrap();
            let event = &session.events[0];
            assert_eq!(event.status.as_deref(), status);
            assert!(event.files.is_empty());
            assert!(event.summary.as_ref().unwrap().len() < output.len());
            let attrs: Value = serde_json::from_str(event.attrs_json.as_deref().unwrap()).unwrap();
            assert_eq!(attrs.pointer("/event/result/output"), Some(&Value::from(output)));
        }
    }

    #[test]
    fn parse_kimi_session_invalid_state_returns_none() {
        for state in ["{", "[]"] {
            assert!(parse_kimi_session(state, fixture_wire(), "session_abc").is_none());
        }
    }
}
