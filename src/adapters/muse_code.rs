use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;
use tracing::{debug, warn};
use walkdir::WalkDir;

use crate::adapters::AdapterSyncContext;
use crate::adapters::events::{self, EventContext};
use crate::adapters::file_scan::{self, FileMetadataSnapshot, FileScanEntry, FileScanOptions};
use crate::adapters::json_util::{json_i64, jsonl_indexed};
use crate::adapters::usage::usage_count;
use crate::adapters::{
    RawMessage, RawSession, ResumeCommand, SourceAdapter, SyncScanResult, first_timestamp,
};
use crate::types::{FileEvidence, FileOperation, RawSessionEvent, RawUsageEvent, Role};

pub(crate) struct MuseCodeAdapter;

const USAGE_PARSER_VERSION: u32 = 1;
const EVENT_PARSER_VERSION: u32 = 1;
const SESSION_LOG: &str = "session.jsonl";
const SUBAGENT_DIR: &str = "subagent";

impl SourceAdapter for MuseCodeAdapter {
    fn id(&self) -> &str {
        "muse-code"
    }

    fn label(&self) -> &str {
        "MU"
    }

    fn usage_parser_version(&self) -> Option<u32> {
        Some(USAGE_PARSER_VERSION)
    }

    fn resume_command(&self, source_id: &str) -> Option<ResumeCommand> {
        Some(ResumeCommand::new("muse", &["resume", source_id]))
    }

    fn start_command(&self, prompt: String) -> Option<ResumeCommand> {
        Some(crate::adapters::prompt_start("muse", prompt))
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let Some(root) = sessions_root() else {
            return Ok(vec![]);
        };
        let mut sessions = Vec::new();
        for entry in collect_entries(&root) {
            let Some(snapshot) = session_snapshot(&entry) else {
                continue;
            };
            let mtime_ms = snapshot.effective_mtime_ms();
            let path = entry.clone();
            let raw = parse_entry(entry, mtime_ms, true)?;
            if session_snapshot(&path).as_ref() != Some(&snapshot) {
                debug!("skipping unstable Muse session {}", path.stat_target.display());
                continue;
            }
            sessions.extend(raw);
        }
        Ok(sessions)
    }

    fn scan_for_sync(
        &self,
        context: &AdapterSyncContext,
        since_ts: Option<i64>,
        include_events: bool,
    ) -> anyhow::Result<Option<SyncScanResult>> {
        let Some(root) = sessions_root() else {
            return Ok(Some(SyncScanResult::default()));
        };
        Ok(Some(file_scan::run_file_scan_with_options_and_snapshot(
            context,
            since_ts,
            FileScanOptions {
                stream_sessions: true,
                usage_parser_version: Some(USAGE_PARSER_VERSION),
                event_parser_version: include_events.then_some(EVENT_PARSER_VERSION),
                metadata_parser_version: None,
            },
            collect_entries(&root),
            session_snapshot,
            |entry, mtime_ms| parse_entry(entry, mtime_ms, include_events),
        )?))
    }
}

fn sessions_root() -> Option<PathBuf> {
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| dirs::home_dir().map(|home| home.join(".local").join("share")))?;
    let root = data_home.join("muse").join("sessions");
    if !root.is_dir() {
        debug!("{} not found, skipping Muse Code", root.display());
        return None;
    }
    Some(root)
}

fn collect_entries(root: &Path) -> Vec<FileScanEntry> {
    WalkDir::new(root)
        .min_depth(5)
        .max_depth(5)
        .into_iter()
        .filter_entry(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file() && entry.file_name() == SESSION_LOG)
        .filter_map(|entry| {
            let session_id = entry.path().parent()?.file_name()?.to_str()?.to_string();
            Some(FileScanEntry {
                session_id,
                stat_target: entry.path().to_path_buf(),
                directory: None,
            })
        })
        .collect()
}

