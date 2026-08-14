use anyhow::Result;

use crate::adapters;
use crate::config::AppConfig;
use crate::db::store::Store;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum InfoFormat {
    Text,
    Json,
}

#[derive(serde::Serialize)]
struct SourceSummary {
    label: String,
    id: String,
    sessions: u64,
    messages: u64,
    range: String,
    error: Option<String>,
}

pub(crate) fn run(format: InfoFormat) -> Result<()> {
    let labels = adapters::source_labels();
    let mut config = AppConfig::load()?;
    config.normalize_sources(&labels);
    let store = Store::open()?;
    let source_stats = store.indexed_source_stats()?;
    let progress = store.semantic_progress().unwrap_or_default();
    let worker = store.background_job_status("pipeline").unwrap_or_default();

    let mut rows = Vec::new();
    let mut grand_sessions = 0u64;
    let mut grand_messages = 0u64;

    for (id, label) in &labels {
        let stats = source_stats.get(id);
        let sessions = stats.map_or(0, |stats| stats.sessions);
        let messages = stats.map_or(0, |stats| stats.messages);
        grand_sessions += sessions;
        grand_messages += messages;

        rows.push(SourceSummary {
            label: label.clone(),
            id: id.clone(),
            sessions,
            messages,
            range: format_date_range(
                stats.and_then(|stats| stats.oldest_started_at),
                stats.and_then(|stats| stats.newest_started_at),
            ),
            error: None,
        });
    }

    if matches!(format, InfoFormat::Json) {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "protocol_version": crate::PROTOCOL_VERSION,
                "schemas": {
                    "export_record": crate::export::RECORD_SCHEMA_VERSION,
                    "database": crate::db::schema::current_schema_version()
                },
                "sources": rows,
                "settings": {
                    "enabled_sources": labels
                        .iter()
                        .filter(|(id, _)| config.is_source_enabled(id))
                        .map(|(id, label)| serde_json::json!({
                            "id": id,
                            "label": label
                        }))
                        .collect::<Vec<_>>(),
                    "time_scope": config.sync_window.label()
                },
                "semantic_queue": {
                    "indexed_sessions": progress.total_sessions,
                    "done_sessions": progress.done_sessions,
                    "pending_sessions": progress.pending_sessions + progress.processing_sessions,
                    "failed_sessions": progress.failed_sessions,
                    "worker_phase": worker.phase,
                    "worker_detail": worker.detail
                }
            }))?
        );
        return Ok(());
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

    println!("Indexed Sources");
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
        source = "Total indexed",
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
