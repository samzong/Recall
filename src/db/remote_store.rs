use std::collections::{BTreeMap, HashSet};

use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::session_store::{clear_session_contents_tx, persist_session_with_usage_and_events_tx};
use super::store::{SessionTopologyWrite, Store};
use crate::host::Location;
use crate::project_scope::ProjectScope;
use crate::types::Session;

pub(crate) const OBJECT_LIMIT: usize = 64 * 1024 * 1024;

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

    pub(crate) fn prepare_remote(&self, scope: &ProjectScope) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let sessions =
            self.list_export_sessions(None, super::search::TimeRange::All, scope, None, None)?;
        for session in &sessions {
            self.remote_identity(&session.id)?;
        }
        for session in sessions {
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
        body.map(|body| serde_json::from_slice(&body).map_err(Into::into)).transpose()
    }

    pub(crate) fn cached_remote(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let (table, hash) = object_parts(key)?;
        Ok(self
            .conn
            .query_row(&format!("SELECT body FROM {table} WHERE digest = ?1"), [hash], |row| {
                row.get(0)
            })
            .optional()?)
    }

    pub(crate) fn cache_remote(&self, key: &str, body: &[u8]) -> Result<()> {
        ensure!(body.len() <= OBJECT_LIMIT, "remote object exceeds 64 MiB");
        let (table, hash) = object_parts(key)?;
        ensure!(digest(body) == hash, "remote object content hash mismatch");
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
                "INSERT OR IGNORE INTO sync_revisions(digest, sync_id, body) VALUES (?1, ?2, ?3)",
                params![hash, revision.sync_id, body],
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
                "INSERT OR IGNORE INTO sync_objects(digest, body) VALUES (?1, ?2)",
                params![hash, body],
            )?;
        }
        Ok(())
    }

    fn alias_session(&self, sync_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT session_id FROM sync_aliases WHERE sync_id = ?1", [sync_id], |row| {
                row.get(0)
            })
            .optional()?)
    }

    fn legacy_match(&self, revision: &Revision) -> Result<Option<String>> {
        let record = &revision.record["session"];
        let mut stmt = self.conn.prepare("SELECT s.id FROM sessions s JOIN native_bindings n ON n.session_id = s.id WHERE s.is_import = 1 AND n.confirmed = 0 AND s.source = ?1 AND s.source_id = ?2")?;
        let candidates = stmt
            .query_map(params![record["source"].as_str(), record["source_id"].as_str()], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut matches = Vec::new();
        for id in candidates {
            if let Some(session) = self.get_session_by_id(&id)?
                && self.remote_snapshot(session)? == revision.record
            {
                matches.push(id);
            }
        }
        Ok(if matches.len() == 1 { matches.pop() } else { None })
    }

    fn write_remote_projection(
        &self,
        tx: &rusqlite::Transaction<'_>,
        id: &str,
        revision: &Revision,
    ) -> Result<()> {
        ensure!(!self.has_native_binding(id)?, "remote cannot replace a native session");
        let data = crate::import::decode_remote(revision.record.clone(), id.to_string())?;
        clear_session_contents_tx(tx, id)?;
        persist_session_with_usage_and_events_tx(
            tx,
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
            self.trigram_message_flag,
        )?;
        for (parent, identity) in data.topology.parents.iter().zip(&revision.parent_sessions) {
            tx.execute("UPDATE session_parent_links SET parent_sync_id = ?5 WHERE session_id = ?1 AND relation = ?2 AND parent_source = ?3 AND parent_source_id = ?4",
                params![id, parent.relation.as_str(), parent.source, parent.source_id, identity])?;
        }
        Ok(())
    }

    pub(crate) fn merge_remote(&self) -> Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        let hashes = self
            .conn
            .prepare("SELECT digest FROM sync_revisions ORDER BY digest")?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for hash in &hashes {
            let revision = self.remote_revision(hash)?.context("missing cached revision")?;
            if self.alias_session(&revision.sync_id)?.is_some() {
                continue;
            }
            let id = if let Some(id) = self.legacy_match(&revision)? {
                let old: Option<String> = tx
                    .query_row(
                        "SELECT current_revision FROM session_sync WHERE session_id = ?1",
                        [&id],
                        |row| row.get(0),
                    )
                    .optional()?
                    .flatten();
                if let Some(old) = old {
                    self.cache_metadata(&Metadata::Association {
                        version: 1,
                        left: old,
                        right: hash.clone(),
                    })?;
                }
                id
            } else {
                let id = uuid::Uuid::new_v4().to_string();
                self.write_remote_projection(&tx, &id, &revision)?;
                id
            };
            tx.execute(
                "INSERT OR IGNORE INTO session_sync(session_id, sync_id) VALUES (?1, ?2)",
                params![id, revision.sync_id],
            )?;
            tx.execute(
                "INSERT INTO sync_aliases(sync_id, session_id) VALUES (?1, ?2)",
                params![revision.sync_id, id],
            )?;
        }
        let metadata = self
            .conn
            .prepare("SELECT body FROM sync_objects ORDER BY digest")?
            .query_map([], |row| row.get::<_, Vec<u8>>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for body in &metadata {
            if let Metadata::Association { left, right, .. } = serde_json::from_slice(body)? {
                self.apply_association(&tx, &left, &right)?;
            }
        }
        for body in &metadata {
            if let Metadata::Locations { sync_id, locations, .. } = serde_json::from_slice(body)?
                && let Some(id) = self.alias_session(&sync_id)?
            {
                for location in locations {
                    crate::host::merge_location(&tx, &id, &location)?;
                }
            }
        }
        let sessions = tx
            .prepare("SELECT session_id, current_revision FROM session_sync")?
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut conflicts = 0;
        for (id, current) in sessions {
            let revisions = self.session_revisions(&id)?;
            let session = self.get_session_by_id(&id)?.context("missing remote session")?;
            for revision in revisions.values() {
                ensure!(
                    revision.record["session"]["source"].as_str() == Some(&session.source)
                        && revision.record["session"]["source_id"].as_str()
                            == Some(&session.source_id),
                    "remote identity contains different native session IDs"
                );
                if let Some(parent) = &revision.parent
                    && let Some(parent_revision) = self.remote_revision(parent)?
                {
                    ensure!(
                        self.alias_session(&parent_revision.sync_id)?.as_deref() == Some(&id),
                        "remote parent belongs to another session"
                    );
                }
            }
            let heads = revision_heads(&revisions);
            let alternatives = heads
                .iter()
                .filter(|head| {
                    current
                        .as_ref()
                        .and_then(|hash| revisions.get(hash))
                        .is_none_or(|selected| selected.record != revisions[**head].record)
                })
                .count();
            tx.execute(
                "UPDATE session_sync SET alternative_versions = ?2 WHERE session_id = ?1",
                params![id, alternatives],
            )?;
            if heads.len() > 1 {
                conflicts += 1;
            }
            if self.has_native_binding(&id)? {
                continue;
            }
            let chosen = if let Some(current) = current.as_ref() {
                let descendants: Vec<_> =
                    heads.iter().filter(|head| descends_from(head, current, &revisions)).collect();
                if descendants.len() == 1 { Some((*descendants[0]).clone()) } else { None }
            } else {
                heads.first().map(|head| (*head).clone())
            };
            if let Some(chosen) = chosen
                && Some(&chosen) != current.as_ref()
            {
                self.write_remote_projection(&tx, &id, &revisions[&chosen])?;
                tx.execute(
                    "UPDATE session_sync SET current_revision = ?2 WHERE session_id = ?1",
                    params![id, chosen],
                )?;
                tx.execute(
                    "UPDATE session_sync SET alternative_versions = ?2 WHERE session_id = ?1",
                    params![id, heads.len().saturating_sub(1)],
                )?;
            }
        }
        tx.commit()?;
        Ok(conflicts)
    }

    fn apply_association(
        &self,
        tx: &rusqlite::Transaction<'_>,
        left: &str,
        right: &str,
    ) -> Result<()> {
        let (Some(left), Some(right)) = (self.remote_revision(left)?, self.remote_revision(right)?)
        else {
            return Ok(());
        };
        ensure!(
            left.record == right.record && left.sync_id != right.sync_id,
            "legacy association proof does not match"
        );
        let (Some(a), Some(b)) =
            (self.alias_session(&left.sync_id)?, self.alias_session(&right.sync_id)?)
        else {
            return Ok(());
        };
        if a == b {
            return Ok(());
        }
        let a_native = self.has_native_binding(&a)?;
        let b_native = self.has_native_binding(&b)?;
        ensure!(!(a_native && b_native), "legacy association cannot combine two native bindings");
        let (keep, remove) = if b_native || (!a_native && b < a) { (b, a) } else { (a, b) };
        for location in crate::host::locations(tx, &remove)? {
            crate::host::merge_location(tx, &keep, &location)?;
        }
        tx.execute(
            "UPDATE sync_aliases SET session_id = ?1 WHERE session_id = ?2",
            params![keep, remove],
        )?;
        clear_session_contents_tx(tx, &remove)?;
        tx.execute("DELETE FROM sessions WHERE id = ?1", [remove])?;
        Ok(())
    }

    pub(crate) fn session_revisions(&self, id: &str) -> Result<BTreeMap<String, Revision>> {
        let mut stmt = self.conn.prepare("SELECT r.digest, r.body FROM sync_revisions r JOIN sync_aliases a ON a.sync_id = r.sync_id WHERE a.session_id = ?1 ORDER BY r.digest")?;
        let rows = stmt
            .query_map([id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(|(hash, body)| Ok((hash, serde_json::from_slice(&body)?))).collect()
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
            keys.extend(
                self.session_revisions(&session.id)?
                    .keys()
                    .map(|hash| format!("v1/revisions/{hash}.json")),
            );
        }
        let metadata = self
            .conn
            .prepare("SELECT digest, body FROM sync_objects")?
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (hash, body) in metadata {
            let include = match serde_json::from_slice(&body)? {
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
