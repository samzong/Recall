use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::info;

use recall::adapters;
use recall::config::AppConfig;
use recall::db;
use recall::db::search::{SearchEngine, SearchFilters, TimeRange};
use recall::db::store::{EventSessionStateMeta, Store, UsageSessionStateMeta};
use recall::embedding::EmbeddingProvider;
use recall::export::ExportOptions;
use recall::semantic;
use recall::types::{self, Message, Role, Session};
use recall::usage::{self, UsageFilters};
use recall::utils;

mod session;

#[derive(Parser)]
#[command(name = "recall", version, about = "Search and recall AI coding sessions")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    #[command(about = "Show indexed source and background job status")]
    Info,
    #[command(about = "Scan configured AI coding session sources")]
    Sync {
        #[arg(long, help = "Reprocess every session, even if unchanged")]
        force: bool,
        #[arg(short, long, help = "Show per-source scan progress and settings")]
        verbose: bool,
        #[arg(long, help = "Sync only this source (id or label, e.g. cursor or CUR)")]
        source: Option<String>,
    },
    #[command(hide = true, name = "__background-worker")]
    BackgroundWorker {
        #[arg(long)]
        sync_first: bool,
    },
    #[command(hide = true, name = "__bench-semantic")]
    BenchSemantic,
    #[command(hide = true, name = "__bench-search")]
    BenchSearch { query: String },
    #[command(hide = true, name = "__bench-eval")]
    BenchEval {
        #[arg(long)]
        dataset: Option<String>,
        #[arg(short, long)]
        verbose: bool,
    },
    #[command(hide = true, name = "__bench-dump-sessions")]
    BenchDumpSessions,
    #[command(about = "Search indexed coding sessions")]
    Search {
        #[arg(help = "Search query text")]
        query: String,
        #[arg(long, help = "Filter by source id or label")]
        source: Option<String>,
        #[arg(long, help = "Filter by time range")]
        time: Option<String>,
        #[arg(long, help = "Filter by project directory, including child paths")]
        project: Option<String>,
    },
    #[command(about = "Show token usage reports")]
    Usage {
        #[arg(long, help = "Output usage report as JSON")]
        json: bool,
        #[arg(long, help = "Filter by source id or label")]
        source: Option<String>,
        #[arg(long, help = "Filter by time range")]
        time: Option<String>,
    },
    #[command(about = "Export session records as JSON Lines")]
    Export {
        #[arg(long, help = "Filter by source id or label")]
        source: Option<String>,
        #[arg(long, help = "Filter by time range")]
        time: Option<String>,
        #[arg(long, help = "Filter by project directory, including child paths")]
        project: Option<String>,
        #[arg(
            long,
            default_value_t = 0,
            help = "Maximum sessions to export; 0 means all (default)"
        )]
        limit: usize,
    },
    #[command(about = "Import session records from JSON Lines")]
    Import {
        #[arg(help = "Input file path, or - for stdin")]
        file: String,
        #[arg(long, help = "Parse and report without writing")]
        dry_run: bool,
    },
    #[command(about = "Share session pages")]
    Share {
        #[command(subcommand)]
        command: ShareCommands,
    },
    #[command(about = "Operate on indexed sessions")]
    Session {
        #[command(subcommand)]
        command: session::SessionCommands,
    },
}

#[derive(Subcommand)]
enum ShareCommands {
    #[command(about = "Initialize Cloudflare Pages sharing")]
    Init {
        #[arg(long, help = "Cloudflare Pages project name")]
        project_name: Option<String>,
        #[arg(long, help = "Local directory used for generated share pages")]
        publish_dir: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    db::schema::register_sqlite_vec();

    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Info) => cmd_info()?,
        Some(Commands::Sync { force, verbose, source }) => {
            cmd_sync(force, verbose, source.as_deref())?
        }
        Some(Commands::BackgroundWorker { sync_first }) => cmd_background_worker(sync_first)?,
        Some(Commands::BenchSemantic) => recall::bench::run_semantic()?,
        Some(Commands::BenchSearch { query }) => recall::bench::run_search(&query)?,
        Some(Commands::BenchEval { dataset, verbose }) => {
            recall::bench::run_eval(dataset.as_deref(), verbose)?
        }
        Some(Commands::BenchDumpSessions) => recall::bench::dump_sessions()?,
        Some(Commands::Search { query, source, time, project }) => {
            cmd_search(&query, source.as_deref(), time.as_deref(), project.as_deref())?
        }
        Some(Commands::Usage { json, source, time }) => {
            cmd_usage(json, source.as_deref(), time.as_deref())?
        }
        Some(Commands::Export { source, time, project, limit }) => {
            cmd_export(source.as_deref(), time.as_deref(), project.as_deref(), limit)?
        }
        Some(Commands::Import { file, dry_run }) => cmd_import(&file, dry_run)?,
        Some(Commands::Share { command }) => match command {
            ShareCommands::Init { project_name, publish_dir } => {
                cmd_share_init(project_name, publish_dir)?
            }
        },
        Some(Commands::Session { command }) => session::cmd_session(command)?,
        None => cmd_tui(None)?,
    }

    Ok(())
}

