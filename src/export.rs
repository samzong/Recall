use std::io::Write;

use anyhow::Result;
use serde::Serialize;

use crate::db::search::TimeRange;
use crate::db::store::Store;
use crate::types::{Message, Role, Session, SessionUsageEventRecord};

const SCHEMA_VERSION: u32 = 1;
const RECORD_TYPE: &str = "session";

pub struct ExportOptions {
    pub sources: Option<Vec<String>>,
    pub time_range: TimeRange,
    pub project: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Serialize)]
struct ExportSessionRecord {
    schema_version: u32,
    record_type: &'static str,
    session: ExportSession,
    messages: Vec<ExportMessage>,
    usage_events: Vec<ExportUsageEvent>,
}

#[derive(Serialize)]
struct ExportSession {
    id: String,
    source: String,
    source_id: String,
    title: String,
    directory: Option<String>,
    started_at: i64,
    updated_at: Option<i64>,
    message_count: u32,
    entrypoint: Option<String>,
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
}

pub fn write_jsonl<W: Write>(store: &Store, options: &ExportOptions, mut writer: W) -> Result<()> {
    let sessions = store.list_export_sessions(
        options.sources.as_deref(),
        options.time_range,
        options.project.as_deref(),
        options.limit,
    )?;

    for session in sessions {
        let messages = store.get_messages(&session.id)?;
        let usage_events = store.list_usage_events_for_session(&session.id)?;
        let record = ExportSessionRecord {
            schema_version: SCHEMA_VERSION,
            record_type: RECORD_TYPE,
            session: session.into(),
            messages: messages.into_iter().map(Into::into).collect(),
            usage_events: usage_events.into_iter().map(Into::into).collect(),
        };
        serde_json::to_writer(&mut writer, &record)?;
        writer.write_all(b"\n")?;
    }

    Ok(())
}

impl From<Session> for ExportSession {
    fn from(session: Session) -> Self {
        Self {
            id: session.id,
            source: session.source,
            source_id: session.source_id,
            title: session.title,
            directory: session.directory,
            started_at: session.started_at,
            updated_at: session.updated_at,
            message_count: session.message_count,
            entrypoint: session.entrypoint,
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
        }
    }
}
