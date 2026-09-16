use super::*;

pub(super) const EVENT_LIMIT_DEFAULT: u32 = 20;
pub(super) const EVENT_LIMIT_MAX: u32 = 50;
pub(super) const FILE_HISTORY_KINDS: [&str; 2] = ["file_write", "file_read"];
pub(super) const PATH_REQUIRED: &str = "path is required.";

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub(super) struct FileHistoryArgs {
    #[schemars(
        description = "File path, relative or absolute. Matches an event when the shorter of\nthe two is a whole path suffix of the other, so an absolute path also\nfinds events indexed under a project-relative path and vice versa."
    )]
    pub(super) path: String,
    #[schemars(
        description = "Optional absolute indexed directory path, or a repo name/owner-repo\nslug/remote URL derived from git identity. A worktree's own directory\nname alone (e.g. \"myrepo--feature\") does not match."
    )]
    #[serde(default)]
    pub(super) project: Option<String>,
    #[schemars(description = "Optional tool id or label such as claude-code or CUR.")]
    #[serde(default)]
    pub(super) source: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Optional event kind. Defaults to file_write/file_read in legacy mode, or all event kinds with target_project."
    )]
    pub(super) kind: Option<String>,
    #[schemars(description = "Maximum events to return. Defaults to 20, capped at 50.")]
    #[serde(default, deserialize_with = "deserialize_opt_u32")]
    #[schemars(range(min = 1, max = 50))]
    pub(super) limit: Option<u32>,
    #[serde(default)]
    #[schemars(
        description = "Enable structured file evidence for this target repository or directory, independently of the session project. Accepts a local directory, remote URL, or unique indexed target repository name/slug. Mutually exclusive with project."
    )]
    pub(super) target_project: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Include command evidence matched by target identity or native absolute path. Inspect match_basis; commands do not prove a modification succeeded. Only valid with target_project."
    )]
    pub(super) include_command_candidates: Option<bool>,
    #[serde(default)]
    #[schemars(
        description = "Opaque next_cursor from structured file history. Repeat the same path, target_project, source, kind and candidate selection; target-relevant index changes invalidate it. Only valid with target_project."
    )]
    pub(super) cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
pub(super) struct EventHit {
    pub(super) session_id: String,
    pub(super) source: String,
    pub(super) project: Option<String>,
    pub(super) title: String,
    pub(super) timestamp: Option<String>,
    pub(super) kind: String,
    pub(super) name: Option<String>,
    pub(super) target: Option<String>,
    pub(super) event_seq: u32,
    pub(super) summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) evidence: Option<FileHistoryEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) visibility: Option<EvidenceVisibility>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) is_meta: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, Deserialize)]
pub(super) struct EventList {
    pub(super) message: Option<String>,
    pub(super) events: Vec<EventHit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) target_file: Option<FileHistoryTarget>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) has_more: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) next_cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) ordering: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) coverage: Option<FileHistoryCoverage>,
}

pub(super) fn empty_events(message: impl Into<String>) -> EventList {
    EventList { message: Some(message.into()), events: Vec::new(), ..Default::default() }
}

pub(super) fn file_history_ready(
    store: &Store,
    args: &FileHistoryArgs,
) -> std::result::Result<EventList, String> {
    let path = args.path.trim();
    if path.is_empty() {
        return Err(PATH_REQUIRED.to_string());
    }
    if let Some(project) = &args.target_project {
        if args.project.is_some() {
            return Err("project and target_project cannot be combined".to_string());
        }
        let engine = SearchEngine::new(&store.conn);
        let target =
            engine.resolve_file_history_target(project, path).map_err(|error| error.to_string())?;
        let (_, sources) = resolve_filters(store, None, args.source.as_deref())?;
        let query = FileHistoryQuery {
            target: target.clone(),
            sources,
            kind: opt_trimmed(args.kind.as_deref()).map(str::to_string),
            include_command_candidates: args.include_command_candidates.unwrap_or(false),
        };
        let page = engine
            .file_history_page(
                &query,
                clamp_limit(args.limit, EVENT_LIMIT_DEFAULT, EVENT_LIMIT_MAX),
                args.cursor.as_deref(),
            )
            .map_err(|error| error.to_string())?;
        let events = page
            .events
            .into_iter()
            .map(|mut hit| {
                hit.hit.target =
                    hit.hit.target.map(|target| truncate_chars(&target, EXCERPT_CHAR_CAP).0);
                let mut event = event_hit(hit.hit);
                event.evidence = Some(hit.evidence);
                event
            })
            .collect::<Vec<_>>();
        return Ok(EventList {
            message: events.is_empty().then(|| "No matching file evidence.".to_string()),
            events,
            target_file: Some(target),
            has_more: Some(page.next_cursor.is_some()),
            next_cursor: page.next_cursor,
            ordering: Some(
                "known_event_timestamp_desc_then_indexed_event_id_desc; unknown_timestamps_last"
                    .to_string(),
            ),
            coverage: page.coverage,
        });
    }
    if args.include_command_candidates.is_some() || args.cursor.is_some() {
        return Err("include_command_candidates and cursor require target_project".to_string());
    }
    let kinds = match opt_trimmed(args.kind.as_deref()) {
        Some(kind) => vec![kind.to_string()],
        None => FILE_HISTORY_KINDS.iter().map(|kind| (*kind).to_string()).collect(),
    };
    let (scope, sources) = resolve_filters(store, args.project.as_deref(), args.source.as_deref())?;
    let hits = SearchEngine::new(&store.conn)
        .list_session_events(&SessionEventQuery {
            kinds: Some(kinds.as_slice()),
            target: path,
            sources: sources.as_deref(),
            scope: &scope,
            limit: clamp_limit(args.limit, EVENT_LIMIT_DEFAULT, EVENT_LIMIT_MAX),
        })
        .map_err(|error| error.to_string())?;
    if hits.is_empty() {
        let message = if store_is_empty(store) { EMPTY_INDEX } else { SEARCH_EMPTY };
        return Ok(EventList {
            message: Some(message.to_string()),
            events: Vec::new(),
            ..Default::default()
        });
    }
    Ok(EventList {
        message: None,
        events: hits.into_iter().map(event_hit).collect(),
        ..Default::default()
    })
}

pub(super) fn event_hit(hit: crate::db::search::SessionEventHit) -> EventHit {
    EventHit {
        session_id: hit.session.id.clone(),
        source: hit.session.source.clone(),
        project: hit.session.directory.clone(),
        title: session_title(&hit.session),
        timestamp: hit.timestamp.map(iso8601),
        kind: hit.kind,
        name: hit.name,
        target: hit.target,
        event_seq: hit.event_seq,
        summary: hit.summary.map(|text| truncate_chars(&text, EXCERPT_CHAR_CAP).0),
        evidence: None,
        visibility: hit.visibility,
        is_meta: hit.is_meta,
    }
}