fn cmd_info() -> Result<()> {
    let all = adapters::all_adapters();
    let labels = adapters::source_labels();
    let mut config = AppConfig::load_or_default();
    config.normalize_sources(&labels);
    let store = Store::open()?;
    let progress = store.semantic_progress().unwrap_or_default();
    let worker = store.background_job_status("pipeline").unwrap_or_default();

    struct SourceSummary {
        label: String,
        id: String,
        sessions: usize,
        messages: usize,
        range: String,
        error: Option<String>,
    }

    let mut rows = Vec::new();

    let mut grand_sessions = 0usize;
    let mut grand_messages = 0usize;

    for adapter in &all {
        let id = adapter.id();
        let label =
            labels.iter().find(|(k, _)| k == id).map(|(_, v)| v.as_str()).unwrap_or(id).to_string();

        match adapter.scan_summary() {
            Ok(Some(summary)) => {
                grand_sessions += summary.sessions;
                grand_messages += summary.messages;

                rows.push(SourceSummary {
                    label,
                    id: id.to_string(),
                    sessions: summary.sessions,
                    messages: summary.messages,
                    range: format_date_range(summary.oldest_started_at, summary.newest_started_at),
                    error: None,
                });
            }
            Ok(None) => match adapter.scan() {
                Ok(sessions) => {
                    let session_count = sessions.len();
                    let message_count: usize = sessions.iter().map(|s| s.messages.len()).sum();
                    let oldest = sessions.iter().map(|s| s.started_at).min();
                    let newest = sessions.iter().map(|s| s.started_at).max();

                    grand_sessions += session_count;
                    grand_messages += message_count;

                    rows.push(SourceSummary {
                        label,
                        id: id.to_string(),
                        sessions: session_count,
                        messages: message_count,
                        range: format_date_range(oldest, newest),
                        error: None,
                    });
                }
                Err(e) => {
                    rows.push(SourceSummary {
                        label,
                        id: id.to_string(),
                        sessions: 0,
                        messages: 0,
                        range: "-".to_string(),
                        error: Some(e.to_string()),
                    });
                }
            },
            Err(e) => {
                rows.push(SourceSummary {
                    label,
                    id: id.to_string(),
                    sessions: 0,
                    messages: 0,
                    range: "-".to_string(),
                    error: Some(e.to_string()),
                });
            }
        }
    }

    let source_width = rows
        .iter()
        .map(|row| format!("{} ({})", row.label, row.id).len())
        .max()
        .unwrap_or(12)
        .max("Source".len());
    let sessions_width = rows
        .iter()
        .map(|row| row.sessions.to_string().len())
        .max()
        .unwrap_or(1)
        .max("Sessions".len())
        .max(grand_sessions.to_string().len());
    let messages_width = rows
        .iter()
        .map(|row| row.messages.to_string().len())
        .max()
        .unwrap_or(1)
        .max("Messages".len())
        .max(grand_messages.to_string().len());

    println!("Source Scan");
    println!(
        "  {source:<source_width$}  {sessions:>sessions_width$}  {messages:>messages_width$}  Range",
        source = "Source",
        sessions = "Sessions",
        messages = "Messages"
    );
    for row in rows {
        let source = format!("{} ({})", row.label, row.id);
        if let Some(error) = row.error {
            println!(
                "  {source:<source_width$}  {sessions:>sessions_width$}  {messages:>messages_width$}  error: {error}",
                sessions = "-",
                messages = "-"
            );
            continue;
        }
        println!(
            "  {source:<source_width$}  {sessions:>sessions_width$}  {messages:>messages_width$}  {range}",
            sessions = row.sessions,
            messages = row.messages,
            range = row.range
        );
    }
    println!(
        "  {source:<source_width$}  {sessions:>sessions_width$}  {messages:>messages_width$}",
        source = "Total scanned",
        sessions = grand_sessions,
        messages = grand_messages
    );

    println!();
    println!("Settings");
    println!(
        "  Sources     {}",
        labels
            .iter()
            .filter(|(id, _)| config.is_source_enabled(id))
            .map(|(_, label)| label.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!("  Time scope  {}", config.sync_window.label());

    println!();
    println!("Semantic Queue");
    println!("  Indexed DB  {} sessions tracked locally", progress.total_sessions);
    println!(
        "  Progress    {} done, {} pending, {} failed",
        progress.done_sessions,
        progress.pending_sessions + progress.processing_sessions,
        progress.failed_sessions
    );
    if let Some(phase) = worker.phase {
        println!("  Worker      {phase}");
    }

    println!();
    println!("Tip: open the TUI and press Ctrl+S to edit settings.");

    Ok(())
}

fn format_date_range(oldest: Option<i64>, newest: Option<i64>) -> String {
    if oldest.is_none() && newest.is_none() {
        return "-".to_string();
    }

    let oldest = oldest
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "-".to_string());
    let newest = newest
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "-".to_string());

    format!("{oldest} -> {newest}")
}

fn cmd_sync(force: bool, verbose: bool, source_filter: Option<&str>) -> Result<()> {
    let labels = adapters::source_labels();
    let sources = resolve_source_filter(source_filter, &labels)?;
    run_sync_job_inner(SyncRunOptions {
        force,
        verbose,
        emit: true,
        usage_only: false,
        backfill_events: false,
        sources,
    })?;
    semantic::ensure_background_worker(false)?;
    Ok(())
}

fn run_sync_job(force: bool, verbose: bool) -> Result<()> {
    cmd_sync(force, verbose, None)
}

fn run_usage_sync_job() -> Result<()> {
    run_sync_job_inner(SyncRunOptions {
        force: false,
        verbose: false,
        emit: false,
        usage_only: true,
        backfill_events: false,
        sources: None,
    })
}

fn run_dashboard_sync_job() -> Result<()> {
    run_sync_job_inner(SyncRunOptions {
        force: false,
        verbose: false,
        emit: false,
        usage_only: true,
        backfill_events: true,
        sources: None,
    })
}

