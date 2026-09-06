use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use tracing::{debug, warn};

use crate::adapters::AdapterSyncContext;
use crate::adapters::events::{
    EventContext, patch_file_evidence, shell_file_evidence, tool_call_event, tool_result_event,
};
use crate::adapters::file_scan::{self, FileScanEntry};
use crate::adapters::json_util::json_i64;
use crate::adapters::paths::file_uri_to_path;
use crate::adapters::sync_state::metadata_state_is_current_for_mtime;
use crate::adapters::{
    RawMessage, RawSession, ReconcilePlan, ResumeCommand, SourceAdapter, SyncScanOutput,
    SyncScanResult, first_timestamp,
};
use crate::types::{
    CommandEvidenceStatus, EvidenceVisibility, FileEvidence, FileEvidenceKind, FileOperation,
    RawSessionEvent, Role,
};

pub(crate) struct CopilotChatAdapter;

const HOSTS: &[&str] = &["Code", "Code - Insiders", "VSCodium"];
const METADATA_PARSER_VERSION: u32 = 1;
const EVENT_PARSER_VERSION: u32 = 1;

impl SourceAdapter for CopilotChatAdapter {
    fn id(&self) -> &str {
        "copilot-chat"
    }

    fn label(&self) -> &str {
        "CCH"
    }

    fn resume_command(&self, _source_id: &str) -> Option<ResumeCommand> {
        None
    }

    fn scan(&self) -> anyhow::Result<Vec<RawSession>> {
        let mut sessions = Vec::new();
        for entry in collect_chat_entries(&vscode_user_roots()) {
            let Some(mtime_ms) = file_scan::stat_mtime_ms(&entry.stat_target) else {
                continue;
            };
            if let Some(raw) = parse_chat_session_for_entry(entry, mtime_ms)? {
                sessions.push(raw);
            }
        }
        Ok(sessions)
    }

    fn scan_for_sync_output(
        &self,
        context: &AdapterSyncContext,
        since_ts: Option<i64>,
        include_events: bool,
        force: bool,
    ) -> anyhow::Result<Option<SyncScanOutput>> {
        Ok(Some(scan_chat_entries_for_sync(
            collect_chat_entries(&vscode_user_roots()),
            context,
            since_ts,
            include_events,
            force,
        )?))
    }
}

fn scan_chat_entries_for_sync(
    entries: Vec<FileScanEntry>,
    context: &AdapterSyncContext,
    since_ts: Option<i64>,
    include_events: bool,
    force: bool,
) -> anyhow::Result<SyncScanOutput> {
    let tombstones = RefCell::new(HashSet::new());
    let scan = if force {
        let mut result = SyncScanResult::default();
        for entry in entries {
            result.stats.candidates += 1;
            let Some(snapshot) = file_scan::file_metadata_snapshot(&entry.stat_target) else {
                result.stats.rejected_before_parse += 1;
                continue;
            };
            let Some(mtime_ms) = snapshot.mtime_ms() else {
                result.stats.rejected_before_parse += 1;
                continue;
            };
            result.stats.parsed += 1;
            let Some(parsed) =
                parse_stable_chat_entry_with(entry, mtime_ms, snapshot, parse_chat_entry)?
            else {
                result.stats.unstable_sessions += 1;
                continue;
            };
            if let Some(source_id) = parsed.other_provider {
                tombstones.borrow_mut().insert(source_id);
            }
            if let Some(session) = parsed.session {
                result.sessions.push(session);
            }
        }
        result
    } else {
        let mut migration = SyncScanResult::default();
        let mut current_entries = Vec::new();
        let unstable = Cell::new(0);
        for entry in entries {
            let Some(snapshot) = file_scan::file_metadata_snapshot(&entry.stat_target) else {
                current_entries.push(entry);
                continue;
            };
            let Some(mtime_ms) = snapshot.mtime_ms() else {
                current_entries.push(entry);
                continue;
            };
            let needs_migration = context.session_meta().contains_key(&entry.session_id)
                && !metadata_state_is_current_for_mtime(
                    Some(METADATA_PARSER_VERSION),
                    context.metadata_state().get(&entry.session_id).copied(),
                    mtime_ms,
                );
            if !needs_migration {
                current_entries.push(entry);
                continue;
            }
            migration.stats.candidates += 1;
            migration.stats.parsed += 1;
            let Some(parsed) =
                parse_stable_chat_entry_with(entry, mtime_ms, snapshot, parse_chat_entry)?
            else {
                migration.stats.unstable_sessions += 1;
                continue;
            };
            if let Some(source_id) = parsed.other_provider {
                tombstones.borrow_mut().insert(source_id);
            }
            if let Some(session) = parsed.session {
                migration.sessions.push(session);
            }
        }
        let mut result = file_scan::run_file_scan_with_options(
            context,
            if include_events { None } else { since_ts },
            file_scan::FileScanOptions {
                metadata_parser_version: Some(METADATA_PARSER_VERSION),
                event_parser_version: include_events.then_some(EVENT_PARSER_VERSION),
                ..Default::default()
            },
            current_entries,
            |entry, mtime_ms| {
                let Some(snapshot) = file_scan::file_metadata_snapshot(&entry.stat_target) else {
                    unstable.set(unstable.get() + 1);
                    return Ok(None);
                };
                let Some(parsed) =
                    parse_stable_chat_entry_with(entry, mtime_ms, snapshot, parse_chat_entry)?
                else {
                    unstable.set(unstable.get() + 1);
                    return Ok(None);
                };
                if let Some(source_id) = parsed.other_provider {
                    tombstones.borrow_mut().insert(source_id);
                }
                Ok(parsed.session)
            },
        )?;
        result.stats.unstable_sessions += unstable.get();
        result.absorb(migration);
        result
    };
    Ok(SyncScanOutput {
        scan,
        reconcile: Some(ReconcilePlan::ExactTombstones(tombstones.into_inner())),
    })
}

fn parse_stable_chat_entry_with<F>(
    entry: FileScanEntry,
    mtime_ms: i64,
    before: file_scan::FileMetadataSnapshot,
    parse_fn: F,
) -> anyhow::Result<Option<ParsedChatEntry>>
where
    F: FnOnce(FileScanEntry, i64) -> anyhow::Result<ParsedChatEntry>,
{
    let path = entry.stat_target.clone();
    let source_id = entry.session_id.clone();
    let parsed = parse_fn(entry, mtime_ms)?;
    if file_scan::file_metadata_snapshot(&path).as_ref() != Some(&before) {
        warn!(
            "skipping unstable Copilot Chat session {source_id}: source file changed while parsing ({})",
            path.display()
        );
        return Ok(None);
    }
    Ok(Some(parsed))
}

