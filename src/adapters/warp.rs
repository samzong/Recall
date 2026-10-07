use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDateTime};
use prost::Message as _;
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use tracing::warn;

use crate::adapters::events::{self, EventContext};
use crate::adapters::{
    AdapterSyncContext, RawMessage, RawSession, ResumeCommand, SourceAdapter, SyncScanOutput,
    SyncScanResult,
};
use crate::types::{
    FileEvidence, FileOperation, ParentLink, ParentRelation, RawSessionEvent, RawUsageEvent, Role,
    ThreadRole,
};

mod proto;

#[cfg(test)]
mod tests;

const METADATA_PARSER_VERSION: u32 = 1;
const EVENT_PARSER_VERSION: u32 = 3;
const USAGE_PARSER_VERSION: u32 = 1;

pub(crate) struct WarpAdapter;

impl SourceAdapter for WarpAdapter {
    fn id(&self) -> &str {
        "warp"
    }

    fn label(&self) -> &str {
        "WP"
    }

    fn usage_parser_version(&self) -> Option<u32> {
        Some(USAGE_PARSER_VERSION)
    }

    fn resume_command(&self, _source_id: &str) -> Option<ResumeCommand> {
        None
    }

    fn scan(&self) -> Result<Vec<RawSession>> {
        Ok(scan_paths(&default_db_paths(), None, None, true)?.sessions)
    }

    fn scan_for_sync_output(
        &self,
        context: &AdapterSyncContext,
        since_ts: Option<i64>,
        include_events: bool,
        force: bool,
    ) -> Result<Option<SyncScanOutput>> {
        Ok(Some(SyncScanOutput {
            scan: scan_paths(
                &default_db_paths(),
                Some(context),
                if force { None } else { since_ts },
                include_events,
            )?,
            reconcile: None,
        }))
    }
}

fn default_db_paths() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let dirs = if cfg!(target_os = "macos") {
        ["Stable", "Preview"]
            .into_iter()
            .flat_map(|channel| {
                let app = format!("dev.warp.Warp-{channel}");
                [
                    home.join(
                        "Library/Group Containers/2BBY89MBSN.dev.warp/Library/Application Support",
                    )
                    .join(&app),
                    home.join("Library/Application Support").join(app),
                ]
            })
            .collect::<Vec<_>>()
    } else if cfg!(target_os = "windows") {
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData/Local"));
        ["Warp", "WarpPreview"].map(|app| base.join("warp").join(app).join("data")).to_vec()
    } else {
        let base = std::env::var_os("XDG_STATE_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/state"));
        ["warp-terminal", "warp-terminal-preview"].map(|app| base.join(app)).to_vec()
    };
    dirs.into_iter()
        .flat_map(|dir| [dir.join("warp.sqlite"), dir.join("tui/warp.sqlite")])
        .collect()
}

fn scan_paths(
    paths: &[PathBuf],
    context: Option<&AdapterSyncContext>,
    since_ts: Option<i64>,
    include_events: bool,
) -> Result<SyncScanResult> {
    let mut result = SyncScanResult::default();
    let mut seen = HashSet::new();
    for path in paths {
        if let Err(error) =
            scan_db_with_seen(path, context, since_ts, include_events, &mut seen, &mut result)
        {
            if error.is::<SessionWriteError>() {
                return Err(error);
            }
            warn!("failed to scan Warp database {}: {error:#}", path.display());
        }
    }
    Ok(result)
}

#[derive(Debug)]
struct SessionWriteError;

impl std::fmt::Display for SessionWriteError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("writing Warp session")
    }
}

impl std::error::Error for SessionWriteError {}

fn has_table(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        [name],
        |row| row.get(0),
    )?)
}

#[cfg(test)]
fn scan_db(
    path: &Path,
    context: Option<&AdapterSyncContext>,
    include_events: bool,
) -> Result<SyncScanResult> {
    let mut result = SyncScanResult::default();
    scan_db_with_seen(path, context, None, include_events, &mut HashSet::new(), &mut result)?;
    Ok(result)
}