fn subagent_logs(session_log: &Path) -> Vec<PathBuf> {
    let Some(session_dir) = session_log.parent() else {
        return Vec::new();
    };
    let subagents = session_dir.join(SUBAGENT_DIR);
    if !subagents.is_dir() {
        return Vec::new();
    }
    let mut logs: Vec<PathBuf> = WalkDir::new(subagents)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file() && entry.file_name() == SESSION_LOG)
        .map(|entry| entry.into_path())
        .collect();
    logs.sort();
    logs
}

type SessionFingerprint = Vec<(PathBuf, FileMetadataSnapshot)>;

fn session_snapshot(
    entry: &FileScanEntry,
) -> Option<file_scan::FileScanSnapshot<SessionFingerprint>> {
    let mut fingerprint = Vec::new();
    let mut effective_mtime_ms = None;
    for path in std::iter::once(entry.stat_target.clone()).chain(subagent_logs(&entry.stat_target))
    {
        let Some(snapshot) = file_scan::file_metadata_snapshot(&path) else {
            if path == entry.stat_target {
                return None;
            }
            continue;
        };
        effective_mtime_ms = effective_mtime_ms.max(snapshot.mtime_ms());
        fingerprint.push((path, snapshot));
    }
    Some(file_scan::FileScanSnapshot::new(effective_mtime_ms?, fingerprint))
}

fn parse_entry(
    entry: FileScanEntry,
    mtime_ms: i64,
    include_events: bool,
) -> anyhow::Result<Option<RawSession>> {
    match parse_session(&entry.stat_target, &entry.session_id, mtime_ms, include_events) {
        Ok(raw) => Ok(raw),
        Err(error) => {
            warn!("failed to parse {}: {error}", entry.stat_target.display());
            Ok(None)
        }
    }
}

struct Record {
    line_index: usize,
    child_index: Option<usize>,
    value: Value,
}

impl Record {
    fn source_event_id(&self) -> String {
        self.value.get("id").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| {
            match self.child_index {
                Some(child) => format!("line:{}:{child}", self.line_index),
                None => format!("line:{}", self.line_index),
            }
        })
    }

    fn timestamp(&self) -> Option<i64> {
        json_i64(self.value.get("recorded_at")).map(|micros| micros / 1000)
    }

    fn belongs_to(&self, stream_id: &str) -> bool {
        match self.value.pointer("/stream/id").and_then(Value::as_str) {
            Some(id) => id == stream_id,
            None => true,
        }
    }

    fn run_event(&self) -> Option<&Value> {
        if self.value.get("payload_type").and_then(Value::as_str) != Some("runtime.session")
            || self.value.pointer("/payload/kind").and_then(Value::as_str) != Some("run")
        {
            return None;
        }
        self.value.pointer("/payload/event")
    }
}

fn read_records(path: &Path, stream_id: &str) -> anyhow::Result<Vec<Record>> {
    let reader = BufReader::new(fs::File::open(path)?);
    let mut records = Vec::new();
    for item in jsonl_indexed(reader.lines()) {
        let (line_index, value) = item?;
        match value.get("children").and_then(Value::as_array) {
            Some(children) => {
                for (child_index, child) in children.iter().enumerate() {
                    let Some(value) = child
                        .get("record_json")
                        .and_then(Value::as_str)
                        .and_then(|json| serde_json::from_str::<Value>(json).ok())
                    else {
                        continue;
                    };
                    records.push(Record { line_index, child_index: Some(child_index), value });
                }
            }
            None => records.push(Record { line_index, child_index: None, value }),
        }
    }
    records.retain(|record| record.belongs_to(stream_id));
    Ok(records)
}

#[derive(Default)]
struct SessionFacts {
    workspace_root: Option<String>,
    route_cwd: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    custom_title: Option<String>,
}

