use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, params};
use serde_json::Value;

use super::store::Store;
use crate::project_scope::ProjectScope;
use crate::types::Session;

mod codec;
mod compression;
mod merge;
mod objects;

pub(crate) use codec::{decode_zstd, encode_delta};
use objects::{Metadata, descends_from, object_parts};
pub(crate) use objects::{Revision, digest, revision_heads, valid_digest, validate_object_key};

const SQLITE_MAX_LENGTH: usize = 1_000_000_000;
pub(crate) const OBJECT_LIMIT: usize = 512 * 1024 * 1024;
const _: () = assert!(OBJECT_LIMIT < SQLITE_MAX_LENGTH);
pub(crate) const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];
pub(crate) const ZSTD_LEVEL: i32 = 3;
const PREPARE_STATE: &str = "prepare_version";
const MERGE_STATE: &str = "merge_version";

pub(crate) fn decode_stored(body: Vec<u8>) -> Result<Vec<u8>> {
    if body.starts_with(&ZSTD_MAGIC) { decode_zstd(&body, None) } else { Ok(body) }
}

pub(crate) fn is_zero(value: &u32) -> bool {
    *value == 0
}

pub(crate) fn alternative_versions(conn: &rusqlite::Connection, id: &str) -> rusqlite::Result<u32> {
    Ok(conn
        .query_row(
            "SELECT alternative_versions FROM session_sync WHERE session_id = ?1",
            [id],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(0))
}

impl Store {
    pub(crate) fn remote_snapshot(&self, session: Session) -> Result<Value> {
        let id = &session.id;
        let topology = self.session_topology(id)?;
        let messages = self.get_messages(id)?;
        let usage = self.list_usage_events_for_session(id)?;
        let events = self.list_session_events_for_session(id)?;
        let mut value =
            crate::export::session_record_value(session, topology, messages, usage, events)?;
        value["session"].as_object_mut().context("missing session object")?.remove("id");
        Ok(value)
    }

    fn remote_identity(&self, session_id: &str) -> Result<String> {
        let existing: Option<String> = self
            .conn
            .query_row(
                "SELECT sync_id FROM session_sync WHERE session_id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?;
        let sync_id = existing.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        self.conn.execute(
            "INSERT OR IGNORE INTO session_sync(session_id, sync_id) VALUES (?1, ?2)",
            params![session_id, sync_id],
        )?;
        self.conn.execute(
            "INSERT OR IGNORE INTO sync_aliases(sync_id, session_id) VALUES (?1, ?2)",
            params![sync_id, session_id],
        )?;
        Ok(sync_id)
    }

    pub(crate) fn prepare_remote(
        &self,
        scope: &ProjectScope,
        on_progress: &mut dyn FnMut(usize, usize),
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let state = format!("{} {}", env!("CARGO_PKG_VERSION"), serde_json::to_string(scope)?);
        let full = !self.sync_state_current(PREPARE_STATE, &state)?;
        let pending: HashSet<String> = self
            .conn
            .prepare(
                "SELECT session_id FROM sync_pending_snapshots
                 UNION SELECT s.id FROM sessions s
                 LEFT JOIN session_sync y ON y.session_id = s.id
                 WHERE y.current_revision IS NULL",
            )?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let sessions: Vec<Session> = self
            .list_export_sessions(None, super::search::TimeRange::All, scope, None, None)?
            .into_iter()
            .filter(|session| full || pending.contains(&session.id))
            .collect();
        let total = sessions.len();
        for (index, session) in sessions.into_iter().enumerate() {
            on_progress(index + 1, total);
            self.prepare_session(session)?;
        }
        self.conn.execute(
            "DELETE FROM sync_pending_snapshots
             WHERE session_id NOT IN (SELECT id FROM sessions)",
            [],
        )?;
        self.mark_sync_state(PREPARE_STATE, &state)?;
        tx.commit()?;
        Ok(())
    }

    fn prepare_session(&self, session: Session) -> Result<()> {
        let id = session.id.clone();
        let sync_id = self.remote_identity(&id)?;
        let current: Option<String> = self.conn.query_row(
            "SELECT current_revision FROM session_sync WHERE session_id = ?1",
            [&id],
            |row| row.get(0),
        )?;
        let mut parent_sessions = Vec::new();
        for parent in self.session_topology(&id)?.parents {
            let identity = self
                .resolve_parent(&id, &parent)?
                .map(|session| self.remote_identity(&session.id))
                .transpose()?;
            parent_sessions.push(identity);
        }
        let locations = session.locations.clone();
        let record = self.remote_snapshot(session)?;
        let unchanged = current
            .as_deref()
            .map(|hash| self.remote_revision(hash))
            .transpose()?
            .flatten()
            .is_some_and(|revision| {
                revision.record == record && revision.parent_sessions == parent_sessions
            });
        let locally_indexed: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM native_bindings WHERE session_id = ?1)",
            [&id],
            |row| row.get(0),
        )?;
        if !unchanged && (current.is_none() || locally_indexed) {
            let revision = Revision {
                version: 1,
                sync_id: sync_id.clone(),
                parent: current,
                parent_sessions,
                record,
            };
            let body = serde_json::to_vec(&revision)?;
            let hash = digest(&body);
            self.cache_remote(&format!("v1/revisions/{hash}.json"), &body)?;
            self.conn.execute(
                "UPDATE session_sync SET current_revision = ?2 WHERE session_id = ?1",
                params![id, hash],
            )?;
        }
        if !locations.is_empty() {
            self.cache_metadata(&Metadata::Locations { version: 1, sync_id, locations })?;
        }
        self.conn.execute("DELETE FROM sync_pending_snapshots WHERE session_id = ?1", [&id])?;
        Ok(())
    }

    fn sync_state_current(&self, key: &str, state: &str) -> Result<bool> {
        let value: Option<String> = self
            .conn
            .query_row("SELECT value FROM sync_state WHERE key = ?1", [key], |row| row.get(0))
            .optional()?;
        Ok(value.as_deref() == Some(state))
    }

    fn mark_sync_state(&self, key: &str, state: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sync_state(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, state],
        )?;
        Ok(())
    }

    fn cache_metadata(&self, value: &Metadata) -> Result<()> {
        let body = serde_json::to_vec(value)?;
        self.cache_remote(&format!("v1/metadata/{}.json", digest(&body)), &body)
    }

    pub(crate) fn remote_revision(&self, hash: &str) -> Result<Option<Revision>> {
        let body: Option<Vec<u8>> = self
            .conn
            .query_row("SELECT body FROM sync_revisions WHERE digest = ?1", [hash], |row| {
                row.get(0)
            })
            .optional()?;
        body.map(|body| Ok(serde_json::from_slice(&decode_stored(body)?)?)).transpose()
    }

    pub(crate) fn cached_remote(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.cached_stored(key)?.map(decode_stored).transpose()
    }

    pub(crate) fn cached_stored(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let (table, hash) = object_parts(key)?;
        Ok(self
            .conn
            .query_row(&format!("SELECT body FROM {table} WHERE digest = ?1"), [hash], |row| {
                row.get(0)
            })
            .optional()?)
    }

    pub(crate) fn session_revisions(&self, id: &str) -> Result<BTreeMap<String, Revision>> {
        let mut stmt = self.conn.prepare("SELECT r.digest, r.body FROM sync_revisions r JOIN sync_aliases a ON a.sync_id = r.sync_id WHERE a.session_id = ?1")?;
        let rows = stmt
            .query_map([id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(hash, body)| Ok((hash, serde_json::from_slice(&decode_stored(body)?)?)))
            .collect()
    }

    pub(crate) fn revision_summaries(&self, id: &str) -> Result<Vec<Value>> {
        let revisions = self.session_revisions(id)?;
        let current: Option<String> = self
            .conn
            .query_row(
                "SELECT current_revision FROM session_sync WHERE session_id = ?1",
                [id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let mut hashes = revision_heads(&revisions);
        if let Some(current) = current.as_ref()
            && let Some((hash, _)) = revisions.get_key_value(current)
            && !hashes.contains(&hash)
        {
            hashes.push(hash);
        }
        Ok(hashes
            .into_iter()
            .map(|hash| {
                serde_json::json!({
                    "digest": hash, "current": Some(hash) == current.as_ref(),
                    "parent": revisions[hash].parent,
                    "message_count": revisions[hash].record["session"]["message_count"],
                    "updated_at": revisions[hash].record["session"]["updated_at"],
                })
            })
            .collect())
    }

    pub(crate) fn remote_uploads(
        &self,
        scope: &ProjectScope,
        listed: &HashSet<String>,
    ) -> Result<Vec<String>> {
        let scoped: HashSet<String> = self
            .list_export_sessions(None, super::search::TimeRange::All, scope, None, None)?
            .into_iter()
            .map(|session| session.id)
            .collect();
        let identities: HashSet<String> = self
            .conn
            .prepare("SELECT sync_id, session_id FROM sync_aliases")?
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
            .filter_map(|row| match row {
                Ok((sync_id, session)) => scoped.contains(&session).then_some(Ok(sync_id)),
                Err(error) => Some(Err(error)),
            })
            .collect::<rusqlite::Result<_>>()?;
        let mut owners = HashMap::new();
        let mut keys = Vec::new();
        let mut stmt = self.conn.prepare("SELECT digest, sync_id FROM sync_revisions")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let (hash, sync_id): (String, String) = (row.get(0)?, row.get(1)?);
            let key = format!("v1/revisions/{hash}.json");
            if identities.contains(&sync_id) && !listed.contains(&key) {
                keys.push(key);
            }
            owners.insert(hash, sync_id);
        }
        let owned = |hash: &str| owners.get(hash).is_some_and(|id| identities.contains(id));
        let mut stmt = self.conn.prepare("SELECT digest, body FROM sync_objects")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let hash: String = row.get(0)?;
            let key = format!("v1/metadata/{hash}.json");
            if listed.contains(&key) {
                continue;
            }
            let include = match serde_json::from_slice(&decode_stored(row.get(1)?)?)? {
                Metadata::Locations { sync_id, .. } => identities.contains(&sync_id),
                Metadata::Association { left, right, .. } => owned(&left) && owned(&right),
            };
            if include {
                keys.push(key);
            }
        }
        keys.sort();
        Ok(keys)
    }

    pub(crate) fn cached_sizes(&self) -> Result<HashMap<String, u64>> {
        let mut sizes = HashMap::new();
        for (table, prefix) in
            [("sync_revisions", "v1/revisions/"), ("sync_objects", "v1/metadata/")]
        {
            let mut stmt = self
                .conn
                .prepare(&format!("SELECT digest, COALESCE(size, LENGTH(body)) FROM {table}"))?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                sizes.insert(format!("{prefix}{}.json", row.get::<_, String>(0)?), row.get(1)?);
            }
        }
        Ok(sizes)
    }
}
