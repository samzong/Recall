use serde_json::{Value, json};

use crate::adapters::RawSession;

pub(super) fn expected_contract(source: &str, raw: &RawSession) -> Value {
    let user_messages = raw
        .messages
        .iter()
        .filter(|message| message.role.as_str() == "user")
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>();
    let generated_title = crate::utils::title_from_user_messages(&user_messages);
    let title =
        raw.custom_title.as_deref().filter(|title| !title.is_empty()).unwrap_or(&generated_title);
    let messages = raw
        .messages
        .iter()
        .enumerate()
        .map(|(seq, message)| {
            json!({
                "seq": seq as u32,
                "role": message.role.as_str(),
                "timestamp": message.timestamp,
                "content": message.content,
            })
        })
        .collect::<Vec<_>>();
    let usage_events = raw
        .usage_events
        .iter()
        .map(|event| {
            json!({
                "event_key": event.event_key,
                "event_seq": event.event_seq,
                "message_seq": event.message_seq,
                "timestamp": event.timestamp,
                "model": event.model,
                "provider": event.provider,
                "input_tokens": event.input_tokens,
                "output_tokens": event.output_tokens,
                "cache_read_tokens": event.cache_read_tokens,
                "cache_write_tokens": event.cache_write_tokens,
                "reasoning_tokens": event.reasoning_tokens,
                "token_source": event.token_source.as_str(),
                "parser_version": event.parser_version,
                "source_path": event.source_path,
                "raw_usage_json": event.raw_usage_json,
            })
        })
        .collect::<Vec<_>>();
    let mut identities = crate::repo_identity::RepoIdentityCache::default();
    let events = raw
        .events
        .iter()
        .map(|event| {
            let mut files = event.files.clone();
            for file in &mut files {
                file.target = identities.resolve_file(&file.path, file.cwd.as_deref()).unwrap();
            }
            json!({
                "event_seq": event.event_seq,
                "timestamp": event.timestamp,
                "kind": event.kind,
                "actor": event.actor,
                "name": event.name,
                "status": event.status,
                "target": event.target,
                "message_seq": event.message_seq,
                "summary": event.summary,
                "source_path": event.source_path,
                "source_event_id": event.source_event_id,
                "tool_call_id": event.tool_call_id,
                "is_meta": event.is_meta,
                "visibility": event.visibility,
                "attrs_json": event.attrs_json,
                "files": files,
                "command_evidence_status": event.command_evidence_status,
                "parser_version": event.parser_version,
            })
        })
        .collect::<Vec<_>>();

    json!({
        "session": {
            "source": source,
            "source_id": raw.source_id,
            "title": title,
            "directory": raw.directory,
            "started_at": raw.started_at,
            "updated_at": raw.updated_at,
            "message_count": raw.messages.len() as u32,
            "entrypoint": raw.entrypoint,
            "custom_title": raw.custom_title,
            "summary": raw.summary,
            "duration_minutes": raw.duration_minutes,
            "source_file_path": raw.source_file_path,
            "topology": {
                "thread_role": raw.thread_role,
                "parents": raw.parent_links,
            },
        },
        "messages": messages,
        "usage_events": usage_events,
        "events": events,
    })
}

pub(super) fn actual_contract(record: &Value) -> Value {
    let session = &record["session"];
    json!({
        "session": {
            "source": session["source"],
            "source_id": session["source_id"],
            "title": session["title"],
            "directory": session["directory"],
            "started_at": session["started_at"],
            "updated_at": session["updated_at"],
            "message_count": session["message_count"],
            "entrypoint": session["entrypoint"],
            "custom_title": session["custom_title"],
            "summary": session["summary"],
            "duration_minutes": session["duration_minutes"],
            "source_file_path": session["source_file_path"],
            "topology": session["topology"],
        },
        "messages": record["messages"],
        "usage_events": record["usage_events"],
        "events": record["events"],
    })
}
