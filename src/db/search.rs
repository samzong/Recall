use std::collections::HashMap;

use rusqlite::Connection;

use crate::types::{MatchSource, SearchResult, Session};
use crate::utils::f32_slice_to_bytes;

pub struct SearchEngine<'a> {
    conn: &'a Connection,
}

pub struct SearchFilters {
    pub sources: Option<Vec<String>>,
    pub time_range: TimeRange,
    pub directory: Option<String>,
}

#[derive(Clone, Copy)]
pub enum TimeRange {
    Today,
    Week,
    Month,
    All,
}

impl TimeRange {
    pub fn millis_ago(&self) -> Option<i64> {
        let now = chrono::Utc::now().timestamp_millis();
        match self {
            TimeRange::Today => Some(now - 24 * 3600 * 1000),
            TimeRange::Week => Some(now - 7 * 24 * 3600 * 1000),
            TimeRange::Month => Some(now - 30 * 24 * 3600 * 1000),
            TimeRange::All => None,
        }
    }
}

struct Hit {
    session_id: String,
    snippet: Option<String>,
}

impl<'a> SearchEngine<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    pub fn hybrid_search(
        &self,
        query: &str,
        embedding: Option<&[f32]>,
        filters: &SearchFilters,
        limit: usize,
    ) -> anyhow::Result<Vec<SearchResult>> {
        let fetch_size = limit * 3;
        let fts_hits = self.fts_search(query, filters, fetch_size)?;
        let vec_hits = match embedding {
            Some(e) => self.vec_search(e, filters, fetch_size)?,
            None => vec![],
        };
        let merged = rrf_merge(&fts_hits, &vec_hits, 60);

        let session_ids: Vec<&str> =
            merged.iter().take(limit).map(|(id, _, _)| id.as_str()).collect();

        let sessions = self.load_sessions(&session_ids)?;

        let mut results = Vec::new();
        for (session_id, _score, match_source) in merged.into_iter().take(limit) {
            if let Some(session) = sessions.get(&session_id) {
                let snippet = fts_hits
                    .iter()
                    .find(|h| h.session_id == session_id)
                    .and_then(|h| h.snippet.clone());
                results.push(SearchResult { session: session.clone(), match_source, snippet });
            }
        }

        Ok(results)
    }

    fn fts_search(
        &self,
        query: &str,
        filters: &SearchFilters,
        limit: usize,
    ) -> anyhow::Result<Vec<Hit>> {
        let escaped = fts5_escape(query);
        if escaped.is_empty() {
            return Ok(vec![]);
        }

        let mut sql = String::from(
            "SELECT m.session_id, SUBSTR(m.content, 1, 200) AS snip,
                    SUM(messages_fts.rank) AS total_rank
             FROM messages_fts
             JOIN messages m ON m.id = messages_fts.rowid
             JOIN sessions s ON s.id = m.session_id
             WHERE messages_fts MATCH ?1",
        );

        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(escaped)];
        let mut param_idx = 2;
        apply_filters(&mut sql, &mut params, &mut param_idx, filters);

        sql.push_str(&format!(" GROUP BY m.session_id ORDER BY total_rank LIMIT {limit}"));

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(param_refs.as_slice(), |row| {
            Ok(Hit { session_id: row.get(0)?, snippet: row.get(1)? })
        })?;

        let mut hits = Vec::new();
        for row in rows {
            hits.push(row?);
        }
        Ok(hits)
    }

    fn vec_search(
        &self,
        embedding: &[f32],
        filters: &SearchFilters,
        limit: usize,
    ) -> anyhow::Result<Vec<Hit>> {
        let blob = f32_slice_to_bytes(embedding);
        let fetch_k = (limit * 5) as i64;

        let mut sql = String::from(
            "SELECT m.session_id, MIN(mv.distance) AS best_distance
             FROM message_vec mv
             JOIN messages m ON m.id = mv.message_id
             JOIN sessions s ON s.id = m.session_id
             WHERE mv.embedding MATCH ?1
               AND k = ?2",
        );

        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> =
            vec![Box::new(blob), Box::new(fetch_k)];
        let mut param_idx = 3;
        apply_filters(&mut sql, &mut params, &mut param_idx, filters);

        sql.push_str(" GROUP BY m.session_id ORDER BY best_distance");

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(param_refs.as_slice(), |row| {
            Ok(Hit { session_id: row.get(0)?, snippet: None })
        })?;

        let mut hits = Vec::new();
        for row in rows {
            hits.push(row?);
        }
        Ok(hits)
    }

    fn load_sessions(&self, ids: &[&str]) -> anyhow::Result<HashMap<String, Session>> {
        let mut map = HashMap::new();
        if ids.is_empty() {
            return Ok(map);
        }

        let placeholders: Vec<String> = (1..=ids.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "SELECT id, source, source_id, title, directory, started_at, updated_at, message_count, entrypoint
             FROM sessions WHERE id IN ({})",
            placeholders.join(", ")
        );

        let params: Vec<&dyn rusqlite::types::ToSql> =
            ids.iter().map(|id| id as &dyn rusqlite::types::ToSql).collect();

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params.as_slice(), |row| {
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

        for row in rows {
            let session = row?;
            map.insert(session.id.clone(), session);
        }
        Ok(map)
    }
}

