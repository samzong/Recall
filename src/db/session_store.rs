use std::collections::{HashMap, HashSet};

use anyhow::Result;
use chrono::Utc;
use rusqlite::OptionalExtension;

use super::event_store::replace_session_events;
use super::project_store::apply_scope_filters;
use super::store::{
    IndexedSessionMeta, ParserStateMeta, SESSION_COLUMNS, SessionListSort, SessionTopologyWrite,
    Store, session_from_row,
};
use super::topology_store::replace_parent_links_tx;
use crate::db::search::{ThreadRoleFilter, TimeRange};
use crate::project_scope::ProjectScope;
use crate::types::{Message, RawSessionEvent, RawUsageEvent, Role, Session};

pub(crate) struct IndexedSourceStats {
    pub(crate) sessions: u64,
    pub(crate) messages: u64,
    pub(crate) oldest_started_at: Option<i64>,
    pub(crate) newest_started_at: Option<i64>,
}

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
    ) -> Result<HashMap<String, IndexedSessionMeta>> {
        let mut stmt = self.conn.prepare(
            "SELECT source_id, id, updated_at, message_count FROM sessions
             WHERE source = ?1 AND id IN (SELECT session_id FROM native_bindings)",
        )?;
        let rows = stmt.query_map(rusqlite::params![source], |row| {
            Ok((
                row.get::<_, String>(0)?,
                IndexedSessionMeta {
                    id: row.get(1)?,
                    updated_at: row.get(2)?,
                    message_count: row.get(3)?,
                },
            ))
        })?;
        rows.collect::<Result<HashMap<_, _>, _>>().map_err(Into::into)
    }

    pub(crate) fn indexed_source_stats(&self) -> Result<HashMap<String, IndexedSourceStats>> {
        let mut stmt = self.conn.prepare(
            "SELECT source, COUNT(*), COALESCE(SUM(message_count), 0),
                    MIN(started_at), MAX(started_at)
             FROM sessions
             GROUP BY source",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                IndexedSourceStats {
                    sessions: row.get(1)?,
                    messages: row.get(2)?,
                    oldest_started_at: row.get(3)?,
                    newest_started_at: row.get(4)?,
                },
            ))
        })?;
        rows.collect::<Result<HashMap<_, _>, _>>().map_err(Into::into)
    }

    pub(crate) fn indexed_active_sessions_after(
        &self,
        after_millis: Option<i64>,
    ) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT source, id
             FROM sessions
             WHERE (?1 IS NULL OR COALESCE(updated_at, started_at) >= ?1)",
        )?;
        let rows = stmt.query_map(rusqlite::params![after_millis], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub(crate) fn imported_source_ids(&self, source: &str) -> Result<HashSet<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT source_id FROM sessions WHERE source = ?1 AND is_import = 1
                      AND id IN (SELECT session_id FROM native_bindings)",
        )?;
        let rows = stmt.query_map(rusqlite::params![source], |row| row.get(0))?;
        rows.collect::<Result<HashSet<_>, _>>().map_err(Into::into)
    }

    pub(crate) fn clear_import_marker(&self, source: &str, source_id: &str) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE sessions SET is_import = 0 WHERE id =
             (SELECT session_id FROM native_bindings WHERE source = ?1 AND source_id = ?2)",
            rusqlite::params![source, source_id],
        )?;
        tx.execute(
            "UPDATE native_bindings SET confirmed = 1 WHERE source = ?1 AND source_id = ?2",
            rusqlite::params![source, source_id],
        )?;
        tx.commit()?;
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
              WHERE id = (SELECT session_id FROM native_bindings WHERE source = ?1 AND source_id = ?2)",
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
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
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
        bind_native_tx(&tx, session)?;
        tx.commit()?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn insert_messages(&self, messages: &[Message]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        insert_messages_tx(&tx, messages, self.trigram_message_flag)?;
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
            self.trigram_message_flag,
        )?;
        bind_native_tx(&tx, session)?;
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
        let old_id: Option<String> = tx
            .query_row(
                "SELECT session_id FROM native_bindings WHERE source = ?1 AND source_id = ?2",
                rusqlite::params![old_source, old_source_id],
                |row| row.get(0),
            )
            .optional()?;
        if old_id.as_deref().is_some_and(|id| id != session.id) {
            delete_session_data_tx(&tx, old_source, old_source_id)?;
        }
        clear_session_contents_tx(&tx, &session.id)?;
        persist_session_with_usage_and_events_tx(
            &tx,
            session,
            messages,
            usage_events,
            usage_parser_version,
            session_events,
            event_parser_version,
            topology,
            self.trigram_message_flag,
        )?;
        bind_native_tx(&tx, session)?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn metadata_state_meta_map(
        &self,
        source: &str,
    ) -> Result<HashMap<String, ParserStateMeta>> {
        self.parser_state_map(
            "SELECT source_id, metadata_parser_version, updated_at
             FROM sessions
             WHERE source = ?1 AND metadata_parser_version IS NOT NULL
               AND id IN (SELECT session_id FROM native_bindings)",
            source,
        )
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
        let rows = stmt.query_map(params.as_slice(), |row| session_from_row(row, &self.conn))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub(crate) fn get_session_by_id(&self, session_id: &str) -> Result<Option<Session>> {
        self.conn
            .query_row(
                &format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE id = ?1"),
                rusqlite::params![session_id],
                |row| session_from_row(row, &self.conn),
            )
            .optional()
            .map_err(Into::into)
    }

    pub(crate) fn get_session_by_source_id(
        &self,
        source: &str,
        source_id: &str,
    ) -> Result<Option<Session>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {SESSION_COLUMNS} FROM sessions WHERE source = ?1 AND source_id = ?2 LIMIT 2"
        ))?;
        let mut rows = stmt.query(rusqlite::params![source, source_id])?;
        let session = rows.next()?.map(|row| session_from_row(row, &self.conn)).transpose()?;
        anyhow::ensure!(
            rows.next()?.is_none(),
            "session source ID is ambiguous; use its local UUID"
        );
        Ok(session)
    }

    pub(crate) fn get_native_session(
        &self,
        source: &str,
        source_id: &str,
    ) -> Result<Option<Session>> {
        self.conn.query_row(
            &format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE id =
                (SELECT session_id FROM native_bindings WHERE source = ?1 AND source_id = ?2 AND confirmed = 1)"),
            rusqlite::params![source, source_id],
            |row| session_from_row(row, &self.conn),
        ).optional().map_err(Into::into)
    }

    pub(crate) fn has_native_binding(&self, session_id: &str) -> Result<bool> {
        self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM native_bindings WHERE session_id = ?1 AND confirmed = 1)",
            [session_id], |row| row.get(0),
        ).map_err(Into::into)
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
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }

    pub(crate) fn stats(&self) -> Result<(u64, u64)> {
        self.stats_for_scope(None, TimeRange::All)
    }

    pub(crate) fn stats_for_scope(
        &self,
        sources: Option<&[String]>,
        time_range: TimeRange,
    ) -> Result<(u64, u64)> {
        self.stats_for_search_scope(sources, time_range, &ProjectScope::Global)
    }

    pub(crate) fn stats_for_search_scope(
        &self,
        sources: Option<&[String]>,
        time_range: TimeRange,
        scope: &ProjectScope,
    ) -> Result<(u64, u64)> {
        let mut sql = String::from(
            "SELECT COUNT(*), COALESCE(SUM(s.message_count), 0) FROM sessions s WHERE 1=1",
        );
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let mut param_idx = 1;
        apply_scope_filters(&mut sql, &mut params, &mut param_idx, sources, time_range, scope);
        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();
        self.conn
            .query_row(&sql, param_refs.as_slice(), |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(Into::into)
    }

    #[cfg(test)]
    pub(crate) fn list_recent_sessions(&self, limit: usize) -> Result<Vec<Session>> {
        self.list_recent_sessions_for_search_scope(
            None,
            TimeRange::All,
            &ProjectScope::Global,
            None,
            limit,
        )
    }

    pub(crate) fn list_recent_sessions_for_search_scope(
        &self,
        sources: Option<&[String]>,
        time_range: TimeRange,
        scope: &ProjectScope,
        excluded_session_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Session>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut sql = format!(
            "SELECT {SESSION_COLUMNS}
             FROM sessions s
             WHERE 1=1"
        );
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let mut param_idx = 1;
        apply_scope_filters(&mut sql, &mut params, &mut param_idx, sources, time_range, scope);
        if let Some(excluded_session_id) = excluded_session_id {
            sql.push_str(&format!(" AND s.id != ?{param_idx}"));
            params.push(Box::new(excluded_session_id.to_string()));
            param_idx += 1;
        }
        sql.push_str(&format!(
            " ORDER BY COALESCE(updated_at, started_at) DESC, started_at DESC, source ASC, source_id ASC LIMIT ?{param_idx} OFFSET ?{}",
            param_idx + 1
        ));
        let mut stmt = self.conn.prepare(&sql)?;
        let page_size = limit as i64;
        let mut offset = 0_i64;
        let mut sessions = Vec::with_capacity(limit);
        loop {
            let mut param_refs: Vec<&dyn rusqlite::types::ToSql> =
                params.iter().map(|param| param.as_ref()).collect();
            param_refs.push(&page_size);
            param_refs.push(&offset);
            let rows =
                stmt.query_map(param_refs.as_slice(), |row| session_from_row(row, &self.conn))?;
            let page = rows.collect::<std::result::Result<Vec<_>, _>>()?;
            let fetched = page.len();
            offset += fetched as i64;
            let mut page = page.into_iter();
            while !page.as_slice().is_empty() {
                let needed = limit - sessions.len();
                sessions.extend(page.by_ref().take(needed));
                self.retain_visible_subagents(&mut sessions)?;
                if sessions.len() == limit {
                    return Ok(sessions);
                }
            }
            if fetched < limit {
                return Ok(sessions);
            }
        }
    }

    fn retain_visible_subagents(&self, sessions: &mut Vec<Session>) -> Result<()> {
        if sessions.is_empty() {
            return Ok(());
        }
        let ids: Vec<String> = sessions.iter().map(|s| s.id.clone()).collect();
        let child_ph: Vec<String> = (1..=ids.len()).map(|i| format!("?{i}")).collect();
        let parent_ph: Vec<String> =
            ((ids.len() + 1)..=(2 * ids.len())).map(|i| format!("?{i}")).collect();
        let hide_sql = format!(
            "SELECT s.id
             FROM sessions s
             JOIN session_parent_links l ON l.session_id = s.id AND l.relation = 'spawn'
             JOIN sessions p ON p.source = l.parent_source AND p.source_id = l.parent_source_id
             WHERE s.thread_role = 'subagent'
               AND ((l.parent_sync_id IS NOT NULL AND EXISTS(
                   SELECT 1 FROM sync_aliases a WHERE a.sync_id = l.parent_sync_id AND a.session_id = p.id))
                 OR (l.parent_sync_id IS NULL
                   AND s.id IN (SELECT session_id FROM native_bindings)
                   AND p.id IN (SELECT session_id FROM native_bindings)))
               AND s.id IN ({})
               AND p.id IN ({})",
            child_ph.join(", "),
            parent_ph.join(", ")
        );
        let params: Vec<&dyn rusqlite::types::ToSql> = ids
            .iter()
            .map(|id| id as &dyn rusqlite::types::ToSql)
            .chain(ids.iter().map(|id| id as &dyn rusqlite::types::ToSql))
            .collect();
        let mut stmt = self.conn.prepare(&hide_sql)?;
        let hidden: HashSet<String> = stmt
            .query_map(params.as_slice(), |row| row.get::<_, String>(0))?
            .collect::<Result<_, _>>()?;
        sessions.retain(|session| !hidden.contains(&session.id));
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn list_indexed_sessions(
        &self,
        sources: Option<&[String]>,
        time_range: TimeRange,
        scope: &ProjectScope,
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
        apply_scope_filters(&mut sql, &mut params, &mut param_idx, sources, time_range, scope);
        if let Some(thread_role) = thread_role {
            sql.push_str(thread_role.sql_predicate());
        }
        let order_by = match sort {
            SessionListSort::Newest => "s.started_at DESC, source ASC, source_id ASC, s.id ASC",
            SessionListSort::Oldest => "s.started_at ASC, source ASC, source_id ASC, s.id ASC",
            SessionListSort::Updated => {
                "COALESCE(s.updated_at, s.started_at) DESC, s.started_at DESC, source ASC, source_id ASC, s.id ASC"
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
        let rows =
            stmt.query_map(param_refs.as_slice(), |row| session_from_row(row, &self.conn))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub(crate) fn list_export_sessions(
        &self,
        sources: Option<&[String]>,
        time_range: TimeRange,
        scope: &ProjectScope,
        thread_role: Option<ThreadRoleFilter>,
        limit: Option<usize>,
    ) -> Result<Vec<Session>> {
        self.list_indexed_sessions(
            sources,
            time_range,
            scope,
            thread_role,
            limit,
            0,
            SessionListSort::Newest,
        )
    }
}

fn delete_session_data_tx(
    tx: &rusqlite::Transaction<'_>,
    source: &str,
    source_id: &str,
) -> Result<()> {
    let session_ids: Vec<String> = {
        let mut stmt = tx.prepare(
            "SELECT session_id FROM native_bindings WHERE source = ?1 AND source_id = ?2",
        )?;
        stmt.query_map(rusqlite::params![source, source_id], |row| row.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    for sid in session_ids {
        tx.execute("DELETE FROM native_bindings WHERE session_id = ?1", [&sid])?;
        let replicated: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM session_sync WHERE session_id = ?1)",
            [&sid],
            |row| row.get(0),
        )?;
        if replicated {
            tx.execute("UPDATE sessions SET is_import = 1 WHERE id = ?1", [&sid])?;
            continue;
        }
        tx.execute(
            "DELETE FROM message_vec WHERE message_id IN (SELECT id FROM messages WHERE session_id = ?1)",
            rusqlite::params![sid],
        )?;
        tx.execute("DELETE FROM sessions WHERE id = ?1", [&sid])?;
    }
    Ok(())
}

fn bind_native_tx(tx: &rusqlite::Transaction<'_>, session: &Session) -> Result<()> {
    tx.execute(
        "INSERT INTO native_bindings(source, source_id, session_id, confirmed)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(source, source_id) DO UPDATE SET
             confirmed = MAX(native_bindings.confirmed, excluded.confirmed)
         WHERE native_bindings.session_id = excluded.session_id",
        rusqlite::params![session.source, session.source_id, session.id, !session.is_import],
    )?;
    let bound: String = tx.query_row(
        "SELECT session_id FROM native_bindings WHERE source = ?1 AND source_id = ?2",
        rusqlite::params![session.source, session.source_id],
        |row| row.get(0),
    )?;
    anyhow::ensure!(bound == session.id, "native session is already bound to another local UUID");
    Ok(())
}

pub(super) fn clear_session_contents_tx(
    tx: &rusqlite::Transaction<'_>,
    session_id: &str,
) -> Result<()> {
    tx.execute(
        "DELETE FROM message_vec WHERE message_id IN (SELECT id FROM messages WHERE session_id = ?1)",
        [session_id],
    )?;
    for table in [
        "messages",
        "usage_events",
        "usage_session_state",
        "session_events",
        "event_session_state",
        "session_embedding_state",
        "session_parent_links",
    ] {
        tx.execute(&format!("DELETE FROM {table} WHERE session_id = ?1"), [session_id])?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn persist_session_with_usage_and_events_tx(
    tx: &rusqlite::Transaction<'_>,
    session: &Session,
    messages: &[Message],
    usage_events: &[RawUsageEvent],
    usage_parser_version: Option<u32>,
    session_events: &[RawSessionEvent],
    event_parser_version: Option<u32>,
    topology: &SessionTopologyWrite<'_>,
    trigram_message_flag: bool,
) -> Result<()> {
    tx.execute(
        "INSERT INTO sessions (id, source, source_id, title, directory, repo_remote, repo_slug, repo_name, started_at, updated_at, message_count, entrypoint, custom_title, summary, duration_minutes, source_file_path, is_import, thread_role, metadata_parser_version)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)
         ON CONFLICT(id) DO UPDATE SET
             source = excluded.source, source_id = excluded.source_id, title = excluded.title,
             directory = excluded.directory, repo_remote = excluded.repo_remote,
             repo_slug = excluded.repo_slug, repo_name = excluded.repo_name,
             started_at = excluded.started_at, updated_at = excluded.updated_at,
             message_count = excluded.message_count, entrypoint = excluded.entrypoint,
             custom_title = excluded.custom_title, summary = excluded.summary,
             duration_minutes = excluded.duration_minutes, source_file_path = excluded.source_file_path,
             is_import = excluded.is_import, thread_role = excluded.thread_role,
             metadata_parser_version = excluded.metadata_parser_version",
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

    insert_messages_tx(tx, messages, trigram_message_flag)?;

    super::usage_store::replace_usage_events(
        tx,
        &session.id,
        &session.source,
        &session.source_id,
        usage_events,
        usage_parser_version,
        session.updated_at,
        super::usage_store::UsageConflict::Update,
    )?;

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

fn insert_messages_tx(
    tx: &rusqlite::Transaction<'_>,
    messages: &[Message],
    trigram_message_flag: bool,
) -> Result<()> {
    let sql = if trigram_message_flag {
        "INSERT INTO messages (session_id, role, content, timestamp, seq, trigram_indexed)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
    } else {
        "INSERT INTO messages (session_id, role, content, timestamp, seq)
         VALUES (?1, ?2, ?3, ?4, ?5)"
    };
    let mut stmt = tx.prepare(sql)?;
    let parameter_count = stmt.parameter_count();
    for msg in messages {
        let indexed = trigram_message_flag && crate::utils::text_needs_trigram(&msg.content);
        let parameters = rusqlite::params![
            msg.session_id,
            msg.role.as_str(),
            msg.content,
            msg.timestamp,
            msg.seq,
            indexed,
        ];
        stmt.execute(&parameters[..parameter_count])?;
    }
    Ok(())
}

#[cfg(test)]
mod source_stats_tests {
    use super::*;
    use crate::db::schema;

    #[test]
    fn indexed_source_stats_use_persisted_session_counts() {
        schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        store
            .conn
            .execute_batch(
                "INSERT INTO sessions
                    (id, source, source_id, title, started_at, message_count)
                 VALUES
                    ('c1', 'codex', 'raw-c1', 'one', 20, 2),
                    ('c2', 'codex', 'raw-c2', 'two', 10, 3),
                    ('o1', 'opencode', 'raw-o1', 'three', 30, 4);",
            )
            .unwrap();

        let stats = store.indexed_source_stats().unwrap();

        let codex = &stats["codex"];
        assert_eq!(codex.sessions, 2);
        assert_eq!(codex.messages, 5);
        assert_eq!(codex.oldest_started_at, Some(10));
        assert_eq!(codex.newest_started_at, Some(20));

        let opencode = &stats["opencode"];
        assert_eq!(opencode.sessions, 1);
        assert_eq!(opencode.messages, 4);
    }
}

#[cfg(test)]
mod search_scope_stats_tests {
    use super::*;
    use crate::db::schema;
    use crate::types::{Message, Role, Session};

    fn store() -> Store {
        schema::register_sqlite_vec();
        Store::open_in_memory().unwrap()
    }

    fn session(
        id: &str,
        source: &str,
        started_at: i64,
        message_count: u32,
        directory: Option<&str>,
    ) -> Session {
        Session {
            source: source.to_string(),
            source_id: format!("raw-{id}"),
            title: id.to_string(),
            directory: directory.map(str::to_string),
            started_at,
            updated_at: Some(started_at),
            message_count,
            ..crate::types::test_support::session(id)
        }
    }

    fn messages(session_id: &str, count: u32) -> Vec<Message> {
        (0..count)
            .map(|seq| Message {
                session_id: session_id.to_string(),
                role: Role::User,
                content: format!("message {seq}"),
                timestamp: Some(i64::from(seq)),
                seq,
            })
            .collect()
    }

    #[test]
    fn stats_for_search_scope_sum_session_counts_without_reading_messages() {
        let store = store();
        store.insert_session(&session("a", "codex", 10, 2, Some("/repo/a"))).unwrap();
        store.insert_session(&session("b", "codex", 20, 3, Some("/repo/b"))).unwrap();
        store.insert_session(&session("c", "opencode", 30, 4, Some("/other"))).unwrap();

        assert_eq!(
            store.stats_for_search_scope(None, TimeRange::All, &ProjectScope::Global).unwrap(),
            (3, 9)
        );
        assert_eq!(
            store
                .stats_for_search_scope(
                    Some(&["codex".to_string()]),
                    TimeRange::All,
                    &ProjectScope::Global
                )
                .unwrap(),
            (2, 5)
        );
        assert_eq!(
            store
                .stats_for_search_scope(
                    None,
                    TimeRange::All,
                    &ProjectScope::Directory("/repo/a".into())
                )
                .unwrap(),
            (1, 2)
        );
    }

    #[test]
    fn persist_keeps_stored_message_count_aligned_with_message_rows() {
        let store = store();
        let first = session("a", "codex", 10, 2, None);
        let second = session("b", "codex", 20, 3, None);
        store.persist_session_with_usage(&first, &messages(&first.id, 2), &[], None).unwrap();
        store.persist_session_with_usage(&second, &messages(&second.id, 3), &[], None).unwrap();

        let stored: u32 = store
            .conn
            .query_row("SELECT message_count FROM sessions WHERE id = ?1", ["a"], |row| row.get(0))
            .unwrap();
        let rows: u32 = store
            .conn
            .query_row("SELECT COUNT(*) FROM messages WHERE session_id = ?1", ["a"], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(stored, 2);
        assert_eq!(rows, 2);

        assert_eq!(
            store.stats_for_search_scope(None, TimeRange::All, &ProjectScope::Global).unwrap(),
            (2, 5)
        );
    }
}
