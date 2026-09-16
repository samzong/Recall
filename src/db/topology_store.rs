use anyhow::Result;
use rusqlite::OptionalExtension;

use super::store::{SessionTopologyWrite, Store};
use crate::types::{ParentLink, Session, SessionTopology, ThreadRole};

impl Store {
    pub(crate) fn persist_topology_for_existing_session(
        &self,
        source: &str,
        source_id: &str,
        topology: &SessionTopologyWrite<'_>,
    ) -> Result<bool> {
        let tx = self.conn.unchecked_transaction()?;
        let session_id: Option<String> = tx
            .query_row(
                "SELECT session_id FROM native_bindings WHERE source = ?1 AND source_id = ?2",
                rusqlite::params![source, source_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(session_id) = session_id else {
            return Ok(false);
        };
        tx.execute(
            "UPDATE sessions SET thread_role = ?1, metadata_parser_version = ?2 WHERE id = ?3",
            rusqlite::params![
                topology.thread_role.map(|role| role.as_str()),
                topology.parser_version,
                session_id,
            ],
        )?;
        replace_parent_links_tx(&tx, &session_id, topology.parents)?;
        tx.commit()?;
        Ok(true)
    }

    pub(crate) fn session_topology(&self, session_id: &str) -> Result<SessionTopology> {
        let thread_role = self
            .conn
            .query_row(
                "SELECT thread_role FROM sessions WHERE id = ?1",
                rusqlite::params![session_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten()
            .and_then(|role| role.parse::<ThreadRole>().ok());
        let mut stmt = self.conn.prepare(
            "SELECT relation, parent_source, parent_source_id
             FROM session_parent_links
             WHERE session_id = ?1
             ORDER BY relation, parent_source, parent_source_id",
        )?;
        let parents = stmt
            .query_map(rusqlite::params![session_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter_map(|(relation, source, source_id)| {
                relation.parse().ok().map(|relation| ParentLink { relation, source, source_id })
            })
            .collect();
        Ok(SessionTopology { thread_role, parents })
    }
    pub(crate) fn resolve_parent(
        &self,
        child_id: &str,
        parent: &ParentLink,
    ) -> Result<Option<Session>> {
        let sync_id: Option<String> = self.conn.query_row(
            "SELECT parent_sync_id FROM session_parent_links
             WHERE session_id = ?1 AND relation = ?2 AND parent_source = ?3 AND parent_source_id = ?4",
            rusqlite::params![child_id, parent.relation.as_str(), parent.source, parent.source_id],
            |row| row.get(0),
        ).optional()?.flatten();
        let parent_id: Option<String> =
            if let Some(sync_id) = sync_id {
                self.conn
                    .query_row(
                        "SELECT session_id FROM sync_aliases WHERE sync_id = ?1",
                        [sync_id],
                        |row| row.get(0),
                    )
                    .optional()?
            } else {
                self.conn.query_row(
                "SELECT session_id FROM native_bindings WHERE source = ?1 AND source_id = ?2
                 AND EXISTS(SELECT 1 FROM native_bindings WHERE session_id = ?3)",
                rusqlite::params![parent.source, parent.source_id, child_id], |row| row.get(0),
            ).optional()?
            };
        parent_id.map(|id| self.get_session_by_id(&id)).transpose().map(Option::flatten)
    }

    pub(crate) fn child_subagents(&self, parent_id: &str) -> Result<Vec<Session>> {
        let Some(parent) = self.get_session_by_id(parent_id)? else {
            return Ok(Vec::new());
        };
        let mut stmt = self.conn.prepare(
            "SELECT s.id FROM sessions s JOIN session_parent_links l ON l.session_id = s.id
             WHERE l.parent_source = ?1 AND l.parent_source_id = ?2 AND l.relation = 'spawn'
             ORDER BY s.started_at, s.id",
        )?;
        let ids = stmt
            .query_map(rusqlite::params![parent.source, parent.source_id], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let link = ParentLink {
            relation: crate::types::ParentRelation::Spawn,
            source: parent.source,
            source_id: parent.source_id,
        };
        let mut children = Vec::new();
        for id in ids {
            if self.resolve_parent(&id, &link)?.is_some_and(|resolved| resolved.id == parent_id)
                && let Some(child) = self.get_session_by_id(&id)?
            {
                children.push(child);
            }
        }
        Ok(children)
    }
}

pub(super) fn replace_parent_links_tx(
    tx: &rusqlite::Transaction<'_>,
    session_id: &str,
    parents: &[ParentLink],
) -> Result<()> {
    tx.execute(
        "DELETE FROM session_parent_links WHERE session_id = ?1",
        rusqlite::params![session_id],
    )?;
    if parents.is_empty() {
        return Ok(());
    }
    let mut stmt = tx.prepare(
        "INSERT OR IGNORE INTO session_parent_links
            (session_id, relation, parent_source, parent_source_id)
         VALUES (?1, ?2, ?3, ?4)",
    )?;
    for parent in parents {
        stmt.execute(rusqlite::params![
            session_id,
            parent.relation.as_str(),
            parent.source,
            parent.source_id,
        ])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema;
    use crate::db::search::{ThreadRoleFilter, TimeRange};
    use crate::db::session_store::persist_session_with_usage_and_events_tx;
    use crate::db::store::SessionListSort;
    use crate::project_scope::ProjectScope;
    use crate::types::{Message, ParentLink, ParentRelation, Role, Session, ThreadRole};

    fn store() -> Store {
        schema::register_sqlite_vec();
        Store::open_in_memory().unwrap()
    }

    fn sess(source_id: &str) -> Session {
        Session {
            source: "codex".to_string(),
            source_id: source_id.to_string(),
            title: "t".to_string(),
            updated_at: Some(10),
            message_count: 1,
            ..crate::types::test_support::session(&format!("local-{source_id}"))
        }
    }

    fn msg(session_id: &str) -> Vec<Message> {
        vec![Message {
            session_id: session_id.to_string(),
            role: Role::User,
            content: "hello world".to_string(),
            timestamp: Some(1),
            seq: 0,
        }]
    }

    fn persist(store: &Store, session: &Session, topology: &SessionTopologyWrite<'_>) {
        store
            .persist_session_with_usage_and_events_with_topology(
                session,
                &msg(&session.id),
                &[],
                None,
                &[],
                None,
                topology,
            )
            .unwrap();
    }

    #[test]
    fn native_refresh_and_removal_leave_same_id_replicas_and_sync_identity_intact() {
        let store = store();
        let mut native = sess("collision");
        native.directory = Some("/work/project".into());
        persist(&store, &native, &SessionTopologyWrite::none());
        let mut replica = native.clone();
        replica.id = "remote-copy".into();
        replica.is_import = true;
        let tx = store.conn.unchecked_transaction().unwrap();
        persist_session_with_usage_and_events_tx(
            &tx,
            &replica,
            &msg(&replica.id),
            &[],
            Some(1),
            &[],
            Some(1),
            &SessionTopologyWrite::none(),
            store.trigram_message_flag,
        )
        .unwrap();
        tx.commit().unwrap();
        assert!(store.get_session_by_source_id("codex", "collision").is_err());
        assert_eq!(store.get_native_session("codex", "collision").unwrap().unwrap().id, native.id);
        assert_eq!(store.session_meta_map("codex").unwrap().len(), 1);
        assert!(store.usage_state_meta_map("codex").unwrap().is_empty());
        assert!(store.event_state_meta_map("codex").unwrap().is_empty());
        assert_eq!(
            store
                .list_indexed_sessions(
                    None,
                    TimeRange::All,
                    &ProjectScope::Directory("/work/project".into()),
                    None,
                    None,
                    0,
                    SessionListSort::Newest
                )
                .unwrap()
                .len(),
            1
        );
        store
            .conn
            .execute("INSERT INTO session_sync(session_id, sync_id, current_revision) VALUES (?1, 'shared-identity', NULL)", [&native.id])
            .unwrap();
        let mut host = crate::host::Host {
            id: uuid::Uuid::new_v4().to_string(),
            name: "macbook".into(),
            revision: 1,
        };
        host.observe(&store.conn, "codex", "collision").unwrap();
        native.title = "refreshed".into();
        store
            .replace_session_with_usage_and_events_with_topology(
                "codex",
                "collision",
                &native,
                &msg(&native.id),
                &[],
                None,
                &[],
                None,
                &SessionTopologyWrite::none(),
            )
            .unwrap();
        assert_eq!(
            store.get_session_by_id(&native.id).unwrap().unwrap().locations[0].host.name,
            "macbook"
        );
        assert_eq!(store.get_session_by_id(&replica.id).unwrap().unwrap().title, "t");
        host.name = "renamed".into();
        host.revision += 1;
        host.observe(&store.conn, "codex", "collision").unwrap();
        assert_eq!(
            store.get_session_by_id(&native.id).unwrap().unwrap().locations[0].host.name,
            "renamed"
        );
        assert!(store.get_session_by_id(&replica.id).unwrap().unwrap().locations.is_empty());
        store.delete_session_data("codex", "collision").unwrap();
        assert!(store.get_native_session("codex", "collision").unwrap().is_none());
        assert_eq!(store.get_messages(&native.id).unwrap().len(), 1);
        assert_eq!(store.get_messages(&replica.id).unwrap().len(), 1);
        assert!(store.session_meta_map("codex").unwrap().is_empty());
        assert!(store.get_session_by_id(&native.id).unwrap().unwrap().is_import);
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT sync_id FROM session_sync WHERE session_id = ?1",
                    [&native.id],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            "shared-identity"
        );
    }

    #[test]
    fn session_topology_round_trips_role_and_parents() {
        let store = store();
        let session = sess("s1");
        let parents = vec![
            ParentLink {
                relation: ParentRelation::Spawn,
                source: "codex".to_string(),
                source_id: "p-spawn".to_string(),
            },
            ParentLink {
                relation: ParentRelation::Fork,
                source: "codex".to_string(),
                source_id: "p-fork".to_string(),
            },
        ];
        persist(
            &store,
            &session,
            &SessionTopologyWrite {
                thread_role: Some(ThreadRole::Subagent),
                parents: &parents,
                parser_version: Some(1),
            },
        );

        let topology = store.session_topology(&session.id).unwrap();
        assert_eq!(topology.thread_role, Some(ThreadRole::Subagent));
        assert_eq!(
            topology.parents,
            vec![
                ParentLink {
                    relation: ParentRelation::Fork,
                    source: "codex".to_string(),
                    source_id: "p-fork".to_string(),
                },
                ParentLink {
                    relation: ParentRelation::Spawn,
                    source: "codex".to_string(),
                    source_id: "p-spawn".to_string(),
                },
            ]
        );
    }

    #[test]
    fn metadata_backfill_updates_topology_without_touching_messages() {
        let store = store();
        let session = sess("s1");
        persist(
            &store,
            &session,
            &SessionTopologyWrite {
                thread_role: Some(ThreadRole::Primary),
                parents: &[],
                parser_version: Some(1),
            },
        );
        let embedding_before: i64 = store
            .conn
            .query_row(
                "SELECT units_total FROM session_embedding_state WHERE session_id = ?1",
                [&session.id],
                |r| r.get(0),
            )
            .unwrap();

        let parents = vec![ParentLink {
            relation: ParentRelation::Fork,
            source: "codex".to_string(),
            source_id: "p1".to_string(),
        }];
        let updated = store
            .persist_topology_for_existing_session(
                "codex",
                "s1",
                &SessionTopologyWrite {
                    thread_role: Some(ThreadRole::Subagent),
                    parents: &parents,
                    parser_version: Some(2),
                },
            )
            .unwrap();
        assert!(updated);

        let topology = store.session_topology(&session.id).unwrap();
        assert_eq!(topology.thread_role, Some(ThreadRole::Subagent));
        assert_eq!(topology.parents, parents);

        assert_eq!(store.get_messages(&session.id).unwrap().len(), 1);
        let same_id: String = store
            .conn
            .query_row(
                "SELECT id FROM sessions WHERE source = 'codex' AND source_id = 's1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(same_id, session.id);
        let embedding_after: i64 = store
            .conn
            .query_row(
                "SELECT units_total FROM session_embedding_state WHERE session_id = ?1",
                [&session.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(embedding_before, embedding_after);

        let meta = store.metadata_state_meta_map("codex").unwrap();
        assert_eq!(meta.get("s1").map(|m| m.parser_version), Some(2));
    }

    #[test]
    fn persist_topology_for_missing_session_reports_false() {
        let store = store();
        let updated = store
            .persist_topology_for_existing_session("codex", "absent", &SessionTopologyWrite::none())
            .unwrap();
        assert!(!updated);
    }

    #[test]
    fn list_indexed_sessions_filters_by_thread_role() {
        let store = store();
        persist(
            &store,
            &sess("primary"),
            &SessionTopologyWrite {
                thread_role: Some(ThreadRole::Primary),
                parents: &[],
                parser_version: Some(1),
            },
        );
        persist(
            &store,
            &sess("subagent"),
            &SessionTopologyWrite {
                thread_role: Some(ThreadRole::Subagent),
                parents: &[],
                parser_version: Some(1),
            },
        );
        persist(
            &store,
            &sess("unknown"),
            &SessionTopologyWrite { thread_role: None, parents: &[], parser_version: Some(1) },
        );

        let ids = |filter| {
            store
                .list_indexed_sessions(
                    None,
                    TimeRange::All,
                    &ProjectScope::Global,
                    Some(filter),
                    None,
                    0,
                    SessionListSort::Newest,
                )
                .unwrap()
                .into_iter()
                .map(|s| s.source_id)
                .collect::<Vec<_>>()
        };

        assert_eq!(ids(ThreadRoleFilter::Primary), vec!["primary".to_string()]);
        assert_eq!(ids(ThreadRoleFilter::Subagent), vec!["subagent".to_string()]);
        assert_eq!(ids(ThreadRoleFilter::Unknown), vec!["unknown".to_string()]);
    }

    #[test]
    fn recent_sessions_for_search_scope_hides_only_reachable_subagents() {
        let store = store();
        persist(
            &store,
            &sess("primary"),
            &SessionTopologyWrite {
                thread_role: Some(ThreadRole::Primary),
                parents: &[],
                parser_version: Some(1),
            },
        );
        let spawn_parent = ParentLink {
            relation: ParentRelation::Spawn,
            source: "codex".to_string(),
            source_id: "primary".to_string(),
        };
        persist(
            &store,
            &sess("child"),
            &SessionTopologyWrite {
                thread_role: Some(ThreadRole::Subagent),
                parents: std::slice::from_ref(&spawn_parent),
                parser_version: Some(1),
            },
        );
        persist(
            &store,
            &sess("orphan"),
            &SessionTopologyWrite {
                thread_role: Some(ThreadRole::Subagent),
                parents: &[],
                parser_version: Some(1),
            },
        );
        persist(
            &store,
            &sess("unknown"),
            &SessionTopologyWrite { thread_role: None, parents: &[], parser_version: Some(1) },
        );

        let mut ids = store
            .list_recent_sessions_for_search_scope(
                None,
                TimeRange::All,
                &ProjectScope::Global,
                None,
                100,
            )
            .unwrap()
            .into_iter()
            .map(|s| s.source_id)
            .collect::<Vec<_>>();
        ids.sort();
        assert_eq!(
            ids,
            vec!["orphan".to_string(), "primary".to_string(), "unknown".to_string()],
            "reachable child hidden; orphaned subagent stays visible"
        );
    }

    #[test]
    fn recent_sessions_keeps_subagent_when_parent_is_beyond_result_limit() {
        let store = store();
        let spawn = ParentLink {
            relation: ParentRelation::Spawn,
            source: "codex".to_string(),
            source_id: "primary".to_string(),
        };
        let mut child = sess("child");
        child.updated_at = Some(100);
        persist(
            &store,
            &child,
            &SessionTopologyWrite {
                thread_role: Some(ThreadRole::Subagent),
                parents: std::slice::from_ref(&spawn),
                parser_version: Some(1),
            },
        );
        let mut visible = sess("visible");
        visible.updated_at = Some(90);
        persist(
            &store,
            &visible,
            &SessionTopologyWrite {
                thread_role: Some(ThreadRole::Primary),
                parents: &[],
                parser_version: Some(1),
            },
        );
        let mut primary = sess("primary");
        primary.updated_at = Some(80);
        persist(
            &store,
            &primary,
            &SessionTopologyWrite {
                thread_role: Some(ThreadRole::Primary),
                parents: &[],
                parser_version: Some(1),
            },
        );

        let ids = store
            .list_recent_sessions_for_search_scope(
                None,
                TimeRange::All,
                &ProjectScope::Global,
                None,
                2,
            )
            .unwrap()
            .into_iter()
            .map(|session| session.source_id)
            .collect::<Vec<_>>();

        assert_eq!(ids, vec!["child", "visible"]);
    }

    #[test]
    fn recent_sessions_keeps_subagent_when_parent_is_out_of_scope() {
        let store = store();
        let now = chrono::Utc::now().timestamp_millis();
        let mut parent = sess("primary");
        parent.started_at = now - 3 * 86_400_000;
        parent.updated_at = Some(parent.started_at);
        let mut child = sess("child");
        child.started_at = now;
        child.updated_at = Some(now);
        persist(
            &store,
            &parent,
            &SessionTopologyWrite {
                thread_role: Some(ThreadRole::Primary),
                parents: &[],
                parser_version: Some(1),
            },
        );
        let spawn = ParentLink {
            relation: ParentRelation::Spawn,
            source: "codex".to_string(),
            source_id: "primary".to_string(),
        };
        persist(
            &store,
            &child,
            &SessionTopologyWrite {
                thread_role: Some(ThreadRole::Subagent),
                parents: std::slice::from_ref(&spawn),
                parser_version: Some(1),
            },
        );
        let ids: Vec<String> = store
            .list_recent_sessions_for_search_scope(
                None,
                TimeRange::Today,
                &ProjectScope::Global,
                None,
                100,
            )
            .unwrap()
            .into_iter()
            .map(|s| s.source_id)
            .collect();
        assert_eq!(
            ids,
            vec!["child".to_string()],
            "subagent stays visible when its parent is out of the active scope"
        );
    }
}
