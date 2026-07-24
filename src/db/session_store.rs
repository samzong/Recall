use std::collections::{HashMap, HashSet};

use anyhow::Result;
use chrono::Utc;
use rusqlite::OptionalExtension;

use super::event_store::replace_session_events;
use super::project_store::apply_scope_filters;
use super::store::{
    MetadataSessionStateMeta, SESSION_COLUMNS, SessionListSort, SessionTopologyWrite, Store,
    session_from_row,
};
use crate::db::search::{RepoFilter, ThreadRoleFilter, TimeRange};
use crate::types::{
    Message, ParentLink, RawSessionEvent, RawUsageEvent, Role, Session, SessionTopology, ThreadRole,
};

impl Store {
    pub(crate) fn session_meta(
        &self,
        source: &str,
        source_id: &str,
    ) -> Result<Option<(Option<i64>, u32)>> {
        let mut stmt = self.conn.prepare(
            "SELECT updated_at, message_count FROM sessions WHERE source = ?1 AND source_id = ?2",
        )?;
        let mut rows = stmt.query(rusqlite::params![source, source_id])?;
        match rows.next()? {
            Some(row) => Ok(Some((row.get(0)?, row.get(1)?))),
            None => Ok(None),
        }
    }

    pub(crate) fn session_meta_map(
        &self,
        source: &str,
    ) -> Result<HashMap<String, (Option<i64>, u32)>> {
        let mut stmt = self.conn.prepare(
            "SELECT source_id, updated_at, message_count FROM sessions WHERE source = ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![source], |row| {
            Ok((row.get::<_, String>(0)?, (row.get(1)?, row.get(2)?)))
        })?;
        rows.collect::<Result<HashMap<_, _>, _>>().map_err(Into::into)
    }

    pub(crate) fn imported_source_ids(&self, source: &str) -> Result<HashSet<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT source_id FROM sessions WHERE source = ?1 AND is_import = 1")?;
        let rows = stmt.query_map(rusqlite::params![source], |row| row.get(0))?;
        rows.collect::<Result<HashSet<_>, _>>().map_err(Into::into)
    }

    pub(crate) fn clear_import_marker(&self, source: &str, source_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET is_import = 0 WHERE source = ?1 AND source_id = ?2",
            rusqlite::params![source, source_id],
        )?;
        Ok(())
    }

    pub(crate) fn update_session_fields(
        &self,
        source: &str,
        source_id: &str,
        custom_title: Option<&str>,
        summary: Option<&str>,
        duration_minutes: Option<u32>,
        source_file_path: Option<&str>,
    ) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE sessions
                SET custom_title = COALESCE(?3, custom_title),
                    summary = COALESCE(?4, summary),
                    duration_minutes = COALESCE(?5, duration_minutes),
                    source_file_path = COALESCE(?6, source_file_path),
                    title = CASE
                        WHEN ?3 IS NOT NULL AND ?3 != '' THEN ?3
                        ELSE title
                    END
              WHERE source = ?1 AND source_id = ?2",
            rusqlite::params![
                source,
                source_id,
                custom_title,
                summary,
                duration_minutes,
                source_file_path,
            ],
        )?;
        Ok(n > 0)
    }

    #[cfg(test)]
    pub(crate) fn insert_session(&self, session: &Session) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sessions (id, source, source_id, title, directory, repo_remote, repo_slug, repo_name, started_at, updated_at, message_count, entrypoint, custom_title, summary, duration_minutes, source_file_path, is_import)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
            rusqlite::params![
                session.id,
                session.source,
                session.source_id,
                session.title,
                session.directory,
                session.repo_remote,
                session.repo_slug,
                session.repo_name,
                session.started_at,
                session.updated_at,
                session.message_count,
                session.entrypoint,
                session.custom_title,
                session.summary,
                session.duration_minutes,
                session.source_file_path,
                session.is_import,
            ],
        )?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn insert_messages(&self, messages: &[Message]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO messages (session_id, role, content, timestamp, seq)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for msg in messages {
                stmt.execute(rusqlite::params![
                    msg.session_id,
                    msg.role.as_str(),
                    msg.content,
                    msg.timestamp,
                    msg.seq,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn persist_session_with_usage(
        &self,
        session: &Session,
        messages: &[Message],
        usage_events: &[RawUsageEvent],
        usage_parser_version: Option<u32>,
    ) -> Result<()> {
        self.persist_session_with_usage_and_events(
            session,
            messages,
            usage_events,
            usage_parser_version,
            &[],
            None,
        )
    }

    #[cfg(test)]
    pub(crate) fn persist_session_with_usage_and_events(
        &self,
        session: &Session,
        messages: &[Message],
        usage_events: &[RawUsageEvent],
        usage_parser_version: Option<u32>,
        session_events: &[RawSessionEvent],
        event_parser_version: Option<u32>,
    ) -> Result<()> {
        self.persist_session_with_usage_and_events_with_topology(
            session,
            messages,
            usage_events,
            usage_parser_version,
            session_events,
            event_parser_version,
            &SessionTopologyWrite::none(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn persist_session_with_usage_and_events_with_topology(
        &self,
        session: &Session,
        messages: &[Message],
        usage_events: &[RawUsageEvent],
        usage_parser_version: Option<u32>,
        session_events: &[RawSessionEvent],
        event_parser_version: Option<u32>,
        topology: &SessionTopologyWrite<'_>,
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        persist_session_with_usage_and_events_tx(
            &tx,
            session,
            messages,
            usage_events,
            usage_parser_version,
            session_events,
            event_parser_version,
            topology,
        )?;
        tx.commit()?;
        Ok(())
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn replace_session_with_usage_and_events(
        &self,
        old_source: &str,
        old_source_id: &str,
        session: &Session,
        messages: &[Message],
        usage_events: &[RawUsageEvent],
        usage_parser_version: Option<u32>,
        session_events: &[RawSessionEvent],
        event_parser_version: Option<u32>,
    ) -> Result<()> {
        self.replace_session_with_usage_and_events_with_topology(
            old_source,
            old_source_id,
            session,
            messages,
            usage_events,
            usage_parser_version,
            session_events,
            event_parser_version,
            &SessionTopologyWrite::none(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn replace_session_with_usage_and_events_with_topology(
        &self,
        old_source: &str,
        old_source_id: &str,
        session: &Session,
        messages: &[Message],
        usage_events: &[RawUsageEvent],
        usage_parser_version: Option<u32>,
        session_events: &[RawSessionEvent],
        event_parser_version: Option<u32>,
        topology: &SessionTopologyWrite<'_>,
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        delete_session_data_tx(&tx, old_source, old_source_id)?;
        persist_session_with_usage_and_events_tx(
            &tx,
            session,
            messages,
            usage_events,
            usage_parser_version,
            session_events,
            event_parser_version,
            topology,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Topology-only write for an already-indexed session: does not touch
    /// messages, usage, events, embeddings, or the local session id.
    pub(crate) fn persist_topology_for_existing_session(
        &self,
        source: &str,
        source_id: &str,
        topology: &SessionTopologyWrite<'_>,
    ) -> Result<bool> {
        let tx = self.conn.unchecked_transaction()?;
        let session_id: Option<String> = tx
            .query_row(
                "SELECT id FROM sessions WHERE source = ?1 AND source_id = ?2",
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

    pub(crate) fn metadata_state_meta_map(
        &self,
        source: &str,
    ) -> Result<HashMap<String, MetadataSessionStateMeta>> {
        let mut stmt = self.conn.prepare(
            "SELECT source_id, metadata_parser_version, updated_at
             FROM sessions
             WHERE source = ?1 AND metadata_parser_version IS NOT NULL",
        )?;
        let rows = stmt.query_map(rusqlite::params![source], |row| {
            Ok((
                row.get::<_, String>(0)?,
                MetadataSessionStateMeta {
                    parser_version: row.get(1)?,
                    source_updated_at: row.get(2)?,
                },
            ))
        })?;
        rows.collect::<Result<HashMap<_, _>, _>>().map_err(Into::into)
    }

    pub(crate) fn delete_session_data(&self, source: &str, source_id: &str) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        delete_session_data_tx(&tx, source, source_id)?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn list_sessions_by_ids(&self, session_ids: &[String]) -> Result<Vec<Session>> {
        if session_ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders =
            std::iter::repeat_n("?", session_ids.len()).collect::<Vec<_>>().join(", ");
        let sql = format!(
            "SELECT {SESSION_COLUMNS}
             FROM sessions
             WHERE id IN ({placeholders})
             ORDER BY started_at DESC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::types::ToSql> =
            session_ids.iter().map(|id| id as &dyn rusqlite::types::ToSql).collect();
        let rows = stmt.query_map(params.as_slice(), session_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub(crate) fn get_session_by_id(&self, session_id: &str) -> Result<Option<Session>> {
        self.conn
            .query_row(
                &format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE id = ?1"),
                rusqlite::params![session_id],
                session_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub(crate) fn get_session_by_source_id(
        &self,
        source: &str,
        source_id: &str,
    ) -> Result<Option<Session>> {
        self.conn
            .query_row(
                &format!(
                    "SELECT {SESSION_COLUMNS}
                     FROM sessions
                     WHERE source = ?1 AND source_id = ?2"
                ),
                rusqlite::params![source, source_id],
                session_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub(crate) fn get_messages(&self, session_id: &str) -> Result<Vec<Message>> {
        let mut stmt = self.conn.prepare(
            "SELECT role, content, timestamp, seq FROM messages WHERE session_id = ?1 ORDER BY seq",
        )?;
        let rows = stmt.query_map(rusqlite::params![session_id], |row| {
            let role_str: String = row.get(0)?;
            Ok(Message {
                session_id: session_id.to_string(),
                role: role_str.parse().unwrap_or(Role::User),
                content: row.get(1)?,
                timestamp: row.get(2)?,
                seq: row.get(3)?,
            })
        })?;
        let mut messages = Vec::new();
        for row in rows {
            messages.push(row?);
        }
        Ok(messages)
    }

    pub(crate) fn stats(&self) -> Result<(u64, u64)> {
        self.stats_for_scope(None, TimeRange::All)
    }

    pub(crate) fn stats_for_scope(
        &self,
        sources: Option<&[String]>,
        time_range: TimeRange,
    ) -> Result<(u64, u64)> {
        self.stats_for_search_scope(sources, time_range, None, None)
    }

    pub(crate) fn stats_for_search_scope(
        &self,
        sources: Option<&[String]>,
        time_range: TimeRange,
        directory: Option<&str>,
        repo: Option<&RepoFilter>,
    ) -> Result<(u64, u64)> {
        let mut session_sql = String::from("SELECT COUNT(*) FROM sessions s WHERE 1=1");
        let mut session_params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let mut session_param_idx = 1;
        apply_scope_filters(
            &mut session_sql,
            &mut session_params,
            &mut session_param_idx,
            sources,
            time_range,
            directory,
            repo,
        );
        let session_param_refs: Vec<&dyn rusqlite::types::ToSql> =
            session_params.iter().map(|p| p.as_ref()).collect();
        let session_count: u64 =
            self.conn.query_row(&session_sql, session_param_refs.as_slice(), |row| row.get(0))?;

        let mut message_sql = String::from(
            "SELECT COUNT(*)
             FROM messages m
             JOIN sessions s ON s.id = m.session_id
             WHERE 1=1",
        );
        let mut message_params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let mut message_param_idx = 1;
        apply_scope_filters(
            &mut message_sql,
            &mut message_params,
            &mut message_param_idx,
            sources,
            time_range,
            directory,
            repo,
        );
        let message_param_refs: Vec<&dyn rusqlite::types::ToSql> =
            message_params.iter().map(|p| p.as_ref()).collect();
        let message_count: u64 =
            self.conn.query_row(&message_sql, message_param_refs.as_slice(), |row| row.get(0))?;
        Ok((session_count, message_count))
    }

    #[cfg(test)]
    pub(crate) fn list_recent_sessions(&self, limit: usize) -> Result<Vec<Session>> {
        self.list_recent_sessions_for_search_scope(None, TimeRange::All, None, None, limit)
    }

    pub(crate) fn list_recent_sessions_for_search_scope(
        &self,
        sources: Option<&[String]>,
        time_range: TimeRange,
        directory: Option<&str>,
        repo: Option<&RepoFilter>,
        limit: usize,
    ) -> Result<Vec<Session>> {
        let mut sql = format!(
            "SELECT {SESSION_COLUMNS}
             FROM sessions s
             WHERE 1=1"
        );
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let mut param_idx = 1;
        apply_scope_filters(
            &mut sql,
            &mut params,
            &mut param_idx,
            sources,
            time_range,
            directory,
            repo,
        );
        sql.push_str(&format!(
            " ORDER BY COALESCE(updated_at, started_at) DESC, started_at DESC, source ASC, source_id ASC LIMIT ?{param_idx}"
        ));
        params.push(Box::new(limit as i64));
        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(param_refs.as_slice(), session_from_row)?;
        let mut sessions = Vec::new();
        for row in rows {
            sessions.push(row?);
        }
        Ok(sessions)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn list_indexed_sessions(
        &self,
        sources: Option<&[String]>,
        time_range: TimeRange,
        directory: Option<&str>,
        repo: Option<&RepoFilter>,
        thread_role: Option<ThreadRoleFilter>,
        limit: Option<usize>,
        offset: usize,
        sort: SessionListSort,
    ) -> Result<Vec<Session>> {
        let mut sql = format!(
            "SELECT {SESSION_COLUMNS}
             FROM sessions s
             WHERE 1=1"
        );
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let mut param_idx = 1;
        apply_scope_filters(
            &mut sql,
            &mut params,
            &mut param_idx,
            sources,
            time_range,
            directory,
            repo,
        );
        if let Some(thread_role) = thread_role {
            sql.push_str(thread_role.sql_predicate());
        }
        let order_by = match sort {
            SessionListSort::Newest => "s.started_at DESC, source ASC, source_id ASC",
            SessionListSort::Oldest => "s.started_at ASC, source ASC, source_id ASC",
            SessionListSort::Updated => {
                "COALESCE(s.updated_at, s.started_at) DESC, s.started_at DESC, source ASC, source_id ASC"
            }
        };
        sql.push_str(&format!(" ORDER BY {order_by}"));
        if let Some(limit) = limit {
            sql.push_str(&format!(" LIMIT ?{param_idx}"));
            params.push(Box::new(limit as i64));
            param_idx += 1;
            if offset > 0 {
                sql.push_str(&format!(" OFFSET ?{param_idx}"));
                params.push(Box::new(offset as i64));
            }
        } else if offset > 0 {
            sql.push_str(&format!(" LIMIT -1 OFFSET ?{param_idx}"));
            params.push(Box::new(offset as i64));
        }
        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(param_refs.as_slice(), session_from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub(crate) fn list_export_sessions(
        &self,
        sources: Option<&[String]>,
        time_range: TimeRange,
        directory: Option<&str>,
        repo: Option<&RepoFilter>,
        thread_role: Option<ThreadRoleFilter>,
        limit: Option<usize>,
    ) -> Result<Vec<Session>> {
        self.list_indexed_sessions(
            sources,
            time_range,
            directory,
            repo,
            thread_role,
            limit,
            0,
            SessionListSort::Newest,
        )
    }
}

fn replace_parent_links_tx(
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

fn delete_session_data_tx(
    tx: &rusqlite::Transaction<'_>,
    source: &str,
    source_id: &str,
) -> Result<()> {
    let session_ids: Vec<String> = {
        let mut stmt =
            tx.prepare("SELECT id FROM sessions WHERE source = ?1 AND source_id = ?2")?;
        stmt.query_map(rusqlite::params![source, source_id], |row| row.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    for sid in &session_ids {
        tx.execute(
            "DELETE FROM message_vec WHERE message_id IN (SELECT id FROM messages WHERE session_id = ?1)",
            rusqlite::params![sid],
        )?;
    }
    tx.execute(
        "DELETE FROM sessions WHERE source = ?1 AND source_id = ?2",
        rusqlite::params![source, source_id],
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn persist_session_with_usage_and_events_tx(
    tx: &rusqlite::Transaction<'_>,
    session: &Session,
    messages: &[Message],
    usage_events: &[RawUsageEvent],
    usage_parser_version: Option<u32>,
    session_events: &[RawSessionEvent],
    event_parser_version: Option<u32>,
    topology: &SessionTopologyWrite<'_>,
) -> Result<()> {
    tx.execute(
        "INSERT INTO sessions (id, source, source_id, title, directory, repo_remote, repo_slug, repo_name, started_at, updated_at, message_count, entrypoint, custom_title, summary, duration_minutes, source_file_path, is_import, thread_role, metadata_parser_version)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
        rusqlite::params![
            session.id,
            session.source,
            session.source_id,
            session.title,
            session.directory,
            session.repo_remote,
            session.repo_slug,
            session.repo_name,
            session.started_at,
            session.updated_at,
            session.message_count,
            session.entrypoint,
            session.custom_title,
            session.summary,
            session.duration_minutes,
            session.source_file_path,
            session.is_import,
            topology.thread_role.map(|role| role.as_str()),
            topology.parser_version,
        ],
    )?;

    replace_parent_links_tx(tx, &session.id, topology.parents)?;

    {
        let mut stmt = tx.prepare(
            "INSERT INTO messages (session_id, role, content, timestamp, seq)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for msg in messages {
            stmt.execute(rusqlite::params![
                msg.session_id,
                msg.role.as_str(),
                msg.content,
                msg.timestamp,
                msg.seq,
            ])?;
        }
    }

    {
        tx.execute(
            "DELETE FROM usage_events WHERE session_id = ?1",
            rusqlite::params![session.id],
        )?;
        let created_at = Utc::now().timestamp_millis();
        let mut stmt = tx.prepare(
            "INSERT INTO usage_events (
                session_id, source, source_id, event_key, event_seq, message_seq,
                timestamp, model, provider, input_tokens, output_tokens,
                cache_read_tokens, cache_write_tokens, reasoning_tokens,
                token_source, parser_version, source_path, raw_usage_json, created_at
             )
             VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19
             )
             ON CONFLICT(session_id, event_key) DO UPDATE SET
                event_seq = excluded.event_seq,
                message_seq = excluded.message_seq,
                timestamp = excluded.timestamp,
                model = excluded.model,
                provider = excluded.provider,
                input_tokens = excluded.input_tokens,
                output_tokens = excluded.output_tokens,
                cache_read_tokens = excluded.cache_read_tokens,
                cache_write_tokens = excluded.cache_write_tokens,
                reasoning_tokens = excluded.reasoning_tokens,
                token_source = excluded.token_source,
                parser_version = excluded.parser_version,
                source_path = excluded.source_path,
                raw_usage_json = excluded.raw_usage_json",
        )?;
        for event in usage_events {
            stmt.execute(rusqlite::params![
                session.id,
                session.source,
                session.source_id,
                event.event_key,
                event.event_seq,
                event.message_seq,
                event.timestamp,
                event.model,
                event.provider,
                event.input_tokens,
                event.output_tokens,
                event.cache_read_tokens,
                event.cache_write_tokens,
                event.reasoning_tokens,
                event.token_source.as_str(),
                event.parser_version,
                event.source_path,
                event.raw_usage_json,
                created_at,
            ])?;
        }
    }

    if let Some(parser_version) = usage_parser_version {
        let synced_at = Utc::now().timestamp_millis();
        tx.execute(
            "INSERT INTO usage_session_state (
                session_id, source, source_id, parser_version,
                source_updated_at, event_count, synced_at
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(source, source_id) DO UPDATE SET
                session_id = excluded.session_id,
                parser_version = excluded.parser_version,
                source_updated_at = excluded.source_updated_at,
                event_count = excluded.event_count,
                synced_at = excluded.synced_at",
            rusqlite::params![
                session.id,
                session.source,
                session.source_id,
                parser_version,
                session.updated_at,
                usage_events.len() as u32,
                synced_at,
            ],
        )?;
    }

    replace_session_events(
        tx,
        &session.id,
        &session.source,
        &session.source_id,
        session_events,
        event_parser_version,
        session.updated_at,
    )?;

    let units_total: i64 = tx.query_row(
        "SELECT COUNT(*) FROM messages
         WHERE session_id = ?1 AND role = 'user' AND LENGTH(content) > 2",
        rusqlite::params![session.id],
        |row| row.get(0),
    )?;

    let now = Utc::now().timestamp_millis();
    if units_total == 0 {
        tx.execute(
            "INSERT INTO session_embedding_state (session_id, status, units_total, units_done, finished_at, last_error)
             VALUES (?1, 'done', 0, 0, ?2, NULL)
             ON CONFLICT(session_id) DO UPDATE SET
                status = 'done',
                units_total = 0,
                units_done = 0,
                started_at = NULL,
                finished_at = excluded.finished_at,
                last_error = NULL",
            rusqlite::params![session.id, now],
        )?;
    } else {
        tx.execute(
            "INSERT INTO session_embedding_state (session_id, status, units_total, units_done, started_at, finished_at, last_error)
             VALUES (?1, 'pending', ?2, 0, NULL, NULL, NULL)
             ON CONFLICT(session_id) DO UPDATE SET
                status = 'pending',
                units_total = excluded.units_total,
                units_done = 0,
                started_at = NULL,
                finished_at = NULL,
                last_error = NULL",
            rusqlite::params![session.id, units_total],
        )?;
    }

    Ok(())
}

#[cfg(test)]
mod topology_tests {
    use super::*;
    use crate::db::schema;
    use crate::db::store::SessionListSort;
    use crate::types::{Message, ParentLink, ParentRelation, Role, Session, ThreadRole};

    fn store() -> Store {
        schema::register_sqlite_vec();
        Store::open_in_memory().unwrap()
    }

    fn sess(source_id: &str) -> Session {
        Session {
            id: format!("local-{source_id}"),
            source: "codex".to_string(),
            source_id: source_id.to_string(),
            title: "t".to_string(),
            directory: None,
            repo_remote: None,
            repo_slug: None,
            repo_name: None,
            started_at: 0,
            updated_at: Some(10),
            message_count: 1,
            entrypoint: None,
            custom_title: None,
            summary: None,
            duration_minutes: None,
            source_file_path: None,
            is_import: false,
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
                    None,
                    None,
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
}
