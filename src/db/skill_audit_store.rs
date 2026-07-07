use anyhow::Result;

use super::store::{SkillAuditEventRow, Store};
use crate::db::search::TimeRange;

impl Store {
    pub(crate) fn list_skill_audit_events(
        &self,
        sources: Option<&[String]>,
        time_range: TimeRange,
    ) -> Result<Vec<SkillAuditEventRow>> {
        let mut sql = String::from(
            "SELECT e.session_id, e.source,
                    COALESCE(e.timestamp, s.updated_at, s.started_at) AS timestamp,
                    e.name, e.target, e.attrs_json
             FROM session_events e
             JOIN sessions s ON s.id = e.session_id
             WHERE (LOWER(e.name) IN ('skill', 'use_skill') OR e.target LIKE '%/skills/%/SKILL.md%'
                    OR e.target LIKE '%/skills/%/references/%')",
        );

        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let mut param_idx = 1;

        if let Some(cutoff) = time_range.millis_ago() {
            sql.push_str(&format!(
                " AND COALESCE(e.timestamp, s.updated_at, s.started_at) >= ?{param_idx}"
            ));
            params.push(Box::new(cutoff));
            param_idx += 1;
        }

        if let Some(source_ids) = sources
            && !source_ids.is_empty()
        {
            sql.push_str(" AND e.source IN (");
            for (i, source_id) in source_ids.iter().enumerate() {
                if i > 0 {
                    sql.push_str(", ");
                }
                sql.push_str(&format!("?{param_idx}"));
                params.push(Box::new(source_id.clone()));
                param_idx += 1;
            }
            sql.push(')');
        }

        sql.push_str(
            " ORDER BY COALESCE(e.timestamp, s.updated_at, s.started_at) ASC, e.session_id ASC, e.event_seq ASC",
        );

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(param_refs.as_slice(), |row| {
            Ok(SkillAuditEventRow {
                session_id: row.get(0)?,
                source: row.get(1)?,
                timestamp: row.get(2)?,
                name: row.get(3)?,
                target: row.get(4)?,
                attrs_json: row.get(5)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}