impl SessionFacts {
    fn observe(&mut self, record: &Value) {
        let inner = record.pointer("/payload/record");
        let text = |key: &str| {
            inner
                .and_then(|inner| inner.get(key))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        };
        match record.get("payload_type").and_then(Value::as_str) {
            Some("runtime.session.metadata") => {
                self.workspace_root = self.workspace_root.take().or_else(|| text("workspace_root"));
                self.provider = self.provider.take().or_else(|| text("provider_id"));
                self.model = self.model.take().or_else(|| text("model_id"));
            }
            Some("runtime.session.route_facts") => {
                self.route_cwd = self.route_cwd.take().or_else(|| text("cwd"));
            }
            Some("session.name.changed") => {
                let manual =
                    record.pointer("/payload/source").and_then(Value::as_str) != Some("automatic");
                if let Some(name) = record
                    .pointer("/payload/new_name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    && manual
                {
                    self.custom_title = Some(name.to_string());
                }
            }
            _ => {}
        }
    }

    fn directory(&self) -> Option<String> {
        self.workspace_root.clone().or_else(|| self.route_cwd.clone())
    }

    fn provider(&self) -> &str {
        self.provider.as_deref().unwrap_or("meta")
    }
}

fn parse_session(
    path: &Path,
    session_id: &str,
    mtime_ms: i64,
    include_events: bool,
) -> anyhow::Result<Option<RawSession>> {
    let source_path = path.to_str().map(str::to_string);
    let records = read_records(path, session_id)?;
    let mut facts = SessionFacts::default();
    for record in &records {
        facts.observe(&record.value);
    }
    let directory = facts.directory();

    let intents = UserIntents::collect(&records);
    let mut messages = Vec::new();
    let mut usage_events = Vec::new();
    let mut events = Vec::new();
    for record in &records {
        if let Some(prompt) = intents.prompt(record) {
            messages.push(RawMessage {
                role: Role::User,
                content: prompt,
                timestamp: record.timestamp(),
            });
            continue;
        }
        let Some(event) = record.run_event() else {
            continue;
        };
        let timestamp = record.timestamp();
        let text = |key: &str| {
            event
                .get(key)
                .and_then(Value::as_str)
                .filter(|text| !text.trim().is_empty())
                .map(str::to_string)
        };
        match event.get("kind").and_then(Value::as_str) {
            Some("started") => {
                if let Some(prompt) = text("prompt")
                    && !intents.covers_run(record)
                {
                    messages.push(RawMessage { role: Role::User, content: prompt, timestamp });
                }
            }
            Some("assistant_message_committed") => {
                if let Some(reply) = text("text") {
                    messages.push(RawMessage { role: Role::Assistant, content: reply, timestamp });
                }
            }
            Some("model_completed") => {
                if let Some(mut usage) = usage_event(
                    record,
                    event,
                    &facts,
                    usage_events.len() as u32,
                    mtime_ms,
                    source_path.as_deref(),
                ) {
                    usage.message_seq = messages.len().checked_sub(1).map(|seq| seq as u32);
                    usage_events.push(usage);
                }
            }
            Some("assistant_tool_calls_committed") if include_events => {
                for (index, call) in event
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    events.extend(tool_call_event(
                        call,
                        event_context(record, index, &events, &messages, &source_path),
                        directory.as_deref(),
                    ));
                }
            }
            Some("tool_result_batch_committed") if include_events => {
                for (index, result) in
                    event.get("results").and_then(Value::as_array).into_iter().flatten().enumerate()
                {
                    events.push(tool_result_event(
                        result,
                        event_context(record, index, &events, &messages, &source_path),
                    ));
                }
            }
            _ => {}
        }
    }

    for log in subagent_logs(path) {
        let Some(child_id) = log.parent().and_then(Path::file_name).and_then(|name| name.to_str())
        else {
            continue;
        };
        let records = match read_records(&log, child_id) {
            Ok(records) => records,
            Err(error) => {
                warn!("failed to read Muse subagent log {}: {error}", log.display());
                continue;
            }
        };
        let log_path = log.to_str().map(str::to_string);
        for record in &records {
            if let Some(event) = record.run_event()
                && event.get("kind").and_then(Value::as_str) == Some("model_completed")
                && let Some(mut usage) = usage_event(
                    record,
                    event,
                    &facts,
                    usage_events.len() as u32,
                    mtime_ms,
                    log_path.as_deref(),
                )
            {
                usage.event_key = format!("{child_id}:{}", usage.event_key);
                usage_events.push(usage);
            }
        }
    }