#[derive(Debug, Clone)]
struct SyncRunOptions {
    force: bool,
    verbose: bool,
    emit: bool,
    usage_only: bool,
    backfill_events: bool,
    sources: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BackfillPlan {
    usage: bool,
    events: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExistingSessionAction {
    Skip,
    BackfillOnly(BackfillPlan),
    RefreshSession,
}

fn run_sync_job_inner(options: SyncRunOptions) -> Result<()> {
    let store = Store::open()?;
    let all = adapters::all_adapters();
    let labels = adapters::source_labels();
    let mut config = AppConfig::load_or_default();
    config.normalize_sources(&labels);
    let since_ts = if options.usage_only { None } else { config.sync_window.to_since_cutoff() };
    // Single matcher across all adapters. None when no rules configured —
    // costs nothing at the loop check site.
    let path_excluder = config.build_path_excluder()?;

    let mut new_sessions = 0u32;
    let mut updated_sessions = 0u32;
    let mut reprocessed_sessions = 0u32;
    let mut total_messages = 0u32;
    let mut skipped = 0u32;
    let mut filtered_out = 0u32;
    let mut excluded_out = 0u32;

    for adapter in &all {
        let source_id = adapter.id();
        let label = adapter.label();

        if options.usage_only
            && !adapters::adapter_supports_usage_dashboard(
                adapter.as_ref(),
                options.backfill_events,
            )
        {
            continue;
        }

        if let Some(sources) = &options.sources
            && !sources.iter().any(|id| id == source_id)
        {
            continue;
        }

        if !config.is_source_enabled(source_id) {
            if options.verbose {
                println!("Skipping {label} (filtered)");
            }
            continue;
        }

        let mut purged_excluded_ids = HashSet::new();
        if let Some(matcher) = &path_excluder {
            excluded_out += delete_excluded_sessions_for_source(
                &store,
                source_id,
                matcher,
                &mut purged_excluded_ids,
            )?;
        }

        if options.verbose {
            println!("Scanning {label}...");
        }
        let include_events = !options.usage_only || options.backfill_events;
        let optimized = if options.force {
            None
        } else {
            match adapter.scan_for_sync(&store, since_ts, include_events) {
                Ok(scan) => scan,
                Err(e) => {
                    if options.emit {
                        eprintln!("Error scanning {label}: {e}");
                    }
                    continue;
                }
            }
        };
        let (raw_sessions, pre_skipped, pre_filtered) = match optimized {
            Some(scan) => {
                (scan.sessions, scan.stats.skipped_sessions, scan.stats.filtered_sessions)
            }
            None => {
                let raw_sessions = match adapter.scan() {
                    Ok(s) => s,
                    Err(e) => {
                        if options.emit {
                            eprintln!("Error scanning {label}: {e}");
                        }
                        continue;
                    }
                };
                (raw_sessions, 0, 0)
            }
        };
        skipped += pre_skipped;
        filtered_out += pre_filtered;
        if let Some(matcher) = &path_excluder {
            excluded_out += delete_excluded_sessions_for_source(
                &store,
                source_id,
                matcher,
                &mut purged_excluded_ids,
            )?;
        }
        if options.verbose {
            println!("  Found {} sessions", raw_sessions.len());
        }

        let mut existing_meta = store.session_meta_map(source_id)?;
        let mut existing_paths = store
            .session_paths_for_source(source_id)?
            .into_iter()
            .map(|path| (path.source_id, (path.directory, path.source_file_path)))
            .collect::<HashMap<_, _>>();
        let mut imported_ids = store.imported_source_ids(source_id)?;
        let mut existing_usage_meta = store.usage_state_meta_map(source_id)?;
        let mut existing_event_meta = if options.usage_only && !options.backfill_events {
            Default::default()
        } else {
            store.event_state_meta_map(source_id)?
        };

        for raw in raw_sessions {
            if let Some(cutoff) = since_ts {
                let ts = raw.updated_at.unwrap_or(raw.started_at);
                if ts < cutoff {
                    filtered_out += 1;
                    continue;
                }
            }

            let raw_source_id = raw.source_id.clone();

            if let Some(matcher) = &path_excluder
                && paths_match_excluded(
                    raw.directory.as_deref(),
                    raw.source_file_path.as_deref(),
                    matcher,
                )
            {
                if existing_meta.remove(&raw_source_id).is_some() {
                    store.delete_session_data(source_id, &raw_source_id)?;
                    existing_paths.remove(&raw_source_id);
                    existing_usage_meta.remove(&raw_source_id);
                    existing_event_meta.remove(&raw_source_id);
                }
                if purged_excluded_ids.insert(raw_source_id) {
                    excluded_out += 1;
                }
                continue;
            }

            let msg_count = raw.messages.len() as u32;
            let usage_backfill_needed = raw.usage_parser_version.is_some_and(|version| {
                !usage_state_is_current(
                    version,
                    existing_usage_meta.get(&raw_source_id).copied(),
                    raw.updated_at,
                )
            });
            let event_backfill_needed = (options.backfill_events || !options.usage_only)
                && raw.event_parser_version.is_some_and(|version| {
                    !event_state_is_current(
                        version,
                        existing_event_meta.get(&raw_source_id).copied(),
                        raw.updated_at,
                    )
                });

            match existing_meta.get(&raw_source_id) {
                Some(&(old_updated_at, old_msg_count)) => {
                    if imported_ids.remove(&raw_source_id) {
                        store.clear_import_marker(source_id, &raw_source_id)?;
                    }
                    let metadata_changed = existing_paths.get(&raw_source_id).is_some_and(
                        |(old_directory, old_source_file_path)| {
                            raw_session_metadata_changed(
                                &raw,
                                old_directory.as_deref(),
                                old_source_file_path.as_deref(),
                            )
                        },
                    );
                    let content_changed = old_msg_count != msg_count
                        || metadata_changed
                        || (raw.updated_at.is_some() && raw.updated_at != old_updated_at);
                    match decide_existing_session_action(
                        options.usage_only,
                        options.backfill_events,
                        options.force,
                        content_changed,
                        usage_backfill_needed,
                        event_backfill_needed,
                    ) {
                        ExistingSessionAction::Skip => {
                            skipped += 1;
                            continue;
                        }
                        ExistingSessionAction::BackfillOnly(plan) => {
                            let mut reprocessed = false;
                            if plan.usage
                                && let Some(parser_version) = raw.usage_parser_version
                                && store.persist_usage_events_for_existing_session(
                                    source_id,
                                    &raw_source_id,
                                    &raw.usage_events,
                                    parser_version,
                                    raw.updated_at,
                                )?
                            {
                                existing_usage_meta.insert(
                                    raw_source_id.clone(),
                                    UsageSessionStateMeta {
                                        parser_version,
                                        source_updated_at: raw.updated_at,
                                    },
                                );
                                reprocessed = true;
                            }
                            if plan.events
                                && let Some(parser_version) = raw.event_parser_version
                                && store.persist_session_events_for_existing_session(
                                    source_id,
                                    &raw_source_id,
                                    &raw.events,
                                    parser_version,
                                    raw.updated_at,
                                )?
                            {
                                existing_event_meta.insert(
                                    raw_source_id.clone(),
                                    EventSessionStateMeta {
                                        parser_version,
                                        source_updated_at: raw.updated_at,
                                    },
                                );
                                reprocessed = true;
                            }
                            if raw.custom_title.is_some()
                                || raw.summary.is_some()
                                || raw.duration_minutes.is_some()
                            {
                                store.update_session_fields(
                                    source_id,
                                    &raw_source_id,
                                    raw.custom_title.as_deref(),
                                    raw.summary.as_deref(),
                                    raw.duration_minutes,
                                    None,
                                )?;
                            }
                            if reprocessed {
                                reprocessed_sessions += 1;
                            }
                            continue;
                        }
                        ExistingSessionAction::RefreshSession => {}
                    }
                    store.delete_session_data(source_id, &raw_source_id)?;
                    existing_event_meta.remove(&raw_source_id);
                    if content_changed {
                        updated_sessions += 1;
                    } else {
                        reprocessed_sessions += 1;
                    }
                }
                None => {
                    new_sessions += 1;
                }
            }

            let session_uuid = uuid::Uuid::new_v4().to_string();
            let title = raw
                .custom_title
                .clone()
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| generate_title(&raw.messages));

            let session = Session {
                id: session_uuid.clone(),
                source: source_id.to_string(),
                source_id: raw.source_id,
                title,
                directory: raw.directory,
                started_at: raw.started_at,
                updated_at: raw.updated_at,
                message_count: msg_count,
                entrypoint: raw.entrypoint,
                custom_title: raw.custom_title,
                summary: raw.summary,
                duration_minutes: raw.duration_minutes,
                source_file_path: raw.source_file_path,
                is_import: false,
            };

            let messages: Vec<Message> = raw
                .messages
                .into_iter()
                .enumerate()
                .map(|(i, m)| Message {
                    session_id: session_uuid.clone(),
                    role: m.role,
                    content: m.content,
                    timestamp: m.timestamp,
                    seq: i as u32,
                })
                .collect();

            let persist_events = !options.usage_only || options.backfill_events;
            let (events, event_parser_version) = if persist_events {
                (raw.events, raw.event_parser_version)
            } else {
                (Vec::new(), None)
            };

            store.persist_session_with_usage_and_events(
                &session,
                &messages,
                &raw.usage_events,
                raw.usage_parser_version,
                &events,
                event_parser_version,
            )?;
            existing_meta
                .insert(session.source_id.clone(), (session.updated_at, session.message_count));
            existing_paths.insert(
                session.source_id.clone(),
                (session.directory.clone(), session.source_file_path.clone()),
            );
            if let Some(parser_version) = raw.usage_parser_version {
                existing_usage_meta.insert(
                    session.source_id.clone(),
                    UsageSessionStateMeta { parser_version, source_updated_at: session.updated_at },
                );
            }
            if let Some(parser_version) = event_parser_version {
                existing_event_meta.insert(
                    session.source_id.clone(),
                    EventSessionStateMeta { parser_version, source_updated_at: session.updated_at },
                );
            }
            total_messages += msg_count;
        }

        info!("{label} done");
    }

    let touched = new_sessions + updated_sessions + reprocessed_sessions;

    if options.verbose {
        println!();
        if options.force {
            print!(
                "Force sync: {new_sessions} new, {updated_sessions} updated, {reprocessed_sessions} reprocessed, {total_messages} messages"
            );
        } else {
            print!(
                "Sync: {new_sessions} new, {updated_sessions} updated, {skipped} unchanged, {total_messages} messages"
            );
        }
        if filtered_out > 0 {
            print!(", {filtered_out} outside configured time scope");
        }
        if excluded_out > 0 {
            print!(", {excluded_out} excluded by excluded_paths");
        }
        println!();
        println!(
            "Settings: sources [{}], time scope [{}]",
            labels
                .iter()
                .filter(|(id, _)| config.is_source_enabled(id))
                .map(|(_, label)| label.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            config.sync_window.label()
        );
        let progress = store.semantic_progress()?;
        if progress.total_sessions > 0 {
            println!(
                "Semantic queue: {}/{} done, {} pending, {} failed",
                progress.done_sessions,
                progress.total_sessions,
                progress.pending_sessions + progress.processing_sessions,
                progress.failed_sessions
            );
        }
    } else if !options.emit {
    } else if options.force {
        println!("Reprocessed {touched} sessions, {total_messages} messages");
    } else if touched == 0 {
        println!("Up to date.");
    } else {
        println!("{new_sessions} new, {updated_sessions} updated, {total_messages} messages");
    }

    Ok(())
}

fn usage_state_is_current(
    required_parser_version: u32,
    state: Option<UsageSessionStateMeta>,
    source_updated_at: Option<i64>,
) -> bool {
    state.is_some_and(|state| {
        state.parser_version >= required_parser_version
            && state.source_updated_at == source_updated_at
    })
}

fn event_state_is_current(
    required_parser_version: u32,
    state: Option<EventSessionStateMeta>,
    source_updated_at: Option<i64>,
) -> bool {
    state.is_some_and(|state| {
        state.parser_version >= required_parser_version
            && state.source_updated_at == source_updated_at
    })
}

fn decide_existing_session_action(
    usage_only: bool,
    backfill_events: bool,
    force: bool,
    content_changed: bool,
    usage_backfill_needed: bool,
    event_backfill_needed: bool,
) -> ExistingSessionAction {
    if usage_only {
        let needs_usage = usage_backfill_needed;
        let needs_events = backfill_events && event_backfill_needed;
        return if needs_usage || needs_events {
            ExistingSessionAction::BackfillOnly(BackfillPlan {
                usage: needs_usage,
                events: needs_events,
            })
        } else {
            ExistingSessionAction::Skip
        };
    }

    if !content_changed && !force {
        return if usage_backfill_needed || event_backfill_needed {
            ExistingSessionAction::BackfillOnly(BackfillPlan {
                usage: usage_backfill_needed,
                events: event_backfill_needed,
            })
        } else {
            ExistingSessionAction::Skip
        };
    }

    ExistingSessionAction::RefreshSession
}

fn cmd_background_worker(sync_first: bool) -> Result<()> {
    semantic::run_background_worker(sync_first, || run_sync_job(false, false))
}

fn raw_session_metadata_changed(
    raw: &adapters::RawSession,
    old_directory: Option<&str>,
    old_source_file_path: Option<&str>,
) -> bool {
    raw.directory.as_deref().is_some_and(|directory| old_directory != Some(directory))
        || raw.source_file_path.as_deref().is_some_and(|path| old_source_file_path != Some(path))
}

fn cmd_search(
    query: &str,
    source_filter: Option<&str>,
    time_filter: Option<&str>,
    project_filter: Option<&str>,
) -> Result<()> {
    let store = Store::open()?;
    let engine = SearchEngine::new(&store.conn);
    let sources = adapters::source_labels();
    let progress = store.semantic_progress().unwrap_or_default();

    let query_embedding = if progress.done_sessions > 0 || progress.processing_sessions > 0 {
        println!("Loading embedding model...");
        match EmbeddingProvider::new(true) {
            Ok(provider) => provider
                .embed_query(&[query])?
                .into_iter()
                .next()
                .map(Some)
                .ok_or_else(|| anyhow::anyhow!("failed to generate query embedding"))?,
            Err(e) => {
                println!("Semantic unavailable: {e}");
                None
            }
        }
    } else {
        None
    };

    let resolved_source = source_filter.and_then(|s| {
        let lower = s.to_lowercase();
        sources
            .iter()
            .find(|(id, label)| id == &lower || label.to_lowercase() == lower)
            .map(|(id, _)| vec![id.clone()])
    });

    let time_range = match time_filter.map(|t| t.to_lowercase()) {
        Some(ref t) if t == "today" => TimeRange::Today,
        Some(ref t) if t == "7d" || t == "week" => TimeRange::Week,
        Some(ref t) if t == "30d" || t == "month" => TimeRange::Month,
        _ => TimeRange::All,
    };

    let filters = SearchFilters {
        sources: resolved_source,
        time_range,
        directory: project_filter.map(String::from),
    };

    let results = engine.hybrid_search(query, query_embedding.as_deref(), &filters, 20, 3)?;

    if results.is_empty() {
        println!("No results found.");
        return Ok(());
    }

    for (i, result) in results.iter().enumerate() {
        let s = &result.session;
        let age = utils::format_age(s.started_at);
        let dir = s.directory.as_deref().unwrap_or("-");
        let source_label = sources
            .iter()
            .find(|(id, _)| id == &s.source)
            .map(|(_, l)| l.as_str())
            .unwrap_or(&s.source);
        let match_label = match result.match_source {
            types::MatchSource::Fts => "FTS",
            types::MatchSource::Vector => "VEC",
            types::MatchSource::Hybrid => "HYB",
        };
        println!("{:>2}. [{source_label}] [{match_label}] {age:>5}  {}", i + 1, s.title);
        if let Some(snippet) = &result.snippet {
            let short: String = snippet.chars().take(120).collect();
            println!("    {short}");
        }
        println!("    dir: {dir}");
        println!();
    }

    Ok(())
}

fn cmd_usage(json: bool, source_filter: Option<&str>, time_filter: Option<&str>) -> Result<()> {
    let sources = usage_source_labels();

    if !json {
        let usage_source_filter = resolve_source_filter(source_filter, &sources)?;
        let usage_time_filter = time_filter.map(|_| parse_time_range(time_filter));
        return cmd_tui(Some((usage_source_filter, usage_time_filter)));
    }

    run_usage_sync_job()?;
    let store = Store::open()?;
    let filters = UsageFilters {
        sources: resolve_source_filter(source_filter, &sources)?,
        time_range: parse_time_range(time_filter),
    };
    let report = usage::build_usage_report(&store, &filters)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    print!("{}", format_usage_report_text(&report));

    Ok(())
}

fn cmd_export(
    source_filter: Option<&str>,
    time_filter: Option<&str>,
    project_filter: Option<&str>,
    limit: usize,
) -> Result<()> {
    let store = Store::open()?;
    let sources = adapters::source_labels();
    let options = ExportOptions {
        session_ids: Vec::new(),
        sources: resolve_source_filter(source_filter, &sources)?,
        time_range: parse_time_range(time_filter),
        project: project_filter.map(String::from),
        limit: if limit == 0 { None } else { Some(limit) },
    };
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    recall::export::write_jsonl(&store, &options, &mut handle)
}

fn cmd_import(file: &str, dry_run: bool) -> Result<()> {
    let store = Store::open()?;
    let summary = if file == "-" {
        let stdin = std::io::stdin();
        recall::import::import_jsonl(&store, dry_run, stdin.lock())?
    } else {
        let f =
            std::fs::File::open(file).map_err(|e| anyhow::anyhow!("cannot open {file}: {e}"))?;
        recall::import::import_jsonl(&store, dry_run, std::io::BufReader::new(f))?
    };

    let suffix = if dry_run { " (dry-run, nothing written)" } else { "" };
    println!(
        "total {} | imported {} | skipped {}{suffix}",
        summary.total, summary.imported, summary.skipped
    );
    Ok(())
}

fn cmd_share_init(project_name: Option<String>, publish_dir: Option<PathBuf>) -> Result<()> {
    let mut config = AppConfig::load_or_default();
    let existing = config.share.clone();
    if let Some(ref share) = existing {
        println!("Share already initialized");
        println!("  Provider     {}", share.provider);
        println!("  Project      {}", share.project_name);
        println!("  Publish dir  {}", share.publish_dir);
        println!("  URL base     https://{}", share.project_domain);
        if !prompt_yes_no_default_yes("Reinitialize?")? {
            return Ok(());
        }
    }

    println!("Checking Wrangler and Cloudflare Pages...");
    recall::share::preflight_cloudflare_pages()?;

    let default_project = existing
        .as_ref()
        .map(|share| share.project_name.clone())
        .unwrap_or_else(recall::share::default_project_name);
    let project_name = match project_name {
        Some(name) => name,
        None => prompt_with_default("Cloudflare Pages project", &default_project)?,
    };

    let default_dir =
        existing.as_ref().map(|share| share.publish_dir.clone()).unwrap_or_else(|| {
            recall::share::default_publish_dir()
                .map(|path| path.to_string_lossy().to_string())
                .unwrap_or_else(|_| "share-pages".to_string())
        });
    let publish_dir = match publish_dir {
        Some(path) => recall::share::expand_path(&path.to_string_lossy()),
        None => {
            let input = prompt_with_default("Local share directory", &default_dir)?;
            recall::share::expand_path(&input)
        }
    };

    println!("Configuring Cloudflare Pages share target...");
    recall::share::init_cloudflare_pages(&mut config, project_name.clone(), publish_dir.clone())?;
    println!("Share initialized");
    println!("  Project      {project_name}");
    println!("  Publish dir  {}", publish_dir.display());
    if let Some(share) = config.share.as_ref() {
        println!("  URL base     https://{}", share.project_domain);
    }
    Ok(())
}

fn prompt_with_default(label: &str, default: &str) -> Result<String> {
    print!("{label} [{default}]: ");
    std::io::stdout().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let trimmed = input.trim();
    if trimmed.is_empty() { Ok(default.to_string()) } else { Ok(trimmed.to_string()) }
}

fn prompt_yes_no_default_yes(label: &str) -> Result<bool> {
    print!("{label} [Y/n]: ");
    std::io::stdout().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let trimmed = input.trim().to_lowercase();
    Ok(trimmed.is_empty() || trimmed == "y" || trimmed == "yes")
}

fn format_usage_report_text(report: &usage::UsageReport) -> String {
    let mut out = String::new();
    writeln!(out, "Usage").unwrap();
    writeln!(out, "  Total tokens  {}", format_usage_number(report.summary.tokens.total_tokens))
        .unwrap();
    writeln!(out, "  Sessions      {}", report.summary.sessions).unwrap();

    if !report.by_source.is_empty() {
        writeln!(out).unwrap();
        writeln!(out, "By source").unwrap();
        for source in report.by_source.iter().take(10) {
            writeln!(
                out,
                "  {:<14} {:>12} tokens  {:>4} sessions",
                source.source,
                format_usage_number(source.tokens.total_tokens),
                source.sessions
            )
            .unwrap();
        }
    }

    if !report.by_model.is_empty() {
        writeln!(out).unwrap();
        writeln!(out, "By model").unwrap();
        for model in report.by_model.iter().take(10) {
            writeln!(
                out,
                "  {:<12} {:<24} {:>12} tokens",
                model.source,
                truncate_usage_label(&model.model, 24),
                format_usage_number(model.tokens.total_tokens)
            )
            .unwrap();
        }
    }

    out
}

fn resolve_source_filter(
    source_filter: Option<&str>,
    sources: &[(String, String)],
) -> Result<Option<Vec<String>>> {
    let Some(source) = source_filter else {
        return Ok(None);
    };
    let lower = source.to_lowercase();
    let resolved = sources
        .iter()
        .find(|(id, label)| id == &lower || label.to_lowercase() == lower)
        .map(|(id, _)| id.clone())
        .ok_or_else(|| anyhow::anyhow!("unknown source: {source}"))?;
    Ok(Some(vec![resolved]))
}

fn usage_source_labels() -> Vec<(String, String)> {
    adapters::all_adapters()
        .into_iter()
        .filter(|adapter| adapter.usage_parser_version().is_some())
        .map(|adapter| (adapter.id().to_string(), adapter.label().to_string()))
        .collect()
}

fn parse_time_range(time_filter: Option<&str>) -> TimeRange {
    match time_filter.map(|t| t.to_lowercase()) {
        Some(ref t) if t == "today" => TimeRange::Today,
        Some(ref t) if t == "7d" || t == "week" => TimeRange::Week,
        Some(ref t) if t == "30d" || t == "month" => TimeRange::Month,
        _ => TimeRange::All,
    }
}

fn format_usage_number(value: i64) -> String {
    let abs = value.abs();
    if abs >= 1_000_000_000 {
        return format_compact_decimal(value as f64 / 1_000_000_000.0, "B");
    }
    if abs >= 1_000_000 {
        return format_compact_decimal(value as f64 / 1_000_000.0, "M");
    }

    let s = value.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

fn format_compact_decimal(value: f64, suffix: &str) -> String {
    let formatted = format!("{value:.2}");
    let trimmed = formatted.trim_end_matches('0').trim_end_matches('.').to_string();
    format!("{trimmed}{suffix}")
}

fn truncate_usage_label(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let suffix = "...";
    let keep = max_chars.saturating_sub(suffix.len());
    let mut out = value.chars().take(keep).collect::<String>();
    out.push_str(suffix);
    out
}
fn cmd_tui(usage_start: Option<(Option<Vec<String>>, Option<TimeRange>)>) -> Result<()> {
    use std::io;
    use std::time::Duration;

    use crossterm::execute;
    use crossterm::terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    };
    use ratatui::Terminal;
    use ratatui::backend::CrosstermBackend;

    use recall::tui::app::{App, AppMode};
    use recall::tui::event::{AppEvent, poll_event};
    use recall::tui::ui;

    let usage_mode = usage_start.is_some();
    let store = Store::open()?;
    semantic::ensure_background_worker(true)?;
    let sources = if usage_start.is_some() {
        adapters::dashboard_source_labels()
    } else {
        adapters::source_labels()
    };

    struct TerminalGuard;
    impl Drop for TerminalGuard {
        fn drop(&mut self) {
            let _ = disable_raw_mode();
            let _ =
                execute!(io::stdout(), crossterm::event::DisableMouseCapture, LeaveAlternateScreen);
        }
    }

    enable_raw_mode()?;
    let _guard = TerminalGuard;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, crossterm::event::EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let engine = SearchEngine::new(&store.conn);
    let mut provider: Option<EmbeddingProvider> = None;
    let mut config = AppConfig::load_or_default();
    config.normalize_sources(&sources);

    let mut app = App::new(&store, sources, config);
    if let Some((source_filter, time_filter)) = usage_start {
        app.source_filter_selection = source_filter.unwrap_or_default();
        if let Some(time_filter) = time_filter {
            app.usage_time_filter = time_filter;
        }
        app.mode = AppMode::Usage;
        app.request_usage_refresh();
    }
    let mut usage_sync_pending = usage_mode;
    let tick_rate = Duration::from_millis(50);

    loop {
        app.poll_share_publish();
        terminal.draw(|f| ui::render(f, &app))?;

        match poll_event(tick_rate)? {
            AppEvent::Key(key) => {
                app.handle_key(key, &store, &engine, &mut provider);
            }
            AppEvent::ScrollUp => app.handle_scroll_up(&store),
            AppEvent::ScrollDown => app.handle_scroll_down(&store),
            AppEvent::Tick => {}
        }

        if app.should_quit {
            break;
        }

        if app.usage_refresh_is_due() {
            if usage_sync_pending {
                usage_sync_pending = false;
                match run_dashboard_sync_job() {
                    Ok(()) => app.refresh_usage(&store),
                    Err(err) => app.fail_usage_refresh(err),
                }
            } else {
                app.refresh_usage(&store);
            }
        }

        app.try_search(&store, &engine, &mut provider);

        if app.should_quit {
            break;
        }
    }

    drop(_guard);
    terminal.show_cursor()?;

    if let Some((command, cwd)) = app.exec_on_exit.take() {
        exec_resume(command, cwd)?;
    }

    Ok(())
}

#[cfg(unix)]
fn exec_resume(command: adapters::ResumeCommand, cwd: Option<String>) -> Result<()> {
    use std::os::unix::process::CommandExt;

    let mut cmd = std::process::Command::new(&command.program);
    cmd.args(&command.args);
    if let Some(ref dir) = cwd
        && std::path::Path::new(dir).is_dir()
    {
        cmd.current_dir(dir);
    }
    let err = cmd.exec();
    Err(anyhow::anyhow!("failed to exec {}: {err}", command.program))
}

#[cfg(not(unix))]
fn exec_resume(command: adapters::ResumeCommand, cwd: Option<String>) -> Result<()> {
    let mut cmd = std::process::Command::new(&command.program);
    cmd.args(&command.args);
    if let Some(ref dir) = cwd
        && std::path::Path::new(dir).is_dir()
    {
        cmd.current_dir(dir);
    }
    let status =
        cmd.status().map_err(|e| anyhow::anyhow!("failed to run {}: {e}", command.program))?;
    std::process::exit(status.code().unwrap_or(0));
}

fn generate_title(messages: &[adapters::RawMessage]) -> String {
    let user_contents: Vec<&str> =
        messages.iter().filter(|m| m.role == Role::User).map(|m| m.content.as_str()).collect();
    utils::title_from_user_messages(&user_contents)
}

fn delete_excluded_sessions_for_source(
    store: &Store,
    source_id: &str,
    matcher: &globset::GlobSet,
    deleted: &mut HashSet<String>,
) -> Result<u32> {
    let mut count = 0;
    for path in store.session_paths_for_source(source_id)? {
        if paths_match_excluded(
            path.directory.as_deref(),
            path.source_file_path.as_deref(),
            matcher,
        ) {
            let source_id_to_delete = path.source_id;
            store.delete_session_data(source_id, &source_id_to_delete)?;
            if deleted.insert(source_id_to_delete) {
                count += 1;
            }
        }
    }
    Ok(count)
}

fn paths_match_excluded(
    directory: Option<&str>,
    source_file_path: Option<&str>,
    matcher: &globset::GlobSet,
) -> bool {
    directory.is_some_and(|path| matcher.is_match(path))
        || source_file_path.is_some_and(|path| path_or_ancestor_matches(path, matcher))
}

fn path_or_ancestor_matches(path: &str, matcher: &globset::GlobSet) -> bool {
    let path = std::path::Path::new(path);
    path.ancestors().any(|candidate| matcher.is_match(candidate))
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{
        BackfillPlan, Cli, Commands, ExistingSessionAction, ShareCommands,
        decide_existing_session_action, delete_excluded_sessions_for_source,
        raw_session_metadata_changed,
    };
    use clap::{CommandFactory, Parser};
    use recall::adapters::{
        RawSession, adapter_supports_usage_dashboard, all_adapters, source_supports_event_backfill,
    };
    use recall::db::{schema, store::Store};
    use recall::types::Session;

    fn matcher(pattern: &str) -> globset::GlobSet {
        let mut builder = globset::GlobSetBuilder::new();
        builder.add(globset::Glob::new(pattern).unwrap());
        builder.build().unwrap()
    }

    fn session(id: &str, source: &str, source_id: &str) -> Session {
        Session {
            id: id.to_string(),
            source: source.to_string(),
            source_id: source_id.to_string(),
            title: "t".to_string(),
            directory: None,
            started_at: 0,
            updated_at: Some(1),
            message_count: 0,
            entrypoint: None,
            custom_title: None,
            summary: None,
            duration_minutes: None,
            source_file_path: None,
            is_import: false,
        }
    }

    #[test]
    fn export_accepts_default_jsonl_without_format_flag() {
        let cli = Cli::try_parse_from(["recall", "export", "--source", "grok"]).unwrap();
        match cli.command {
            Some(Commands::Export { source, .. }) => {
                assert_eq!(source.as_deref(), Some("grok"));
            }
            _ => panic!("expected export command"),
        }
    }

    #[test]
    fn export_rejects_removed_jsonl_flag() {
        assert!(Cli::try_parse_from(["recall", "export", "--jsonl"]).is_err());
    }

    #[test]
    fn share_init_accepts_project_and_publish_dir() {
        let cli = Cli::try_parse_from([
            "recall",
            "share",
            "init",
            "--project-name",
            "recall-share",
            "--publish-dir",
            "/tmp/recall-share",
        ])
        .unwrap();
        match cli.command {
            Some(Commands::Share {
                command: ShareCommands::Init { project_name, publish_dir },
            }) => {
                assert_eq!(project_name.as_deref(), Some("recall-share"));
                assert_eq!(publish_dir.unwrap().to_string_lossy(), "/tmp/recall-share");
            }
            _ => panic!("expected share init command"),
        }
    }

    #[test]
    fn top_level_help_describes_public_commands() {
        let mut command = Cli::command();
        let help = command.render_long_help().to_string();
        let compact_help = help.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(!help.contains("--jsonl"));
        assert!(compact_help.contains("info Show indexed source and background job status"));
        assert!(compact_help.contains("sync Scan configured AI coding session sources"));
        assert!(compact_help.contains("search Search indexed coding sessions"));
        assert!(compact_help.contains("usage Show token usage reports"));
        assert!(compact_help.contains("export Export session records as JSON Lines"));
        assert!(compact_help.contains("import Import session records from JSON Lines"));
        assert!(compact_help.contains("share Share session pages"));
        assert!(compact_help.contains("session Operate on indexed sessions"));
    }

    #[test]
    fn public_subcommand_help_describes_arguments_and_options() {
        for subcommand in ["search", "usage", "export", "import"] {
            let mut command = Cli::command();
            let command = command.find_subcommand_mut(subcommand).unwrap();
            let help = command.render_long_help().to_string();
            assert!(!help.contains("<SOURCE>    "), "{subcommand} source help missing");
            assert!(!help.contains("<TIME>        "), "{subcommand} time help missing");
            assert!(!help.contains("<QUERY>  "), "{subcommand} query help missing");
            assert!(!help.contains("<FILE>  \n"), "{subcommand} file help missing");
        }
    }

    #[test]
    fn dashboard_sync_skips_sources_without_usage_or_events() {
        for adapter in all_adapters() {
            let id = adapter.id();
            if matches!(id, "cline" | "antigravity-cli" | "kiro-cli") {
                assert!(
                    !adapter_supports_usage_dashboard(adapter.as_ref(), true),
                    "{id} should be skipped during dashboard sync"
                );
            }
            if source_supports_event_backfill(id) {
                assert!(adapter_supports_usage_dashboard(adapter.as_ref(), true));
            }
        }
    }

    #[test]
    fn usage_only_never_refreshes_existing_session() {
        assert_eq!(
            decide_existing_session_action(true, false, false, true, true, true),
            ExistingSessionAction::BackfillOnly(BackfillPlan { usage: true, events: false })
        );
        assert_eq!(
            decide_existing_session_action(true, false, false, true, false, true),
            ExistingSessionAction::Skip
        );
    }

    #[test]
    fn usage_only_can_backfill_events_without_refresh() {
        assert_eq!(
            decide_existing_session_action(true, true, false, true, false, true),
            ExistingSessionAction::BackfillOnly(BackfillPlan { usage: false, events: true })
        );
        assert_eq!(
            decide_existing_session_action(true, true, false, true, true, true),
            ExistingSessionAction::BackfillOnly(BackfillPlan { usage: true, events: true })
        );
    }

    #[test]
    fn full_sync_refreshes_changed_existing_session() {
        assert_eq!(
            decide_existing_session_action(false, false, false, true, true, true),
            ExistingSessionAction::RefreshSession
        );
    }

    #[test]
    fn full_sync_backfills_unchanged_existing_session_in_place() {
        assert_eq!(
            decide_existing_session_action(false, false, false, false, true, true),
            ExistingSessionAction::BackfillOnly(BackfillPlan { usage: true, events: true })
        );
        assert_eq!(
            decide_existing_session_action(false, false, false, false, false, false),
            ExistingSessionAction::Skip
        );
    }

    #[test]
    fn full_sync_treats_new_session_metadata_as_changed() {
        let raw = RawSession::search_only(
            "raw1",
            Some("/Users/x/git/samzong/Recall".to_string()),
            0,
            Some(1),
            None,
            vec![],
        );
        assert!(raw_session_metadata_changed(&raw, None, None));
        assert!(!raw_session_metadata_changed(&raw, Some("/Users/x/git/samzong/Recall"), None));

        let mut raw_with_path = RawSession::search_only("raw1", None, 0, Some(1), None, vec![]);
        raw_with_path.source_file_path = Some("/tmp/session.jsonl".to_string());
        assert!(raw_session_metadata_changed(&raw_with_path, None, None));
    }

    #[test]
    fn delete_excluded_sessions_for_source_uses_persisted_source_file_path() {
        schema::register_sqlite_vec();
        let matcher = matcher("**/observer-sessions");
        let store = Store::open_in_memory().unwrap();
        store.insert_session(&session("id-1", "claude-code", "s1")).unwrap();
        store
            .update_session_fields(
                "claude-code",
                "s1",
                None,
                None,
                None,
                Some("/tmp/observer-sessions/session.jsonl"),
            )
            .unwrap();

        let mut deleted = HashSet::new();
        let count =
            delete_excluded_sessions_for_source(&store, "claude-code", &matcher, &mut deleted)
                .unwrap();

        assert_eq!(count, 1);
        assert!(deleted.contains("s1"));
        assert!(store.session_paths_for_source("claude-code").unwrap().is_empty());
    }
}
