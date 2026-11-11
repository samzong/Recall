use rusqlite::{Connection, Transaction, TransactionBehavior};

pub(super) fn migrate_v20(conn: &Connection) -> anyhow::Result<()> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    if super::read_schema_version(&tx)? >= 20 {
        return Ok(());
    }
    let has_flag: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('messages') WHERE name='trigram_indexed')",
        [],
        |row| row.get(0),
    )?;
    if !has_flag {
        tx.execute_batch(
            "ALTER TABLE messages ADD COLUMN trigram_indexed INTEGER CHECK (trigram_indexed IN (0, 1));",
        )?;
    }
    tx.execute_batch(
        "DROP TRIGGER IF EXISTS messages_trigram_ai;
         DROP TRIGGER IF EXISTS messages_trigram_ad;",
    )?;
    let has_index: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='messages_fts_trigram' AND type='table')",
        [],
        |row| row.get(0),
    )?;
    if has_index {
        tx.execute_batch(
            "ALTER TABLE messages_fts_trigram RENAME TO messages_fts_trigram_legacy;",
        )?;
    }
    tx.execute_batch(
        "CREATE VIRTUAL TABLE messages_fts_trigram USING fts5(
            content, content=messages, content_rowid=id, tokenize='trigram remove_diacritics 1'
         );
         CREATE TRIGGER messages_trigram_ai AFTER INSERT ON messages
         WHEN new.trigram_indexed = 1 OR new.trigram_indexed IS NULL BEGIN
            INSERT INTO messages_fts_trigram(rowid, content) VALUES (new.id, new.content);
         END;
         CREATE TRIGGER messages_trigram_ad AFTER DELETE ON messages
         WHEN (old.trigram_indexed = 1 OR old.trigram_indexed IS NULL)
           AND EXISTS (
             SELECT 1 FROM background_job_state WHERE job='trigram'
               AND (phase IN ('ready', 'done') OR old.id > json_extract(detail, '$.end')
                    OR old.id <= json_extract(detail, '$.cursor'))
           ) BEGIN
            INSERT INTO messages_fts_trigram(messages_fts_trigram, rowid, content)
            VALUES ('delete', old.id, old.content);
         END;",
    )?;
    let end: i64 =
        tx.query_row("SELECT COALESCE(MAX(id), ?1) FROM messages", [i64::MIN], |row| row.get(0))?;
    let phase = if end != i64::MIN {
        "backfill"
    } else if has_index {
        "ready"
    } else {
        "done"
    };
    let detail = serde_json::json!({"cursor": i64::MIN, "end": end}).to_string();
    tx.execute(
        "INSERT INTO background_job_state(job, phase, detail, updated_at)
         VALUES ('trigram', ?1, ?2, unixepoch() * 1000)
         ON CONFLICT(job) DO UPDATE SET phase=excluded.phase, detail=excluded.detail,
                                       updated_at=excluded.updated_at",
        rusqlite::params![phase, detail],
    )?;
    tx.execute_batch("PRAGMA user_version = 20;")?;
    tx.commit()?;
    Ok(())
}

impl crate::db::store::Store {
    pub(crate) fn trigram_backfill_pending(&self) -> anyhow::Result<bool> {
        Ok(self.conn.query_row(
            "SELECT phase != 'done' FROM background_job_state WHERE job='trigram'",
            [],
            |row| row.get(0),
        )?)
    }

    pub(crate) fn backfill_trigram_batch(&self) -> anyhow::Result<bool> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let (phase, cursor, end): (String, i64, i64) = tx.query_row(
            "SELECT phase, json_extract(detail, '$.cursor'), json_extract(detail, '$.end')
                 FROM background_job_state WHERE job='trigram'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        if phase != "backfill" {
            anyhow::ensure!(phase == "ready" || phase == "done", "Invalid trigram backfill phase");
            return Ok(false);
        }
        let rows: Vec<(i64, String)> = {
            let mut stmt = tx.prepare(
                "SELECT id, content FROM messages WHERE id > ?1 AND id <= ?2 ORDER BY id LIMIT 128",
            )?;
            stmt.query_map([cursor, end], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<Result<_, _>>()?
        };
        if let Some((last, _)) = rows.last() {
            {
                let mut flag = tx.prepare("UPDATE messages SET trigram_indexed=?2 WHERE id=?1 AND trigram_indexed IS NOT ?2")?;
                let mut insert =
                    tx.prepare("INSERT INTO messages_fts_trigram(rowid, content) VALUES (?1, ?2)")?;
                for (id, content) in &rows {
                    let needs = crate::utils::text_needs_trigram(content);
                    flag.execute(rusqlite::params![id, needs])?;
                    if needs {
                        insert.execute(rusqlite::params![id, content])?;
                    }
                }
            }
            tx.execute(
                "UPDATE background_job_state SET detail=json_set(detail, '$.cursor', ?1),
                    updated_at=unixepoch() * 1000 WHERE job='trigram'",
                [last],
            )?;
        } else {
            tx.execute_batch(
                "UPDATE background_job_state SET phase='ready', updated_at=unixepoch() * 1000
                 WHERE job='trigram';",
            )?;
        }
        tx.commit()?;
        Ok(!rows.is_empty())
    }

    pub(crate) fn finish_trigram_backfill(&self) -> anyhow::Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let phase: String = tx.query_row(
            "SELECT phase FROM background_job_state WHERE job='trigram'",
            [],
            |row| row.get(0),
        )?;
        if phase == "ready" {
            tx.execute_batch(
                "DROP TABLE IF EXISTS messages_fts_trigram_legacy;
                 UPDATE background_job_state SET phase='done', updated_at=unixepoch() * 1000
                 WHERE job='trigram';",
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}
