mod file_history;
pub(crate) use file_history::{
    FileHistoryCoverage, FileHistoryEvidence, FileHistoryQuery, FileHistoryTarget,
};

use std::collections::{HashMap, HashSet};

use chrono::{Local, TimeZone};
use rusqlite::Connection;

use crate::db::project_store::apply_project_scope;
use crate::db::store::{SESSION_COLUMNS, session_from_row};
use crate::project_scope::ProjectScope;
use crate::types::{MatchSource, SearchResult, Session};
use crate::utils::f32_slice_to_bytes;

const SQLITE_VEC_MAX_K: usize = 4096;

pub(crate) struct SearchEngine<'a> {
    conn: &'a Connection,
}

#[derive(Debug, Clone)]
pub(crate) struct SearchFilters {
    pub(crate) sources: Option<Vec<String>>,
    pub(crate) time_range: TimeRange,
    pub(crate) scope: ProjectScope,
    pub(crate) thread_role: Option<ThreadRoleFilter>,
    pub(crate) excluded_session_id: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct SessionEventQuery<'a> {
    pub(crate) kinds: Option<&'a [String]>,
    pub(crate) target: &'a str,
    pub(crate) sources: Option<&'a [String]>,
    pub(crate) scope: &'a ProjectScope,
    pub(crate) limit: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct SessionEventHit {
    pub(crate) session: Session,
    pub(crate) kind: String,
    pub(crate) name: Option<String>,
    pub(crate) target: Option<String>,
    pub(crate) event_seq: u32,
    pub(crate) summary: Option<String>,
    pub(crate) timestamp: Option<i64>,
    pub(crate) visibility: Option<crate::types::EvidenceVisibility>,
    pub(crate) is_meta: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RepoFilter {
    Remote(String),
    Slug(String),
    Name(String),
}

impl RepoFilter {
    pub(crate) fn column_and_value(&self) -> (&'static str, &str) {
        match self {
            RepoFilter::Remote(remote) => ("repo_remote", remote),
            RepoFilter::Slug(slug) => ("repo_slug", slug),
            RepoFilter::Name(name) => ("repo_name", name),
        }
    }
}

/// `Unknown` maps to persisted `thread_role IS NULL` (source could not classify).
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum ThreadRoleFilter {
    Primary,
    Subagent,
    Unknown,
}

impl ThreadRoleFilter {
    pub(crate) fn sql_predicate(self) -> &'static str {
        match self {
            ThreadRoleFilter::Primary => " AND s.thread_role = 'primary'",
            ThreadRoleFilter::Subagent => " AND s.thread_role = 'subagent'",
            ThreadRoleFilter::Unknown => " AND s.thread_role IS NULL",
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ThreadRoleFilter::Primary => "primary",
            ThreadRoleFilter::Subagent => "subagent",
            ThreadRoleFilter::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TimeRange {
    Today,
    Week,
    Month,
    All,
}

impl TimeRange {
    pub(crate) fn millis_ago(&self) -> Option<i64> {
        self.cutoff_millis_at(Local::now())
    }

    pub(crate) fn cutoff_millis_at(&self, now: chrono::DateTime<Local>) -> Option<i64> {
        match self {
            TimeRange::Today => now
                .date_naive()
                .and_hms_opt(0, 0, 0)
                .and_then(|start| Local.from_local_datetime(&start).earliest())
                .map(|start| start.timestamp_millis()),
            TimeRange::Week => Some(now.timestamp_millis() - 7 * 24 * 3600 * 1000),
            TimeRange::Month => Some(now.timestamp_millis() - 30 * 24 * 3600 * 1000),
            TimeRange::All => None,
        }
    }
}

struct Hit {
    session_id: String,
    snippet: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct MessageHit {
    pub(crate) session_id: String,
    pub(crate) source_session_id: String,
    pub(crate) source: String,
    pub(crate) title: String,
    pub(crate) seq: u32,
    pub(crate) role: String,
    pub(crate) timestamp: Option<i64>,
    pub(crate) excerpt: String,
    pub(crate) locations: Vec<crate::host::Location>,
    pub(crate) alternative_versions: u32,
}

impl<'a> SearchEngine<'a> {
    pub(crate) fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    pub(crate) fn search_messages(
        &self,
        query: &str,
        filters: &SearchFilters,
        session_id: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageHit>> {
        anyhow::ensure!((1..=50).contains(&limit), "limit must be between 1 and 50");
        let tokens = tokenize_query(query);
        let trigram = crate::db::schema::has_trigram_fts(self.conn)?;
        let queries = [
            ("messages_fts", unicode61_fts5_query(&tokens, trigram)),
            (
                "messages_fts_trigram",
                if trigram { trigram_fts5_query(&tokens) } else { String::new() },
            ),
        ];
        let mut hits: HashMap<i64, (MessageHit, f64)> = HashMap::new();
        for (table, query) in queries {
            if query.is_empty() {
                continue;
            }
            let mut sql = format!(
                "SELECT m.id, m.session_id, s.source_id, s.source, s.title, m.seq, m.role,
                        m.timestamp, snippet({table}, 0, char(1), char(2), '…', 48)
                 FROM {table} JOIN messages m ON m.id = {table}.rowid
                 JOIN sessions s ON s.id = m.session_id WHERE {table} MATCH ?1"
            );
            let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(query)];
            let mut param_idx = 2;
            apply_filters(&mut sql, &mut params, &mut param_idx, filters);
            if let Some(id) = session_id {
                sql.push_str(&format!(" AND m.session_id = ?{param_idx}"));
                params.push(Box::new(id.to_string()));
            }
            sql.push_str(&format!(
                " ORDER BY {table}.rank, m.session_id, m.seq, m.id LIMIT {limit}"
            ));
            let refs: Vec<&dyn rusqlite::types::ToSql> =
                params.iter().map(|p| p.as_ref()).collect();
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map(refs.as_slice(), |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    MessageHit {
                        session_id: row.get(1)?,
                        source_session_id: row.get(2)?,
                        source: row.get(3)?,
                        title: row.get::<_, String>(4)?.chars().take(200).collect(),
                        seq: row.get(5)?,
                        role: row.get(6)?,
                        timestamp: row.get(7)?,
                        excerpt: message_excerpt(&row.get::<_, String>(8)?),
                        locations: crate::host::locations(self.conn, &row.get::<_, String>(1)?)?,
                        alternative_versions: super::remote_store::alternative_versions(
                            self.conn,
                            &row.get::<_, String>(1)?,
                        )?,
                    },
                ))
            })?;
            for (rank, row) in rows.enumerate() {
                let (id, hit) = row?;
                hits.entry(id).or_insert((hit, 0.0)).1 += 1.0 / (60 + rank) as f64;
            }
        }
        let mut hits: Vec<_> = hits.into_iter().collect();
        hits.sort_by(|a, b| {
            b.1.1
                .total_cmp(&a.1.1)
                .then_with(|| a.1.0.session_id.cmp(&b.1.0.session_id))
                .then_with(|| a.1.0.seq.cmp(&b.1.0.seq))
                .then_with(|| a.0.cmp(&b.0))
        });
        Ok(hits.into_iter().take(limit).map(|(_, (hit, _))| hit).collect())
    }

    pub(crate) fn hybrid_search(
        &self,
        query: &str,
        embedding: Option<&[f32]>,
        filters: &SearchFilters,
        limit: usize,
        fetch_multiplier: usize,
    ) -> anyhow::Result<Vec<SearchResult>> {
        let fetch_size = limit.saturating_mul(fetch_multiplier).max(1);
        let fts_hits = self.fts_search(query, filters, Some(fetch_size))?;
        let vec_hits = match embedding {
            Some(embedding) => self.vec_search(embedding, filters, fetch_size.saturating_mul(5))?,
            None => vec![],
        };
        self.search_results(fts_hits, vec_hits, 0, Some(limit))
    }

    pub(crate) fn list_session_events(
        &self,
        query: &SessionEventQuery<'_>,
    ) -> anyhow::Result<Vec<SessionEventHit>> {
        let session_cols = qualified_session_columns();
        let mut sql = format!(
            "SELECT {session_cols}, e.kind, e.name, e.target, e.event_seq, e.summary, e.timestamp, e.visibility, e.is_meta
             FROM session_events e
             JOIN sessions s ON s.id = e.session_id
             WHERE 1=1"
        );
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let mut param_idx = 1;

        if let Some(kinds) = query.kinds
            && !kinds.is_empty()
        {
            let placeholders: Vec<String> =
                (0..kinds.len()).map(|offset| format!("?{}", param_idx + offset)).collect();
            sql.push_str(&format!(" AND e.kind IN ({})", placeholders.join(", ")));
            for kind in kinds {
                params.push(Box::new(kind.clone()));
            }
            param_idx += kinds.len();
        }
        apply_target_path_match(&mut sql, &mut params, &mut param_idx, query.target);

        let filters = SearchFilters {
            sources: query.sources.map(<[String]>::to_vec),
            time_range: TimeRange::All,
            scope: query.scope.clone(),
            thread_role: None,
            excluded_session_id: None,
        };
        apply_filters(&mut sql, &mut params, &mut param_idx, &filters);

        sql.push_str(&format!(
            " ORDER BY COALESCE(e.timestamp, s.updated_at, s.started_at) DESC, e.event_seq DESC
              LIMIT ?{param_idx}"
        ));
        params.push(Box::new(i64::try_from(query.limit).unwrap_or(i64::MAX)));

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|param| param.as_ref()).collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(param_refs.as_slice(), |row| {
            Ok(SessionEventHit {
                session: session_from_row(row, self.conn)?,
                kind: row.get(17)?,
                name: row.get(18)?,
                target: row.get(19)?,
                event_seq: row.get(20)?,
                summary: row.get(21)?,
                timestamp: row.get(22)?,
                visibility: row
                    .get::<_, Option<String>>(23)?
                    .as_deref()
                    .and_then(crate::types::EvidenceVisibility::parse),
                is_meta: row.get(24)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub(crate) fn hybrid_search_page(
        &self,
        query: &str,
        embedding: Option<&[f32]>,
        filters: &SearchFilters,
        limit: Option<usize>,
        offset: usize,
    ) -> anyhow::Result<Vec<SearchResult>> {
        if limit == Some(0) {
            return Ok(vec![]);
        }

        let fts_hits = self.fts_search(query, filters, None)?;
        let vec_hits = match embedding {
            Some(embedding) => self.vec_search(embedding, filters, SQLITE_VEC_MAX_K)?,
            None => vec![],
        };
        self.search_results(fts_hits, vec_hits, offset, limit)
    }

    fn search_results(
        &self,
        fts_hits: Vec<Hit>,
        vec_hits: Vec<Hit>,
        offset: usize,
        limit: Option<usize>,
    ) -> anyhow::Result<Vec<SearchResult>> {
        let merged = rrf_merge(&fts_hits, &vec_hits, 10);
        let snippets: HashMap<_, _> =
            fts_hits.into_iter().map(|hit| (hit.session_id, hit.snippet)).collect();
        let limit = limit.unwrap_or(usize::MAX);
        let session_ids: Vec<&str> =
            merged.iter().skip(offset).take(limit).map(|(id, _, _)| id.as_str()).collect();
        let sessions = self.load_sessions(&session_ids)?;

        let mut results = Vec::new();
        for (session_id, _score, match_source) in merged.into_iter().skip(offset).take(limit) {
            if let Some(session) = sessions.get(&session_id) {
                let snippet = snippets.get(&session_id).cloned().flatten();
                results.push(SearchResult { session: session.clone(), match_source, snippet });
            }
        }
        Ok(results)
    }

    fn fts_search(
        &self,
        query: &str,
        filters: &SearchFilters,
        limit: Option<usize>,
    ) -> anyhow::Result<Vec<Hit>> {
        let tokens = tokenize_query(query);
        if tokens.is_empty() {
            return Ok(vec![]);
        }
        let queries = if crate::db::schema::has_trigram_fts(self.conn)? {
            vec![
                ("messages_fts_trigram", trigram_fts5_query(&tokens)),
                ("messages_fts", unicode61_fts5_query(&tokens, true)),
            ]
        } else {
            vec![("messages_fts", unicode61_fts5_query(&tokens, false))]
        };
        let mut hits = Vec::new();
        let mut seen = HashSet::new();
        for (table, match_query) in queries {
            if match_query.is_empty() {
                continue;
            }
            for hit in self.fts_table_search(table, match_query, filters, limit)? {
                if seen.insert(hit.session_id.clone()) {
                    hits.push(hit);
                }
            }
        }
        if let Some(limit) = limit {
            hits.truncate(limit);
        }
        Ok(hits)
    }

    fn fts_table_search(
        &self,
        table: &'static str,
        match_query: String,
        filters: &SearchFilters,
        limit: Option<usize>,
    ) -> anyhow::Result<Vec<Hit>> {
        let mut sql = format!(
            "SELECT m.session_id, SUBSTR(m.content, 1, 200) AS snip,
                    MIN({table}.rank) AS best_rank
             FROM {table}
             JOIN messages m ON m.id = {table}.rowid
             JOIN sessions s ON s.id = m.session_id
             WHERE {table} MATCH ?1",
        );

        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(match_query)];
        let mut param_idx = 2;
        apply_filters(&mut sql, &mut params, &mut param_idx, filters);

        sql.push_str(" GROUP BY m.session_id ORDER BY best_rank, m.session_id");
        if let Some(limit) = limit {
            let limit = i64::try_from(limit).unwrap_or(i64::MAX);
            sql.push_str(&format!(" LIMIT {limit}"));
        }

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|param| param.as_ref()).collect();

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
        requested_k: usize,
    ) -> anyhow::Result<Vec<Hit>> {
        let blob = f32_slice_to_bytes(embedding);
        let excluded_vectors = match filters.excluded_session_id.as_deref() {
            Some(session_id) => self.conn.query_row(
                "SELECT COUNT(*)
                 FROM message_vec mv
                 JOIN messages m ON m.id = mv.message_id
                 WHERE m.session_id = ?1",
                rusqlite::params![session_id],
                |row| row.get::<_, usize>(0),
            )?,
            None => 0,
        };
        let fetch_k =
            requested_k.saturating_add(excluded_vectors).clamp(1, SQLITE_VEC_MAX_K) as i64;

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

        sql.push_str(" GROUP BY m.session_id ORDER BY best_distance, m.session_id");

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|param| param.as_ref()).collect();

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
        const SESSION_LOAD_CHUNK_SIZE: usize = 900;

        let mut map = HashMap::new();
        for ids in ids.chunks(SESSION_LOAD_CHUNK_SIZE) {
            let placeholders: Vec<String> = (1..=ids.len()).map(|i| format!("?{i}")).collect();
            let sql = format!(
                "SELECT {SESSION_COLUMNS}
                 FROM sessions WHERE id IN ({})",
                placeholders.join(", ")
            );
            let params: Vec<&dyn rusqlite::types::ToSql> =
                ids.iter().map(|id| id as &dyn rusqlite::types::ToSql).collect();
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map(params.as_slice(), |row| session_from_row(row, self.conn))?;

            for row in rows {
                let session = row?;
                map.insert(session.id.clone(), session);
            }
        }
        Ok(map)
    }
}

fn qualified_session_columns() -> String {
    SESSION_COLUMNS.split(", ").map(|name| format!("s.{name}")).collect::<Vec<_>>().join(", ")
}

fn apply_target_path_match(
    sql: &mut String,
    params: &mut Vec<Box<dyn rusqlite::types::ToSql>>,
    param_idx: &mut usize,
    target: &str,
) {
    sql.push_str(&format!(
        " AND e.target IS NOT NULL AND (
            e.target = ?{p}
            OR substr(e.target, -length(?{p}) - 1) IN ('/' || ?{p}, char(92) || ?{p})
            OR substr(?{p}, -length(e.target) - 1) IN ('/' || e.target, char(92) || e.target)
         )",
        p = *param_idx
    ));
    params.push(Box::new(target.to_string()));
    *param_idx += 1;
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
    apply_project_scope(sql, params, param_idx, &filters.scope);
    if let Some(thread_role) = filters.thread_role {
        sql.push_str(thread_role.sql_predicate());
    }
    if let Some(excluded_session_id) = filters.excluded_session_id.as_deref() {
        sql.push_str(&format!(" AND s.id != ?{}", *param_idx));
        params.push(Box::new(excluded_session_id.to_string()));
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

    results.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    results
}

const FTS_TRIGRAM_MIN_CHARS: usize = 3;

fn message_excerpt(marked: &str) -> String {
    let hit = marked.find('\u{1}').map_or(0, |position| marked[..position].chars().count());
    let start = hit.saturating_sub(160);
    let mut excerpt: String =
        marked.chars().filter(|c| !matches!(c, '\u{1}' | '\u{2}')).skip(start).take(400).collect();
    if start > 0 {
        excerpt.insert(0, '…');
    }
    excerpt
}

fn tokenize_query(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(|token| {
            token
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '_')
                .collect::<String>()
                .to_lowercase()
        })
        .filter(|token| !token.is_empty())
        .collect()
}

fn fts5_term(token: &str, prefix: bool) -> String {
    let mut term = String::from("\"");
    term.push_str(&token.replace('"', "\"\""));
    term.push('"');
    if prefix {
        term.push('*');
    }
    term
}

fn trigram_fts5_query(tokens: &[String]) -> String {
    tokens
        .iter()
        .filter(|token| token_uses_trigram(token))
        .map(|token| fts5_term(token, false))
        .collect::<Vec<_>>()
        .join(" OR ")
}

fn unicode61_fts5_query(tokens: &[String], trigram_available: bool) -> String {
    let last = tokens.len().saturating_sub(1);
    tokens
        .iter()
        .enumerate()
        .filter(|(_, token)| !trigram_available || !token_uses_trigram(token))
        .map(|(index, token)| {
            let prefix = index == last && token.chars().count() >= 2;
            fts5_term(token, prefix)
        })
        .collect::<Vec<_>>()
        .join(" OR ")
}

fn token_uses_trigram(token: &str) -> bool {
    token.chars().count() >= FTS_TRIGRAM_MIN_CHARS && crate::utils::text_needs_trigram(token)
}

#[cfg(test)]
mod tests;
