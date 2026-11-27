use std::collections::{BTreeMap, HashSet};

use anyhow::{Context, Result, ensure};
#[cfg(test)]
use rusqlite::OptionalExtension;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::super::store::Store;
use super::{OBJECT_LIMIT, ZSTD_LEVEL, ZSTD_MAGIC};
use crate::host::Location;

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
pub(super) enum Metadata {
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

pub(super) fn validate_identity(value: &str) -> Result<()> {
    ensure!(uuid::Uuid::parse_str(value)?.to_string() == value, "invalid sync identity");
    Ok(())
}

impl Store {
    #[cfg(test)]
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
        self.cache_encoded(key, body, None)
    }

    pub(crate) fn cache_encoded(
        &self,
        key: &str,
        body: &[u8],
        stored: Option<&[u8]>,
    ) -> Result<()> {
        ensure!(body.len() <= OBJECT_LIMIT, "remote object exceeds 512 MiB");
        let stored = match stored.filter(|stored| stored.starts_with(&ZSTD_MAGIC)) {
            Some(stored) => stored.to_vec(),
            None => zstd::encode_all(body, ZSTD_LEVEL)?,
        };
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
            crate::import::decode_remote(
                revision.record.clone(),
                uuid::Uuid::new_v4().to_string(),
            )?;
            self.conn.execute(
                "INSERT OR IGNORE INTO sync_revisions(digest, sync_id, body, size) VALUES (?1, ?2, ?3, ?4)",
                params![hash, revision.sync_id, stored, size],
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
                params![hash, stored, size],
            )?;
        }
        Ok(())
    }
}

pub(super) fn descends_from(
    hash: &str,
    ancestor: &str,
    revisions: &BTreeMap<String, Revision>,
) -> bool {
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

pub(crate) fn validate_object_key(key: &str) -> Result<()> {
    object_parts(key).map(|_| ())
}

pub(super) fn object_parts(key: &str) -> Result<(&'static str, &str)> {
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
