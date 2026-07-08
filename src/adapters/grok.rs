use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;
use tracing::debug;

use crate::adapters::file_scan::{self, FileScanEntry, FileScanOptions};
use crate::adapters::json_util::{json_i64, jsonl_indexed, rfc3339_ms};
use crate::adapters::paths::resolve_home_dir;
use crate::adapters::{
    RawMessage, RawSession, ResumeCommand, SourceAdapter, SyncScanResult, SyncScanStats,
};
use crate::db::store::Store;
use crate::types::{RawUsageEvent, Role, TokenSource};

const USAGE_PARSER_VERSION: u32 = 1;

pub(crate) struct GrokAdapter;

impl SourceAdapter for GrokAdapter {
    fn id(&self) -> &str {
        "grok"
    }

    fn label(&self) -> &str {
        "GRK"
    }

    fn usage_parser_version(&self) -> Option<u32> {
        Some(USAGE_PARSER_VERSION)
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        Some(ResumeCommand {
            program: "grok".to_string(),
            args: vec!["--resume".to_string(), source_id.to_string()],
        })
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let Some(sessions_dir) = resolve_grok_sessions_dir()? else {
            return Ok(vec![]);
        };

        let (entries, _) = collect_grok_entries(&sessions_dir);
        let mut sessions = Vec::new();
        for entry in entries {
            let Some(mtime_ms) = file_scan::stat_mtime_ms(&entry.stat_target) else {
                continue;
            };
            if let Some(raw) = parse_grok_session_for_entry(&entry, mtime_ms)? {
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
        let Some(sessions_dir) = resolve_grok_sessions_dir()? else {
            return Ok(Some(SyncScanResult { sessions: vec![], stats: SyncScanStats::default() }));
        };
        Ok(Some(scan_for_sync_impl(&sessions_dir, store, since_ts)?))
    }

    fn prune(&self, store: &Store) -> anyhow::Result<()> {
        let Some(sessions_dir) = resolve_grok_sessions_dir()? else {
            return Ok(());
        };
        prune_impl(&sessions_dir, store)
    }
}

struct GrokSummary {
    session_id: String,
    directory: Option<String>,
    started_at: i64,
    updated_at: Option<i64>,
    current_model_id: Option<String>,
}

struct PromptTokenState {
    prompt_id: String,
    peak_total_tokens: i64,
    timestamp: Option<i64>,
    model: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AgentChunkKey {
    prompt_id: Option<String>,
    turn_start_ms: Option<i64>,
}

struct PendingAgentMessage {
    key: AgentChunkKey,
    message_index: usize,
}

fn resolve_grok_sessions_dir() -> anyhow::Result<Option<PathBuf>> {
    resolve_home_dir(".grok/sessions", "~/.grok/sessions not found, skipping Grok")
}

fn scan_for_sync_impl(
    sessions_dir: &Path,
    store: &Store,
    since_ts: Option<i64>,
) -> anyhow::Result<SyncScanResult> {
    let (entries, _) = collect_grok_entries(sessions_dir);
    file_scan::run_file_scan_with_options(
        store,
        "grok",
        since_ts,
        FileScanOptions {
            usage_parser_version: Some(USAGE_PARSER_VERSION),
            event_parser_version: None,
        },
        entries,
        |entry, mtime_ms| parse_grok_session_for_entry(&entry, mtime_ms),
    )
}

fn prune_impl(sessions_dir: &Path, store: &Store) -> anyhow::Result<()> {
    let (_, subagent_ids) = collect_grok_entries(sessions_dir);
    if subagent_ids.is_empty() {
        return Ok(());
    }
    let existing = store.session_meta_map("grok")?;
    for source_id in &subagent_ids {
        if existing.contains_key(source_id) {
            store.delete_session_data("grok", source_id)?;
        }
    }
    Ok(())
}

fn collect_grok_entries(sessions_dir: &Path) -> (Vec<FileScanEntry>, Vec<String>) {
    let mut entries = Vec::new();
    let mut subagent_ids = Vec::new();

    let workspace_dirs = match fs::read_dir(sessions_dir) {
        Ok(dirs) => dirs,
        Err(err) => {
            debug!("cannot read Grok sessions dir: {err}");
            return (entries, subagent_ids);
        }
    };

    for workspace_entry in workspace_dirs.flatten() {
        let workspace_path = workspace_entry.path();
        if !workspace_path.is_dir() {
            continue;
        }
        let workspace_name =
            workspace_path.file_name().and_then(|name| name.to_str()).unwrap_or("");
        if workspace_name == "session_search.sqlite" {
            continue;
        }

        let fallback_directory = decode_grok_workspace_dir(workspace_name);

        let session_dirs = match fs::read_dir(&workspace_path) {
            Ok(dirs) => dirs,
            Err(_) => continue,
        };

        for session_entry in session_dirs.flatten() {
            let session_path = session_entry.path();
            if !session_path.is_dir() {
                continue;
            }
            let session_id = match session_path.file_name().and_then(|name| name.to_str()) {
                Some(id) if is_grok_session_id(id) => id.to_string(),
                _ => continue,
            };

            let updates_path = session_path.join("updates.jsonl");
            if !updates_path.is_file() {
                continue;
            }

            let (summary_directory, session_kind) = load_summary_probe(&session_path);
            if matches!(session_kind.as_deref(), Some("subagent" | "subagent_resume")) {
                subagent_ids.push(session_id);
                continue;
            }

            let directory = summary_directory.or(fallback_directory.clone());

            entries.push(FileScanEntry { session_id, stat_target: updates_path, directory });
        }
    }

    (entries, subagent_ids)
}

fn load_summary_probe(session_dir: &Path) -> (Option<String>, Option<String>) {
    let summary_path = session_dir.join("summary.json");
    let Ok(content) = fs::read_to_string(summary_path) else {
        return (None, None);
    };
    let Ok(doc) = serde_json::from_str::<Value>(&content) else {
        return (None, None);
    };
    let directory = doc
        .get("info")
        .and_then(|info| info.get("cwd"))
        .and_then(|cwd| cwd.as_str())
        .map(str::trim)
        .filter(|cwd| !cwd.is_empty())
        .map(str::to_string);
    let session_kind = doc.get("session_kind").and_then(|kind| kind.as_str()).map(str::to_string);
    (directory, session_kind)
}

fn is_grok_session_id(id: &str) -> bool {
    uuid::Uuid::try_parse(id).is_ok()
}

fn decode_grok_workspace_dir(encoded: &str) -> Option<String> {
    let mut out = String::with_capacity(encoded.len());
    let bytes = encoded.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = &encoded[index + 1..index + 3];
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte as char);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index] as char);
        index += 1;
    }
    if out.is_empty() {
        return None;
    }
    Some(out)
}