    if messages.is_empty() && usage_events.is_empty() && events.is_empty() {
        return Ok(None);
    }
    let started_at = first_timestamp(None, &messages, &usage_events, &events).unwrap_or(mtime_ms);
    let mut session =
        RawSession::search_only(session_id, directory, started_at, Some(mtime_ms), None, messages);
    session.source_file_path = source_path;
    session.custom_title = facts.custom_title;
    session = session.with_usage(usage_events, USAGE_PARSER_VERSION);
    if include_events {
        session = session.with_events(events, EVENT_PARSER_VERSION);
    }
    Ok(Some(session))
}

struct UserIntents {
    accepted: HashSet<String>,
    dropped: HashSet<String>,
    runs: HashSet<String>,
}

impl UserIntents {
    fn collect(records: &[Record]) -> Self {
        let mut intents =
            Self { accepted: HashSet::new(), dropped: HashSet::new(), runs: HashSet::new() };
        for record in records {
            let payload = record.value.get("payload");
            let intent_id = payload
                .and_then(|payload| payload.get("intent_id"))
                .and_then(Value::as_str)
                .map(str::to_string);
            match (record.value.get("payload_type").and_then(Value::as_str), intent_id) {
                (Some("runtime.user_intent.accepted"), Some(id)) if is_main_surface(payload) => {
                    intents.accepted.insert(id);
                }
                (Some("runtime.user_intent.materialized"), Some(id))
                    if intents.accepted.contains(&id) =>
                {
                    let outcome = payload.and_then(|payload| payload.get("outcome"));
                    if let Some(run_id) =
                        outcome.and_then(|outcome| outcome.get("run_id")).and_then(Value::as_str)
                    {
                        intents.runs.insert(run_id.to_string());
                    }
                    if outcome.and_then(|outcome| outcome.get("kind")).and_then(Value::as_str)
                        == Some("terminal_no_effect")
                    {
                        intents.dropped.insert(id);
                    }
                }
                _ => {}
            }
        }
        intents
    }

    fn prompt(&self, record: &Record) -> Option<String> {
        if record.value.get("payload_type").and_then(Value::as_str)
            != Some("runtime.user_intent.accepted")
        {
            return None;
        }
        let payload = record.value.get("payload")?;
        let id = payload.get("intent_id").and_then(Value::as_str)?;
        if !self.accepted.contains(id) || self.dropped.contains(id) {
            return None;
        }
        let text = payload
            .get("refill_blocks")
            .and_then(Value::as_array)?
            .iter()
            .filter(|block| block.get("kind").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        (!text.trim().is_empty()).then_some(text)
    }

    fn covers_run(&self, record: &Record) -> bool {
        record
            .value
            .pointer("/payload/run_id")
            .and_then(Value::as_str)
            .is_some_and(|run_id| self.accepted.contains(run_id) || self.runs.contains(run_id))
    }
}

fn is_main_surface(payload: Option<&Value>) -> bool {
    payload.and_then(|payload| payload.get("surface")).and_then(Value::as_str) == Some("main")
}

fn usage_event(
    record: &Record,
    event: &Value,
    facts: &SessionFacts,
    event_seq: u32,
    fallback_timestamp: i64,
    source_path: Option<&str>,
) -> Option<RawUsageEvent> {
    let usage = event.get("usage")?;
    let input = usage_count(usage, &["input_tokens"]);
    let cache_read_tokens = usage_count(usage, &["cache_read_tokens", "cached_tokens"]).min(input);
    let cache_write_tokens = usage_count(usage, &["cache_write_tokens"]);
    let output = usage_count(usage, &["output_tokens"]);
    let reasoning_tokens = usage_count(usage, &["reasoning_tokens"]).min(output);
    let input_tokens = input - cache_read_tokens;
    let output_tokens = output - reasoning_tokens;
    if input_tokens == 0
        && output_tokens == 0
        && cache_read_tokens == 0
        && cache_write_tokens == 0
        && reasoning_tokens == 0
    {
        return None;
    }
    let model = event
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.trim().is_empty())
        .or(facts.model.as_deref())
        .unwrap_or("unknown")
        .to_string();
    Some(RawUsageEvent {
        model,
        provider: facts.provider().to_string(),
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_write_tokens,
        reasoning_tokens,
        source_path: source_path.map(str::to_string),
        raw_usage_json: Some(usage.to_string()),
        ..RawUsageEvent::observed(
            record.source_event_id(),
            event_seq,
            record.timestamp().unwrap_or(fallback_timestamp),
            USAGE_PARSER_VERSION,
        )
    })
}

