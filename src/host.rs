use anyhow::{Result, ensure};
use rmcp::schemars;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub(crate) struct Host {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) revision: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
pub(crate) struct Location {
    pub(crate) host: Host,
    pub(crate) directory: Option<String>,
    pub(crate) source_file_path: Option<String>,
    pub(crate) observed_at: i64,
}

impl Host {
    pub(crate) fn validate(&self) -> Result<()> {
        uuid::Uuid::parse_str(&self.id)?;
        ensure!(
            !self.name.trim().is_empty()
                && self.name.chars().count() <= 80
                && !self.name.chars().any(char::is_control),
            "invalid host name"
        );
        ensure!((1..=i64::MAX as u64).contains(&self.revision), "invalid host revision");
        Ok(())
    }

    pub(crate) fn observe(
        &self,
        conn: &rusqlite::Connection,
        source: &str,
        source_id: &str,
    ) -> Result<()> {
        use rusqlite::OptionalExtension;
        let native: Option<(String, Option<String>, Option<String>)> = conn
            .query_row(
                "SELECT s.id, s.directory, s.source_file_path FROM sessions s
             JOIN native_bindings n ON n.session_id = s.id
             WHERE n.source = ?1 AND n.source_id = ?2 AND n.confirmed = 1",
                rusqlite::params![source, source_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((session_id, directory, source_file_path)) = native {
            let observed_at = locations(conn, &session_id)?
                .into_iter()
                .find(|location| {
                    location.host.id == self.id
                        && location.directory == directory
                        && location.source_file_path == source_file_path
                })
                .map_or_else(
                    || chrono::Utc::now().timestamp_millis(),
                    |location| location.observed_at,
                );
            let tx = conn.unchecked_transaction()?;
            merge_location(
                &tx,
                &session_id,
                &Location { host: self.clone(), directory, source_file_path, observed_at },
            )?;
            tx.commit()?;
        }
        Ok(())
    }
}

pub(crate) fn merge_location(
    conn: &rusqlite::Connection,
    session_id: &str,
    location: &Location,
) -> Result<()> {
    location.host.validate()?;
    conn.execute(
        "INSERT INTO hosts(id, name, revision) VALUES (?1, ?2, ?3)
         ON CONFLICT(id) DO UPDATE SET name = excluded.name, revision = excluded.revision
         WHERE excluded.revision > hosts.revision",
        rusqlite::params![location.host.id, location.host.name, location.host.revision],
    )?;
    conn.execute(
        "INSERT INTO session_locations(session_id, host_id, directory, source_file_path, observed_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(session_id, host_id) DO UPDATE SET directory = excluded.directory,
             source_file_path = excluded.source_file_path, observed_at = excluded.observed_at
         WHERE excluded.observed_at >= session_locations.observed_at",
        rusqlite::params![session_id, location.host.id, location.directory, location.source_file_path, location.observed_at],
    )?;
    Ok(())
}

pub(crate) fn label(locations: &[Location]) -> String {
    if locations.is_empty() {
        return "unknown".to_string();
    }
    locations
        .iter()
        .map(|location| {
            let same_name = locations.iter().any(|other| {
                other.host.id != location.host.id && other.host.name == location.host.name
            });
            if same_name {
                format!(
                    "{} ({})",
                    location.host.name,
                    location.host.id.chars().take(8).collect::<String>()
                )
            } else {
                location.host.name.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

pub(crate) fn locations(
    conn: &rusqlite::Connection,
    session_id: &str,
) -> rusqlite::Result<Vec<Location>> {
    let mut stmt = conn.prepare(
        "SELECT h.id, h.name, h.revision, l.directory, l.source_file_path, l.observed_at
         FROM session_locations l JOIN hosts h ON h.id = l.host_id
         WHERE l.session_id = ?1 ORDER BY h.name, h.id",
    )?;
    stmt.query_map([session_id], |row| {
        Ok(Location {
            host: Host { id: row.get(0)?, name: row.get(1)?, revision: row.get(2)? },
            directory: row.get(3)?,
            source_file_path: row.get(4)?,
            observed_at: row.get(5)?,
        })
    })?
    .collect()
}