fn parse_grok_session_for_entry(
    entry: &FileScanEntry,
    mtime_ms: i64,
) -> anyhow::Result<Option<RawSession>> {
    let session_dir = entry
        .stat_target
        .parent()
        .ok_or_else(|| anyhow::anyhow!("grok updates path has no parent"))?;

    let summary = match load_grok_summary(session_dir, &entry.session_id) {
        Ok(summary) => summary,
        Err(err) => {
            debug!("failed to read Grok summary for {}: {err}", entry.session_id);
            return Ok(None);
        }
    };

    let (messages, mut usage_events) = match parse_grok_updates(&entry.stat_target) {
        Ok(parsed) => parsed,
        Err(err) => {
            debug!("failed to parse Grok updates {}: {err}", entry.stat_target.display());
            return Ok(None);
        }
    };

    if messages.is_empty() {
        return Ok(None);
    }

    let fallback_model = summary.current_model_id.clone().unwrap_or_else(|| "grok".to_string());
    let source_path = entry.stat_target.to_str().map(str::to_string);
    for event in &mut usage_events {
        if event.model.is_empty() {
            event.model = fallback_model.clone();
        }
        event.source_path = source_path.clone();
    }

    let started_at =
        summary.started_at.max(messages.first().and_then(|message| message.timestamp).unwrap_or(0));

    let mut session = RawSession::search_only(
        summary.session_id,
        summary.directory.or(entry.directory.clone()),
        started_at,
        summary.updated_at.or(Some(mtime_ms)),
        None,
        messages,
    )
    .with_usage(usage_events, USAGE_PARSER_VERSION);

    session.updated_at = Some(mtime_ms);
    Ok(Some(session))
}