fn vscode_user_roots() -> Vec<PathBuf> {
    let Some(config) = dirs::config_dir() else {
        return Vec::new();
    };
    HOSTS.iter().map(|host| config.join(host).join("User")).filter(|path| path.is_dir()).collect()
}

fn collect_chat_entries(user_roots: &[PathBuf]) -> Vec<FileScanEntry> {
    let mut by_id: HashMap<String, FileScanEntry> = HashMap::new();
    for user_root in user_roots {
        collect_empty_window_entries(user_root, &mut by_id);
        collect_transferred_entries(user_root, &mut by_id);
        collect_workspace_entries(user_root, &mut by_id);
    }
    by_id.into_values().collect()
}

fn collect_empty_window_entries(user_root: &Path, by_id: &mut HashMap<String, FileScanEntry>) {
    let dir = user_root.join("globalStorage").join("emptyWindowChatSessions");
    collect_session_files(&dir, None, by_id);
}

fn collect_transferred_entries(user_root: &Path, by_id: &mut HashMap<String, FileScanEntry>) {
    let dir = user_root.join("globalStorage").join("transferredChatSessions");
    collect_session_files(&dir, None, by_id);
}

fn collect_workspace_entries(user_root: &Path, by_id: &mut HashMap<String, FileScanEntry>) {
    let storage = user_root.join("workspaceStorage");
    let read = match fs::read_dir(&storage) {
        Ok(read) => read,
        Err(error) => {
            debug!("cannot read {}: {error}", storage.display());
            return;
        }
    };
    for entry in read.flatten() {
        let workspace_dir = entry.path();
        if !workspace_dir.is_dir() {
            continue;
        }
        let directory = workspace_folder(&workspace_dir.join("workspace.json"));
        collect_session_files(&workspace_dir.join("chatSessions"), directory, by_id);
    }
}

fn collect_session_files(
    dir: &Path,
    directory: Option<String>,
    by_id: &mut HashMap<String, FileScanEntry>,
) {
    let read = match fs::read_dir(dir) {
        Ok(read) => read,
        Err(_) => return,
    };
    for entry in read.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let ext = path.extension().and_then(|ext| ext.to_str());
        if ext != Some("jsonl") && ext != Some("json") {
            continue;
        }
        let session_id = match path.file_stem().and_then(|name| name.to_str()) {
            Some(name) if !name.is_empty() => name.to_string(),
            _ => continue,
        };
        match by_id.get(&session_id) {
            Some(existing)
                if existing.stat_target.extension().and_then(|ext| ext.to_str())
                    == Some("jsonl")
                    && ext == Some("json") =>
            {
                continue;
            }
            _ => {}
        }
        by_id.insert(
            session_id.clone(),
            FileScanEntry { session_id, stat_target: path, directory: directory.clone() },
        );
    }
}

fn workspace_folder(path: &Path) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&content).ok()?;
    file_uri_to_path(value.get("folder").and_then(|folder| folder.as_str())?)
}

fn parse_chat_session_for_entry(
    entry: FileScanEntry,
    mtime_ms: i64,
) -> anyhow::Result<Option<RawSession>> {
    Ok(parse_chat_entry(entry, mtime_ms)?.session)
}

#[derive(Default)]
struct ParsedChatEntry {
    session: Option<RawSession>,
    other_provider: Option<String>,
}

fn parse_chat_entry(entry: FileScanEntry, mtime_ms: i64) -> anyhow::Result<ParsedChatEntry> {
    let source_file_path = entry.stat_target.to_str().map(str::to_string);
    let Some(doc) = load_chat_document(&entry.stat_target) else {
        return Ok(ParsedChatEntry::default());
    };
    match classify_chat_provider(&doc) {
        ChatProvider::Copilot => {}
        ChatProvider::Other => {
            return Ok(ParsedChatEntry {
                other_provider: Some(entry.session_id),
                ..Default::default()
            });
        }
        ChatProvider::Unknown => return Ok(ParsedChatEntry::default()),
    }
    let mut parsed = messages_from_doc(&doc);
    for event in &mut parsed.events {
        event.source_path = source_file_path.clone();
    }
    if parsed.messages.is_empty() && parsed.events.is_empty() {
        return Ok(ParsedChatEntry::default());
    }
    let started_at =
        first_timestamp(parsed.started_at, &parsed.messages, &[], &parsed.events).unwrap_or(0);
    let updated_at = Some(mtime_ms);
    let mut session = RawSession::search_only(
        entry.session_id,
        entry.directory,
        started_at,
        updated_at,
        None,
        parsed.messages,
    )
    .with_events(parsed.events, EVENT_PARSER_VERSION);
    session.source_file_path = source_file_path;
    session.custom_title = parsed.custom_title;
    session.metadata_parser_version = Some(METADATA_PARSER_VERSION);
    Ok(ParsedChatEntry { session: Some(session), other_provider: None })
}

fn load_chat_document(path: &Path) -> Option<Value> {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("jsonl") => replay_chat_jsonl(path),
        Some("json") => match fs::read_to_string(path) {
            Ok(content) => match serde_json::from_str(&content) {
                Ok(value) => Some(value),
                Err(error) => {
                    debug!("failed to parse copilot chat session {}: {error}", path.display());
                    None
                }
            },
            Err(error) => {
                debug!("failed to read copilot chat session {}: {error}", path.display());
                None
            }
        },
        _ => None,
    }
}

fn replay_chat_jsonl(path: &Path) -> Option<Value> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) => {
            debug!("failed to read copilot chat session {}: {error}", path.display());
            return None;
        }
    };
    let mut lines = BufReader::new(file).lines().map_while(Result::ok).enumerate().peekable();
    let mut doc = None;
    while let Some((index, line)) = lines.next() {
        let is_last = lines.peek().is_none();
        let line = line.trim().trim_start_matches('\u{feff}');
        if line.is_empty() {
            continue;
        }
        let event: Value = match serde_json::from_str(line) {
            Ok(event) => event,
            Err(_) => {
                if !is_last {
                    debug!(
                        "skipping malformed copilot chat jsonl line {} in {}",
                        index + 1,
                        path.display()
                    );
                }
                continue;
            }
        };
        match json_i64(event.get("kind")).unwrap_or(-1) {
            0 => doc = event.get("v").cloned(),
            1 => {
                if let Some(root) = doc.as_mut() {
                    apply_set(root, event.get("k"), event.get("v").cloned().unwrap_or(Value::Null));
                }
            }
            2 => {
                if let Some(root) = doc.as_mut() {
                    let values = event.get("v").and_then(Value::as_array);
                    let truncate_to =
                        json_i64(event.get("i")).and_then(|index| usize::try_from(index).ok());
                    apply_push(root, event.get("k"), values, truncate_to);
                }
            }
            3 => {
                if let Some(root) = doc.as_mut() {
                    apply_delete(root, event.get("k"));
                }
            }
            _ => {}
        }
    }
    doc
}

