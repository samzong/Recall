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
use recall::embedding::EmbeddingProvider;
use recall::export::ExportOptions;
use recall::semantic;
use recall::types::{self, MatchSource, Message, Role, Session};

use crate::{SyncRunOptions, parse_time_range, resolve_source_filter, run_sync_job_inner};

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

struct SessionListOptions<'a> {
    query: Option<&'a str>,
    source_filter: Option<&'a str>,
    time_filter: Option<&'a str>,
    project_filter: Option<&'a str>,
    limit: usize,
    offset: usize,
    all: bool,
    sync: bool,
    sort: Option<SessionSort>,
    format: SessionListFormat,
}

struct SessionShowOptions<'a> {
    id: Option<&'a str>,
    source_filter: Option<&'a str>,
    source_id: Option<&'a str>,
    messages: bool,
    include: Option<&'a str>,
    from_seq: Option<u32>,
    to_seq: Option<u32>,
    role: SessionRoleFilter,
    format: SessionDetailFormat,
}

#[derive(Default)]
struct SessionIncludes {
    messages: bool,
    usage: bool,
    events: bool,
}

#[derive(Debug, Clone, Copy)]
enum PendingCommandAction {
    Resume,
    OpenApp,
}

pub(crate) fn cmd_session(command: SessionCommands) -> Result<()> {
    match command {
        SessionCommands::List {
            query,
            source,
            time,
            project,
            limit,
            offset,
            all,
            sync,
            sort,
            format,
        } => cmd_session_list(SessionListOptions {
            query: query.as_deref(),
            source_filter: source.as_deref(),
            time_filter: time.as_deref(),
            project_filter: project.as_deref(),
            limit,
            offset,
            all,
            sync,
            sort,
            format,
        }),
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
        } => cmd_session_show(SessionShowOptions {
            id: id.as_deref(),
            source_filter: source.as_deref(),
            source_id: source_id.as_deref(),
            messages,
            include: include.as_deref(),
            from_seq,
            to_seq,
            role,
            format,
        }),
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
        SessionCommands::Share { id, source, source_id, dry_run, open, copy_url, format } => {
            cmd_session_share(
                id.as_deref(),
                source.as_deref(),
                source_id.as_deref(),
                dry_run,
                open,
                copy_url,
                format,
            )
        }
        SessionCommands::Resume { id, source, source_id, print_command, format } => {
            cmd_session_command(
                id.as_deref(),
                source.as_deref(),
                source_id.as_deref(),
                print_command,
                format,
                PendingCommandAction::Resume,
            )
        }
        SessionCommands::Open { id, source, source_id, print_command, format } => {
            cmd_session_command(
                id.as_deref(),
                source.as_deref(),
                source_id.as_deref(),
                print_command,
                format,
                PendingCommandAction::OpenApp,
            )
        }
    }
}

