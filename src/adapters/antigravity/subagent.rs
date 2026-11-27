use serde_json::Value;

use crate::adapters::RawMessage;
use crate::types::Role;

pub(super) fn parse_antigravity_system_message(content: &str) -> Option<(&str, &str)> {
    let msg_start = if content.starts_with("[Message] ") {
        0
    } else {
        content.find("\n[Message] ").map(|idx| idx + 1)?
    };
    let msg = &content[msg_start + "[Message] ".len()..];
    let content_pos = msg.find(" content=")?;
    let header = &msg[..content_pos];
    let body = &msg[content_pos + " content=".len()..];
    let sender = header
        .split_whitespace()
        .find_map(|token| token.strip_prefix("sender="))
        .filter(|sender| !sender.is_empty())?;
    Some((sender, body))
}

pub(super) fn extract_antigravity_system_body(body: &str) -> String {
    let unwrapped = body.trim_end().strip_suffix("</SYSTEM_MESSAGE>").unwrap_or(body).trim();
    let extracted = extract_original_task(unwrapped);
    super::extract_user_request(extracted)
}

pub(super) fn collect_send_message_reports(
    v: &Value,
    parent_id: &str,
    inline_content: &str,
    timestamp: Option<i64>,
    messages: &mut Vec<RawMessage>,
) {
    for call in v.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
        if call.get("name").and_then(Value::as_str) != Some("send_message") {
            continue;
        }
        let Some(args) = call.get("args") else {
            continue;
        };
        let Some(recipient) = unwrap_antigravity_string(args.get("Recipient")) else {
            continue;
        };
        if recipient.trim() != parent_id {
            continue;
        }
        let Some(message) = unwrap_antigravity_string(args.get("Message")) else {
            continue;
        };
        let message = message.trim();
        if !message.is_empty() && message != inline_content {
            messages.push(RawMessage {
                role: Role::Assistant,
                content: message.to_string(),
                timestamp,
            });
        }
    }
}

fn extract_original_task(content: &str) -> &str {
    let open = "<original_task>";
    let close = "</original_task>";
    let Some(start) = content.find(open) else {
        return content;
    };
    let task_start = start + open.len();
    let mut cursor = task_start;
    let mut depth = 1usize;
    while depth > 0 {
        let rest = &content[cursor..];
        let Some(next_close) = rest.find(close) else {
            return content;
        };
        if let Some(next_open) = rest[..next_close].find(open) {
            depth += 1;
            cursor += next_open + open.len();
        } else {
            depth -= 1;
            if depth == 0 {
                return content[task_start..cursor + next_close].trim();
            }
            cursor += next_close + close.len();
        }
    }
    content
}

fn unwrap_antigravity_string(value: Option<&Value>) -> Option<String> {
    let Value::String(s) = value? else {
        return None;
    };
    Some(serde_json::from_str::<String>(s).unwrap_or_else(|_| s.to_string()))
}