enum PathSeg {
    Key(String),
    Index(usize),
}

fn path_segs(path: Option<&Value>) -> Option<Vec<PathSeg>> {
    let segments = path?.as_array()?;
    let mut out = Vec::with_capacity(segments.len());
    for segment in segments {
        if let Some(key) = segment.as_str() {
            out.push(PathSeg::Key(key.to_string()));
        } else {
            let index = json_i64(Some(segment)).filter(|index| *index >= 0)?;
            out.push(PathSeg::Index(index as usize));
        }
    }
    Some(out)
}

fn apply_set(root: &mut Value, path: Option<&Value>, value: Value) {
    let Some(segs) = path_segs(path) else {
        return;
    };
    if segs.is_empty() {
        *root = value;
        return;
    }
    let mut current = root;
    for index in 0..segs.len() - 1 {
        current = match get_or_create(current, &segs[index], Some(&segs[index + 1])) {
            Some(next) => next,
            None => return,
        };
    }
    if let Some(last) = segs.last() {
        assign_last(current, last, value);
    }
}

fn apply_push(
    root: &mut Value,
    path: Option<&Value>,
    values: Option<&Vec<Value>>,
    truncate_to: Option<usize>,
) {
    let Some(segs) = path_segs(path) else {
        return;
    };
    if segs.is_empty() {
        return;
    }
    let mut current = root;
    for index in 0..segs.len() - 1 {
        current = match get_or_create(current, &segs[index], Some(&segs[index + 1])) {
            Some(next) => next,
            None => return,
        };
    }
    let Some(last) = segs.last() else {
        return;
    };
    let slot = match last {
        PathSeg::Key(key) => {
            if !current.is_object() {
                *current = Value::Object(Map::new());
            }
            let object = match current.as_object_mut() {
                Some(object) => object,
                None => return,
            };
            object.entry(key.clone()).or_insert_with(|| Value::Array(Vec::new()))
        }
        PathSeg::Index(index) => {
            if !current.is_array() {
                *current = Value::Array(Vec::new());
            }
            let array = match current.as_array_mut() {
                Some(array) => array,
                None => return,
            };
            while array.len() <= *index {
                array.push(Value::Null);
            }
            &mut array[*index]
        }
    };
    if slot.is_null() {
        *slot = Value::Array(Vec::new());
    }
    let Some(array) = slot.as_array_mut() else {
        return;
    };
    if let Some(len) = truncate_to {
        array.truncate(len);
    }
    if let Some(values) = values {
        array.extend(values.iter().cloned());
    }
}

fn apply_delete(root: &mut Value, path: Option<&Value>) {
    let Some(segs) = path_segs(path) else {
        return;
    };
    if segs.is_empty() {
        return;
    }
    let mut current = root;
    for seg in segs.iter().take(segs.len() - 1) {
        current = match descend_existing(current, seg) {
            Some(next) => next,
            None => return,
        };
    }
    match segs.last() {
        Some(PathSeg::Key(key)) => {
            if let Some(object) = current.as_object_mut() {
                object.remove(key);
            }
        }
        Some(PathSeg::Index(index)) => {
            if let Some(array) = current.as_array_mut()
                && *index < array.len()
            {
                array.remove(*index);
            }
        }
        None => {}
    }
}

fn get_or_create<'a>(
    parent: &'a mut Value,
    seg: &PathSeg,
    next: Option<&PathSeg>,
) -> Option<&'a mut Value> {
    let init = match next {
        Some(PathSeg::Index(_)) => Value::Array(Vec::new()),
        _ => Value::Object(Map::new()),
    };
    match seg {
        PathSeg::Key(key) => {
            if !parent.is_object() {
                *parent = Value::Object(Map::new());
            }
            let object = parent.as_object_mut()?;
            if !object.contains_key(key) {
                object.insert(key.clone(), init);
            }
            object.get_mut(key)
        }
        PathSeg::Index(index) => {
            if !parent.is_array() {
                *parent = Value::Array(Vec::new());
            }
            let array = parent.as_array_mut()?;
            while array.len() <= *index {
                array.push(if array.len() == *index { init.clone() } else { Value::Null });
            }
            if array[*index].is_null() {
                array[*index] = init;
            }
            array.get_mut(*index)
        }
    }
}

fn descend_existing<'a>(parent: &'a mut Value, seg: &PathSeg) -> Option<&'a mut Value> {
    match seg {
        PathSeg::Key(key) => parent.as_object_mut()?.get_mut(key),
        PathSeg::Index(index) => parent.as_array_mut()?.get_mut(*index),
    }
}

fn assign_last(parent: &mut Value, seg: &PathSeg, value: Value) {
    match seg {
        PathSeg::Key(key) => {
            if !parent.is_object() {
                *parent = Value::Object(Map::new());
            }
            if let Some(object) = parent.as_object_mut() {
                object.insert(key.clone(), value);
            }
        }
        PathSeg::Index(index) => {
            if !parent.is_array() {
                *parent = Value::Array(Vec::new());
            }
            if let Some(array) = parent.as_array_mut() {
                while array.len() <= *index {
                    array.push(Value::Null);
                }
                array[*index] = value;
            }
        }
    }
}

struct ParsedChat {
    messages: Vec<RawMessage>,
    events: Vec<RawSessionEvent>,
    custom_title: Option<String>,
    started_at: Option<i64>,
}

