use anyhow::Result;
use rusqlite::Connection;

use crate::types::{Message, Role, Session};
use crate::utils::f32_slice_to_bytes;

pub struct Store {
    pub conn: Connection,
}

impl Store {
    pub fn open() -> Result<Self> {
        let data_dir = dirs::data_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot determine data directory"))?
            .join("recall");
        std::fs::create_dir_all(&data_dir)?;
        let db_path = data_dir.join("recall.db");
        let conn = Connection::open(&db_path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        crate::db::schema::init(&conn)?;
        Ok(Store { conn })
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        crate::db::schema::init(&conn)?;
        Ok(Store { conn })
    }

    pub fn session_meta(
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

    pub fn insert_session(&self, session: &Session) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO sessions (id, source, source_id, title, directory, started_at, updated_at, message_count, entrypoint)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                session.id,
                session.source,
                session.source_id,
                session.title,
                session.directory,
                session.started_at,
                session.updated_at,
                session.message_count,
                session.entrypoint,
            ],
        )?;
        Ok(())
    }

    pub fn insert_messages(&self, messages: &[Message]) -> Result<()> {
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

    pub fn upsert_embeddings(&self, items: &[(i64, &[f32])]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut del = tx.prepare("DELETE FROM message_vec WHERE message_id = ?1")?;
            let mut ins =
                tx.prepare("INSERT INTO message_vec (message_id, embedding) VALUES (?1, ?2)")?;
            for &(message_id, embedding) in items {
                let blob = f32_slice_to_bytes(embedding);
                del.execute(rusqlite::params![message_id])?;
                ins.execute(rusqlite::params![message_id, blob])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn embeddable_messages(&self, session_id: &str) -> Result<Vec<(i64, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, content FROM messages
             WHERE session_id = ?1 AND role = 'user' AND LENGTH(content) > 2
             ORDER BY seq",
        )?;
        let rows = stmt.query_map(rusqlite::params![session_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn delete_session_data(&self, source: &str, source_id: &str) -> Result<()> {
        let session_ids: Vec<String> = {
            let mut stmt = self
                .conn
                .prepare("SELECT id FROM sessions WHERE source = ?1 AND source_id = ?2")?;
            stmt.query_map(rusqlite::params![source, source_id], |row| row.get(0))?
                .filter_map(|r| r.ok())
                .collect()
        };
        for sid in &session_ids {
            self.conn.execute(
                "DELETE FROM message_vec WHERE message_id IN (SELECT id FROM messages WHERE session_id = ?1)",
                rusqlite::params![sid],
            )?;
        }
        self.conn.execute(
            "DELETE FROM sessions WHERE source = ?1 AND source_id = ?2",
            rusqlite::params![source, source_id],
        )?;
        Ok(())
    }

    pub fn get_messages(&self, session_id: &str) -> Result<Vec<Message>> {
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

    pub fn stats(&self) -> Result<(u64, u64)> {
        let session_count: u64 =
            self.conn.query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))?;
        let message_count: u64 =
            self.conn.query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))?;
        Ok((session_count, message_count))
    }

    pub fn list_recent_sessions(&self, limit: usize) -> Result<Vec<Session>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, source, source_id, title, directory, started_at, updated_at, message_count, entrypoint
             FROM sessions ORDER BY started_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![limit as i64], |row| {
            Ok(Session {
                id: row.get(0)?,
                source: row.get(1)?,
                source_id: row.get(2)?,
                title: row.get(3)?,
                directory: row.get(4)?,
                started_at: row.get(5)?,
                updated_at: row.get(6)?,
                message_count: row.get(7)?,
                entrypoint: row.get(8)?,
            })
        })?;
        let mut sessions = Vec::new();
        for row in rows {
            sessions.push(row?);
        }
        Ok(sessions)
    }
}
