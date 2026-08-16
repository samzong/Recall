use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;
use tracing::warn;
use walkdir::WalkDir;

use crate::adapters::file_scan::{self, FileScanEntry};
use crate::adapters::json_util::{json_i64, jsonl_indexed};
use crate::adapters::paths::resolve_home_dir;
use crate::adapters::usage::usage_count;
use crate::adapters::{
    RawMessage, RawSession, ResumeCommand, SourceAdapter, SyncScanResult, SyncScanStats,
    first_timestamp,
};
use crate::db::store::Store;
use crate::types::{RawUsageEvent, Role};

pub(crate) struct KimiCodeAdapter;

const USAGE_PARSER_VERSION: u32 = 1;

impl SourceAdapter for KimiCodeAdapter {
    fn id(&self) -> &str {
        "kimi-code"
    }
    fn label(&self) -> &str {
        "KC"
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        Some(ResumeCommand {
            program: "kimi".to_string(),
            args: vec!["--session".to_string(), source_id.to_string()],
        })
    }

    fn usage_parser_version(&self) -> Option<u32> {
        Some(USAGE_PARSER_VERSION)
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let Some(sessions_dir) = resolve_kimi_dir()? else {
            return Ok(vec![]);
        };

        let mut sessions = Vec::new();
        for entry in collect_session_entries(&sessions_dir) {
            let Some(mtime_ms) = file_scan::stat_mtime_ms(&entry.stat_target) else {
                continue;
            };
            if let Some(raw) = parse_kimi_session_file(entry, mtime_ms)? {
                sessions.push(raw);
            }
        }
        Ok(sessions)
    }

    fn scan_for_sync(
        &self,
        store: &Store,
        since_ts: Option<i64>,
        _include_events: bool,
    ) -> anyhow::Result<Option<SyncScanResult>> {
        let Some(sessions_dir) = resolve_kimi_dir()? else {
            return Ok(Some(SyncScanResult { sessions: vec![], stats: SyncScanStats::default() }));
        };
        let result = file_scan::run_file_scan_with_options(
            store,
            "kimi-code",
            since_ts,
            file_scan::FileScanOptions {
                usage_parser_version: Some(USAGE_PARSER_VERSION),
                ..Default::default()
            },
            collect_session_entries(&sessions_dir),
            parse_kimi_session_file,
        )?;
        Ok(Some(result))
    }
}

fn resolve_kimi_dir() -> anyhow::Result<Option<PathBuf>> {
    resolve_home_dir(".kimi-code/sessions", "~/.kimi-code/sessions not found, skipping Kimi Code")
}

#[derive(Default, Clone)]
struct StateMeta {
    id: Option<String>,
    cwd: Option<String>,
    created_at: Option<i64>,
    title: Option<String>,
    is_custom_title: bool,
}

fn parse_state_json(content: &str) -> StateMeta {
    let v: Value = match serde_json::from_str(content) {
        Ok(v) => v,
        Err(_) => return StateMeta::default(),
    };
    StateMeta {
        id: v.get("id").and_then(|s| s.as_str()).map(str::to_string),
        cwd: v.get("cwd").and_then(|s| s.as_str()).map(str::to_string),
        created_at: json_i64(v.get("createdAt")),
        title: v.get("title").and_then(|s| s.as_str()).map(str::to_string),
        is_custom_title: v.get("isCustomTitle").and_then(|b| b.as_bool()).unwrap_or(false),
    }
}

fn collect_session_entries(sessions_dir: &Path) -> Vec<FileScanEntry> {
    let mut entries = Vec::new();
    if !sessions_dir.exists() {
        return entries;
    }

    for dir_entry in WalkDir::new(sessions_dir).max_depth(3).into_iter().filter_map(|e| e.ok()) {
        let path = dir_entry.path();
        if !path.is_file() || path.file_name().and_then(|n| n.to_str()) != Some("state.json") {
            continue;
        }
        let Some(session_dir) = path.parent() else {
            continue;
        };
        let dir_name = session_dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !dir_name.starts_with("session_") {
            continue;
        }
        let wire_path = session_dir.join("agents").join("main").join("wire.jsonl");
        if !wire_path.is_file() {
            continue;
        }

        let meta =
            fs::read_to_string(path).map(|content| parse_state_json(&content)).unwrap_or_default();
        let session_id = meta.id.clone().unwrap_or_else(|| dir_name.to_string());
        entries.push(FileScanEntry {
            session_id,
            stat_target: wire_path,
            directory: meta.cwd.clone(),
        });
    }

    entries
}

fn parse_kimi_session_file(
    entry: FileScanEntry,
    mtime_ms: i64,
) -> anyhow::Result<Option<RawSession>> {
    match parse_kimi_session_file_impl(&entry, mtime_ms) {
        Ok(raw) => Ok(raw),
        Err(e) => {
            warn!("failed to parse {}: {e}", entry.stat_target.display());
            Ok(None)
        }
    }
}