fn cmd_session_list(options: SessionListOptions<'_>) -> Result<()> {
    let SessionListOptions {
        query,
        source_filter,
        time_filter,
        project_filter,
        limit,
        offset,
        all,
        sync,
        sort,
        format,
    } = options;

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
    let effective_limit = if all { None } else { Some(limit) };
    let rows: Vec<SessionListRow> = if let Some(query) = query.filter(|q| !q.trim().is_empty()) {
        let engine = SearchEngine::new(&store.conn);
        let embedding = session_query_embedding(&store, query)?;
        let filters = SearchFilters {
            sources: resolved_source.clone(),
            time_range,
            directory: project_filter.map(String::from),
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
                project_filter,
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

fn cmd_session_show(options: SessionShowOptions<'_>) -> Result<()> {
    let SessionShowOptions {
        id,
        source_filter,
        source_id,
        messages: messages_flag,
        include,
        from_seq,
        to_seq,
        role,
        format,
    } = options;

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
            print_session_text(&session, &messages, &usage_events, &events)
        }
        SessionDetailFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&session_detail_json(
                    &session,
                    &sources,
                    &messages,
                    &usage_events,
                    &events,
                ))?
            );
        }
        SessionDetailFormat::Jsonl => {
            println!(
                "{}",
                serde_json::to_string(&session_detail_json(
                    &session,
                    &sources,
                    &messages,
                    &usage_events,
                    &events,
                ))?
            );
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
                content.push_str(&render_session_text(session, &messages));
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

fn cmd_session_share(
    id: Option<&str>,
    source_filter: Option<&str>,
    source_id: Option<&str>,
    dry_run: bool,
    open: bool,
    copy_url: bool,
    format: SessionActionFormat,
) -> Result<()> {
    let store = Store::open()?;
    let sources = adapters::source_labels();
    let session = resolve_session_ref(&store, &sources, id, source_filter, source_id)?;
    let messages = store.get_messages(&session.id)?;
    let config = AppConfig::load_or_default();
    let preview = recall::share::preview_session(&config, &session, &messages)?;
    let url = if dry_run {
        preview.url.clone()
    } else {
        eprintln!("Sharing session {}...", session.id);
        recall::share::publish_session(&config, &session, &messages)?
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
    action: PendingCommandAction,
) -> Result<()> {
    let store = Store::open()?;
    let sources = adapters::source_labels();
    let session = resolve_session_ref(&store, &sources, id, source_filter, source_id)?;
    if session.is_import {
        anyhow::bail!("imported session is not resumable on this machine");
    }
    let command = match action {
        PendingCommandAction::Resume => {
            adapters::resume_command_for(&session.source, &session.source_id)
        }
        PendingCommandAction::OpenApp => {
            adapters::app_command_for(&session.source, &session.source_id)
        }
    }
    .ok_or_else(|| {
        let action_label = match action {
            PendingCommandAction::Resume => "resume",
            PendingCommandAction::OpenApp => "open",
        };
        anyhow::anyhow!("{action_label} is not supported for {}", session.source)
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

    let mut process = Command::new(&command.program);
    process
        .args(&command.args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if let Some(directory) = &session.directory {
        process.current_dir(directory);
    }
    let status = process.status()?;
    if !status.success() {
        anyhow::bail!("command exited with status {status}");
    }
    Ok(())
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

fn session_query_embedding(store: &Store, query: &str) -> Result<Option<Vec<f32>>> {
    let progress = store.semantic_progress().unwrap_or_default();
    if progress.done_sessions == 0 && progress.processing_sessions == 0 {
        return Ok(None);
    }
    eprintln!("Loading embedding model...");
    match EmbeddingProvider::new(true) {
        Ok(provider) => {
            let embedding = provider
                .embed_query(&[query])?
                .into_iter()
                .next()
                .ok_or_else(|| anyhow::anyhow!("failed to generate query embedding"))?;
            Ok(Some(embedding))
        }
        Err(e) => {
            eprintln!("Semantic unavailable: {e}");
            Ok(None)
        }
    }
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

fn session_detail_json(
    session: &Session,
    sources: &[(String, String)],
    messages: &[Message],
    usage_events: &[types::SessionUsageEventRecord],
    events: &[types::SessionEventRecord],
) -> serde_json::Value {
    serde_json::json!({
        "session": session_json(session, sources),
        "messages": messages.iter().map(message_json).collect::<Vec<_>>(),
        "usage_events": usage_events.iter().map(usage_event_json).collect::<Vec<_>>(),
        "events": events.iter().map(session_event_json).collect::<Vec<_>>()
    })
}

fn session_json(session: &Session, sources: &[(String, String)]) -> serde_json::Value {
    serde_json::json!({
        "id": session.id,
        "source": session.source,
        "source_label": source_label_for(&session.source, sources),
        "source_id": session.source_id,
        "title": session.title,
        "project": session.directory,
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

fn message_json(message: &Message) -> serde_json::Value {
    serde_json::json!({
        "seq": message.seq,
        "role": message.role.as_str(),
        "timestamp": message.timestamp,
        "content": message.content
    })
}

fn usage_event_json(event: &types::SessionUsageEventRecord) -> serde_json::Value {
    serde_json::json!({
        "event_key": event.event_key,
        "event_seq": event.event_seq,
        "message_seq": event.message_seq,
        "timestamp": event.timestamp,
        "model": event.model,
        "provider": event.provider,
        "input_tokens": event.input_tokens,
        "output_tokens": event.output_tokens,
        "cache_read_tokens": event.cache_read_tokens,
        "cache_write_tokens": event.cache_write_tokens,
        "reasoning_tokens": event.reasoning_tokens,
        "token_source": event.token_source,
        "parser_version": event.parser_version,
        "source_path": event.source_path,
        "raw_usage_json": event.raw_usage_json
    })
}

fn session_event_json(event: &types::SessionEventRecord) -> serde_json::Value {
    serde_json::json!({
        "event_seq": event.event_seq,
        "timestamp": event.timestamp,
        "kind": event.kind,
        "actor": event.actor,
        "name": event.name,
        "status": event.status,
        "target": event.target,
        "message_seq": event.message_seq,
        "summary": event.summary,
        "source_path": event.source_path,
        "source_event_id": event.source_event_id,
        "attrs_json": event.attrs_json,
        "parser_version": event.parser_version
    })
}

fn print_session_text(
    session: &Session,
    messages: &[Message],
    usage_events: &[types::SessionUsageEventRecord],
    events: &[types::SessionEventRecord],
) {
    print!("{}", render_session_text(session, messages));
    if !usage_events.is_empty() {
        println!("Usage events: {}", usage_events.len());
    }
    if !events.is_empty() {
        println!("Session events: {}", events.len());
    }
}

fn render_session_text(session: &Session, messages: &[Message]) -> String {
    let mut content = String::new();
    content.push_str(&format!("Session: {}\n", session.title));
    content.push_str(&format!("ID: {}\n", session.id));
    content.push_str(&format!("Source: {}\n", session.source));
    content.push_str(&format!("Source ID: {}\n", session.source_id));
    if let Some(ref dir) = session.directory {
        content.push_str(&format!("Directory: {dir}\n"));
    }
    content.push_str(&format!(
        "Date: {}\n",
        chrono::DateTime::from_timestamp_millis(session.started_at)
            .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_default()
    ));
    content.push_str(&format!("Messages: {}\n", messages.len()));
    content.push_str("\n---\n\n");

    for msg in messages {
        let role = match msg.role {
            Role::User => "User",
            Role::Assistant => "Assistant",
        };
        content.push_str(&format!("## {role} [{}]\n\n{}\n\n", msg.seq, msg.content));
    }
    content
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
