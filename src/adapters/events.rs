use serde_json::Value;

use crate::types::RawSessionEvent;

pub(crate) struct EventContext {
    pub(crate) event_seq: u32,
    pub(crate) timestamp: Option<i64>,
    pub(crate) source_path: Option<String>,
    pub(crate) source_event_id: Option<String>,
    pub(crate) message_seq: Option<u32>,
    pub(crate) parser_version: u32,
}

pub(crate) fn tool_call_event(
    context: EventContext,
    name: String,
    args: Option<&Value>,
) -> RawSessionEvent {
    let target = args.and_then(target_from_value);
    let kind = infer_tool_kind(&name, target.as_deref());
    let summary = args.map(|value| match value {
        Value::String(text) if text.trim().is_empty() => format!("[{name}]"),
        Value::String(text) => format!("[{name}] {text}"),
        other => format!("[{name}] {other}"),
    });
    RawSessionEvent {
        event_seq: context.event_seq,
        timestamp: context.timestamp,
        kind: kind.to_string(),
        actor: "assistant".to_string(),
        name: Some(name),
        status: None,
        target,
        message_seq: context.message_seq,
        summary,
        source_path: context.source_path,
        source_event_id: context.source_event_id,
        attrs_json: args.map(|value| value.to_string()),
        parser_version: context.parser_version,
    }
}

pub(crate) fn tool_call_event_from_text(
    context: EventContext,
    name: String,
    args: Option<&str>,
) -> RawSessionEvent {
    let parsed = args.and_then(|text| serde_json::from_str::<Value>(text).ok());
    let target =
        parsed.as_ref().and_then(target_from_value).or_else(|| command_target(&name, args));
    let kind = infer_tool_kind(&name, target.as_deref());
    let summary = args.map(|text| {
        if text.trim().is_empty() { format!("[{name}]") } else { format!("[{name}] {text}") }
    });
    RawSessionEvent {
        event_seq: context.event_seq,
        timestamp: context.timestamp,
        kind: kind.to_string(),
        actor: "assistant".to_string(),
        name: Some(name),
        status: None,
        target,
        message_seq: context.message_seq,
        summary,
        source_path: context.source_path,
        source_event_id: context.source_event_id,
        attrs_json: parsed.map(|value| value.to_string()),
        parser_version: context.parser_version,
    }
}

pub(crate) fn tool_result_event(
    context: EventContext,
    name: Option<String>,
    summary: Option<String>,
) -> RawSessionEvent {
    RawSessionEvent {
        event_seq: context.event_seq,
        timestamp: context.timestamp,
        kind: "tool_result".to_string(),
        actor: "tool".to_string(),
        name,
        status: None,
        target: None,
        message_seq: context.message_seq,
        summary,
        source_path: context.source_path,
        source_event_id: context.source_event_id,
        attrs_json: None,
        parser_version: context.parser_version,
    }
}

pub(crate) fn target_from_value(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => non_empty(text),
        Value::Array(values) => command_target_from_array(values),
        Value::Object(map) => {
            for key in [
                "path",
                "file_path",
                "filePath",
                "target",
                "command",
                "cmd",
                "query",
                "pattern",
                "glob",
                "glob_pattern",
                "regex",
                "url",
            ] {
                if let Some(text) = map.get(key).and_then(target_from_value) {
                    return Some(text);
                }
            }
            None
        }
        _ => None,
    }
}

fn command_target_from_array(values: &[Value]) -> Option<String> {
    let parts: Option<Vec<&str>> = values.iter().map(|value| value.as_str()).collect();
    let parts = parts?;
    if parts.is_empty() {
        return None;
    }
    if parts.len() >= 3
        && matches!(parts[0], "bash" | "sh" | "zsh")
        && matches!(parts[1], "-c" | "-lc")
    {
        return non_empty(parts[2]);
    }
    non_empty(&parts.join(" "))
}

fn command_target(name: &str, args: Option<&str>) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    if lower.contains("bash") || lower.contains("shell") || lower.contains("exec") {
        return args.and_then(non_empty);
    }
    None
}

fn infer_tool_kind(name: &str, target: Option<&str>) -> &'static str {
    let lower = name.to_ascii_lowercase();
    if lower.contains("bash") || lower.contains("shell") || lower.contains("exec") {
        return "command";
    }
    if lower.contains("grep")
        || lower.contains("search")
        || lower.contains("glob")
        || lower.contains("find")
    {
        return "search";
    }
    if target.is_some()
        && (lower.contains("edit")
            || lower.contains("write")
            || lower.contains("patch")
            || lower.contains("delete"))
    {
        return "file_write";
    }
    if target.is_some()
        && (lower.contains("read") || lower.contains("open") || lower.contains("view"))
    {
        return "file_read";
    }
    "tool_call"
}

fn non_empty(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
}
