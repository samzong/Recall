use std::collections::HashSet;
use std::io::BufRead;

use anyhow::{Result, anyhow, bail};
use serde::Deserialize;

use crate::db::store::Store;
use crate::types::{Message, RawSessionEvent, RawUsageEvent, Role, Session, TokenSource};

const RECORD_TYPE: &str = "session";

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ImportSummary {
    pub total: usize,
    pub imported: usize,
    pub skipped: usize,
}

pub fn run_cli(file: &str, dry_run: bool) -> Result<()> {
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
    events: Vec<ImportEvent>,
}

#[derive(Deserialize)]
struct ImportSession {
    source: String,
    source_id: String,
    title: String,
    #[serde(default)]
    directory: Option<String>,
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

#[derive(Deserialize)]
struct ImportEvent {
    event_seq: u32,
    #[serde(default)]
    timestamp: Option<i64>,
    kind: String,
    actor: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    message_seq: Option<u32>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    source_path: Option<String>,
    #[serde(default)]
    source_event_id: Option<String>,
    #[serde(default)]
    attrs_json: Option<String>,
    #[serde(default)]
    parser_version: u32,
}

pub fn import_jsonl<R: BufRead>(store: &Store, dry_run: bool, reader: R) -> Result<ImportSummary> {
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
        if !matches!(record.schema_version, 2 | 3) {
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

fn persist_record(store: &Store, record: ImportRecord, line_no: usize) -> Result<()> {
    let session_uuid = uuid::Uuid::new_v4().to_string();
    let s = record.session;

    let session = Session {
        id: session_uuid.clone(),
        source: s.source,
        source_id: s.source_id,
        title: s.title,
        directory: s.directory,
        started_at: s.started_at,
        updated_at: s.updated_at,
        message_count: record.messages.len() as u32,
        entrypoint: s.entrypoint,
        custom_title: s.custom_title,
        summary: s.summary,
        duration_minutes: s.duration_minutes,
        source_file_path: s.source_file_path,
        is_import: true,
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

    let events: Vec<RawSessionEvent> = record
        .events
        .into_iter()
        .map(|e| RawSessionEvent {
            event_seq: e.event_seq,
            timestamp: e.timestamp,
            kind: e.kind,
            actor: e.actor,
            name: e.name,
            status: e.status,
            target: e.target,
            message_seq: e.message_seq,
            summary: e.summary,
            source_path: e.source_path,
            source_event_id: e.source_event_id,
            attrs_json: e.attrs_json,
            parser_version: e.parser_version,
        })
        .collect();

    store.persist_session_with_usage_and_events(
        &session,
        &messages,
        &usage_events,
        None,
        &events,
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema;
    use crate::db::search::TimeRange;
    use crate::export::{ExportOptions, write_jsonl};

    fn setup() -> Store {
        schema::register_sqlite_vec();
        Store::open_in_memory().unwrap()
    }

    fn full_session(source: &str, source_id: &str, title: &str) -> Session {
        Session {
            id: format!("local-{source_id}"),
            source: source.to_string(),
            source_id: source_id.to_string(),
            title: title.to_string(),
            directory: Some("/tmp/project".to_string()),
            started_at: 1_000,
            updated_at: Some(2_000),
            message_count: 2,
            entrypoint: Some("cli".to_string()),
            custom_title: Some("Custom".to_string()),
            summary: Some("A summary".to_string()),
            duration_minutes: Some(7),
            source_file_path: Some("/home/origin/.codex/sessions/a.jsonl".to_string()),
            is_import: false,
        }
    }

    fn full_messages(session_id: &str) -> Vec<Message> {
        vec![
            Message {
                session_id: session_id.to_string(),
                role: Role::User,
                content: "zebraquery".to_string(),
                timestamp: Some(1_000),
                seq: 0,
            },
            Message {
                session_id: session_id.to_string(),
                role: Role::Assistant,
                content: "assistant reply".to_string(),
                timestamp: Some(1_500),
                seq: 2,
            },
        ]
    }

    fn full_usage_event() -> RawUsageEvent {
        RawUsageEvent {
            event_key: "uk1".to_string(),
            event_seq: 0,
            message_seq: Some(2),
            timestamp: 1_500,
            model: "gpt-test".to_string(),
            provider: "openai".to_string(),
            input_tokens: 10,
            output_tokens: 20,
            cache_read_tokens: 3,
            cache_write_tokens: 2,
            reasoning_tokens: 1,
            token_source: TokenSource::Observed,
            parser_version: 4,
            source_path: Some("/home/origin/raw.jsonl".to_string()),
            raw_usage_json: Some("{\"input_tokens\":10}".to_string()),
        }
    }

    fn full_event() -> RawSessionEvent {
        RawSessionEvent {
            event_seq: 0,
            timestamp: Some(1_200),
            kind: "tool".to_string(),
            actor: "assistant".to_string(),
            name: Some("Shell".to_string()),
            status: Some("ok".to_string()),
            target: Some("ls".to_string()),
            message_seq: Some(0),
            summary: Some("ran ls".to_string()),
            source_path: Some("/home/origin/raw.jsonl".to_string()),
            source_event_id: Some("ev-1".to_string()),
            attrs_json: Some("{\"exit_code\":0}".to_string()),
            parser_version: 5,
        }
    }

    fn persist_full(store: &Store, source: &str, source_id: &str, title: &str) {
        let session = full_session(source, source_id, title);
        let messages = full_messages(&session.id);
        store
            .persist_session_with_usage_and_events(
                &session,
                &messages,
                &[full_usage_event()],
                Some(4),
                &[full_event()],
                Some(5),
            )
            .unwrap();
    }

    fn export_all(store: &Store) -> String {
        let options = ExportOptions {
            session_ids: Vec::new(),
            sources: None,
            time_range: TimeRange::All,
            project: None,
            limit: None,
        };
        let mut out = Vec::new();
        write_jsonl(store, &options, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn count(store: &Store, sql: &str) -> i64 {
        store.conn.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    #[test]
    fn roundtrip_preserves_all_fields() {
        let a = setup();
        persist_full(&a, "codex", "src-1", "Roundtrip");
        let exported = export_all(&a);

        let b = setup();
        let summary = import_jsonl(&b, false, exported.as_bytes()).unwrap();
        assert_eq!(summary, ImportSummary { total: 1, imported: 1, skipped: 0 });

        let reexported = export_all(&b);
        let mut orig: serde_json::Value = serde_json::from_str(exported.trim()).unwrap();
        let mut copy: serde_json::Value = serde_json::from_str(reexported.trim()).unwrap();
        orig["session"]["id"] = serde_json::Value::Null;
        copy["session"]["id"] = serde_json::Value::Null;
        assert_eq!(orig, copy, "export -> import -> export must be lossless");
    }

    #[test]
    fn import_marks_is_import_and_writes_no_state_rows() {
        let a = setup();
        persist_full(&a, "codex", "src-1", "Marked");
        let exported = export_all(&a);

        let b = setup();
        import_jsonl(&b, false, exported.as_bytes()).unwrap();

        assert_eq!(count(&b, "SELECT COUNT(*) FROM sessions WHERE is_import = 1"), 1);
        assert_eq!(count(&b, "SELECT COUNT(*) FROM usage_session_state"), 0);
        assert_eq!(count(&b, "SELECT COUNT(*) FROM event_session_state"), 0);
        assert_eq!(count(&b, "SELECT COUNT(*) FROM usage_events"), 1);
        assert_eq!(count(&b, "SELECT COUNT(*) FROM session_events"), 1);
    }

    #[test]
    fn import_is_idempotent() {
        let a = setup();
        persist_full(&a, "codex", "src-1", "Idem");
        persist_full(&a, "grok", "src-2", "Idem2");
        let exported = export_all(&a);

        let b = setup();
        let first = import_jsonl(&b, false, exported.as_bytes()).unwrap();
        assert_eq!(first, ImportSummary { total: 2, imported: 2, skipped: 0 });

        let second = import_jsonl(&b, false, exported.as_bytes()).unwrap();
        assert_eq!(second, ImportSummary { total: 2, imported: 0, skipped: 2 });
        assert_eq!(count(&b, "SELECT COUNT(*) FROM sessions"), 2);
        assert_eq!(count(&b, "SELECT COUNT(*) FROM messages"), 4);
        assert_eq!(count(&b, "SELECT COUNT(*) FROM usage_events"), 2);
    }

    #[test]
    fn local_roundtrip_imports_nothing_and_keeps_local_rows() {
        let store = setup();
        persist_full(&store, "codex", "src-1", "Local");
        let exported = export_all(&store);

        let summary = import_jsonl(&store, false, exported.as_bytes()).unwrap();
        assert_eq!(summary, ImportSummary { total: 1, imported: 0, skipped: 1 });
        assert_eq!(count(&store, "SELECT COUNT(*) FROM sessions"), 1);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM sessions WHERE is_import = 0"), 1);
        assert_eq!(export_all(&store), exported, "local data must be untouched");
    }

    #[test]
    fn skip_keeps_local_content_on_conflicting_import() {
        let a = setup();
        persist_full(&a, "codex", "src-1", "Remote version");
        let exported = export_all(&a);

        let b = setup();
        persist_full(&b, "codex", "src-1", "Local version");
        let summary = import_jsonl(&b, false, exported.as_bytes()).unwrap();
        assert_eq!(summary, ImportSummary { total: 1, imported: 0, skipped: 1 });

        let title: String = b
            .conn
            .query_row(
                "SELECT title FROM sessions WHERE source = 'codex' AND source_id = 'src-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(title, "Local version");
    }

    #[test]
    fn dry_run_writes_nothing() {
        let a = setup();
        persist_full(&a, "codex", "src-1", "Dry");
        let exported = export_all(&a);

        let b = setup();
        let summary = import_jsonl(&b, true, exported.as_bytes()).unwrap();
        assert_eq!(summary, ImportSummary { total: 1, imported: 1, skipped: 0 });
        for table in
            ["sessions", "messages", "usage_events", "session_events", "session_embedding_state"]
        {
            assert_eq!(count(&b, &format!("SELECT COUNT(*) FROM {table}")), 0, "{table} dirty");
        }
    }

    #[test]
    fn duplicate_keys_in_one_file_count_once_in_dry_run_and_real_run() {
        let a = setup();
        persist_full(&a, "codex", "src-1", "Dup");
        let line = export_all(&a);
        let doubled = format!("{line}{line}");

        let b = setup();
        let dry = import_jsonl(&b, true, doubled.as_bytes()).unwrap();
        let real = import_jsonl(&b, false, doubled.as_bytes()).unwrap();

        let expected = ImportSummary { total: 2, imported: 1, skipped: 1 };
        assert_eq!(dry, expected, "dry-run must predict exactly what a real import writes");
        assert_eq!(real, expected);
        assert_eq!(count(&b, "SELECT COUNT(*) FROM sessions"), 1);
    }

    #[test]
    fn malformed_line_fails_with_line_number_keeping_prior_rows() {
        let a = setup();
        persist_full(&a, "codex", "src-1", "Good");
        let mut exported = export_all(&a);
        exported.push_str("not json\n");

        let b = setup();
        let err = import_jsonl(&b, false, exported.as_bytes()).unwrap_err();
        assert!(err.to_string().contains("line 2"), "unexpected error: {err}");
        assert_eq!(count(&b, "SELECT COUNT(*) FROM sessions"), 1);
    }

    #[test]
    fn rejects_unsupported_schema_version_and_record_type() {
        let store = setup();
        let bad_version = r#"{"schema_version":99,"record_type":"session","session":{"source":"codex","source_id":"x","title":"t","started_at":0}}"#;
        let err = import_jsonl(&store, false, bad_version.as_bytes()).unwrap_err();
        assert!(err.to_string().contains("schema_version"), "unexpected error: {err}");

        let bad_type = r#"{"schema_version":3,"record_type":"snapshot","session":{"source":"codex","source_id":"x","title":"t","started_at":0}}"#;
        let err = import_jsonl(&store, false, bad_type.as_bytes()).unwrap_err();
        assert!(err.to_string().contains("record_type"), "unexpected error: {err}");
    }

    #[test]
    fn accepts_schema_version_2_without_v3_fields() {
        let store = setup();
        let v2 = r#"{"schema_version":2,"record_type":"session","session":{"source":"codex","source_id":"v2-1","title":"V2","started_at":100},"messages":[{"seq":0,"role":"user","timestamp":100,"content":"hello v2"}],"usage_events":[{"event_key":"k","event_seq":0,"message_seq":null,"timestamp":100,"model":"m","provider":"p","input_tokens":1,"output_tokens":2,"cache_read_tokens":0,"cache_write_tokens":0,"reasoning_tokens":0,"token_source":"observed"}],"events":[{"event_seq":0,"timestamp":null,"kind":"tool","actor":"assistant"}]}"#;
        let summary = import_jsonl(&store, false, v2.as_bytes()).unwrap();
        assert_eq!(summary, ImportSummary { total: 1, imported: 1, skipped: 0 });

        let parser_version: i64 = store
            .conn
            .query_row("SELECT parser_version FROM usage_events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(parser_version, 0, "missing v3 fields must default to 0");
    }

    #[test]
    fn preserves_message_seq_and_fts() {
        let a = setup();
        persist_full(&a, "codex", "src-1", "Seq");
        let exported = export_all(&a);

        let b = setup();
        import_jsonl(&b, false, exported.as_bytes()).unwrap();

        let seqs: Vec<u32> = {
            let mut stmt = b.conn.prepare("SELECT seq FROM messages ORDER BY seq").unwrap();
            let rows = stmt.query_map([], |row| row.get(0)).unwrap();
            rows.collect::<Result<Vec<_>, _>>().unwrap()
        };
        assert_eq!(seqs, vec![0, 2], "non-contiguous seq must be preserved");

        let fts_hits =
            count(&b, "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'zebraquery'");
        assert_eq!(fts_hits, 1, "imported messages must be searchable via FTS");
    }

    #[test]
    fn local_sync_refresh_clears_is_import() {
        let a = setup();
        persist_full(&a, "codex", "src-1", "Converge");
        let exported = export_all(&a);

        let b = setup();
        import_jsonl(&b, false, exported.as_bytes()).unwrap();
        assert_eq!(count(&b, "SELECT COUNT(*) FROM sessions WHERE is_import = 1"), 1);

        persist_full(&b, "codex", "src-1", "Now local");
        assert_eq!(count(&b, "SELECT COUNT(*) FROM sessions WHERE is_import = 1"), 0);
    }
}
