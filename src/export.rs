use std::io::Write;

use anyhow::{Result, anyhow};
use serde::Serialize;

use crate::adapters;
use crate::db::search::{RepoFilter, TimeRange};
use crate::db::store::Store;
use crate::query::{parse_time_range, resolve_source_filter};
use crate::types::{Message, Role, Session, SessionEventRecord, SessionUsageEventRecord};

const SCHEMA_VERSION: u32 = 4;
const RECORD_TYPE: &str = "session";

pub struct ExportOptions {
    pub session_ids: Vec<String>,
    pub sources: Option<Vec<String>>,
    pub time_range: TimeRange,
    pub project: Option<String>,
    pub repo: Option<RepoFilter>,
    pub limit: Option<usize>,
}

pub fn run_cli(
    source_filter: Option<&str>,
    time_filter: Option<&str>,
    project_filter: Option<&str>,
    repo_filter: Option<&str>,
    limit: usize,
) -> Result<()> {
    let store = Store::open()?;
    let sources = adapters::source_labels();
    let (directory, repo) = store.resolve_project_repo_filters(project_filter, repo_filter)?;
    let options = ExportOptions {
        session_ids: Vec::new(),
        sources: resolve_source_filter(source_filter, &sources)?,
        time_range: parse_time_range(time_filter),
        project: directory,
        repo,
        limit: if limit == 0 { None } else { Some(limit) },
    };
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    write_jsonl(&store, &options, &mut handle)
}

#[derive(Serialize)]
struct ExportSessionRecord {
    schema_version: u32,
    record_type: &'static str,
    session: ExportSession,
    messages: Vec<ExportMessage>,
    usage_events: Vec<ExportUsageEvent>,
    events: Vec<ExportEvent>,
}

#[derive(Serialize)]
struct ExportSession {
    id: String,
    source: String,
    source_id: String,
    title: String,
    directory: Option<String>,
    repo_remote: Option<String>,
    repo_slug: Option<String>,
    repo_name: Option<String>,
    started_at: i64,
    updated_at: Option<i64>,
    message_count: u32,
    entrypoint: Option<String>,
    custom_title: Option<String>,
    summary: Option<String>,
    duration_minutes: Option<u32>,
    source_file_path: Option<String>,
}

#[derive(Serialize)]
struct ExportMessage {
    seq: u32,
    role: &'static str,
    timestamp: Option<i64>,
    content: String,
}

#[derive(Serialize)]
struct ExportUsageEvent {
    event_key: String,
    event_seq: u32,
    message_seq: Option<u32>,
    timestamp: i64,
    model: String,
    provider: String,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    reasoning_tokens: i64,
    token_source: String,
    parser_version: u32,
    source_path: Option<String>,
    raw_usage_json: Option<String>,
}

#[derive(Serialize)]
struct ExportEvent {
    event_seq: u32,
    timestamp: Option<i64>,
    kind: String,
    actor: String,
    name: Option<String>,
    status: Option<String>,
    target: Option<String>,
    message_seq: Option<u32>,
    summary: Option<String>,
    source_path: Option<String>,
    source_event_id: Option<String>,
    attrs_json: Option<String>,
    parser_version: u32,
}

pub fn write_jsonl<W: Write>(store: &Store, options: &ExportOptions, mut writer: W) -> Result<()> {
    let sessions = if options.session_ids.is_empty() {
        store.list_export_sessions(
            options.sources.as_deref(),
            options.time_range,
            options.project.as_deref(),
            options.repo.as_ref(),
            options.limit,
        )?
    } else {
        let mut sessions = Vec::with_capacity(options.session_ids.len());
        for session_id in &options.session_ids {
            let session = store
                .get_session_by_id(session_id)?
                .ok_or_else(|| anyhow!("session not found: {session_id}"))?;
            sessions.push(session);
        }
        sessions
    };

    write_jsonl_for_sessions(store, sessions, &mut writer)
}

pub fn session_record_value(
    session: Session,
    messages: Vec<Message>,
    usage_events: Vec<SessionUsageEventRecord>,
    events: Vec<SessionEventRecord>,
) -> Result<serde_json::Value> {
    Ok(serde_json::to_value(build_session_record(session, messages, usage_events, events))?)
}

fn write_jsonl_for_sessions<W: Write>(
    store: &Store,
    sessions: Vec<Session>,
    mut writer: W,
) -> Result<()> {
    for session in sessions {
        let messages = store.get_messages(&session.id)?;
        let usage_events = store.list_usage_events_for_session(&session.id)?;
        let events = store.list_session_events_for_session(&session.id)?;
        let record = build_session_record(session, messages, usage_events, events);
        serde_json::to_writer(&mut writer, &record)?;
        writer.write_all(b"\n")?;
    }

    Ok(())
}

fn build_session_record(
    session: Session,
    messages: Vec<Message>,
    usage_events: Vec<SessionUsageEventRecord>,
    events: Vec<SessionEventRecord>,
) -> ExportSessionRecord {
    ExportSessionRecord {
        schema_version: SCHEMA_VERSION,
        record_type: RECORD_TYPE,
        session: session.into(),
        messages: messages.into_iter().map(Into::into).collect(),
        usage_events: usage_events.into_iter().map(Into::into).collect(),
        events: events.into_iter().map(Into::into).collect(),
    }
}

impl From<Session> for ExportSession {
    fn from(session: Session) -> Self {
        Self {
            id: session.id,
            source: session.source,
            source_id: session.source_id,
            title: session.title,
            directory: session.directory,
            repo_remote: session.repo_remote,
            repo_slug: session.repo_slug,
            repo_name: session.repo_name,
            started_at: session.started_at,
            updated_at: session.updated_at,
            message_count: session.message_count,
            entrypoint: session.entrypoint,
            custom_title: session.custom_title,
            summary: session.summary,
            duration_minutes: session.duration_minutes,
            source_file_path: session.source_file_path,
        }
    }
}

impl From<Message> for ExportMessage {
    fn from(message: Message) -> Self {
        let role = match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        Self { seq: message.seq, role, timestamp: message.timestamp, content: message.content }
    }
}

impl From<SessionUsageEventRecord> for ExportUsageEvent {
    fn from(event: SessionUsageEventRecord) -> Self {
        Self {
            event_key: event.event_key,
            event_seq: event.event_seq,
            message_seq: event.message_seq,
            timestamp: event.timestamp,
            model: event.model,
            provider: event.provider,
            input_tokens: event.input_tokens,
            output_tokens: event.output_tokens,
            cache_read_tokens: event.cache_read_tokens,
            cache_write_tokens: event.cache_write_tokens,
            reasoning_tokens: event.reasoning_tokens,
            token_source: event.token_source,
            parser_version: event.parser_version,
            source_path: event.source_path,
            raw_usage_json: event.raw_usage_json,
        }
    }
}

impl From<SessionEventRecord> for ExportEvent {
    fn from(event: SessionEventRecord) -> Self {
        Self {
            event_seq: event.event_seq,
            timestamp: event.timestamp,
            kind: event.kind,
            actor: event.actor,
            name: event.name,
            status: event.status,
            target: event.target,
            message_seq: event.message_seq,
            summary: event.summary,
            source_path: event.source_path,
            source_event_id: event.source_event_id,
            attrs_json: event.attrs_json,
            parser_version: event.parser_version,
        }
    }
}
