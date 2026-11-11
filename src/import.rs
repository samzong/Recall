use std::collections::HashSet;
use std::io::BufRead;

use anyhow::{Result, anyhow, bail};
use serde::Deserialize;

use crate::db::store::{SessionTopologyWrite, Store};
use crate::types::{
    Message, ParentLink, RawSessionEvent, RawUsageEvent, Role, Session, TokenSource,
};

const RECORD_TYPE: &str = "session";

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ImportSummary {
    pub(crate) total: usize,
    pub(crate) imported: usize,
    pub(crate) skipped: usize,
}

pub(crate) fn run_cli(file: &str, dry_run: bool) -> Result<()> {
    let _lock = crate::utils::acquire_sync_lock()?;
    let store = Store::open()?;
    let summary = if file == "-" {
        let stdin = std::io::stdin();
        import_jsonl(&store, dry_run, stdin.lock())?
    } else {
        let f = std::fs::File::open(file).map_err(|e| anyhow!("cannot open {file}: {e}"))?;
        import_jsonl(&store, dry_run, std::io::BufReader::new(f))?
    };

    let suffix = if dry_run { " (dry-run, nothing written)" } else { "" };
    println!(
        "total {} | imported {} | skipped {}{suffix}",
        summary.total, summary.imported, summary.skipped
    );
    Ok(())
}

#[derive(Deserialize)]
struct ImportRecord {
    schema_version: u32,
    record_type: String,
    session: ImportSession,
    #[serde(default)]
    messages: Vec<ImportMessage>,
    #[serde(default)]
    usage_events: Vec<ImportUsageEvent>,
    #[serde(default)]
    events: Vec<RawSessionEvent>,
}

#[derive(Deserialize)]
struct ImportSession {
    source: String,
    source_id: String,
    title: String,
    #[serde(default)]
    directory: Option<String>,
    #[serde(default)]
    repo_remote: Option<String>,
    #[serde(default)]
    repo_slug: Option<String>,
    #[serde(default)]
    repo_name: Option<String>,
    started_at: i64,
    #[serde(default)]
    updated_at: Option<i64>,
    #[serde(default)]
    entrypoint: Option<String>,
    #[serde(default)]
    custom_title: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    duration_minutes: Option<u32>,
    #[serde(default)]
    source_file_path: Option<String>,
    #[serde(default)]
    topology: ImportTopology,
}

#[derive(Deserialize, Default)]
struct ImportTopology {
    #[serde(default)]
    thread_role: Option<String>,
    #[serde(default)]
    parents: Vec<ImportParentLink>,
}

#[derive(Deserialize)]
struct ImportParentLink {
    relation: String,
    source: String,
    source_id: String,
}

#[derive(Deserialize)]
struct ImportMessage {
    seq: u32,
    role: String,
    #[serde(default)]
    timestamp: Option<i64>,
    content: String,
}

#[derive(Deserialize)]
struct ImportUsageEvent {
    event_key: String,
    event_seq: u32,
    #[serde(default)]
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
    #[serde(default)]
    parser_version: u32,
    #[serde(default)]
    source_path: Option<String>,
    #[serde(default)]
    raw_usage_json: Option<String>,
}

pub(crate) fn import_jsonl<R: BufRead>(
    store: &Store,
    dry_run: bool,
    reader: R,
) -> Result<ImportSummary> {
    let mut summary = ImportSummary::default();
    let mut seen: HashSet<(String, String)> = HashSet::new();

    for (idx, line) in reader.lines().enumerate() {
        let line_no = idx + 1;
        let line = line.map_err(|e| anyhow!("line {line_no}: read failed: {e}"))?;
        if line.trim().is_empty() {
            continue;
        }

        let record: ImportRecord = serde_json::from_str(&line)
            .map_err(|e| anyhow!("line {line_no}: invalid record: {e}"))?;
        if record.record_type != RECORD_TYPE {
            bail!("line {line_no}: unsupported record_type '{}'", record.record_type);
        }
        if !matches!(record.schema_version, 2..=7) {
            bail!("line {line_no}: unsupported schema_version {}", record.schema_version);
        }

        summary.total += 1;

        let key = (record.session.source.clone(), record.session.source_id.clone());
        if seen.contains(&key)
            || store.session_meta(&record.session.source, &record.session.source_id)?.is_some()
        {
            summary.skipped += 1;
            continue;
        }
        seen.insert(key);
        summary.imported += 1;
        if dry_run {
            continue;
        }

        persist_record(store, record, line_no)?;
    }

    Ok(summary)
}

