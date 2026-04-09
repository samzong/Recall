use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::info;

use recall::adapters;
use recall::config::AppConfig;
use recall::db;
use recall::db::search::{SearchEngine, SearchFilters, TimeRange};
use recall::db::store::Store;
use recall::embedding::EmbeddingProvider;
use recall::semantic;
use recall::types::{self, Message, Role, Session};
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
    Index,
    Sync,
    Search {
        query: String,
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
        Some(Commands::Index) => cmd_index(false)?,
        Some(Commands::Sync) => cmd_index(true)?,
        Some(Commands::Search { query, source, time }) => {
            cmd_search(&query, source.as_deref(), time.as_deref())?
        }
        None => cmd_tui()?,
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

        match adapter.scan() {
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
    println!(
        "  Build       {}",
        if EmbeddingProvider::is_available() { "enabled" } else { "disabled (FTS-only binary)" }
    );
    println!("  Indexed DB  {} sessions tracked locally", progress.total_sessions);
    println!(
        "  Progress    {} done, {} pending, {} failed",
        progress.done_sessions,
        progress.pending_sessions + progress.processing_sessions,
        progress.failed_sessions
    );
    if let Some(title) = progress.current_session_title {
        println!("  Active      {title}");
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

fn cmd_index(incremental: bool) -> Result<()> {
    let store = Store::open()?;
    let all = adapters::all_adapters();
    let labels = adapters::source_labels();
    let mut config = AppConfig::load_or_default();
    config.normalize_sources(&labels);
    let since_ts = config.sync_window.to_since_cutoff();

    let mut new_sessions = 0u32;
    let mut updated_sessions = 0u32;
    let mut total_messages = 0u32;
    let mut skipped = 0u32;
    let mut filtered_out = 0u32;

    for adapter in &all {
        let source_id = adapter.id();
        let label = adapter.label();

        if !config.is_source_enabled(source_id) {
            println!("Skipping {label} (filtered)");
            continue;
        }

        println!("Scanning {label}...");
        let raw_sessions = match adapter.scan() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("  Error scanning {label}: {e}");
                continue;
            }
        };
        println!("  Found {} sessions", raw_sessions.len());

        for raw in raw_sessions {
            if let Some(cutoff) = since_ts {
                let ts = raw.updated_at.unwrap_or(raw.started_at);
                if ts < cutoff {
                    filtered_out += 1;
                    continue;
                }
            }

            let msg_count = raw.messages.len() as u32;

            if incremental {
                if let Some((old_updated_at, old_msg_count)) =
                    store.session_meta(source_id, &raw.source_id)?
                {
                    let changed = old_msg_count != msg_count
                        || (raw.updated_at.is_some() && raw.updated_at != old_updated_at);
                    if !changed {
                        skipped += 1;
                        continue;
                    }
                    store.delete_session_data(source_id, &raw.source_id)?;
                    updated_sessions += 1;
                } else {
                    new_sessions += 1;
                }
            } else {
                store.delete_session_data(source_id, &raw.source_id)?;
                new_sessions += 1;
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

            store.insert_session(&session)?;

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

            store.insert_messages(&messages)?;
            let units_total = store.embeddable_message_count(&session_uuid)?;
            store.upsert_session_embedding_state(&session_uuid, units_total)?;
            total_messages += msg_count;
        }

        info!("{label} done");
    }

    println!();
    if incremental {
        print!(
            "Sync: {new_sessions} new, {updated_sessions} updated, {skipped} unchanged, {total_messages} messages"
        );
    } else {
        print!("Indexed {} sessions, {total_messages} messages", new_sessions);
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

    Ok(())
}

fn cmd_search(query: &str, source_filter: Option<&str>, time_filter: Option<&str>) -> Result<()> {
    let store = Store::open()?;
    let engine = SearchEngine::new(&store.conn);
    let sources = adapters::source_labels();
    let progress = store.semantic_progress().unwrap_or_default();

    let query_embedding = if EmbeddingProvider::is_available()
        && (progress.done_sessions > 0 || progress.processing_sessions > 0)
    {
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

    let results = engine.hybrid_search(query, query_embedding.as_deref(), &filters, 20)?;

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
fn cmd_tui() -> Result<()> {
    use std::io;
    use std::time::Duration;

    use crossterm::execute;
    use crossterm::terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    };
    use ratatui::Terminal;
    use ratatui::backend::CrosstermBackend;

    use recall::tui::app::App;
    use recall::tui::event::{AppEvent, poll_event};
    use recall::tui::ui;

    let store = Store::open()?;
    semantic::start_background_worker();
    let sources = adapters::source_labels();

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

        app.try_search(&store, &engine, &mut provider);

        if app.should_quit {
            break;
        }
    }

    drop(_guard);
    terminal.show_cursor()?;

    Ok(())
}

fn generate_title(messages: &[adapters::RawMessage]) -> String {
    let first_user_msg = messages.iter().find(|m| m.role == Role::User);
    match first_user_msg {
        Some(msg) => {
            let text = msg.content.trim();
            if text.chars().count() > 80 {
                let truncated: String = text.chars().take(77).collect();
                format!("{truncated}...")
            } else {
                text.to_string()
            }
        }
        None => "Untitled".to_string(),
    }
}