fn parse_kimi_session_file_impl(
    entry: &FileScanEntry,
    mtime_ms: i64,
) -> anyhow::Result<Option<RawSession>> {
    let wire_path = &entry.stat_target;
    let Some(session_dir) = wire_path.parent().and_then(Path::parent).and_then(Path::parent) else {
        return Ok(None);
    };
    let meta = fs::read_to_string(session_dir.join("state.json"))
        .map(|content| parse_state_json(&content))
        .unwrap_or_default();

    let file = fs::File::open(wire_path)?;
    let reader = BufReader::new(file);
    let source_path = wire_path.to_str().map(str::to_string);
    parse_kimi_wire(
        meta,
        entry.session_id.clone(),
        reader.lines(),
        mtime_ms,
        entry.directory.clone(),
        source_path,
    )
}

fn parse_kimi_wire(
    meta: StateMeta,
    session_id: String,
    lines: impl Iterator<Item = std::io::Result<String>>,
    mtime_ms: i64,
    directory: Option<String>,
    source_path: Option<String>,
) -> anyhow::Result<Option<RawSession>> {
    let directory = meta.cwd.clone().or(directory);
    let mut messages = Vec::new();
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

    if messages.is_empty() && usage_events.is_empty() {
        return Ok(None);
    }

    let started_at = first_timestamp(meta.created_at, &messages, &usage_events, &[]).unwrap_or(0);
    let duration_minutes = match (
        first_timestamp(None, &messages, &usage_events, &[]),
        crate::adapters::last_timestamp(None, &messages, &usage_events, &[]),
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
    if !usage_events.is_empty() {
        session = session.with_usage(usage_events, USAGE_PARSER_VERSION);
    }
    Ok(Some(session))
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
pub(crate) fn parse_kimi_session(
    state_json: &str,
    wire_jsonl: &str,
    session_id: &str,
) -> Option<RawSession> {
    let meta = parse_state_json(state_json);
    let lines = wire_jsonl.lines().map(|line| Ok(line.to_string()));
    parse_kimi_wire(meta, session_id.to_string(), lines, 0, None, None).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_state() -> &'static str {
        r#"{"id":"session_abc","cwd":"/repo","createdAt":1700000000000,"updatedAt":1700000060000,"title":"fix the bug","isCustomTitle":false,"archived":false}"#
    }

    fn fixture_wire() -> &'static str {
        concat!(
            r#"{"type":"metadata","protocol_version":"1.5","created_at":1700000000100}"#,
            "\n",
            r#"{"type":"turn.prompt","time":1700000001000,"origin":{"kind":"user"},"input":[{"type":"text","text":"fix the bug"}]}"#,
            "\n",
            r#"{"type":"context.append_message","time":1700000001001,"message":{"id":"m1","role":"user","origin":{"kind":"user"},"content":[{"type":"text","text":"fix the bug"}]}}"#,
            "\n",
            r#"{"type":"context.append_message","time":1700000002000,"message":{"id":"m2","role":"user","origin":{"kind":"injection"},"content":[{"type":"text","text":"<system-reminder>noise</system-reminder>"}]}}"#,
            "\n",
            r#"{"type":"context.append_message","time":1700000002500,"message":{"id":"m3","role":"user","origin":{"kind":"task"},"content":[{"type":"text","text":"<notification>background task done</notification>"}]}}"#,
            "\n",
            r#"{"type":"llm.request","time":1700000003000,"model":"kimi-k3","provider":"moonshot","kind":"loop"}"#,
            "\n",
            r#"{"type":"context.append_loop_event","time":1700000004000,"event":{"type":"content.part","uuid":"u1","part":{"type":"think","think":"pondering"}}}"#,
            "\n",
            r#"{"type":"context.append_loop_event","time":1700000005000,"event":{"type":"content.part","uuid":"u2","part":{"type":"text","text":"Here is the fix."}}}"#,
            "\n",
            r#"{"type":"usage.record","time":1700000006000,"model":"kimi-k3","usageScope":"turn","usage":{"inputOther":1200,"output":80,"inputCacheRead":300,"inputCacheCreation":40}}"#,
        )
    }

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
    }

    #[test]
    fn parse_kimi_session_custom_title_goes_to_custom_title() {
        let state = r#"{"id":"session_x","title":"my rename","isCustomTitle":true}"#;
        let session = parse_kimi_session(state, fixture_wire(), "session_x").unwrap();
        assert_eq!(session.custom_title.as_deref(), Some("my rename"));
        assert_eq!(session.summary, None);
    }

    #[test]
    fn parse_kimi_session_empty_wire_returns_none() {
        let wire = r#"{"type":"metadata","protocol_version":"1.5","created_at":1700000000100}"#;
        assert!(parse_kimi_session(fixture_state(), wire, "session_abc").is_none());
    }
}