fn scan_db_with_seen(
    path: &Path,
    context: Option<&AdapterSyncContext>,
    since_ts: Option<i64>,
    include_events: bool,
    seen: &mut HashSet<String>,
    result: &mut SyncScanResult,
) -> Result<()> {
    if !path.is_file() {
        return Ok(());
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .context("opening Warp database read-only")?;
    conn.busy_timeout(Duration::from_secs(3))?;
    let snapshot = conn.unchecked_transaction()?;
    if !has_table(&snapshot, "agent_conversations")? || !has_table(&snapshot, "agent_tasks")? {
        return Ok(());
    }
    let mut stmt = snapshot.prepare(
        "SELECT conversation_id, last_modified_at, conversation_data FROM agent_conversations
         WHERE (?1 IS NULL OR conversation_id = ?1) ORDER BY conversation_id",
    )?;
    let mut rows = stmt.query([context.and_then(AdapterSyncContext::target_source_id)])?;
    while let Some(row) = rows.next()? {
        result.stats.candidates += 1;
        let parsed = (|| {
            let id: String = row.get(0)?;
            let modified: String = row.get(1)?;
            if let Some(cutoff) = since_ts
                && before_cutoff(&snapshot, &id, &modified, cutoff)?
            {
                result.stats.rejected_before_parse += 1;
                return Ok(None);
            }
            let data: String = row.get(2)?;
            let Some(mut raw) =
                parse_conversation(&snapshot, &id, &modified, path, include_events)?
            else {
                return Ok(None);
            };
            match serde_json::from_str::<Value>(&data) {
                Ok(data) => {
                    if let Some(parent) = data
                        .get("parent_conversation_id")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|parent| !parent.is_empty())
                    {
                        raw.thread_role = Some(ThreadRole::Subagent);
                        raw.parent_links = vec![ParentLink {
                            relation: ParentRelation::Spawn,
                            source: "warp".into(),
                            source_id: parent.into(),
                        }];
                    }
                }
                Err(error) => warn!("failed to parse Warp conversation metadata: {error}"),
            }
            raw.metadata_parser_version = Some(METADATA_PARSER_VERSION);
            Ok::<_, anyhow::Error>(Some(raw))
        })();
        match parsed {
            Ok(Some(raw)) => {
                if !seen.insert(raw.source_id.clone()) {
                    result.stats.filtered_sessions += 1;
                    continue;
                }
                result.stats.parsed += 1;
                if let Some(raw) = match context {
                    Some(context) => context.stream_session(raw).context(SessionWriteError)?,
                    None => Some(raw),
                } {
                    result.sessions.push(raw);
                }
            }
            Ok(None) => result.stats.filtered_sessions += 1,
            Err(error) => warn!("failed to parse Warp conversation: {error:#}"),
        }
    }
    Ok(())
}