fn apply_filters(
    sql: &mut String,
    params: &mut Vec<Box<dyn rusqlite::types::ToSql>>,
    param_idx: &mut usize,
    filters: &SearchFilters,
) {
    if let Some(ref sources) = filters.sources
        && !sources.is_empty()
    {
        let placeholders: Vec<String> =
            (0..sources.len()).map(|offset| format!("?{}", *param_idx + offset)).collect();
        sql.push_str(&format!(" AND s.source IN ({})", placeholders.join(", ")));
        for source in sources {
            params.push(Box::new(source.clone()));
        }
        *param_idx += sources.len();
    }
    if let Some(min_ts) = filters.time_range.millis_ago() {
        sql.push_str(&format!(" AND s.started_at >= ?{}", *param_idx));
        params.push(Box::new(min_ts));
        *param_idx += 1;
    }
    if let Some(ref dir) = filters.directory {
        sql.push_str(&format!(" AND s.directory LIKE ?{}", *param_idx));
        params.push(Box::new(format!("{dir}%")));
        *param_idx += 1;
    }
}

fn rrf_merge(fts_hits: &[Hit], vec_hits: &[Hit], k: u32) -> Vec<(String, f64, MatchSource)> {
    let mut scores: HashMap<String, (f64, bool, bool)> = HashMap::new();

    for (rank, hit) in fts_hits.iter().enumerate() {
        let entry = scores.entry(hit.session_id.clone()).or_insert((0.0, false, false));
        entry.0 += 1.0 / (k as f64 + rank as f64 + 1.0);
        entry.1 = true;
    }

    for (rank, hit) in vec_hits.iter().enumerate() {
        let entry = scores.entry(hit.session_id.clone()).or_insert((0.0, false, false));
        entry.0 += 1.0 / (k as f64 + rank as f64 + 1.0);
        entry.2 = true;
    }

    let mut results: Vec<(String, f64, MatchSource)> = scores
        .into_iter()
        .map(|(id, (score, in_fts, in_vec))| {
            let source = match (in_fts, in_vec) {
                (true, true) => MatchSource::Hybrid,
                (true, false) => MatchSource::Fts,
                (false, true) => MatchSource::Vector,
                (false, false) => unreachable!(),
            };
            (id, score, source)
        })
        .collect();

    results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    results
}

fn fts5_escape(query: &str) -> String {
    let tokens: Vec<&str> = query.split_whitespace().collect();
    if tokens.is_empty() {
        return String::new();
    }
    tokens
        .iter()
        .map(|t| {
            let cleaned: String = t.chars().filter(|c| c.is_alphanumeric() || *c == '_').collect();
            cleaned.to_lowercase()
        })
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" OR ")
}
