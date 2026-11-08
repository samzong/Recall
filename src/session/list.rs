use super::*;

struct SessionListRow {
    session: Session,
    match_source: Option<MatchSource>,
    snippet: Option<String>,
}

pub(crate) fn run_session_list(args: &SessionListArgs) -> Result<()> {
    let query = args.query.as_deref();
    let source_filter = args.source.as_deref();
    let time_filter = args.time.as_deref();
    let project_filter = args.project.as_deref();
    let repo_filter = args.repo.as_deref();
    let (thread_role, limit, offset, all, sync, sort, format) =
        (args.thread_role, args.limit, args.offset, args.all, args.sync, args.sort, args.format);
    if all && limit != 50 {
        anyhow::bail!("--all cannot be combined with --limit");
    }

    let sources = adapters::source_labels();
    let resolved_source = resolve_source_filter(source_filter, &sources)?;
    let time_range = parse_time_range(time_filter)?;

    let store = Store::open()?;
    let scope = store.resolve_scope(project_filter, repo_filter)?.announce();

    if sync {
        run_sync_job_inner(SyncRunOptions {
            force: false,
            verbose: false,
            emit: false,
            usage_only: false,
            backfill_events: false,
            sources: resolved_source.clone(),
            scope: scope.clone(),
            target_session: None,
        })?;
        semantic::ensure_background_worker(false)?;
    }

    let query = query.filter(|q| !q.trim().is_empty());
    let semantic_enabled = query.map(|_| search_semantic_enabled());
    let effective_limit = if all { None } else { Some(limit) };
    let rows: Vec<SessionListRow> = if let Some(query) = query {
        let engine = SearchEngine::new(&store.conn);
        let embedding =
            query_embedding_with(&store, semantic_enabled.unwrap_or(false), query, |message| {
                eprintln!("{message}")
            })?;
        let filters = SearchFilters {
            sources: resolved_source.clone(),
            time_range,
            scope: scope.clone(),
            thread_role,
            excluded_session_id: None,
        };
        let results = engine.hybrid_search_page(
            query,
            embedding.as_deref(),
            &filters,
            effective_limit,
            offset,
        )?;
        results
            .into_iter()
            .map(|result| SessionListRow {
                session: result.session,
                match_source: Some(result.match_source),
                snippet: result.snippet,
            })
            .collect::<Vec<_>>()
    } else {
        let sort = match sort.unwrap_or(SessionSort::Newest) {
            SessionSort::Newest | SessionSort::Relevance => SessionListSort::Newest,
            SessionSort::Oldest => SessionListSort::Oldest,
            SessionSort::Updated => SessionListSort::Updated,
        };
        store
            .list_indexed_sessions(
                resolved_source.as_deref(),
                time_range,
                &scope,
                thread_role,
                effective_limit,
                offset,
                sort,
            )?
            .into_iter()
            .map(|session| SessionListRow { session, match_source: None, snippet: None })
            .collect::<Vec<_>>()
    };

    match format {
        SessionListFormat::Table => print_session_list_table(&rows, &sources),
        SessionListFormat::Json => {
            print_session_list_json(&store, &rows, &sources, args, &scope, semantic_enabled)?
        }
        SessionListFormat::Jsonl => {
            for row in &rows {
                println!(
                    "{}",
                    serde_json::to_string(&session_list_row_json(&store, row, &sources)?)?
                );
            }
        }
    }
    Ok(())
}

fn print_session_list_table(rows: &[SessionListRow], sources: &[(String, String)]) {
    println!("{:<36}  {:<4}  {:<20}  {:<10}  title", "id", "src", "source_id", "messages");
    for row in rows {
        let session = &row.session;
        let label = source_label_for(&session.source, sources);
        let source_id = truncate_middle(&session.source_id, 20);
        println!(
            "{:<36}  {:<4}  {:<20}  {:>8}  {}",
            session.id, label, source_id, session.message_count, session.title
        );
        if let Some(directory) = &session.directory {
            println!("  project: {directory}");
        }
        println!("  host: {}", crate::host::label(&session.locations));
        if session.alternative_versions > 0 {
            println!("  alternative versions: {}", session.alternative_versions);
        }
        if let Some(snippet) = &row.snippet {
            println!("  match: {}", snippet.chars().take(160).collect::<String>());
        }
    }
}

fn print_session_list_json(
    store: &Store,
    rows: &[SessionListRow],
    sources: &[(String, String)],
    args: &SessionListArgs,
    scope: &ProjectScope,
    semantic_enabled: Option<bool>,
) -> Result<()> {
    let sessions = rows
        .iter()
        .map(|row| session_list_row_json(store, row, sources))
        .collect::<Result<Vec<_>>>()?;
    let mut output = serde_json::json!({
        "filters": {
            "query": args.query,
            "source": args.source,
            "project": args.project,
            "repo": args.repo,
            "effective_scope": {
                "kind": scope.kind(),
                "value": scope.value()
            },
            "time": args.time.as_deref().unwrap_or("all"),
            "thread_role": args.thread_role.map(|role| role.as_str()),
            "limit": if args.all { serde_json::Value::Null } else { serde_json::json!(args.limit) },
            "offset": args.offset,
            "sort": args.sort.map(session_sort_label)
        },
        "sessions": sessions,
        "next_offset": if args.all || rows.len() < args.limit {
            serde_json::Value::Null
        } else {
            serde_json::json!(args.offset.saturating_add(rows.len()))
        }
    });
    if let Some(enabled) = semantic_enabled {
        output["semantic_enabled"] = serde_json::json!(enabled);
    }
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

fn session_list_row_json(
    store: &Store,
    row: &SessionListRow,
    sources: &[(String, String)],
) -> Result<serde_json::Value> {
    let session = &row.session;
    let topology = store.session_topology(&session.id)?;
    Ok(serde_json::json!({
        "id": session.id,
        "source": session.source,
        "source_label": source_label_for(&session.source, sources),
        "source_id": session.source_id,
        "title": session.title,
        "project": session.directory,
        "repo_remote": session.repo_remote,
        "repo_slug": session.repo_slug,
        "repo_name": session.repo_name,
        "started_at": session.started_at,
        "updated_at": session.updated_at,
        "message_count": session.message_count,
        "entrypoint": session.entrypoint,
        "custom_title": session.custom_title,
        "summary": session.summary,
        "duration_minutes": session.duration_minutes,
        "source_file_path": session.source_file_path,
        "is_import": session.is_import,
        "locations": session.locations,
        "alternative_versions": session.alternative_versions,
        "topology": topology,
        "match_source": row.match_source.as_ref().map(match_source_label),
        "snippet": row.snippet
    }))
}

fn match_source_label(source: &MatchSource) -> &'static str {
    match source {
        MatchSource::Fts => "fts",
        MatchSource::Vector => "vector",
        MatchSource::Hybrid => "hybrid",
    }
}

fn session_sort_label(sort: SessionSort) -> &'static str {
    match sort {
        SessionSort::Newest => "newest",
        SessionSort::Oldest => "oldest",
        SessionSort::Updated => "updated",
        SessionSort::Relevance => "relevance",
    }
}

fn truncate_middle(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    if max_chars <= 3 {
        return ".".repeat(max_chars);
    }
    let keep = max_chars.saturating_sub(3);
    let head = keep / 2;
    let tail = keep - head;
    let prefix: String = value.chars().take(head).collect();
    let suffix: String =
        value.chars().rev().take(tail).collect::<Vec<_>>().into_iter().rev().collect();
    format!("{prefix}...{suffix}")
}
