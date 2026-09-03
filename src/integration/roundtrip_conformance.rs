use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::adapters::{RawSession, claude_code, codex, kimi_code};
use crate::db::schema;
use crate::db::search::TimeRange;
use crate::db::store::Store;
use crate::export::{ExportIncludes, ExportOptions, write_jsonl as write_export_jsonl};
use crate::project_scope::ProjectScope;
use crate::sync::persist_raw_session_for_conformance;

fn assert_roundtrip<F>(source: &str, parse: F)
where
    F: Fn() -> Result<RawSession>,
{
    schema::register_sqlite_vec();
    let raw = parse().unwrap();
    let expected = expected_contract(source, &raw);
    let store =
        persist_raw_session_for_conformance(Store::open_in_memory().unwrap(), source, raw).unwrap();
    let first = export_record(&store).unwrap();
    assert_eq!(actual_contract(&first), expected, "{source} initial round-trip");

    let first_id = first["session"]["id"].as_str().unwrap().to_string();
    assert!(!first_id.is_empty(), "{source} local id is empty");

    let mut refreshed = parse().unwrap();
    refreshed.updated_at =
        Some(refreshed.updated_at.unwrap_or(refreshed.started_at).saturating_add(1));
    let refreshed_expected = expected_contract(source, &refreshed);
    let store = persist_raw_session_for_conformance(store, source, refreshed).unwrap();
    let second = export_record(&store).unwrap();

    assert_eq!(second["session"]["id"], first_id, "{source} local id changed on refresh");
    assert_eq!(actual_contract(&second), refreshed_expected, "{source} refresh round-trip");
}