fn event_context(
    record: &Record,
    index: usize,
    events: &[RawSessionEvent],
    messages: &[RawMessage],
    source_path: &Option<String>,
) -> EventContext {
    EventContext {
        event_seq: events.len() as u32,
        timestamp: record.timestamp(),
        source_path: source_path.clone(),
        source_event_id: Some(format!("{}:{index}", record.source_event_id())),
        message_seq: messages.len().checked_sub(1).map(|seq| seq as u32),
        parser_version: EVENT_PARSER_VERSION,
    }
}

fn tool_call_event(
    call: &Value,
    context: EventContext,
    cwd: Option<&str>,
) -> Option<RawSessionEvent> {
    let name = call.get("name").and_then(Value::as_str).filter(|name| !name.is_empty())?;
    let args = match call.get("args") {
        Some(Value::String(text)) => serde_json::from_str::<Value>(text).ok(),
        Some(other) => Some(other.clone()),
        None => None,
    };
    let mut event = events::tool_call_event(context, name.to_string(), args.as_ref());
    let arg = |key: &str| {
        args.as_ref()
            .and_then(|args| args.get(key))
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
    };
    let operation = match name {
        "read_file" => Some(FileOperation::Read),
        "write_file" | "edit_file" => Some(FileOperation::Write),
        _ => None,
    };
    if let Some(operation) = operation
        && let Some(path) = arg("path")
    {
        event.kind =
            if operation == FileOperation::Read { "file_read" } else { "file_write" }.into();
        event.target = Some(path.to_string());
        event.files.push(FileEvidence::call(path.to_string(), operation, cwd.map(str::to_string)));
    } else if name == "bash"
        && let Some(command) = arg("command")
    {
        let cwd = match args.as_ref().and_then(|args| args.get("workdir")) {
            Some(workdir) => workdir.as_str().filter(|path| Path::new(path).is_absolute()),
            None => cwd,
        };
        let (files, status) = events::shell_file_evidence(command, cwd);
        event.kind = "command".into();
        event.target = Some(command.to_string());
        event.files = files;
        event.command_evidence_status = Some(status);
    }
    event.tool_call_id =
        call.get("call_id").and_then(Value::as_str).filter(|id| !id.is_empty()).map(str::to_string);
    Some(event)
}

