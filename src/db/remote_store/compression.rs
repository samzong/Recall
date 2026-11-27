use anyhow::Result;
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};

use super::super::schema::REVISION_COMPRESSION_JOB;
use super::super::store::Store;
use super::{ZSTD_LEVEL, ZSTD_MAGIC, decode_zstd};

impl Store {
    pub(crate) fn revision_compression_pending(&self) -> Result<bool> {
        let pending: Option<bool> = self
            .conn
            .query_row(
                "SELECT phase != 'done' FROM background_job_state WHERE job = ?1",
                [REVISION_COMPRESSION_JOB],
                |row| row.get(0),
            )
            .optional()?;
        Ok(pending.unwrap_or(false))
    }

    pub(crate) fn backfill_revision_compression_batch(&self) -> Result<bool> {
        const BATCH: i64 = 32;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let state: Option<(String, Option<String>)> = tx
            .query_row(
                "SELECT phase, detail FROM background_job_state WHERE job = ?1",
                [REVISION_COMPRESSION_JOB],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((phase, detail)) = state else {
            return Ok(false);
        };
        if phase != "backfill" {
            return Ok(false);
        }
        let parsed: serde_json::Value = detail
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?
            .unwrap_or_else(|| serde_json::json!({}));
        let mut rev_cursor =
            parsed.get("rev_cursor").and_then(serde_json::Value::as_i64).unwrap_or(0);
        let rev_end = parsed.get("rev_end").and_then(serde_json::Value::as_i64).unwrap_or(0);
        let mut obj_cursor =
            parsed.get("obj_cursor").and_then(serde_json::Value::as_i64).unwrap_or(0);
        let obj_end = parsed.get("obj_end").and_then(serde_json::Value::as_i64).unwrap_or(0);

        if rev_cursor < rev_end {
            if let Some(last) =
                recompress_table_batch(&tx, "sync_revisions", rev_cursor, rev_end, BATCH)?
            {
                rev_cursor = last;
            } else {
                rev_cursor = rev_end;
            }
            let next_detail = serde_json::json!({
                "rev_cursor": rev_cursor,
                "rev_end": rev_end,
                "obj_cursor": obj_cursor,
                "obj_end": obj_end,
            })
            .to_string();
            tx.execute(
                "UPDATE background_job_state SET detail = ?2, updated_at = unixepoch() * 1000 WHERE job = ?1",
                params![REVISION_COMPRESSION_JOB, next_detail],
            )?;
            tx.commit()?;
            return Ok(true);
        }

        if obj_cursor < obj_end {
            if let Some(last) =
                recompress_table_batch(&tx, "sync_objects", obj_cursor, obj_end, BATCH)?
            {
                obj_cursor = last;
            } else {
                obj_cursor = obj_end;
            }
            let next_detail = serde_json::json!({
                "rev_cursor": rev_cursor,
                "rev_end": rev_end,
                "obj_cursor": obj_cursor,
                "obj_end": obj_end,
            })
            .to_string();
            tx.execute(
                "UPDATE background_job_state SET detail = ?2, updated_at = unixepoch() * 1000 WHERE job = ?1",
                params![REVISION_COMPRESSION_JOB, next_detail],
            )?;
            tx.commit()?;
            return Ok(true);
        }

        tx.execute(
            "UPDATE background_job_state SET phase = 'done', detail = NULL, updated_at = unixepoch() * 1000 WHERE job = ?1",
            [REVISION_COMPRESSION_JOB],
        )?;
        tx.commit()?;
        Ok(false)
    }
}

fn recompress_table_batch(
    tx: &Transaction<'_>,
    table: &str,
    cursor: i64,
    end: i64,
    limit: i64,
) -> Result<Option<i64>> {
    let rows: Vec<(i64, Vec<u8>, Option<i64>)> = {
        let mut stmt = tx.prepare(&format!(
            "SELECT rowid, body, size FROM {table} WHERE rowid > ?1 AND rowid <= ?2 ORDER BY rowid LIMIT ?3"
        ))?;
        stmt.query_map(params![cursor, end, limit], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?
        .collect::<rusqlite::Result<_>>()?
    };
    let Some((last_rowid, _, _)) = rows.last() else {
        return Ok(None);
    };
    let last_rowid = *last_rowid;
    let mut update_both =
        tx.prepare(&format!("UPDATE {table} SET body = ?1, size = ?2 WHERE rowid = ?3"))?;
    let mut update_size = tx.prepare(&format!("UPDATE {table} SET size = ?1 WHERE rowid = ?2"))?;
    for (rowid, body, size) in rows {
        if !body.starts_with(&ZSTD_MAGIC) {
            let raw_len = body.len() as i64;
            let encoded = zstd::encode_all(body.as_slice(), ZSTD_LEVEL)?;
            update_both.execute(params![encoded, raw_len, rowid])?;
        } else if size.is_none() {
            let raw_len = decode_zstd(&body, None)?.len() as i64;
            update_size.execute(params![raw_len, rowid])?;
        }
    }
    Ok(Some(last_rowid))
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::*;
    use crate::db::schema;

    #[test]
    fn backfill_revision_compression_compresses_legacy_rows_without_queueing_sync() {
        schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let mut session = crate::types::test_support::session("sess-compress");
        session.source = "codex".to_string();
        session.source_id = "codex-compress".to_string();
        store.insert_session(&session).unwrap();

        let sync_id = uuid::Uuid::new_v4().to_string();
        let record = store.remote_snapshot(session.clone()).unwrap();
        let rev = Revision {
            version: 1,
            sync_id: sync_id.clone(),
            parent: None,
            parent_sessions: Vec::new(),
            record,
        };
        let rev_bytes = serde_json::to_vec(&rev).unwrap();
        let rev_hash = digest(&rev_bytes);

        let meta =
            Metadata::Locations { version: 1, sync_id: sync_id.clone(), locations: Vec::new() };
        let obj_bytes = serde_json::to_vec(&meta).unwrap();
        let obj_hash = digest(&obj_bytes);

        store
            .conn
            .execute_batch("DELETE FROM sync_pending_snapshots; DELETE FROM sync_pending_heads;")
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO sync_aliases(sync_id, session_id) VALUES (?1, ?2)",
                params![sync_id, session.id],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO sync_revisions(digest, sync_id, body, size) VALUES (?1, ?2, ?3, NULL)",
                params![rev_hash, sync_id, rev_bytes],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO sync_objects(digest, body, size) VALUES (?1, ?2, NULL)",
                params![obj_hash, obj_bytes],
            )
            .unwrap();

        let rev_end: i64 = store
            .conn
            .query_row("SELECT MAX(rowid) FROM sync_revisions", [], |r| r.get(0))
            .unwrap();
        let obj_end: i64 =
            store.conn.query_row("SELECT MAX(rowid) FROM sync_objects", [], |r| r.get(0)).unwrap();
        let detail = serde_json::json!({
            "rev_cursor": 0,
            "rev_end": rev_end,
            "obj_cursor": 0,
            "obj_end": obj_end,
        })
        .to_string();
        store
            .conn
            .execute(
                "INSERT INTO background_job_state(job, phase, detail, updated_at)
                 VALUES (?1, 'backfill', ?2, 0)
                 ON CONFLICT(job) DO UPDATE SET phase = 'backfill', detail = excluded.detail",
                params![REVISION_COMPRESSION_JOB, detail],
            )
            .unwrap();

        store
            .conn
            .execute_batch("DELETE FROM sync_pending_snapshots; DELETE FROM sync_pending_heads;")
            .unwrap();
        assert!(store.revision_compression_pending().unwrap());
        while store.backfill_revision_compression_batch().unwrap() {}
        assert!(!store.revision_compression_pending().unwrap());

        let stored_rev: Vec<u8> = store
            .conn
            .query_row("SELECT body FROM sync_revisions WHERE digest = ?1", [&rev_hash], |r| {
                r.get(0)
            })
            .unwrap();
        let stored_obj: Vec<u8> = store
            .conn
            .query_row("SELECT body FROM sync_objects WHERE digest = ?1", [&obj_hash], |r| r.get(0))
            .unwrap();
        assert!(stored_rev.starts_with(&ZSTD_MAGIC));
        assert!(stored_obj.starts_with(&ZSTD_MAGIC));
        assert_eq!(
            store.cached_remote_size(&format!("v1/revisions/{rev_hash}.json")).unwrap(),
            Some(rev_bytes.len() as u64)
        );
        assert_eq!(
            store.cached_remote_size(&format!("v1/metadata/{obj_hash}.json")).unwrap(),
            Some(obj_bytes.len() as u64)
        );
        assert_eq!(store.session_revisions(&session.id).unwrap().len(), 1);

        let pending_snaps: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM sync_pending_snapshots", [], |r| r.get(0))
            .unwrap();
        let pending_heads: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM sync_pending_heads", [], |r| r.get(0))
            .unwrap();
        assert_eq!(pending_snaps, 0);
        assert_eq!(pending_heads, 0);
    }
}