fn expected_contract(source: &str, raw: &RawSession) -> Value {
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
    let events = raw
        .events
        .iter()
        .map(|event| {
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

fn actual_contract(record: &Value) -> Value {
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

fn export_record(store: &Store) -> Result<Value> {
    let options = ExportOptions {
        session_ids: Vec::new(),
        sources: None,
        time_range: TimeRange::All,
        scope: ProjectScope::Global,
        thread_role: None,
        limit: None,
        includes: ExportIncludes::full(),
    };
    let mut output = Vec::new();
    write_export_jsonl(store, &options, &mut output)?;
    let records = String::from_utf8(output)?
        .lines()
        .map(serde_json::from_str)
        .collect::<serde_json::Result<Vec<Value>>>()?;
    anyhow::ensure!(records.len() == 1, "expected one export record");
    Ok(records.into_iter().next().unwrap())
}

fn write_jsonl(path: &Path, lines: &[Value]) {
    let mut file = fs::File::create(path).unwrap();
    for line in lines {
        writeln!(file, "{line}").unwrap();
    }
}

#[test]
fn codex_single_file_round_trip() {
    let root = tempfile::tempdir().unwrap();
    let sessions = root.path().join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let source_id = "019a4c01-e8f4-7270-bdab-7f19273b2501";
    let path = sessions.join(format!("rollout-2026-04-13T10-00-00-{source_id}.jsonl"));
    write_jsonl(
        &path,
        &[
            json!({
                "type": "session_meta",
                "payload": {
                    "id": source_id,
                    "timestamp": "2026-04-13T10:00:00Z",
                    "cwd": "/fixture/codex"
                }
            }),
            json!({
                "type": "event_msg",
                "timestamp": "2026-04-13T10:00:01Z",
                "payload": {"type": "user_message", "message": "Trace the persistence path"}
            }),
            json!({
                "type": "event_msg",
                "timestamp": "2026-04-13T10:00:02Z",
                "payload": {"type": "agent_message", "message": "The mapping is intact."}
            }),
            json!({
                "type": "response_item",
                "timestamp": "2026-04-13T10:00:03Z",
                "payload": {
                    "type": "function_call",
                    "name": "read_file",
                    "arguments": "{\"path\":\"src/db/events.rs\"}",
                    "call_id": "call_conformance"
                }
            }),
            json!({
                "type": "response_item",
                "timestamp": "2026-04-13T10:00:04Z",
                "payload": {
                    "type": "function_call_output",
                    "call_id": "call_conformance",
                    "output": "event store mapping"
                }
            }),
        ],
    );

    assert_roundtrip("codex", || {
        codex::parse_codex_session_with_options(&path, true)?
            .context("Codex fixture was not parsed")
    });
}

#[test]
fn kimi_composite_files_round_trip() {
    let root = tempfile::tempdir().unwrap();
    let session_dir = root.path().join("wd_fixture").join("session_kimi_contract");
    let wire_dir = session_dir.join("agents").join("main");
    fs::create_dir_all(&wire_dir).unwrap();
    fs::write(
        session_dir.join("state.json"),
        json!({
            "id": "session_kimi_contract",
            "cwd": "/fixture/kimi",
            "createdAt": 1_700_000_000_000_i64,
            "title": "Composite session summary",
            "isCustomTitle": false
        })
        .to_string(),
    )
    .unwrap();
    let wire_path = wire_dir.join("wire.jsonl");
    write_jsonl(
        &wire_path,
        &[
            json!({
                "type": "context.append_message",
                "time": 1_700_000_001_000_i64,
                "message": {
                    "role": "user",
                    "origin": {"kind": "user"},
                    "content": [{"type": "text", "text": "Read both Kimi files"}]
                }
            }),
            json!({
                "type": "llm.request",
                "time": 1_700_000_002_000_i64,
                "model": "kimi-k3",
                "provider": "moonshot"
            }),
            json!({
                "type": "context.append_loop_event",
                "time": 1_700_000_003_000_i64,
                "event": {
                    "type": "content.part",
                    "part": {"type": "text", "text": "Both files agree."}
                }
            }),
            json!({
                "type": "usage.record",
                "time": 1_700_000_004_000_i64,
                "model": "kimi-k3",
                "usage": {
                    "inputOther": 120,
                    "output": 30,
                    "inputCacheRead": 20,
                    "inputCacheCreation": 10
                }
            }),
        ],
    );

    assert_roundtrip("kimi-code", || {
        kimi_code::parse_conformance_fixture(root.path())?.context("Kimi fixture was not parsed")
    });
}

#[test]
fn claude_events_and_topology_round_trip() {
    let root = tempfile::tempdir().unwrap();
    let transcript_dir =
        root.path().join("projects").join("fixture").join("parent-claude").join("subagents");
    fs::create_dir_all(&transcript_dir).unwrap();
    let path = transcript_dir.join("agent-claude.jsonl");
    write_jsonl(
        &path,
        &[
            json!({
                "type": "custom-title",
                "customTitle": "Inspect persistence mappings"
            }),
            json!({
                "type": "summary",
                "summary": "Tool events retained"
            }),
            json!({
                "type": "user",
                "sessionId": "parent-claude",
                "cwd": "/fixture/claude",
                "timestamp": "2026-04-13T10:00:00Z",
                "message": {"content": "Inspect the session store"}
            }),
            json!({
                "type": "assistant",
                "sessionId": "parent-claude",
                "requestId": "req-contract",
                "timestamp": "2026-04-13T10:00:01Z",
                "message": {
                    "id": "msg-contract",
                    "model": "claude-sonnet-4-5",
                    "content": [
                        {"type": "text", "text": "Reading the store."},
                        {
                            "type": "tool_use",
                            "id": "tool-contract",
                            "name": "Read",
                            "input": {"path": "src/db/session_store.rs"}
                        }
                    ],
                    "usage": {
                        "input_tokens": 100,
                        "output_tokens": 20,
                        "cache_read_input_tokens": 40,
                        "cache_creation_input_tokens": 5
                    }
                }
            }),
            json!({
                "type": "user",
                "sessionId": "parent-claude",
                "timestamp": "2026-04-13T10:00:02Z",
                "message": {
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "tool-contract",
                            "content": "mapping body"
                        }
                    ]
                }
            }),
        ],
    );

    assert_roundtrip("claude-code", || {
        claude_code::parse_conformance_fixture(root.path())?
            .context("Claude fixture was not parsed")
    });
}