pub(crate) struct DecodedSession {
    pub(crate) session: Session,
    pub(crate) messages: Vec<Message>,
    pub(crate) usage_events: Vec<RawUsageEvent>,
    pub(crate) events: Vec<RawSessionEvent>,
    pub(crate) topology: crate::types::SessionTopology,
}

pub(crate) fn decode_remote(
    value: serde_json::Value,
    session_uuid: String,
) -> Result<DecodedSession> {
    for field in ["messages", "usage_events", "events"] {
        anyhow::ensure!(value[field].is_array(), "remote record must include {field}");
    }
    anyhow::ensure!(
        value["session"]["message_count"].as_u64()
            == value["messages"].as_array().map(|messages| messages.len() as u64),
        "remote message count mismatch"
    );
    let record: ImportRecord = serde_json::from_value(value)?;
    anyhow::ensure!(
        record.schema_version == crate::export::RECORD_SCHEMA_VERSION
            && record.record_type == RECORD_TYPE,
        "unsupported remote session record"
    );
    let data = decode_record(record, session_uuid, 0)?;
    let sequences: HashSet<_> = data.messages.iter().map(|message| message.seq).collect();
    anyhow::ensure!(sequences.len() == data.messages.len(), "duplicate remote message sequence");
    Ok(data)
}

fn persist_record(store: &Store, record: ImportRecord, line_no: usize) -> Result<()> {
    let data = decode_record(record, uuid::Uuid::new_v4().to_string(), line_no)?;
    store.persist_session_with_usage_and_events_with_topology(
        &data.session,
        &data.messages,
        &data.usage_events,
        None,
        &data.events,
        None,
        &SessionTopologyWrite {
            thread_role: data.topology.thread_role,
            parents: &data.topology.parents,
            parser_version: None,
        },
    )
}

fn decode_record(
    record: ImportRecord,
    session_uuid: String,
    line_no: usize,
) -> Result<DecodedSession> {
    let s = record.session;

    let session = Session {
        id: session_uuid.clone(),
        source: s.source,
        source_id: s.source_id,
        title: s.title,
        directory: s.directory,
        repo_remote: s.repo_remote,
        repo_slug: s.repo_slug,
        repo_name: s.repo_name,
        started_at: s.started_at,
        updated_at: s.updated_at,
        message_count: record.messages.len() as u32,
        entrypoint: s.entrypoint,
        custom_title: s.custom_title,
        summary: s.summary,
        duration_minutes: s.duration_minutes,
        source_file_path: s.source_file_path,
        is_import: true,
        locations: Vec::new(),
        alternative_versions: 0,
    };

    let messages = record
        .messages
        .into_iter()
        .map(|m| {
            let role: Role = m
                .role
                .parse()
                .map_err(|()| anyhow!("line {line_no}: invalid message role '{}'", m.role))?;
            Ok(Message {
                session_id: session_uuid.clone(),
                role,
                content: m.content,
                timestamp: m.timestamp,
                seq: m.seq,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let usage_events = record
        .usage_events
        .into_iter()
        .map(|e| {
            let token_source: TokenSource = e.token_source.parse().map_err(|()| {
                anyhow!("line {line_no}: invalid token_source '{}'", e.token_source)
            })?;
            Ok(RawUsageEvent {
                event_key: e.event_key,
                event_seq: e.event_seq,
                message_seq: e.message_seq,
                timestamp: e.timestamp,
                model: e.model,
                provider: e.provider,
                input_tokens: e.input_tokens,
                output_tokens: e.output_tokens,
                cache_read_tokens: e.cache_read_tokens,
                cache_write_tokens: e.cache_write_tokens,
                reasoning_tokens: e.reasoning_tokens,
                token_source,
                parser_version: e.parser_version,
                source_path: e.source_path,
                raw_usage_json: e.raw_usage_json,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let thread_role = s.topology.thread_role.and_then(|role| role.parse().ok());
    let parents: Vec<ParentLink> = s
        .topology
        .parents
        .into_iter()
        .filter_map(|parent| {
            Some(ParentLink {
                relation: parent.relation.parse().ok()?,
                source: parent.source,
                source_id: parent.source_id,
            })
        })
        .collect();
    Ok(DecodedSession {
        session,
        messages,
        usage_events,
        events: record.events,
        topology: crate::types::SessionTopology { thread_role, parents },
    })
}

#[cfg(test)]
mod tests;
