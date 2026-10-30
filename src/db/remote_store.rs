use std::collections::{BTreeMap, HashSet};

use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::store::Store;
use crate::host::Location;
use crate::project_scope::ProjectScope;
use crate::types::Session;

mod merge;

const SQLITE_MAX_LENGTH: usize = 1_000_000_000;
pub(crate) const OBJECT_LIMIT: usize = 512 * 1024 * 1024;
const _: () = assert!(OBJECT_LIMIT < SQLITE_MAX_LENGTH);
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];
const ZSTD_LEVEL: i32 = 3;

fn decode_stored(body: Vec<u8>) -> Result<Vec<u8>> {
    if body.starts_with(&ZSTD_MAGIC) { Ok(zstd::decode_all(body.as_slice())?) } else { Ok(body) }
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

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Revision {
    pub(crate) version: u32,
    pub(crate) sync_id: String,
    pub(crate) parent: Option<String>,
    pub(crate) parent_sessions: Vec<Option<String>>,
    pub(crate) record: Value,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Metadata {
    Locations { version: u32, sync_id: String, locations: Vec<Location> },
    Association { version: u32, left: String, right: String },
}

pub(crate) fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(crate) fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_identity(value: &str) -> Result<()> {
    ensure!(uuid::Uuid::parse_str(value)?.to_string() == value, "invalid sync identity");
    Ok(())
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
        let sessions =
            self.list_export_sessions(None, super::search::TimeRange::All, scope, None, None)?;
        for session in &sessions {
            self.remote_identity(&session.id)?;
        }
        let total = sessions.len();
        for (index, session) in sessions.into_iter().enumerate() {
            on_progress(index + 1, total);
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
        }
        tx.commit()?;
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
        let (table, hash) = object_parts(key)?;
        self.conn
            .query_row(&format!("SELECT body FROM {table} WHERE digest = ?1"), [hash], |row| {
                row.get(0)
            })
            .optional()?
            .map(decode_stored)
            .transpose()
    }

    pub(crate) fn cached_remote_size(&self, key: &str) -> Result<Option<u64>> {
        let (table, hash) = object_parts(key)?;
        Ok(self
            .conn
            .query_row(
                &format!("SELECT COALESCE(size, LENGTH(body)) FROM {table} WHERE digest = ?1"),
                [hash],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub(crate) fn cache_remote(&self, key: &str, body: &[u8]) -> Result<()> {
        ensure!(body.len() <= OBJECT_LIMIT, "remote object exceeds 512 MiB");
        let (table, hash) = object_parts(key)?;
        ensure!(digest(body) == hash, "remote object content hash mismatch");
        let size = body.len() as i64;
        if table == "sync_revisions" {
            let revision: Revision = serde_json::from_slice(body)?;
            ensure!(revision.version == 1, "unsupported remote revision version");
            validate_identity(&revision.sync_id)?;
            ensure!(revision.parent.as_deref().is_none_or(valid_digest), "invalid parent revision");
            for identity in revision.parent_sessions.iter().flatten() {
                validate_identity(identity)?;
            }
            let decoded = crate::import::decode_remote(
                revision.record.clone(),
                uuid::Uuid::new_v4().to_string(),
            )?;
            ensure!(
                revision.parent_sessions.len() == decoded.topology.parents.len(),
                "invalid portable parent links"
            );
            self.conn.execute(
                "INSERT OR IGNORE INTO sync_revisions(digest, sync_id, body, size) VALUES (?1, ?2, ?3, ?4)",
                params![hash, revision.sync_id, zstd::encode_all(body, ZSTD_LEVEL)?, size],
            )?;
        } else {
            let metadata: Metadata = serde_json::from_slice(body)?;
            match metadata {
                Metadata::Locations { version, sync_id, locations } => {
                    ensure!(version == 1, "unsupported remote metadata version");
                    validate_identity(&sync_id)?;
                    let mut hosts = HashSet::new();
                    for location in locations {
                        location.host.validate()?;
                        ensure!(hosts.insert(location.host.id), "duplicate location host");
                    }
                }
                Metadata::Association { version, left, right } => {
                    ensure!(
                        version == 1
                            && left != right
                            && valid_digest(&left)
                            && valid_digest(&right),
                        "invalid legacy association"
                    );
                }
            }
            self.conn.execute(
                "INSERT OR IGNORE INTO sync_objects(digest, body, size) VALUES (?1, ?2, ?3)",
                params![hash, zstd::encode_all(body, ZSTD_LEVEL)?, size],
            )?;
        }
        Ok(())
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

    pub(crate) fn remote_uploads(&self, scope: &ProjectScope) -> Result<Vec<String>> {
        let sessions =
            self.list_export_sessions(None, super::search::TimeRange::All, scope, None, None)?;
        let mut identities = HashSet::new();
        let mut keys = Vec::new();
        for session in sessions {
            let aliases = self
                .conn
                .prepare("SELECT sync_id FROM sync_aliases WHERE session_id = ?1")?
                .query_map([&session.id], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            identities.extend(aliases);
            let digests = self
                .conn
                .prepare(
                    "SELECT r.digest FROM sync_revisions r
                     JOIN sync_aliases a ON a.sync_id = r.sync_id
                     WHERE a.session_id = ?1",
                )?
                .query_map([&session.id], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            keys.extend(digests.into_iter().map(|hash| format!("v1/revisions/{hash}.json")));
        }
        let metadata = self
            .conn
            .prepare("SELECT digest, body FROM sync_objects")?
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (hash, body) in metadata {
            let include = match serde_json::from_slice(&decode_stored(body)?)? {
                Metadata::Locations { sync_id, .. } => identities.contains(&sync_id),
                Metadata::Association { left, right, .. } => {
                    self.remote_revision(&left)?
                        .is_some_and(|revision| identities.contains(&revision.sync_id))
                        && self
                            .remote_revision(&right)?
                            .is_some_and(|revision| identities.contains(&revision.sync_id))
                }
            };
            if include {
                keys.push(format!("v1/metadata/{hash}.json"));
            }
        }
        keys.sort();
        keys.dedup();
        Ok(keys)
    }
}

fn descends_from(hash: &str, ancestor: &str, revisions: &BTreeMap<String, Revision>) -> bool {
    let mut hash = hash;
    let mut seen = HashSet::new();
    while seen.insert(hash) {
        if hash == ancestor {
            return true;
        }
        if let (Some(current), Some(ancestor)) = (revisions.get(hash), revisions.get(ancestor))
            && current.record == ancestor.record
        {
            return true;
        }
        let Some(parent) = revisions.get(hash).and_then(|revision| revision.parent.as_deref())
        else {
            break;
        };
        hash = parent;
    }
    false
}

pub(crate) fn revision_heads(revisions: &BTreeMap<String, Revision>) -> Vec<&String> {
    let parents: HashSet<&str> =
        revisions.values().filter_map(|revision| revision.parent.as_deref()).collect();
    let mut heads: Vec<&String> = Vec::new();
    for hash in revisions.keys().filter(|hash| !parents.contains(hash.as_str())) {
        if !heads.iter().any(|head| revisions[*head].record == revisions[hash].record) {
            heads.push(hash);
        }
    }
    heads
        .iter()
        .copied()
        .filter(|hash| {
            !heads.iter().any(|other| other != hash && descends_from(other, hash, revisions))
        })
        .collect()
}

fn object_parts(key: &str) -> Result<(&'static str, &str)> {
    let (table, suffix) = if let Some(suffix) = key.strip_prefix("v1/revisions/") {
        ("sync_revisions", suffix)
    } else if let Some(suffix) = key.strip_prefix("v1/metadata/") {
        ("sync_objects", suffix)
    } else {
        anyhow::bail!("unsupported object in remote library: {key}");
    };
    let hash = suffix.strip_suffix(".json").context("invalid remote object name")?;
    ensure!(valid_digest(hash), "invalid remote object name");
    Ok((table, hash))
}