fn load_grok_summary(session_dir: &Path, fallback_id: &str) -> anyhow::Result<GrokSummary> {
    let summary_path = session_dir.join("summary.json");
    let content = fs::read_to_string(&summary_path)?;
    let doc: Value = serde_json::from_str(&content)?;

    let session_id = doc
        .get("info")
        .and_then(|info| info.get("id"))
        .and_then(|id| id.as_str())
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| fallback_id.to_string());

    let directory = doc
        .get("info")
        .and_then(|info| info.get("cwd"))
        .and_then(|cwd| cwd.as_str())
        .map(str::trim)
        .filter(|cwd| !cwd.is_empty())
        .map(str::to_string);

    let current_model_id = doc
        .get("current_model_id")
        .and_then(|model| model.as_str())
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_string);

    let started_at = rfc3339_ms(doc.get("created_at")).unwrap_or(0);
    let updated_at = rfc3339_ms(doc.get("updated_at"));
    Ok(GrokSummary { session_id, directory, started_at, updated_at, current_model_id })
}

fn parse_grok_updates(
    updates_path: &Path,
) -> anyhow::Result<(Vec<RawMessage>, Vec<RawUsageEvent>)> {
    let file = fs::File::open(updates_path)?;
    parse_grok_updates_reader(BufReader::new(file))
}

fn parse_grok_updates_reader<R: BufRead>(
    reader: R,
) -> anyhow::Result<(Vec<RawMessage>, Vec<RawUsageEvent>)> {
    let mut messages = Vec::new();
    let mut pending_agent: Option<PendingAgentMessage> = None;
    let mut prompts: Vec<PromptTokenState> = Vec::new();
    let mut prompt_index: HashMap<String, usize> = HashMap::new();
    let mut last_model: Option<String> = None;
    for item in jsonl_indexed(reader.lines()) {
        let (_, doc) = item?;

        let params = match doc.get("params") {
            Some(params) => params,
            None => continue,
        };
        let update = match params.get("update") {
            Some(update) => update,
            None => continue,
        };
        let session_update =
            update.get("sessionUpdate").and_then(|value| value.as_str()).unwrap_or("");
        let timestamp_ms = json_i64(doc.get("timestamp")).map(|ts| ts * 1000);
        track_prompt_tokens(params, timestamp_ms, &mut prompts, &mut prompt_index, &mut last_model);

        match session_update {
            "user_message_chunk" => {
                pending_agent = None;
                let content = extract_update_text(update.get("content"));
                if !content.is_empty() {
                    messages.push(RawMessage {
                        role: Role::User,
                        content,
                        timestamp: timestamp_ms,
                    });
                }
            }
            "agent_message_chunk" => {
                let content = extract_update_chunk_text(update.get("content"));
                if !content.trim().is_empty() {
                    push_or_append_agent_message(
                        &mut messages,
                        &mut pending_agent,
                        agent_chunk_key(params),
                        content,
                        timestamp_ms,
                    );
                }
            }
            "tool_call" => {
                if let Some(content) = format_tool_call(update) {
                    messages.push(RawMessage {
                        role: Role::Assistant,
                        content,
                        timestamp: timestamp_ms,
                    });
                }
            }
            "tool_call_update" => {
                if update.get("status").and_then(|status| status.as_str()) == Some("completed")
                    && let Some(content) = format_tool_call_result(update)
                {
                    messages.push(RawMessage {
                        role: Role::Assistant,
                        content,
                        timestamp: timestamp_ms,
                    });
                }
            }
            "agent_thought_chunk" | "available_commands_update" => {}
            _ => {}
        }
    }

    Ok((messages, prompt_usage_events(prompts)))
}

