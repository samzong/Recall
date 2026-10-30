use rusqlite::Connection;

use super::read_schema_version;

const MARK: &str = "INSERT INTO sync_pending_snapshots(session_id) VALUES";
const HEAD: &str = "INSERT INTO sync_pending_heads(session_id) VALUES";

fn session_triggers(table: &str, event: &str, guard: &str, rows: &[&str]) -> String {
    let body: String =
        rows.iter().map(|row| format!("{MARK} ({row}) ON CONFLICT DO NOTHING;")).collect();
    let name = event.split_whitespace().next().unwrap_or(event).to_ascii_lowercase();
    format!(
        "CREATE TRIGGER IF NOT EXISTS sync_snapshot_{table}_{name} AFTER {event} ON {table} {guard}
         BEGIN {body} END;"
    )
}

fn snapshot_sql() -> Vec<(&'static str, String)> {
    let mut sql = Vec::new();
    for table in ["messages", "usage_events", "session_events", "session_parent_links"] {
        sql.push((table, session_triggers(table, "INSERT", "", &["NEW.session_id"])));
        sql.push((table, session_triggers(table, "DELETE", "", &["OLD.session_id"])));
    }
    for table in ["usage_events", "session_events", "session_parent_links"] {
        sql.push((
            table,
            session_triggers(table, "UPDATE", "", &["OLD.session_id", "NEW.session_id"]),
        ));
    }
    sql.push((
        "messages",
        session_triggers(
            "messages",
            "UPDATE OF session_id, role, content, timestamp, seq, origin, source_kind, turn_id",
            "",
            &["OLD.session_id", "NEW.session_id"],
        ),
    ));
    sql.push(("sessions", session_triggers("sessions", "INSERT", "", &["NEW.id"])));
    sql.push((
        "sessions",
        session_triggers(
            "sessions",
            "UPDATE",
            "WHEN (NEW.id, NEW.source, NEW.source_id, NEW.title, NEW.directory, NEW.started_at,
               NEW.updated_at, NEW.message_count, NEW.entrypoint, NEW.custom_title, NEW.summary,
               NEW.duration_minutes, NEW.source_file_path, NEW.repo_remote, NEW.repo_slug,
               NEW.repo_name, NEW.thread_role)
          IS NOT (OLD.id, OLD.source, OLD.source_id, OLD.title, OLD.directory, OLD.started_at,
               OLD.updated_at, OLD.message_count, OLD.entrypoint, OLD.custom_title, OLD.summary,
               OLD.duration_minutes, OLD.source_file_path, OLD.repo_remote, OLD.repo_slug,
               OLD.repo_name, OLD.thread_role)",
            &["OLD.id", "NEW.id"],
        ),
    ));
    sql.push((
        "session_locations",
        session_triggers("session_locations", "INSERT", "", &["NEW.session_id"]),
    ));
    sql.push((
        "session_locations",
        session_triggers("session_locations", "DELETE", "", &["OLD.session_id"]),
    ));
    sql.push(("session_locations", session_triggers(
        "session_locations",
        "UPDATE",
        "WHEN (NEW.session_id, NEW.host_id, NEW.directory, NEW.source_file_path, NEW.observed_at)
          IS NOT (OLD.session_id, OLD.host_id, OLD.directory, OLD.source_file_path, OLD.observed_at)",
        &["OLD.session_id", "NEW.session_id"],
    )));
    for (event, rows) in
        [("INSERT", &["NEW"][..]), ("DELETE", &["OLD"]), ("UPDATE", &["OLD", "NEW"])]
    {
        let body: String = rows
            .iter()
            .map(|row| {
                format!(
                    "{MARK} ({row}.session_id) ON CONFLICT DO NOTHING;
                     INSERT INTO sync_pending_snapshots(session_id)
                     SELECT session_id FROM session_parent_links
                     WHERE parent_source = {row}.source AND parent_source_id = {row}.source_id ON CONFLICT DO NOTHING;"
                )
            })
            .collect();
        sql.push((
            "native_bindings",
            format!(
                "CREATE TRIGGER IF NOT EXISTS sync_snapshot_native_bindings_{name}
             AFTER {event} ON native_bindings BEGIN {body} END;",
                name = event.to_ascii_lowercase()
            ),
        ));
        let body: String = rows
            .iter()
            .map(|row| {
                format!(
                    "INSERT INTO sync_pending_snapshots(session_id)
                     SELECT session_id FROM session_parent_links WHERE parent_sync_id = {row}.sync_id ON CONFLICT DO NOTHING;"
                )
            })
            .collect();
        sql.push((
            "sync_aliases",
            format!(
                "CREATE TRIGGER IF NOT EXISTS sync_snapshot_sync_aliases_{name}
             AFTER {event} ON sync_aliases BEGIN {body} END;",
                name = event.to_ascii_lowercase()
            ),
        ));
    }
    sql.push(("hosts", "CREATE TRIGGER IF NOT EXISTS sync_snapshot_hosts_update AFTER UPDATE ON hosts
            WHEN (NEW.id, NEW.name, NEW.revision) IS NOT (OLD.id, OLD.name, OLD.revision)
            BEGIN
              INSERT INTO sync_pending_snapshots(session_id)
              SELECT session_id FROM session_locations WHERE host_id IN (OLD.id, NEW.id) ON CONFLICT DO NOTHING;
            END;".to_string()));
    sql
}

fn heads_sql() -> Vec<(&'static str, String)> {
    let head = |row: &str| format!("{HEAD} ({row}) ON CONFLICT DO NOTHING;");
    let (old, new) = (head("OLD.session_id"), head("NEW.session_id"));
    let both = format!("{old} {new}");
    let trigger = |name: &str, event: &str, table: &str, body: &str| {
        format!(
            "CREATE TRIGGER IF NOT EXISTS sync_heads_{name} AFTER {event} ON {table} BEGIN {body} END;"
        )
    };
    vec![
        (
            "sync_revisions",
            trigger(
                "sync_revisions_insert",
                "INSERT",
                "sync_revisions",
                "INSERT INTO sync_pending_heads(session_id)
                 SELECT session_id FROM sync_aliases WHERE sync_id = NEW.sync_id ON CONFLICT DO NOTHING;",
            ),
        ),
        ("sync_aliases", trigger("sync_aliases_insert", "INSERT", "sync_aliases", &new)),
        ("sync_aliases", trigger("sync_aliases_update", "UPDATE", "sync_aliases", &both)),
        ("session_sync", trigger("session_sync_insert", "INSERT", "session_sync", &new)),
        (
            "session_sync",
            trigger(
                "session_sync_update",
                "UPDATE OF session_id, current_revision",
                "session_sync",
                &both,
            ),
        ),
        ("native_bindings", trigger("native_bindings_insert", "INSERT", "native_bindings", &new)),
        ("native_bindings", trigger("native_bindings_delete", "DELETE", "native_bindings", &old)),
        ("native_bindings", trigger("native_bindings_update", "UPDATE", "native_bindings", &both)),
        (
            "sessions",
            trigger(
                "sessions_update",
                "UPDATE OF id, source, source_id",
                "sessions",
                &format!("{} {}", head("OLD.id"), head("NEW.id")),
            ),
        ),
    ]
}

pub(super) fn migrate_v19(conn: &Connection) -> anyhow::Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS sync_pending_snapshots (session_id TEXT PRIMARY KEY) WITHOUT ROWID;
         CREATE TABLE IF NOT EXISTS sync_pending_heads (session_id TEXT PRIMARY KEY) WITHOUT ROWID;
         CREATE TABLE IF NOT EXISTS sync_state (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE INDEX IF NOT EXISTS idx_session_parent_links_sync
             ON session_parent_links(parent_sync_id);",
    )?;
    super::add_column_if_missing(
        &tx,
        "ALTER TABLE session_sync ADD COLUMN head_count INTEGER NOT NULL DEFAULT 0",
    )?;
    let mut exists = tx
        .prepare("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)")?;
    for (table, sql) in snapshot_sql().into_iter().chain(heads_sql()) {
        if exists.query_row([table], |row| row.get::<_, bool>(0))? {
            tx.execute_batch(&sql)?;
        }
    }
    drop(exists);
    if read_schema_version(&tx)? >= 18 {
        tx.execute_batch("PRAGMA user_version = 19;")?;
    }
    tx.commit()?;
    Ok(())
}
