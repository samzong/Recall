use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::Result;
use clap::{Subcommand, ValueEnum};
use recall::adapters;
use recall::config::AppConfig;
use recall::db::search::{SearchEngine, SearchFilters, TimeRange};
use recall::db::store::{SessionListSort, Store};
use recall::export::ExportOptions;
use recall::handoff;
use recall::query::{parse_time_range, query_embedding, resolve_source_filter};
use recall::semantic;
use recall::session_action::{self, SessionAction};
use recall::types::{MatchSource, Message, Role, Session};
use recall::{sync::SyncRunOptions, sync::run_sync_job_inner, transcript};

#[derive(Subcommand)]
pub(crate) enum SessionCommands {
    #[command(about = "List indexed sessions")]
    List {
        #[arg(long, help = "Search query text")]
        query: Option<String>,
        #[arg(long, help = "Filter by source id or label")]
        source: Option<String>,
        #[arg(long, help = "Filter by time range")]
        time: Option<String>,
        #[arg(long, help = "Filter by project directory, including child paths")]
        project: Option<String>,
        #[arg(long, help = "Filter by repository identity")]
        repo: Option<String>,
        #[arg(long, default_value_t = 50, help = "Maximum sessions to return")]
        limit: usize,
        #[arg(long, default_value_t = 0, help = "Skip sessions for pagination")]
        offset: usize,
        #[arg(long, help = "Return all matching sessions")]
        all: bool,
        #[arg(long, help = "Run incremental sync before listing")]
        sync: bool,
        #[arg(long, value_enum, help = "Sort order")]
        sort: Option<SessionSort>,
        #[arg(long, value_enum, default_value_t = SessionListFormat::Table)]
        format: SessionListFormat,
    },
    #[command(about = "Show one indexed session")]
    Show {
        #[arg(long, help = "Recall session id")]
        id: Option<String>,
        #[arg(long, help = "Source id or label")]
        source: Option<String>,
        #[arg(long, help = "Source-native session id")]
        source_id: Option<String>,
        #[arg(long, help = "Include messages in structured output")]
        messages: bool,
        #[arg(long, help = "Comma-separated: metadata,messages,usage,events")]
        include: Option<String>,
        #[arg(long, help = "First message sequence to include")]
        from_seq: Option<u32>,
        #[arg(long, help = "Last message sequence to include")]
        to_seq: Option<u32>,
        #[arg(long, value_enum, default_value_t = SessionRoleFilter::All)]
        role: SessionRoleFilter,
        #[arg(long, value_enum, default_value_t = SessionDetailFormat::Text)]
        format: SessionDetailFormat,
    },
    #[command(about = "Export selected sessions")]
    Export {
        #[arg(long = "id", help = "Recall session id; may be repeated")]
        ids: Vec<String>,
        #[arg(long, help = "Source id or label")]
        source: Option<String>,
        #[arg(long, help = "Source-native session id")]
        source_id: Option<String>,
        #[arg(long, help = "File containing newline-delimited session ids")]
        ids_file: Option<PathBuf>,
        #[arg(long, value_enum, default_value_t = SessionExportFormat::Jsonl)]
        format: SessionExportFormat,
        #[arg(long, help = "Output path; stdout when omitted")]
        output: Option<PathBuf>,
    },
    #[command(about = "Share one selected session")]
    Share {
        #[arg(long, help = "Recall session id")]
        id: Option<String>,
        #[arg(long, help = "Source id or label")]
        source: Option<String>,
        #[arg(long, help = "Source-native session id")]
        source_id: Option<String>,
        #[arg(long, help = "Validate and render metadata without deploying")]
        dry_run: bool,
        #[arg(long, help = "Open the resulting URL")]
        open: bool,
        #[arg(long, help = "Copy the resulting URL to clipboard")]
        copy_url: bool,
        #[arg(long, help = "Markdown file to render as the share page TL;DR")]
        tldr_file: Option<PathBuf>,
        #[arg(long, value_enum, default_value_t = SessionActionFormat::Text)]
        format: SessionActionFormat,
    },
    #[command(about = "Resume one selected session in its source CLI")]
    Resume {
        #[arg(long, help = "Recall session id")]
        id: Option<String>,
        #[arg(long, help = "Source id or label")]
        source: Option<String>,
        #[arg(long, help = "Source-native session id")]
        source_id: Option<String>,
        #[arg(long, help = "Print the command instead of executing it")]
        print_command: bool,
        #[arg(long, value_enum, default_value_t = SessionActionFormat::Text)]
        format: SessionActionFormat,
    },
    #[command(about = "Open one selected session in its source app")]
    Open {
        #[arg(long, help = "Recall session id")]
        id: Option<String>,
        #[arg(long, help = "Source id or label")]
        source: Option<String>,
        #[arg(long, help = "Source-native session id")]
        source_id: Option<String>,
        #[arg(long, help = "Print the command instead of executing it")]
        print_command: bool,
        #[arg(long, value_enum, default_value_t = SessionActionFormat::Text)]
        format: SessionActionFormat,
    },
    #[command(about = "Handoff one selected session to a new target agent session")]
    Handoff {
        #[arg(long, help = "Recall session id")]
        id: Option<String>,
        #[arg(long, help = "Source id or label")]
        source: Option<String>,
        #[arg(long, help = "Source-native session id")]
        source_id: Option<String>,
        #[arg(long, help = "Target agent id")]
        to: String,
        #[arg(long, help = "Print the handoff prompt instead of executing the target")]
        print_prompt: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum SessionListFormat {
    Table,
    Json,
    Jsonl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum SessionDetailFormat {
    Text,
    Json,
    Jsonl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum SessionExportFormat {
    Jsonl,
    Text,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum SessionActionFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum SessionSort {
    Newest,
    Oldest,
    Updated,
    Relevance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum SessionRoleFilter {
    All,
    User,
    Assistant,
}

struct SessionListRow {
    session: Session,
    match_source: Option<MatchSource>,
    snippet: Option<String>,
}

struct SessionIncludes {
    messages: bool,
    usage: bool,
    events: bool,
}

pub(crate) fn cmd_session(command: SessionCommands) -> Result<()> {
    match command {
        SessionCommands::List {
            query,
            source,
            time,
            project,
            repo,
            limit,
            offset,
            all,
            sync,
            sort,
            format,
        } => cmd_session_list(
            query.as_deref(),
            source.as_deref(),
            time.as_deref(),
            project.as_deref(),
            repo.as_deref(),
            limit,
            offset,
            all,
            sync,
            sort,
            format,
        ),
        SessionCommands::Show {
            id,
            source,
            source_id,
            messages,
            include,
            from_seq,
            to_seq,
            role,
            format,
        } => cmd_session_show(
            id.as_deref(),
            source.as_deref(),
            source_id.as_deref(),
            messages,
            include.as_deref(),
            from_seq,
            to_seq,
            role,
            format,
        ),
        SessionCommands::Export { ids, source, source_id, ids_file, format, output } => {
            cmd_session_export(
                ids,
                source.as_deref(),
                source_id.as_deref(),
                ids_file,
                format,
                output,
            )
        }
        SessionCommands::Share {
            id,
            source,
            source_id,
            dry_run,
            open,
            copy_url,
            tldr_file,
            format,
        } => cmd_session_share(
            id.as_deref(),
            source.as_deref(),
            source_id.as_deref(),
            dry_run,
            open,
            copy_url,
            tldr_file.as_deref(),
            format,
        ),
        SessionCommands::Resume { id, source, source_id, print_command, format } => {
            cmd_session_command(
                id.as_deref(),
                source.as_deref(),
                source_id.as_deref(),
                print_command,
                format,
                SessionAction::Resume,
            )
        }
        SessionCommands::Open { id, source, source_id, print_command, format } => {
            cmd_session_command(
                id.as_deref(),
                source.as_deref(),
                source_id.as_deref(),
                print_command,
                format,
                SessionAction::OpenApp,
            )
        }
        SessionCommands::Handoff { id, source, source_id, to, print_prompt } => {
            cmd_session_handoff(
                id.as_deref(),
                source.as_deref(),
                source_id.as_deref(),
                &to,
                print_prompt,
            )
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn cmd_session_list(
    query: Option<&str>,
    source_filter: Option<&str>,
    time_filter: Option<&str>,
    project_filter: Option<&str>,
    repo_filter: Option<&str>,
    limit: usize,
    offset: usize,
    all: bool,
    sync: bool,
    sort: Option<SessionSort>,
    format: SessionListFormat,
) -> Result<()> {
    if all && limit != 50 {
        anyhow::bail!("--all cannot be combined with --limit");
    }

    let sources = adapters::source_labels();
    let resolved_source = resolve_source_filter(source_filter, &sources)?;
    let time_range = parse_time_range(time_filter);

    if sync {
        run_sync_job_inner(SyncRunOptions {
            force: false,
            verbose: false,
            emit: false,
            usage_only: false,
            backfill_events: false,
            sources: resolved_source.clone(),
        })?;
        semantic::ensure_background_worker(false)?;
    }

    let store = Store::open()?;
    let (directory, repo) = store.resolve_project_repo_filters(project_filter, repo_filter)?;
    let effective_repo_filter = repo_filter
        .or_else(|| if repo.is_some() && directory.is_none() { project_filter } else { None });
    let effective_limit = if all { None } else { Some(limit) };
    let rows: Vec<SessionListRow> = if let Some(query) = query.filter(|q| !q.trim().is_empty()) {
        let engine = SearchEngine::new(&store.conn);
        let embedding = query_embedding(&store, query, |message| eprintln!("{message}"))?;
        let filters = SearchFilters {
            sources: resolved_source.clone(),
            time_range,
            directory: directory.clone(),
            repo: repo.clone(),
        };
        let search_limit = effective_limit.unwrap_or(10_000).saturating_add(offset).max(1);
        let results =
            engine.hybrid_search(query, embedding.as_deref(), &filters, search_limit, 3)?;
        results
            .into_iter()
            .skip(offset)
            .take(effective_limit.unwrap_or(usize::MAX))
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
                directory.as_deref(),
                repo.as_ref(),
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
        SessionListFormat::Json => print_session_list_json(
            &rows,
            &sources,
            query,
            source_filter,
            time_filter,
            project_filter,
            effective_repo_filter,
            limit,
            offset,
            all,
            sort,
        )?,
        SessionListFormat::Jsonl => {
            for row in &rows {
                println!("{}", serde_json::to_string(&session_list_row_json(row, &sources))?);
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_session_show(
    id: Option<&str>,
    source_filter: Option<&str>,
    source_id: Option<&str>,
    messages_flag: bool,
    include: Option<&str>,
    from_seq: Option<u32>,
    to_seq: Option<u32>,
    role: SessionRoleFilter,
    format: SessionDetailFormat,
) -> Result<()> {
    let store = Store::open()?;
    let sources = adapters::source_labels();
    let session = resolve_session_ref(&store, &sources, id, source_filter, source_id)?;
    let includes = parse_session_includes(include, messages_flag, format);
    let messages = if includes.messages {
        filter_session_messages(store.get_messages(&session.id)?, from_seq, to_seq, role)
    } else {
        Vec::new()
    };
    let usage_events =
        if includes.usage { store.list_usage_events_for_session(&session.id)? } else { Vec::new() };
    let events = if includes.events {
        store.list_session_events_for_session(&session.id)?
    } else {
        Vec::new()
    };

    match format {
        SessionDetailFormat::Text => {
            print!("{}", transcript::render_plain(&session, &messages));
            if !usage_events.is_empty() {
                println!("Usage events: {}", usage_events.len());
            }
            if !events.is_empty() {
                println!("Session events: {}", events.len());
            }
        }
        SessionDetailFormat::Json => {
            let value =
                recall::export::session_record_value(session, messages, usage_events, events)?;
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        SessionDetailFormat::Jsonl => {
            let value =
                recall::export::session_record_value(session, messages, usage_events, events)?;
            println!("{}", serde_json::to_string(&value)?);
        }
    }
    Ok(())
}

fn cmd_session_export(
    ids: Vec<String>,
    source_filter: Option<&str>,
    source_id: Option<&str>,
    ids_file: Option<PathBuf>,
    format: SessionExportFormat,
    output: Option<PathBuf>,
) -> Result<()> {
    let store = Store::open()?;
    let sources = adapters::source_labels();
    let sessions = resolve_session_refs(&store, &sources, ids, source_filter, source_id, ids_file)?;
    if sessions.is_empty() {
        anyhow::bail!("no sessions selected; pass --id, --ids-file, or --source with --source-id");
    }

    match format {
        SessionExportFormat::Jsonl => {
            let options = ExportOptions {
                session_ids: sessions.iter().map(|session| session.id.clone()).collect(),
                sources: None,
                time_range: TimeRange::All,
                project: None,
                repo: None,
                limit: None,
            };
            if let Some(path) = output {
                ensure_parent_dir(&path)?;
                let file = fs::File::create(&path)?;
                recall::export::write_jsonl(&store, &options, file)?;
            } else {
                let stdout = std::io::stdout();
                let handle = stdout.lock();
                recall::export::write_jsonl(&store, &options, handle)?;
            }
        }
        SessionExportFormat::Text => {
            let mut content = String::new();
            for (idx, session) in sessions.iter().enumerate() {
                if idx > 0 {
                    content.push_str("\n\n");
                }
                let messages = store.get_messages(&session.id)?;
                content.push_str(&transcript::render_plain(session, &messages));
            }
            if let Some(path) = output {
                ensure_parent_dir(&path)?;
                fs::write(path, content)?;
            } else {
                print!("{content}");
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_session_share(
    id: Option<&str>,
    source_filter: Option<&str>,
    source_id: Option<&str>,
    dry_run: bool,
    open: bool,
    copy_url: bool,
    tldr_file: Option<&Path>,
    format: SessionActionFormat,
) -> Result<()> {
    let store = Store::open()?;
    let sources = adapters::source_labels();
    let session = resolve_session_ref(&store, &sources, id, source_filter, source_id)?;
    let messages = store.get_messages(&session.id)?;
    let usage_events = store.list_usage_events_for_session(&session.id)?;
    let config = AppConfig::load_or_default();
    let tldr_markdown = tldr_file.and_then(read_tldr_file);
    let render_options = recall::share::ShareRenderOptions { tldr_markdown };
    let preview = recall::share::preview_session_with_options(
        &config,
        &session,
        &messages,
        &usage_events,
        &render_options,
    )?;
    let url = if dry_run {
        preview.url.clone()
    } else {
        eprintln!("Sharing session {}...", session.id);
        recall::share::publish_session_with_options(
            &config,
            &session,
            &messages,
            &usage_events,
            &render_options,
        )?
    };

    if copy_url {
        copy_to_clipboard(&url)?;
    }
    if open {
        open_url(&url)?;
    }

    match format {
        SessionActionFormat::Text => {
            if dry_run {
                println!("Dry run OK");
            }
            println!("{url}");
        }
        SessionActionFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "session": session_ref_json(&session),
                    "share": {
                        "provider": preview.provider,
                        "project_name": preview.project_name,
                        "project_domain": preview.project_domain,
                        "publish_dir": preview.publish_dir,
                        "file_path": preview.file_path,
                        "share_id": preview.share_id,
                        "url": url,
                        "html_bytes": preview.html_bytes
                    },
                    "dry_run": dry_run
                }))?
            );
        }
    }
    Ok(())
}

fn cmd_session_command(
    id: Option<&str>,
    source_filter: Option<&str>,
    source_id: Option<&str>,
    print_command: bool,
    format: SessionActionFormat,
    action: SessionAction,
) -> Result<()> {
    let store = Store::open()?;
    let sources = adapters::source_labels();
    let session = resolve_session_ref(&store, &sources, id, source_filter, source_id)?;
    if session.is_import {
        anyhow::bail!("imported session is not resumable on this machine");
    }
    let command = session_action::command_for(action, &session.source, &session.source_id)
        .ok_or_else(|| {
            anyhow::anyhow!("{} is not supported for {}", action.label(), session.source)
        })?;

    if print_command {
        match format {
            SessionActionFormat::Text => println!("{}", command.display()),
            SessionActionFormat::Json => {
                let display = command.display();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "session": session_ref_json(&session),
                        "command": {
                            "program": command.program,
                            "args": command.args,
                            "display": display
                        }
                    }))?
                );
            }
        }
        return Ok(());
    }

    session_action::run(&command, session.directory.as_deref())
}

fn cmd_session_handoff(
    id: Option<&str>,
    source_filter: Option<&str>,
    source_id: Option<&str>,
    target: &str,
    print_prompt: bool,
) -> Result<()> {
    let store = Store::open()?;
    let sources = adapters::source_labels();
    let session = resolve_session_ref(&store, &sources, id, source_filter, source_id)?;
    let target = handoff::target_for(target)?;
    let messages = store.get_messages(&session.id)?;
    let prompt = handoff::build_prompt(&session, &messages);

    if print_prompt {
        print!("{prompt}");
        return Ok(());
    }

    let command = handoff::command_for_target(target, prompt);
    session_action::run(&command, handoff_working_directory(session.directory.as_deref()))
}

fn handoff_working_directory(directory: Option<&str>) -> Option<&str> {
    directory.filter(|dir| std::path::Path::new(*dir).is_dir())
}

fn resolve_session_ref(
    store: &Store,
    sources: &[(String, String)],
    id: Option<&str>,
    source_filter: Option<&str>,
    source_id: Option<&str>,
) -> Result<Session> {
    match (id, source_filter, source_id) {
        (Some(id), None, None) => {
            store.get_session_by_id(id)?.ok_or_else(|| anyhow::anyhow!("session not found: {id}"))
        }
        (None, Some(source), Some(source_id)) => {
            let source = resolve_single_source(source, sources)?;
            store.get_session_by_source_id(&source, source_id)?.ok_or_else(|| {
                anyhow::anyhow!("session not found: source={source} source_id={source_id}")
            })
        }
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
            anyhow::bail!("use either --id or --source with --source-id, not both")
        }
        (None, Some(_), None) | (None, None, Some(_)) => {
            anyhow::bail!("--source and --source-id must be provided together")
        }
        (None, None, None) => {
            anyhow::bail!("missing session selector: pass --id or --source with --source-id")
        }
    }
}

fn resolve_session_refs(
    store: &Store,
    sources: &[(String, String)],
    mut ids: Vec<String>,
    source_filter: Option<&str>,
    source_id: Option<&str>,
    ids_file: Option<PathBuf>,
) -> Result<Vec<Session>> {
    if let Some(path) = ids_file {
        let content = fs::read_to_string(&path)?;
        ids.extend(
            content
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .map(String::from),
        );
    }

    let mut sessions = Vec::new();
    for id in ids {
        sessions.push(resolve_session_ref(store, sources, Some(&id), None, None)?);
    }
    if source_filter.is_some() || source_id.is_some() {
        sessions.push(resolve_session_ref(store, sources, None, source_filter, source_id)?);
    }
    Ok(sessions)
}

fn resolve_single_source(source: &str, sources: &[(String, String)]) -> Result<String> {
    let lower = source.to_lowercase();
    sources
        .iter()
        .find(|(id, label)| id == &lower || label.to_lowercase() == lower)
        .map(|(id, _)| id.clone())
        .ok_or_else(|| anyhow::anyhow!("unknown source: {source}"))
}

fn parse_session_includes(
    include: Option<&str>,
    messages_flag: bool,
    format: SessionDetailFormat,
) -> SessionIncludes {
    let mut includes = SessionIncludes {
        messages: matches!(format, SessionDetailFormat::Text) || messages_flag,
        usage: false,
        events: false,
    };
    let Some(include) = include else {
        return includes;
    };
    for part in include.split(',').map(|part| part.trim().to_lowercase()) {
        match part.as_str() {
            "all" => {
                includes.messages = true;
                includes.usage = true;
                includes.events = true;
            }
            "metadata" | "" => {}
            "messages" => includes.messages = true,
            "usage" => includes.usage = true,
            "events" => includes.events = true,
            _ => {}
        }
    }
    includes
}

fn filter_session_messages(
    messages: Vec<Message>,
    from_seq: Option<u32>,
    to_seq: Option<u32>,
    role: SessionRoleFilter,
) -> Vec<Message> {
    messages
        .into_iter()
        .filter(|message| from_seq.is_none_or(|seq| message.seq >= seq))
        .filter(|message| to_seq.is_none_or(|seq| message.seq <= seq))
        .filter(|message| match role {
            SessionRoleFilter::All => true,
            SessionRoleFilter::User => matches!(message.role, Role::User),
            SessionRoleFilter::Assistant => matches!(message.role, Role::Assistant),
        })
        .collect()
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
        if let Some(snippet) = &row.snippet {
            println!("  match: {}", snippet.chars().take(160).collect::<String>());
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn print_session_list_json(
    rows: &[SessionListRow],
    sources: &[(String, String)],
    query: Option<&str>,
    source: Option<&str>,
    time: Option<&str>,
    project: Option<&str>,
    repo: Option<&str>,
    limit: usize,
    offset: usize,
    all: bool,
    sort: Option<SessionSort>,
) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "filters": {
                "query": query,
                "source": source,
                "project": project,
                "repo": repo,
                "time": time.unwrap_or("all"),
                "limit": if all { serde_json::Value::Null } else { serde_json::json!(limit) },
                "offset": offset,
                "sort": sort.map(session_sort_label)
            },
            "sessions": rows.iter().map(|row| session_list_row_json(row, sources)).collect::<Vec<_>>(),
            "next_offset": if all || rows.len() < limit {
                serde_json::Value::Null
            } else {
                serde_json::json!(offset + rows.len())
            }
        }))?
    );
    Ok(())
}

fn session_list_row_json(row: &SessionListRow, sources: &[(String, String)]) -> serde_json::Value {
    let session = &row.session;
    let mut value = session_json(session, sources);
    if let Some(map) = value.as_object_mut() {
        map.insert(
            "match_source".to_string(),
            row.match_source
                .as_ref()
                .map(match_source_label)
                .map(|source| serde_json::Value::String(source.to_string()))
                .unwrap_or(serde_json::Value::Null),
        );
        map.insert(
            "snippet".to_string(),
            row.snippet
                .as_ref()
                .map(|snippet| serde_json::Value::String(snippet.clone()))
                .unwrap_or(serde_json::Value::Null),
        );
    }
    value
}

fn session_json(session: &Session, sources: &[(String, String)]) -> serde_json::Value {
    serde_json::json!({
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
        "is_import": session.is_import
    })
}

fn session_ref_json(session: &Session) -> serde_json::Value {
    serde_json::json!({
        "id": session.id,
        "source": session.source,
        "source_id": session.source_id,
        "title": session.title
    })
}

fn source_label_for<'a>(source: &'a str, sources: &'a [(String, String)]) -> &'a str {
    sources.iter().find(|(id, _)| id == source).map(|(_, label)| label.as_str()).unwrap_or(source)
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

fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn copy_to_clipboard(text: &str) -> Result<()> {
    let (program, args): (&str, &[&str]) = if cfg!(target_os = "macos") {
        ("pbcopy", &[])
    } else if cfg!(target_os = "windows") {
        ("clip.exe", &[])
    } else {
        ("xclip", &["-selection", "clipboard"])
    };
    let mut child = Command::new(program).args(args).stdin(Stdio::piped()).spawn()?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(text.as_bytes())?;
    }
    let status = child.wait()?;
    if !status.success() {
        anyhow::bail!("{program} exited with status {status}");
    }
    Ok(())
}

fn read_tldr_file(path: &Path) -> Option<String> {
    match fs::read_to_string(path) {
        Ok(content) => {
            let trimmed = content.trim();
            if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
        }
        Err(err) => {
            eprintln!("Warning: skipping TL;DR file {}: {err}", path.display());
            None
        }
    }
}

fn open_url(url: &str) -> Result<()> {
    let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else if cfg!(target_os = "windows") {
        ("cmd", vec!["/C", "start", "", url])
    } else {
        ("xdg-open", vec![url])
    };
    let status = Command::new(program).args(args).status()?;
    if !status.success() {
        anyhow::bail!("{program} exited with status {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handoff_working_directory_keeps_existing_directory() {
        let directory = std::env::temp_dir().to_string_lossy().into_owned();

        assert_eq!(handoff_working_directory(Some(directory.as_str())), Some(directory.as_str()));
    }

    #[test]
    fn handoff_working_directory_drops_missing_directory() {
        let directory = std::env::temp_dir()
            .join(format!("recall-missing-{}", uuid::Uuid::new_v4()))
            .to_string_lossy()
            .into_owned();

        assert_eq!(handoff_working_directory(Some(directory.as_str())), None);
    }

    #[test]
    fn read_tldr_file_skips_missing_file() {
        let path = std::env::temp_dir().join(format!("recall-missing-{}.md", uuid::Uuid::new_v4()));

        assert_eq!(read_tldr_file(&path), None);
    }

    #[test]
    fn read_tldr_file_trims_blank_and_content() {
        let path = std::env::temp_dir().join(format!("recall-tldr-{}.md", uuid::Uuid::new_v4()));
        fs::write(&path, "  **Summary**  \n").unwrap();

        assert_eq!(read_tldr_file(&path).as_deref(), Some("**Summary**"));

        fs::write(&path, " \n\t").unwrap();
        assert_eq!(read_tldr_file(&path), None);

        let _ = fs::remove_file(path);
    }
}