fn track_prompt_tokens(
    params: &Value,
    timestamp_ms: Option<i64>,
    prompts: &mut Vec<PromptTokenState>,
    prompt_index: &mut HashMap<String, usize>,
    last_model: &mut Option<String>,
) {
    let Some(meta) = params.get("_meta") else {
        return;
    };
    if let Some(model) = meta.get("modelId").and_then(|value| value.as_str())
        && !model.is_empty()
    {
        *last_model = Some(model.to_string());
    }
    let Some(prompt_id) =
        meta.get("promptId").and_then(|value| value.as_str()).filter(|id| !id.is_empty())
    else {
        return;
    };
    let Some(total_tokens) = json_i64(meta.get("totalTokens")) else {
        return;
    };
    let index = match prompt_index.get(prompt_id) {
        Some(index) => *index,
        None => {
            prompts.push(PromptTokenState {
                prompt_id: prompt_id.to_string(),
                peak_total_tokens: 0,
                timestamp: None,
                model: None,
            });
            prompt_index.insert(prompt_id.to_string(), prompts.len() - 1);
            prompts.len() - 1
        }
    };
    let prompt = &mut prompts[index];
    prompt.peak_total_tokens = prompt.peak_total_tokens.max(total_tokens);
    if timestamp_ms.is_some() {
        prompt.timestamp = timestamp_ms;
    }
    if prompt.model.is_none() {
        prompt.model = last_model.clone();
    }
}

fn prompt_usage_events(prompts: Vec<PromptTokenState>) -> Vec<RawUsageEvent> {
    let mut events = Vec::new();
    let mut previous_peak = 0i64;
    for prompt in prompts {
        let delta = prompt.peak_total_tokens - previous_peak;
        previous_peak = prompt.peak_total_tokens;
        if delta <= 0 {
            continue;
        }
        events.push(RawUsageEvent {
            event_key: format!("prompt:{}", prompt.prompt_id),
            event_seq: events.len() as u32,
            message_seq: None,
            timestamp: prompt.timestamp.unwrap_or_default(),
            model: prompt.model.unwrap_or_default(),
            provider: "xai".to_string(),
            input_tokens: delta,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
            token_source: TokenSource::Derived,
            parser_version: USAGE_PARSER_VERSION,
            source_path: None,
            raw_usage_json: Some(format!("{{\"totalTokens\":{}}}", prompt.peak_total_tokens)),
        });
    }
    events
}

fn agent_chunk_key(params: &Value) -> AgentChunkKey {
    let meta = params.get("_meta");
    AgentChunkKey {
        prompt_id: meta
            .and_then(|meta| meta.get("promptId"))
            .and_then(|prompt| prompt.as_str())
            .filter(|prompt| !prompt.is_empty())
            .map(str::to_string),
        turn_start_ms: json_i64(meta.and_then(|meta| meta.get("turnStartMs"))),
    }
}

fn push_or_append_agent_message(
    messages: &mut Vec<RawMessage>,
    pending_agent: &mut Option<PendingAgentMessage>,
    key: AgentChunkKey,
    content: String,
    timestamp: Option<i64>,
) {
    if let Some(pending) = pending_agent
        && pending.key == key
        && let Some(message) = messages.get_mut(pending.message_index)
    {
        message.content.push_str(&content);
        return;
    }

    let message_index = messages.len();
    messages.push(RawMessage { role: Role::Assistant, content, timestamp });
    *pending_agent = Some(PendingAgentMessage { key, message_index });
}

