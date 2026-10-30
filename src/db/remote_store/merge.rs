use super::*;
use crate::db::session_store::{
    clear_session_contents_tx, persist_session_with_usage_and_events_tx,
};
use crate::db::store::SessionTopologyWrite;

impl Store {
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
            .prepare(
                "SELECT digest FROM sync_revisions r
                 WHERE NOT EXISTS (SELECT 1 FROM sync_aliases a WHERE a.sync_id = r.sync_id)
                 ORDER BY digest",
            )?
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
            .map(|body| decode_stored(body?))
            .collect::<Result<Vec<_>>>()?;
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
}
