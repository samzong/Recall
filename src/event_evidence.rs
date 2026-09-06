use anyhow::{Result, ensure};
use rmcp::schemars;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::db::event_store::EventReference;
use crate::db::store::Store;

const READ_BUDGET: usize = 64 * 1024 * 1024;

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EvidencePart {
    #[default]
    Payload,
    Before,
    After,
}

#[derive(Debug, Serialize, Deserialize)]
struct EvidenceCursor {
    version: u8,
    event_ref: EventReference,
    content_digest: String,
    part: EvidencePart,
    byte_offset: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct EvidencePage {
    event_ref: String,
    part: EvidencePart,
    encoding: String,
    byte_offset: usize,
    total_bytes: usize,
    data: String,
    content_digest: String,
    native_source_status: String,
    next_cursor: Option<String>,
}

pub(crate) fn read(
    store: &Store,
    session_id: &str,
    reference: &str,
    part: EvidencePart,
    cursor: Option<&str>,
    max_bytes: usize,
) -> Result<EvidencePage> {
    ensure!((1024..=65536).contains(&max_bytes), "max_bytes must be between 1024 and 65536");
    ensure!(reference.len() <= 4096, "invalid event_ref");
    let reference: EventReference = serde_json::from_str(reference)?;
    ensure!(reference.version == 1, "unsupported event_ref version");
    let tx = store.conn.unchecked_transaction()?;
    let native: Option<(String, String)> = tx.query_row(
        "SELECT e.source, e.source_id FROM session_events e JOIN file_history_state h ON h.id = 1 WHERE e.session_id = ?1 AND e.id = ?2 AND h.index_id = ?3",
        rusqlite::params![session_id, reference.event_id, reference.index_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional()?;
    let (source, source_id) =
        native.ok_or_else(|| anyhow::anyhow!("event_ref is stale; query file_history again"))?;
    let event_id = reference.event_id;
    let mut remaining = READ_BUDGET;
    let fields = [
        "event_seq",
        "timestamp",
        "kind",
        "actor",
        "name",
        "status",
        "target",
        "message_seq",
        "summary",
        "source_path",
        "source_event_id",
        "tool_call_id",
        "is_meta",
        "visibility",
        "attrs_json",
        "parser_version",
        "command_evidence_status",
    ];
    let lengths = fields
        .iter()
        .map(|field| format!("COALESCE(length(CAST({field} AS BLOB)), 0)"))
        .collect::<Vec<_>>()
        .join(" + ");
    let bytes: usize = tx.query_row(&format!("SELECT {lengths} + COALESCE((SELECT SUM(length(CAST(evidence_json AS BLOB))) FROM event_files WHERE event_id = ?1), 0) FROM session_events WHERE id = ?1"), [event_id], |row| row.get(0))?;
    let upper_bound = bytes
        .checked_mul(6)
        .and_then(|bytes| bytes.checked_add(4096))
        .ok_or_else(|| anyhow::anyhow!("evidence_budget_exceeded"))?;
    ensure!(upper_bound <= remaining, "evidence_budget_exceeded");
    remaining -= upper_bound;
    let (anchor, call_id): (Option<u32>, Option<String>) = tx.query_row(
        "SELECT message_seq, tool_call_id FROM session_events WHERE id = ?1",
        [event_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let (data, native_source_status) = match part {
        EvidencePart::Payload => {
            let mut related = Vec::new();
            if let Some(call_id) = call_id.as_deref().filter(|id| !id.trim().is_empty()) {
                let mut stmt = tx.prepare("SELECT id FROM session_events WHERE session_id = ?1 AND tool_call_id = ?2 AND id != ?3 ORDER BY event_seq")?;
                for row in stmt
                    .query_map(rusqlite::params![session_id, call_id, event_id], |row| {
                        row.get::<_, i64>(0)
                    })?
                {
                    let related_ref = crate::db::event_store::event_reference(&tx, row?)?;
                    let value = serde_json::to_string(&related_ref)?;
                    ensure!(value.len() * 6 <= remaining, "evidence_budget_exceeded");
                    remaining -= value.len() * 6;
                    related.push(value);
                }
            }
            let fields = fields.iter().map(|field| {
                if *field == "is_meta" {
                    "'is_meta', json(CASE is_meta WHEN 0 THEN 'false' WHEN 1 THEN 'true' ELSE 'null' END)".to_string()
                } else {
                    format!("'{field}', {field}")
                }
            }).collect::<Vec<_>>().join(", ");
            let discussion = anchor.map(|around_seq| serde_json::json!({"session_id":session_id,"around_seq":around_seq,"max_chars":6000}));
            let document: String = tx.query_row(&format!(
                "SELECT json_object('event', json_object({fields}), 'files', json(COALESCE((SELECT json_group_array(json(evidence_json)) FROM (SELECT evidence_json FROM event_files WHERE event_id = ?1 ORDER BY position)), '[]')), 'related_event_refs', json(?2), 'discussion', json(?3)) FROM session_events WHERE id = ?1"
            ), rusqlite::params![event_id, serde_json::to_string(&related)?, serde_json::to_string(&discussion)?], |row| row.get(0))?;
            (document, "unverified")
        }
        EvidencePart::Before | EvidencePart::After => {
            let is_import: bool = tx.query_row(
                "SELECT is_import FROM sessions WHERE id = ?1",
                [session_id],
                |row| row.get(0),
            )?;
            ensure!(!is_import && source == "cursor", "source_unverified");
            let (source_path, source_event_id, attrs): (
                Option<String>,
                Option<String>,
                Option<String>,
            ) = tx.query_row(
                "SELECT source_path, source_event_id, attrs_json FROM session_events WHERE id = ?1",
                [event_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;

            ensure!(
                source_path.as_deref() == Some(format!("composer:{}", source_id).as_str()),
                "source_unverified"
            );
            let attrs = attrs.as_deref().ok_or_else(|| anyhow::anyhow!("content_reference_not_recorded; read payload.related_event_refs and select the native result event_ref"))?;
            let native: serde_json::Value = serde_json::from_str(attrs)?;
            ensure!(
                native
                    .get(if part == EvidencePart::Before {
                        "beforeContentId"
                    } else {
                        "afterContentId"
                    })
                    .and_then(serde_json::Value::as_str)
                    .is_some(),
                "content_reference_not_recorded; read payload.related_event_refs and select the native result event_ref"
            );
            let content = crate::adapters::cursor::read_content_evidence(
                &source_id,
                source_event_id.as_deref().ok_or_else(|| anyhow::anyhow!("source_unverified"))?,
                call_id.as_deref(),
                attrs,
                part == EvidencePart::Before,
                &mut remaining,
            )?;
            (content, "content_verified")
        }
    };
    let content_digest = format!("{:x}", Sha256::digest(data.as_bytes()));
    let mut offset = 0;
    if let Some(cursor) = cursor {
        ensure!(cursor.len() <= 2048, "invalid evidence cursor");
        let cursor: EvidenceCursor = serde_json::from_str(cursor)?;
        ensure!(cursor.version == 1 && cursor.part == part, "invalid evidence cursor");
        ensure!(
            cursor.event_ref == reference && cursor.content_digest == content_digest,
            "evidence cursor is stale; read the event again"
        );
        offset = cursor.byte_offset;
        ensure!(
            offset <= data.len() && data.is_char_boundary(offset),
            "invalid evidence cursor offset"
        );
    }
    let mut page = EvidencePage {
        event_ref: serde_json::to_string(&reference)?,
        part,
        encoding: "utf-8".into(),
        byte_offset: offset,
        total_bytes: data.len(),
        data: String::new(),
        content_digest: content_digest.clone(),
        native_source_status: native_source_status.into(),
        next_cursor: None,
    };
    let next = |byte_offset| {
        serde_json::to_string(&EvidenceCursor {
            version: 1,
            event_ref: reference.clone(),
            content_digest: content_digest.clone(),
            part,
            byte_offset,
        })
    };
    page.next_cursor = Some(next(data.len())?);
    let overhead = serde_json::to_vec(&page)?.len();
    ensure!(overhead + 24 <= max_bytes, "max_bytes is too small for this event_ref");
    let mut end = data.len().min(offset.saturating_add((max_bytes - overhead) / 6));
    while !data.is_char_boundary(end) {
        end -= 1;
    }
    ensure!(end > offset || offset == data.len(), "max_bytes is too small for a UTF-8 character");
    page.data = data[offset..end].to_string();
    page.next_cursor = (end < data.len()).then(|| next(end)).transpose()?;
    ensure!(serde_json::to_vec(&page)?.len() <= max_bytes, "evidence response budget exceeded");
    Ok(page)
}
