use std::collections::HashMap;

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

impl<'a> SearchEngine<'a> {
    pub(crate) fn new(conn: &'a Connection) -> Self {
        Self { conn }
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
            "SELECT {session_cols}, e.kind, e.name, e.target, e.event_seq, e.summary, e.timestamp
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
        let match_query = fts5_query(query);
        if match_query.is_empty() {
            return Ok(vec![]);
        }
        let mut sql = String::from(
            "SELECT m.session_id, SUBSTR(m.content, 1, 200) AS snip,
                    MIN(messages_fts.rank) AS best_rank
             FROM messages_fts
             JOIN messages m ON m.id = messages_fts.rowid
             JOIN sessions s ON s.id = m.session_id
             WHERE messages_fts MATCH ?1",
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
        let fetch_k = requested_k.clamp(1, SQLITE_VEC_MAX_K) as i64;

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

const FTS_PREFIX_MIN_CHARS: usize = 2;

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

fn fts5_query(query: &str) -> String {
    let tokens = tokenize_query(query);
    let last = tokens.len().saturating_sub(1);
    tokens
        .iter()
        .enumerate()
        .map(|(index, token)| {
            let prefix = index == last && token.chars().count() >= FTS_PREFIX_MIN_CHARS;
            fts5_term(token, prefix)
        })
        .collect::<Vec<_>>()
        .join(" OR ")
}

#[cfg(test)]
mod tests {
    use super::{SearchEngine, SessionEventQuery, fts5_query, tokenize_query};
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
    fn fts5_query_quotes_terms_and_prefixes_last_token() {
        assert_eq!(
            fts5_query("context power café Codex "),
            r#""context" OR "power" OR "café" OR "codex"*"#
        );
        assert_eq!(fts5_query("a"), r#""a""#);
        assert_eq!(fts5_query("AND OR NOT"), r#""and" OR "or" OR "not"*"#);
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
}
