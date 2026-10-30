use anyhow::{Context, Result, ensure};
use serde::Deserialize;

use crate::db::remote_store::{
    ZSTD_LEVEL, ZSTD_MAGIC, decode_stored, decode_zstd, encode_delta, valid_digest,
    validate_object_key,
};
use crate::db::store::Store;

const REVISIONS: &str = "v1/revisions/";
const COMPRESSED: &str = ".json.zst";
const DELTA: &str = ".delta-";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Encoding {
    Raw,
    Zstd,
    Delta(String),
}

#[derive(Clone, Debug)]
pub(super) struct Name {
    pub(super) canonical: String,
    pub(super) encoding: Encoding,
}

impl Name {
    pub(super) fn parse(key: &str) -> Result<Self> {
        if let Some(name) = key.strip_prefix(REVISIONS)
            && let Some(name) = name.strip_suffix(".zst")
        {
            let (hash, encoding) = match name.split_once(DELTA) {
                Some((hash, base)) => {
                    ensure!(valid_digest(base) && base != hash, "invalid remote object name");
                    (hash, Encoding::Delta(revision_key(base)))
                }
                None => (
                    name.strip_suffix(".json").context("invalid remote object name")?,
                    Encoding::Zstd,
                ),
            };
            ensure!(valid_digest(hash), "invalid remote object name");
            return Ok(Self { canonical: revision_key(hash), encoding });
        }
        validate_object_key(key)?;
        Ok(Self { canonical: key.to_string(), encoding: Encoding::Raw })
    }

    pub(super) fn base(&self) -> Option<&str> {
        match &self.encoding {
            Encoding::Delta(base) => Some(base),
            Encoding::Raw | Encoding::Zstd => None,
        }
    }

    pub(super) fn decode(&self, body: &[u8], base: Option<&[u8]>) -> Result<Vec<u8>> {
        match &self.encoding {
            Encoding::Raw => Ok(body.to_vec()),
            Encoding::Zstd => decode_zstd(body, None),
            Encoding::Delta(_) => decode_zstd(body, Some(base.context("missing delta base")?)),
        }
    }
}

fn revision_key(hash: &str) -> String {
    format!("{REVISIONS}{hash}.json")
}

#[derive(Deserialize)]
struct Parent {
    parent: Option<String>,
}

pub(super) fn encode(
    store: &Store,
    canonical: &str,
    available: &dyn Fn(&str) -> bool,
) -> Result<(String, Vec<u8>)> {
    let stored =
        store.cached_stored(canonical)?.context("remote recovery object is missing locally")?;
    let Some(hash) = canonical.strip_prefix(REVISIONS).and_then(|name| name.strip_suffix(".json"))
    else {
        return Ok((canonical.to_string(), decode_stored(stored)?));
    };
    let (raw, full) = if stored.starts_with(&ZSTD_MAGIC) {
        (decode_zstd(&stored, None)?, stored)
    } else {
        let full = zstd::encode_all(stored.as_slice(), ZSTD_LEVEL)?;
        (stored, full)
    };
    if let Some(parent) = serde_json::from_slice::<Parent>(&raw)?.parent {
        let base = revision_key(&parent);
        if available(&base)
            && let Some(prefix) = store.cached_remote(&base)?
        {
            let delta = encode_delta(&raw, &prefix)?;
            if delta.len() < full.len() {
                return Ok((format!("{REVISIONS}{hash}{DELTA}{parent}.zst"), delta));
            }
        }
    }
    Ok((format!("{REVISIONS}{hash}{COMPRESSED}"), full))
}
