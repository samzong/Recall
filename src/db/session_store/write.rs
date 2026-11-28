use anyhow::Result;
use chrono::Utc;
use rusqlite::OptionalExtension;

use super::super::event_store::replace_session_events;
use super::super::store::{SessionTopologyWrite, Store};
use super::super::topology_store::replace_parent_links_tx;
use crate::db::{skill_audit_store, usage_store};
use crate::types::{ExcludedCodexUsage, Message, RawSessionEvent, RawUsageEvent, Session};

impl Store {
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

    pub(crate) fn update_session_directory_and_repo(
        &self,
        source: &str,
        source_id: &str,
        directory: &str,
        repo_remote: Option<&str>,
        repo_slug: Option<&str>,
        repo_name: Option<&str>,
    ) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE sessions
                SET directory = ?3,
                    repo_remote = ?4,
                    repo_slug = ?5,
                    repo_name = ?6
              WHERE id = (SELECT session_id FROM native_bindings WHERE source = ?1 AND source_id = ?2)",
            rusqlite::params![source, source_id, directory, repo_remote, repo_slug, repo_name],
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
        insert_messages_tx(&tx, messages)?;
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
        bind_native_tx(&tx, session)?;
        usage_store::replace_codex_excluded_usage_tx(
            &tx,
            &session.source,
            &session.source_id,
            usage_events,
            &[],
            usage_parser_version,
        )?;
        usage_store::flush_dirty_usage_tx(&tx)?;
        skill_audit_store::flush_dirty_skill_tx(&tx)?;
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

    #[cfg(test)]
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
        self.replace_session_with_usage_events_and_codex_exclusions(
            old_source,
            old_source_id,
            session,
            messages,
            usage_events,
            &[],
            usage_parser_version,
            session_events,
            event_parser_version,
            topology,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn replace_session_with_usage_events_and_codex_exclusions(
        &self,
        old_source: &str,
        old_source_id: &str,
        session: &Session,
        messages: &[Message],
        usage_events: &[RawUsageEvent],
        excluded_codex_usage: &[ExcludedCodexUsage],
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
        tx.execute(
            "DELETE FROM message_vec WHERE message_id IN (SELECT id FROM messages WHERE session_id = ?1)",
            [&session.id],
        )?;
        for table in [
            "messages",
            "session_events",
            "event_session_state",
            "session_embedding_state",
            "session_parent_links",
        ] {
            tx.execute(&format!("DELETE FROM {table} WHERE session_id = ?1"), [&session.id])?;
        }
        if usage_parser_version.is_some() || !usage_events.is_empty() {
            for table in ["usage_events", "usage_session_state"] {
                tx.execute(&format!("DELETE FROM {table} WHERE session_id = ?1"), [&session.id])?;
            }
        }
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
        bind_native_tx(&tx, session)?;
        usage_store::replace_codex_excluded_usage_tx(
            &tx,
            &session.source,
            &session.source_id,
            usage_events,
            excluded_codex_usage,
            usage_parser_version,
        )?;
        usage_store::flush_dirty_usage_tx(&tx)?;
        skill_audit_store::flush_dirty_skill_tx(&tx)?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn delete_session_data(&self, source: &str, source_id: &str) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        delete_session_data_tx(&tx, source, source_id)?;
        usage_store::flush_dirty_usage_tx(&tx)?;
        skill_audit_store::flush_dirty_skill_tx(&tx)?;
        tx.commit()?;
        Ok(())
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

pub(in super::super) fn clear_session_contents_tx(
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
pub(in super::super) fn persist_session_with_usage_and_events_tx(
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

    insert_messages_tx(tx, messages)?;

    if usage_parser_version.is_some() || !usage_events.is_empty() {
        usage_store::replace_usage_events(
            tx,
            &session.id,
            &session.source,
            &session.source_id,
            usage_events,
            usage_parser_version,
            session.updated_at,
            usage_store::UsageConflict::Update,
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

fn insert_messages_tx(tx: &rusqlite::Transaction<'_>, messages: &[Message]) -> Result<()> {
    let mut stmt = tx.prepare(
        "INSERT INTO messages (session_id, role, content, timestamp, seq, trigram_indexed)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    for msg in messages {
        let indexed = crate::utils::text_needs_trigram(&msg.content);
        let parameters = rusqlite::params![
            msg.session_id,
            msg.role.as_str(),
            msg.content,
            msg.timestamp,
            msg.seq,
            indexed,
        ];
        stmt.execute(parameters)?;
    }
    Ok(())
}