fn before_cutoff(conn: &Connection, id: &str, modified: &str, cutoff: i64) -> Result<bool> {
    if parse_timestamp(modified).is_none_or(|timestamp| timestamp >= cutoff) {
        return Ok(false);
    }
    let mut stmt =
        conn.prepare("SELECT last_modified_at FROM agent_tasks WHERE conversation_id = ?1")?;
    let mut rows = stmt.query([id])?;
    while let Some(row) = rows.next()? {
        let modified: String = row.get(0)?;
        if parse_timestamp(&modified).is_none_or(|timestamp| timestamp >= cutoff) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn parse_timestamp(text: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(text.trim()).map(|time| time.timestamp_millis()).ok().or_else(
        || {
            NaiveDateTime::parse_from_str(text.trim(), "%Y-%m-%d %H:%M:%S%.f")
                .ok()
                .map(|time| time.and_utc().timestamp_millis())
        },
    )
}

impl proto::Timestamp {
    fn millis(&self) -> Option<i64> {
        if !(0..1_000_000_000).contains(&self.nanos) {
            return None;
        }
        self.seconds.checked_mul(1000)?.checked_add(i64::from(self.nanos) / 1_000_000)
    }
}

fn parse_conversation(
    conn: &Connection,
    id: &str,
    modified: &str,
    path: &Path,
    include_events: bool,
) -> Result<Option<RawSession>> {
    let mut updated_at = parse_timestamp(modified);
    let mut records = Vec::new();
    let mut title = None;
    let mut stmt = conn.prepare(
        "SELECT task, last_modified_at FROM agent_tasks WHERE conversation_id = ?1 ORDER BY id",
    )?;
    let mut rows = stmt.query([id])?;
    while let Some(row) = rows.next()? {
        let bytes: Vec<u8> = row.get(0)?;
        let task = proto::Task::decode(bytes.as_slice()).context("decoding Warp task protobuf")?;
        let modified: String = row.get(1)?;
        updated_at = updated_at.max(parse_timestamp(&modified));
        if task.dependencies.as_ref().is_none_or(|deps| deps.parent_task_id.is_empty())
            && !task.description.trim().is_empty()
            && title.is_none()
        {
            title = Some(task.description);
        }
        let mut ordering_time = task
            .messages
            .iter()
            .find_map(|message| message.timestamp.as_ref().and_then(proto::Timestamp::millis));
        for (seq, message) in task.messages.into_iter().enumerate() {
            let key = if message.id.is_empty() {
                format!("{}:{seq}", task.id)
            } else {
                message.id.clone()
            };
            ordering_time =
                message.timestamp.as_ref().and_then(proto::Timestamp::millis).or(ordering_time);
            records.push((ordering_time, key, task.id.clone(), message));
        }
    }
    records.sort_by_key(|(time, _, _, _)| *time);
    let mut seen = HashSet::new();
    records.retain(|(_, key, _, _)| seen.insert(key.clone()));
    let mut messages = Vec::new();
    let mut session_events = Vec::new();
    let mut usage_events = Vec::new();
    let mut directory = None;
    let mut task_directories = HashMap::new();
    let tool_calls: HashMap<_, _> = records
        .iter()
        .filter_map(|(_, _, _, message)| message.tool_call.as_ref())
        .filter(|call| !call.tool_call_id.is_empty())
        .map(|call| {
            (
                call.tool_call_id.clone(),
                (
                    call_name(call).to_string(),
                    call.run_shell_command.as_ref().map(|shell| shell.command.clone()),
                ),
            )
        })
        .collect();
    let mut started_at = None;
    let source_path = path.to_string_lossy().into_owned();
    for (_, key, task_id, message) in records {
        let timestamp = message.timestamp.as_ref().and_then(proto::Timestamp::millis);
        started_at = started_at.or(timestamp);
        updated_at = updated_at.max(timestamp);
        let context =
            message.user_query.as_ref().and_then(|query| query.context.as_ref()).or_else(|| {
                message.tool_call_result.as_ref().and_then(|result| result.context.as_ref())
            });
        if let Some(pwd) = context
            .and_then(|context| context.directory.as_ref())
            .map(|directory| directory.pwd.clone())
            .filter(|pwd| !pwd.trim().is_empty())
        {
            directory = directory.or_else(|| Some(pwd.clone()));
            task_directories.insert(task_id.clone(), pwd);
        }
        let active_cwd = task_directories.get(&task_id).map(String::as_str);
        if let Some(query) = message.user_query {
            if !query.origin.as_ref().is_some_and(|origin| {
                matches!(
                    origin.variant,
                    Some(
                        proto::QueryOrigin::ParentAgent(_)
                            | proto::QueryOrigin::AgentMessageWake(_)
                            | proto::QueryOrigin::Schedule(_)
                            | proto::QueryOrigin::Automation(_)
                            | proto::QueryOrigin::ServerSynthesized(_)
                    )
                )
            }) && !query.query.trim().is_empty()
            {
                messages.push(RawMessage { role: Role::User, content: query.query, timestamp });
            }
        } else if let Some(output) = message.agent_output {
            if !output.text.trim().is_empty() {
                messages.push(RawMessage {
                    role: Role::Assistant,
                    content: output.text,
                    timestamp,
                });
            }
        } else if include_events {
            let context = EventContext {
                event_seq: session_events.len() as u32,
                timestamp,
                source_path: Some(source_path.clone()),
                source_event_id: Some(key.clone()),
                message_seq: None,
                parser_version: EVENT_PARSER_VERSION,
            };
            if let Some(call) = message.tool_call {
                let event = call_event(&call, context, active_cwd);
                session_events.push(event);
            } else if let Some(result) = message.tool_call_result {
                let call = tool_calls.get(&result.tool_call_id);
                let name = call.map(|(name, _)| name.clone());
                let command = call.and_then(|(_, command)| command.as_deref());
                session_events.push(result_event(&result, context, name, command, active_cwd));
            }
        }
        if let Some(metadata) = message.request_metadata
            && let Some(timestamp) = timestamp
        {
            parse_usage(&metadata, &key, timestamp, &source_path, &mut usage_events)?;
        }
    }
    if messages.is_empty() && session_events.is_empty() && usage_events.is_empty() {
        return Ok(None);
    }
    let started_at =
        started_at.or(updated_at).context("Warp conversation has no valid timestamp")?;
    let mut raw = RawSession::search_only(id, directory, started_at, updated_at, None, messages)
        .with_usage(usage_events, USAGE_PARSER_VERSION);
    if include_events {
        raw = raw.with_events(session_events, EVENT_PARSER_VERSION);
    }
    raw.source_file_path = Some(source_path);
    raw.custom_title = title;
    Ok(Some(raw))
}

fn call_name(call: &proto::ToolCall) -> &str {
    if call.run_shell_command.is_some() {
        "run_shell_command"
    } else if call.read_files.is_some() {
        "read_files"
    } else if call.apply_file_diffs.is_some() {
        "apply_file_diffs"
    } else {
        call.other.as_ref().map(proto::OtherTool::name).unwrap_or("unknown")
    }
}

fn call_event(call: &proto::ToolCall, context: EventContext, cwd: Option<&str>) -> RawSessionEvent {
    let mut files = Vec::new();
    let name = call_name(call);
    let args = if let Some(shell) = &call.run_shell_command {
        Some(json!({"command": shell.command}))
    } else if let Some(read) = &call.read_files {
        files.extend(read.files.iter().filter(|file| !file.name.trim().is_empty()).map(|file| {
            FileEvidence::call(file.name.clone(), FileOperation::Read, cwd.map(str::to_string))
        }));
        Some(json!({"paths": read.files.iter().map(|file| &file.name).collect::<Vec<_>>()}))
    } else if let Some(diff) = &call.apply_file_diffs {
        for (paths, operation) in [
            (&diff.diffs, FileOperation::Write),
            (&diff.new_files, FileOperation::Write),
            (&diff.deleted_files, FileOperation::Delete),
        ] {
            for file in paths.iter().filter(|file| !file.file_path.trim().is_empty()) {
                files.push(FileEvidence::call(
                    file.file_path.clone(),
                    operation.clone(),
                    cwd.map(str::to_string),
                ));
            }
        }
        for update in &diff.v4a_updates {
            if update.file_path.trim().is_empty() {
                continue;
            }
            let moved = !update.move_to.trim().is_empty();
            files.push(FileEvidence::call(
                update.file_path.clone(),
                if moved { FileOperation::MoveFrom } else { FileOperation::Write },
                cwd.map(str::to_string),
            ));
            if moved {
                files.push(FileEvidence::call(
                    update.move_to.clone(),
                    FileOperation::MoveTo,
                    cwd.map(str::to_string),
                ));
            }
        }
        Some(
            json!({"summary": diff.summary, "paths": files.iter().map(|file| &file.path).collect::<Vec<_>>()}),
        )
    } else {
        None
    };
    let mut event = events::tool_call_event(context, name.into(), args.as_ref());
    event.tool_call_id = Some(call.tool_call_id.clone()).filter(|id| !id.is_empty());
    if name == "run_shell_command" {
        let (evidence, status) =
            events::shell_file_evidence(&call.run_shell_command.as_ref().unwrap().command, cwd);
        files = evidence;
        event.command_evidence_status = Some(status);
    } else if name == "read_files" || name == "apply_file_diffs" {
        event.kind = if name == "read_files" { "file_read" } else { "file_write" }.into();
        event.target = files.first().map(|file| file.path.clone());
    }
    event.files = files;
    event
}

fn result_event(
    result: &proto::ToolCallResult,
    context: EventContext,
    name: Option<String>,
    call_command: Option<&str>,
    cwd: Option<&str>,
) -> RawSessionEvent {
    let mut event = events::tool_result_event(context, name, None);
    event.tool_call_id = Some(result.tool_call_id.clone()).filter(|id| !id.is_empty());
    if let Some(shell) = &result.run_shell_command {
        event.name = Some("run_shell_command".into());
        let exit_code =
            shell.command_finished.as_ref().map(|finished| finished.exit_code).or(shell.exit_code);
        let output = shell
            .command_finished
            .as_ref()
            .map(|finished| &finished.output)
            .unwrap_or(&shell.output);
        event.summary =
            Some(events::bounded_summary(output.clone())).filter(|text| !text.is_empty());
        event.status = if shell.permission_denied.is_some() {
            Some("denied".into())
        } else if shell.terminal_busy.is_some() {
            Some("terminal_busy".into())
        } else {
            exit_code.map(|code| if code == 0 { "success" } else { "error" }.into())
        };
        if !shell.command.trim().is_empty() {
            event.target = Some(shell.command.clone());
            if call_command != Some(shell.command.as_str()) {
                let (files, status) = events::shell_file_evidence(&shell.command, cwd);
                event.files = files;
                event.command_evidence_status = Some(status);
            }
        }
        event.attrs_json = Some(json!({"exit_code": exit_code}).to_string());
    } else if result.cancel.is_some() {
        event.status = Some("cancelled".into());
    } else if let Some(outcome) = result.read_files.as_ref().or(result.apply_file_diffs.as_ref()) {
        event.status = if outcome.error.is_some() {
            Some("error".into())
        } else if outcome.success.is_some() || outcome.any_files_success.is_some() {
            Some("success".into())
        } else {
            None
        };
    }
    event
}

fn parse_usage(
    metadata: &proto::RequestMetadata,
    message_id: &str,
    timestamp: i64,
    source_path: &str,
    events: &mut Vec<RawUsageEvent>,
) -> Result<()> {
    let Some(charges) = &metadata.charges else {
        return Ok(());
    };
    for (category, usage) in &charges.usage_by_category {
        for (origin, models) in [
            ("warp", &usage.direct_api_inference_usage),
            ("byok", &usage.byok_inference_usage),
            ("custom_endpoint", &usage.custom_endpoint_inference_usage),
        ] {
            for (model, usage) in models {
                let Some(tokens) = &usage.token_count else {
                    continue;
                };
                let mut event = RawUsageEvent::observed(
                    json!([message_id, category, origin, model]).to_string(),
                    events.len() as u32,
                    timestamp,
                    USAGE_PARSER_VERSION,
                );
                event.model = model.clone();
                event.input_tokens = tokens.input.try_into()?;
                event.output_tokens = tokens.output.try_into()?;
                event.cache_read_tokens = tokens.input_cache_read.try_into()?;
                event.cache_write_tokens = tokens.input_cache_write.try_into()?;
                event.source_path = Some(source_path.into());
                event.raw_usage_json = Some(json!({"category": category, "origin": origin, "input": tokens.input, "output": tokens.output, "input_cache_read": tokens.input_cache_read, "input_cache_write": tokens.input_cache_write}).to_string());
                events.push(event);
            }
        }
    }
    Ok(())
}