fn messages_from_doc(doc: &Value) -> ParsedChat {
    let custom_title = doc
        .get("customTitle")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let started_at = json_i64(doc.get("creationDate"));
    let mut messages = Vec::new();
    let mut events = Vec::new();
    let Some(requests) = doc.get("requests").and_then(|value| value.as_array()) else {
        return ParsedChat { messages, events, custom_title, started_at };
    };
    for (request_index, request) in requests.iter().enumerate() {
        let hidden = request.get("hiddenFromTranscript").and_then(Value::as_bool) == Some(true)
            || request.get("isHidden").and_then(Value::as_bool) == Some(true);
        let timestamp = json_i64(request.get("timestamp"))
            .or_else(|| json_i64(request.get("responseTimestamp")));
        let text = request_text(request.get("message"));
        if !hidden && !text.is_empty() {
            messages.push(RawMessage { role: Role::User, content: text, timestamp });
        }
        for (response_index, part) in
            request.get("response").and_then(Value::as_array).into_iter().flatten().enumerate()
        {
            let context = || EventContext {
                event_seq: events.len() as u32,
                timestamp,
                source_path: None,
                source_event_id: request
                    .get("requestId")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                message_seq: messages.len().checked_sub(1).map(|seq| seq as u32),
                parser_version: EVENT_PARSER_VERSION,
            };
            let mut extracted = Vec::new();
            match part.get("kind").and_then(Value::as_str) {
                Some("toolInvocationSerialized") => {
                    let name = part.get("toolId").and_then(Value::as_str).unwrap_or("");
                    let data = part.get("toolSpecificData");
                    let input = data
                        .filter(|data| data.get("kind").and_then(Value::as_str) == Some("input"))
                        .and_then(|data| data.get("rawInput"));
                    let parsed_input = input
                        .and_then(Value::as_str)
                        .and_then(|input| serde_json::from_str::<Value>(input).ok());
                    let input = parsed_input.as_ref().or(input);
                    let mut call = tool_call_event(context(), name.to_string(), input.or(data));
                    call.kind = "tool_call".to_string();
                    call.target = None;
                    call.tool_call_id =
                        part.get("toolCallId").and_then(Value::as_str).map(str::to_string);
                    call.files = copilot_tool_files(name, input);
                    let confirmation = part.get("isConfirmed");
                    call.status = if confirmation.and_then(Value::as_bool) == Some(false)
                        || confirmation.and_then(|value| value.get("type")).and_then(Value::as_u64)
                            == Some(0)
                    {
                        Some("denied".to_string())
                    } else if confirmation
                        .and_then(|value| value.get("type"))
                        .and_then(Value::as_u64)
                        == Some(5)
                    {
                        Some("skipped".to_string())
                    } else {
                        None
                    };
                    let terminal = data.filter(|data| {
                        data.get("kind").and_then(Value::as_str) == Some("terminal")
                    });
                    if terminal.is_some() || name == "run_in_terminal" {
                        call.kind = "command".to_string();
                        let command = terminal
                            .and_then(|data| {
                                ["userEdited", "toolEdited", "original"]
                                    .into_iter()
                                    .find_map(|key| {
                                        data.get("commandLine")
                                            .and_then(|line| line.get(key))
                                            .and_then(Value::as_str)
                                    })
                                    .or_else(|| data.get("command").and_then(Value::as_str))
                            })
                            .or_else(|| {
                                input.and_then(|input| input.get("command")).and_then(Value::as_str)
                            });
                        let raw_cwd = terminal.and_then(|data| data.get("cwd"));
                        let cwd = raw_cwd
                            .and_then(copilot_file_uri)
                            .filter(|cwd| Path::new(cwd).is_absolute());
                        let (files, mut status) = command
                            .map(|command| shell_file_evidence(command, cwd.as_deref()))
                            .unwrap_or((Vec::new(), CommandEvidenceStatus::Unsupported));
                        if raw_cwd.is_some()
                            && cwd.is_none()
                            && status != CommandEvidenceStatus::LimitExceeded
                        {
                            status = CommandEvidenceStatus::Unsupported;
                        }
                        call.files = files;
                        call.command_evidence_status = Some(status);
                    }
                    let result = part.get("resultDetails").filter(|result| !result.is_null());
                    let terminal_output =
                        terminal.and_then(|data| data.get("terminalCommandOutput"));
                    let exit_code = terminal
                        .and_then(|data| data.pointer("/terminalCommandState/exitCode"))
                        .and_then(Value::as_i64);
                    let call_id = call.tool_call_id.clone();
                    extracted.push(call);
                    if result.is_some() || terminal_output.is_some() || exit_code.is_some() {
                        let mut outcome = tool_result_event(
                            context(),
                            Some(name.to_string()),
                            result.or(terminal_output).map(Value::to_string),
                        );
                        outcome.tool_call_id = call_id;
                        outcome.status = result
                            .and_then(|result| result.get("isError"))
                            .and_then(Value::as_bool)
                            .map(|error| if error { "error" } else { "success" }.to_string())
                            .or_else(|| {
                                exit_code.map(|code| {
                                    if code == 0 { "success" } else { "error" }.to_string()
                                })
                            });
                        extracted.push(outcome);
                    }
                }
                Some("textEditGroup") => {
                    let mut event =
                        tool_call_event(context(), "textEditGroup".to_string(), Some(part));
                    event.kind = "file_write".to_string();
                    event.target = None;
                    if let Some(path) = part.get("uri").and_then(copilot_file_uri) {
                        event.files.push(FileEvidence {
                            path,
                            operation: FileOperation::Write,
                            kind: FileEvidenceKind::Observation,
                            cwd: None,
                            target: None,
                        });
                    }
                    extracted.push(event);
                }
                _ => {}
            }
            for mut event in extracted {
                event.event_seq = events.len() as u32;
                event.visibility = Some(
                    if hidden
                        || part.get("presentation").and_then(Value::as_str) == Some("hidden")
                        || (part.get("presentation").and_then(Value::as_str)
                            == Some("hiddenAfterComplete")
                            && part.get("isComplete").and_then(Value::as_bool) == Some(true))
                    {
                        EvidenceVisibility::Hidden
                    } else {
                        EvidenceVisibility::Visible
                    },
                );
                if event.visibility == Some(EvidenceVisibility::Hidden) {
                    event.message_seq = None;
                }
                event.attrs_json = Some(
                    serde_json::json!({
                        "requestId": request.get("requestId"),
                        "request_index": request_index,
                        "response_index": response_index,
                        "hiddenFromTranscript": request.get("hiddenFromTranscript"),
                        "isHidden": request.get("isHidden"),
                        "part": part
                    })
                    .to_string(),
                );
                events.push(event);
            }
        }
        let assistant = response_text(request.get("response"));
        if !hidden && !assistant.is_empty() {
            messages.push(RawMessage { role: Role::Assistant, content: assistant, timestamp });
        }
    }
    ParsedChat { messages, events, custom_title, started_at }
}

fn copilot_tool_files(name: &str, input: Option<&Value>) -> Vec<FileEvidence> {
    let Some(input) = input else {
        return Vec::new();
    };
    if name == "copilot_applyPatch" {
        return input
            .get("input")
            .and_then(Value::as_str)
            .map(patch_file_evidence)
            .unwrap_or_default();
    }
    let (paths, operation) = match name {
        "copilot_readFile" => (vec![input.get("filePath")], FileOperation::Read),
        "copilot_createFile" | "copilot_insertEdit" | "copilot_replaceString" => {
            (vec![input.get("filePath")], FileOperation::Write)
        }
        "copilot_multiReplaceString" => (
            input
                .get("replacements")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|replacement| replacement.get("filePath"))
                .collect(),
            FileOperation::Write,
        ),
        _ => return Vec::new(),
    };
    paths
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|path| !path.trim().is_empty())
        .map(|path| FileEvidence {
            path: path.to_string(),
            operation: operation.clone(),
            kind: FileEvidenceKind::Call,
            cwd: None,
            target: None,
        })
        .collect()
}

