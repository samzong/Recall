use super::SearchFilters;
use crate::db::project_store::apply_project_scope;
use crate::db::store::SESSION_COLUMNS;

const FTS_TRIGRAM_MIN_CHARS: usize = 3;

pub(super) fn qualified_session_columns() -> String {
    SESSION_COLUMNS.split(", ").map(|name| format!("s.{name}")).collect::<Vec<_>>().join(", ")
}

pub(super) fn apply_target_path_match(
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

pub(super) fn apply_filters(
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

pub(super) fn tokenize_query(query: &str) -> Vec<String> {
    query
        .split(|c: char| c.is_whitespace() || c.is_control())
        .map(|token| token.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase())
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

pub(super) fn trigram_fts5_query(tokens: &[String]) -> String {
    tokens
        .iter()
        .filter(|token| token_uses_trigram(token))
        .map(|token| fts5_term(token, false))
        .collect::<Vec<_>>()
        .join(" OR ")
}

pub(super) fn unicode61_fts5_query(tokens: &[String]) -> String {
    let last = tokens.len().saturating_sub(1);
    tokens
        .iter()
        .enumerate()
        .filter(|(_, token)| !token_uses_trigram(token))
        .map(|(index, token)| {
            let prefix = index == last && token.chars().count() >= 2;
            fts5_term(token, prefix)
        })
        .collect::<Vec<_>>()
        .join(" OR ")
}

pub(super) fn token_uses_trigram(token: &str) -> bool {
    token.chars().count() >= FTS_TRIGRAM_MIN_CHARS && crate::utils::text_needs_trigram(token)
}
