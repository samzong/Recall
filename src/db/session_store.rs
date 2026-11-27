mod write;
pub(super) use write::{clear_session_contents_tx, persist_session_with_usage_and_events_tx};

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use rusqlite::OptionalExtension;

use super::project_store::apply_scope_filters;
use super::store::{
    IndexedSessionMeta, ParserStateMeta, SESSION_COLUMNS, SessionListSort, Store, session_from_row,
};
use crate::db::search::{ThreadRoleFilter, TimeRange};
use crate::project_scope::ProjectScope;
use crate::types::{Message, Role, Session};

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
            "SELECT source_id, id, updated_at, message_count, directory, custom_title, started_at FROM sessions
             WHERE source = ?1 AND id IN (SELECT session_id FROM native_bindings)",
        )?;
        let rows = stmt.query_map(rusqlite::params![source], |row| {
            Ok((
                row.get::<_, String>(0)?,
                IndexedSessionMeta {
                    id: row.get(1)?,
                    updated_at: row.get(2)?,
                    message_count: row.get(3)?,
                    directory: row.get(4)?,
                    custom_title: row.get(5)?,
                    started_at: row.get(6)?,
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

#[cfg(test)]
mod tests;