fn copilot_file_uri(value: &Value) -> Option<String> {
    if let Some(uri) = value.as_str() {
        return file_uri_to_path(uri);
    }
    if value.get("scheme").and_then(Value::as_str) != Some("file") {
        return None;
    }
    let path = value.get("path").and_then(Value::as_str).filter(|path| !path.trim().is_empty())?;
    let authority = value.get("authority").and_then(Value::as_str).unwrap_or("");
    if authority.is_empty() { Some(path.to_string()) } else { Some(format!("//{authority}{path}")) }
}

fn request_text(message: Option<&Value>) -> String {
    let Some(message) = message else {
        return String::new();
    };
    if let Some(text) = message.as_str().map(str::trim).filter(|text| !text.is_empty()) {
        return text.to_string();
    }
    if let Some(text) =
        message.get("text").and_then(Value::as_str).map(str::trim).filter(|text| !text.is_empty())
    {
        return text.to_string();
    }
    let Some(parts) = message.get("parts").and_then(Value::as_array) else {
        return String::new();
    };
    parts
        .iter()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChatProvider {
    Copilot,
    Other,
    Unknown,
}

fn classify_chat_provider(session: &Value) -> ChatProvider {
    let responder = session
        .get("responderUsername")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty());
    if responder.is_some_and(|name| name.eq_ignore_ascii_case("github copilot")) {
        return ChatProvider::Copilot;
    }
    let mut saw_other = responder.is_some();
    for id in session
        .get("requests")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|request| {
            request
                .pointer("/agent/extensionId/value")
                .or_else(|| request.pointer("/agent/extensionId"))
                .and_then(Value::as_str)
        })
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        if id.to_ascii_lowercase().starts_with("github.copilot") {
            return ChatProvider::Copilot;
        }
        saw_other = true;
    }
    if saw_other { ChatProvider::Other } else { ChatProvider::Unknown }
}