fn extract_update_text(content: Option<&Value>) -> String {
    let Some(content) = content else {
        return String::new();
    };
    if let Some(text) = content.as_str() {
        return text.trim().to_string();
    }
    if let Some(text) = content.get("text").and_then(|text| text.as_str()) {
        return text.trim().to_string();
    }
    if let Some(parts) = content.as_array() {
        let mut out = Vec::new();
        for part in parts {
            if let Some(text) = part.get("text").and_then(|text| text.as_str()) {
                let text = text.trim();
                if !text.is_empty() {
                    out.push(text);
                }
            }
        }
        return out.join("\n");
    }
    String::new()
}

fn extract_update_chunk_text(content: Option<&Value>) -> String {
    let Some(content) = content else {
        return String::new();
    };
    if let Some(text) = content.as_str() {
        return text.to_string();
    }
    if let Some(text) = content.get("text").and_then(|text| text.as_str()) {
        return text.to_string();
    }
    if let Some(parts) = content.as_array() {
        let mut out = Vec::new();
        for part in parts {
            if let Some(text) = part.get("text").and_then(|text| text.as_str())
                && !text.trim().is_empty()
            {
                out.push(text);
            }
        }
        return out.join("\n");
    }
    String::new()
}

fn format_tool_call(update: &Value) -> Option<String> {
    let title = update.get("title").and_then(|title| title.as_str()).unwrap_or("tool");
    let raw_input = update
        .get("rawInput")
        .map(|input| serde_json::to_string(input).unwrap_or_default())
        .unwrap_or_default();
    if raw_input.is_empty() {
        return Some(format!("[{title}]"));
    }
    Some(format!("[{title}] {raw_input}"))
}