fn tool_result_event(result: &Value, context: EventContext) -> RawSessionEvent {
    let text = result.get("text").and_then(Value::as_str).unwrap_or_default();
    let report = text
        .trim_start()
        .starts_with('{')
        .then(|| serde_json::from_str::<Value>(text).ok())
        .flatten()
        .filter(|report| report.get("output").is_some_and(Value::is_string));
    let summary = report
        .as_ref()
        .and_then(|report| report.get("output"))
        .and_then(Value::as_str)
        .unwrap_or(text);
    let mut event = events::tool_result_event(context, None, Some(summary.to_string()));
    event.status = report
        .as_ref()
        .and_then(|report| json_i64(report.get("exit_code")))
        .map(|code| if code == 0 { "success" } else { "error" }.to_string());
    event.tool_call_id = result
        .get("tool_call_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string);
    event.attrs_json = Some(result.to_string());
    event
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const SESSION: &str = "01a11229-4e55-75d0-b7c7-79be450adef7";
    const CHILD: &str = "ea396c61-f788-4cbb-92bf-e531b6c1c098";

    fn record(stream: &str, id: &str, micros: i64, payload_type: &str, payload: Value) -> Value {
        json!({
            "schema_version": 1,
            "id": id,
            "stream": {"kind": "session", "id": stream},
            "recorded_at": micros,
            "record_type": "event",
            "payload_type": payload_type,
            "payload": payload,
        })
    }

    fn run(stream: &str, id: &str, micros: i64, event: Value) -> Value {
        record(stream, id, micros, "runtime.session", json!({"kind": "run", "event": event}))
    }

    fn intent(id: &str, micros: i64, text: &str) -> Value {
        record(
            SESSION,
            &format!("accepted-{id}"),
            micros,
            "runtime.user_intent.accepted",
            json!({"intent_id": id, "surface": "main", "refill_blocks": [{"kind": "text", "text": text}]}),
        )
    }

    fn materialized(id: &str, micros: i64, outcome: &str) -> Value {
        record(
            SESSION,
            &format!("materialized-{id}"),
            micros,
            "runtime.user_intent.materialized",
            json!({"intent_id": id, "outcome": {"kind": outcome, "run_id": format!("run-of-{id}")}}),
        )
    }

    fn write_log(path: &Path, records: &[Value]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body: String = records.iter().map(|record| format!("{record}\n")).collect();
        fs::write(path, body).unwrap();
    }

    fn session_log(root: &Path) -> PathBuf {
        root.join("2026/10/06").join(SESSION).join(SESSION_LOG)
    }

    fn write_session(root: &Path) -> PathBuf {
        let path = session_log(root);
        let frame_child = record(
            SESSION,
            "r-frame",
            1_791_306_059_000_000,
            "runtime.session.permission_format_declared",
            json!({"format": "profile_v1"}),
        );
        write_log(
            &path,
            &[
                json!({
                    "retained_frame": "session_permission_transaction",
                    "children": [{"child_index": 0, "record_json": frame_child.to_string()}],
                }),
                record(
                    SESSION,
                    "r-meta",
                    1_791_306_059_489_558,
                    "runtime.session.metadata",
                    json!({"kind": "metadata", "record": {"workspace_root": "/w/pool", "provider_id": "meta"}}),
                ),
                intent("intent-1", 1_791_306_059_682_263, "find the pool leak"),
                record(
                    SESSION,
                    "r-start",
                    1_791_306_059_689_712,
                    "runtime.session",
                    json!({"kind": "run", "run_id": "run-of-intent-1", "event": {"kind": "started", "prompt": "find the pool leak"}}),
                ),
                materialized("intent-1", 1_791_306_059_701_800, "top_level_turn_started"),
                run(
                    SESSION,
                    "r-task",
                    1_791_306_059_700_000,
                    json!({"kind": "started", "task_id": "t-1"}),
                ),
                run(
                    "task-stream",
                    "r-foreign",
                    1_791_306_059_710_000,
                    json!({"kind": "started", "prompt": "delegated objective"}),
                ),
                run(
                    SESSION,
                    "r-calls",
                    1_791_306_060_000_000,
                    json!({"kind": "assistant_tool_calls_committed", "tool_calls": [
                        {"call_id": "call-1", "name": "bash", "args": "{\"command\":\"go test ./pool\",\"workdir\":\"/w/pool\"}"},
                        {"call_id": "call-2", "name": "read_file", "args": "{\"path\":\"/w/pool/acquire.go\"}"},
                    ]}),
                ),
                run(
                    SESSION,
                    "r-results",
                    1_791_306_061_000_000,
                    json!({"kind": "tool_result_batch_committed", "results": [
                        {"tool_call_id": "call-1", "text": "{\"command\":\"go test ./pool\",\"exit_code\":1,\"output\":\"leaked 4 connections\"}"},
                        {"tool_call_id": "call-2", "text": "package pool"},
                    ]}),
                ),
                intent("steer-1", 1_791_306_061_500_000, "also check the timeout path"),
                materialized("steer-1", 1_791_306_061_500_100, "active_run_delivery"),
                intent("noop-1", 1_791_306_061_600_000, "never delivered"),
                materialized("noop-1", 1_791_306_061_600_100, "terminal_no_effect"),
                run(
                    SESSION,
                    "r-model",
                    1_791_306_062_000_000,
                    json!({"kind": "model_completed", "model": "muse-spark-1.3", "usage": {
                        "input_tokens": 1000, "output_tokens": 300, "cached_tokens": 800,
                        "cache_read_tokens": 800, "cache_write_tokens": 50, "reasoning_tokens": 120,
                    }}),
                ),
                run(
                    SESSION,
                    "r-attribution",
                    1_791_306_062_100_000,
                    json!({"kind": "goal_usage_attribution", "record": {"quantity": {"input_tokens": 1000}}}),
                ),
                run(
                    SESSION,
                    "r-reply",
                    1_791_306_063_000_000,
                    json!({"kind": "assistant_message_committed", "text": "The defer closes instead of returning."}),
                ),
                record(
                    SESSION,
                    "r-name",
                    1_791_306_063_100_000,
                    "session.name.changed",
                    json!({"new_name": "smooth-coma", "source": "automatic"}),
                ),
            ],
        );
        write_log(
            &path.parent().unwrap().join(SUBAGENT_DIR).join(CHILD).join(SESSION_LOG),
            &[
                run(
                    CHILD,
                    "c-start",
                    1_791_306_059_998_046,
                    json!({"kind": "started", "prompt": "You are a reminder observer."}),
                ),
                run(
                    CHILD,
                    "c-model",
                    1_791_306_060_500_000,
                    json!({"kind": "model_completed", "model": "muse-spark-1.3", "usage": {"input_tokens": 400, "output_tokens": 10, "cached_tokens": 0}}),
                ),
            ],
        );
        path
    }

    #[test]
    fn parses_main_conversation_with_tools_and_subagent_usage() {
        let root = tempfile::tempdir().unwrap();
        let path = write_session(root.path());

        let session = parse_session(&path, SESSION, 1_791_306_070_000, true).unwrap().unwrap();

        let messages: Vec<_> =
            session.messages.iter().map(|m| (m.role.as_str(), m.content.as_str())).collect();
        assert_eq!(
            messages,
            [
                ("user", "find the pool leak"),
                ("user", "also check the timeout path"),
                ("assistant", "The defer closes instead of returning."),
            ]
        );
        assert_eq!(session.messages[0].timestamp, Some(1_791_306_059_682));
        assert_eq!(session.directory.as_deref(), Some("/w/pool"));
        assert_eq!(session.custom_title, None);

        let usage: Vec<_> = session
            .usage_events
            .iter()
            .map(|u| {
                (
                    u.event_key.as_str(),
                    u.input_tokens,
                    u.cache_read_tokens,
                    u.cache_write_tokens,
                    u.output_tokens,
                    u.reasoning_tokens,
                )
            })
            .collect();
        assert_eq!(
            usage,
            [("r-model", 200, 800, 50, 180, 120), (&*format!("{CHILD}:c-model"), 400, 0, 0, 10, 0)]
        );
        assert!(
            session
                .usage_events
                .iter()
                .all(|u| u.model == "muse-spark-1.3" && u.provider == "meta")
        );

        let command = session.events.iter().find(|e| e.kind == "command").unwrap();
        assert_eq!(command.target.as_deref(), Some("go test ./pool"));
        assert_eq!(command.tool_call_id.as_deref(), Some("call-1"));
        let read = session.events.iter().find(|e| e.kind == "file_read").unwrap();
        assert_eq!(read.files[0].path, "/w/pool/acquire.go");
        let failed = session
            .events
            .iter()
            .find(|e| e.kind == "tool_result" && e.tool_call_id.as_deref() == Some("call-1"))
            .unwrap();
        assert_eq!(failed.status.as_deref(), Some("error"));
        assert_eq!(failed.summary.as_deref(), Some("leaked 4 connections"));
    }

    #[test]
    fn collects_only_dated_main_session_logs() {
        let root = tempfile::tempdir().unwrap();
        let path = write_session(root.path());
        write_log(
            &root.path().join(".msp-view-v1/2026/10/06").join(SESSION).join(SESSION_LOG),
            &[],
        );

        let entries = collect_entries(root.path());

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id, SESSION);
        assert_eq!(entries[0].stat_target, path);
    }
}
