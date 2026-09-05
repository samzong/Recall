use serde_json::Value;

use crate::types::{FileEvidence, FileEvidenceKind, FileOperation, RawSessionEvent};

const TOOL_RESULT_SUMMARY_MAX_BYTES: usize = 4096;

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
        files: Vec::new(),
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
        tool_call_id: None,
        is_meta: None,
        visibility: None,
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
        files: Vec::new(),
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
        tool_call_id: None,
        is_meta: None,
        visibility: None,
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
        files: Vec::new(),
        event_seq: context.event_seq,
        timestamp: context.timestamp,
        kind: "tool_result".to_string(),
        actor: "tool".to_string(),
        name,
        status: None,
        target: None,
        message_seq: context.message_seq,
        summary: summary.map(bounded_summary),
        source_path: context.source_path,
        source_event_id: context.source_event_id,
        tool_call_id: None,
        is_meta: None,
        visibility: None,
        attrs_json: None,
        parser_version: context.parser_version,
    }
}

pub(crate) fn file_write_event(
    context: EventContext,
    name: String,
    target: String,
) -> RawSessionEvent {
    let summary = format!("[{name}] {target}");
    RawSessionEvent {
        files: Vec::new(),
        event_seq: context.event_seq,
        timestamp: context.timestamp,
        kind: "file_write".to_string(),
        actor: "assistant".to_string(),
        name: Some(name),
        status: None,
        target: Some(target),
        message_seq: context.message_seq,
        summary: Some(summary),
        source_path: context.source_path,
        source_event_id: context.source_event_id,
        tool_call_id: None,
        is_meta: None,
        visibility: None,
        attrs_json: None,
        parser_version: context.parser_version,
    }
}

pub(crate) fn patch_file_evidence(text: &str) -> Vec<FileEvidence> {
    let text = text.trim();
    if text.lines().next() != Some("*** Begin Patch")
        || text.lines().last() != Some("*** End Patch")
    {
        return Vec::new();
    }
    let mut files: Vec<FileEvidence> = Vec::new();
    let mut update = None;
    for line in text.lines() {
        let parsed = if let Some(path) = line.strip_prefix("*** Update File: ") {
            update = Some(files.len());
            Some((path, FileOperation::Write))
        } else if let Some(path) = line.strip_prefix("*** Add File: ") {
            update = None;
            Some((path, FileOperation::Write))
        } else if let Some(path) = line.strip_prefix("*** Delete File: ") {
            update = None;
            Some((path, FileOperation::Delete))
        } else if let Some(path) =
            line.strip_prefix("*** Move to: ").filter(|path| !path.trim().is_empty())
        {
            if let Some(index) = update.take() {
                if let Some(file) = files.get_mut(index) {
                    file.operation = FileOperation::MoveFrom;
                    Some((path, FileOperation::MoveTo))
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        if let Some((path, operation)) = parsed
            && !path.trim().is_empty()
        {
            files.push(FileEvidence {
                path: path.into(),
                operation,
                kind: FileEvidenceKind::Call,
                cwd: None,
                target: None,
            });
        }
    }
    files
}

const PATCH_FILE_PREFIXES: [&str; 4] =
    ["*** Add File: ", "*** Update File: ", "*** Delete File: ", "*** Move to: "];

pub(crate) fn patch_file_targets(text: &str) -> Vec<String> {
    let mut targets = Vec::new();
    for line in text.lines() {
        let path = PATCH_FILE_PREFIXES
            .iter()
            .find_map(|prefix| line.trim_start().strip_prefix(prefix))
            .and_then(non_empty);
        if let Some(path) = path
            && !targets.contains(&path)
        {
            targets.push(path);
        }
    }
    targets
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

pub(crate) fn bounded_summary(summary: String) -> String {
    if summary.len() <= TOOL_RESULT_SUMMARY_MAX_BYTES {
        return summary;
    }
    let mut end = TOOL_RESULT_SUMMARY_MAX_BYTES;
    while !summary.is_char_boundary(end) {
        end -= 1;
    }
    let mut capped = summary[..end].to_string();
    capped.push('…');
    capped
}

#[cfg(test)]
mod tests {
    use super::{EventContext, TOOL_RESULT_SUMMARY_MAX_BYTES, tool_result_event};

    fn context() -> EventContext {
        EventContext {
            event_seq: 0,
            timestamp: None,
            source_path: None,
            source_event_id: None,
            message_seq: None,
            parser_version: 1,
        }
    }

    #[test]
    fn patch_headers_preserve_context_and_incomplete_moves() {
        let files = super::patch_file_evidence(
            "*** Begin Patch\r\n*** Update File: src/main.rs\r\n*** Move to:   \r\n@@\r\n *** Update File: example.rs\r\n*** End Patch",
        );
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "src/main.rs");
        assert_eq!(files[0].operation, crate::types::FileOperation::Write);
    }

    #[test]
    fn tool_result_summary_is_capped_on_a_char_boundary() {
        let long = "汉".repeat(TOOL_RESULT_SUMMARY_MAX_BYTES);
        let event = tool_result_event(context(), None, Some(long));
        let summary = event.summary.unwrap();
        assert!(summary.ends_with('…'));
        assert!(summary.len() <= TOOL_RESULT_SUMMARY_MAX_BYTES + '…'.len_utf8());
        assert!(summary.trim_end_matches('…').chars().all(|c| c == '汉'));

        let short = "ok".to_string();
        let event = tool_result_event(context(), None, Some(short.clone()));
        assert_eq!(event.summary, Some(short));
    }
}