fn format_tool_call_result(update: &Value) -> Option<String> {
    let title = update.get("title").and_then(|title| title.as_str()).unwrap_or("tool");
    let content = update
        .get("content")
        .and_then(|content| content.as_array())
        .and_then(|items| items.first())
        .and_then(|item| item.get("content"))
        .and_then(|content| content.get("text"))
        .and_then(|text| text.as_str())
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .or_else(|| {
            let text = extract_update_text(update.get("content"));
            (!text.is_empty()).then_some(text)
        })?;

    let mut out = format!("[{title}] -> ");
    const MAX_CHARS: usize = 500;
    if content.chars().count() > MAX_CHARS {
        out.push_str(&content.chars().take(MAX_CHARS).collect::<String>());
        out.push_str("...");
    } else {
        out.push_str(&content);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::db::{schema, store::Store};
    use crate::types::Session;

    fn setup_store() -> Store {
        schema::register_sqlite_vec();
        Store::open_in_memory().unwrap()
    }

    fn temp_grok_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "recall-grok-test-{}-{}",
            label,
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_grok_session(
        root: &Path,
        workspace_encoded: &str,
        session_id: &str,
        summary: &str,
        updates: &str,
    ) -> PathBuf {
        let session_dir = root.join(workspace_encoded).join(session_id);
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(session_dir.join("summary.json"), summary).unwrap();
        let updates_path = session_dir.join("updates.jsonl");
        fs::write(&updates_path, updates).unwrap();
        updates_path
    }

    fn make_existing_session(source_id: &str, updated_at: i64, message_count: u32) -> Session {
        Session {
            id: format!("internal-{source_id}"),
            source: "grok".to_string(),
            source_id: source_id.to_string(),
            title: "existing".to_string(),
            directory: None,
            repo_remote: None,
            repo_slug: None,
            repo_name: None,
            started_at: 0,
            updated_at: Some(updated_at),
            message_count,
            entrypoint: None,
            custom_title: None,
            summary: None,
            duration_minutes: None,
            source_file_path: None,
            is_import: false,
        }
    }

    #[test]
    fn decode_grok_workspace_dir_decodes_percent_encoding() {
        assert_eq!(
            decode_grok_workspace_dir("%2FUsers%2Fx%2Fgit%2Fsamzong%2FRecall").as_deref(),
            Some("/Users/x/git/samzong/Recall")
        );
    }

    #[test]
    fn parse_grok_updates_extracts_messages() {
        let jsonl = r#"{"timestamp":10,"method":"session/update","params":{"sessionId":"019e9003-1ed9-70e3-803b-1e7f96a072eb","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"hello"}},"_meta":{"totalTokens":100,"turnStartMs":1000}}}
{"timestamp":11,"method":"session/update","params":{"sessionId":"019e9003-1ed9-70e3-803b-1e7f96a072eb","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}},"_meta":{"totalTokens":250,"promptId":"prompt-1","modelId":"grok-composer-2.5-fast","turnStartMs":1000}}}
{"timestamp":12,"method":"session/update","params":{"sessionId":"019e9003-1ed9-70e3-803b-1e7f96a072eb","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":" there"}},"_meta":{"totalTokens":255,"promptId":"prompt-1","turnStartMs":1000}}}
{"timestamp":20,"method":"session/update","params":{"sessionId":"019e9003-1ed9-70e3-803b-1e7f96a072eb","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"next"}},"_meta":{"totalTokens":260,"turnStartMs":2000}}}
{"timestamp":21,"method":"session/update","params":{"sessionId":"019e9003-1ed9-70e3-803b-1e7f96a072eb","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"done"}},"_meta":{"totalTokens":400,"promptId":"prompt-2","turnStartMs":2000}}}
"#;

        let (messages, usage_events) = parse_grok_updates_reader(Cursor::new(jsonl)).unwrap();

        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(messages[0].content, "hello");
        assert_eq!(messages[1].content, "hi there");
        assert_eq!(usage_events.len(), 2);
        assert_eq!(usage_events[0].event_key, "prompt:prompt-1");
        assert_eq!(usage_events[0].input_tokens, 255);
        assert_eq!(usage_events[0].model, "grok-composer-2.5-fast");
        assert_eq!(usage_events[0].timestamp, 12_000);
        assert_eq!(usage_events[1].event_key, "prompt:prompt-2");
        assert_eq!(usage_events[1].input_tokens, 145);
    }

    #[test]
    fn parse_grok_updates_resyncs_after_compaction_shrink() {
        let jsonl = r#"{"timestamp":10,"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"a"}},"_meta":{"totalTokens":1000,"promptId":"prompt-a"}}}
{"timestamp":20,"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"b"}},"_meta":{"totalTokens":800,"promptId":"prompt-b"}}}
{"timestamp":30,"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"c"}},"_meta":{"totalTokens":900,"promptId":"prompt-c"}}}
"#;

        let (_, usage_events) = parse_grok_updates_reader(Cursor::new(jsonl)).unwrap();

        assert_eq!(usage_events.len(), 2);
        assert_eq!(usage_events[0].event_key, "prompt:prompt-a");
        assert_eq!(usage_events[1].event_key, "prompt:prompt-c");
        assert_eq!(usage_events[1].input_tokens, 100);
    }

    #[test]
    fn parse_grok_updates_merges_streamed_agent_chunks() {
        let jsonl = r#"{"timestamp":10,"method":"session/update","params":{"sessionId":"019e9003-1ed9-70e3-803b-1e7f96a072eb","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"hello"}}}}
{"timestamp":11,"method":"session/update","params":{"sessionId":"019e9003-1ed9-70e3-803b-1e7f96a072eb","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"first "}},"_meta":{"promptId":"prompt-1","turnStartMs":1000}}}
{"timestamp":12,"method":"session/update","params":{"sessionId":"019e9003-1ed9-70e3-803b-1e7f96a072eb","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"response"}},"_meta":{"promptId":"prompt-1","turnStartMs":1000}}}
{"timestamp":20,"method":"session/update","params":{"sessionId":"019e9003-1ed9-70e3-803b-1e7f96a072eb","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"next"}}}}
{"timestamp":21,"method":"session/update","params":{"sessionId":"019e9003-1ed9-70e3-803b-1e7f96a072eb","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"second"}},"_meta":{"promptId":"prompt-2","turnStartMs":2000}}}
"#;

        let (messages, _) = parse_grok_updates_reader(Cursor::new(jsonl)).unwrap();

        assert_eq!(messages.len(), 4);
        assert_eq!(messages[1].role, Role::Assistant);
        assert_eq!(messages[1].content, "first response");
        assert_eq!(messages[1].timestamp, Some(11_000));
        assert_eq!(messages[3].content, "second");
    }

    #[test]
    fn collect_grok_entries_reads_summary_cwd() {
        let root = temp_grok_root("collect");
        let session_id = "019e9003-1ed9-70e3-803b-1e7f96a072eb";
        write_grok_session(
            &root,
            "%2Ftmp%2Fproject",
            session_id,
            r#"{"info":{"id":"019e9003-1ed9-70e3-803b-1e7f96a072eb","cwd":"/tmp/from-summary"},"created_at":"2026-06-04T00:00:00Z"}"#,
            r#"{"timestamp":1,"method":"session/update","params":{"sessionId":"019e9003-1ed9-70e3-803b-1e7f96a072eb","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"hey"}}}}"#,
        );

        let (entries, subagent_ids) = collect_grok_entries(&root);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id, session_id);
        assert_eq!(entries[0].directory.as_deref(), Some("/tmp/from-summary"));
        assert!(subagent_ids.is_empty());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn collect_grok_entries_skips_subagent_sessions() {
        let root = temp_grok_root("subagent");
        let normal_id = "019e9003-1ed9-70e3-803b-1e7f96a072eb";
        let subagent_id = "119e9003-1ed9-70e3-803b-1e7f96a072eb";
        let resume_id = "229e9003-1ed9-70e3-803b-1e7f96a072eb";
        write_grok_session(
            &root,
            "%2Ftmp%2Fproject",
            normal_id,
            r#"{"info":{"id":"019e9003-1ed9-70e3-803b-1e7f96a072eb","cwd":"/tmp/project"},"created_at":"2026-06-04T00:00:00Z"}"#,
            r#"{"timestamp":1,"method":"session/update","params":{"sessionId":"019e9003-1ed9-70e3-803b-1e7f96a072eb","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"hey"}}}}"#,
        );
        write_grok_session(
            &root,
            "%2Ftmp%2Fproject",
            subagent_id,
            r#"{"info":{"id":"119e9003-1ed9-70e3-803b-1e7f96a072eb","cwd":"/tmp/project"},"created_at":"2026-06-04T00:00:00Z","session_kind":"subagent"}"#,
            r#"{"timestamp":1,"method":"session/update","params":{"sessionId":"119e9003-1ed9-70e3-803b-1e7f96a072eb","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"machinery"}}}}"#,
        );
        write_grok_session(
            &root,
            "%2Ftmp%2Fproject",
            resume_id,
            r#"{"info":{"id":"229e9003-1ed9-70e3-803b-1e7f96a072eb","cwd":"/tmp/project"},"created_at":"2026-06-04T00:00:00Z","session_kind":"subagent_resume"}"#,
            r#"{"timestamp":1,"method":"session/update","params":{"sessionId":"229e9003-1ed9-70e3-803b-1e7f96a072eb","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"machinery"}}}}"#,
        );

        let (entries, mut subagent_ids) = collect_grok_entries(&root);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id, normal_id);
        subagent_ids.sort();
        assert_eq!(subagent_ids, vec![subagent_id.to_string(), resume_id.to_string()]);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn prune_deletes_existing_subagent_session() {
        let root = temp_grok_root("subagent-delete");
        let normal_id = "019e9003-1ed9-70e3-803b-1e7f96a072eb";
        let subagent_id = "119e9003-1ed9-70e3-803b-1e7f96a072eb";
        write_grok_session(
            &root,
            "%2Ftmp%2Fproject",
            normal_id,
            r#"{"info":{"id":"019e9003-1ed9-70e3-803b-1e7f96a072eb","cwd":"/tmp/project"},"created_at":"2026-06-04T00:00:00Z"}"#,
            r#"{"timestamp":1,"method":"session/update","params":{"sessionId":"019e9003-1ed9-70e3-803b-1e7f96a072eb","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"hey"}}}}"#,
        );
        write_grok_session(
            &root,
            "%2Ftmp%2Fproject",
            subagent_id,
            r#"{"info":{"id":"119e9003-1ed9-70e3-803b-1e7f96a072eb","cwd":"/tmp/project"},"created_at":"2026-06-04T00:00:00Z","session_kind":"subagent"}"#,
            r#"{"timestamp":1,"method":"session/update","params":{"sessionId":"119e9003-1ed9-70e3-803b-1e7f96a072eb","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"machinery"}}}}"#,
        );
        let store = setup_store();
        store.insert_session(&make_existing_session(subagent_id, 1, 1)).unwrap();

        prune_impl(&root, &store).unwrap();
        let result = scan_for_sync_impl(&root, &store, None).unwrap();

        assert!(!store.session_meta_map("grok").unwrap().contains_key(subagent_id));
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].source_id, normal_id);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_for_sync_skips_unchanged_session() {
        let root = temp_grok_root("skip");
        let session_id = "019e9003-1ed9-70e3-803b-1e7f96a072eb";
        let updates_path = write_grok_session(
            &root,
            "%2Ftmp%2Fproject",
            session_id,
            r#"{"info":{"id":"019e9003-1ed9-70e3-803b-1e7f96a072eb","cwd":"/tmp/project"},"created_at":"2026-06-04T00:00:00Z"}"#,
            r#"{"timestamp":1,"method":"session/update","params":{"sessionId":"019e9003-1ed9-70e3-803b-1e7f96a072eb","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"hey"}}}}"#,
        );
        let mtime = file_scan::stat_mtime_ms(&updates_path).unwrap();
        let store = setup_store();
        store.insert_session(&make_existing_session(session_id, mtime, 1)).unwrap();

        let result = scan_for_sync_impl(&root, &store, None).unwrap();
        assert_eq!(result.sessions.len(), 1, "missing usage state must trigger a backfill parse");

        store
            .persist_usage_events_for_existing_session(
                "grok",
                session_id,
                &[],
                USAGE_PARSER_VERSION,
                Some(mtime),
            )
            .unwrap();

        let result = scan_for_sync_impl(&root, &store, None).unwrap();
        assert_eq!(result.sessions.len(), 0);
        assert_eq!(result.stats.skipped_sessions, 1);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_grok_session_attaches_usage_with_model_fallback() {
        let root = temp_grok_root("usage");
        let session_id = "019e9003-1ed9-70e3-803b-1e7f96a072eb";
        let updates_path = write_grok_session(
            &root,
            "%2Ftmp%2Fproject",
            session_id,
            r#"{"info":{"id":"019e9003-1ed9-70e3-803b-1e7f96a072eb","cwd":"/tmp/project"},"created_at":"2026-06-04T00:00:00Z","current_model_id":"grok-composer-2.5-fast"}"#,
            r#"{"timestamp":10,"method":"session/update","params":{"sessionId":"019e9003-1ed9-70e3-803b-1e7f96a072eb","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"hey"}},"_meta":{"turnStartMs":1000}}}
{"timestamp":11,"method":"session/update","params":{"sessionId":"019e9003-1ed9-70e3-803b-1e7f96a072eb","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"sure"}},"_meta":{"totalTokens":500,"promptId":"prompt-1","turnStartMs":1000}}}"#,
        );
        let mtime = file_scan::stat_mtime_ms(&updates_path).unwrap();
        let entry = FileScanEntry {
            session_id: session_id.to_string(),
            stat_target: updates_path.clone(),
            directory: None,
        };

        let session = parse_grok_session_for_entry(&entry, mtime).unwrap().unwrap();

        assert_eq!(session.usage_parser_version, Some(USAGE_PARSER_VERSION));
        assert_eq!(session.usage_events.len(), 1);
        assert_eq!(session.usage_events[0].model, "grok-composer-2.5-fast");
        assert_eq!(session.usage_events[0].source_path.as_deref(), updates_path.to_str());

        let _ = fs::remove_dir_all(&root);
    }
}