fn response_text(response: Option<&Value>) -> String {
    let Some(items) = response.and_then(|value| value.as_array()) else {
        return String::new();
    };
    let mut parts = Vec::new();
    for item in items {
        let Some(object) = item.as_object() else {
            continue;
        };
        match object.get("kind").and_then(|value| value.as_str()) {
            None | Some("markdownContent") => {
                if let Some(text) = object
                    .get("value")
                    .and_then(|value| value.as_str())
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                {
                    parts.push(text.to_string());
                }
            }
            _ => {}
        }
    }
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{schema, store::Store};
    use crate::types::Session;

    fn setup_store() -> Store {
        schema::register_sqlite_vec();
        Store::open_in_memory().unwrap()
    }

    fn temp_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "recall-cch-test-{}-{}",
            label,
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_jsonl(path: &Path, lines: &[&str]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, lines.join("\n") + "\n").unwrap();
    }

    fn sample_jsonl_lines() -> [&'static str; 4] {
        [
            r#"{"kind":0,"v":{"version":3,"creationDate":1700000000000,"customTitle":"Fix login","sessionId":"sess-1","requests":[],"lastMessageDate":1700000005000}}"#,
            r#"{"kind":1,"k":["requests",0],"v":{"requestId":"r1","timestamp":1700000001000,"message":{"text":"hello"},"response":[]}}"#,
            r#"{"kind":1,"k":["requests",0,"response"],"v":[{"kind":"thinking","value":"plan"},{"kind":"toolInvocationSerialized","toolId":"copilot_createFile","toolCallId":"jsonl-call","toolSpecificData":{"kind":"input","rawInput":{"filePath":"/tmp/new.rs","content":"new"}},"isComplete":true,"isConfirmed":{"type":5}},{"kind":"textEditGroup","uri":{"scheme":"file","path":"/tmp/new.rs"},"edits":[[{"range":{"startLineNumber":1,"startColumn":1,"endLineNumber":1,"endColumn":1},"text":"new"}]],"done":true},{"value":"Hi there."}]}"#,
            r#"{"kind":1,"k":["responderUsername"],"v":"GitHub Copilot"}"#,
        ]
    }

    #[test]
    fn parse_jsonl_replays_patches_and_skips_thinking() {
        let root = temp_root("jsonl");
        let path = root.join("sess-1.jsonl");
        write_jsonl(&path, &sample_jsonl_lines());

        let session = parse_chat_session_for_entry(
            FileScanEntry {
                session_id: "sess-1".to_string(),
                stat_target: path,
                directory: Some("/tmp/proj".to_string()),
            },
            99,
        )
        .unwrap()
        .unwrap();

        assert_eq!(session.source_id, "sess-1");
        assert_eq!(session.directory.as_deref(), Some("/tmp/proj"));
        assert_eq!(session.custom_title.as_deref(), Some("Fix login"));
        assert_eq!(session.started_at, 1_700_000_000_000);
        assert_eq!(session.updated_at, Some(99));
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, Role::User);
        assert_eq!(session.messages[0].content, "hello");
        assert_eq!(session.messages[1].role, Role::Assistant);
        assert_eq!(session.messages[1].content, "Hi there.");
        assert_eq!(session.events.len(), 2);
        assert_eq!(session.events[0].status.as_deref(), Some("skipped"));
        assert_eq!(session.events[0].tool_call_id.as_deref(), Some("jsonl-call"));
        assert_eq!(session.events[0].files[0].kind, FileEvidenceKind::Call);
        assert_eq!(session.events[1].files[0].kind, FileEvidenceKind::Observation);
        assert_eq!(session.events[1].source_event_id.as_deref(), Some("r1"));
        assert!(session.events[1].tool_call_id.is_none());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_json_snapshot_extracts_requests() {
        let root = temp_root("json");
        let path = root.join("sess-2.json");
        let payload = "完整".repeat(3000);
        let doc = serde_json::json!({
            "version": 3, "sessionId": "sess-2", "creationDate": 1700000000000i64,
            "customTitle": "Old chat", "responderUsername": "GitHub Copilot",
            "requests": [{"requestId": "request-2", "timestamp": 1700000001000i64, "message": {"text": "translate this"}, "response": [
                {"kind": "toolInvocationSerialized", "toolId": "copilot_readFile", "toolCallId": "read-1", "isComplete": true, "isConfirmed": true, "toolSpecificData": {"kind": "input", "rawInput": {"filePath": "/tmp/a.rs"}}, "resultDetails": {"input": "read", "output": [{"type": "embed", "value": payload}], "isError": false}},
                {"kind": "toolInvocationSerialized", "toolId": "copilot_multiReplaceString", "toolCallId": "edit-1", "isComplete": true, "isConfirmed": {"type": 0}, "toolSpecificData": {"kind": "input", "rawInput": {"replacements": [{"filePath": "/tmp/a.rs", "newString": "new"}, {"filePath": "/tmp/b.rs", "newString": "other"}]}}},
                {"kind": "toolInvocationSerialized", "toolId": "copilot_applyPatch", "toolCallId": "patch-1", "isComplete": true, "toolSpecificData": {"kind": "input", "rawInput": {"input": "*** Begin Patch\n*** Delete File: a.rs\n*** Add File: c.rs\n+new\n*** End Patch"}}, "resultDetails": {"input": "patch", "output": "partial failure", "isError": true}},
                {"kind": "textEditGroup", "uri": {"scheme": "file", "path": "/tmp/b.rs"}, "edits": [[{"range": {"startLineNumber": 1, "startColumn": 1, "endLineNumber": 1, "endColumn": 1}, "text": "new"}]], "state": {"sha1": "hash", "applied": 0}, "done": true},
                {"kind": "toolInvocationSerialized", "toolId": "other", "toolCallId": "refs", "isComplete": true, "resultDetails": [{"scheme": "file", "path": "/tmp/reference-only.rs"}]},
                {"kind": "toolInvocationSerialized", "toolId": "run_in_terminal", "toolCallId": "cmd", "isComplete": true, "toolSpecificData": {"kind": "terminal", "commandLine": {"original": "mv wrong-original wrong-target", "userEdited": "mv before.rs after.rs", "toolEdited": "mv wrong-tool wrong-target", "forDisplay": "mv wrong-display wrong-target"}, "cwd": {"scheme": "file", "path": "/actual/terminal"}, "terminalCommandOutput": {"text": "failed", "truncated": true}, "terminalCommandState": {"exitCode": 1}}},
                {"value": "done"}
            ]}]
        });
        fs::write(&path, doc.to_string()).unwrap();

        let session = parse_chat_session_for_entry(
            FileScanEntry {
                session_id: "sess-2".to_string(),
                stat_target: path.clone(),
                directory: None,
            },
            1,
        )
        .unwrap()
        .unwrap();

        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].content, "translate this");
        assert_eq!(session.messages[1].content, "done");
        assert_eq!(session.custom_title.as_deref(), Some("Old chat"));
        assert_eq!(session.events.len(), 10);
        assert_eq!(session.events[0].files[0].operation, FileOperation::Read);
        assert_eq!(session.events[1].status.as_deref(), Some("success"));
        assert!(session.events[1].attrs_json.as_deref().unwrap().contains(&payload));
        assert!(session.events[1].files.is_empty());
        assert_eq!(session.events[2].status.as_deref(), Some("denied"));
        assert_eq!(
            session.events[2].files.iter().map(|file| file.path.as_str()).collect::<Vec<_>>(),
            vec!["/tmp/a.rs", "/tmp/b.rs"]
        );
        assert_eq!(session.events[3].files[0].operation, FileOperation::Delete);
        assert!(session.events[3].status.is_none());
        assert_eq!(session.events[4].status.as_deref(), Some("error"));
        assert_eq!(session.events[5].files[0].kind, FileEvidenceKind::Observation);
        assert!(session.events[5].tool_call_id.is_none());
        assert!(session.events[5].status.is_none());
        assert!(session.events[6].files.is_empty());
        assert!(session.events[7].files.is_empty());
        assert!(session.events[7].status.is_none());
        assert_eq!(session.events[8].kind, "command");
        assert_eq!(
            session.events[8].command_evidence_status,
            Some(CommandEvidenceStatus::Complete)
        );
        assert_eq!(
            session.events[8].files.iter().map(|file| file.path.as_str()).collect::<Vec<_>>(),
            vec!["before.rs", "after.rs"]
        );
        assert!(
            session.events[8]
                .files
                .iter()
                .all(|file| file.cwd.as_deref() == Some("/actual/terminal")
                    && file.kind == FileEvidenceKind::Command)
        );
        assert!(session.events[8].status.is_none());
        assert_eq!(session.events[9].status.as_deref(), Some("error"));
        assert!(
            session
                .events
                .iter()
                .all(|event| event.source_event_id.as_deref() == Some("request-2")
                    && event.message_seq == Some(0))
        );

        for data in [
            serde_json::json!({"kind": "terminal", "commandLine": {"original": "mv old.rs new.rs", "toolEdited": "mv chosen.rs target.rs"}}),
            serde_json::json!({"kind": "terminal", "command": "mv chosen.rs target.rs", "language": "shellscript"}),
            serde_json::json!({"kind": "input", "rawInput": "{\"command\":\"mv chosen.rs target.rs\"}"}),
        ] {
            let mut doc = doc.clone();
            doc["requests"][0]["response"] = serde_json::json!([{"kind": "toolInvocationSerialized", "toolId": "run_in_terminal", "toolCallId": "cwd-unknown", "toolSpecificData": data}]);
            fs::write(&path, doc.to_string()).unwrap();
            let session = parse_chat_session_for_entry(
                FileScanEntry {
                    session_id: "sess-2".to_string(),
                    stat_target: path.clone(),
                    directory: Some("/misleading/workspace".to_string()),
                },
                1,
            )
            .unwrap()
            .unwrap();
            let event = &session.events[0];
            assert_eq!(event.command_evidence_status, Some(CommandEvidenceStatus::Unsupported));
            assert_eq!(event.files[0].path, "chosen.rs");
            assert!(event.files.iter().all(|file| file.cwd.is_none() && file.target.is_none()));
            assert!(event.status.is_none());
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn empty_requests_are_skipped() {
        let root = temp_root("empty");
        let path = root.join("empty.jsonl");
        write_jsonl(&path, &[r#"{"kind":0,"v":{"version":3,"sessionId":"empty","requests":[]}}"#]);
        let session = parse_chat_session_for_entry(
            FileScanEntry { session_id: "empty".to_string(), stat_target: path, directory: None },
            1,
        )
        .unwrap();
        assert!(session.is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn truncated_jsonl_keeps_replayed_prefix() {
        let root = temp_root("trunc");
        let path = root.join("trunc.jsonl");
        write_jsonl(
            &path,
            &[
                r#"{"kind":0,"v":{"version":3,"creationDate":1,"responderUsername":"GitHub Copilot","requests":[{"timestamp":2,"message":{"text":"keep"},"response":[{"value":"ok"}]}]}}"#,
                r#"{"kind":1,"k":["requests",0,"response""#,
            ],
        );
        let session = parse_chat_session_for_entry(
            FileScanEntry { session_id: "trunc".to_string(), stat_target: path, directory: None },
            1,
        )
        .unwrap()
        .unwrap();
        assert_eq!(session.messages[0].content, "keep");
        assert_eq!(session.messages[1].content, "ok");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn collect_prefers_jsonl_over_json_and_reads_workspace_folder() {
        let root = temp_root("collect");
        let user = root.join("User");
        let ws = user.join("workspaceStorage").join("abc");
        fs::create_dir_all(ws.join("chatSessions")).unwrap();
        fs::write(ws.join("workspace.json"), r#"{"folder":"file:///Users/x/git/foo%20bar"}"#)
            .unwrap();
        write_jsonl(&ws.join("chatSessions").join("sess-1.jsonl"), &sample_jsonl_lines());
        fs::write(ws.join("chatSessions").join("sess-1.json"), r#"{"requests":[]}"#).unwrap();
        fs::create_dir_all(user.join("globalStorage").join("emptyWindowChatSessions")).unwrap();
        write_jsonl(
            &user.join("globalStorage").join("emptyWindowChatSessions").join("empty.jsonl"),
            &[r#"{"kind":0,"v":{"version":3,"requests":[]}}"#],
        );

        let entries = collect_chat_entries(&[user]);
        assert_eq!(entries.len(), 2);
        let sess = entries.iter().find(|entry| entry.session_id == "sess-1").unwrap();
        assert!(sess.stat_target.extension().unwrap() == "jsonl");
        assert_eq!(sess.directory.as_deref(), Some("/Users/x/git/foo bar"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn jsonl_kind_two_pushes_and_kind_three_deletes() {
        let root = temp_root("push");
        let path = root.join("sess-push.jsonl");
        write_jsonl(
            &path,
            &[
                r#"{"kind":0,"v":{"version":3,"responderUsername":"GitHub Copilot","requests":[{"timestamp":1,"message":{"text":"hello"},"response":[]}]}}"#,
                r#"{"kind":2,"k":["requests",0,"response"],"v":[{"value":"Hi."}]}"#,
                r#"{"kind":3,"k":["requests",0,"response",0]}"#,
                r#"{"kind":2,"k":["requests",0,"response"],"v":[{"value":"Hello."}]}"#,
            ],
        );
        let session = parse_chat_session_for_entry(
            FileScanEntry {
                session_id: "sess-push".to_string(),
                stat_target: path,
                directory: None,
            },
            1,
        )
        .unwrap()
        .unwrap();
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[1].content, "Hello.");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn markdown_content_parts_are_indexed() {
        let doc = serde_json::json!({
            "responderUsername": "GitHub Copilot",
            "requests": [{
                "message": {"text": "q"},
                "response": [{"kind":"markdownContent","value":"answer"}]
            }]
        });
        let parsed = messages_from_doc(&doc);
        assert_eq!(parsed.messages[1].content, "answer");
    }

    #[test]
    fn other_chat_providers_are_skipped() {
        let root = temp_root("other");
        let path = root.join("other.json");
        fs::write(
            &path,
            r#"{"responderUsername":"Other Agent","requests":[{"message":{"text":"nope"},"response":[{"value":"nope"}]}]}"#,
        )
        .unwrap();
        let session = parse_chat_session_for_entry(
            FileScanEntry { session_id: "other".to_string(), stat_target: path, directory: None },
            1,
        )
        .unwrap();
        assert!(session.is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn explicit_other_provider_is_distinct_from_unknown() {
        let other = serde_json::json!({"responderUsername": "Other Agent"});
        let unknown = serde_json::json!({"requests": [{"message": {"text": "hello"}}]});

        assert_ne!(classify_chat_provider(&other), classify_chat_provider(&unknown));
    }

    #[test]
    fn unstable_provider_file_produces_no_classification() {
        let root = temp_root("unstable-provider");
        let path = root.join("changing.json");
        fs::write(&path, r#"{"responderUsername":"Other Agent","requests":[]}"#).unwrap();
        let snapshot = file_scan::file_metadata_snapshot(&path).unwrap();
        let mtime_ms = snapshot.mtime_ms().unwrap();
        let parsed = parse_stable_chat_entry_with(
            FileScanEntry {
                session_id: "changing".to_string(),
                stat_target: path.clone(),
                directory: None,
            },
            mtime_ms,
            snapshot,
            |entry, mtime_ms| {
                let parsed = parse_chat_entry(entry, mtime_ms)?;
                fs::write(
                    &path,
                    r#"{"responderUsername":"GitHub Copilot","requests":[{"message":{"text":"changed"}}]}"#,
                )?;
                Ok(parsed)
            },
        )
        .unwrap();
        assert!(parsed.is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn sync_tombstones_only_explicit_other_provider_and_can_rebuild() {
        let root = temp_root("reconcile");
        let user = root.join("User");
        let dir = user.join("globalStorage").join("emptyWindowChatSessions");
        fs::create_dir_all(&dir).unwrap();
        let other_path = dir.join("other.json");
        fs::write(
            &other_path,
            r#"{"responderUsername":"Other Agent","requests":[{"message":{"text":"other"},"response":[{"value":"answer"}]}]}"#,
        )
        .unwrap();
        fs::write(
            dir.join("unknown.json"),
            r#"{"requests":[{"message":{"text":"unknown"},"response":[{"value":"answer"}]}]}"#,
        )
        .unwrap();
        fs::write(
            dir.join("copilot.json"),
            r#"{"responderUsername":"GitHub Copilot","requests":[{"message":{"text":"copilot"},"response":[{"value":"answer"}]}]}"#,
        )
        .unwrap();

        let store = setup_store();
        for source_id in ["other", "unknown"] {
            store
                .insert_session(&Session {
                    id: format!("internal-{source_id}"),
                    source: "copilot-chat".to_string(),
                    source_id: source_id.to_string(),
                    title: "existing".to_string(),
                    directory: None,
                    repo_remote: None,
                    repo_slug: None,
                    repo_name: None,
                    started_at: 0,
                    updated_at: file_scan::stat_mtime_ms(&dir.join(format!("{source_id}.json"))),
                    message_count: 2,
                    entrypoint: None,
                    custom_title: None,
                    summary: None,
                    duration_minutes: None,
                    source_file_path: None,
                    is_import: false,
                })
                .unwrap();
        }
        let context = AdapterSyncContext::from_store_for_test(&store, "copilot-chat").unwrap();
        let output = scan_chat_entries_for_sync(
            collect_chat_entries(std::slice::from_ref(&user)),
            &context,
            Some(i64::MAX),
            false,
            false,
        )
        .unwrap();
        assert!(matches!(
            output.reconcile,
            Some(ReconcilePlan::ExactTombstones(ids)) if ids == HashSet::from(["other".to_string()])
        ));
        assert!(output.scan.sessions.is_empty());

        fs::write(
            &other_path,
            r#"{"responderUsername":"GitHub Copilot","requests":[{"message":{"text":"restored"},"response":[{"value":"answer"}]}]}"#,
        )
        .unwrap();
        let rebuilt = scan_chat_entries_for_sync(
            collect_chat_entries(&[user]),
            &AdapterSyncContext::empty_for_test("copilot-chat"),
            None,
            true,
            true,
        )
        .unwrap();
        assert!(matches!(
            rebuilt.reconcile,
            Some(ReconcilePlan::ExactTombstones(ids)) if ids.is_empty()
        ));
        assert!(rebuilt.scan.sessions.iter().any(|session| session.source_id == "other"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn collect_includes_transferred_chat_sessions() {
        let root = temp_root("xfer");
        let user = root.join("User");
        write_jsonl(
            &user.join("globalStorage").join("transferredChatSessions").join("moved.jsonl"),
            &sample_jsonl_lines(),
        );
        let entries = collect_chat_entries(&[user]);
        assert!(entries.iter().any(|entry| entry.session_id == "moved"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn hidden_requests_are_skipped() {
        let doc = serde_json::json!({
            "creationDate": 1,
            "responderUsername": "GitHub Copilot",
            "requests": [{"message": {"text": "preceding"}, "response": [{"value": "preceding answer"}]}, {
                "hiddenFromTranscript": true,
                "timestamp": 2,
                "message": {"text": "secret"},
                "response": [{"value": "nope"}, {"kind": "toolInvocationSerialized", "toolId": "copilot_createFile", "toolCallId": "hidden-call", "isComplete": true, "toolSpecificData": {"kind": "input", "rawInput": {"filePath": "/tmp/hidden.rs", "content": "secret"}}}, {"kind": "textEditGroup", "uri": "file:///tmp/hidden.rs", "edits": [], "done": true}]
            }, {
                "timestamp": 3,
                "message": {"text": "visible"},
                "response": [{"value": "ok"}, {"kind": "toolInvocationSerialized", "toolId": "copilot_createFile", "presentation": "hidden", "toolSpecificData": {"kind": "input", "rawInput": {"filePath": "/tmp/presentation.rs"}}}, {"kind": "toolInvocationSerialized", "toolId": "copilot_createFile", "presentation": "hiddenAfterComplete", "isComplete": true}]
            }]
        });
        let parsed = messages_from_doc(&doc);
        assert_eq!(parsed.messages.len(), 4);
        assert_eq!(parsed.messages[0].content, "preceding");
        assert_eq!(parsed.messages[2].content, "visible");
        assert_eq!(parsed.events.len(), 4);
        assert!(
            parsed.events.iter().all(|event| event.visibility == Some(EvidenceVisibility::Hidden)
                && event.message_seq.is_none()
                && event.status.is_none())
        );
        assert_eq!(parsed.events[0].tool_call_id.as_deref(), Some("hidden-call"));
        assert_eq!(parsed.events[0].files[0].kind, FileEvidenceKind::Call);
        assert_eq!(parsed.events[1].files[0].kind, FileEvidenceKind::Observation);
    }

    #[test]
    fn scan_for_sync_skips_unchanged_session() {
        let root = temp_root("skip");
        let user = root.join("User");
        let path = user.join("globalStorage").join("emptyWindowChatSessions").join("sess-1.jsonl");
        write_jsonl(&path, &sample_jsonl_lines());
        let mtime = file_scan::stat_mtime_ms(&path).unwrap();

        let mut old = parse_chat_session_for_entry(
            FileScanEntry {
                session_id: "sess-1".to_string(),
                stat_target: path.clone(),
                directory: None,
            },
            mtime,
        )
        .unwrap()
        .unwrap();
        old.event_parser_version = Some(EVENT_PARSER_VERSION - 1);
        let store =
            crate::sync::persist_raw_session_for_conformance(setup_store(), "copilot-chat", old)
                .unwrap();
        let entries = collect_chat_entries(&[user]);
        let context = AdapterSyncContext::from_store_for_test(&store, "copilot-chat").unwrap();
        let unchanged =
            scan_chat_entries_for_sync(entries.clone(), &context, None, false, false).unwrap();
        assert!(unchanged.scan.sessions.is_empty());
        assert_eq!(unchanged.scan.stats.skipped_sessions, 1);
        let backfill =
            scan_chat_entries_for_sync(entries.clone(), &context, Some(i64::MAX), true, false)
                .unwrap();
        assert_eq!(backfill.scan.sessions.len(), 1);
        let fresh = backfill.scan.sessions.into_iter().next().unwrap();
        let store =
            crate::sync::persist_raw_session_for_conformance(store, "copilot-chat", fresh).unwrap();
        let context = AdapterSyncContext::from_store_for_test(&store, "copilot-chat").unwrap();
        let unchanged = scan_chat_entries_for_sync(entries, &context, None, true, false).unwrap();
        assert!(unchanged.scan.sessions.is_empty());
        assert_eq!(unchanged.scan.stats.skipped_sessions, 1);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_for_sync_reparses_when_same_turn_response_grows() {
        use std::time::{Duration, UNIX_EPOCH};

        let root = temp_root("same-turn");
        let user = root.join("User");
        let path = user.join("globalStorage").join("emptyWindowChatSessions").join("sess-1.jsonl");
        write_jsonl(&path, &sample_jsonl_lines());
        let first_mtime = 1_700_000_000_000i64;
        fs::File::open(&path)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_millis(first_mtime as u64))
            .unwrap();

        let store = setup_store();
        store
            .insert_session(&Session {
                id: "internal-sess-1".to_string(),
                source: "copilot-chat".to_string(),
                source_id: "sess-1".to_string(),
                title: "existing".to_string(),
                directory: None,
                repo_remote: None,
                repo_slug: None,
                repo_name: None,
                started_at: 0,
                updated_at: Some(first_mtime),
                message_count: 2,
                entrypoint: None,
                custom_title: None,
                summary: None,
                duration_minutes: None,
                source_file_path: None,
                is_import: false,
            })
            .unwrap();

        let later_mtime = first_mtime + 5_000;
        write_jsonl(
            &path,
            &[
                sample_jsonl_lines()[0],
                sample_jsonl_lines()[1],
                r#"{"kind":1,"k":["requests",0,"response"],"v":[{"kind":"thinking","value":"plan"},{"value":"Hi there. Full answer."}]}"#,
                sample_jsonl_lines()[3],
            ],
        );
        fs::File::open(&path)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_millis(later_mtime as u64))
            .unwrap();

        let entries = collect_chat_entries(&[user]);
        let result = file_scan::run_file_scan_with_options(
            &AdapterSyncContext::from_store_for_test(&store, "copilot-chat").unwrap(),
            None,
            Default::default(),
            entries,
            parse_chat_session_for_entry,
        )
        .unwrap();
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].updated_at, Some(later_mtime));
        assert_eq!(result.sessions[0].messages.len(), 2);
        assert_eq!(result.sessions[0].messages[1].content, "Hi there. Full answer.");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn delete_patch_removes_object_key() {
        let mut doc = serde_json::json!({"pendingRequests":[{"id":1}],"requests":[]});
        apply_delete(&mut doc, Some(&serde_json::json!(["pendingRequests"])));
        assert!(doc.get("pendingRequests").is_none());
    }
}
