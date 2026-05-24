use std::fmt::Write as _;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::info;

use recall::adapters;
use recall::config::AppConfig;
use recall::db;
use recall::db::search::{SearchEngine, SearchFilters, TimeRange};
use recall::db::store::{Store, UsageSessionStateMeta};
use recall::embedding::EmbeddingProvider;
use recall::semantic;
use recall::types::{self, Message, Role, Session};
use recall::usage::{self, UsageFilters};
use recall::utils;

#[derive(Parser)]
#[command(name = "recall", version, about = "Search and recall AI coding sessions")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    Info,
    Sync {
        #[arg(long, help = "Reprocess every session, even if unchanged")]
        force: bool,
        #[arg(short, long, help = "Show per-source scan progress and settings")]
        verbose: bool,
    },
    #[command(hide = true, name = "__background-worker")]
    BackgroundWorker {
        #[arg(long)]
        sync_first: bool,
    },
    #[command(hide = true, name = "__bench-semantic")]
    BenchSemantic,
    #[command(hide = true, name = "__bench-search")]
    BenchSearch {
        query: String,
    },
    #[command(hide = true, name = "__bench-eval")]
    BenchEval {
        #[arg(long)]
        dataset: Option<String>,
        #[arg(short, long)]
        verbose: bool,
    },
    #[command(hide = true, name = "__bench-dump-sessions")]
    BenchDumpSessions,
    Search {
        query: String,
        #[arg(long)]
        source: Option<String>,
        #[arg(long)]
        time: Option<String>,
    },
    Usage {
        #[arg(long, help = "Output usage report as JSON")]
        json: bool,
        #[arg(long)]
        source: Option<String>,
        #[arg(long)]
        time: Option<String>,
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
        Some(Commands::Sync { force, verbose }) => cmd_sync(force, verbose)?,
        Some(Commands::BackgroundWorker { sync_first }) => cmd_background_worker(sync_first)?,
        Some(Commands::BenchSemantic) => recall::bench::run_semantic()?,
        Some(Commands::BenchSearch { query }) => recall::bench::run_search(&query)?,
        Some(Commands::BenchEval { dataset, verbose }) => {
            recall::bench::run_eval(dataset.as_deref(), verbose)?
        }
        Some(Commands::BenchDumpSessions) => recall::bench::dump_sessions()?,
        Some(Commands::Search { query, source, time }) => {
            cmd_search(&query, source.as_deref(), time.as_deref())?
        }
        Some(Commands::Usage { json, source, time }) => {
            cmd_usage(json, source.as_deref(), time.as_deref())?
        }
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

fn cmd_sync(force: bool, verbose: bool) -> Result<()> {
    run_sync_job(force, verbose)?;
    semantic::ensure_background_worker(false)?;
    Ok(())
}

fn run_sync_job(force: bool, verbose: bool) -> Result<()> {
    run_sync_job_inner(SyncRunOptions { force, verbose, emit: true, usage_only: false })
}

fn run_usage_sync_job() -> Result<()> {
    run_sync_job_inner(SyncRunOptions {
        force: false,
        verbose: false,
        emit: false,
        usage_only: true,
    })
}

#[derive(Debug, Clone, Copy)]
struct SyncRunOptions {
    force: bool,
    verbose: bool,
    emit: bool,
    usage_only: bool,
}

fn run_sync_job_inner(options: SyncRunOptions) -> Result<()> {
    let store = Store::open()?;
    let all = adapters::all_adapters();
    let labels = adapters::source_labels();
    let mut config = AppConfig::load_or_default();
    config.normalize_sources(&labels);
    let since_ts = if options.usage_only { None } else { config.sync_window.to_since_cutoff() };

    let mut new_sessions = 0u32;
    let mut updated_sessions = 0u32;
    let mut reprocessed_sessions = 0u32;
    let mut total_messages = 0u32;
    let mut skipped = 0u32;
    let mut filtered_out = 0u32;

    for adapter in &all {
        let source_id = adapter.id();
        let label = adapter.label();

        if options.usage_only && adapter.usage_parser_version().is_none() {
            continue;
        }

        if !config.is_source_enabled(source_id) {
            if options.verbose {
                println!("Skipping {label} (filtered)");
            }
            continue;
        }

        if options.verbose {
            println!("Scanning {label}...");
        }
        let optimized = if options.force {
            None
        } else {
            match adapter.scan_for_sync(&store, since_ts) {
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
        if options.verbose {
            println!("  Found {} sessions", raw_sessions.len());
        }

        let mut existing_meta = store.session_meta_map(source_id)?;
        let mut existing_usage_meta = store.usage_state_meta_map(source_id)?;

        for raw in raw_sessions {
            if let Some(cutoff) = since_ts {
                let ts = raw.updated_at.unwrap_or(raw.started_at);
                if ts < cutoff {
                    filtered_out += 1;
                    continue;
                }
            }

            let msg_count = raw.messages.len() as u32;
            let usage_backfill_needed = raw.usage_parser_version.is_some_and(|version| {
                !usage_state_is_current(
                    version,
                    existing_usage_meta.get(&raw.source_id).copied(),
                    raw.updated_at,
                )
            });

            match existing_meta.get(&raw.source_id) {
                Some(&(old_updated_at, old_msg_count)) => {
                    let content_changed = old_msg_count != msg_count
                        || (raw.updated_at.is_some() && raw.updated_at != old_updated_at);
                    if !content_changed && usage_backfill_needed && !options.force {
                        if let Some(parser_version) = raw.usage_parser_version
                            && store.persist_usage_events_for_existing_session(
                                source_id,
                                &raw.source_id,
                                &raw.usage_events,
                                parser_version,
                                raw.updated_at,
                            )?
                        {
                            existing_usage_meta.insert(
                                raw.source_id.clone(),
                                UsageSessionStateMeta {
                                    parser_version,
                                    source_updated_at: raw.updated_at,
                                },
                            );
                            reprocessed_sessions += 1;
                        }
                        continue;
                    }
                    if !content_changed && !options.force {
                        skipped += 1;
                        continue;
                    }
                    store.delete_session_data(source_id, &raw.source_id)?;
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
            let title = generate_title(&raw.messages);

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

            store.persist_session_with_usage(
                &session,
                &messages,
                &raw.usage_events,
                raw.usage_parser_version,
            )?;
            existing_meta
                .insert(session.source_id.clone(), (session.updated_at, session.message_count));
            if let Some(parser_version) = raw.usage_parser_version {
                existing_usage_meta.insert(
                    session.source_id.clone(),
                    UsageSessionStateMeta { parser_version, source_updated_at: session.updated_at },
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

fn cmd_background_worker(sync_first: bool) -> Result<()> {
    semantic::run_background_worker(sync_first, || run_sync_job(false, false))
}

fn cmd_search(query: &str, source_filter: Option<&str>, time_filter: Option<&str>) -> Result<()> {
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

    let filters = SearchFilters { sources: resolved_source, time_range, directory: None };

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
    ["codex", "claude-code", "opencode"]
        .into_iter()
        .map(|source| (source.to_string(), source.to_string()))
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
    let sources =
        if usage_start.is_some() { usage_source_labels() } else { adapters::source_labels() };

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
                match run_usage_sync_job() {
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
