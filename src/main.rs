use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::info;

use recall::adapters;
use recall::db;
use recall::db::search::{SearchEngine, SearchFilters, TimeRange};
use recall::db::store::Store;
use recall::embedding::EmbeddingProvider;
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
    Index {
        #[arg(long, help = "Comma-separated source IDs (e.g. claude-code,codex)")]
        source: Option<String>,
        #[arg(long, help = "Only index sessions newer than this (e.g. 7d, 30d, 3m)")]
        since: Option<String>,
    },
    Sync {
        #[arg(long, help = "Comma-separated source IDs (e.g. claude-code,codex)")]
        source: Option<String>,
        #[arg(long, help = "Only index sessions newer than this (e.g. 7d, 30d, 3m)")]
        since: Option<String>,
    },
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
        Some(Commands::Index { source, since }) => {
            cmd_index(false, source.as_deref(), since.as_deref())?
        }
        Some(Commands::Sync { source, since }) => {
            cmd_index(true, source.as_deref(), since.as_deref())?
        }
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

    println!("Detected sources:\n");

    let mut grand_sessions = 0usize;
    let mut grand_messages = 0usize;

    for adapter in &all {
        let id = adapter.id();
        let label = labels.iter().find(|(k, _)| k == id).map(|(_, v)| v.as_str()).unwrap_or(id);

        print!("  {label} ({id}): ");
        match adapter.scan() {
            Ok(sessions) => {
                let session_count = sessions.len();
                let message_count: usize = sessions.iter().map(|s| s.messages.len()).sum();
                let oldest = sessions.iter().map(|s| s.started_at).min();
                let newest = sessions.iter().map(|s| s.started_at).max();

                grand_sessions += session_count;
                grand_messages += message_count;

                if session_count == 0 {
                    println!("no sessions found");
                    continue;
                }

                let oldest_str = oldest
                    .and_then(chrono::DateTime::from_timestamp_millis)
                    .map(|dt| dt.format("%Y-%m-%d").to_string())
                    .unwrap_or_default();
                let newest_str = newest
                    .and_then(chrono::DateTime::from_timestamp_millis)
                    .map(|dt| dt.format("%Y-%m-%d").to_string())
                    .unwrap_or_default();

                println!(
                    "{session_count} sessions, {message_count} messages ({oldest_str} ~ {newest_str})"
                );
            }
            Err(e) => {
                println!("error: {e}");
            }
        }
    }

    println!("\n  Total: {grand_sessions} sessions, {grand_messages} messages");
    println!("\nTip: use `recall index --source claude-code --since 30d` to index selectively.");

    Ok(())
}

fn resolve_source_filter(filter: Option<&str>) -> Vec<String> {
    match filter {
        None => Vec::new(),
        Some(s) => s.split(',').map(|v| v.trim().to_lowercase()).collect(),
    }
}

fn cmd_index(
    incremental: bool,
    source_filter: Option<&str>,
    since_filter: Option<&str>,
) -> Result<()> {
    let store = Store::open()?;
    let all = adapters::all_adapters();
    let allowed_sources = resolve_source_filter(source_filter);
    let since_ts = since_filter.and_then(utils::parse_since);

    if since_filter.is_some() && since_ts.is_none() {
        eprintln!("Invalid --since value. Use format like: 7d, 30d, 3m, 2w");
        std::process::exit(1);
    }

    let mut message_texts: Vec<(i64, String)> = Vec::new();
    let mut new_sessions = 0u32;
    let mut updated_sessions = 0u32;
    let mut total_messages = 0u32;
    let mut skipped = 0u32;
    let mut filtered_out = 0u32;

    for adapter in &all {
        let source_id = adapter.id();
        let label = adapter.label();

        if !allowed_sources.is_empty() && !allowed_sources.contains(&source_id.to_string()) {
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

            for (msg_id, content) in store.embeddable_messages(&session_uuid)? {
                let text = format!("{}: {content}", session.title);
                let text = if text.chars().count() > 500 {
                    text.chars().take(500).collect()
                } else {
                    text
                };
                message_texts.push((msg_id, text));
            }
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
        print!(", {filtered_out} filtered by --since");
    }
    println!();

    if !message_texts.is_empty() {
        println!("Loading embedding model...");
        let provider = EmbeddingProvider::new(true)?;
        let texts: Vec<String> = message_texts.iter().map(|(_, t)| t.clone()).collect();
        println!("Embedding {} messages...", texts.len());
        let embeddings = provider.embed_documents(&texts)?;

        println!("Writing embeddings...");
        let items: Vec<(i64, &[f32])> = message_texts
            .iter()
            .zip(embeddings.iter())
            .map(|((id, _), emb)| (*id, emb.as_slice()))
            .collect();
        store.upsert_embeddings(&items)?;
        println!("Embeddings complete.");
    }

    Ok(())
}

fn cmd_search(query: &str, source_filter: Option<&str>, time_filter: Option<&str>) -> Result<()> {
    let store = Store::open()?;
    println!("Loading embedding model...");
    let provider = EmbeddingProvider::new(true)?;
    let engine = SearchEngine::new(&store.conn);
    let sources = adapters::source_labels();

    let query_embedding = provider.embed_query(&[query])?;
    let query_embedding = query_embedding
        .first()
        .ok_or_else(|| anyhow::anyhow!("failed to generate query embedding"))?;

    let resolved_source = source_filter.and_then(|s| {
        let lower = s.to_lowercase();
        sources
            .iter()
            .find(|(id, label)| id == &lower || label.to_lowercase() == lower)
            .map(|(id, _)| id.clone())
    });

    let time_range = match time_filter.map(|t| t.to_lowercase()) {
        Some(ref t) if t == "today" => TimeRange::Today,
        Some(ref t) if t == "7d" || t == "week" => TimeRange::Week,
        Some(ref t) if t == "30d" || t == "month" => TimeRange::Month,
        _ => TimeRange::All,
    };

    let filters = SearchFilters { source: resolved_source, time_range, directory: None };

    let results = engine.hybrid_search(query, Some(query_embedding), &filters, 20)?;

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

    let mut app = App::new(&store, sources);
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
