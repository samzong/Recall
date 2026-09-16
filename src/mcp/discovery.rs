use super::*;

pub(super) const SEARCH_LIMIT_DEFAULT: u32 = 10;
pub(super) const SEARCH_LIMIT_MAX: u32 = 50;
pub(super) const LIST_LIMIT_DEFAULT: u32 = 10;
pub(super) const LIST_LIMIT_MAX: u32 = 50;

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub(super) struct SearchSessionsArgs {
    #[schemars(description = "Required free-text query.")]
    pub(super) query: String,
    #[schemars(
        description = "Optional absolute indexed directory path, or a repo name/owner-repo\nslug/remote URL derived from git identity. A worktree's own directory\nname alone (e.g. \"myrepo--feature\") does not match."
    )]
    #[serde(default)]
    pub(super) project: Option<String>,
    #[schemars(description = "Optional tool id or label such as claude-code or CUR.")]
    #[serde(default)]
    pub(super) source: Option<String>,
    #[schemars(description = "Maximum hits to return. Defaults to 10, capped at 50.")]
    #[serde(default, deserialize_with = "deserialize_opt_u32")]
    #[schemars(range(min = 1, max = 50))]
    pub(super) limit: Option<u32>,
    #[serde(default)]
    #[schemars(
        description = "Optional unique literal copied into this tool call so Recall can verify and exclude the invoking session when no host identity is available. It never affects relevance."
    )]
    pub(super) invocation_nonce: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(super) struct SearchMessagesArgs {
    #[serde(flatten)]
    pub(super) search: SearchSessionsArgs,
    #[serde(default)]
    #[schemars(
        description = "Optional Recall session_id to search exactly this session, including the invoking session."
    )]
    pub(super) session_id: Option<String>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub(super) struct ListRecentSessionsArgs {
    #[schemars(
        description = "Optional absolute indexed directory path, or a repo name/owner-repo\nslug/remote URL derived from git identity. A worktree's own directory\nname alone (e.g. \"myrepo--feature\") does not match."
    )]
    #[serde(default)]
    pub(super) project: Option<String>,
    #[schemars(description = "Optional tool id or label such as claude-code or CUR.")]
    #[serde(default)]
    pub(super) source: Option<String>,
    #[schemars(description = "Maximum hits to return. Defaults to 10, capped at 50.")]
    #[serde(default, deserialize_with = "deserialize_opt_u32")]
    #[schemars(range(min = 1, max = 50))]
    pub(super) limit: Option<u32>,
    #[serde(default)]
    #[schemars(
        description = "Optional unique literal copied into this tool call so Recall can verify and exclude the invoking session when no host identity is available."
    )]
    pub(super) invocation_nonce: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
pub(super) struct SessionHit {
    pub(super) session_id: String,
    pub(super) source_session_id: String,
    pub(super) source: String,
    pub(super) project: Option<String>,
    pub(super) title: String,
    pub(super) excerpt: Option<String>,
    pub(super) timestamp: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) locations: Vec<crate::host::Location>,
    #[serde(default, skip_serializing_if = "crate::db::remote_store::is_zero")]
    pub(super) alternative_versions: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
pub(super) struct HitList {
    pub(super) message: Option<String>,
    pub(super) hits: Vec<SessionHit>,
    pub(super) current_session: CurrentSession,
}

pub(super) fn search_ready(
    store: &Store,
    args: &SearchSessionsArgs,
    current_session: &CurrentSession,
) -> std::result::Result<HitList, String> {
    let (scope, sources) = resolve_filters(store, args.project.as_deref(), args.source.as_deref())?;
    let limit = clamp_limit(args.limit, SEARCH_LIMIT_DEFAULT, SEARCH_LIMIT_MAX);
    let embedding = query_embedding(store, &args.query, |message| {
        tracing::info!("{message}");
    })
    .map_err(|error| error.to_string())?;
    let filters = SearchFilters {
        sources,
        time_range: TimeRange::All,
        scope,
        thread_role: None,
        excluded_session_id: current_session.session_id.clone(),
    };
    let results = SearchEngine::new(&store.conn)
        .hybrid_search(&args.query, embedding.as_deref(), &filters, limit, 3)
        .map_err(|error| error.to_string())?;
    if results.is_empty() {
        let message = if store_is_empty(store) { EMPTY_INDEX } else { SEARCH_EMPTY };
        return Ok(HitList {
            message: Some(message.to_string()),
            hits: Vec::new(),
            current_session: current_session.clone(),
        });
    }
    Ok(HitList {
        message: None,
        current_session: current_session.clone(),
        hits: results
            .into_iter()
            .map(|result| session_hit(&result.session, result.snippet))
            .collect(),
    })
}

pub(super) fn list_ready(
    store: &Store,
    args: &ListRecentSessionsArgs,
    current_session: &CurrentSession,
) -> std::result::Result<HitList, String> {
    let (scope, sources) = resolve_filters(store, args.project.as_deref(), args.source.as_deref())?;
    let limit = clamp_limit(args.limit, LIST_LIMIT_DEFAULT, LIST_LIMIT_MAX);
    let sessions = store
        .list_recent_sessions_for_search_scope(
            sources.as_deref(),
            TimeRange::All,
            &scope,
            current_session.session_id.as_deref(),
            limit,
        )
        .map_err(|error| error.to_string())?;
    if sessions.is_empty() {
        let message = if store_is_empty(store) { EMPTY_INDEX } else { SEARCH_EMPTY };
        return Ok(HitList {
            message: Some(message.to_string()),
            hits: Vec::new(),
            current_session: current_session.clone(),
        });
    }
    Ok(HitList {
        message: None,
        current_session: current_session.clone(),
        hits: sessions
            .into_iter()
            .map(|session| {
                let excerpt =
                    session.summary.as_deref().map(|text| truncate_chars(text, EXCERPT_CHAR_CAP).0);
                session_hit(&session, excerpt)
            })
            .collect(),
    })
}
