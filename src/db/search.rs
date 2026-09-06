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

#[derive(Debug, Clone, PartialEq, Eq)]
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
                session: session_from_row(row)?,
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
            let rows = stmt.query_map(params.as_slice(), session_from_row)?;

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

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileHistoryTarget {
    pub(crate) requested_path: String,
    pub(crate) repo_remote: Option<String>,
    pub(crate) repo_root: Option<String>,
    pub(crate) path: Option<String>,
    pub(crate) absolute_path: Option<String>,
}

impl SearchEngine<'_> {
    pub(crate) fn resolve_file_history_target(
        &self,
        project: &str,
        path: &str,
    ) -> anyhow::Result<FileHistoryTarget> {
        use crate::project_scope::{SelectorForm, classify_selector};
        use crate::repo_identity::{
            RepoIdentityCache, git_toplevel, normalize_remote_url, origin_identity,
        };
        let project = project.trim();
        anyhow::ensure!(!project.is_empty(), "target_project is required");
        anyhow::ensure!(
            !path.is_empty()
                && !path.starts_with('~')
                && path.len() <= 4096
                && !path.contains('\0'),
            "invalid target path"
        );
        let (remote, root) = match classify_selector(project) {
            SelectorForm::Global => {
                anyhow::bail!("target_project must identify one repository or directory")
            }
            SelectorForm::Directory => {
                let directory = std::fs::canonicalize(project).map_err(|_| {
                    anyhow::anyhow!(
                        "target_project directory is unavailable; pass an indexed remote identity"
                    )
                })?;
                let directory = directory
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("invalid target directory"))?;
                let root = git_toplevel(directory).unwrap_or_else(|| directory.to_string());
                let remote = origin_identity(&root).map(|identity| identity.remote);
                (remote, Some(root))
            }
            SelectorForm::Repository(identity) => (Some(identity.remote), None),
            form => {
                let mut stmt = self.conn.prepare("SELECT DISTINCT json_extract(evidence_json, '$.target.repo_remote') FROM event_files WHERE json_extract(evidence_json, '$.target.repo_remote') IS NOT NULL")?;
                let mut matches = Vec::new();
                for remote in stmt.query_map([], |row| row.get::<_, String>(0))? {
                    let remote = remote?;
                    let Some(identity) = normalize_remote_url(&remote) else {
                        continue;
                    };
                    let matches_selector = match &form {
                        SelectorForm::Slug(slug) => identity.slug == *slug,
                        SelectorForm::IndexedName => identity.name == project,
                        _ => false,
                    };
                    if matches_selector {
                        matches.push(remote);
                    }
                }
                anyhow::ensure!(
                    matches.len() == 1,
                    "target_project has no unique indexed target identity; pass an absolute directory or remote URL"
                );
                (matches.pop(), None)
            }
        };
        let mut absolute_path = None;
        let relative = if std::path::Path::new(path).is_absolute() || root.is_some() {
            let resolved = RepoIdentityCache::default().resolve_file(path, root.as_deref());
            if let Some(file) = resolved.filter(|file| {
                remote.as_deref().is_some_and(|remote| file.repo_remote.as_deref() == Some(remote))
                    || root.as_deref().is_some_and(|root| {
                        file.repo_root.as_deref() == Some(root)
                            || (file.repo_root.is_none()
                                && std::path::Path::new(&file.absolute_path).starts_with(root))
                    })
            }) {
                absolute_path = Some(file.absolute_path.clone());
                file.repo_relative_path
                    .or_else(|| {
                        root.as_deref()
                            .and_then(|root| {
                                std::path::Path::new(&file.absolute_path).strip_prefix(root).ok()
                            })
                            .and_then(std::path::Path::to_str)
                            .map(str::to_string)
                    })
                    .ok_or_else(|| anyhow::anyhow!("target path is outside target_project"))?
            } else if std::path::Path::new(path).is_absolute() {
                let mut stmt = self.conn.prepare("SELECT DISTINCT json_extract(evidence_json, '$.target.repo_relative_path') FROM event_files WHERE json_extract(evidence_json, '$.target.repo_remote') = ?1 AND json_extract(evidence_json, '$.target.absolute_path') = ?2")?;
                let paths = stmt
                    .query_map(rusqlite::params![remote, path], |row| {
                        row.get::<_, Option<String>>(0)
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                if paths.len() != 1 || paths[0].is_none() {
                    return Ok(FileHistoryTarget {
                        requested_path: path.to_string(),
                        repo_remote: remote,
                        repo_root: root,
                        path: None,
                        absolute_path: Some(path.to_string()),
                    });
                }
                absolute_path = Some(path.to_string());
                paths
                    .into_iter()
                    .next()
                    .flatten()
                    .ok_or_else(|| anyhow::anyhow!("target path identity is unresolved"))?
            } else {
                anyhow::bail!("target path is outside target_project or cannot be resolved");
            }
        } else {
            path.to_string()
        };
        let mut normalized = std::path::PathBuf::new();
        for component in std::path::Path::new(&relative).components() {
            match component {
                std::path::Component::Normal(part) => normalized.push(part),
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir if normalized.pop() => {}
                _ => anyhow::bail!("target path must stay inside target_project"),
            }
        }
        anyhow::ensure!(!normalized.as_os_str().is_empty(), "target path must identify a file");
        Ok(FileHistoryTarget {
            requested_path: path.to_string(),
            repo_remote: remote,
            repo_root: root,
            path: Some(
                normalized
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("invalid target path"))?
                    .to_string(),
            ),
            absolute_path,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileHistoryQuery {
    pub(crate) target: FileHistoryTarget,
    pub(crate) sources: Option<Vec<String>>,
    pub(crate) kind: Option<String>,
    pub(crate) include_command_candidates: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileHistoryMatch {
    pub(crate) evidence: crate::types::FileEvidence,
    pub(crate) match_basis: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileHistoryEvidence {
    pub(crate) event_ref: String,
    pub(crate) source_session_id: String,
    pub(crate) actor: String,
    pub(crate) status: Option<String>,
    pub(crate) message_seq: Option<u32>,
    pub(crate) tool_call_id: Option<String>,
    pub(crate) source_event_id: Option<String>,
    pub(crate) parser_version: u32,
    pub(crate) visibility: Option<String>,
    pub(crate) is_import: bool,
    pub(crate) command_evidence_status: Option<String>,
    pub(crate) files: Vec<FileHistoryMatch>,
    pub(crate) file_associations: u64,
    pub(crate) matching_file_associations: u64,
    pub(crate) files_truncated: bool,
    pub(crate) target_truncated: bool,
}

#[derive(Debug)]
pub(crate) struct FileHistoryHit {
    pub(crate) hit: SessionEventHit,
    pub(crate) evidence: FileHistoryEvidence,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileHistorySourceCoverage {
    pub(crate) source: String,
    pub(crate) registered: bool,
    pub(crate) supports_event_backfill: bool,
    pub(crate) indexed_sessions: u64,
    pub(crate) imported_sessions: u64,
    pub(crate) sessions_without_parser_state: u64,
    pub(crate) observed_parser_versions: std::collections::BTreeMap<u32, u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileHistoryCoverage {
    pub(crate) scope: String,
    pub(crate) native_source_scan: String,
    pub(crate) parser_currency: String,
    pub(crate) import_coverage: String,
    pub(crate) sources: Vec<FileHistorySourceCoverage>,
}

#[derive(Debug)]
pub(crate) struct FileHistoryPage {
    pub(crate) events: Vec<FileHistoryHit>,
    pub(crate) next_cursor: Option<String>,
    pub(crate) coverage: Option<FileHistoryCoverage>,
}

#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct FileHistorySnapshot {
    index_id: String,
    events: u64,
    last_event_id: i64,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct FileHistoryCursor {
    version: u8,
    query: FileHistoryQuery,
    snapshot: FileHistorySnapshot,
    last_unknown_time: bool,
    last_timestamp: i64,
    last_event_id: i64,
}

fn file_history_predicate() -> &'static str {
    "((json_extract(f.evidence_json, '$.target.repo_remote') = ?1
            AND json_extract(f.evidence_json, '$.target.repo_relative_path') = ?3)
       OR (json_extract(f.evidence_json, '$.target.repo_remote') IS NULL
            AND json_extract(f.evidence_json, '$.target.repo_root') = ?2
            AND json_extract(f.evidence_json, '$.target.repo_relative_path') = ?3)
       OR (json_extract(f.evidence_json, '$.target.absolute_path') = ?4)
       OR (f.path = ?4 AND substr(f.path, 1, 1) = '/')
       OR (f.path = ?6 AND substr(?6, 1, 1) = '/'))
      AND (?5 OR json_extract(f.evidence_json, '$.kind') != 'command')"
}

fn file_history_parameters(query: &FileHistoryQuery) -> Vec<Box<dyn rusqlite::types::ToSql>> {
    vec![
        Box::new(query.target.repo_remote.clone()),
        Box::new(query.target.repo_root.clone()),
        Box::new(query.target.path.clone()),
        Box::new(query.target.absolute_path.clone()),
        Box::new(query.include_command_candidates),
        Box::new(query.target.requested_path.clone()),
    ]
}

impl SearchEngine<'_> {
    pub(crate) fn file_history_page(
        &self,
        query: &FileHistoryQuery,
        limit: usize,
        cursor: Option<&str>,
    ) -> anyhow::Result<FileHistoryPage> {
        anyhow::ensure!((1..=50).contains(&limit), "file history limit must be between 1 and 50");
        let tx = self.conn.unchecked_transaction()?;
        let mut parameters = file_history_parameters(query);
        let mut filter = format!(
            "e.id IN (SELECT f.event_id FROM event_files f WHERE {})",
            file_history_predicate()
        );
        if let Some(kind) = &query.kind {
            parameters.push(Box::new(kind.clone()));
            filter.push_str(&format!(" AND e.kind = ?{}", parameters.len()));
        }
        if let Some(sources) = &query.sources {
            if sources.is_empty() {
                filter.push_str(" AND 0");
            } else {
                let placeholders = sources
                    .iter()
                    .map(|source| {
                        parameters.push(Box::new(source.clone()));
                        format!("?{}", parameters.len())
                    })
                    .collect::<Vec<_>>();
                filter.push_str(&format!(" AND e.source IN ({})", placeholders.join(",")));
            }
        }
        let mut remaining_bytes = 64 * 1024 * 1024;
        let refs: Vec<&dyn rusqlite::types::ToSql> =
            parameters.iter().map(|value| value.as_ref()).collect();
        let snapshot = tx.query_row(
            &format!("SELECT (SELECT index_id FROM file_history_state WHERE id = 1), COUNT(*), COALESCE(MAX(e.id), 0) FROM session_events e NOT INDEXED WHERE {filter}"),
            refs.as_slice(),
            |row| Ok(FileHistorySnapshot { index_id: row.get(0)?, events: row.get(1)?, last_event_id: row.get(2)? }),
        )?;
        let continuation = if let Some(cursor) = cursor {
            anyhow::ensure!(cursor.len() <= 16384, "invalid file history cursor");
            let cursor: FileHistoryCursor = serde_json::from_str(cursor)
                .map_err(|_| anyhow::anyhow!("invalid file history cursor"))?;
            anyhow::ensure!(
                cursor.version == 1 && cursor.query == *query && cursor.last_event_id > 0,
                "file history cursor does not match this query"
            );
            anyhow::ensure!(
                cursor.snapshot == snapshot,
                "file history cursor is stale; query the target again"
            );
            Some(cursor)
        } else {
            None
        };
        if let Some(cursor) = continuation {
            let index = parameters.len() + 1;
            parameters.push(Box::new(cursor.last_unknown_time));
            parameters.push(Box::new(cursor.last_timestamp));
            parameters.push(Box::new(cursor.last_event_id));
            filter.push_str(&format!(" AND ((e.timestamp IS NULL) > ?{index} OR ((e.timestamp IS NULL) = ?{index} AND (COALESCE(e.timestamp, 0) < ?{} OR (COALESCE(e.timestamp, 0) = ?{} AND e.id < ?{}))))", index + 1, index + 1, index + 2));
        }
        parameters.push(Box::new((limit + 1) as i64));
        let sql = format!(
            "SELECT {}, e.kind, e.name, substr(e.target, 1, 201), e.event_seq, substr(e.summary, 1, 201), e.timestamp, e.id, e.actor, e.status, e.message_seq, e.tool_call_id, e.source_event_id, e.parser_version, e.visibility, e.command_evidence_status, length(e.target) > 200, e.is_meta FROM session_events e NOT INDEXED JOIN sessions s ON s.id = e.session_id WHERE {filter} ORDER BY (e.timestamp IS NULL), COALESCE(e.timestamp, 0) DESC, e.id DESC LIMIT ?{}",
            qualified_session_columns(),
            parameters.len()
        );
        let refs: Vec<&dyn rusqlite::types::ToSql> =
            parameters.iter().map(|value| value.as_ref()).collect();
        let page_lengths = SESSION_COLUMNS
            .split(", ")
            .chain(["kind", "actor", "status", "tool_call_id", "source_event_id", "name"])
            .map(|field| format!("COALESCE(length(CAST({field} AS BLOB)), 0)"))
            .collect::<Vec<_>>()
            .join(" + ");
        let page_bytes: usize = tx.query_row(
            &format!("SELECT COALESCE(SUM({page_lengths}), 0) FROM ({sql})"),
            refs.as_slice(),
            |row| row.get(0),
        )?;
        anyhow::ensure!(page_bytes <= remaining_bytes, "evidence_budget_exceeded");
        remaining_bytes -= page_bytes;
        let mut stmt = tx.prepare(&sql)?;
        let mut rows = stmt
            .query_map(refs.as_slice(), |row| {
                let session = session_from_row(row)?;
                Ok((
                    row.get::<_, i64>(23)?,
                    FileHistoryHit {
                        evidence: FileHistoryEvidence {
                            event_ref: String::new(),
                            source_session_id: session.source_id.clone(),
                            actor: row.get(24)?,
                            status: row.get(25)?,
                            message_seq: row.get(26)?,
                            tool_call_id: row.get(27)?,
                            source_event_id: row.get(28)?,
                            parser_version: row.get(29)?,
                            visibility: row.get(30)?,
                            command_evidence_status: row.get(31)?,
                            target_truncated: row.get::<_, Option<bool>>(32)?.unwrap_or(false),
                            is_import: session.is_import,
                            files: Vec::new(),
                            file_associations: 0,
                            matching_file_associations: 0,
                            files_truncated: false,
                        },
                        hit: SessionEventHit {
                            session,
                            kind: row.get(17)?,
                            name: row.get(18)?,
                            target: row.get(19)?,
                            event_seq: row.get(20)?,
                            summary: row.get(21)?,
                            timestamp: row.get(22)?,
                            visibility: row
                                .get::<_, Option<String>>(30)?
                                .as_deref()
                                .and_then(crate::types::EvidenceVisibility::parse),
                            is_meta: row.get(33)?,
                        },
                    },
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        let has_more = rows.len() > limit;
        rows.truncate(limit);
        let next_cursor = if has_more {
            let (id, last) =
                rows.last().ok_or_else(|| anyhow::anyhow!("missing file history continuation"))?;
            Some(serde_json::to_string(&FileHistoryCursor {
                version: 1,
                query: query.clone(),
                snapshot,
                last_unknown_time: last.hit.timestamp.is_none(),
                last_timestamp: last.hit.timestamp.unwrap_or(0),
                last_event_id: *id,
            })?)
        } else {
            None
        };
        for (id, hit) in &mut rows {
            hit.evidence.event_ref =
                serde_json::to_string(&crate::db::event_store::event_reference(&tx, *id)?)?;
            hit.evidence.file_associations = tx.query_row(
                "SELECT COUNT(*) FROM event_files WHERE event_id = ?1",
                [*id],
                |row| row.get(0),
            )?;
            let mut parameters = file_history_parameters(query);
            parameters.push(Box::new(*id));
            let refs: Vec<&dyn rusqlite::types::ToSql> =
                parameters.iter().map(|value| value.as_ref()).collect();
            hit.evidence.matching_file_associations = tx.query_row(
                &format!(
                    "SELECT COUNT(*) FROM event_files f WHERE event_id = ?7 AND {}",
                    file_history_predicate()
                ),
                refs.as_slice(),
                |row| row.get(0),
            )?;
            let files_sql = format!(
                "SELECT evidence_json FROM event_files f WHERE event_id = ?7 AND {} ORDER BY position LIMIT 32",
                file_history_predicate()
            );
            let bytes: usize = tx.query_row(&format!("SELECT COALESCE(SUM(length(CAST(evidence_json AS BLOB))), 0) FROM ({files_sql})"), refs.as_slice(), |row| row.get(0))?;
            anyhow::ensure!(bytes <= remaining_bytes, "evidence_budget_exceeded");
            remaining_bytes -= bytes;
            let mut stmt = tx.prepare(&files_sql)?;
            for evidence in stmt.query_map(refs.as_slice(), |row| row.get::<_, String>(0))? {
                let evidence: crate::types::FileEvidence = serde_json::from_str(&evidence?)?;
                let basis = if evidence.target.as_ref().is_some_and(|target| {
                    query.target.path.is_some()
                        && target.repo_remote.is_some()
                        && target.repo_remote == query.target.repo_remote
                        && target.repo_relative_path.as_deref() == query.target.path.as_deref()
                }) {
                    "repository_identity"
                } else if evidence.target.as_ref().is_some_and(|target| {
                    query.target.path.is_some()
                        && target.repo_root.is_some()
                        && target.repo_root == query.target.repo_root
                        && target.repo_relative_path.as_deref() == query.target.path.as_deref()
                }) {
                    "repository_root"
                } else if evidence.target.as_ref().is_some_and(|target| {
                    Some(target.absolute_path.as_str()) == query.target.absolute_path.as_deref()
                }) {
                    "absolute_path"
                } else {
                    "native_absolute_path"
                };
                hit.evidence
                    .files
                    .push(FileHistoryMatch { evidence, match_basis: basis.to_string() });
            }
            hit.evidence.files_truncated =
                hit.evidence.matching_file_associations > hit.evidence.files.len() as u64;
        }
        let coverage = cursor
            .is_none()
            .then(|| SearchEngine::new(&tx).file_history_coverage(query.sources.as_deref()))
            .transpose()?;
        Ok(FileHistoryPage {
            events: rows.into_iter().map(|(_, hit)| hit).collect(),
            next_cursor,
            coverage,
        })
    }

    fn file_history_coverage(
        &self,
        sources: Option<&[String]>,
    ) -> anyhow::Result<FileHistoryCoverage> {
        let selected = |source: &str| {
            sources.is_none_or(|sources| sources.iter().any(|selected| selected == source))
        };
        let mut coverage = std::collections::BTreeMap::new();
        for adapter in crate::adapters::all_adapters() {
            if selected(adapter.id()) {
                coverage.insert(
                    adapter.id().to_string(),
                    FileHistorySourceCoverage {
                        source: adapter.id().to_string(),
                        registered: true,
                        supports_event_backfill: crate::adapters::source_supports_event_backfill(
                            adapter.id(),
                        ),
                        ..Default::default()
                    },
                );
            }
        }
        let mut stmt = self.conn.prepare("SELECT s.source, COUNT(*), SUM(s.is_import), SUM(p.session_id IS NULL) FROM sessions s LEFT JOIN event_session_state p ON p.session_id = s.id GROUP BY s.source")?;
        for row in stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, u64>(1)?,
                row.get::<_, u64>(2)?,
                row.get::<_, u64>(3)?,
            ))
        })? {
            let (source, indexed_sessions, imported_sessions, sessions_without_parser_state) = row?;
            if !selected(&source) {
                continue;
            }
            let entry = coverage
                .entry(source.clone())
                .or_insert_with(|| FileHistorySourceCoverage { source, ..Default::default() });
            entry.indexed_sessions = indexed_sessions;
            entry.imported_sessions = imported_sessions;
            entry.sessions_without_parser_state = sessions_without_parser_state;
        }
        let mut stmt = self.conn.prepare("SELECT source, parser_version, COUNT(*) FROM event_session_state GROUP BY source, parser_version")?;
        for row in stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?, row.get::<_, u64>(2)?))
        })? {
            let (source, version, count) = row?;
            if let Some(entry) = coverage.get_mut(&source) {
                entry.observed_parser_versions.insert(version, count);
            }
        }
        Ok(FileHistoryCoverage {
            scope: "selected_sources_all_indexed_sessions".to_string(),
            native_source_scan: "not_performed".to_string(),
            parser_currency: "recorded_versions_only".to_string(),
            import_coverage: "unknown".to_string(),
            sources: coverage.into_values().collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FileHistoryQuery, SearchEngine, SearchFilters, SessionEventQuery, TimeRange,
        tokenize_query, trigram_fts5_query, unicode61_fts5_query,
    };
    use crate::db::schema;
    use crate::db::store::Store;
    use crate::project_scope::ProjectScope;
    use crate::types::{RawSessionEvent, Session};

    #[test]
    fn tokenize_query_strips_punctuation_and_case() {
        assert_eq!(
            tokenize_query("context power café Codex "),
            vec!["context", "power", "café", "codex"]
        );
        assert_eq!(tokenize_query("bug OR 1=1 --"), vec!["bug", "or", "11"]);
    }

    #[test]
    fn fts5_queries_split_trigram_and_short_tokens() {
        let tokens = tokenize_query("context power café Codex ");
        assert_eq!(trigram_fts5_query(&tokens), "");
        assert_eq!(
            unicode61_fts5_query(&tokens, true),
            r#""context" OR "power" OR "café" OR "codex"*"#
        );
        let short = tokenize_query("rx \u{77e9}\u{9635}");
        assert_eq!(trigram_fts5_query(&short), "");
        assert_eq!(
            unicode61_fts5_query(&short, true),
            format!(r#""rx" OR "{}"*"#, "\u{77e9}\u{9635}")
        );
        let keywords = tokenize_query("AND OR NOT");
        assert_eq!(trigram_fts5_query(&keywords), "");
        assert_eq!(unicode61_fts5_query(&keywords, true), r#""and" OR "or" OR "not"*"#);
        let phrase_text = "\u{7edf}\u{8ba1}\u{7684}\u{4e0d}\u{51c6}\u{786e}";
        let phrase = tokenize_query(phrase_text);
        assert_eq!(trigram_fts5_query(&phrase), format!("\"{phrase_text}\""));
    }

    #[test]
    fn search_keeps_short_mixed_and_deferred_legacy_terms_without_embeddings() {
        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        store
            .conn
            .execute_batch(
                "INSERT INTO sessions (id, source, source_id, title, started_at)
                 VALUES
                    ('short', 'test', 'short', 'Short', 1),
                    ('cjk', 'test', 'cjk', 'CJK', 2),
                    ('long', 'test', 'long', 'Long', 3);
                 INSERT INTO messages (session_id, role, content, seq)
                 VALUES
                    ('short', 'user', 'rx routing', 0),
                    ('long', 'user', 'context cache enables powercontext', 0);",
            )
            .unwrap();
        let cjk = "\u{77e9}\u{9635}\u{8fd0}\u{7b97}";
        store
            .conn
            .execute(
                "INSERT INTO messages (session_id, role, content, seq, trigram_indexed)
                 VALUES ('cjk', 'user', ?1, 0, ?2)",
                rusqlite::params![cjk, crate::utils::text_needs_trigram(cjk)],
            )
            .unwrap();
        let filters = SearchFilters {
            sources: None,
            time_range: TimeRange::All,
            scope: ProjectScope::Global,
            thread_role: None,
            excluded_session_id: None,
        };
        let engine = SearchEngine::new(&store.conn);

        let short = engine.hybrid_search("rx", None, &filters, 10, 3).unwrap();
        assert_eq!(short[0].session.id, "short");
        let cjk = engine.hybrid_search("\u{77e9}\u{9635}", None, &filters, 10, 3).unwrap();
        assert_eq!(cjk[0].session.id, "cjk");
        let mut mixed: Vec<_> = engine
            .hybrid_search("context rx", None, &filters, 10, 3)
            .unwrap()
            .into_iter()
            .map(|result| result.session.id)
            .collect();
        mixed.sort();
        assert_eq!(mixed, vec!["long".to_string(), "short".to_string()]);

        store.conn.execute_batch("DROP TABLE messages_fts_trigram;").unwrap();
        let legacy = engine.hybrid_search("powercon", None, &filters, 10, 3).unwrap();
        assert_eq!(legacy[0].session.id, "long");
    }

    fn setup_store() -> Store {
        schema::register_sqlite_vec();
        Store::open_in_memory().unwrap()
    }

    fn session(id: &str, source: &str, directory: &str) -> Session {
        Session {
            id: id.to_string(),
            source: source.to_string(),
            source_id: format!("src-{id}"),
            title: id.to_string(),
            directory: Some(directory.to_string()),
            repo_remote: None,
            repo_slug: None,
            repo_name: None,
            started_at: 1_000,
            updated_at: Some(1_000),
            message_count: 0,
            entrypoint: None,
            custom_title: None,
            summary: None,
            duration_minutes: None,
            source_file_path: None,
            is_import: false,
        }
    }

    fn event(seq: u32, kind: &str, target: &str, timestamp: i64) -> RawSessionEvent {
        RawSessionEvent {
            command_evidence_status: None,
            files: Vec::new(),
            event_seq: seq,
            timestamp: Some(timestamp),
            kind: kind.to_string(),
            actor: "assistant".to_string(),
            name: Some(kind.to_string()),
            status: None,
            target: Some(target.to_string()),
            message_seq: Some(1),
            summary: Some(format!("{kind} {target}")),
            source_path: None,
            source_event_id: None,
            tool_call_id: None,
            is_meta: None,
            visibility: None,
            attrs_json: Some(r#"{"secret":"nope"}"#.to_string()),
            parser_version: 1,
        }
    }

    fn seed_events() -> Store {
        let store = setup_store();
        store.insert_session(&session("s1", "codex", "/tmp/demo")).unwrap();
        store.insert_session(&session("s2", "claude-code", "/tmp/demo")).unwrap();
        store
            .persist_session_events_for_existing_session(
                "codex",
                "src-s1",
                &[
                    event(0, "file_write", "/tmp/demo/src/db/schema.rs", 5_000),
                    event(1, "command", "/tmp/demo/src/db/schema.rs", 6_000),
                    event(2, "file_write", "old_schema.rs", 4_000),
                ],
                1,
                None,
            )
            .unwrap();
        store
            .persist_session_events_for_existing_session(
                "claude-code",
                "src-s2",
                &[event(0, "file_read", "src/db/schema.rs", 8_000)],
                1,
                None,
            )
            .unwrap();
        store
    }

    fn query_events(
        store: &Store,
        target: &str,
        kinds: Option<&[String]>,
    ) -> Vec<super::SessionEventHit> {
        SearchEngine::new(&store.conn)
            .list_session_events(&SessionEventQuery {
                kinds,
                target,
                sources: None,
                scope: &ProjectScope::Global,
                limit: 50,
            })
            .unwrap()
    }

    #[test]
    fn list_session_events_matches_exact_or_separator_suffix() {
        let store = seed_events();
        let kinds = vec!["file_write".to_string(), "file_read".to_string()];
        let hits = query_events(&store, "src/db/schema.rs", Some(&kinds));
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].session.id, "s2");
        assert_eq!(hits[0].target.as_deref(), Some("src/db/schema.rs"));
        assert_eq!(hits[1].session.id, "s1");
        assert_eq!(hits[1].target.as_deref(), Some("/tmp/demo/src/db/schema.rs"));
        assert!(hits.iter().all(|hit| hit.kind != "command"));

        let bare = query_events(&store, "schema.rs", Some(&kinds));
        assert_eq!(bare.len(), 2);
        assert!(bare.iter().all(|hit| {
            hit.target.as_deref().is_some_and(|target| {
                target == "schema.rs"
                    || target.ends_with("/schema.rs")
                    || target.ends_with("\\schema.rs")
            })
        }));

        let no_substring = query_events(&store, "schema.rs", None);
        assert!(no_substring.iter().all(|hit| hit.target.as_deref() != Some("old_schema.rs")));
    }

    #[test]
    fn list_session_events_matches_relative_target_from_absolute_path() {
        let store = seed_events();
        let hits = query_events(&store, "/abs/elsewhere/src/db/schema.rs", None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session.id, "s2");
        assert_eq!(hits[0].target.as_deref(), Some("src/db/schema.rs"));
    }

    #[test]
    fn list_session_events_orders_newest_event_first() {
        let store = seed_events();
        let hits = query_events(&store, "schema.rs", None);
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].timestamp, Some(8_000));
        assert_eq!(hits[1].timestamp, Some(6_000));
        assert_eq!(hits[1].kind, "command");
        assert_eq!(hits[2].timestamp, Some(5_000));
    }

    #[test]
    fn list_session_events_ranks_timestampless_events_by_session_activity() {
        let store = seed_events();
        let mut recent = session("s3", "cursor", "/tmp/demo");
        recent.updated_at = Some(9_000);
        store.insert_session(&recent).unwrap();
        let mut no_timestamp = event(0, "file_write", "src/db/schema.rs", 0);
        no_timestamp.timestamp = None;
        store
            .persist_session_events_for_existing_session(
                "cursor",
                "src-s3",
                &[no_timestamp],
                1,
                None,
            )
            .unwrap();

        let hits = query_events(&store, "schema.rs", None);
        assert_eq!(hits.len(), 4);
        assert_eq!(hits[0].session.id, "s3");
        assert_eq!(hits[0].timestamp, None);
        assert_eq!(hits[1].timestamp, Some(8_000));
    }
    #[test]
    fn message_search_returns_distinct_anchors_and_match_centered_excerpts() {
        crate::db::schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        store.conn.execute_batch("INSERT INTO sessions (id, source, source_id, title, started_at) VALUES ('s', 'codex', 'native', 'Test', 1), ('other', 'claude-code', 'other', 'Other', 2);").unwrap();
        let long = format!("{} evidencekeyword fixes the lock", "ordinary preamble ".repeat(100));
        for (id, seq, content) in [
            ("s", 83, long.as_str()),
            ("s", 90, "evidencekeyword verified"),
            ("other", 2, "evidencekeyword unrelated"),
        ] {
            store.conn.execute("INSERT INTO messages (session_id, seq, role, content) VALUES (?1, ?2, 'assistant', ?3)", rusqlite::params![id, seq, content]).unwrap();
        }
        let filters = SearchFilters {
            sources: None,
            time_range: TimeRange::All,
            scope: ProjectScope::Global,
            thread_role: None,
            excluded_session_id: None,
        };
        let engine = SearchEngine::new(&store.conn);
        let hits = engine.search_messages("evidencekeyword", &filters, Some("s"), 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.session_id == "s"
            && h.role == "assistant"
            && h.excerpt.contains("evidencekeyword")));
        let mut seqs = hits.iter().map(|h| h.seq).collect::<Vec<_>>();
        seqs.sort_unstable();
        assert_eq!(seqs, vec![83, 90]);
        let excluded = SearchFilters { excluded_session_id: Some("s".into()), ..filters.clone() };
        assert_eq!(
            engine.search_messages("evidencekeyword", &excluded, None, 10).unwrap()[0].session_id,
            "other"
        );
        let filtered = SearchFilters { sources: Some(vec!["codex".into()]), ..filters };
        assert_eq!(engine.search_messages("evidencekeyword", &filtered, None, 1).unwrap().len(), 1);
    }
    #[test]
    fn file_history_pages_target_evidence_across_session_projects() {
        use crate::types::{FileEvidence, FileEvidenceKind, FileOperation, FileTarget};
        let store = setup_store();
        let mut foreign = session("foreign", "codex", "/tmp/foreign");
        foreign.repo_remote = Some("github.com/example/foreign".to_string());
        store.insert_session(&foreign).unwrap();
        let mut other = session("other", "codex", "/tmp/project");
        other.repo_remote = Some("github.com/example/project".to_string());
        store.insert_session(&other).unwrap();
        let mut imported = session("import", "unknown-agent", "/tmp/import");
        imported.is_import = true;
        store.insert_session(&imported).unwrap();
        let file = FileEvidence {
            path: "src/lib.rs".to_string(),
            operation: FileOperation::Write,
            kind: FileEvidenceKind::Call,
            cwd: Some("/tmp/project--feature".to_string()),
            target: Some(FileTarget {
                absolute_path: "/tmp/project--feature/src/lib.rs".to_string(),
                repo_root: Some("/tmp/project--feature".to_string()),
                repo_relative_path: Some("src/lib.rs".to_string()),
                repo_remote: Some("github.com/example/project".to_string()),
            }),
        };
        let mut events = (0..65)
            .map(|seq| {
                let mut event = event(seq, "file_write", "src/lib.rs", 5000);
                event.timestamp = (seq < 60).then_some(5000);
                event.source_event_id = Some(format!("native-{seq}"));
                event.files = vec![file.clone()];
                let mut second = file.clone();
                second.path = "src/other.rs".to_string();
                second.target.as_mut().unwrap().repo_relative_path =
                    Some("src/other.rs".to_string());
                second.target.as_mut().unwrap().absolute_path =
                    "/tmp/project--feature/src/other.rs".to_string();
                event.files.push(second);
                event
            })
            .collect::<Vec<_>>();
        let mut command = event(65, "command", "edit src/lib.rs", 5000);
        command.files = vec![file.clone()];
        command.files[0].kind = FileEvidenceKind::Command;
        command.command_evidence_status = Some(crate::types::CommandEvidenceStatus::Complete);
        events.push(command);
        let mut unknown = event(66, "command", "edit src/lib.rs", 5000);
        unknown.files = vec![file.clone()];
        unknown.files[0].kind = FileEvidenceKind::Command;
        unknown.files[0].target = None;
        unknown.files[0].cwd = None;
        events.push(unknown);
        let mut approval = event(67, "approval", "src/lib.rs", 5000);
        approval.files = vec![file.clone()];
        approval.status = Some("approved".to_string());
        events.push(approval);
        let mut observed = event(68, "tool_result", "src/lib.rs", 5000);
        observed.files = vec![file.clone()];
        observed.files[0].kind = FileEvidenceKind::Observation;
        observed.files[0].operation = FileOperation::Delete;
        events.push(observed);
        let mut native_absolute = event(69, "command", "edit src/lib.rs", 5000);
        native_absolute.files = vec![file.clone()];
        native_absolute.files[0].kind = FileEvidenceKind::Command;
        native_absolute.files[0].path = "/tmp/project--feature/src/lib.rs".to_string();
        native_absolute.files[0].target = None;
        native_absolute.files[0].cwd = None;
        events.push(native_absolute);
        store
            .persist_session_events_for_existing_session("codex", "src-foreign", &events, 1, None)
            .unwrap();
        let mut wrong = event(0, "file_write", "src/lib.rs", 5000);
        wrong.files = vec![file];
        wrong.files[0].target.as_mut().unwrap().repo_remote =
            Some("github.com/other/project".to_string());
        wrong.files[0].target.as_mut().unwrap().repo_root = Some("/tmp/other-project".to_string());
        wrong.files[0].target.as_mut().unwrap().absolute_path =
            "/tmp/other-project/src/lib.rs".to_string();
        store
            .persist_session_events_for_existing_session(
                "codex",
                "src-other",
                &[wrong.clone()],
                1,
                None,
            )
            .unwrap();
        let engine = SearchEngine::new(&store.conn);
        let query = FileHistoryQuery {
            target: engine
                .resolve_file_history_target("https://github.com/example/project.git", "src/lib.rs")
                .unwrap(),
            sources: None,
            kind: None,
            include_command_candidates: false,
        };
        assert!(engine.resolve_file_history_target("project", "src/lib.rs").is_err());
        assert!(engine.resolve_file_history_target("project--feature", "src/lib.rs").is_err());
        let basename = FileHistoryQuery {
            target: engine
                .resolve_file_history_target("github.com/example/project", "lib.rs")
                .unwrap(),
            ..query.clone()
        };
        assert!(engine.file_history_page(&basename, 20, None).unwrap().events.is_empty());
        let first = engine.file_history_page(&query, 17, None).unwrap();
        assert_eq!(first.events.len(), 17);
        assert!(first.events.iter().all(|event| event.hit.session.id == "foreign"));
        let write = first.events.iter().find(|event| event.hit.kind == "file_write").unwrap();
        assert_eq!(write.evidence.file_associations, 2);
        assert_eq!(write.evidence.files.len(), 1);
        assert!(first.events.iter().any(|event| event.hit.kind == "approval"
            && event.evidence.status.as_deref() == Some("approved")));
        let imported = first
            .coverage
            .as_ref()
            .unwrap()
            .sources
            .iter()
            .find(|source| source.source == "unknown-agent")
            .unwrap();
        assert!(!imported.registered);
        assert_eq!(imported.imported_sessions, 1);
        assert_eq!(imported.sessions_without_parser_state, 1);
        let cursor = first.next_cursor.clone().unwrap();
        store
            .persist_session_events_for_existing_session(
                "codex",
                "src-other",
                &[wrong],
                1,
                Some(9999),
            )
            .unwrap();
        assert!(engine.file_history_page(&query, 17, Some(&cursor)).is_ok());
        let mut references =
            first.events.iter().map(|hit| hit.evidence.event_ref.clone()).collect::<Vec<_>>();
        let mut next = first.next_cursor;
        let mut last_unknown = false;
        while let Some(cursor) = next {
            let page = engine.file_history_page(&query, 17, Some(&cursor)).unwrap();
            assert!(page.coverage.is_none());
            for hit in &page.events {
                if last_unknown {
                    assert!(hit.hit.timestamp.is_none());
                }
                last_unknown = hit.hit.timestamp.is_none();
                references.push(hit.evidence.event_ref.clone());
            }
            next = page.next_cursor;
        }
        assert_eq!(references.len(), 67);
        assert_eq!(references.iter().collect::<std::collections::HashSet<_>>().len(), 67);
        let candidates = FileHistoryQuery { include_command_candidates: true, ..query.clone() };
        let mut count = 0;
        let mut next = None;
        loop {
            let page = engine.file_history_page(&candidates, 50, next.as_deref()).unwrap();
            count += page.events.len();
            next = page.next_cursor;
            if next.is_none() {
                break;
            }
        }
        assert_eq!(count, 68);
        let absolute = FileHistoryQuery {
            target: engine
                .resolve_file_history_target(
                    "https://github.com/example/project.git",
                    "/tmp/project--feature/src/lib.rs",
                )
                .unwrap(),
            ..candidates.clone()
        };
        let first_absolute = engine.file_history_page(&absolute, 50, None).unwrap();
        assert!(first_absolute.events.iter().any(|hit| hit.hit.event_seq == 69));
        assert!(first_absolute.events.iter().all(|hit| hit.hit.event_seq != 66));
        let unresolved_absolute = FileHistoryQuery {
            target: engine
                .resolve_file_history_target(
                    "https://github.com/example/unrelated.git",
                    "/tmp/project--feature/src/lib.rs",
                )
                .unwrap(),
            ..absolute.clone()
        };
        assert!(unresolved_absolute.target.path.is_none());
        let native_matches = engine.file_history_page(&unresolved_absolute, 50, None).unwrap();
        assert!(native_matches.events.iter().any(|hit| hit.hit.event_seq == 69));
        assert!(native_matches.events.iter().all(|hit| hit.hit.event_seq != 66));
        assert!(engine.file_history_page(&candidates, 17, Some(&cursor)).is_err());
        events[0].files[0].operation = FileOperation::Delete;
        store
            .persist_session_events_for_existing_session("codex", "src-foreign", &events, 1, None)
            .unwrap();
        assert!(
            engine
                .file_history_page(&query, 17, Some(&cursor))
                .unwrap_err()
                .to_string()
                .contains("stale")
        );
        store
            .conn
            .execute("UPDATE sessions SET summary = ?1 WHERE id = 'foreign'", ["x".repeat(131072)])
            .unwrap();
        let first = engine.file_history_page(&query, 1, None).unwrap();
        let second = engine.file_history_page(&query, 1, first.next_cursor.as_deref()).unwrap();
        assert_eq!(first.events.len(), 1);
        assert_eq!(second.events.len(), 1);
        assert_ne!(first.events[0].evidence.event_ref, second.events[0].evidence.event_ref);
        store.conn.execute("UPDATE event_files SET evidence_json = json_set(evidence_json, '$.cwd', printf('%.*c', 67108865, 'x')) WHERE event_id = (SELECT MIN(id) FROM session_events WHERE session_id = 'foreign') AND position = 1", []).unwrap();
        assert_eq!(engine.file_history_page(&query, 1, None).unwrap().events.len(), 1);
        let before_delete = engine.file_history_page(&query, 1, None).unwrap();
        store.delete_session_data("codex", "src-foreign").unwrap();
        assert!(engine.file_history_page(&query, 1, before_delete.next_cursor.as_deref()).is_err());
    }

    #[test]
    fn event_references_bind_an_index_and_an_immutable_record() {
        let stores = [setup_store(), setup_store()];
        let mut references = Vec::new();
        for store in &stores {
            store.insert_session(&session("s1", "codex", "/tmp/project")).unwrap();
            store
                .persist_session_events_for_existing_session(
                    "codex",
                    "src-s1",
                    &[event(0, "file_write", "src/lib.rs", 5000)],
                    1,
                    None,
                )
                .unwrap();
            let tx = store.conn.unchecked_transaction().unwrap();
            references.push(crate::db::event_store::event_reference(&tx, 1).unwrap());
        }
        assert_eq!(references[0].event_id, references[1].event_id);
        assert_ne!(references[0], references[1]);
        let store = &stores[0];
        store
            .persist_session_events_for_existing_session(
                "codex",
                "src-s1",
                &[event(0, "file_write", "src/lib.rs", 5000)],
                1,
                None,
            )
            .unwrap();
        let tx = store.conn.unchecked_transaction().unwrap();
        assert!(crate::db::event_store::event_reference(&tx, references[0].event_id).is_err());
        let id = tx.query_row("SELECT id FROM session_events", [], |row| row.get(0)).unwrap();
        let replacement = crate::db::event_store::event_reference(&tx, id).unwrap();
        assert_eq!(replacement.index_id, references[0].index_id);
        assert!(replacement.event_id > references[0].event_id);
    }
}
